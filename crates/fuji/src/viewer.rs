//! Full-size photo viewer, shown over the grid.
//!
//! Shows the grid thumbnail at once (scaled up), then fades in the large
//! JPEG preview when it arrives. Neighbouring previews are fetched ahead so
//! arrowing through photos feels instant. Copying, downloading and dragging
//! the photo out are the gallery's ([`crate::transfer`]); its messages and
//! the right-click menu show over the viewer.
//!
//! The image is letterboxed on black and can be zoomed (wheel, Ctrl+wheel or
//! pinch, double-click, +/-/0) and dragged when zoomed. Zoom changes ease in
//! over a few frames; the math lives in [`crate::zoom`]. Opening, closing
//! and stepping between photos are eased too.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fuji_icloud::{Asset, AssetId, AssetKind, PhotosClient, Resource};
use gpui::{
    AnimationExt as _, App, Bounds, Context, CursorStyle, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ObjectFit, Pixels, Point, ScrollDelta, ScrollWheelEvent, Subscription, WeakEntity, Window,
    actions, canvas, div, img, prelude::*, px,
};

use crate::anim::{self, Eased};
use crate::assets::icon;
use crate::grid::PhotoGrid;
use crate::image_cache::LruImageCache;
use crate::zoom::{Geometry, View};
use crate::{net, theme, thumbs, titlebar, ui};

actions!(
    viewer,
    [
        Close, Previous, Next, CopyImage, Download, ZoomIn, ZoomOut, ZoomFit
    ]
);

const CONTEXT: &str = "Viewer";
const MAX_PREVIEW_REQUESTS: usize = 3;
/// How far ahead and behind to fetch previews.
const PREFETCH: usize = 2;
/// How long the fade out takes before the viewer is removed.
const CLOSE_MS: u64 = 130;
/// How far a photo slides in when stepping to it.
const SLIDE: f32 = 56.;
/// How far the pointer moves with the button down before a drag-out starts.
const DRAG_THRESHOLD: f32 = 6.;
/// Space around the image when it fits.
const MARGIN: f32 = 16.;
/// The viewer's own bar, under the app's title bar.
const BAR_ROW: f32 = 52.;
/// How close to the bottom the pointer brings up the filmstrip.
const STRIP_ZONE: f32 = 150.;
/// Filmstrip thumbnails: size, gap, and how many each side of the photo.
const STRIP_THUMB: f32 = 46.;
const STRIP_GAP: f32 = 3.;
const STRIP_REACH: usize = 20;
/// Zoom step for one wheel notch or key press.
const ZOOM_STEP: f32 = 1.25;
/// How quickly zoom eases to its target (higher is snappier).
const ZOOM_EASE: f32 = 18.;
/// GPUI reports a wheel notch as 3 "lines" on Windows (the default setting)
/// and on Linux.
const LINES_PER_NOTCH: f32 = 3.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Close, Some(CONTEXT)),
        KeyBinding::new("left", Previous, Some(CONTEXT)),
        KeyBinding::new("right", Next, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", CopyImage, Some(CONTEXT)),
        KeyBinding::new("ctrl-s", Download, Some(CONTEXT)),
        KeyBinding::new("=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("+", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("-", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("0", ZoomFit, Some(CONTEXT)),
        KeyBinding::new("ctrl-0", ZoomFit, Some(CONTEXT)),
    ]);
}

pub enum ViewerEvent {
    /// The viewer closed while showing this timeline index.
    Closed { index: usize },
}

