//! The photo grid: a virtualized list of rows, filled from iCloud on demand.
//!
//! Only the rows on screen are built. Items load in pages as they come into
//! view, and thumbnails download (to a disk cache) for visible tiles first.
//! Decoded thumbnails live in a bounded GPU cache, so memory stays flat no
//! matter how far you scroll.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Album, Asset, AssetId, AssetKind, PhotosClient, PhotosError, Resource};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, Entity, EventEmitter, Focusable,
    FontWeight, ObjectFit, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle,
    Window, bounce, div, ease_in_out, ease_out_quint, img, prelude::*, px, uniform_list,
};

use crate::image_cache::LruImageCache;
use crate::viewer::{Viewer, ViewerEvent};
use crate::{net, theme, thumbs};

/// Items per CloudKit page request.
const PAGE: usize = 100;
const MAX_PAGE_REQUESTS: usize = 2;
const MAX_THUMB_REQUESTS: usize = 8;
/// Rows beyond the visible ones to load ahead.
const PREFETCH_ROWS: usize = 6;
/// Decoded thumbnails kept on the GPU.
const IMAGE_CACHE_SIZE: usize = 600;

pub enum GridEvent {
    SessionExpired,
}

enum LoadState {
    Loading,
    Ready { count: usize },
    Failed(SharedString),
}

pub struct PhotoGrid {
    client: Arc<PhotosClient>,
    thumb_dir: Arc<PathBuf>,
    album: Album,
    /// Bumped on album change so results for the old album are ignored.
    generation: u64,
    state: LoadState,
    /// Timeline index (0 = newest) to item.
    assets: HashMap<usize, Asset>,
    pages_requested: HashSet<usize>,
    page_queue: Vec<usize>,
    pages_in_flight: usize,
    thumbs_ready: HashMap<AssetId, Arc<Path>>,
    thumbs_failed: HashSet<AssetId>,
    thumbs_in_flight: HashSet<AssetId>,
    /// Thumbnails to fetch, highest priority last.
    thumb_queue: Vec<(AssetId, Resource)>,
    /// Shown above the grid when something failed; cleared by Retry.
    error: Option<SharedString>,
    image_cache: Entity<LruImageCache>,
    preview_dir: Arc<PathBuf>,
    viewer: Option<(Entity<Viewer>, Subscription)>,
    scroll: UniformListScrollHandle,
    /// Columns at the last render, for scrolling to an item.
    columns: usize,
}

impl EventEmitter<GridEvent> for PhotoGrid {}

impl PhotoGrid {
    pub fn new(
        client: Arc<PhotosClient>,
        dsid: &str,
        album: Album,
        cx: &mut Context<Self>,
    ) -> Self {
        let preview_dir = Arc::new(thumbs::preview_dir(dsid));
        let prune_dir = preview_dir.clone();
        cx.background_spawn(async move { thumbs::prune(&prune_dir, thumbs::PREVIEWS_KEPT) })
            .detach();
        let mut grid = Self {
            client,
            thumb_dir: Arc::new(thumbs::cache_dir(dsid)),
            album,
            generation: 0,
            state: LoadState::Loading,
            assets: HashMap::new(),
            pages_requested: HashSet::new(),
            page_queue: Vec::new(),
            pages_in_flight: 0,
            thumbs_ready: HashMap::new(),
            thumbs_failed: HashSet::new(),
            thumbs_in_flight: HashSet::new(),
            thumb_queue: Vec::new(),
            error: None,
            image_cache: cx.new(|cx| LruImageCache::new(IMAGE_CACHE_SIZE, cx)),
            preview_dir,
            viewer: None,
            scroll: UniformListScrollHandle::new(),
            columns: 1,
        };
        grid.load_count(cx);
        grid
    }

    pub fn set_album(&mut self, album: Album, cx: &mut Context<Self>) {
        if album != self.album {
            self.album = album;
            self.reload(cx);
        }
    }

    pub fn item_count(&self) -> usize {
        match self.state {
            LoadState::Ready { count } => count,
            _ => 0,
        }
    }

    pub fn asset(&self, index: usize) -> Option<&Asset> {
        self.assets.get(&index)
    }

    pub fn thumbnail_path(&self, id: &AssetId) -> Option<Arc<Path>> {
        self.thumbs_ready.get(id).cloned()
    }

