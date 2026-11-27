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

use fuji_icloud::{Album, Asset, AssetId, LibraryChanges, PhotosClient, PhotosError, Resource};
use gpui::{
    AnimationExt as _, AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter,
    ExternalPaths, FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Subscription, Window, actions, canvas, div, img, point,
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
use crate::transfer::{Awaiting, PendingUpload, Trash};
use crate::ui::{self, MenuItem, Status};
use crate::viewer::{Viewer, ViewerEvent};
use crate::{net, theme, thumbs};

actions!(
    gallery,
    [
        CopySelection,
        Paste,
        DownloadSelection,
        DeleteSelection,
        FavoriteSelection,
        Cancel,
        SelectAll,
        ZoomIn,
        ZoomOut,
        ZoomReset
    ]
);

mod pages;
mod render;
mod scroll;
mod select;
mod sync;

const CONTEXT: &str = "Gallery";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-c", CopySelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-s", DownloadSelection, Some(CONTEXT)),
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("delete", DeleteSelection, Some(CONTEXT)),
        KeyBinding::new(".", FavoriteSelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl-+", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("ctrl-0", ZoomReset, Some(CONTEXT)),
    ]);
}

/// Items per full-record page (with download URLs).
/// Small, because iCloud takes about 50 ms per full record: a short page
/// gets the first thumbnails on screen sooner.
pub(crate) const PAGE: usize = 50;
/// Full-record pages kept in memory.
const MAX_PAGES: usize = 80;
const MAX_PAGE_REQUESTS: usize = 4;
/// Pages waiting to be requested. Older ones are dropped: after a fast
/// scroll, what was passed on the way no longer matters.
const MAX_QUEUED_PAGES: usize = 6;
/// Items per index-scan request (small records, no URLs).
const SCAN_PAGE: usize = 200;
const MAX_SCAN_REQUESTS: usize = net::BULK_THREADS;
const MAX_THUMB_REQUESTS: usize = 10;
/// Extra content kept built above and below the viewport.
const OVERSCAN: f32 = 600.;
/// Thumbnails are fetched ahead this many viewports below the screen (and
/// half as far above), after what's on screen.
const LOOKAHEAD_VIEWPORTS: f32 = 1.5;
/// Record pages requested for the lookahead at once, so it never crowds out
/// pages for what's on screen.
const LOOKAHEAD_PAGES: usize = 2;
/// Items looked at for the lookahead, at most.
const LOOKAHEAD_ITEMS: usize = 300;
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
/// The header floats over the top of the mosaic, which starts below it.
const HEADER_SPACE: f32 = 80.;
/// The scrubber's track starts below the header.
const SCRUBBER_TOP: f32 = HEADER_SPACE + 12.;
/// How long the scrubber's date stays up after scrolling.
const SCRUBBER_LINGER: Duration = Duration::from_millis(900);
/// How far the pointer moves with the button down before a drag starts.
const DRAG_THRESHOLD: f32 = 6.;
const TOAST_TIMEOUT: Duration = Duration::from_secs(4);
/// How often the library checks iCloud for changes made elsewhere. Each
/// check is one small request when nothing changed.
const SYNC_INTERVAL: Duration = Duration::from_secs(30);
/// How long changes from the feed are remembered, to lay over a rescan:
/// iCloud's listing can lag the feed by a while.
const RECENT_FOR: Duration = Duration::from_secs(10 * 60);