pub struct Viewer {
    grid: WeakEntity<PhotoGrid>,
    client: Arc<PhotosClient>,
    preview_dir: Arc<PathBuf>,
    index: usize,
    focus_handle: FocusHandle,
    previews_ready: HashMap<AssetId, Arc<Path>>,
    previews_failed: HashSet<AssetId>,
    previews_in_flight: HashSet<AssetId>,
    image_cache: Entity<LruImageCache>,
    _grid_observer: Subscription,
    /// What's on screen now, and where it's easing to.
    view: View,
    target: View,
    last_frame: Option<Instant>,
    /// The image area's bounds from the last paint.
    stage_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Option<Drag>,
    /// Where the button went down on a photo that fits: it may be dragged out.
    press: Option<Point<Pixels>>,
    /// 0 to 1 as the viewer opens: the photo settles into place.
    appear: Eased,
    /// 1 or -1 down to 0 after stepping to the next or previous photo.
    slide: Eased,
    closing: bool,
    /// The info panel is open.
    info_open: bool,
    /// The pointer is near the bottom: show the filmstrip.
    strip_shown: bool,
}

struct Drag {
    start: Point<Pixels>,
    start_view: View,
}

impl EventEmitter<ViewerEvent> for Viewer {}

impl Focusable for Viewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Viewer {
    pub fn new(
        grid: &Entity<PhotoGrid>,
        client: Arc<PhotosClient>,
        preview_dir: Arc<PathBuf>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-check what to load whenever the grid gets new items.
        let observer = cx.observe(grid, |this: &mut Self, _, cx| {
            this.load_around(cx);
            cx.notify();
        });
        let viewer = Self {
            grid: grid.downgrade(),
            client,
            preview_dir,
            index,
            focus_handle: cx.focus_handle(),
            previews_ready: HashMap::new(),
            previews_failed: HashSet::new(),
            previews_in_flight: HashSet::new(),
            image_cache: cx.new(|cx| LruImageCache::new(PREFETCH * 2 + 4, cx)),
            _grid_observer: observer,
            view: View::FIT,
            target: View::FIT,
            last_frame: None,
            stage_bounds: Rc::new(Cell::new(None)),
            drag: None,
            press: None,
            appear: {
                let mut appear = Eased::new(0.);
                appear.set(1.);
                appear
            },
            slide: Eased::new(0.),
            closing: false,
            info_open: false,
            strip_shown: false,
        };
        // The grid is mid-update while it creates us; read it once it's done.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |viewer, cx| viewer.load_around(cx)).ok();
        });
        viewer
    }

    fn count(&self, cx: &App) -> usize {
        self.grid
            .upgrade()
            .map_or(0, |grid| grid.read(cx).item_count())
    }

    fn asset(&self, index: usize, cx: &App) -> Option<Asset> {
        self.grid.upgrade()?.read(cx).asset(index).cloned()
    }

    /// Make sure the current item and its neighbours are loading.
    fn load_around(&mut self, cx: &mut Context<Self>) {
        let count = self.count(cx);
        if count == 0 {
            return;
        }
        let first = self.index.saturating_sub(PREFETCH);
        let last = (self.index + PREFETCH).min(count - 1);
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| {
                for ix in [self.index, last, first] {
                    grid.ensure_loaded(ix, cx);
                }
            });
        }

        // Current first, then the next ones, then the previous ones.
        let order = std::iter::once(self.index)
            .chain(self.index + 1..=last)
            .chain((first..self.index).rev());
        for ix in order {
            if self.previews_in_flight.len() >= MAX_PREVIEW_REQUESTS {
                break;
            }
            let Some(asset) = self.asset(ix, cx) else {
                continue;
            };
            // No large rendition: the thumbnail (already on screen) is all there is.
            let Some(resource) = asset.preview.clone() else {
                continue;
            };
            let id = asset.id;
            if self.previews_ready.contains_key(&id)
                || self.previews_failed.contains(&id)
                || self.previews_in_flight.contains(&id)
            {
                continue;
            }
            self.start_preview(id, resource, cx);
        }
    }

    fn start_preview(&mut self, id: AssetId, resource: Resource, cx: &mut Context<Self>) {
        self.previews_in_flight.insert(id.clone());
        let client = self.client.clone();
        let dir = self.preview_dir.clone();
        cx.spawn(async move |this, cx| {
            let task_id = id.clone();
            let result = net::run(move || thumbs::ensure(&client, &dir, &task_id, &resource)).await;
            this.update(cx, |this, cx| {
                this.previews_in_flight.remove(&id);
                match result {
                    Some(Ok(path)) => {
                        this.previews_ready.insert(id, path.into());
                    }
                    _ => {
                        this.previews_failed.insert(id);
                    }
                }
                this.load_around(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn go_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.count(cx);
        if count == 0 || index >= count || index == self.index {
            return;
        }
        // The new photo slides in from the side it came from.
        self.slide.jump(if index > self.index { 1. } else { -1. });
        self.slide.set(0.);
        self.index = index;
        self.view = View::FIT;
        self.target = View::FIT;
        self.drag = None;
        self.press = None;
        self.load_around(cx);
        if self.strip_shown {
            self.want_strip_thumbnails(cx);
        }
        cx.notify();
    }

    /// The filmstrip's range of photos around the current one.
    fn strip_range(&self, cx: &App) -> std::ops::Range<usize> {
        let count = self.count(cx);
        self.index.saturating_sub(STRIP_REACH)..(self.index + STRIP_REACH + 1).min(count)
    }

    fn want_strip_thumbnails(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<usize> = self.strip_range(cx).collect();
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| grid.want_thumbnails(wanted, cx));
        }
    }

    fn set_strip_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.strip_shown != shown {
            self.strip_shown = shown;
            if shown {
                self.want_strip_thumbnails(cx);
            }
            cx.notify();
        }
    }

    /// Layout of the current image in the stage, once the stage has been laid out.
    fn geometry(&self, window: &Window, cx: &App) -> Option<Geometry> {
        let bounds = self.stage_bounds.get()?;
        let (image_w, image_h) = natural_size(&self.asset(self.index, cx)?)?;
        Some(Geometry {
            viewport_w: f32::from(bounds.size.width),
            viewport_h: f32::from(bounds.size.height),
            image_w,
            image_h,
            margin: MARGIN,
            scale_factor: window.scale_factor(),
        })
    }

    /// `position` relative to the stage centre.
    fn offset_from_centre(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.stage_bounds.get()?;
        let centre = bounds.center();
        Some((
            f32::from(position.x - centre.x),
            f32::from(position.y - centre.y),
        ))
    }

    fn set_target(&mut self, target: View, cx: &mut Context<Self>) {
        self.target = target;
        cx.notify();
    }

    fn zoom_by(
        &mut self,
        factor: f32,
        at: Option<(f32, f32)>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(g) = self.geometry(window, cx) else {
            return;
        };
        let (x, y) = at.unwrap_or((0., 0.));
        let target = g.zoom_at(self.target, self.target.zoom * factor, x, y);
        self.set_target(target, cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(ZOOM_STEP, None, window, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1. / ZOOM_STEP, None, window, cx);
    }

    fn zoom_fit(&mut self, _: &ZoomFit, _: &mut Window, cx: &mut Context<Self>) {
        self.set_target(View::FIT, cx);
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.geometry(window, cx) else {
            return;
        };
        let at = self.offset_from_centre(event.position);
        match event.delta {
            // Mouse wheel: zoom toward the cursor.
            ScrollDelta::Lines(lines) => {
                self.zoom_by(ZOOM_STEP.powf(lines.y / LINES_PER_NOTCH), at, window, cx);
            }
            // Touchpad: pinch (sent as Ctrl+scroll) zooms; two-finger
            // scrolling pans once zoomed in, and zooms when it fits.
            ScrollDelta::Pixels(delta) => {
                let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
                if event.modifiers.control || self.target.is_fit() {
                    self.zoom_by((dy * 0.005).exp(), at, window, cx);
                } else {
                    let panned = g.clamp(View {
                        pan_x: self.target.pan_x + dx,
                        pan_y: self.target.pan_y + dy,
                        ..self.target
                    });
                    // Follow the fingers directly; easing here feels laggy.
                    self.view = panned;
                    self.set_target(panned, cx);
                }
            }
        }
        cx.stop_propagation();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if event.click_count == 2 {
            if let (Some(g), Some((x, y))) = (
                self.geometry(window, cx),
                self.offset_from_centre(event.position),
            ) {
                self.drag = None;
                self.set_target(g.toggle_at(self.target, x, y), cx);
            }
            return;
        }
        if !self.target.is_fit() {
            self.drag = Some(Drag {
                start: event.position,
                start_view: self.target,
            });
            cx.notify();
        } else {
            self.press = Some(event.position);
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Near the bottom (and not dragging), the filmstrip comes up.
        if let Some(bounds) = self.stage_bounds.get() {
            let near = f32::from(bounds.bottom() - event.position.y) < STRIP_ZONE
                && event.pressed_button.is_none();
            self.set_strip_shown(near, cx);
        }
        if let Some(start) = self.press {
            let moved = event.position - start;
            if event.pressed_button != Some(MouseButton::Left) {
                self.press = None;
            } else if f32::from(moved.x).hypot(f32::from(moved.y)) > DRAG_THRESHOLD {
                // Dragging a photo that fits carries it out as a file.
                self.press = None;
                let index = self.index;
                if let Some(grid) = self.grid.upgrade() {
                    grid.update(cx, |grid, cx| grid.start_drag(vec![index], window, cx));
                }
            }
            return;
        }
        let Some(drag) = &self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            cx.notify();
            return;
        }
        let Some(g) = self.geometry(window, cx) else {
            return;
        };
        let moved = event.position - drag.start;
        let panned = g.clamp(View {
            pan_x: drag.start_view.pan_x + f32::from(moved.x),
            pan_y: drag.start_view.pan_y + f32::from(moved.y),
            ..drag.start_view
        });
        self.view = panned;
        self.set_target(panned, cx);
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.press = None;
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Ease the visible view toward the target; keep frames coming until there.
    fn advance_animation(&mut self, window: &mut Window, cx: &App) {
        let now = Instant::now();
        let dt = self
            .last_frame
            .map_or(1. / 60., |t| (now - t).as_secs_f32())
            .clamp(0.001, 0.05);
        self.last_frame = Some(now);

        let settling = self.appear.tick(13., 0.004) | self.slide.tick(15., 0.004);
        if settling {
            window.request_animation_frame();
        }

        // Keep both in bounds if the window was resized.
        if let Some(g) = self.geometry(window, cx) {
            self.target = g.clamp(self.target);
        }
        if self.view.close_to(self.target) {
            self.view = self.target;
            return;
        }
        self.view = self
            .view
            .approach(self.target, 1. - (-dt * ZOOM_EASE).exp());
        window.request_animation_frame();
    }

    fn previous(&mut self, _: &Previous, _: &mut Window, cx: &mut Context<Self>) {
        if self.index > 0 {
            self.go_to(self.index - 1, cx);
        }
    }

    fn next(&mut self, _: &Next, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to(self.index + 1, cx);
    }

    fn close(&mut self, _: &Close, _: &mut Window, cx: &mut Context<Self>) {
        self.begin_close(cx);
    }

    /// Fade out, then tell the gallery to remove the viewer.
    fn begin_close(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.closing = true;
        cx.notify();
        let wait = Duration::from_millis(if anim::reduced() { 0 } else { CLOSE_MS });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                cx.emit(ViewerEvent::Closed { index: this.index })
            })
            .ok();
        })
        .detach();
    }

    fn copy(&mut self, _: &CopyImage, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_current(cx);
    }

    fn copy_current(&mut self, cx: &mut Context<Self>) {
        let index = self.index;
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| grid.copy(vec![index], cx));
        }
    }

    fn download(&mut self, _: &Download, _: &mut Window, cx: &mut Context<Self>) {
        self.download_current(cx);
    }

    fn download_current(&mut self, cx: &mut Context<Self>) {
        let index = self.index;
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| grid.download(vec![index], cx));
        }
    }

    fn open_menu(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.index;
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| {
                grid.open_viewer_menu(index, event.position, cx)
            });
        }
    }

    fn render_top_bar(
        &self,
        asset: Option<&Asset>,
        count: usize,
        zoom_percent: Option<u32>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let title = asset
            .and_then(|a| a.filename.clone())
            .unwrap_or_else(|| "Photo".into());
        let date = asset.and_then(|a| format_date(a.created_at_ms));
        let position = format!(
            "{} of {}",
            group_digits(self.index + 1),
            group_digits(count)
        );

        let info_open = self.info_open;
        let row = div()
            .flex()
            .items_center()
            .gap_4()
            .h(px(BAR_ROW))
            // The close button's icon sits under the app's mark, the
            // position under the window's right edge padding.
            .pl(px(7.))
            .pr(px(14.))
            .text_sm()
            .child(ui::icon_button(
                "close",
                "close",
                cx.listener(|this, _, _, cx| this.begin_close(cx)),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(title),
                    )
                    .when_some(date, |el, date| {
                        el.child(div().text_xs().text_color(theme::text_muted()).child(date))
                    }),
            )
            .child(div().flex_1())
            .when_some(zoom_percent, |el, percent| {
                // Click to go back to fit.
                el.child(
                    div()
                        .id("zoom-level")
                        // Fixed width so the bar doesn't shift as digits change.
                        .w(px(52.))
                        .h(theme::CONTROL_HEIGHT)
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(theme::RADIUS_CONTROL)
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                        .on_click(cx.listener(|this, _, _, cx| this.set_target(View::FIT, cx)))
                        .child(format!("{percent}%")),
                )
            })
            .child(ui::text_button(
                "copy",
                Some("copy"),
                "Copy",
                cx.listener(|this, _, _, cx| this.copy_current(cx)),
            ))
            .child(ui::text_button(
                "download",
                Some("download"),
                "Download",
                cx.listener(|this, _, _, cx| this.download_current(cx)),
            ))
            .child(
                ui::icon_button(
                    "info",
                    if info_open { "info-active" } else { "info" },
                    cx.listener(|this, _, _, cx| {
                        this.info_open = !this.info_open;
                        cx.notify();
                    }),
                )
                .when(info_open, |el| el.bg(theme::fill())),
            )
            .child(
                div()
                    .min_w(px(80.))
                    .text_right()
                    .text_size(px(12.))
                    .text_color(theme::text_muted())
                    .child(position),
            );

        // From the window's top: black behind the app's bar and this one,
        // fading out below them.
        let bar = f32::from(titlebar::HEIGHT);
        let height = bar + BAR_ROW + 36.;
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(height))
            .pt(titlebar::HEIGHT)
            .bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(gpui::black().opacity(0.9), (bar + BAR_ROW - 8.) / height),
                gpui::linear_color_stop(gpui::black().opacity(0.), 1.),
            ))
            .child(row)
    }

    /// Details of the photo, in a panel on the right.
    fn render_info(&self, asset: &Asset) -> impl IntoElement + use<> {
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        if let Some(name) = &asset.filename {
            rows.push(("Name", name.clone()));
        }
        if let Some(date) = format_long_date(asset.created_at_ms) {
            rows.push(("Taken", date));
        }
        rows.push((
            "Kind",
            match asset.kind {
                AssetKind::Photo => "Photo".to_string(),
                AssetKind::LivePhoto => "Live Photo".to_string(),
                AssetKind::Video { duration_secs } => {
                    format!("Video, {}:{:02}", duration_secs / 60, duration_secs % 60)
                }
            },
        ));
        if asset.width > 0 && asset.height > 0 {
            let megapixels = asset.width as f32 * asset.height as f32 / 1_000_000.;
            rows.push((
                "Size",
                format!(
                    "{} \u{00d7} {} \u{00b7} {megapixels:.1} MP",
                    asset.width, asset.height
                ),
            ));
        }
        if let Some(original) = &asset.original {
            if original.size > 0 {
                rows.push(("File", format_file_size(original.size)));
            }
            if let Some(format) = format_name(&original.file_type) {
                rows.push(("Format", format));
            }
        }
        if asset.favorite {
            rows.push(("Favorite", "Yes".to_string()));
        }

        div()
            .id("viewer-info")
            .occlude()
            .absolute()
            .top(titlebar::HEIGHT + px(BAR_ROW + 4.))
            .right(px(14.))
            .w(px(280.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(16.))
            .bg(theme::popover())
            .border_1()
            .border_color(theme::line())
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child("Info"),
            )
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text_meta())
                            .child(label),
                    )
                    .child(div().text_color(theme::text_title()).child(value))
            }))
            .with_animation("viewer-info-open", anim::ease(anim::FAST), |el, t| {
                el.opacity(t)
            })
    }

    /// Thumbnails of the photos around this one, along the bottom; click
    /// one to go to it. The current one sits in the middle.
    fn render_strip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let grid = self.grid.upgrade()?;
        let width = f32::from(self.stage_bounds.get()?.size.width);
        let middle = width / 2.;
        let step = STRIP_THUMB + STRIP_GAP;
        let current = self.index;
        let thumbs: Vec<_> = self
            .strip_range(cx)
            .map(|i| {
                let grid = grid.read(cx);
                let path = grid.asset(i).and_then(|a| grid.thumbnail_path(&a.id));
                (i, path)
            })
            .collect();
        let row_top = 26.;
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(row_top + STRIP_THUMB + 26.))
                .bg(gpui::linear_gradient(
                    0.,
                    gpui::linear_color_stop(gpui::black().opacity(0.8), 0.),
                    gpui::linear_color_stop(gpui::black().opacity(0.), 1.),
                ))
                .children(thumbs.into_iter().map(|(i, path)| {
                    let offset = i as f32 - current as f32;
                    let this = i == current;
                    let size = if this { STRIP_THUMB + 8. } else { STRIP_THUMB };
                    let x = middle + offset * step + offset.signum() * 4. - size / 2.;
                    div()
                        .id(ElementId::NamedInteger("strip".into(), i as u64))
                        .occlude()
                        .absolute()
                        .left(px(x))
                        .top(px(row_top - if this { 4. } else { 0. }))
                        .size(px(size))
                        .rounded(px(6.))
                        .overflow_hidden()
                        .cursor_pointer()
                        .bg(theme::placeholder())
                        .when(this, |el| el.border_2().border_color(gpui::white()))
                        .when(!this, |el| el.opacity(0.75).hover(|el| el.opacity(1.)))
                        .when_some(path, |el, path| {
                            el.child(img(path).size_full().object_fit(ObjectFit::Cover))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.go_to(i, cx)))
                }))
                .with_animation("viewer-strip", anim::ease(anim::FAST), |el, t| {
                    el.opacity(t)
                }),
        )
    }
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.advance_animation(window, cx);
        let count = self.count(cx);
        let asset = self.asset(self.index, cx);
        let thumb = asset.as_ref().and_then(|a| {
            self.grid
                .upgrade()
                .and_then(|grid| grid.read(cx).thumbnail_path(&a.id))
        });
        let preview = asset
            .as_ref()
            .and_then(|a| self.previews_ready.get(&a.id).cloned());
        let preview_failed = asset
            .as_ref()
            .is_some_and(|a| self.previews_failed.contains(&a.id));
        let has_preview = asset.as_ref().is_some_and(|a| a.preview.is_some());
        let loading = asset.is_none() || (has_preview && preview.is_none() && !preview_failed);
        let note = asset.as_ref().and_then(|a| match a.kind {
            AssetKind::Video { .. } => Some("Video playback is coming soon. Download to watch."),
            _ => None,
        });

        let closing = self.closing;
        let geometry = self.geometry(window, cx);
        // Settle in from slightly small on opening; slide in when stepping.
        let (appear, slide) = (self.appear.get(), self.slide.get());
        let layer_opacity = (1. - slide.abs()).clamp(0., 1.);
        let rect = geometry.map(|g| {
            let mut r = g.rect(self.view);
            let scale = 0.955 + 0.045 * appear;
            r.x += r.w * (1. - scale) / 2. + slide * SLIDE;
            r.y += r.h * (1. - scale) / 2.;
            r.w *= scale;
            r.h *= scale;
            r
        });
        let zoom_percent = geometry
            .filter(|_| !self.target.is_fit())
            .map(|g| (self.target.zoom / g.actual_size_zoom() * 100.).round() as u32);
        let zoomed = !self.target.is_fit();

        let top_bar = self.render_top_bar(asset.as_ref(), count, zoom_percent, cx);
        let index = self.index;
        let has_previous = index > 0 && !zoomed;
        let has_next = index + 1 < count && !zoomed;

        // Both layers are drawn into the same letterboxed rectangle, so the
        // sharp preview lands exactly on top of the thumbnail.
        let place = |el: gpui::Div| match rect {
            Some(r) => el
                .absolute()
                .left(px(r.x))
                .top(px(r.y))
                .w(px(r.w))
                .h(px(r.h))
                .opacity(layer_opacity),
            None => el.absolute().size_0(),
        };
        let bounds_cell = self.stage_bounds.clone();

        let info = self
            .info_open
            .then(|| asset.as_ref().map(|a| self.render_info(a)))
            .flatten();
        let strip = if self.strip_shown && !zoomed {
            self.render_strip(cx)
        } else {
            None
        };
        let stage = div()
            .image_cache(self.image_cache.clone())
            .id("viewer-stage")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .overflow_hidden()
            .bg(gpui::black())
            .when(zoomed, |el| {
                el.cursor(if self.drag.is_some() {
                    CursorStyle::ClosedHand
                } else {
                    CursorStyle::OpenHand
                })
            })
            .on_scroll_wheel(cx.listener(Self::on_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::open_menu))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered {
                    this.set_strip_shown(false, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_drag(cx)),
            )
            // Measure the stage; redraw if it changed (first frame, resize).
            .child(
                canvas(
                    move |bounds, window, _| {
                        if bounds_cell.get() != Some(bounds) {
                            bounds_cell.set(Some(bounds));
                            window.refresh();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            // Upscaled thumbnail first, so there's never a blank screen.
            .when_some(thumb, |el, path| {
                el.child(place(div()).child(img(path).size_full().object_fit(ObjectFit::Contain)))
            })
            .when_some(preview, |el, path| {
                el.child(
                    place(div())
                        .child(img(path).size_full().object_fit(ObjectFit::Contain))
                        .with_animation(
                            ElementId::NamedInteger("preview-fade".into(), index as u64),
                            anim::ease(anim::NORMAL),
                            move |el, t| el.opacity(t * layer_opacity),
                        ),
                )
            })
            .when(loading, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(STRIP_ZONE / 2.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .bg(gpui::black().opacity(0.6))
                                .text_xs()
                                .text_color(gpui::white())
                                .child(ui::loading_status(
                                    "preview-loading",
                                    "Loading\u{2026}",
                                    gpui::white(),
                                )),
                        ),
                )
            })
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .absolute()
                        .bottom_4()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(pill(note)),
                )
            })
            .when(has_previous, |el| {
                el.child(edge_button(
                    "previous",
                    "previous",
                    true,
                    cx.listener(|this, _, window, cx| this.previous(&Previous, window, cx)),
                ))
            })
            .when(has_next, |el| {
                el.child(edge_button(
                    "next",
                    "next",
                    false,
                    cx.listener(|this, _, window, cx| this.next(&Next, window, cx)),
                ))
            });

        div()
            .id("viewer")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::download))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_fit))
            .occlude()
            .absolute()
            .inset_0()
            .bg(gpui::black())
            .text_color(theme::text())
            .child(stage)
            .children(strip)
            .child(top_bar)
            .children(info)
            .with_animation(
                if closing {
                    "viewer-close"
                } else {
                    "viewer-open"
                },
                anim::ease(if closing { CLOSE_MS } else { 180 }),
                move |el, t| el.opacity(if closing { 1. - t } else { t }),
            )
    }
}

