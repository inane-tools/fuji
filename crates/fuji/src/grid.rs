//! The gallery: a mosaic of the album under month and day headers.
//!
//! - The album's index (date, shape, type of every item) comes from a disk
//!   cache at once, and is refreshed from iCloud in the background.
//! - The mosaic is laid out from the index ([`crate::layout`]); only rows on
//!   screen are built.
//! - Thumbnails already on disk show without any network request; others are
//!   fetched for what's visible, using download URLs from full records loaded
//!   by page.
//! - Decoded thumbnails live in a bounded GPU cache, so memory stays flat.
//! - Scrolling and zooming ease toward their targets; a timeline scrubber on
//!   the right jumps through the years.
//! - Photos can be selected, copied, downloaded, dragged out as files, and
//!   pasted in (uploaded). The work is in [`crate::transfer`].

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fuji_icloud::{Album, Asset, AssetId, PhotosClient, PhotosError, Resource};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, Pixels, Point, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    SharedString, Subscription, Transformation, Window, actions, div, img, percentage, point,
    prelude::*, px,
};
use jiff::civil::Date;

use crate::anim::{self, Eased};
use crate::assets::icon;
use crate::image_cache::LruImageCache;
use crate::index::{self, IndexItem, Kind};
use crate::layout::{self, Layout, RowKind, Span};
use crate::scrubber::{self, Track};
use crate::search::Query;
use crate::settings::{self, Settings};
use crate::transfer::PendingUpload;
use crate::ui::{self, MenuItem, Status};
use crate::viewer::{Viewer, ViewerEvent};
use crate::{net, theme, thumbs, titlebar};

actions!(
    gallery,
    [
        CopySelection,
        Paste,
        DownloadSelection,
        Cancel,
        SelectAll,
        ZoomIn,
        ZoomOut,
        ZoomReset
    ]
);

const CONTEXT: &str = "Gallery";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-c", CopySelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-s", DownloadSelection, Some(CONTEXT)),
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl-+", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("ctrl-0", ZoomReset, Some(CONTEXT)),
    ]);
}

/// Items per full-record page (with download URLs).
pub(crate) const PAGE: usize = 100;
/// Full-record pages kept in memory.
const MAX_PAGES: usize = 40;
const MAX_PAGE_REQUESTS: usize = 2;
/// Pages waiting to be requested. Older ones are dropped: after a fast
/// scroll, what was passed on the way no longer matters.
const MAX_QUEUED_PAGES: usize = 6;
/// Items per index-scan request (small records, no URLs).
const SCAN_PAGE: usize = 200;
const MAX_SCAN_REQUESTS: usize = 6;
const MAX_THUMB_REQUESTS: usize = 8;
/// Extra content kept built above and below the viewport.
const OVERSCAN: f32 = 600.;
/// Decoded thumbnails kept in memory, at least; more when zoomed out.
const IMAGE_CACHE_SIZE: usize = 120;
const TILE_GAP: f32 = 4.;

/// How far one wheel notch scrolls.
const WHEEL_STEP: f32 = 120.;
/// GPUI reports a wheel notch as 3 "lines".
const LINES_PER_NOTCH: f32 = 3.;
/// How quickly scrolling and zooming ease to their targets (per second).
const SCROLL_RATE: f32 = 16.;
const ZOOM_RATE: f32 = 14.;
/// Zoom change for one wheel notch or key press.
const ZOOM_STEP: f32 = 1.2;
/// Past this many viewports from its target, the gallery is "flying": it
/// draws placeholders instead of loading thumbnails it's about to leave.
const FLYING_VIEWPORTS: f32 = 1.5;

/// The strip on the right holding the timeline scrubber.
const SCRUBBER_WIDTH: f32 = 48.;
const SCRUBBER_INSET: f32 = 14.;
/// The gallery runs under the app's title bar; its header floats over the
/// top of the mosaic, which starts below both.
const BAR: f32 = titlebar::HEIGHT_PX;
const HEADER_SPACE: f32 = BAR + 92.;
/// The scrubber's track starts below the header.
const SCRUBBER_TOP: f32 = HEADER_SPACE;
/// How long the scrubber's date stays up after scrolling.
const SCRUBBER_LINGER: Duration = Duration::from_millis(900);
/// How far the pointer moves with the button down before a drag starts.
const DRAG_THRESHOLD: f32 = 6.;
const TOAST_TIMEOUT: Duration = Duration::from_secs(4);

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
    /// Pages received but not yet joined to the run from the start, by first rank.
    pages: HashMap<usize, Vec<IndexItem>>,
    /// Rank up to which pages have been joined.
    joined: usize,
    /// The new index so far, when it replaces a cached one at the end.
    fresh: Vec<IndexItem>,
    /// Show results as they arrive (nothing cached to show meanwhile).
    progressive: bool,
}

/// The photo to keep still under the pointer while the zoom changes.
struct ZoomAnchor {
    index: usize,
    /// How far down its row (0 to 1) the anchor point is.
    fraction: f32,
    /// The anchor point, from the top of the gallery's viewport.
    screen_y: f32,
}

#[derive(Default)]
struct Scrub {
    dragging: bool,
    /// Pointer position along the track while over the scrubber.
    hover: Option<f32>,
    /// When the gallery last scrolled; the date shows for a moment after.
    scrolled: Option<Instant>,
    generation: u64,
}

enum MenuKind {
    /// On one or more photos.
    Items(Vec<usize>),
    /// On a day or month header, whose first item is `first`.
    Header { first: usize, month: bool },
    /// On the gallery itself.
    Background,
    /// On the photo open in the viewer.
    Viewer(usize),
}

struct OpenMenu {
    serial: u64,
    position: Point<Pixels>,
    kind: MenuKind,
}

/// An album being searched. `PhotoGrid::index` holds the matches, so
/// layout, selection and the viewer work on them as on any album; pages of
/// full records are fetched by album rank.
/// What decides a match.
enum Matcher {
    /// fuji's own search: dates, types, favorites, file names.
    Local(Query),
    /// What a device found (Skybridge): asset IDs, and the device's name.
    Device {
        ids: HashSet<String>,
        provider: SharedString,
        /// The device returned as many as asked for: there may be more.
        capped: bool,
    },
}

impl Matcher {
    fn matches(&self, item: &IndexItem, day: Option<Date>) -> bool {
        match self {
            Matcher::Local(query) => query.matches(item, day),
            Matcher::Device { ids, .. } => ids.contains(&item.id),
        }
    }

    fn filter(&self, items: &[IndexItem], days: &[Option<Date>]) -> Vec<usize> {
        match self {
            Matcher::Local(query) => query.filter(items, days),
            Matcher::Device { ids, .. } => items
                .iter()
                .enumerate()
                .filter(|(_, item)| ids.contains(&item.id))
                .map(|(i, _)| i)
                .collect(),
        }
    }
}

struct Search {
    query: Matcher,
    all: Vec<IndexItem>,
    all_days: Vec<Option<Date>>,
    /// Album rank of each match.
    ranks: Vec<usize>,
}