pub enum GridEvent {
    SessionExpired,
    /// The back button: return to the Collections page.
    Back,
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
    /// The change feed's position when the scan began.
    token: Option<String>,
    started: Instant,
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
    pub(crate) album: Album,
    /// The album's name, shown as the heading.
    title: SharedString,
    /// Opened from Collections: a back button before the title.
    back: bool,
    /// Bumped on album change so results for the old album are ignored.
    pub(crate) generation: u64,
    state: LoadState,
    pub(crate) index: Vec<IndexItem>,
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
    /// Where the index stands in iCloud's change feed: a sync asks only for
    /// what changed since.
    token: Option<String>,
    /// A change-feed check is out; `true` if the user asked for it (show it).
    syncing: Option<bool>,
    /// Bumped when ranks shift, so pages asked for before then are dropped.
    page_epoch: u64,
    /// How many more items iCloud counted than the last full scan listed
    /// (records it counts but can't return). A sync expects the same gap.
    count_gap: isize,
    /// Quiet syncs in a row whose count didn't fit `count_gap`.
    gap_misses: u8,
    /// What the change feed brought lately (`None`: removed), newest last.
    recent: Vec<(Instant, String, Option<IndexItem>)>,
    /// Uploads iCloud hasn't listed yet.
    pub(crate) awaiting: Option<Awaiting>,
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
    /// Single records the viewer asked for ahead of their page (each once).
    singles_asked: HashSet<AssetId>,
    // Thumbnails.
    disk_thumbs: Option<Arc<HashSet<String>>>,
    thumbs_ready: HashMap<AssetId, Arc<Path>>,
    /// Items whose thumbnails the viewer's filmstrip wants, besides those on screen.
    wanted_thumbs: Vec<usize>,
    thumbs_failed: HashSet<AssetId>,
    thumbs_in_flight: HashSet<AssetId>,
    /// Thumbnails to fetch, highest priority last.
    thumb_queue: Vec<(AssetId, Resource)>,
    /// Since when the screen has been missing thumbnails (FUJI_DEBUG timing).
    screen_wanted: Option<Instant>,
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
    pub(crate) selected: BTreeSet<usize>,
    /// Where a shift-click range starts.
    pub(crate) select_anchor: Option<usize>,
    /// A tile the button went down on, and where: a drag may follow.
    press: Option<(usize, Point<Pixels>)>,
    /// Swallow the click that ends a press which became a drag.
    pub(crate) suppress_click: bool,
    pub(crate) dragging_out: bool,
    menu: Option<OpenMenu>,
    menu_serial: u64,
    // Feedback.
    pub(crate) toast: Option<(u64, Status)>,
    toast_serial: u64,
    pub(crate) pending_upload: Option<PendingUpload>,
    /// Items the user asked to delete, waiting for them to confirm.
    pub(crate) pending_delete: Option<Vec<String>>,
    /// Items taken out of the gallery, deleted in iCloud once the moment to
    /// undo has passed.
    pub(crate) trash: Option<Trash>,
    /// Deletions sent and not yet answered.
    pub(crate) deleting: usize,
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
            back: false,
            generation: 0,
            state: LoadState::Loading,
            index: Vec::new(),
            days: Vec::new(),
            search: None,
            search_text: String::new(),
            month_in_view: None,
            device_pending: None,
            scan: None,
            token: None,
            syncing: None,
            page_epoch: 0,
            count_gap: 0,
            gap_misses: 0,
            recent: Vec::new(),
            awaiting: None,
            layout: None,
            layout_dirty: true,
            assets: HashMap::new(),
            page_ids: HashMap::new(),
            page_lru: VecDeque::new(),
            pages_requested: HashSet::new(),
            page_queue: Vec::new(),
            pages_in_flight: 0,
            singles_asked: HashSet::new(),
            disk_thumbs: None,
            thumbs_ready: HashMap::new(),
            wanted_thumbs: Vec::new(),
            thumbs_failed: HashSet::new(),
            thumbs_in_flight: HashSet::new(),
            thumb_queue: Vec::new(),
            screen_wanted: None,
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
            pending_delete: None,
            trash: None,
            deleting: 0,
            _settings: settings,
        };
        grid.list_disk_thumbs(cx);
        grid.load_album(cx);
        // Pick up what changed on other devices, now and then.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SYNC_INTERVAL).await;
                if this.update(cx, |this, cx| this.sync_quietly(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        grid
    }

    /// Show `album`, headed `title`. A rename alone just changes the heading.
    /// Show a back button (opened from Collections) or not.
    pub fn set_back(&mut self, back: bool, cx: &mut Context<Self>) {
        if self.back != back {
            self.back = back;
            cx.notify();
        }
    }

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

    /// The id of the item at `index`, known before its full record is.
    pub fn item_id(&self, index: usize) -> Option<AssetId> {
        Some(self.index.get(index)?.asset_id())
    }

    /// The shape (width / height) of the item at `index`, from the index.
    pub fn item_aspect(&self, index: usize) -> Option<f32> {
        Some(self.index.get(index)?.aspect)
    }

    /// The thumbnail of the item at `index`, if it's here yet.
    pub fn item_thumbnail(&self, index: usize) -> Option<Arc<Path>> {
        self.thumbnail_path(&self.index.get(index)?.asset_id())
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

    /// Select the items with these ids and scroll the first into view.
    pub(crate) fn show_items(&mut self, ids: &[String], cx: &mut Context<Self>) {
        if self.viewer.is_some() {
            return;
        }
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.selected = self
            .index
            .iter()
            .enumerate()
            .filter(|(_, item)| wanted.contains(item.id.as_str()))
            .map(|(ix, _)| ix)
            .collect();
        self.select_anchor = None;
        self.toast = None;
        if let Some(&first) = self.selected.iter().next()
            && let Some(row) = self.layout.as_ref().and_then(|l| l.row_of_item(first))
        {
            let viewport = f32::from(self.scroll.bounds().size.height);
            self.scroll_toward(row.y + row.height / 2. - viewport / 2., cx);
        }
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

    // ---- Feedback -------------------------------------------------------------

    pub(crate) fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.toast_serial += 1;
        let serial = self.toast_serial;
        let finished = !matches!(status, Status::Working(_));
        let linger = match status {
            Status::Undoable { .. } | Status::Listed { .. } => crate::transfer::UNDO_WINDOW,
            _ => TOAST_TIMEOUT,
        };
        self.toast = Some((serial, status));
        cx.notify();
        if finished {
            // Clear the message after a moment, unless something newer replaced it.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(linger).await;
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
        let dialog = match (&self.pending_upload, &self.pending_delete) {
            (Some(upload), _) => Some(self.render_upload_dialog(upload, cx).into_any_element()),
            (None, Some(ids)) => Some(self.render_delete_dialog(ids.len(), cx).into_any_element()),
            _ => None,
        };

        div()
            .id("gallery-root")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::download_selection))
            .on_action(cx.listener(Self::delete_selection))
            .on_action(cx.listener(Self::favorite_selection))
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
            // Files and folders dropped in are offered for upload; anything
            // else (photos dragged out of Fuji passing back over it) isn't.
            .can_drop(|value, _, _| {
                value
                    .downcast_ref::<ExternalPaths>()
                    .is_some_and(|paths| crate::transfer::can_upload(paths.paths()))
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.dropped(paths.paths().to_vec(), cx)
            }))
            .child(body)
            .child(header)
            .when(cx.has_active_drag(), |el| el.child(drop_hint()))
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

#[derive(Clone, Copy)]
struct TileLook {
    selected: bool,
    /// Something is selected, so every tile shows its mark.
    selecting: bool,
    /// The open context menu applies to this tile.
    highlighted: bool,
}

/// A small label on a tile, readable on any photo.
/// Shown over the gallery while files it can upload are dragged over
/// the window (hidden for anything else).
fn drop_hint() -> impl IntoElement {
    div()
        .id("drop-hint")
        .opacity(0.)
        .drag_over::<ExternalPaths>(|style, paths, _, _| {
            if crate::transfer::can_upload(paths.paths()) {
                style.opacity(1.)
            } else {
                style
            }
        })
        .absolute()
        .inset(px(8.))
        .rounded(px(16.))
        .border_2()
        .border_dashed()
        .border_color(theme::accent())
        .bg(theme::accent_soft())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::accent_text())
        .child("Drop to add to iCloud Photos")
}

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
        Album::Hidden => "Nothing hidden.",
        Album::Smart(_) => "None in your library.",
        Album::User(_) | Album::Shared(_) => "This album is empty.",
        Album::Memory(_) => "This memory has no photos.",
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
