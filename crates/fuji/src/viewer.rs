//! Full-size photo viewer, shown over the grid.
//!
//! Shows the grid thumbnail at once (scaled up), then fades in the large
//! JPEG preview when it arrives. Neighbouring previews are fetched ahead so
//! arrowing through photos feels instant. Copy puts the preview on the
//! clipboard; Download saves the original file (HEIC, RAW, MOV, ...).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Asset, AssetId, AssetKind, PhotosClient, PhotosError, Resource};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClipboardItem, Context, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, Image, ImageFormat, KeyBinding, ObjectFit,
    SharedString, Subscription, WeakEntity, Window, actions, bounce, div, ease_in_out,
    ease_out_quint, img, prelude::*, px,
};

use crate::grid::PhotoGrid;
use crate::image_cache::LruImageCache;
use crate::{net, theme, thumbs};

actions!(viewer, [Close, Previous, Next, CopyImage, Download]);

const CONTEXT: &str = "Viewer";
const MAX_PREVIEW_REQUESTS: usize = 3;
/// How far ahead and behind to fetch previews.
const PREFETCH: usize = 2;
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Close, Some(CONTEXT)),
        KeyBinding::new("left", Previous, Some(CONTEXT)),
        KeyBinding::new("right", Next, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", CopyImage, Some(CONTEXT)),
        KeyBinding::new("ctrl-s", Download, Some(CONTEXT)),
    ]);
}

pub enum ViewerEvent {
    /// The viewer closed while showing this timeline index.
    Closed { index: usize },
}