    /// Load the page containing `index` soon, ahead of anything else queued.
    pub fn ensure_loaded(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.item_count() || self.assets.contains_key(&index) {
            return;
        }
        let page = index / PAGE;
        if self.pages_requested.insert(page) {
            self.page_queue.push(page);
            self.pump(cx);
        }
    }

    fn open_viewer(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let grid = cx.entity();
        let client = self.client.clone();
        let preview_dir = self.preview_dir.clone();
        let viewer = cx.new(|cx| Viewer::new(&grid, client, preview_dir, index, cx));
        let subscription = cx.subscribe(&viewer, |this, _, event, cx| match event {
            ViewerEvent::Closed { index } => this.close_viewer(*index, cx),
        });
        viewer.focus_handle(cx).focus(window);
        self.viewer = Some((viewer, subscription));
        cx.notify();
    }

    fn close_viewer(&mut self, index: usize, cx: &mut Context<Self>) {
        self.viewer = None;
        // Keep the photo you were looking at in view.
        self.scroll
            .scroll_to_item(index / self.columns.max(1), ScrollStrategy::Center);
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.viewer = None;
        self.generation += 1;
        self.state = LoadState::Loading;
        self.assets.clear();
        self.pages_requested.clear();
        self.page_queue.clear();
        self.pages_in_flight = 0;
        self.thumbs_failed.clear();
        self.thumbs_in_flight.clear();
        self.thumb_queue.clear();
        self.error = None;
        self.load_count(cx);
        cx.notify();
    }

