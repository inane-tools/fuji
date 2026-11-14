//! The gallery: a mosaic of the album under year, month and day headers.
//!
//! - The album's index (date, shape, type of every item) comes from a disk
//!   cache at once, and is refreshed from iCloud in the background.
//! - The mosaic is laid out from the index ([`crate::layout`]); only rows on
//!   screen are built.
//! - Thumbnails already on disk show without any network request; others are
//!   fetched for what's visible, using download URLs from full records loaded
//!   by page.
//! - Decoded thumbnails live in a bounded GPU cache, so memory stays flat.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Album, Asset, AssetId, PhotosClient, PhotosError, Resource};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, ElementId, Entity, EventEmitter, Focusable,
    FontWeight, ObjectFit, ScrollHandle, SharedString, Subscription, Window, bounce, div,
    ease_in_out, ease_out_quint, img, point, prelude::*, px,
};

use crate::assets::icon;
use crate::image_cache::LruImageCache;
use crate::index::{self, IndexItem, Kind};
use crate::layout::{self, Layout, RowKind};
use crate::viewer::{Viewer, ViewerEvent};
use crate::{net, theme, thumbs};

/// Items per full-record page (with download URLs).
const PAGE: usize = 100;
/// Full-record pages kept in memory.
const MAX_PAGES: usize = 40;
const MAX_PAGE_REQUESTS: usize = 2;
/// Items per index-scan request (small records, no URLs).
const SCAN_PAGE: usize = 200;
const MAX_SCAN_REQUESTS: usize = 6;
const MAX_THUMB_REQUESTS: usize = 8;
/// Extra content kept built above and below the viewport.
const OVERSCAN: f32 = 600.;
/// Decoded thumbnails kept in memory: a couple of screenfuls.
const IMAGE_CACHE_SIZE: usize = 120;
const TARGET_ROW_HEIGHT: f32 = 200.;
const TILE_GAP: f32 = 4.;

pub enum GridEvent {
    SessionExpired,
}

enum LoadState {
    /// No index yet (no cache, first scan pages not back).
    Loading,
    Ready,
    Failed(SharedString),
}

/// A full refresh of the album index from iCloud.
struct Scan {
    count: usize,
    next_rank: usize,
    in_flight: usize,
    /// Pages received, by first rank.
    pages: HashMap<usize, Vec<IndexItem>>,
    /// Show results as they arrive (nothing cached to show meanwhile).
    progressive: bool,
}

pub struct PhotoGrid {
    client: Arc<PhotosClient>,
    account_dir: PathBuf,
    thumb_dir: Arc<PathBuf>,
    preview_dir: Arc<PathBuf>,
    album: Album,
    /// Bumped on album change so results for the old album are ignored.
    generation: u64,
    state: LoadState,
    index: Vec<IndexItem>,
    scan: Option<Scan>,
    layout: Option<Layout>,
    layout_dirty: bool,
    // Full records (download URLs), by page of rank.
    assets: HashMap<AssetId, Asset>,
    page_ids: HashMap<usize, Vec<AssetId>>,
    page_lru: VecDeque<usize>,
    pages_requested: HashSet<usize>,
    page_queue: Vec<usize>,
    pages_in_flight: usize,
    // Thumbnails.
    disk_thumbs: Option<Arc<HashSet<String>>>,
    thumbs_ready: HashMap<AssetId, Arc<Path>>,
    thumbs_failed: HashSet<AssetId>,
    thumbs_in_flight: HashSet<AssetId>,
    /// Thumbnails to fetch, highest priority last.
    thumb_queue: Vec<(AssetId, Resource)>,
    error: Option<SharedString>,
    image_cache: Entity<LruImageCache>,
    viewer: Option<(Entity<Viewer>, Subscription)>,
    scroll: ScrollHandle,
}

impl EventEmitter<GridEvent> for PhotoGrid {}