/// The image's shape and size in pixels. The thumbnail is a rendered,
/// correctly rotated copy, so its proportions are trusted over the original's
/// (which may be stored sideways with an EXIF rotation); the preview, or else
/// the original, gives the size.
fn natural_size(asset: &Asset) -> Option<(f32, f32)> {
    let dims =
        |r: &Resource| (r.width > 0 && r.height > 0).then_some((r.width as f32, r.height as f32));
    let original =
        (asset.width > 0 && asset.height > 0).then_some((asset.width as f32, asset.height as f32));
    let shape = asset
        .thumbnail
        .as_ref()
        .and_then(dims)
        .or_else(|| asset.preview.as_ref().and_then(dims))
        .or(original)?;
    let long_edge = asset
        .preview
        .as_ref()
        .and_then(dims)
        .or(original)
        .map_or(shape.0.max(shape.1), |(w, h)| w.max(h));
    let ratio = shape.0 / shape.1;
    Some(if ratio >= 1. {
        (long_edge, long_edge / ratio)
    } else {
        (long_edge * ratio, long_edge)
    })
}

fn format_date(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let zoned = jiff::Timestamp::from_millisecond(ms)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%-d %b %Y, %H:%M").to_string())
}

/// "Sunday 1 November 2026 at 14:32".
fn format_long_date(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    let zoned = jiff::Timestamp::from_millisecond(ms)
        .ok()?
        .to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%A %-d %B %Y at %H:%M").to_string())
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.0} KB", (b / KB).max(1.))
    }
}

