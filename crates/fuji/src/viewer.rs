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

use fuji_icloud::{Asset, AssetId, AssetKind, PhotoDetails, PhotosClient, Resource};
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
use crate::video::{self, VideoPlayer};
use crate::zoom::{Geometry, View};
use crate::{net, theme, thumbs, ui};

actions!(
    viewer,
    [
        Close,
        Previous,
        Next,
        CopyImage,
        Download,
        ZoomIn,
        ZoomOut,
        ZoomFit,
        PlayPause,
        ToggleFavorite
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
/// How long a Live Photo is held still before its motion plays.
const HOLD_TO_PLAY: Duration = Duration::from_millis(300);
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
/// The bar hides this long after the pointer last moved.
const CHROME_LINGER: Duration = Duration::from_millis(2500);
const CHROME_FADE_MS: u64 = 220;
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
        KeyBinding::new("space", PlayPause, Some(CONTEXT)),
        KeyBinding::new(".", ToggleFavorite, Some(CONTEXT)),
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
    /// The filmstrip's small thumbnails, kept apart from the large images.
    strip_cache: Entity<LruImageCache>,
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
    /// Waits out a press on a Live Photo, to play its motion while held.
    _hold_timer: Option<gpui::Task<()>>,
    /// The motion is playing because the photo is held.
    held: bool,
    /// 0 to 1 as the viewer opens: the photo settles into place.
    appear: Eased,
    /// 1 or -1 down to 0 after stepping to the next or previous photo.
    slide: Eased,
    closing: bool,
    /// The info panel is open.
    info_open: bool,
    /// Camera, lens, exposure and so on, by photo; `None` if it failed.
    details: HashMap<AssetId, Option<PhotoDetails>>,
    details_in_flight: HashSet<AssetId>,
    /// The pointer is near the bottom: show the filmstrip.
    strip_shown: bool,
    /// The video playing, and which photo it belongs to.
    player: Option<(usize, VideoPlayer)>,
    /// Its newest frame (dropped from the GPU when replaced).
    video_frame: Option<Arc<gpui::RenderImage>>,
    /// Why the current video can't play.
    video_error: Option<(usize, String)>,
    /// The player is a Live Photo's motion, playing once over the still.
    live: bool,
    /// The bar shows while the pointer moves, and hides when it rests.
    chrome_shown: bool,
    /// Bumped on each show and hide, so each one fades.
    chrome_serial: u64,
    chrome_hidden_at: Instant,
    /// The pointer is over the bar: keep it.
    pointer_near_top: bool,
    _chrome_timer: Option<gpui::Task<()>>,
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
            // What's on screen (preview and thumbnail) and the photo before:
            // stepping back is instant, and full-size decodes don't pile up.
            image_cache: cx.new(|cx| LruImageCache::new(4, cx)),
            strip_cache: cx.new(|cx| {
                let mut cache = LruImageCache::new(STRIP_REACH * 2 + 8, cx);
                cache.set_max_height(128);
                cache
            }),
            _grid_observer: observer,
            view: View::FIT,
            target: View::FIT,
            last_frame: None,
            stage_bounds: Rc::new(Cell::new(None)),
            drag: None,
            press: None,
            _hold_timer: None,
            held: false,
            appear: {
                let mut appear = Eased::new(0.);
                appear.set(1.);
                appear
            },
            slide: Eased::new(0.),
            closing: false,
            info_open: false,
            details: HashMap::new(),
            details_in_flight: HashSet::new(),
            strip_shown: false,
            player: None,
            video_frame: None,
            live: false,
            video_error: None,
            chrome_shown: true,
            chrome_serial: 0,
            chrome_hidden_at: Instant::now(),
            pointer_near_top: false,
            _chrome_timer: None,
        };
        // The grid is mid-update while it creates us; read it once it's done.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |viewer, cx| {
                viewer.load_around(cx);
                viewer.show_chrome(cx);
            })
            .ok();
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

    /// With the info panel open, fetch the current photo's details.
    fn want_details(&mut self, cx: &mut Context<Self>) {
        if !self.info_open {
            return;
        }
        let Some(asset) = self.asset(self.index, cx) else {
            return;
        };
        let Some(master) = asset.master.clone() else {
            return;
        };
        let id = asset.id.clone();
        if self.details.contains_key(&id) || !self.details_in_flight.insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.details(&master)).await;
            this.update(cx, |this, cx| {
                this.details_in_flight.remove(&id);
                this.details.insert(id, result.and_then(Result::ok));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Play the current photo if it's a video and isn't playing yet.
    fn ensure_video(&mut self, asset: Option<&Asset>, cx: &mut Context<Self>) {
        let index = self.index;
        if self.player.as_ref().is_some_and(|(i, _)| *i == index)
            || self.video_error.as_ref().is_some_and(|(i, _)| *i == index)
        {
            return;
        }
        self.stop_video(cx);
        // A Live Photo moves only when asked (its LIVE badge).
        if !asset.is_some_and(|a| matches!(a.kind, AssetKind::Video { .. })) {
            return;
        }
        let Some(url) = asset.and_then(|a| a.video.as_ref()).map(|v| v.url.clone()) else {
            return;
        };
        match VideoPlayer::open(&url, true) {
            Ok(player) => self.player = Some((index, player)),
            Err(err) => self.video_error = Some((index, err)),
        }
    }

    /// Play the current Live Photo's motion once, from the start.
    fn play_live(&mut self, cx: &mut Context<Self>) {
        let index = self.index;
        if self.live && self.player.as_ref().is_some_and(|(i, _)| *i == index) {
            return;
        }
        let Some(url) = self
            .asset(index, cx)
            .filter(|a| a.kind == AssetKind::LivePhoto)
            .and_then(|a| a.video.map(|v| v.url))
        else {
            return;
        };
        self.stop_video(cx);
        if let Ok(player) = VideoPlayer::open(&url, true) {
            self.player = Some((index, player));
            self.live = true;
            cx.notify();
        }
    }

    /// Back to the still, if the motion is playing.
    fn stop_live(&mut self, cx: &mut Context<Self>) {
        if self.live {
            self.stop_video(cx);
            cx.notify();
        }
    }

    fn stop_video(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.player = None;
        if let Some(frame) = self.video_frame.take() {
            cx.drop_image(frame, None);
        }
    }

    fn toggle_favorite(&mut self, _: &ToggleFavorite, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.index;
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| grid.toggle_favorite(vec![index], cx));
        }
        self.show_chrome(cx);
        cx.notify();
    }

    /// The list of albums to add the photo to, under the top bar's right
    /// end, where Info opens.
    fn add_to_album(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.index;
        let position = gpui::point(window.viewport_size().width - px(14.), px(BAR_ROW + 8.));
        self.info_open = false;
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| {
                grid.open_album_picker(vec![index], position, gpui::Corner::TopRight, window, cx)
            });
        }
        self.show_chrome(cx);
        cx.notify();
    }

    /// The key: add the photo to the album used last (or pick one).
    fn add_to_last_album(
        &mut self,
        _: &crate::album_picker::AddToLastAlbum,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self.index;
        let position = gpui::point(window.viewport_size().width - px(14.), px(BAR_ROW + 8.));
        if let Some(grid) = self.grid.upgrade() {
            grid.update(cx, |grid, cx| {
                grid.add_to_last_album(
                    vec![index],
                    Some((position, gpui::Corner::TopRight)),
                    window,
                    cx,
                )
            });
        }
        self.show_chrome(cx);
        cx.notify();
    }

    fn album_picker_open(&self, cx: &App) -> bool {
        self.grid
            .upgrade()
            .is_some_and(|grid| grid.read(cx).albums.is_open())
    }

    fn play_pause(&mut self, _: &PlayPause, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, player)) = &self.player {
            player.toggle();
            self.show_chrome(cx);
            cx.notify();
        }
    }

    /// "LIVE" by the file name: the pointer on it plays the motion.
    fn render_live_badge(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let playing = self.live;
        let ink = if playing {
            theme::accent_text()
        } else {
            theme::text_muted()
        };
        div()
            .id("live-badge")
            .flex_none()
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(3.))
            .h(px(18.))
            .px(px(5.))
            .rounded(px(5.))
            .bg(if playing {
                theme::accent_soft()
            } else {
                theme::hover()
            })
            .text_size(px(10.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ink)
            .child(icon("live", px(12.), ink))
            .child("LIVE")
            .tooltip(ui::tooltip(
                "Point here, or press and hold the photo, to play",
            ))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered {
                    this.play_live(cx);
                } else {
                    this.stop_live(cx);
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| this.play_live(cx)))
    }

    /// Play, pause, where it is (drag to seek), and sound on or off.
    fn render_video_controls(
        &self,
        player: &VideoPlayer,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (position, duration) = player.position();
        let paused = player.is_paused();
        let muted = player.is_muted();
        let weak = cx.weak_entity();
        let time = |t: f64| {
            div()
                .w(px(44.))
                .text_size(px(11.))
                .text_color(theme::text_muted())
                .child(video::format_time(t))
        };
        div()
            .id("video-controls")
            .occlude()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(4.))
            .rounded_full()
            .bg(theme::popover())
            .shadow_lg()
            .child(
                ui::icon_button(
                    "video-play",
                    if paused { "play-fill" } else { "pause-fill" },
                    cx.listener(|this, _, window, cx| this.play_pause(&PlayPause, window, cx)),
                )
                .tooltip(ui::tooltip(if paused {
                    "Play (Space)"
                } else {
                    "Pause (Space)"
                })),
            )
            .child(time(position).text_right())
            .child(ui::slider(
                "video-seek",
                if duration > 0. {
                    (position / duration) as f32
                } else {
                    0.
                },
                px(320.),
                move |fraction, _, cx| {
                    weak.update(cx, |this, cx| {
                        if let Some((_, player)) = &this.player {
                            let (_, duration) = player.position();
                            player.seek(fraction as f64 * duration);
                            cx.notify();
                        }
                    })
                    .ok();
                },
            ))
            .child(time(duration))
            .child(
                ui::icon_button(
                    "video-mute",
                    if muted { "speaker-off" } else { "speaker" },
                    cx.listener(move |this, _, _, cx| {
                        if let Some((_, player)) = &this.player {
                            player.set_muted(!muted);
                            cx.notify();
                        }
                    }),
                )
                .tooltip(ui::tooltip(if muted { "Sound on" } else { "Mute" })),
            )
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
        self.stop_video(cx);
        self.load_around(cx);
        self.want_details(cx);
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

    /// The pointer moved somewhere in the window.
    fn pointer_moved(&mut self, position: Point<Pixels>, pressed: bool, cx: &mut Context<Self>) {
        let Some(bounds) = self.stage_bounds.get() else {
            return;
        };
        if !bounds.contains(&position) {
            return;
        }
        // Near the bottom (and not dragging), the filmstrip comes up.
        let near_bottom = f32::from(bounds.bottom() - position.y) < STRIP_ZONE && !pressed;
        self.set_strip_shown(near_bottom, cx);
        self.pointer_near_top = f32::from(position.y - bounds.top()) < BAR_ROW + 36.;
        self.show_chrome(cx);
    }

    /// Show the bar, and hide it again once the pointer has rested.
    fn show_chrome(&mut self, cx: &mut Context<Self>) {
        if !self.chrome_shown {
            self.chrome_shown = true;
            self.chrome_serial += 1;
            cx.notify();
        }
        self._chrome_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHROME_LINGER).await;
            this.update(cx, |this, cx| this.rest_chrome(cx)).ok();
        }));
    }

    fn rest_chrome(&mut self, cx: &mut Context<Self>) {
        if self.info_open || self.pointer_near_top || self.album_picker_open(cx) {
            self.show_chrome(cx);
            return;
        }
        self.chrome_shown = false;
        self.chrome_serial += 1;
        self.chrome_hidden_at = Instant::now();
        cx.notify();
        // Draw once more after the fade, to drop the hidden bar.
        self._chrome_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(CHROME_FADE_MS + 40))
                .await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
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
            // Held still a moment on a Live Photo: play its motion until let go.
            let index = self.index;
            if self
                .asset(index, cx)
                .is_some_and(|a| a.kind == AssetKind::LivePhoto && a.video.is_some())
            {
                self._hold_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(HOLD_TO_PLAY).await;
                    this.update(cx, |this, cx| {
                        if this.press.is_some() && this.index == index {
                            this.press = None;
                            this.held = true;
                            this.play_live(cx);
                        }
                    })
                    .ok();
                }));
            }
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        self._hold_timer = None;
        if std::mem::take(&mut self.held) {
            self.stop_live(cx);
        }
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
    ) -> gpui::Div {
        let title = asset
            .and_then(|a| a.filename.clone())
            .unwrap_or_else(|| "Photo".into());
        let live = asset.is_some_and(|a| a.kind == AssetKind::LivePhoto && a.video.is_some());
        let date = asset.and_then(|a| format_date(a.created_at_ms));
        let position = format!(
            "{} of {}",
            group_digits(self.index + 1),
            group_digits(count)
        );

        let info_open = self.info_open;
        let can_favorite = self
            .grid
            .upgrade()
            .is_some_and(|grid| grid.read(cx).can_favorite());
        let can_add_to_album = self
            .grid
            .upgrade()
            .is_some_and(|grid| grid.read(cx).can_add_to_album());
        let favorite = asset.is_some_and(|a| a.favorite);
        let subtitle = match date {
            Some(date) => format!("{date} \u{00b7} {position}"),
            None => position,
        };
        // Three columns: close on the left, the photo's name in the middle,
        // its actions on the right. The sides share the rest equally, so the
        // name stays centered.
        let actions = div()
            .flex_1()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.))
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
                        .tooltip(ui::tooltip("Fit to window"))
                        .on_click(cx.listener(|this, _, _, cx| this.set_target(View::FIT, cx)))
                        .child(format!("{percent}%")),
                )
            })
            .when(can_favorite, |el| {
                el.child(
                    ui::icon_button(
                        "favorite",
                        if favorite { "heart-fill" } else { "favorites" },
                        cx.listener(|this, _, window, cx| {
                            this.toggle_favorite(&ToggleFavorite, window, cx)
                        }),
                    )
                    .tooltip(ui::tooltip(if favorite {
                        "Unfavorite (.)"
                    } else {
                        "Favorite (.)"
                    })),
                )
            })
            .child(
                ui::icon_button(
                    "copy",
                    "copy",
                    cx.listener(|this, _, _, cx| this.copy_current(cx)),
                )
                .tooltip(ui::tooltip("Copy (Ctrl+C)")),
            )
            .child(
                ui::icon_button(
                    "download",
                    "download",
                    cx.listener(|this, _, _, cx| this.download_current(cx)),
                )
                .tooltip(ui::tooltip("Download (Ctrl+S)")),
            )
            .when(can_add_to_album, |el| {
                el.child(
                    ui::icon_button(
                        "add-to-album",
                        "album-add",
                        cx.listener(|this, _, window, cx| this.add_to_album(window, cx)),
                    )
                    .tooltip(ui::tooltip("Add to Album (A)")),
                )
            })
            .child(
                ui::icon_button(
                    "info",
                    if info_open { "info-active" } else { "info" },
                    cx.listener(|this, _, _, cx| {
                        this.info_open = !this.info_open;
                        this.want_details(cx);
                        cx.notify();
                    }),
                )
                .tooltip(ui::tooltip("Info")),
            );

        let row = div()
            .flex()
            .items_center()
            .gap_4()
            .h(px(BAR_ROW))
            // The close icon sits under the app's mark; the right side
            // mirrors that inset.
            .px(px(7.))
            .child(
                div().flex_1().flex().items_center().child(
                    ui::icon_button(
                        "close",
                        "close",
                        cx.listener(|this, _, _, cx| this.begin_close(cx)),
                    )
                    .tooltip(ui::tooltip("Close (Esc)")),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(460.))
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .max_w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(title),
                            )
                            .when(live, |el| el.child(self.render_live_badge(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text_muted())
                            .child(subtitle),
                    ),
            )
            .child(actions);

        // Flat and translucent, like the gallery's header.
        let mut bar_fill = theme::surface();
        bar_fill.a = 0.72;
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bg(bar_fill)
            .child(row)
    }

    /// Details of the photo, in a panel on the right: when, what took it
    /// and how, how big, where, and the app that saved it.
    fn render_info(&self, asset: &Asset) -> impl IntoElement + use<> {
        let details = self.details.get(&asset.id).cloned().flatten();
        let loading = self.details_in_flight.contains(&asset.id);
        let d = details.unwrap_or_default();

        // Size, megapixels, file size.
        let mut size_parts = Vec::new();
        if asset.width > 0 && asset.height > 0 {
            let megapixels = asset.width as f32 * asset.height as f32 / 1_000_000.;
            size_parts.push(format!("{megapixels:.1} MP"));
            size_parts.push(format!("{} \u{00d7} {}", asset.width, asset.height));
        }
        if let Some(original) = asset.original.as_ref().filter(|o| o.size > 0) {
            size_parts.push(format_file_size(original.size));
        }
        let format = asset
            .original
            .as_ref()
            .and_then(|o| format_name(&o.file_type));

        // ISO 1250 · 27 mm · −0.1 ev · ƒ2.2 · 1/11 s
        let mut exposure = Vec::new();
        if let Some(iso) = d.iso {
            exposure.push(format!("ISO {iso}"));
        }
        if let Some(mm) = d.focal_length_35mm {
            exposure.push(format!("{mm} mm"));
        }
        if let Some(ev) = d.exposure_bias {
            exposure.push(format_ev(ev));
        }
        if let Some(f) = d.f_number {
            exposure.push(format!("\u{0192}{}", trim_number(f, 2)));
        }
        if let Some(t) = d.exposure_time {
            exposure.push(format_shutter(t));
        }

        let kind = match asset.kind {
            AssetKind::Photo => None,
            AssetKind::LivePhoto => Some("Live Photo".to_string()),
            AssetKind::Video { duration_secs } => Some(format!(
                "Video, {}:{:02}",
                duration_secs / 60,
                duration_secs % 60
            )),
        };
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        if let Some(kind) = kind {
            rows.push(("Kind", kind));
        }
        if let Some(app) = &d.saved_from {
            rows.push(("Saved from", app.clone()));
        }
        if let Some(software) = &d.software {
            rows.push(("Software", software.clone()));
        }
        if asset.favorite {
            rows.push(("Favorite", "Yes".to_string()));
        }

        let camera_card = div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .p(px(12.))
            .rounded(px(12.))
            .bg(theme::fill())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(match d.camera.clone() {
                        Some(camera) => div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(camera),
                        // Screenshots and saved images say nothing about a camera.
                        None => div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme::text_muted())
                            .child(if loading {
                                "Camera"
                            } else {
                                "No camera information"
                            }),
                    })
                    .when_some(format, |el, format| {
                        el.child(
                            div()
                                .flex_none()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded(px(5.))
                                .bg(theme::fill_hover())
                                .text_size(px(10.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::text_muted())
                                .child(format),
                        )
                    }),
            )
            .when_some(d.lens.clone(), |el, lens| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(lens),
                )
            })
            .when(!size_parts.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(size_parts.join(" \u{00b7} ")),
                )
            })
            .when(!exposure.is_empty(), |el| {
                el.child(
                    div()
                        .mt(px(8.))
                        .pt(px(8.))
                        .border_t_1()
                        .border_color(theme::hover())
                        .flex()
                        .justify_between()
                        .text_size(px(11.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme::text_title())
                        .children(exposure.into_iter().map(|part| div().child(part))),
                )
            })
            .when(d.flash_fired, |el| {
                el.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_meta())
                        .child("Flash fired"),
                )
            });

        let location = d.location.map(|place| {
            let ns = if place.latitude >= 0. { "N" } else { "S" };
            let ew = if place.longitude >= 0. { "E" } else { "W" };
            let mut text = format!(
                "{:.4}\u{00b0} {ns}, {:.4}\u{00b0} {ew}",
                place.latitude.abs(),
                place.longitude.abs()
            );
            if let Some(altitude) = place.altitude {
                text.push_str(&format!(" \u{00b7} {altitude:.0} m"));
            }
            let url = format!(
                "https://www.openstreetmap.org/?mlat={lat:.5}&mlon={lon:.5}#map=15/{lat:.5}/{lon:.5}",
                lat = place.latitude,
                lon = place.longitude
            );
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_meta())
                        .child("Location"),
                )
                .child(div().text_color(theme::text_title()).child(text))
                .child(
                    div().flex().child(ui::link("info-map", "Show on map", move |_, _, cx| {
                        cx.open_url(&url)
                    })),
                )
        });

        div()
            .id("viewer-info")
            .occlude()
            .absolute()
            .top(px(BAR_ROW + 8.))
            .right(px(14.))
            .w(px(300.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(px(16.))
            .bg(theme::popover())
            .shadow_lg()
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text())
                            .truncate()
                            .child(asset.filename.clone().unwrap_or_else(|| "Photo".into())),
                    )
                    .when_some(format_long_date(asset.created_at_ms), |el, date| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_muted())
                                .child(date),
                        )
                    }),
            )
            .child(camera_card)
            .when(loading, |el| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_meta())
                        .child(ui::loading_status(
                            "info-loading",
                            "Loading details\u{2026}",
                            theme::text_meta(),
                        )),
                )
            })
            .children(location)
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
                let path = grid.item_thumbnail(i);
                (i, path)
            })
            .collect();
        let row_top = 26.;
        let mut strip_from = theme::surface();
        strip_from.a = 0.85;
        let mut strip_to = strip_from;
        strip_to.a = 0.;
        Some(
            div()
                .image_cache(self.strip_cache.clone())
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(row_top + STRIP_THUMB + 26.))
                .bg(gpui::linear_gradient(
                    0.,
                    gpui::linear_color_stop(strip_from, 0.),
                    gpui::linear_color_stop(strip_to, 1.),
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
                        .when(this, |el| el.border_2().border_color(theme::text()))
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
        // Videos play as soon as they're shown.
        self.ensure_video(asset.as_ref(), cx);
        let index_now = self.index;
        let mut playing = false;
        if let Some((_, player)) = self.player.as_mut().filter(|(i, _)| *i == index_now) {
            if let Some(frame) = player.next_frame()
                && let Some(old) = self.video_frame.replace(frame)
            {
                cx.drop_image(old, Some(window));
            }
            if let Some(err) = player.error() {
                self.video_error = Some((index_now, err));
            }
            // A Live Photo's motion keeps drawing from the moment it's
            // asked for (the engine may not say it's playing yet), plays
            // once, then the still returns.
            playing = !player.is_paused() || self.live;
            if self.live && player.is_ended() {
                self.stop_video(cx);
                playing = false;
            }
        }
        if self
            .video_error
            .as_ref()
            .is_some_and(|(i, _)| *i == index_now)
        {
            self.stop_video(cx);
        }
        if playing {
            window.request_animation_frame();
        }
        let video_frame = self.video_frame.clone();
        // The controls rest with the bar while playing; paused, they stay.
        let video_controls = self
            .player
            .as_ref()
            .filter(|(i, player)| {
                *i == index_now && !self.live && (self.chrome_shown || player.is_paused())
            })
            .map(|(_, player)| self.render_video_controls(player, cx));
        let note: Option<gpui::SharedString> = self
            .video_error
            .as_ref()
            .filter(|(i, _)| *i == index_now)
            .map(|(_, err)| err.clone().into())
            .or_else(|| {
                asset.as_ref().and_then(|a| match a.kind {
                    AssetKind::Video { .. } if a.video.is_none() => {
                        Some("This video has no playable version. Download to watch.".into())
                    }
                    _ => None,
                })
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

        // The bar fades in when the pointer moves and out when it rests.
        let chrome_shown = self.chrome_shown;
        let top_bar = (chrome_shown
            || self.chrome_hidden_at.elapsed() < Duration::from_millis(CHROME_FADE_MS))
        .then(|| {
            self.render_top_bar(asset.as_ref(), count, zoom_percent, cx)
                .with_animation(
                    ElementId::NamedInteger("viewer-chrome".into(), self.chrome_serial),
                    anim::ease(if chrome_shown { 140 } else { CHROME_FADE_MS }),
                    move |el, t| el.opacity(if chrome_shown { t } else { 1. - t }),
                )
        });
        // Every pointer move in the window, whatever is under it, so the
        // filmstrip and bar don't flicker as the pointer crosses them.
        let weak = cx.weak_entity();
        let pointer = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let weak = weak.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase.bubble() {
                        let pressed = event.pressed_button.is_some();
                        weak.update(cx, |viewer, cx| {
                            viewer.pointer_moved(event.position, pressed, cx)
                        })
                        .ok();
                    }
                });
            },
        )
        .absolute()
        .size_0();
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
        let strip_up = self.strip_shown && !zoomed;
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
            .bg(theme::surface())
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
            // The video's frames over its poster.
            .when_some(video_frame, |el, frame| {
                el.child(
                    place(div()).child(
                        img(gpui::ImageSource::Render(frame))
                            .size_full()
                            .object_fit(ObjectFit::Contain),
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
                    cx.listener(Self::open_menu),
                ))
            })
            .when(has_next, |el| {
                el.child(edge_button(
                    "next",
                    "next",
                    false,
                    cx.listener(|this, _, window, cx| this.next(&Next, window, cx)),
                    cx.listener(Self::open_menu),
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
            .on_action(cx.listener(Self::play_pause))
            .on_action(cx.listener(Self::toggle_favorite))
            .on_action(cx.listener(Self::add_to_last_album))
            .occlude()
            .absolute()
            .inset_0()
            .bg(theme::surface())
            .text_color(theme::text())
            .child(stage)
            .children(strip)
            .when_some(video_controls, |el, controls| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(if strip_up { 112. } else { 24. }))
                        .flex()
                        .justify_center()
                        .child(controls),
                )
            })
            .children(top_bar)
            .children(info)
            .child(pointer)
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

/// "1/120 s", "0.5 s", "2 s".
fn format_shutter(seconds: f64) -> String {
    if seconds >= 0.3 {
        format!("{} s", trim_number(seconds, 1))
    } else {
        format!("1/{} s", (1. / seconds).round() as u64)
    }
}

/// "0 ev", "+0.3 ev", "-1 ev" (with a minus sign).
fn format_ev(ev: f64) -> String {
    let rounded = (ev * 10.).round() / 10.;
    if rounded == 0. {
        "0 ev".to_string()
    } else if rounded > 0. {
        format!("+{} ev", trim_number(rounded, 1))
    } else {
        format!("\u{2212}{} ev", trim_number(-rounded, 1))
    }
}

/// At most `places` decimals, without trailing zeros: 2.2, 1.78, 4.
fn trim_number(n: f64, places: usize) -> String {
    let text = format!("{n:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
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

fn pill(text: impl Into<gpui::SharedString>) -> gpui::Div {
    div()
        .px_3()
        .py_1()
        .rounded_full()
        .bg(gpui::black().opacity(0.6))
        .text_xs()
        .text_color(gpui::white())
        .child(text.into())
}

type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static>;

/// A tall, invisible hit area along one edge, with a chevron that shows on
/// hover. Presses on it don't reach the photo behind (two quick clicks
/// would zoom it), except the right button's, for the menu.
fn edge_button(
    id: &'static str,
    glyph: &'static str,
    left: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_menu: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_click: ClickHandler = Box::new(on_click);
    div()
        .id(id)
        .group(id)
        .block_mouse_except_scroll()
        .on_mouse_down(MouseButton::Right, on_menu)
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
    fn exposure_formats() {
        assert_eq!(format_shutter(1. / 11.), "1/11 s");
        assert_eq!(format_shutter(1. / 120.), "1/120 s");
        assert_eq!(format_shutter(2.), "2 s");
        assert_eq!(format_ev(-0.07421875), "\u{2212}0.1 ev");
        assert_eq!(format_ev(0.01), "0 ev");
        assert_eq!(format_ev(0.33), "+0.3 ev");
        assert_eq!(trim_number(2.2, 2), "2.2");
        assert_eq!(trim_number(1.78, 2), "1.78");
        assert_eq!(trim_number(4., 2), "4");
    }

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