#[derive(Clone)]
enum Status {
    Working(SharedString),
    Done {
        message: SharedString,
        /// Offer "Show in folder" for a saved file.
        reveal: Option<PathBuf>,
    },
    Failed(SharedString),
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
    /// Copy was pressed before the preview arrived.
    copy_pending: Option<AssetId>,
    status: Option<Status>,
    /// Bumped on every status change, so old timeouts don't clear new messages.
    status_generation: u64,
    image_cache: Entity<LruImageCache>,
    _grid_observer: Subscription,
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
            copy_pending: None,
            status: None,
            status_generation: 0,
            image_cache: cx.new(|cx| LruImageCache::new(PREFETCH * 2 + 4, cx)),
            _grid_observer: observer,
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
                        this.previews_ready.insert(id.clone(), path.into());
                        if this.copy_pending.as_ref() == Some(&id) {
                            this.copy_pending = None;
                            this.copy_current(cx);
                        }
                    }
                    _ => {
                        this.previews_failed.insert(id.clone());
                        if this.copy_pending.as_ref() == Some(&id) {
                            this.copy_pending = None;
                            this.set_status(Status::Failed("Couldn't load the photo.".into()), cx);
                        }
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
        self.index = index;
        self.copy_pending = None;
        self.load_around(cx);
        cx.notify();
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
        cx.emit(ViewerEvent::Closed { index: self.index });
    }

    fn copy(&mut self, _: &CopyImage, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_current(cx);
    }

    fn copy_current(&mut self, cx: &mut Context<Self>) {
        let Some(asset) = self.asset(self.index, cx) else {
            return;
        };
        let Some(path) = self.previews_ready.get(&asset.id).cloned() else {
            // Copy once the preview arrives.
            self.copy_pending = Some(asset.id);
            self.set_status(Status::Working("Copying\u{2026}".into()), cx);
            return;
        };
        self.set_status(Status::Working("Copying\u{2026}".into()), cx);
        cx.spawn(async move |this, cx| {
            let bytes = cx
                .background_spawn(async move { std::fs::read(&path) })
                .await;
            this.update(cx, |this, cx| match bytes {
                Ok(bytes) => {
                    cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                        ImageFormat::Jpeg,
                        bytes,
                    )));
                    this.set_status(
                        Status::Done {
                            message: "Copied to clipboard".into(),
                            reveal: None,
                        },
                        cx,
                    );
                }
                Err(_) => this.set_status(Status::Failed("Couldn't copy the photo.".into()), cx),
            })
            .ok();
        })
        .detach();
    }

    fn download(&mut self, _: &Download, _: &mut Window, cx: &mut Context<Self>) {
        self.download_current(cx);
    }

    fn download_current(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Some(Status::Working(_))) {
            return;
        }
        let Some(asset) = self.asset(self.index, cx) else {
            return;
        };
        let Some(original) = asset.original.clone() else {
            self.set_status(
                Status::Failed("The original isn't available for this item.".into()),
                cx,
            );
            return;
        };
        let name = suggested_name(&asset, &original);
        let folder = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let chosen = cx.prompt_for_new_path(&folder, Some(&name));
        let client = self.client.clone();

        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.set_status(Status::Working("Downloading original\u{2026}".into()), cx)
            })
            .ok();
            let target = path.clone();
            let result = net::run(move || client.download_to(&original, &target)).await;
            this.update(cx, |this, cx| {
                let status = match result {
                    Some(Ok(_)) => Status::Done {
                        message: format!(
                            "Saved {}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        )
                        .into(),
                        reveal: Some(path),
                    },
                    Some(Err(PhotosError::SessionExpired)) => {
                        Status::Failed("Your session expired. Sign in again to download.".into())
                    }
                    Some(Err(err)) => Status::Failed(err.to_string().into()),
                    None => Status::Failed("Download failed.".into()),
                };
                this.set_status(status, cx);
            })
            .ok();
        })
        .detach();
    }

    fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status_generation += 1;
        let generation = self.status_generation;
        let finished = !matches!(status, Status::Working(_));
        self.status = Some(status);
        cx.notify();
        if finished {
            // Clear the message after a moment, unless something newer replaced it.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(STATUS_TIMEOUT).await;
                this.update(cx, |this, cx| {
                    if this.status_generation == generation {
                        this.status = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn render_top_bar(
        &self,
        asset: Option<&Asset>,
        count: usize,
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

        let status: Option<AnyElement> = self.status.clone().map(|status| match status {
            Status::Working(text) => div()
                .text_color(theme::text_muted())
                .child(text)
                .with_animation(
                    "status-pulse",
                    Animation::new(Duration::from_millis(1100))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    |el, t| el.opacity(0.5 + 0.5 * t),
                )
                .into_any_element(),
            Status::Done { message, reveal } => div()
                .flex()
                .gap_2()
                .child(div().text_color(theme::text()).child(message))
                .when_some(reveal, |el, path| {
                    el.child(link("reveal", "Show in folder", move |_, _, cx| {
                        cx.reveal_path(&path)
                    }))
                })
                .into_any_element(),
            Status::Failed(text) => div()
                .text_color(theme::danger())
                .child(text)
                .into_any_element(),
        });

        div()
            .flex()
            .items_center()
            .gap_4()
            .h(px(52.))
            .px_4()
            .flex_none()
            .text_sm()
            .child(icon_button(
                "close",
                "\u{2715}",
                cx.listener(|this, _, window, cx| this.close(&Close, window, cx)),
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
            .children(status)
            .child(text_button(
                "copy",
                "Copy",
                cx.listener(|this, _, _, cx| this.copy_current(cx)),
            ))
            .child(text_button(
                "download",
                "Download",
                cx.listener(|this, _, _, cx| this.download_current(cx)),
            ))
            .child(div().text_color(theme::text_muted()).child(position))
    }
}

impl Render for Viewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let top_bar = self.render_top_bar(asset.as_ref(), count, cx);
        let index = self.index;
        let has_previous = index > 0;
        let has_next = index + 1 < count;

        let stage = div()
            .image_cache(self.image_cache.clone())
            .id("viewer-stage")
            .flex_1()
            .relative()
            .overflow_hidden()
            // Upscaled thumbnail first, so there's never a blank screen.
            .when_some(thumb, |el, path| {
                el.child(
                    div()
                        .absolute()
                        .inset_4()
                        .child(img(path).size_full().object_fit(ObjectFit::Contain)),
                )
            })
            .when_some(preview, |el, path| {
                el.child(
                    div()
                        .absolute()
                        .inset_4()
                        .child(img(path).size_full().object_fit(ObjectFit::Contain))
                        .with_animation(
                            ElementId::NamedInteger("preview-fade".into(), index as u64),
                            Animation::new(Duration::from_millis(220))
                                .with_easing(ease_out_quint()),
                            |el, t| el.opacity(t),
                        ),
                )
            })
            .when(loading, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_4()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            pill("Loading\u{2026}").with_animation(
                                "preview-loading",
                                Animation::new(Duration::from_millis(1100))
                                    .repeat()
                                    .with_easing(bounce(ease_in_out)),
                                |el, t| el.opacity(0.4 + 0.6 * t),
                            ),
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
                    "\u{2039}",
                    true,
                    cx.listener(|this, _, window, cx| this.previous(&Previous, window, cx)),
                ))
            })
            .when(has_next, |el| {
                el.child(edge_button(
                    "next",
                    "\u{203A}",
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
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(gpui::black().opacity(0.96))
            .text_color(theme::text())
            .child(top_bar)
            .child(stage)
            .with_animation(
                "viewer-open",
                Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t),
            )
    }
}

fn suggested_name(asset: &Asset, original: &Resource) -> String {
    if let Some(name) = asset.filename.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let ext = match original.file_type.as_str() {
        "public.heic" => "HEIC",
        "public.heif" => "HEIF",
        "public.png" => "PNG",
        "com.apple.quicktime-movie" => "MOV",
        "public.mpeg-4" => "MP4",
        "com.adobe.raw-image" => "DNG",
        _ => "JPG",
    };
    format!("photo.{ext}")
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

fn icon_button(
    id: &'static str,
    glyph: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .text_color(theme::text_muted())
        .hover(|el| el.bg(gpui::white().opacity(0.08)).text_color(theme::text()))
        .on_click(on_click)
        .child(glyph)
}

fn text_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(gpui::white().opacity(0.06))
        .hover(|el| el.bg(gpui::white().opacity(0.12)))
        .active(|el| el.bg(gpui::white().opacity(0.18)))
        .on_click(on_click)
        .child(label)
}

fn link(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .text_color(theme::accent())
        .cursor_pointer()
        .hover(|el| el.underline())
        .on_click(on_click)
        .child(label)
}

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
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(gpui::black().opacity(0.5))
                .text_color(gpui::white())
                .text_2xl()
                .opacity(0.)
                .group_hover(id, |el| el.opacity(1.))
                .child(glyph),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn names_fall_back_to_type() {
        let original = Resource {
            url: String::new(),
            width: 0,
            height: 0,
            size: 0,
            file_type: "public.heic".into(),
        };
        let mut asset = Asset {
            id: AssetId("a".into()),
            kind: AssetKind::Photo,
            filename: None,
            width: 0,
            height: 0,
            favorite: false,
            created_at_ms: 0,
            thumbnail: None,
            preview: None,
            original: None,
        };
        assert_eq!(suggested_name(&asset, &original), "photo.HEIC");
        asset.filename = Some("IMG_1234.HEIC".into());
        assert_eq!(suggested_name(&asset, &original), "IMG_1234.HEIC");
    }
}