/// A file type's everyday name, from its uniform type identifier.
fn format_name(uti: &str) -> Option<String> {
    let name = match uti {
        "" => return None,
        "public.jpeg" => "JPEG",
        "public.heic" => "HEIC",
        "public.heif" => "HEIF",
        "public.png" => "PNG",
        "com.compuserve.gif" => "GIF",
        "public.tiff" => "TIFF",
        "com.adobe.raw-image" => "DNG",
        "com.apple.quicktime-movie" => "QuickTime movie",
        "public.mpeg-4" => "MPEG-4 video",
        other => return other.rsplit('.').next().map(|s| s.to_uppercase()),
    };
    Some(name.to_string())
}

fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn pill(text: &'static str) -> gpui::Div {
    div()
        .px_3()
        .py_1()
        .rounded_full()
        .bg(gpui::black().opacity(0.6))
        .text_xs()
        .text_color(gpui::white())
        .child(text)
}

type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>;

/// A tall, invisible hit area along one edge, with a chevron that shows on hover.
fn edge_button(
    id: &'static str,
    glyph: &'static str,
    left: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_click: ClickHandler = Box::new(on_click);
    div()
        .id(id)
        .group(id)
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(96.))
        .when(left, |el| el.left_0())
        .when(!left, |el| el.right_0())
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_click(move |event, window, cx| on_click(event, window, cx))
        .child(
            div()
                .size(px(40.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme::scrim(0.55))
                .opacity(0.)
                .group_hover(id, |el| el.opacity(1.))
                .child(icon(glyph, px(20.), gpui::white())),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_formats() {
        assert_eq!(format_file_size(2_400_000), "2.3 MB");
        assert_eq!(format_file_size(10), "1 KB");
        assert_eq!(format_name("public.heic").as_deref(), Some("HEIC"));
        assert_eq!(format_name("org.webmproject.webp").as_deref(), Some("WEBP"));
        assert_eq!(format_name(""), None);
    }

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(7), "7");
        assert_eq!(group_digits(12_345), "12,345");
    }

    #[test]
    fn dates_format() {
        assert_eq!(format_date(0), None);
        let text = format_date(1_700_000_000_000).unwrap();
        assert!(text.contains("2023"), "{text}");
    }
}