    fn load_count(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let album = self.album;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = net::run(move || {
                client.check_ready()?;
                client.count(album)
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.state = match result {
                    Some(Ok(count)) => LoadState::Ready { count },
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => LoadState::Failed(err.to_string().into()),
                    None => LoadState::Failed("Something went wrong loading the library.".into()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Called while rendering: queue the pages and thumbnails for what's on screen.
    fn request_visible(&mut self, rows: Range<usize>, columns: usize, cx: &mut Context<Self>) {
        let LoadState::Ready { count } = self.state else {
            return;
        };
        if count == 0 || self.error.is_some() {
            return;
        }

        let ahead = rows.start.saturating_sub(PREFETCH_ROWS)..rows.end + PREFETCH_ROWS;
        let first = (ahead.start * columns).min(count - 1);
        let last = (ahead.end * columns).min(count) - 1;
        // Pages furthest from the viewport go first in the queue, so the
        // visible ones pop off first.
        let visible_page = (rows.start * columns).min(count - 1) / PAGE;
        let mut pages: Vec<usize> = (first / PAGE..=last / PAGE)
            .filter(|page| !self.pages_requested.contains(page))
            .collect();
        pages.sort_by_key(|page| std::cmp::Reverse(page.abs_diff(visible_page)));
        for page in pages {
            self.pages_requested.insert(page);
            self.page_queue.push(page);
        }

        // Thumbnails: only for visible tiles, in reading order.
        let visible = (rows.start * columns).min(count)..(rows.end * columns).min(count);
        self.thumb_queue = visible
            .rev()
            .filter_map(|ix| {
                let asset = self.assets.get(&ix)?;
                let resource = asset.thumbnail.clone()?;
                let id = &asset.id;
                let needed = !self.thumbs_ready.contains_key(id)
                    && !self.thumbs_failed.contains(id)
                    && !self.thumbs_in_flight.contains(id);
                needed.then(|| (id.clone(), resource))
            })
            .collect();

        self.pump(cx);
    }

    /// Start queued requests, up to the concurrency limits.
    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.pages_in_flight < MAX_PAGE_REQUESTS {
            let Some(page) = self.page_queue.pop() else {
                break;
            };
            self.start_page(page, cx);
        }
        while self.thumbs_in_flight.len() < MAX_THUMB_REQUESTS {
            let Some((id, resource)) = self.thumb_queue.pop() else {
                break;
            };
            self.start_thumb(id, resource, cx);
        }
    }

    fn start_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let LoadState::Ready { count } = self.state else {
            return;
        };
        let start = page * PAGE;
        let end = (start + PAGE).min(count);
        if start >= end {
            return;
        }
        // CloudKit ranks run newest first, the same order as the grid.
        let client = self.client.clone();
        let album = self.album;
        let generation = self.generation;
        self.pages_in_flight += 1;

        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.fetch(album, start, end - start)).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.pages_in_flight -= 1;
                match result {
                    Some(Ok(assets)) => {
                        for (offset, asset) in assets.into_iter().enumerate() {
                            this.assets.insert(start + offset, asset);
                        }
                    }
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => this.fail(page, err.to_string(), cx),
                    None => this.fail(page, "Loading photos failed.".into(), cx),
                }
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn fail(&mut self, page: usize, message: String, cx: &mut Context<Self>) {
        // Stop requesting until the user retries, rather than hammering iCloud.
        self.pages_requested.remove(&page);
        self.page_queue.clear();
        self.error = Some(message.into());
        cx.notify();
    }

    fn start_thumb(&mut self, id: AssetId, resource: Resource, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let dir = self.thumb_dir.clone();
        let generation = self.generation;
        self.thumbs_in_flight.insert(id.clone());

        cx.spawn(async move |this, cx| {
            let task_id = id.clone();
            let result = net::run(move || thumbs::ensure(&client, &dir, &task_id, &resource)).await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.thumbs_in_flight.remove(&id);
                }
                match result {
                    Some(Ok(path)) => {
                        this.thumbs_ready.insert(id, path.into());
                    }
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    _ => {
                        this.thumbs_failed.insert(id);
                    }
                }
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        if let LoadState::Failed(_) = self.state {
            self.reload(cx);
            return;
        }
        self.error = None;
        self.thumbs_failed.clear();
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let subtitle = match &self.state {
            LoadState::Ready { count } => format_count(*count),
            _ => String::new(),
        };
        div()
            .px(px(theme::GRID_PADDING))
            .pt_4()
            .pb_2()
            .flex()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(album_title(self.album)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child(subtitle),
            )
            .child(div().flex_1())
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_sm()
                        .child(div().text_color(theme::danger()).child(error))
                        .child(retry_link(cx)),
                )
            })
    }
}

impl Render for PhotoGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let body: AnyElement = match &self.state {
            LoadState::Loading => centered(
                div().child("Loading your library\u{2026}").with_animation(
                    "library-loading",
                    Animation::new(Duration::from_millis(1100))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    |el, t| el.opacity(0.4 + 0.6 * t),
                ),
            ),
            LoadState::Failed(message) => centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .max_w(px(420.))
                    .text_center()
                    .child(div().text_color(theme::danger()).child(message.clone()))
                    .child(retry_link(cx)),
            ),
            LoadState::Ready { count: 0 } => centered(div().child(empty_message(self.album))),
            LoadState::Ready { count } => {
                let count = *count;
                let grid_width =
                    f32::from(window.viewport_size().width) - f32::from(theme::SIDEBAR_WIDTH);
                let columns = columns_for(grid_width).max(1);
                let tile_size = tile_size_for(grid_width, columns);
                self.columns = columns;
                let rows = count.div_ceil(columns);
                div()
                    .flex_1()
                    .image_cache(self.image_cache.clone())
                    .child(
                        uniform_list(
                            ElementId::NamedInteger("photo-grid".into(), self.generation),
                            rows,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                this.request_visible(range.clone(), columns, cx);
                                range
                                    .map(|row| {
                                        let start = row * columns;
                                        let end = (start + columns).min(count);
                                        div()
                                            .flex()
                                            .gap(px(theme::TILE_GAP))
                                            .px(px(theme::GRID_PADDING))
                                            .pb(px(theme::TILE_GAP))
                                            .children((start..end).map(|ix| {
                                                let asset = this.assets.get(&ix);
                                                let thumb = asset.and_then(|a| {
                                                    this.thumbs_ready.get(&a.id).cloned()
                                                });
                                                tile(ix, tile_size, asset, thumb)
                                                    .map(|tile| {
                                                        tile.on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.open_viewer(ix, window, cx)
                                                            },
                                                        ))
                                                        .into_any_element()
                                                    })
                                                    .unwrap_or_else(|| empty_tile(ix, tile_size))
                                            }))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full(),
                    )
                    .into_any_element()
            }
        };

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .when_some(self.viewer.as_ref(), |el, (viewer, _)| {
                el.child(viewer.clone())
            })
    }
}

fn empty_tile(index: usize, size: f32) -> AnyElement {
    tile_base(index, size).into_any_element()
}

fn tile_base(index: usize, size: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(ElementId::NamedInteger("tile".into(), index as u64))
        .size(px(size))
        .flex_none()
        .relative()
        .rounded_sm()
        .overflow_hidden()
        .bg(theme::field())
}