pub struct PhotoGrid {
    pub(crate) client: Arc<PhotosClient>,
    account_dir: PathBuf,
    thumb_dir: Arc<PathBuf>,
    pub(crate) preview_dir: Arc<PathBuf>,
    album: Album,
    /// The album's name, shown as the heading.
    title: SharedString,
    /// Bumped on album change so results for the old album are ignored.
    generation: u64,
    state: LoadState,
    index: Vec<IndexItem>,
    /// The calendar day of each index item.
    days: Vec<Option<Date>>,
    /// While searching: the whole album, and the album rank of each item
    /// in `index` (which then holds only the matches).
    search: Option<Search>,
    /// What's typed in the search field, kept across album changes.
    search_text: String,
    /// The month at the top of the mosaic, for the header.
    month_in_view: Option<(i16, i8)>,
    /// A device is searching for `search_text`; its name if known.
    device_pending: Option<SharedString>,
    scan: Option<Scan>,
    layout: Option<Layout>,
    layout_dirty: bool,
    // Full records (download URLs), by page of rank.
    assets: HashMap<AssetId, Asset>,
    page_ids: HashMap<usize, Vec<AssetId>>,
    page_lru: VecDeque<usize>,
    pages_requested: HashSet<usize>,
    /// Pages to request, most urgent last.
    page_queue: Vec<usize>,
    pages_in_flight: usize,
    // Thumbnails.
    disk_thumbs: Option<Arc<HashSet<String>>>,
    thumbs_ready: HashMap<AssetId, Arc<Path>>,
    /// Items whose thumbnails the viewer's filmstrip wants, besides those on screen.
    wanted_thumbs: Vec<usize>,
    thumbs_failed: HashSet<AssetId>,
    thumbs_in_flight: HashSet<AssetId>,
    /// Thumbnails to fetch, highest priority last.
    thumb_queue: Vec<(AssetId, Resource)>,
    error: Option<SharedString>,
    image_cache: Entity<LruImageCache>,
    image_cache_size: usize,
    viewer: Option<(Entity<Viewer>, Subscription)>,
    focus_handle: FocusHandle,
    // Motion.
    scroll: ScrollHandle,
    /// The scroll position (content top), easing to where the wheel or the
    /// scrubber sent it.
    scroller: Eased,
    max_scroll: f32,
    row_height: Eased,
    zoom_anchor: Option<ZoomAnchor>,
    scrub: Scrub,
    track: Option<Track>,
    // Selection and pointer.
    selected: BTreeSet<usize>,
    /// Where a shift-click range starts.
    select_anchor: Option<usize>,
    /// A tile the button went down on, and where: a drag may follow.
    press: Option<(usize, Point<Pixels>)>,
    /// Swallow the click that ends a press which became a drag.
    pub(crate) suppress_click: bool,
    pub(crate) dragging_out: bool,
    menu: Option<OpenMenu>,
    menu_serial: u64,
    // Feedback.
    toast: Option<(u64, Status)>,
    toast_serial: u64,
    pub(crate) pending_upload: Option<PendingUpload>,
    _settings: Subscription,
}

impl EventEmitter<GridEvent> for PhotoGrid {}

impl Focusable for PhotoGrid {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PhotoGrid {
    pub fn new(
        client: Arc<PhotosClient>,
        dsid: &str,
        album: Album,
        title: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let thumb_dir = Arc::new(thumbs::cache_dir(dsid));
        let preview_dir = Arc::new(thumbs::preview_dir(dsid));
        let account_dir = thumb_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| thumb_dir.to_path_buf());

        let prune_dir = preview_dir.clone();
        cx.background_spawn(async move {
            thumbs::prune(&prune_dir, thumbs::PREVIEWS_KEPT);
            crate::export::clean();
        })
        .detach();

        // Follow the zoom when it's changed in Settings.
        let settings = cx.observe_global::<Settings>(|this, cx| {
            let height = Settings::get(cx).row_height;
            if (this.row_height.target() - height).abs() > 0.01 {
                this.row_height.set(height);
                cx.notify();
            }
        });

        let mut grid = Self {
            client,
            account_dir,
            thumb_dir,
            preview_dir,
            album,
            title,
            generation: 0,
            state: LoadState::Loading,
            index: Vec::new(),
            days: Vec::new(),
            search: None,
            search_text: String::new(),
            month_in_view: None,
            device_pending: None,
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
            wanted_thumbs: Vec::new(),
            thumbs_failed: HashSet::new(),
            thumbs_in_flight: HashSet::new(),
            thumb_queue: Vec::new(),
            error: None,
            image_cache: cx.new(|cx| LruImageCache::new(IMAGE_CACHE_SIZE, cx)),
            image_cache_size: IMAGE_CACHE_SIZE,
            viewer: None,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            scroller: Eased::new(0.),
            max_scroll: 0.,
            row_height: Eased::new(Settings::get(cx).row_height),
            zoom_anchor: None,
            scrub: Scrub::default(),
            track: None,
            selected: BTreeSet::new(),
            select_anchor: None,
            press: None,
            suppress_click: false,
            dragging_out: false,
            menu: None,
            menu_serial: 0,
            toast: None,
            toast_serial: 0,
            pending_upload: None,
            _settings: settings,
        };
        grid.list_disk_thumbs(cx);
        grid.load_album(cx);
        grid
    }

    /// Show `album`, headed `title`. A rename alone just changes the heading.
    pub fn set_album(&mut self, album: Album, title: SharedString, cx: &mut Context<Self>) {
        if title != self.title {
            self.title = title;
            cx.notify();
        }
        if album != self.album {
            self.album = album;
            self.load_album(cx);
        }
    }

    /// Show only the items matching `text` (all of them when it's empty).
    pub fn set_search(&mut self, text: &str, cx: &mut Context<Self>) {
        if text == self.search_text {
            return;
        }
        self.search_text = text.to_string();
        let matcher = Query::parse(text, jiff::Zoned::now().date()).map(Matcher::Local);
        self.apply_matcher(matcher, cx);
    }

    /// Show what a device found for `text` (if that's still the search).
    pub fn set_device_results(
        &mut self,
        text: &str,
        ids: Vec<String>,
        provider: SharedString,
        capped: bool,
        cx: &mut Context<Self>,
    ) {
        if text != self.search_text {
            return;
        }
        self.device_pending = None;
        let matcher = Matcher::Device {
            ids: ids.into_iter().collect(),
            provider,
            capped,
        };
        self.apply_matcher(Some(matcher), cx);
    }

    /// Show (or stop showing) that a device is searching.
    pub fn set_device_pending(&mut self, pending: Option<SharedString>, cx: &mut Context<Self>) {
        if self.device_pending != pending {
            self.device_pending = pending;
            cx.notify();
        }
    }