impl PhotoGrid {
    pub fn new(
        client: Arc<PhotosClient>,
        dsid: &str,
        album: Album,
        cx: &mut Context<Self>,
    ) -> Self {
        let thumb_dir = Arc::new(thumbs::cache_dir(dsid));
        let preview_dir = Arc::new(thumbs::preview_dir(dsid));
        let account_dir = thumb_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| thumb_dir.to_path_buf());

        let prune_dir = preview_dir.clone();
        cx.background_spawn(async move { thumbs::prune(&prune_dir, thumbs::PREVIEWS_KEPT) })
            .detach();

        let mut grid = Self {
            client,
            account_dir,
            thumb_dir,
            preview_dir,
            album,
            generation: 0,
            state: LoadState::Loading,
            index: Vec::new(),
            scan: None,
            layout: None,
            layout_dirty: true,
            assets: HashMap::new(),
            page_ids: HashMap::new(),
            page_lru: VecDeque::new(),
            pages_requested: HashSet::new(),
            page_queue: Vec::new(),
            pages_in_flight: 0,
            disk_thumbs: None,
            thumbs_ready: HashMap::new(),
            thumbs_failed: HashSet::new(),
            thumbs_in_flight: HashSet::new(),
            thumb_queue: Vec::new(),
            error: None,
            image_cache: cx.new(|cx| LruImageCache::new(IMAGE_CACHE_SIZE, cx)),
            viewer: None,
            scroll: ScrollHandle::new(),
        };
        grid.list_disk_thumbs(cx);
        grid.load_album(cx);
        grid
    }

    pub fn set_album(&mut self, album: Album, cx: &mut Context<Self>) {
        if album != self.album {
            self.album = album;
            self.load_album(cx);
        }
    }

    // ---- What the viewer reads -------------------------------------------

    pub fn item_count(&self) -> usize {
        self.index.len()
    }

    /// The full record (with download URLs) at `index`, if loaded.
    pub fn asset(&self, index: usize) -> Option<&Asset> {
        let item = self.index.get(index)?;
        self.assets.get(&AssetId(item.id.clone()))
    }

    pub fn thumbnail_path(&self, id: &AssetId) -> Option<Arc<Path>> {
        self.thumbs_ready.get(id).cloned().or_else(|| {
            let path = thumbs::path_for(&self.thumb_dir, id);
            self.on_disk(&path).then(|| path.into())
        })
    }

    /// Load the full record for `index` soon, ahead of anything else queued.
    pub fn ensure_loaded(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.index.len() && self.asset(index).is_none() {
            self.request_page(index / PAGE);
            self.pump(cx);
        }
    }

    // ---- Album index ------------------------------------------------------

    fn load_album(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.state = LoadState::Loading;
        self.index.clear();
        self.scan = None;
        self.layout = None;
        self.layout_dirty = true;
        self.assets.clear();
        self.page_ids.clear();
        self.page_lru.clear();
        self.pages_requested.clear();
        self.page_queue.clear();
        self.pages_in_flight = 0;
        self.thumbs_failed.clear();
        self.thumbs_in_flight.clear();
        self.thumb_queue.clear();
        self.error = None;
        self.viewer = None;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();

        let generation = self.generation;
        let path = index::cache_path(&self.account_dir, self.album);
        let client = self.client.clone();
        let album = self.album;
        cx.spawn(async move |this, cx| {
            // Cached index first, so the gallery shows right away.
            let cached = cx.background_spawn(async move { index::load(&path) }).await;
            let had_cache = cached.is_some();
            this.update(cx, |this, cx| {
                if this.generation == generation
                    && let Some(items) = cached
                {
                    this.index = items;
                    this.state = LoadState::Ready;
                    this.layout_dirty = true;
                    cx.notify();
                }
            })
            .ok();

            // Then refresh from iCloud.
            let count = net::run(move || {
                client.check_ready()?;
                client.count(album)
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match count {
                    Some(Ok(count)) => this.start_scan(count, !had_cache, cx),
                    Some(Err(PhotosError::SessionExpired)) => cx.emit(GridEvent::SessionExpired),
                    Some(Err(err)) => this.fail(err.to_string(), cx),
                    None => this.fail("Something went wrong loading the library.".into(), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    fn start_scan(&mut self, count: usize, progressive: bool, cx: &mut Context<Self>) {
        if count == 0 {
            self.index.clear();
            self.state = LoadState::Ready;
            self.layout_dirty = true;
            index::save(&index::cache_path(&self.account_dir, self.album), &[]);
            cx.notify();
            return;
        }
        self.scan = Some(Scan {
            count,
            next_rank: 0,
            in_flight: 0,
            pages: HashMap::new(),
            progressive,
        });
        self.pump_scan(cx);
        cx.notify();
    }

    fn pump_scan(&mut self, cx: &mut Context<Self>) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        while scan.in_flight < MAX_SCAN_REQUESTS && scan.next_rank < scan.count {
            let start = scan.next_rank;
            let limit = SCAN_PAGE.min(scan.count - start);
            scan.next_rank += limit;
            scan.in_flight += 1;

            let client = self.client.clone();
            let album = self.album;
            let generation = self.generation;
            cx.spawn(async move |this, cx| {
                let result = net::run(move || client.scan(album, start, limit)).await;
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match result {
                        Some(Ok(assets)) => {
                            let items = assets.iter().map(IndexItem::from).collect();
                            this.scan_page_done(start, items, cx);
                        }
                        Some(Err(PhotosError::SessionExpired)) => {
                            cx.emit(GridEvent::SessionExpired)
                        }
                        Some(Err(err)) => {
                            this.scan = None;
                            this.fail(err.to_string(), cx);
                        }
                        None => {
                            this.scan = None;
                            this.fail("Updating the library failed.".into(), cx);
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn scan_page_done(&mut self, start: usize, items: Vec<IndexItem>, cx: &mut Context<Self>) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        scan.in_flight -= 1;
        scan.pages.insert(start, items);

        // The contiguous prefix received so far.
        let mut prefix = Vec::new();
        let mut rank = 0;
        while let Some(page) = scan.pages.get(&rank) {
            prefix.extend_from_slice(page);
            rank += SCAN_PAGE.min(scan.count - rank);
            if page.is_empty() || rank >= scan.count {
                break;
            }
        }
        let done = rank >= scan.count && scan.in_flight == 0 && scan.next_rank >= scan.count;

        if done {
            self.scan = None;
            index::save(&index::cache_path(&self.account_dir, self.album), &prefix);
            self.index = prefix;
            self.state = LoadState::Ready;
            self.layout_dirty = true;
        } else if scan.progressive && prefix.len() > self.index.len() {
            self.index = prefix;
            self.state = LoadState::Ready;
            self.layout_dirty = true;
        }
        self.pump_scan(cx);
        cx.notify();
    }

    // ---- Full records (download URLs) ------------------------------------

    fn request_page(&mut self, page: usize) {
        if self.pages_requested.insert(page) {
            self.page_queue.push(page);
        }
    }

    fn start_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let count = self.index.len();
        let start = page * PAGE;
        if start >= count {
            self.pages_requested.remove(&page);
            return;
        }
        let limit = PAGE.min(count - start);
        let client = self.client.clone();
        let album = self.album;
        let generation = self.generation;
        self.pages_in_flight += 1;

        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.fetch(album, start, limit)).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.pages_in_flight -= 1;
                match result {
                    Some(Ok(assets)) => this.store_page(page, assets),
                    Some(Err(PhotosError::SessionExpired)) => {
                        cx.emit(GridEvent::SessionExpired);
                        return;
                    }
                    Some(Err(err)) => {
                        this.pages_requested.remove(&page);
                        this.page_queue.clear();
                        this.fail(err.to_string(), cx);
                    }
                    None => {
                        this.pages_requested.remove(&page);
                        this.fail("Loading photos failed.".into(), cx);
                    }
                }
                this.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn store_page(&mut self, page: usize, assets: Vec<Asset>) {
        let ids = assets.iter().map(|a| a.id.clone()).collect();
        for asset in assets {
            // Fresh URLs: let failed thumbnails try again.
            self.thumbs_failed.remove(&asset.id);
            self.assets.insert(asset.id.clone(), asset);
        }
        self.page_ids.insert(page, ids);
        self.page_lru.retain(|p| *p != page);
        self.page_lru.push_back(page);
        // Keep memory bounded: forget the least recently loaded pages.
        while self.page_lru.len() > MAX_PAGES {
            let Some(old) = self.page_lru.pop_front() else {
                break;
            };
            for id in self.page_ids.remove(&old).unwrap_or_default() {
                self.assets.remove(&id);
            }
            self.pages_requested.remove(&old);
        }
    }

    // ---- Thumbnails ---------------------------------------------------------

    /// List the thumbnail cache once, so cached thumbnails show without
    /// touching the disk (or the network) per tile.
    fn list_disk_thumbs(&mut self, cx: &mut Context<Self>) {
        let dir = self.thumb_dir.clone();
        cx.spawn(async move |this, cx| {
            let names = cx
                .background_spawn(async move {
                    std::fs::read_dir(&*dir)
                        .map(|entries| {
                            entries
                                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                                .collect::<HashSet<_>>()
                        })
                        .unwrap_or_default()
                })
                .await;
            this.update(cx, |this, cx| {
                this.disk_thumbs = Some(Arc::new(names));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn on_disk(&self, path: &Path) -> bool {
        let (Some(names), Some(name)) = (&self.disk_thumbs, path.file_name()) else {
            return false;
        };
        name.to_str().is_some_and(|n| names.contains(n))
    }

    /// Called while rendering with the items on screen, top to bottom.
    fn request_visible(&mut self, visible: &[usize], cx: &mut Context<Self>) {
        if self.error.is_some() {
            return;
        }
        let mut queue = Vec::new();
        for &ix in visible {
            let Some(item) = self.index.get(ix) else {
                continue;
            };
            let id = item.asset_id();
            if self.thumbs_ready.contains_key(&id)
                || self.thumbs_failed.contains(&id)
                || self.thumbs_in_flight.contains(&id)
            {
                continue;
            }
            let path = thumbs::path_for(&self.thumb_dir, &id);
            if self.on_disk(&path) {
                self.thumbs_ready.insert(id, path.into());
                continue;
            }
            if self.disk_thumbs.is_none() {
                // Wait for the cache listing before fetching anything.
                continue;
            }
            match self.assets.get(&id).and_then(|a| a.thumbnail.clone()) {
                Some(resource) => queue.push((id, resource)),
                None => self.request_page(ix / PAGE),
            }
        }
        // Visible order, first popped first.
        queue.reverse();
        self.thumb_queue = queue;
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

    // ---- Viewer ---------------------------------------------------------------

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
        if let Some(row) = self.layout.as_ref().and_then(|l| l.row_of_item(index)) {
            let viewport = f32::from(self.scroll.bounds().size.height);
            let offset = self.scroll.offset();
            let top = -f32::from(offset.y);
            if row.y < top || row.y + row.height > top + viewport {
                let target = (row.y + row.height / 2. - viewport / 2.).max(0.);
                self.scroll.set_offset(point(offset.x, px(-target)));
            }
        }
        cx.notify();
    }

    // ---- Errors -------------------------------------------------------------

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        if matches!(self.state, LoadState::Loading) {
            self.state = LoadState::Failed(message.into());
        } else {
            // Keep showing what we have; stop requesting until Retry.
            self.error = Some(message.into());
        }
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, LoadState::Failed(_))
            || (self.scan.is_none() && self.error.is_some())
        {
            self.load_album(cx);
            return;
        }
        self.error = None;
        self.thumbs_failed.clear();
        cx.notify();
    }

    // ---- Rendering --------------------------------------------------------------

    fn ensure_layout(&mut self, width: f32) {
        let stale = self
            .layout
            .as_ref()
            .is_none_or(|l| (l.width - width).abs() > 0.5);
        if !(self.layout_dirty || stale) || width <= 0. {
            return;
        }
        let tz = jiff::tz::TimeZone::system();
        let today = jiff::Zoned::now().with_time_zone(tz.clone()).date();
        let params = layout::Params {
            width,
            padding_x: theme::CONTENT_PADDING,
            target_height: TARGET_ROW_HEIGHT,
            gap: TILE_GAP,
        };
        self.layout = Some(layout::build(&self.index, params, today, &tz));
        self.layout_dirty = false;
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let status: Option<SharedString> = match (&self.scan, &self.state) {
            (Some(scan), LoadState::Ready) if scan.progressive => Some(
                format!(
                    "Loading {} of {}\u{2026}",
                    group_digits(self.index.len()),
                    group_digits(scan.count)
                )
                .into(),
            ),
            (Some(_), _) => Some("Updating\u{2026}".into()),
            _ => None,
        };
        let count = matches!(self.state, LoadState::Ready)
            .then(|| format_count(self.scan.as_ref().map_or(self.index.len(), |s| s.count)));

        div()
            .flex_none()
            .flex()
            .items_baseline()
            .gap_3()
            .px(px(theme::CONTENT_PADDING))
            .pt(px(20.))
            .pb(px(8.))
            .child(
                div()
                    .text_size(px(26.))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .text_color(theme::text())
                    .child(album_title(self.album)),
            )
            .when_some(count, |el, count| {
                el.child(div().text_sm().text_color(theme::text_meta()).child(count))
            })
            .child(div().flex_1())
            .when_some(status, |el, status| {
                el.child(div().text_xs().text_color(theme::text_meta()).child(status))
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .child(icon("warning", px(14.), theme::danger()))
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
                    Animation::new(Duration::from_millis(1800))
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
            LoadState::Ready if self.index.is_empty() && self.scan.is_none() => {
                centered(div().child(empty_message(self.album)))
            }
            LoadState::Ready => self.render_mosaic(window, cx),
        };

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::surface())
            .child(header)
            .child(body)
            .when_some(self.viewer.as_ref(), |el, (viewer, _)| {
                el.child(viewer.clone())
            })
    }
}

impl PhotoGrid {
    fn render_mosaic(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // The scroll area's size from the last frame; before the first
        // frame, estimate from the window.
        let bounds = self.scroll.bounds();
        let width = if bounds.size.width > px(0.) {
            f32::from(bounds.size.width)
        } else {
            f32::from(window.viewport_size().width) - f32::from(theme::SIDEBAR_WIDTH)
        };
        let viewport_h = if bounds.size.height > px(0.) {
            f32::from(bounds.size.height)
        } else {
            f32::from(window.viewport_size().height)
        };
        self.ensure_layout(width);
        let Some(layout) = self.layout.take() else {
            return div().flex_1().into_any_element();
        };

        let top = -f32::from(self.scroll.offset().y);
        let rows = layout.rows_in(top - OVERSCAN, top + viewport_h + OVERSCAN);
        let on_screen = layout.rows_in(top, top + viewport_h);

        // Thumbnails for what's on screen, top to bottom.
        let visible: Vec<usize> = layout.rows[on_screen]
            .iter()
            .filter_map(|row| match &row.kind {
                RowKind::Tiles(tiles) => Some(tiles.iter().map(|t| t.index)),
                _ => None,
            })
            .flatten()
            .collect();
        self.request_visible(&visible, cx);

        let mut children: Vec<AnyElement> = Vec::with_capacity(rows.len() * 4);
        for row in &layout.rows[rows] {
            match &row.kind {
                RowKind::Year(label) => children.push(
                    header_row(row.y, row.height)
                        .items_end()
                        .pb(px(8.))
                        .text_size(px(32.))
                        .font_weight(FontWeight::EXTRA_BOLD)
                        .text_color(theme::text())
                        .child(label.clone())
                        .into_any_element(),
                ),
                RowKind::Month(label) => children.push(
                    header_row(row.y, row.height)
                        .items_end()
                        .pb(px(6.))
                        .text_size(px(20.))
                        .font_weight(FontWeight::EXTRA_BOLD)
                        .text_color(theme::text())
                        .child(label.clone())
                        .into_any_element(),
                ),
                RowKind::Day(label) => children.push(
                    header_row(row.y, row.height)
                        .items_center()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_title())
                        .child(label.clone())
                        .into_any_element(),
                ),
                RowKind::Tiles(tiles) => {
                    for t in tiles {
                        let Some(item) = self.index.get(t.index) else {
                            continue;
                        };
                        let thumb = self.thumbs_ready.get(&item.asset_id()).cloned();
                        let ix = t.index;
                        children.push(
                            tile(ix, item, thumb)
                                .absolute()
                                .left(px(t.x))
                                .top(px(row.y))
                                .w(px(t.width))
                                .h(px(row.height))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_viewer(ix, window, cx)
                                }))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        let height = layout.height;
        self.layout = Some(layout);

        div()
            .flex_1()
            .min_h_0()
            .image_cache(self.image_cache.clone())
            .child(
                div()
                    .id("gallery")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().relative().w_full().h(px(height)).children(children)),
            )
            .into_any_element()
    }
}

fn header_row(y: f32, height: f32) -> gpui::Div {
    div()
        .absolute()
        .left(px(theme::CONTENT_PADDING))
        .right(px(theme::CONTENT_PADDING))
        .top(px(y))
        .h(px(height))
        .flex()
}

fn tile(index: usize, item: &IndexItem, thumb: Option<Arc<Path>>) -> gpui::Stateful<gpui::Div> {
    let duration = match item.kind {
        Kind::Video(secs) => Some(format!("{}:{:02}", secs / 60, secs % 60)),
        _ => None,
    };
    div()
        .id(ElementId::NamedInteger("tile".into(), index as u64))
        .overflow_hidden()
        .rounded(theme::RADIUS_TILE)
        .bg(theme::placeholder())
        .cursor_pointer()
        .when_some(thumb, |el, path| {
            el.child(
                img(path)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    // Fade in when the thumbnail arrives.
                    .with_animation(
                        ElementId::NamedInteger("thumb-fade".into(), index as u64),
                        Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                        |el, t| el.opacity(t),
                    ),
            )
        })
        // Hover: one flat scrim step, no lift or glow.
        .child(
            div()
                .absolute()
                .inset_0()
                .hover(|el| el.bg(gpui::white().opacity(0.07))),
        )
        .when_some(duration, |el, duration| {
            el.child(
                badge()
                    .right(px(6.))
                    .child(icon("play-fill", px(10.), gpui::white()))
                    .child(duration),
            )
        })
        .when(item.kind == Kind::Live, |el| {
            el.child(
                badge()
                    .right(px(6.))
                    .child(icon("live", px(12.), gpui::white())),
            )
        })
        .when(item.favorite, |el| {
            el.child(
                badge()
                    .left(px(6.))
                    .child(icon("heart-fill", px(11.), gpui::white())),
            )
        })
}

/// A small label on a tile, readable on any photo.
fn badge() -> gpui::Div {
    div()
        .absolute()
        .bottom(px(6.))
        .h(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .gap(px(3.))
        .rounded(px(5.))
        .bg(theme::scrim(0.5))
        .text_color(gpui::white())
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
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
        .text_color(theme::accent_text())
        .cursor_pointer()
        .hover(|el| el.text_color(theme::text()))
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
        Album::Favorites => "No favorites yet. Favorite a photo on your iPhone to see it here.",
        Album::Videos => "No videos.",
        Album::RecentlyDeleted => "Nothing recently deleted.",
    }
}

pub fn group_digits(n: usize) -> String {
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

fn format_count(count: usize) -> String {
    format!(
        "{} {}",
        group_digits(count),
        if count == 1 { "item" } else { "items" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_readable() {
        assert_eq!(format_count(1), "1 item");
        assert_eq!(format_count(999), "999 items");
        assert_eq!(format_count(12_345), "12,345 items");
        assert_eq!(format_count(1_234_567), "1,234,567 items");
    }
}