/// A loaded tile; `None` while the item itself hasn't loaded.
fn tile(
    index: usize,
    size: f32,
    asset: Option<&Asset>,
    thumb: Option<Arc<Path>>,
) -> Option<gpui::Stateful<gpui::Div>> {
    let asset = asset?;
    let base = tile_base(index, size);

    let badge = match asset.kind {
        AssetKind::Photo => None,
        AssetKind::LivePhoto => Some("LIVE".to_string()),
        AssetKind::Video { duration_secs } => {
            Some(format!("{}:{:02}", duration_secs / 60, duration_secs % 60))
        }
    };

    base.cursor_pointer()
        .when_some(thumb, |el, path| {
            el.child(
                img(path)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    // Fade in when the thumbnail arrives.
                    .with_animation(
                        ElementId::NamedInteger("thumb-fade".into(), index as u64),
                        Animation::new(Duration::from_millis(200)).with_easing(ease_out_quint()),
                        |el, t| el.opacity(t),
                    ),
            )
        })
        .child(
            // Hover highlight.
            div()
                .absolute()
                .inset_0()
                .hover(|el| el.bg(gpui::white().opacity(0.08))),
        )
        .when_some(badge, |el, badge| {
            el.child(
                div()
                    .absolute()
                    .bottom_1()
                    .right_1()
                    .px_1()
                    .rounded_sm()
                    .bg(gpui::black().opacity(0.45))
                    .text_color(gpui::white())
                    .text_xs()
                    .child(badge),
            )
        })
        .when(asset.favorite, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom_1()
                    .left_1()
                    .text_xs()
                    .text_color(theme::favorite())
                    .child("\u{2665}"),
            )
        })
        .into()
}

fn centered(content: impl IntoElement) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(theme::text_muted())
        .child(content)
        .into_any_element()
}

fn retry_link(cx: &mut Context<PhotoGrid>) -> impl IntoElement + use<> {
    div()
        .id("retry")
        .text_sm()
        .text_color(theme::accent())
        .cursor_pointer()
        .hover(|el| el.underline())
        .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))
        .child("Retry")
}

pub fn album_title(album: Album) -> &'static str {
    match album {
        Album::All => "Library",
        Album::Favorites => "Favorites",
        Album::Videos => "Videos",
        Album::RecentlyDeleted => "Recently Deleted",
    }
}

fn empty_message(album: Album) -> &'static str {
    match album {
        Album::All => "Your library is empty.",
        Album::Favorites => "No favorites yet.",
        Album::Videos => "No videos.",
        Album::RecentlyDeleted => "Nothing recently deleted.",
    }
}

fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    format!("{out} {}", if count == 1 { "item" } else { "items" })
}

/// Tile edge length that fills `width` exactly with `columns` tiles.
fn tile_size_for(width: f32, columns: usize) -> f32 {
    let usable = width - theme::GRID_PADDING * 2. - theme::TILE_GAP * (columns as f32 - 1.);
    (usable / columns as f32).floor().max(1.)
}

/// How many tiles of about `TILE_SIZE` fit across `width` pixels.
fn columns_for(width: f32) -> usize {
    let usable = (width - theme::GRID_PADDING * 2.).max(theme::TILE_SIZE);
    ((usable + theme::TILE_GAP) / (theme::TILE_SIZE + theme::TILE_GAP)).floor() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_least_one_column() {
        assert_eq!(columns_for(10.), 1);
    }

    #[test]
    fn columns_grow_with_width() {
        assert!(columns_for(1400.) > columns_for(700.));
    }

    #[test]
    fn tiles_fill_the_width() {
        for width in [700., 980., 1333., 2400.] {
            let columns = columns_for(width);
            let size = tile_size_for(width, columns);
            let used = size * columns as f32
                + theme::TILE_GAP * (columns as f32 - 1.)
                + theme::GRID_PADDING * 2.;
            assert!(width - used < columns as f32, "width {width}: {used} used");
            assert!(
                size >= theme::TILE_SIZE * 0.95,
                "tiles shouldn't shrink much"
            );
        }
    }

    #[test]
    fn counts_are_readable() {
        assert_eq!(format_count(1), "1 item");
        assert_eq!(format_count(999), "999 items");
        assert_eq!(format_count(12_345), "12,345 items");
        assert_eq!(format_count(1_234_567), "1,234,567 items");
    }
}