    fn apply_matcher(&mut self, matcher: Option<Matcher>, cx: &mut Context<Self>) {
        let (all, all_days) = match self.search.take() {
            Some(search) => (search.all, search.all_days),
            None => (
                std::mem::take(&mut self.index),
                std::mem::take(&mut self.days),
            ),
        };
        // Leave the viewer, menu and zoom anchor: positions change.
        self.viewer = None;
        self.menu = None;
        self.zoom_anchor = None;
        self.select_anchor = None;
        self.search = matcher.map(|query| Search {
            query,
            all: Vec::new(),
            all_days: Vec::new(),
            ranks: Vec::new(),
        });
        self.show_album_items(all, all_days);
        self.scroller.jump(0.);
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// The number of items in the whole album, searched or not.
    fn album_len(&self) -> usize {
        self.search
            .as_ref()
            .map_or(self.index.len(), |s| s.all.len())
    }

    /// The album rank of the item shown at `index`.
    fn rank(&self, index: usize) -> usize {
        self.search
            .as_ref()
            .and_then(|s| s.ranks.get(index).copied())
            .unwrap_or(index)
    }

    /// Take `items` (with their `days`) as the whole album, showing the
    /// ones the search matches, and keep the selection on the same photos.
    fn show_album_items(&mut self, items: Vec<IndexItem>, days: Vec<Option<Date>>) {
        let chosen: HashSet<String> = self
            .selected
            .iter()
            .filter_map(|&ix| self.index.get(ix).map(|item| item.id.clone()))
            .collect();
        let (shown, shown_days) = match self.search.as_mut() {
            Some(search) => {
                search.ranks = search.query.filter(&items, &days);
                let shown = search.ranks.iter().map(|&r| items[r].clone()).collect();
                let shown_days = search.ranks.iter().map(|&r| days[r]).collect();
                search.all = items;
                search.all_days = days;
                (shown, shown_days)
            }
            None => (items, days),
        };
        if !chosen.is_empty() {
            self.selected = shown
                .iter()
                .enumerate()
                .filter(|(_, item)| chosen.contains(&item.id))
                .map(|(ix, _)| ix)
                .collect();
            self.select_anchor = None;
        }
        self.index = shown;
        self.days = shown_days;
        self.layout_dirty = true;
    }

    /// Add items that arrived at the end of the album.
    fn append_album_items(&mut self, items: Vec<IndexItem>, days: Vec<Option<Date>>) {
        match self.search.as_mut() {
            Some(search) => {
                let base = search.all.len();
                for (i, (item, day)) in items.iter().zip(&days).enumerate() {
                    if search.query.matches(item, *day) {
                        search.ranks.push(base + i);
                        self.index.push(item.clone());
                        self.days.push(*day);
                    }
                }
                search.all.extend(items);
                search.all_days.extend(days);
            }
            None => {
                self.index.extend(items);
                self.days.extend(days);
            }
        }
        self.layout_dirty = true;
    }

    /// The thumbnail and preview caches were deleted: forget what was in them.
    pub fn cache_cleared(&mut self, cx: &mut Context<Self>) {
        self.disk_thumbs = Some(Arc::new(HashSet::new()));
        self.thumbs_ready.clear();
        self.thumbs_failed.clear();
        cx.notify();
    }

    // ---- What the viewer and transfers read --------------------------------

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
            self.request_page(self.rank(index) / PAGE);
            self.pump(cx);
        }
    }

    // ---- Album index ------------------------------------------------------

    fn load_album(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.state = LoadState::Loading;
        self.index.clear();
        self.days.clear();
        if let Some(search) = self.search.as_mut() {
            search.all.clear();
            search.all_days.clear();
            search.ranks.clear();
        }
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
        self.selected.clear();
        self.select_anchor = None;
        self.menu = None;
        self.zoom_anchor = None;
        self.scroller.jump(0.);
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();

        let generation = self.generation;
        let path = index::cache_path(&self.account_dir, &self.album);
        cx.spawn(async move |this, cx| {
            // Cached index first, so the gallery shows right away.
            let cached = cx
                .background_spawn(async move {
                    let items = index::load(&path)?;
                    let days = layout::days(&items, &jiff::tz::TimeZone::system());
                    Some((items, days))
                })
                .await;
            let had_cache = cached.is_some();
            this.update(cx, |this, cx| {
                if this.generation == generation
                    && let Some((items, days)) = cached
                {
                    this.show_album_items(items, days);
                    this.state = LoadState::Ready;
                    cx.notify();
                }
                // Then refresh from iCloud.
                if this.generation == generation {
                    this.refresh(!had_cache, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask iCloud how many items the album has and rescan it.
    pub(crate) fn refresh(&mut self, progressive: bool, cx: &mut Context<Self>) {
        if self.scan.is_some() {
            return;
        }
        let generation = self.generation;
        let client = self.client.clone();
        let album = self.album.clone();
        cx.spawn(async move |this, cx| {
            let count = net::run(move || {
                client.check_ready()?;
                client.count(&album)
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match count {
                    Some(Ok(count)) => this.start_scan(count, progressive, cx),
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
        if self.scan.is_some() {
            return;
        }
        if count == 0 {
            self.set_index(Vec::new());
            self.state = LoadState::Ready;
            self.save_index(cx);
            cx.notify();
            return;
        }
        // Showing pages as they arrive only works from an empty gallery.
        let progressive = progressive && self.album_len() == 0;
        self.scan = Some(Scan {
            count,
            next_rank: 0,
            in_flight: 0,
            pages: HashMap::new(),
            joined: 0,
            fresh: Vec::new(),
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
            let album = self.album.clone();
            let generation = self.generation;
            cx.spawn(async move |this, cx| {
                let result = net::run(move || {
                    client
                        .scan(&album, start, limit)
                        .map(|assets| assets.iter().map(IndexItem::from).collect::<Vec<_>>())
                })
                .await;
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match result {
                        Some(Ok(items)) => this.scan_page_done(start, items, cx),
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

        // Join the pages that now continue the run from the start. Each item
        // is moved once, however the pages arrive.
        let mut arrived = Vec::new();
        while let Some(page) = scan.pages.remove(&scan.joined) {
            scan.joined += SCAN_PAGE.min(scan.count - scan.joined);
            arrived.extend(page);
        }
        let done = scan.in_flight == 0 && scan.next_rank >= scan.count;
        let progressive = scan.progressive;

        if progressive {
            if !arrived.is_empty() {
                let tz = jiff::tz::TimeZone::system();
                let days = layout::days(&arrived, &tz);
                self.append_album_items(arrived, days);
                self.state = LoadState::Ready;
            }
        } else {
            scan.fresh.extend(arrived);
        }
        if done {
            let scan = self.scan.take();
            if !progressive && let Some(scan) = scan {
                self.set_index(scan.fresh);
            }
            self.state = LoadState::Ready;
            self.save_index(cx);
        }
        self.pump_scan(cx);
        cx.notify();
    }

    /// Replace the album's items, keeping the selection on the same photos.
    fn set_index(&mut self, items: Vec<IndexItem>) {
        let days = layout::days(&items, &jiff::tz::TimeZone::system());
        self.show_album_items(items, days);
    }

    fn save_index(&self, cx: &mut Context<Self>) {
        let path = index::cache_path(&self.account_dir, &self.album);
        let items = match &self.search {
            Some(search) => search.all.clone(),
            None => self.index.clone(),
        };
        cx.background_spawn(async move { index::save(&path, &items) })
            .detach();
    }

    // ---- Full records (download URLs) ------------------------------------

    fn request_page(&mut self, page: usize) {
        if self.pages_requested.insert(page) {
            self.page_queue.push(page);
            // Forget the stalest requests; they're asked for again if needed.
            while self.page_queue.len() > MAX_QUEUED_PAGES {
                let stale = self.page_queue.remove(0);
                self.pages_requested.remove(&stale);
            }
        }
    }

    fn start_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let count = self.album_len();
        let start = page * PAGE;
        if start >= count {
            self.pages_requested.remove(&page);
            return;
        }
        let limit = PAGE.min(count - start);
        let client = self.client.clone();
        let album = self.album.clone();
        let generation = self.generation;
        self.pages_in_flight += 1;

        cx.spawn(async move |this, cx| {
            let result = net::run(move || client.fetch(&album, start, limit)).await;
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
                        for queued in this.page_queue.drain(..) {
                            this.pages_requested.remove(&queued);
                        }
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

    /// Fetch these items' thumbnails too (the viewer's filmstrip).
    pub(crate) fn want_thumbnails(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        self.wanted_thumbs = indices;
        self.request_visible(&[], cx);
    }

    /// Called while rendering with the items on screen, top to bottom.
    fn request_visible(&mut self, visible: &[usize], cx: &mut Context<Self>) {
        if self.error.is_some() {
            return;
        }
        let mut queue = Vec::new();
        let wanted = std::mem::take(&mut self.wanted_thumbs);
        for &ix in visible.iter().chain(&wanted) {
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
                None => self.request_page(self.rank(ix) / PAGE),
            }
        }
        self.wanted_thumbs = wanted;
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

    pub(crate) fn open_viewer(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.index.len() {
            return;
        }
        let grid = cx.entity();
        let client = self.client.clone();
        let preview_dir = self.preview_dir.clone();
        let viewer = cx.new(|cx| Viewer::new(&grid, client, preview_dir, index, cx));
        let subscription =
            cx.subscribe_in(&viewer, window, |this, _, event, window, cx| match event {
                ViewerEvent::Closed { index } => this.close_viewer(*index, window, cx),
            });
        viewer.focus_handle(cx).focus(window);
        self.viewer = Some((viewer, subscription));
        self.menu = None;
        self.press = None;
        cx.notify();
    }

    fn close_viewer(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.viewer = None;
        self.menu = None;
        self.focus_handle.focus(window);
        // Keep the photo you were looking at in view.
        if let Some(row) = self.layout.as_ref().and_then(|l| l.row_of_item(index)) {
            let viewport = f32::from(self.scroll.bounds().size.height);
            let top = -f32::from(self.scroll.offset().y);
            if row.y < top || row.y + row.height > top + viewport {
                let target = (row.y + row.height / 2. - viewport / 2.).max(0.);
                self.jump_to(target);
            }
        }
        cx.notify();
    }

    // ---- Scrolling and zooming ----------------------------------------------

    fn top(&self) -> f32 {
        -f32::from(self.scroll.offset().y)
    }

    /// Scroll to `top` at once.
    fn jump_to(&mut self, top: f32) {
        let top = top.max(0.);
        self.scroller.jump(top);
        self.scroll.set_offset(point(px(0.), px(-top)));
    }

    /// Ease the scroll position toward `top`.
    fn scroll_toward(&mut self, top: f32, cx: &mut Context<Self>) {
        if !self.scroller.is_moving() {
            self.scroller.jump(self.top());
        }
        self.scroller.set(top.clamp(0., self.max_scroll));
        self.show_scrubber_date(cx);
        cx.notify();
    }

    /// Show the scrubber's date for a moment.
    fn show_scrubber_date(&mut self, cx: &mut Context<Self>) {
        self.scrub.scrolled = Some(Instant::now());
        self.scrub.generation += 1;
        let generation = self.scrub.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCRUBBER_LINGER).await;
            this.update(cx, |this, cx| {
                if this.scrub.generation == generation {
                    this.scrub.scrolled = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let zoom = event.modifiers.control || event.modifiers.platform;
        match event.delta {
            ScrollDelta::Lines(lines) => {
                let notches = lines.y / LINES_PER_NOTCH;
                if zoom {
                    self.zoom_by(ZOOM_STEP.powf(notches), Some(event.position), cx);
                } else {
                    let from = if self.scroller.is_moving() {
                        self.scroller.target()
                    } else {
                        self.top()
                    };
                    self.scroll_toward(from - notches * WHEEL_STEP, cx);
                }
            }
            // A touchpad already scrolls smoothly: follow the fingers.
            ScrollDelta::Pixels(delta) => {
                if zoom {
                    let factor = (f32::from(delta.y) * 0.005).exp();
                    self.zoom_by(factor, Some(event.position), cx);
                } else {
                    let top = (self.top() - f32::from(delta.y)).clamp(0., self.max_scroll);
                    self.jump_to(top);
                    self.show_scrubber_date(cx);
                    cx.notify();
                }
            }
        }
        cx.stop_propagation();
    }

    /// Change the size of the mosaic's rows, keeping the photo under `at`
    /// (window coordinates; the middle of the gallery if `None`) in place.
    fn zoom_by(&mut self, factor: f32, at: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let target = (self.row_height.target() * factor)
            .clamp(settings::MIN_ROW_HEIGHT, settings::MAX_ROW_HEIGHT);
        self.zoom_to(target, at, cx);
    }

    fn zoom_to(&mut self, target: f32, at: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        if (target - self.row_height.target()).abs() < 0.01 {
            return;
        }
        if self.zoom_anchor.is_none()
            && let Some(layout) = &self.layout
        {
            let bounds = self.scroll.bounds();
            let viewport = f32::from(bounds.size.height);
            let screen_y = at
                .map(|p| f32::from(p.y - bounds.top()))
                .filter(|y| (0. ..=viewport).contains(y))
                .unwrap_or(viewport / 2.);
            let y = self.top() + screen_y;
            self.zoom_anchor = layout.tile_row_at(y).and_then(|row| {
                let RowKind::Tiles(tiles) = &row.kind else {
                    return None;
                };
                Some(ZoomAnchor {
                    index: tiles.first()?.index,
                    fraction: ((y - row.y) / row.height).clamp(0., 1.),
                    screen_y,
                })
            });
        }
        // The anchor decides the scroll position while zooming.
        self.scroller.jump(self.top());
        self.row_height.set(target);
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1. / ZOOM_STEP, None, cx);
    }

    fn zoom_reset(&mut self, _: &ZoomReset, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(settings::DEFAULT_ROW_HEIGHT, None, cx);
    }

    /// Scroll to the scrubber position under the pointer.
    fn scrub_to(&mut self, pointer: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(track) = self.track else {
            return;
        };
        let y = f32::from(pointer.y - self.scroll.bounds().top()) - SCRUBBER_TOP;
        self.scrub.hover = Some(y.clamp(0., track.length));
        self.scroll_toward(track.scroll_for(y), cx);
    }

    // ---- Selection ------------------------------------------------------------

    /// What an action on tile `index` applies to: the whole selection if the
    /// tile is part of it, otherwise just the tile.
    fn targets_for(&self, index: usize) -> Vec<usize> {
        if self.selected.contains(&index) {
            self.selected.iter().copied().collect()
        } else {
            vec![index]
        }
    }

    /// Whether every photo of the day, month or year around `index` is selected.
    fn span_selected(&self, index: usize, span: Span) -> bool {
        let range = layout::span(&self.days, index, span);
        !range.is_empty() && self.selected.range(range.clone()).count() == range.len()
    }

    /// Select the whole day, month or year around `index`, or deselect it
    /// if it's all selected already.
    fn toggle_span(&mut self, index: usize, span: Span, cx: &mut Context<Self>) {
        let range = layout::span(&self.days, index, span);
        if self.span_selected(index, span) {
            self.selected.retain(|ix| !range.contains(ix));
        } else {
            self.selected.extend(range);
            self.select_anchor = Some(index);
        }
        cx.notify();
    }

    fn toggle_selected(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.select_anchor = Some(index);
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        self.select_anchor = None;
        cx.notify();
    }

    fn tile_clicked(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if std::mem::take(&mut self.suppress_click) {
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.shift
            && let Some(anchor) = self.select_anchor
        {
            self.selected.extend(anchor.min(index)..=anchor.max(index));
            cx.notify();
        } else if modifiers.control || modifiers.platform || !self.selected.is_empty() {
            self.toggle_selected(index, cx);
        } else {
            self.open_viewer(index, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_some() {
            return;
        }
        if self.menu.take().is_none() && self.pending_upload.take().is_none() {
            self.selected.clear();
            self.select_anchor = None;
        }
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() {
            self.selected = (0..self.index.len()).collect();
            cx.notify();
        }
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.copy(self.selected.iter().copied().collect(), cx);
        }
    }

    fn download_selection(
        &mut self,
        _: &DownloadSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.viewer.is_none() && !self.selected.is_empty() {
            self.download(self.selected.iter().copied().collect(), cx);
        }
    }

    fn paste_action(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() {
            self.paste(cx);
        }
    }

    // ---- Pointer --------------------------------------------------------------

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let held = event.pressed_button == Some(MouseButton::Left);
        if self.scrub.dragging {
            if held {
                self.scrub_to(event.position, cx);
            } else {
                self.scrub.dragging = false;
                cx.notify();
            }
            return;
        }
        let Some((index, start)) = self.press else {
            return;
        };
        if !held {
            self.press = None;
            return;
        }
        let moved = event.position - start;
        if f32::from(moved.x).hypot(f32::from(moved.y)) > DRAG_THRESHOLD {
            self.press = None;
            self.start_drag(self.targets_for(index), window, cx);
        }
    }

    fn end_press(&mut self, cx: &mut Context<Self>) {
        self.press = None;
        if std::mem::take(&mut self.scrub.dragging) {
            cx.notify();
        }
    }

    pub(crate) fn open_viewer_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.open_menu(MenuKind::Viewer(index), position, cx);
    }

    fn open_menu(&mut self, kind: MenuKind, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu_serial += 1;
        self.menu = Some(OpenMenu {
            serial: self.menu_serial,
            position,
            kind,
        });
        cx.notify();
    }

    // ---- Feedback -------------------------------------------------------------

    pub(crate) fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.toast_serial += 1;
        let serial = self.toast_serial;
        let finished = !matches!(status, Status::Working(_));
        self.toast = Some((serial, status));
        cx.notify();
        if finished {
            // Clear the message after a moment, unless something newer replaced it.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TOAST_TIMEOUT).await;
                this.update(cx, |this, cx| {
                    if this.toast.as_ref().is_some_and(|(s, _)| *s == serial) {
                        this.toast = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        matches!(self.toast, Some((_, Status::Working(_))))
    }

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
        let today = jiff::Zoned::now().date();
        let params = layout::Params {
            width,
            padding_x: theme::CONTENT_PADDING,
            padding_right: SCRUBBER_WIDTH - theme::CONTENT_PADDING,
            target_height: self.row_height.get(),
            gap: TILE_GAP,
            top_padding: HEADER_SPACE,
        };
        self.layout = Some(layout::build(&self.index, &self.days, params, today));
        self.layout_dirty = false;
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let sync_status: Option<SharedString> = match (&self.scan, &self.state) {
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
        // A device's search in progress says so in words.
        let status = self
            .device_pending
            .as_ref()
            .map(|name| SharedString::from(format!("Asking {name}\u{2026}")));
        let syncing = sync_status.is_some();
        let sync = div()
            .id("sync")
            .size(theme::CONTROL_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .hover(|el| el.bg(theme::hover()))
            .tooltip(ui::tooltip(
                sync_status.unwrap_or_else(|| "Check for new photos".into()),
            ))
            .on_click(cx.listener(|this, _, _, cx| this.refresh(false, cx)))
            .child(if syncing && !anim::reduced() {
                icon("sync", px(16.), theme::accent_text())
                    .with_animation(
                        "sync-turning",
                        Animation::new(Duration::from_millis(1100)).repeat(),
                        |svg, t| svg.with_transformation(Transformation::rotate(percentage(t))),
                    )
                    .into_any_element()
            } else {
                icon(
                    "sync",
                    px(16.),
                    if syncing {
                        theme::accent_text()
                    } else {
                        theme::text_muted()
                    },
                )
                .into_any_element()
            });
        // While a device searches and ours found nothing, no count yet.
        let waiting = self.device_pending.is_some() && self.index.is_empty();
        let count = (matches!(self.state, LoadState::Ready) && !waiting).then(|| {
            if let Some(search) = &self.search {
                let n = self.index.len();
                let count = format!(
                    "{} {}",
                    group_digits(n),
                    if n == 1 { "result" } else { "results" }
                );
                match &search.query {
                    Matcher::Device {
                        provider, capped, ..
                    } => {
                        let plus = if *capped { "+" } else { "" };
                        let count = format!(
                            "{}{plus} {}",
                            group_digits(n),
                            if n == 1 && plus.is_empty() {
                                "result"
                            } else {
                                "results"
                            }
                        );
                        format!("{count} \u{00b7} Provided by {provider}")
                    }
                    Matcher::Local(_) => count,
                }
            } else {
                format_count(self.scan.as_ref().map_or(self.index.len(), |s| s.count))
            }
        });
        let chosen = self.selected.len();

        let tools = if chosen > 0 {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::accent_text())
                        .child(format!("{} selected", group_digits(chosen))),
                )
                .child(ui::text_button(
                    "selection-copy",
                    Some("copy"),
                    "Copy",
                    cx.listener(|this, _, window, cx| {
                        this.copy_selection(&CopySelection, window, cx)
                    }),
                ))
                .child(ui::text_button(
                    "selection-download",
                    Some("download"),
                    "Download",
                    cx.listener(|this, _, window, cx| {
                        this.download_selection(&DownloadSelection, window, cx)
                    }),
                ))
                .child(ui::icon_button(
                    "selection-clear",
                    "close",
                    cx.listener(|this, _, _, cx| this.clear_selection(cx)),
                ))
                .with_animation("selection-tools", anim::ease(anim::NORMAL), |el, t| {
                    el.opacity(t)
                })
                .into_any_element()
        } else {
            let weak = cx.weak_entity();
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(sync)
                .child(div().w(px(6.)))
                .child(ui::icon_button(
                    "zoom-out",
                    "minus",
                    cx.listener(|this, _, window, cx| this.zoom_out(&ZoomOut, window, cx)),
                ))
                .child(ui::slider(
                    "zoom-slider",
                    settings::zoom_fraction(self.row_height.target()),
                    px(110.),
                    move |fraction, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.zoom_to(settings::row_height_at(fraction), None, cx)
                        })
                        .ok();
                    },
                ))
                .child(ui::icon_button(
                    "zoom-in",
                    "plus",
                    cx.listener(|this, _, window, cx| this.zoom_in(&ZoomIn, window, cx)),
                ))
                .into_any_element()
        };

        // Under the title: the month in view, then the count.
        let month = self
            .month_in_view
            .filter(|_| self.search.is_none())
            .map(|(year, month)| format!("{} {year}", layout::month_name(month)));
        let subtitle: Vec<String> = month.into_iter().chain(count).collect();
        let surface = theme::surface();
        let mut clear = surface;
        clear.a = 0.;

        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            // Scrolling goes through to the mosaic; clicks don't.
            .block_mouse_except_scroll()
            .flex()
            .items_center()
            .gap_3()
            .px(px(theme::CONTENT_PADDING))
            .pt(px(BAR + 10.))
            .pb(px(40.))
            // Behind the app's bar too: solid behind the bar and the title,
            // fading out over the photos below them.
            .bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(surface, (BAR + 62.) / (BAR + 100.)),
                gpui::linear_color_stop(clear, 1.),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(26.))
                            .line_height(px(32.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_color(theme::text())
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(13.))
                            .children(subtitle.into_iter().enumerate().map(|(i, part)| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .when(i > 0, |el| {
                                        el.child(
                                            div().text_color(theme::text_meta()).child("\u{00b7}"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .when(i == 0, |el| {
                                                el.font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme::text_title())
                                            })
                                            .when(i > 0, |el| el.text_color(theme::text_meta()))
                                            .child(part),
                                    )
                            })),
                    ),
            )
            .child(div().flex_1())
            .when_some(status, |el, status| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_meta())
                        .child(ui::loading_status(
                            "header-status",
                            status,
                            theme::text_meta(),
                        )),
                )
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
            .when(matches!(self.state, LoadState::Ready), |el| el.child(tools))
    }

    fn render_menu(&self, menu: &OpenMenu, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.weak_entity();
        // Each entry runs against the grid, once the menu has closed.
        let run = |f: fn(&mut Self, &[usize], &mut Window, &mut Context<Self>),
                   targets: Vec<usize>| {
            let this = this.clone();
            move |window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| f(this, &targets, window, cx))
                    .ok();
            }
        };
        let mut items = Vec::new();
        match &menu.kind {
            MenuKind::Items(targets) => {
                let many = targets.len() > 1;
                if !many {
                    items.push(MenuItem::new(
                        "open",
                        "Open",
                        run(
                            |this, t, window, cx| this.open_viewer(t[0], window, cx),
                            targets.clone(),
                        ),
                    ));
                }
                items.push(
                    MenuItem::new(
                        "copy",
                        if many {
                            format!("Copy {} items", targets.len())
                        } else {
                            "Copy".into()
                        },
                        run(|this, t, _, cx| this.copy(t.to_vec(), cx), targets.clone()),
                    )
                    .hint("Ctrl+C"),
                );
                items.push(
                    MenuItem::new(
                        "download",
                        if many {
                            format!("Download {} originals\u{2026}", targets.len())
                        } else {
                            "Download original\u{2026}".into()
                        },
                        run(
                            |this, t, _, cx| this.download(t.to_vec(), cx),
                            targets.clone(),
                        ),
                    )
                    .hint("Ctrl+S"),
                );
                let all_selected = targets.iter().all(|ix| self.selected.contains(ix));
                items.push(
                    MenuItem::new(
                        "select",
                        if all_selected { "Deselect" } else { "Select" },
                        run(
                            |this, t, _, cx| {
                                if t.iter().all(|ix| this.selected.contains(ix)) {
                                    for ix in t {
                                        this.selected.remove(ix);
                                    }
                                } else {
                                    this.selected.extend(t.iter().copied());
                                    this.select_anchor = t.first().copied();
                                }
                                cx.notify();
                            },
                            targets.clone(),
                        ),
                    )
                    .separated(),
                );
                if !self.selected.is_empty() {
                    items.push(MenuItem::new(
                        "close",
                        "Clear selection",
                        run(|this, _, _, cx| this.clear_selection(cx), Vec::new()),
                    ));
                }
            }
            MenuKind::Background => {
                items.push(
                    MenuItem::new(
                        "upload",
                        "Paste to add to iCloud",
                        run(|this, _, _, cx| this.paste(cx), Vec::new()),
                    )
                    .hint("Ctrl+V"),
                );
                items.push(
                    MenuItem::new(
                        "select",
                        "Select all",
                        run(
                            |this, _, window, cx| this.select_all(&SelectAll, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+A"),
                );
                items.push(
                    MenuItem::new(
                        "plus",
                        "Zoom in",
                        run(
                            |this, _, window, cx| this.zoom_in(&ZoomIn, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+=")
                    .separated(),
                );
                items.push(
                    MenuItem::new(
                        "minus",
                        "Zoom out",
                        run(
                            |this, _, window, cx| this.zoom_out(&ZoomOut, window, cx),
                            Vec::new(),
                        ),
                    )
                    .hint("Ctrl+-"),
                );
            }
            MenuKind::Header { first, month } => {
                let first = *first;
                let year = self
                    .days
                    .get(first)
                    .copied()
                    .flatten()
                    .map(|day| day.year());
                let mut spans = vec![(Span::Month, "month".to_string())];
                if !*month {
                    spans.insert(0, (Span::Day, "day".to_string()));
                }
                if let Some(year) = year {
                    spans.push((Span::Year, format!("all of {year}")));
                }
                for (span, what) in spans {
                    let verb = if self.span_selected(first, span) {
                        "Deselect"
                    } else {
                        "Select"
                    };
                    let label = match span {
                        Span::Year => format!("{verb} {what}"),
                        _ => format!("{verb} this {what}"),
                    };
                    let this = this.clone();
                    items.push(MenuItem::new("select", label, move |_, cx| {
                        this.update(cx, |this, cx| this.toggle_span(first, span, cx))
                            .ok();
                    }));
                }
                if !self.selected.is_empty() {
                    items.push(
                        MenuItem::new(
                            "close",
                            "Clear selection",
                            run(|this, _, _, cx| this.clear_selection(cx), Vec::new()),
                        )
                        .separated(),
                    );
                }
            }
            MenuKind::Viewer(index) => {
                let target = vec![*index];
                items.push(
                    MenuItem::new(
                        "copy",
                        "Copy",
                        run(|this, t, _, cx| this.copy(t.to_vec(), cx), target.clone()),
                    )
                    .hint("Ctrl+C"),
                );
                items.push(
                    MenuItem::new(
                        "download",
                        "Download original\u{2026}",
                        run(|this, t, _, cx| this.download(t.to_vec(), cx), target),
                    )
                    .hint("Ctrl+S"),
                );
            }
        }
        let this = cx.weak_entity();
        ui::context_menu(menu.serial, menu.position, items, move |_, cx| {
            this.update(cx, |this, cx| {
                this.menu = None;
                cx.notify();
            })
            .ok();
        })
    }

    fn render_upload_dialog(
        &self,
        upload: &PendingUpload,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let count = upload.items.len();
        let title = if count == 1 {
            "Add to iCloud Photos?".to_string()
        } else {
            format!("Add {count} items to iCloud Photos?")
        };
        let mut names: Vec<&str> = upload
            .items
            .iter()
            .take(3)
            .map(|i| i.name.as_str())
            .collect();
        if count > 3 {
            names.push("\u{2026}");
        }
        let body = format!(
            "{} will be uploaded to your library and appear on all your devices.",
            names.join(", ")
        );
        let confirm = cx.weak_entity();
        let cancel = cx.weak_entity();
        ui::confirm_dialog(
            title,
            body,
            "Add",
            move |_, cx| {
                confirm.update(cx, |this, cx| this.confirm_upload(cx)).ok();
            },
            move |_, cx| {
                cancel
                    .update(cx, |this, cx| {
                        this.pending_upload = None;
                        cx.notify();
                    })
                    .ok();
            },
        )
    }
}

impl Render for PhotoGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.month_in_view = None;
        let body: AnyElement = match &self.state {
            LoadState::Loading => centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.))
                    .child(ui::spinner("library-loading", px(22.), theme::text_muted()))
                    .child("Loading your library\u{2026}"),
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
            LoadState::Ready if self.index.is_empty() && self.device_pending.is_some() => {
                let name = self.device_pending.clone().unwrap_or_default();
                centered(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(12.))
                        .child(ui::spinner("device-searching", px(22.), theme::text_muted()))
                        .child(format!("{name} is searching\u{2026}")),
                )
            }
            LoadState::Ready if self.index.is_empty() && self.search.is_some() => centered(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_color(theme::text())
                            .child(format!("No results for \u{201c}{}\u{201d}", self.search_text.trim())),
                    )
                    .child(div().text_xs().text_color(theme::text_meta()).child(
                        "Search by date, type, favorite or file name: \u{201c}march 2021\u{201d}, \u{201c}videos\u{201d}, \u{201c}IMG_12\u{201d}.",
                    ))
                    .when(self.scan.is_some(), |el| {
                        el.child(div().text_xs().text_color(theme::text_meta()).child(
                            "Still loading the album; more may match.",
                        ))
                    }),
            ),
            LoadState::Ready if self.index.is_empty() && self.scan.is_none() => {
                centered(div().child(empty_message(&self.album)))
            }
            LoadState::Ready => self.render_mosaic(window, cx),
        };
        let header = self.render_header(cx);
        // Fade the album in when it changes.
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(body)
            .with_animation(
                ElementId::NamedInteger("album".into(), self.generation),
                anim::ease(anim::NORMAL),
                |el, t| el.opacity(t),
            );

        let menu = self.menu.as_ref().map(|menu| self.render_menu(menu, cx));
        let dialog = self
            .pending_upload
            .as_ref()
            .map(|upload| self.render_upload_dialog(upload, cx));

        div()
            .id("gallery-root")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::download_selection))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::surface())
            .child(body)
            .child(header)
            .when_some(self.viewer.as_ref(), |el, (viewer, _)| {
                el.child(viewer.clone())
            })
            .children(dialog)
            .when_some(self.toast.clone(), |el, (serial, status)| {
                el.child(ui::toast(serial, status))
            })
            .children(menu)
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
            f32::from(window.viewport_size().width)
        };
        let viewport_h = if bounds.size.height > px(0.) {
            f32::from(bounds.size.height)
        } else {
            f32::from(window.viewport_size().height)
        };

        // Zoom: re-lay out at the eased row height, holding the anchor still.
        let zooming = self.row_height.tick(ZOOM_RATE, 0.25);
        if zooming {
            self.layout_dirty = true;
            window.request_animation_frame();
        }
        self.ensure_layout(width);
        let Some(layout) = self.layout.take() else {
            return div().flex_1().into_any_element();
        };
        self.max_scroll = (layout.height - viewport_h).max(0.);
        if let Some(anchor) = &self.zoom_anchor {
            if let Some(row) = layout.row_of_item(anchor.index) {
                let top = row.y + anchor.fraction * row.height - anchor.screen_y;
                self.jump_to(top.clamp(0., self.max_scroll));
            }
            if !zooming {
                self.zoom_anchor = None;
            }
        }
        if !zooming && (Settings::get(cx).row_height - self.row_height.get()).abs() > 0.01 {
            // The zoom settled: remember it.
            let height = self.row_height.get();
            cx.defer(move |cx| Settings::update(cx, |settings| settings.row_height = height));
        }

        // Scroll: ease toward where the wheel or the scrubber pointed.
        if self.scroller.is_moving() {
            self.scroller
                .set(self.scroller.target().min(self.max_scroll));
            self.scroller.tick(SCROLL_RATE, 0.5);
            let top = self.scroller.get();
            self.scroll.set_offset(point(px(0.), px(-top)));
            window.request_animation_frame();
        }
        let top = self.top().clamp(0., self.max_scroll);
        let flying = self.scroller.is_moving()
            && !zooming
            && (self.scroller.target() - top).abs() > viewport_h * FLYING_VIEWPORTS;

        // The month under the header, once its own heading has gone under.
        self.month_in_view = layout
            .month_at(top + HEADER_SPACE * 0.7)
            .filter(|mark| mark.y + layout::MONTH_HEIGHT < top + HEADER_SPACE)
            .map(|mark| (mark.year, mark.month));

        let rows = layout.rows_in(top - OVERSCAN, top + viewport_h + OVERSCAN);
        if !flying {
            // Thumbnails for what's on screen, top to bottom.
            let on_screen = layout.rows_in(top, top + viewport_h);
            let visible: Vec<usize> = layout.rows[on_screen]
                .iter()
                .filter_map(|row| match &row.kind {
                    RowKind::Tiles(tiles) => Some(tiles.iter().map(|t| t.index)),
                    _ => None,
                })
                .flatten()
                .collect();
            self.request_visible(&visible, cx);
        }

        let selecting = !self.selected.is_empty();
        let menu_targets: &[usize] = match &self.menu {
            Some(OpenMenu {
                kind: MenuKind::Items(targets),
                ..
            }) => targets,
            _ => &[],
        };
        let mut children: Vec<AnyElement> = Vec::with_capacity(rows.len() * 6);
        let mut tiles_built = 0;
        for row in &layout.rows[rows] {
            match &row.kind {
                RowKind::Month(label) => children.push(
                    self.render_header_row(row, label, true, selecting, cx)
                        .items_end()
                        .pb(px(6.))
                        .text_size(px(20.))
                        .font_weight(FontWeight::EXTRA_BOLD)
                        .text_color(theme::text())
                        .into_any_element(),
                ),
                RowKind::Day(label) => children.push(
                    self.render_header_row(row, label, false, selecting, cx)
                        .items_center()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_title())
                        .into_any_element(),
                ),
                RowKind::Tiles(tiles) => {
                    for t in tiles {
                        let Some(item) = self.index.get(t.index) else {
                            continue;
                        };
                        let thumb = (!flying)
                            .then(|| self.thumbs_ready.get(&item.asset_id()).cloned())
                            .flatten();
                        let look = TileLook {
                            selected: self.selected.contains(&t.index),
                            selecting,
                            highlighted: menu_targets.contains(&t.index),
                        };
                        tiles_built += 1;
                        children.push(
                            self.render_tile(t.index, item, thumb, look, cx)
                                .left(px(t.x))
                                .top(px(row.y))
                                .w(px(t.width))
                                .h(px(row.height))
                                .into_any_element(),
                        );
                    }
                }
            }
        }

        // Zoomed out, more tiles are on screen than the cache usually holds.
        let wanted = (tiles_built * 3 / 2).max(IMAGE_CACHE_SIZE);
        if wanted != self.image_cache_size && !zooming {
            self.image_cache_size = wanted;
            self.image_cache
                .update(cx, |cache, _| cache.set_capacity(wanted));
        }

        let track = Track {
            content: layout.height,
            viewport: viewport_h,
            length: (viewport_h - SCRUBBER_TOP - SCRUBBER_INSET).max(1.),
        };
        self.track = Some(track);
        let scrubber =
            (self.max_scroll > 0.).then(|| self.render_scrubber(&layout, track, top, cx));
        let height = layout.height;
        self.layout = Some(layout);

        div()
            .image_cache(self.image_cache.clone())
            .id("gallery-area")
            .flex_1()
            .min_h_0()
            .relative()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_press(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_press(cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| this.focus_handle.focus(window)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    this.open_menu(MenuKind::Background, event.position, cx);
                }),
            )
            .child(
                div()
                    .id("gallery")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(height))
                            .on_scroll_wheel(cx.listener(Self::on_wheel))
                            .children(children),
                    ),
            )
            .children(scrubber)
            .into_any_element()
    }

    /// A month or day header. Its mark (on hover, or while selecting)
    /// selects everything under it; right-click offers the year too.
    fn render_header_row(
        &self,
        row: &layout::Row,
        label: &str,
        month: bool,
        selecting: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let first = row.first;
        let span = if month { Span::Month } else { Span::Day };
        let chosen = selecting && self.span_selected(first, span);
        let group = SharedString::from(format!("header-{first}-{month}"));
        let mark = div()
            .id("mark")
            .size(px(18.))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_span(first, span, cx);
                cx.stop_propagation();
            }))
            .map(|el| {
                if chosen {
                    el.bg(theme::accent())
                        .child(icon("check", px(11.), gpui::white()))
                } else {
                    el.border_2()
                        .border_color(theme::text_meta())
                        .opacity(if selecting { 0.8 } else { 0. })
                        .group_hover(group.clone(), |el| el.opacity(1.))
                }
            });
        div()
            .id(ElementId::NamedInteger(
                if month { "month-header" } else { "day-header" }.into(),
                first as u64,
            ))
            .group(group)
            .absolute()
            .left(px(theme::CONTENT_PADDING))
            .right(px(SCRUBBER_WIDTH))
            .top(px(row.y))
            .h(px(row.height))
            .flex()
            .gap(px(10.))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    this.open_menu(MenuKind::Header { first, month }, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(label.to_string())
            .child(
                // Centre the mark on the label's line.
                div()
                    .h(px(if month { 26. } else { 18. }))
                    .flex()
                    .items_center()
                    .child(mark),
            )
    }

    fn render_tile(
        &self,
        index: usize,
        item: &IndexItem,
        thumb: Option<Arc<Path>>,
        look: TileLook,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let group = SharedString::from(format!("tile-{index}"));
        let duration = match item.kind {
            Kind::Video(secs) => Some(format!("{}:{:02}", secs / 60, secs % 60)),
            _ => None,
        };
        let picture = div()
            .absolute()
            .overflow_hidden()
            .rounded(theme::RADIUS_TILE)
            .bg(theme::placeholder())
            .when_some(thumb, |el, path| {
                el.child(
                    img(path)
                        .size_full()
                        .object_fit(ObjectFit::Cover)
                        // Fade in when the thumbnail arrives.
                        .with_animation(
                            ElementId::NamedInteger("thumb-fade".into(), index as u64),
                            anim::ease(180),
                            |el, t| el.opacity(t),
                        ),
                )
            })
            // Hover: one flat scrim step, no lift or glow.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .when(look.highlighted, |el| el.bg(gpui::white().opacity(0.12)))
                    .group_hover(group.clone(), |el| el.bg(gpui::white().opacity(0.07))),
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
            });
        // A selected photo steps back inside an accent frame.
        let picture = if look.selected {
            picture
                .with_animation(
                    ElementId::NamedInteger("tile-select".into(), index as u64),
                    anim::ease(anim::FAST),
                    |el, t| el.inset(px(6. * t)),
                )
                .into_any_element()
        } else {
            picture.inset_0().into_any_element()
        };

        // The round mark in the corner: shows on hover, selects on click.
        let mark = div()
            .id(ElementId::NamedInteger("tile-mark".into(), index as u64))
            .absolute()
            .top(px(6.))
            .left(px(6.))
            .size(px(22.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_selected(index, cx);
                cx.stop_propagation();
            }))
            .map(|el| {
                if look.selected {
                    el.bg(theme::accent())
                        .child(icon("check", px(13.), gpui::white()))
                } else {
                    el.bg(theme::scrim(0.35))
                        .border_2()
                        .border_color(gpui::white().opacity(0.9))
                        .opacity(if look.selecting { 0.75 } else { 0. })
                        .group_hover(group.clone(), |el| el.opacity(1.))
                }
            });

        div()
            .id(ElementId::NamedInteger("tile".into(), index as u64))
            .group(group)
            .absolute()
            .rounded(theme::RADIUS_TILE)
            .cursor_pointer()
            .when(look.selected, |el| el.bg(theme::accent_soft()))
            .child(picture)
            .child(mark)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, _| {
                    this.press = Some((index, event.position));
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window);
                    let targets = this.targets_for(index);
                    this.open_menu(MenuKind::Items(targets), event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(cx.listener(move |this, event, window, cx| {
                this.tile_clicked(index, event, window, cx)
            }))
    }

    fn render_scrubber(
        &mut self,
        layout: &Layout,
        track: Track,
        top: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let thumb = track.position(top);
        let active = self.scrub.dragging || self.scrub.hover.is_some();
        // The date follows the pointer when it's over the track, and the
        // scroll position otherwise.
        let pointer = self.scrub.hover.filter(|_| !self.scrub.dragging);
        let date_at = pointer.unwrap_or(thumb);
        let date = (active || self.scrub.scrolled.is_some())
            .then(|| layout.month_at(track.scroll_for(date_at) + 1.))
            .flatten()
            .map(|mark| format!("{} {}", layout::month_name(mark.month), mark.year));

        let years = scrubber::year_labels(&layout.months, track, 20.);
        let ticks = scrubber::month_ticks(&layout.months, track, 6.);
        let ink = if active {
            theme::text_muted()
        } else {
            theme::text_meta()
        };

        div()
            .id("scrubber")
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(SCRUBBER_WIDTH))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.scrub.dragging = true;
                    this.scrub_to(event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(track) = this.track {
                    let y = f32::from(event.position.y - this.scroll.bounds().top()) - SCRUBBER_TOP;
                    this.scrub.hover = Some(y.clamp(0., track.length));
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && !this.scrub.dragging {
                    this.scrub.hover = None;
                    cx.notify();
                }
            }))
            .children(ticks.into_iter().map(|y| {
                div()
                    .absolute()
                    .right(px(10.))
                    .top(px(SCRUBBER_TOP + y))
                    .w(px(4.))
                    .h(px(1.))
                    .bg(theme::line())
            }))
            .children(years.into_iter().map(|(y, year)| {
                div()
                    .absolute()
                    .right(px(18.))
                    .top(px(SCRUBBER_TOP + y - 7.))
                    .h(px(14.))
                    .flex()
                    .items_center()
                    .text_size(px(10.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(ink)
                    .child(year.to_string())
            }))
            .when_some(pointer, |el, y| {
                el.child(
                    div()
                        .absolute()
                        .left(px(6.))
                        .right(px(6.))
                        .top(px(SCRUBBER_TOP + y))
                        .h(px(1.))
                        .bg(theme::text_meta()),
                )
            })
            // Where the gallery is scrolled to.
            .child(
                div()
                    .absolute()
                    .right(px(4.))
                    .top(px(SCRUBBER_TOP + thumb - 10.))
                    .w(px(3.))
                    .h(px(20.))
                    .rounded_full()
                    .bg(if active {
                        theme::accent_text()
                    } else {
                        theme::accent()
                    }),
            )
            .when_some(date, |el, date| {
                el.child(
                    div()
                        .absolute()
                        .right(px(SCRUBBER_WIDTH + 4.))
                        .top(px(SCRUBBER_TOP + date_at - 14.))
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(8.))
                        .bg(theme::popover())
                        .border_1()
                        .border_color(theme::line())
                        .shadow_lg()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .whitespace_nowrap()
                        .child(date)
                        .with_animation("scrubber-date", anim::ease(anim::FAST), |el, t| {
                            el.opacity(t)
                        }),
                )
            })
    }
}

#[derive(Clone, Copy)]
struct TileLook {
    selected: bool,
    /// Something is selected, so every tile shows its mark.
    selecting: bool,
    /// The open context menu applies to this tile.
    highlighted: bool,
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
    ui::link(
        "retry",
        "Retry",
        cx.listener(|this, _, _, cx| this.retry(cx)),
    )
}

fn empty_message(album: &Album) -> &'static str {
    match album {
        Album::All => "Your library is empty.",
        Album::Favorites => "No favorites yet. Favorite a photo on your iPhone to see it here.",
        Album::Videos => "No videos.",
        Album::RecentlyDeleted => "Nothing recently deleted.",
        Album::User(_) => "This album is empty.",
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
