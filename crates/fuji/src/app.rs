//! The main window: the title bar, which is also the navigation (Library,
//! Favorites and Collections as tabs, search, the account), then the page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{
    Account, Album, AuthClient, PhotosClient, PhotosError, SavedSession, SharedAlbum, SmartAlbum,
    UserAlbum,
};
use gpui::{
    AnimationExt as _, Bounds, Context, ElementId, Entity, Focusable, FontWeight, KeyBinding,
    Pixels, Point, SharedString, Subscription, Task, Window, actions, canvas, div, point,
    prelude::*, px,
};

use crate::anim::{self, Eased};
use crate::assets::icon;
use crate::collections::{self, Card};
use crate::device_search::DeviceSearch;
use crate::grid::{GridEvent, PhotoGrid};
use crate::input::TextInput;
use crate::login::{LoginEvent, LoginView};
use crate::settings::{SettingsEvent, SettingsView};
use crate::{index, net, session_store, theme, thumbs, titlebar, ui};

/// How quickly the tab highlight follows the selection (per second).
const PILL_RATE: f32 = 18.;
/// Below this window width the tabs show icons only.
const TAB_LABELS_MIN_WIDTH: f32 = 1040.;
/// The bar itself, measured so tab positions can be made relative to it.
const BAR_KEY: u64 = 999;
const SEARCH_CONTEXT: &str = "SearchField";

actions!(fuji, [FocusSearch, CloseSearch]);

pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-f", FocusSearch, None),
        KeyBinding::new("escape", CloseSearch, Some(SEARCH_CONTEXT)),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Library,
    Favorites,
    /// The Collections page.
    Collections,
    /// Something opened from Collections, in the gallery.
    Collection(Collection),
    Settings,
}

/// What the Collections page offers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Collection {
    Videos,
    Smart(SmartAlbum),
    Hidden,
    RecentlyDeleted,
    /// A user album, by its place in `FujiApp::albums`.
    Album(usize),
    /// A shared album, by its place in `FujiApp::shared_albums`.
    Shared(usize),
}

impl Collection {
    fn key(self) -> u64 {
        match self {
            Collection::Videos => 10,
            Collection::Smart(smart) => {
                20 + SmartAlbum::ALL
                    .iter()
                    .position(|s| *s == smart)
                    .unwrap_or(0) as u64
            }
            Collection::Hidden => 40,
            Collection::RecentlyDeleted => 41,
            Collection::Album(i) => 1_000 + i as u64,
            Collection::Shared(i) => 100_000 + i as u64,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Collection::Videos => "videos",
            Collection::Smart(smart) => match smart {
                SmartAlbum::LivePhotos => "type-live",
                SmartAlbum::Portrait => "type-portrait",
                SmartAlbum::LongExposure => "type-long-exposure",
                SmartAlbum::Panoramas => "type-panoramas",
                SmartAlbum::TimeLapse => "type-time-lapse",
                SmartAlbum::SloMo => "type-slo-mo",
                SmartAlbum::Bursts => "type-bursts",
                SmartAlbum::Screenshots => "type-screenshots",
                SmartAlbum::Animated => "type-animated",
            },
            Collection::Hidden => "hidden",
            Collection::RecentlyDeleted => "deleted",
            Collection::Album(_) => "albums",
            Collection::Shared(_) => "shared-albums",
        }
    }
}

impl Section {
    /// Icon name; the `-active` (filled) variant marks the selection.
    fn icon(self) -> &'static str {
        match self {
            Section::Library => "library",
            Section::Favorites => "favorites",
            Section::Collections | Section::Collection(_) => "collections",
            Section::Settings => "settings",
        }
    }

    /// Distinct per section, for element ids and animations.
    fn key(self) -> u64 {
        match self {
            Section::Library => 0,
            Section::Favorites => 1,
            Section::Collections => 2,
            Section::Settings => 5,
            Section::Collection(collection) => collection.key(),
        }
    }

    /// The bar entry that shows this section as selected.
    fn tab_key(self) -> u64 {
        match self {
            Section::Collection(_) => Section::Collections.key(),
            _ => self.key(),
        }
    }
}

// There is only ever one of these, so the size of `Main` costs nothing.
#[allow(clippy::large_enum_variant)]
enum Screen {
    /// Trying the saved session before showing anything else.
    Resuming,
    Login(Entity<LoginView>),
    Main {
        account: Account,
        /// `None` if iCloud Photos isn't available for the account.
        grid: Option<Entity<PhotoGrid>>,
        settings: Entity<SettingsView>,
        _subscriptions: Vec<Subscription>,
        /// Saves the session's cookies now and then, and when the app quits.
        _persist: (Task<()>, Subscription),
    },
}

pub struct FujiApp {
    screen: Screen,
    section: Section,
    /// The user's albums, on the Collections page.
    albums: Vec<UserAlbum>,
    /// Shared albums the user owns or joined.
    shared_albums: Vec<SharedAlbum>,
    /// Item counts for the Collections page, by album slug.
    counts: HashMap<String, usize>,
    /// Cover thumbnails for the Collections page, by album slug.
    covers: HashMap<String, Arc<Path>>,
    /// The library client and thumbnail folder, for loading Collections.
    photos: Option<(Arc<PhotosClient>, Arc<Path>)>,
    _collections_load: Option<Task<()>>,
    /// The account picture, once it's been found or fetched.
    avatar: Option<Arc<Path>>,
    /// The highlight behind the selected tab: its left edge and width,
    /// easing from tab to tab.
    pill_x: Eased,
    pill_width: Eased,
    pill_placed: bool,
    /// Where each bar entry was drawn last frame, by tab key.
    tab_bounds: Rc<RefCell<HashMap<u64, Bounds<Pixels>>>>,
    /// The open account menu: its opening's serial and where it hangs (from
    /// its right corner).
    account_menu: Option<(u64, Point<Pixels>)>,
    menu_serial: u64,
    search: Entity<TextInput>,
    /// The query the gallery was last given.
    search_text: SharedString,
    _login_subscription: Option<Subscription>,
    _search: Subscription,
    /// Search by the paired iPhone, when fuji is paired with one.
    device_search: Option<Arc<DeviceSearch>>,
    /// The device search in flight; replaced (cancelled) by the next.
    _device_query: Option<Task<()>>,
}

impl FujiApp {
    pub fn new(saved: Option<SavedSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search", cx).compact("search"));
        let _search = cx.observe(&search, |this, input, cx| {
            let text = SharedString::from(input.read(cx).text().to_string());
            this.search_changed(text, cx);
        });
        // Follow Windows switching between light and dark.
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();
        let mut this = Self {
            screen: Screen::Resuming,
            section: Section::Library,
            albums: Vec::new(),
            shared_albums: Vec::new(),
            counts: HashMap::new(),
            covers: HashMap::new(),
            photos: None,
            _collections_load: None,
            avatar: None,
            pill_x: Eased::new(0.),
            pill_width: Eased::new(0.),
            pill_placed: false,
            tab_bounds: Rc::default(),
            account_menu: None,
            menu_serial: 0,
            search,
            search_text: SharedString::default(),
            _login_subscription: None,
            _search,
            device_search: None,
            _device_query: None,
        };
        match saved {
            Some(saved) if saved.session_token.is_some() => this.resume(saved, window, cx),
            Some(saved) => {
                let auth = Arc::new(AuthClient::from_saved(&saved));
                this.show_login(Some(saved.apple_id), auth, None, window, cx);
            }
            None => this.show_login(None, Arc::new(AuthClient::new()), None, window, cx),
        }
        this
    }

    fn resume(&mut self, saved: SavedSession, window: &mut Window, cx: &mut Context<Self>) {
        let auth = Arc::new(AuthClient::from_saved(&saved));
        let apple_id = saved.apple_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let task_auth = auth.clone();
            let result = cx
                .background_spawn(async move {
                    session_store::restore_cookies(&task_auth);
                    let account = task_auth.resume();
                    if account.is_ok() {
                        // Apple rotates tokens and cookies; keep the new ones.
                        session_store::persist(&task_auth);
                    }
                    account
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(account) => this.show_main(account, auth, window, cx),
                Err(err) => {
                    let notice = SharedString::from(err.to_string());
                    this.show_login(Some(apple_id), auth, Some(notice), window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn show_login(
        &mut self,
        apple_id: Option<String>,
        auth: Arc<AuthClient>,
        notice: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let login = cx.new(|cx| LoginView::new(apple_id, auth.clone(), notice, window, cx));
        let login_auth = auth.clone();
        self._login_subscription = Some(cx.subscribe_in(
            &login,
            window,
            move |this, _, event, window, cx| match event {
                LoginEvent::SignedIn(account) => {
                    this.show_main(account.clone(), login_auth.clone(), window, cx)
                }
            },
        ));
        self.screen = Screen::Login(login);
        self.avatar = None;
        cx.notify();
    }

    fn show_main(
        &mut self,
        account: Account,
        auth: Arc<AuthClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._login_subscription = None;
        // The album lists are reloaded for this account.
        self.albums.clear();
        self.shared_albums.clear();
        self.counts.clear();
        self.covers.clear();
        if matches!(self.section, Section::Settings | Section::Collection(_)) {
            self.section = Section::Library;
        }
        let (album, title) = self
            .section_album(self.section)
            .unwrap_or((Album::All, "Library".into()));
        let client = PhotosClient::new(&auth, &account).ok().map(Arc::new);
        let grid = client
            .clone()
            .map(|client| cx.new(|cx| PhotoGrid::new(client, &account.dsid, album, title, cx)));
        self.photos = client
            .clone()
            .map(|client| (client, Arc::from(thumbs::cache_dir(&account.dsid))));
        if let Some(client) = client {
            self.load_albums(client, &account.dsid, cx);
        }
        let mut subscriptions = Vec::new();
        if let Some(grid) = &grid {
            let apple_id = account.apple_id.clone();
            subscriptions.push(
                cx.subscribe_in(
                    grid,
                    window,
                    move |this, _, event, window, cx| match event {
                        GridEvent::Back => this.select(Section::Collections, window, cx),
                        GridEvent::SessionExpired => {
                            let saved = session_store::load().unwrap_or_default();
                            let auth = Arc::new(AuthClient::from_saved(&SavedSession {
                                session_token: None,
                                ..saved
                            }));
                            let notice = "Your iCloud session has expired. Sign in again.".into();
                            this.show_login(Some(apple_id.clone()), auth, Some(notice), window, cx);
                        }
                    },
                ),
            );
            grid.focus_handle(cx).focus(window);
        }

        let name = display_name(&account);
        let cache_dirs = vec![
            thumbs::cache_dir(&account.dsid),
            thumbs::preview_dir(&account.dsid),
        ];
        let settings = cx.new(|cx| {
            SettingsView::new(name.into(), account.apple_id.clone().into(), cache_dirs, cx)
        });
        subscriptions.push(
            cx.subscribe_in(
                &settings,
                window,
                |this, _, event, window, cx| match event {
                    SettingsEvent::SignOut => this.sign_out(window, cx),
                    SettingsEvent::CacheCleared => {
                        if let Screen::Main {
                            grid: Some(grid), ..
                        } = &this.screen
                        {
                            grid.update(cx, |grid, cx| grid.cache_cleared(cx));
                        }
                    }
                },
            ),
        );

        self.load_avatar(&account, auth.clone(), cx);
        self.start_device_search(&account, auth.clone(), cx);
        self.search_text = SharedString::default();
        self.search.update(cx, |search, cx| search.clear(cx));
        self.screen = Screen::Main {
            account,
            grid,
            settings,
            _subscriptions: subscriptions,
            _persist: keep_session_saved(auth, cx),
        };
        cx.notify();
    }

    /// Show the account picture: the copy kept from last time at once, then
    /// the current one from iCloud.
    fn load_avatar(&mut self, account: &Account, auth: Arc<AuthClient>, cx: &mut Context<Self>) {
        let dir = thumbs::account_dir(&account.dsid);
        let account = account.clone();
        cx.spawn(async move |this, cx| {
            let find_dir = dir.clone();
            let kept = cx
                .background_spawn(async move {
                    ["avatar.jpg", "avatar.png"]
                        .iter()
                        .map(|name| find_dir.join(name))
                        .find(|path| path.exists())
                })
                .await;
            let had_one = kept.is_some();
            if let Some(path) = kept {
                this.update(cx, |this, cx| this.set_avatar(path.into(), cx))
                    .ok();
            }
            let fetched = net::run(move || {
                let bytes = fuji_icloud::account_picture(&auth, &account)?;
                let name = if crate::export::is_jpeg(&bytes) {
                    "avatar.jpg"
                } else if bytes.starts_with(b"\x89PNG") {
                    "avatar.png"
                } else {
                    return None;
                };
                std::fs::create_dir_all(&dir).ok()?;
                let path = dir.join(name);
                // Leave an unchanged picture alone, so the one on screen stays valid.
                if std::fs::read(&path).is_ok_and(|old| old == bytes) {
                    return None;
                }
                std::fs::write(&path, bytes).ok()?;
                Some(path)
            })
            .await
            .flatten();
            // A changed picture replaces a shown one on the next launch
            // (images are cached by path); a first one shows right away.
            if let Some(path) = fetched
                && !had_one
            {
                this.update(cx, |this, cx| this.set_avatar(path.into(), cx))
                    .ok();
            }
        })
        .detach();
    }

    fn set_avatar(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        if let Screen::Main { settings, .. } = &self.screen {
            settings.update(cx, |settings, cx| {
                settings.set_picture(Some(path.clone()), cx)
            });
        }
        self.avatar = Some(path);
        cx.notify();
    }

    /// What a collection shows in the gallery.
    fn collection_album(&self, collection: Collection) -> Option<Album> {
        Some(match collection {
            Collection::Videos => Album::Videos,
            Collection::Smart(smart) => Album::Smart(smart),
            Collection::Hidden => Album::Hidden,
            Collection::RecentlyDeleted => Album::RecentlyDeleted,
            Collection::Album(i) => self.albums.get(i)?.album(),
            Collection::Shared(i) => self.shared_albums.get(i)?.album(),
        })
    }

    /// What a section shows in the gallery, and its heading; `None` for
    /// pages that aren't the gallery.
    fn section_album(&self, section: Section) -> Option<(Album, SharedString)> {
        let album = match section {
            Section::Library => Album::All,
            Section::Favorites => Album::Favorites,
            Section::Collection(collection) => self.collection_album(collection)?,
            Section::Collections | Section::Settings => return None,
        };
        Some((album, self.label(section)))
    }

    fn label(&self, section: Section) -> SharedString {
        match section {
            Section::Library => "Library".into(),
            Section::Favorites => "Favorites".into(),
            Section::Collections => "Collections".into(),
            Section::Settings => "Settings".into(),
            Section::Collection(collection) => match collection {
                Collection::Videos => "Videos".into(),
                Collection::Smart(smart) => smart.title().into(),
                Collection::Hidden => "Hidden".into(),
                Collection::RecentlyDeleted => "Recently Deleted".into(),
                Collection::Album(i) => self
                    .albums
                    .get(i)
                    .map(|a| SharedString::from(a.name.clone()))
                    .unwrap_or_default(),
                Collection::Shared(i) => self
                    .shared_albums
                    .get(i)
                    .map(|a| SharedString::from(a.name.clone()))
                    .unwrap_or_default(),
            },
        }
    }

    /// The albums kept from last time at once, then the current list.
    fn load_albums(&mut self, client: Arc<PhotosClient>, dsid: &str, cx: &mut Context<Self>) {
        let path = index::albums_path(&thumbs::account_dir(dsid));
        cx.spawn(async move |this, cx| {
            let kept_path = path.clone();
            let kept = cx
                .background_spawn(async move { index::load_albums(&kept_path) })
                .await;
            if let Some(albums) = kept {
                this.update(cx, |this, cx| this.set_albums(albums, cx)).ok();
            }
            let fetched = net::run(move || {
                let albums = client.albums()?;
                index::save_albums(&path, &albums);
                Ok::<_, PhotosError>(albums)
            })
            .await;
            match fetched {
                Some(Ok(albums)) => {
                    this.update(cx, |this, cx| this.set_albums(albums, cx)).ok();
                }
                // The gallery reports an expired session; the list just stays.
                Some(Err(err)) if std::env::var_os("FUJI_DEBUG").is_some() => {
                    eprintln!("[fuji] couldn't list albums: {err}");
                }
                _ => {}
            }
        })
        .detach();
    }

    /// Replace the album list, keeping the selected album selected if it's
    /// still there (and following a rename).
    fn set_albums(&mut self, albums: Vec<UserAlbum>, cx: &mut Context<Self>) {
        if albums == self.albums {
            return;
        }
        let selected = match self.section {
            Section::Collection(Collection::Album(i)) => self.albums.get(i).map(|a| a.id.clone()),
            _ => None,
        };
        self.albums = albums;
        if let Some(id) = selected {
            let section = match self.albums.iter().position(|a| a.id == id) {
                Some(i) => Section::Collection(Collection::Album(i)),
                None => Section::Collections,
            };
            self.section = section;
            self.show_section_in_grid(section, cx);
        }
        cx.notify();
    }

    /// Point the gallery at what `section` shows, if it's a gallery section.
    fn show_section_in_grid(&self, section: Section, cx: &mut Context<Self>) -> bool {
        if let (
            Some((album, title)),
            Screen::Main {
                grid: Some(grid), ..
            },
        ) = (self.section_album(section), &self.screen)
        {
            let back = matches!(section, Section::Collection(_));
            grid.update(cx, |grid, cx| {
                grid.set_back(back, cx);
                grid.set_album(album, title, cx);
            });
            return true;
        }
        false
    }

    /// Count what the Collections page shows and find covers for it.
    fn load_collections(&mut self, cx: &mut Context<Self>) {
        let Some((client, thumb_dir)) = self.photos.clone() else {
            return;
        };
        let albums = self.albums.clone();
        self._collections_load = Some(cx.spawn(async move |this, cx| {
            let list_client = client.clone();
            let listed_albums = albums.clone();
            let listed = net::run(move || {
                let albums = listed_albums;
                let shared = list_client.shared_albums().unwrap_or_default();
                let mut wanted: Vec<Album> =
                    vec![Album::Videos, Album::Hidden, Album::RecentlyDeleted];
                wanted.extend(SmartAlbum::ALL.iter().map(|s| Album::Smart(*s)));
                wanted.extend(albums.iter().map(UserAlbum::album));
                let ids: Vec<String> = wanted.iter().filter_map(Album::count_id).collect();
                let found: HashMap<String, usize> = list_client
                    .index_counts(&ids)
                    .map(|c| c.into_iter().collect())
                    .unwrap_or_default();
                let mut counts: HashMap<String, usize> = wanted
                    .iter()
                    .filter_map(|album| {
                        let n = *found.get(&album.count_id()?)?;
                        Some((index::album_slug(album), n))
                    })
                    .collect();
                for album in &shared {
                    if let Ok(n) = list_client.count(&album.album()) {
                        counts.insert(index::album_slug(&album.album()), n);
                    }
                }
                (shared, counts)
            })
            .await;
            let Some((shared, counts)) = listed else {
                return;
            };
            // Covers for what isn't empty. Hidden and Recently Deleted show
            // no preview, as in Photos.
            let mut covered: Vec<Album> = counts
                .iter()
                .filter(|(slug, n)| {
                    **n > 0 && !matches!(slug.as_str(), "hidden" | "recently-deleted")
                })
                .filter_map(|(slug, _)| {
                    let album = match slug.as_str() {
                        "videos" => Album::Videos,
                        _ => SmartAlbum::ALL
                            .iter()
                            .map(|s| Album::Smart(*s))
                            .chain(albums.iter().map(UserAlbum::album))
                            .chain(shared.iter().map(SharedAlbum::album))
                            .find(|a| index::album_slug(a) == *slug)?,
                    };
                    Some(album)
                })
                .collect();
            covered.sort_by_key(index::album_slug);
            this.update(cx, |this, cx| {
                if this.shared_albums != shared {
                    this.shared_albums = shared;
                }
                this.counts = counts;
                cx.notify();
            })
            .ok();
            for album in covered {
                let (client, dir) = (client.clone(), thumb_dir.clone());
                let slug = index::album_slug(&album);
                let cover = net::run(move || {
                    let first = client.fetch(&album, 0, 1).ok()?.into_iter().next()?;
                    let thumb = first.thumbnail.as_ref()?;
                    thumbs::ensure(&client, &dir, &first.id, thumb).ok()
                })
                .await
                .flatten();
                if let Some(path) = cover {
                    this.update(cx, |this, cx| {
                        this.covers.insert(slug, path.into());
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
    }

    fn select(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;
        self.account_menu = None;
        if section == Section::Collections {
            self.load_collections(cx);
        }
        if self.show_section_in_grid(section, cx)
            && let Screen::Main {
                grid: Some(grid), ..
            } = &self.screen
        {
            grid.focus_handle(cx).focus(window);
        }
        if section == Section::Settings
            && let Screen::Main { settings, .. } = &self.screen
        {
            settings.update(cx, |settings, cx| settings.measure_cache(cx));
        }
        cx.notify();
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Main { account, .. } = &self.screen else {
            return;
        };
        let apple_id = account.apple_id.clone();
        // Keep the trust token so signing back in on this machine skips the code.
        let saved = session_store::load().unwrap_or_default();
        cx.background_spawn(async { session_store::sign_out() })
            .detach();
        let auth = Arc::new(AuthClient::from_saved(&SavedSession {
            session_token: None,
            ..saved
        }));
        self.show_login(Some(apple_id), auth, None, window, cx);
    }

    // ---- Search ---------------------------------------------------------------

    fn search_changed(&mut self, text: SharedString, cx: &mut Context<Self>) {
        if text == self.search_text {
            return;
        }
        self.search_text = text.clone();
        // Searching from a page that isn't the gallery searches the library.
        if self.section_album(self.section).is_none() {
            self.section = Section::Library;
            self.show_section_in_grid(Section::Library, cx);
        }
        if let Screen::Main {
            grid: Some(grid), ..
        } = &self.screen
        {
            grid.update(cx, |grid, cx| grid.set_search(&text, cx));
        }
        self.ask_device(text, cx);
        cx.notify();
    }

    fn with_grid(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut PhotoGrid, &mut Context<PhotoGrid>),
    ) {
        if let Screen::Main {
            grid: Some(grid), ..
        } = &self.screen
        {
            grid.update(cx, f);
        }
    }

    /// If fuji is paired with a device, let it search too (Skybridge).
    fn start_device_search(
        &mut self,
        account: &Account,
        auth: Arc<AuthClient>,
        cx: &mut Context<Self>,
    ) {
        self.device_search = None;
        let (dsid, apple_id) = (account.dsid.clone(), account.apple_id.clone());
        cx.spawn(async move |this, cx| {
            let search = cx
                .background_spawn(async move {
                    let pairing = session_store::load_pairing()?;
                    DeviceSearch::start(auth, dsid, apple_id, pairing.key)
                })
                .await;
            this.update(cx, |this, _| this.device_search = search.map(Arc::new))
                .ok();
        })
        .detach();
    }

    /// Ask the paired device to search for `text`, once typing pauses; its
    /// results replace fuji's own when they come.
    fn ask_device(&mut self, text: SharedString, cx: &mut Context<Self>) {
        let Some(search) = self.device_search.clone() else {
            return;
        };
        if text.trim().is_empty() {
            self._device_query = None;
            self.with_grid(cx, |grid, cx| grid.set_device_pending(None, cx));
            return;
        }
        self._device_query = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            // Say a device is on it, by name once known.
            let asking = SharedString::from(
                search
                    .device_name()
                    .unwrap_or_else(|| "your iPhone".to_string()),
            );
            this.update(cx, |this, cx| {
                this.with_grid(cx, |grid, cx| grid.set_device_pending(Some(asking), cx))
            })
            .ok();
            let result = search.search(text.trim().to_string()).await;
            this.update(cx, |this, cx| {
                if this.search_text != text {
                    return;
                }
                match result {
                    Ok(found) => {
                        let provider = SharedString::from(found.provider);
                        this.with_grid(cx, |grid, cx| {
                            grid.set_device_results(&text, found.ids, provider, found.capped, cx)
                        });
                    }
                    Err(err) => {
                        if std::env::var_os("FUJI_DEBUG").is_some() {
                            eprintln!("device search: {err}");
                        }
                        this.with_grid(cx, |grid, cx| grid.set_device_pending(None, cx));
                    }
                }
            })
            .ok();
        }));
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.screen, Screen::Main { .. }) {
            self.search.focus_handle(cx).focus(window);
        }
    }

    /// Escape in the field: clear it, then give the gallery the keyboard.
    fn close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| search.clear(cx));
        if let Screen::Main {
            grid: Some(grid), ..
        } = &self.screen
        {
            grid.focus_handle(cx).focus(window);
        }
    }

    // ---- The bar ----------------------------------------------------------------

    fn open_account_menu(&mut self, cx: &mut Context<Self>) {
        if self.account_menu.take().is_some() {
            cx.notify();
            return;
        }
        let at = self
            .tab_bounds
            .borrow()
            .get(&Section::Settings.tab_key())
            .map(|b| {
                point(
                    b.origin.x + b.size.width,
                    b.origin.y + b.size.height + px(6.),
                )
            })
            .unwrap_or_else(|| point(px(800.), titlebar::HEIGHT));
        self.menu_serial += 1;
        self.account_menu = Some((self.menu_serial, at));
        cx.notify();
    }

    /// The account's picture and name, then Settings and Log Out.
    fn render_account_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (serial, at) = self.account_menu?;
        let Screen::Main { account, .. } = &self.screen else {
            return None;
        };
        let name = display_name(account);
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(8.))
            .pt(px(6.))
            .pb(px(10.))
            .mb(px(4.))
            .border_b_1()
            .border_color(theme::line())
            .child(ui::avatar(self.avatar.clone(), &name, px(36.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .truncate()
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_meta())
                            .truncate()
                            .child(account.apple_id.clone()),
                    ),
            )
            .into_any_element();
        let this = cx.entity().downgrade();
        let (to_settings, to_sign_out, on_dismiss) = (this.clone(), this.clone(), this);
        let items = vec![
            ui::MenuItem::new("settings", "Settings", move |window, cx| {
                to_settings
                    .update(cx, |this, cx| this.select(Section::Settings, window, cx))
                    .ok();
            }),
            ui::MenuItem::new("sign-out", "Log Out", move |window, cx| {
                to_sign_out
                    .update(cx, |this, cx| this.sign_out(window, cx))
                    .ok();
            }),
        ];
        Some(ui::popup_menu(
            serial,
            at,
            gpui::Corner::TopRight,
            Some(header),
            items,
            move |_, cx| {
                on_dismiss
                    .update(cx, |this, cx| {
                        this.account_menu = None;
                        cx.notify();
                    })
                    .ok();
            },
        ))
    }

    /// Something in the bar that remembers where it was drawn, so the
    /// highlight can find it.
    fn measured(&self, key: u64) -> impl IntoElement + use<> {
        let store = self.tab_bounds.clone();
        canvas(
            move |bounds, window, _| {
                let changed = store.borrow_mut().insert(key, bounds) != Some(bounds);
                if changed {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        // Pinned to the corner: without an inset, an absolute child starts
        // after the padding, which shifted every tab by its padding.
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// A tab: icon, label (when there's room), and for the Albums menu a caret.
    fn tab(
        &self,
        key: u64,
        icon_name: &'static str,
        label: Option<SharedString>,
        active: bool,
        caret: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let glyph = if active {
            format!("{icon_name}-active")
        } else {
            icon_name.to_string()
        };
        div()
            .id(ElementId::NamedInteger("tab".into(), key))
            .relative()
            .occlude()
            .h(px(28.))
            .flex_none()
            .px(px(if label.is_some() { 10. } else { 7. }))
            .flex()
            .items_center()
            .gap(px(7.))
            .rounded(theme::RADIUS_CONTROL)
            .cursor_pointer()
            .text_size(px(13.))
            .font_weight(FontWeight::MEDIUM)
            .map(|el| {
                if active {
                    el.text_color(theme::text())
                } else {
                    el.text_color(theme::text_muted())
                        .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                }
            })
            .child(self.measured(key))
            .child(icon(
                &glyph,
                px(16.),
                if active {
                    theme::accent_text()
                } else {
                    theme::text_meta()
                },
            ))
            .when_some(label, |el, label| {
                el.child(div().max_w(px(160.)).truncate().child(label))
            })
            .when(caret, |el| {
                el.child(icon("caret-down", px(11.), theme::text_meta()))
            })
    }

    /// The title bar: the mark, a tab per section (albums in a menu),
    /// search, settings and the account, then the window buttons. Before
    /// sign-in only the mark and the buttons.
    fn render_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let main = matches!(self.screen, Screen::Main { .. });
        let wide = f32::from(window.viewport_size().width) >= TAB_LABELS_MIN_WIDTH;

        // The highlight eases to wherever the selected entry was drawn.
        let selected = self
            .tab_bounds
            .borrow()
            .get(&self.section.tab_key())
            .copied();
        let bar = self.tab_bounds.borrow().get(&BAR_KEY).copied();
        let pill = match (main, selected, bar) {
            (true, Some(bounds), Some(bar)) => {
                // Relative to the bar, which is where the highlight is drawn.
                let x = f32::from(bounds.origin.x - bar.origin.x);
                let w = f32::from(bounds.size.width);
                if self.pill_placed {
                    self.pill_x.set(x);
                    self.pill_width.set(w);
                } else {
                    self.pill_x.jump(x);
                    self.pill_width.jump(w);
                    self.pill_placed = true;
                }
                if self.pill_x.tick(PILL_RATE, 0.3) | self.pill_width.tick(PILL_RATE, 0.3) {
                    window.request_animation_frame();
                }
                Some(
                    div()
                        .absolute()
                        .left(px(self.pill_x.get()))
                        .top(bounds.origin.y - bar.origin.y)
                        .w(px(self.pill_width.get()))
                        .h(bounds.size.height)
                        .rounded(theme::RADIUS_CONTROL)
                        .bg(theme::accent_soft()),
                )
            }
            _ => None,
        };

        let brand = titlebar::drag_area()
            .flex_none()
            .pl(px(14.))
            .pr(px(10.))
            .child(titlebar::brand());

        let tabs = main.then(|| {
            let mut tabs = titlebar::drag_area().flex_none().gap(px(2.));
            for section in [Section::Library, Section::Favorites, Section::Collections] {
                let active = self.section.tab_key() == section.key();
                tabs = tabs.child(
                    self.tab(
                        section.key(),
                        section.icon(),
                        wide.then(|| self.label(section)),
                        active,
                        false,
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select(section, window, cx)),
                    ),
                );
            }
            tabs
        });

        let right = main.then(|| {
            let (name, avatar) = match &self.screen {
                Screen::Main { account, .. } => (display_name(account), self.avatar.clone()),
                _ => (String::new(), None),
            };
            titlebar::drag_area()
                .flex_none()
                .gap(px(4.))
                .pr(px(8.))
                .child(
                    div()
                        .occlude()
                        .key_context(SEARCH_CONTEXT)
                        .on_action(cx.listener(Self::close_search))
                        .w(px(if wide { 240. } else { 170. }))
                        .mr(px(4.))
                        .child(self.search.clone()),
                )
                .child(
                    div()
                        .id("account")
                        .relative()
                        .occlude()
                        .size(px(28.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|el| el.bg(theme::hover()))
                        .when(self.account_menu.is_some(), |el| el.bg(theme::hover()))
                        .on_click(cx.listener(|this, _, _, cx| this.open_account_menu(cx)))
                        .child(self.measured(Section::Settings.tab_key()))
                        .child(ui::avatar(avatar, &name, px(22.))),
                )
        });

        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .h(titlebar::HEIGHT)
            .w_full()
            .bg(theme::surface())
            .child(self.measured(BAR_KEY))
            .children(pill)
            .child(brand)
            .children(tabs)
            .child(titlebar::drag_area().flex_1())
            .children(right)
            .when(titlebar::CUSTOM, |el| el.child(titlebar::controls(window)))
    }
}

impl Render for FujiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Light or dark, and the accent, before anything is drawn.
        let system_light = matches!(
            window.appearance(),
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
        );
        crate::settings::Settings::get(cx).apply_theme(system_light);
        // One font and base color for every screen. The bar is drawn on
        // every system: where the system draws the title bar, it's a
        // toolbar under it.
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(theme::FONT)
            .bg(theme::surface())
            .text_color(theme::text())
            .on_action(cx.listener(Self::focus_search));
        let main = matches!(self.screen, Screen::Main { .. });
        let root = root
            .when(titlebar::CUSTOM || main, |el| {
                el.child(self.render_bar(window, cx))
            })
            .children(self.render_account_menu(cx));
        let body = div().flex_1().min_h_0();

        let (grid, settings) = match &self.screen {
            Screen::Resuming => return root.child(body.child(resuming())),
            Screen::Login(login) => return root.child(body.child(login.clone())),
            Screen::Main { grid, settings, .. } => (grid.clone(), settings.clone()),
        };

        let message = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme::text_muted())
                .child(text)
        };
        let is_gallery = self.section_album(self.section).is_some();
        let content = match (self.section, is_gallery, grid) {
            (Section::Settings, _, _) => div().size_full().child(settings),
            (Section::Collections, _, _) => self.render_collections(cx),
            (_, true, Some(grid)) => div().size_full().child(grid),
            (_, true, None) => message(PhotosError::NotAvailable.to_string()),
            (_, false, _) => message(format!(
                "{} aren't available in Fuji yet.",
                self.label(self.section)
            )),
        };
        // Pages that aren't the gallery fade in; the gallery fades itself
        // per album.
        let content = if is_gallery {
            content.into_any_element()
        } else {
            content
                .with_animation(
                    ElementId::NamedInteger("section".into(), self.section.key()),
                    anim::ease(anim::NORMAL),
                    |el, t| el.opacity(t),
                )
                .into_any_element()
        };

        root.child(body.child(div().size_full().child(content).with_animation(
            "main-fade-in",
            anim::ease(anim::NORMAL),
            |el, t| el.opacity(t),
        )))
    }
}

impl FujiApp {
    /// Albums, shared albums, media types and utilities, as cards.
    fn render_collections(&self, cx: &mut Context<Self>) -> gpui::Div {
        let card = |this: &Self, collection: Collection, cx: &mut Context<Self>| {
            let album = this.collection_album(collection);
            let slug = album.as_ref().map(index::album_slug).unwrap_or_default();
            collections::card(
                Card {
                    key: collection.key(),
                    title: this.label(Section::Collection(collection)),
                    count: this.counts.get(&slug).copied(),
                    cover: this.covers.get(&slug).cloned(),
                    icon: collection.icon(),
                    badge: matches!(collection, Collection::Videos | Collection::Smart(_)),
                },
                cx.listener(move |this, _, window, cx| {
                    this.select(Section::Collection(collection), window, cx)
                }),
            )
        };
        let mut page = div().flex().flex_col().gap(px(28.)).child(
            div()
                .text_size(px(28.))
                .font_weight(FontWeight::BLACK)
                .text_color(theme::text())
                .child("Collections"),
        );
        if !self.albums.is_empty() {
            let cards = (0..self.albums.len())
                .map(|i| card(self, Collection::Album(i), cx))
                .collect();
            page = page.child(collections::section("Albums", cards));
        }
        if !self.shared_albums.is_empty() {
            let cards = (0..self.shared_albums.len())
                .map(|i| card(self, Collection::Shared(i), cx))
                .collect();
            page = page.child(collections::section("Shared Albums", cards));
        }
        // Media types the library has any of (all of them until counted).
        let counted = !self.counts.is_empty();
        let media: Vec<Collection> = std::iter::once(Collection::Videos)
            .chain(SmartAlbum::ALL.iter().map(|s| Collection::Smart(*s)))
            .filter(|c| {
                let slug = self
                    .collection_album(*c)
                    .map(|a| index::album_slug(&a))
                    .unwrap_or_default();
                !counted || self.counts.get(&slug).is_some_and(|n| *n > 0)
            })
            .collect();
        let cards = media.into_iter().map(|c| card(self, c, cx)).collect();
        page = page.child(collections::section("Media Types", cards));
        let cards = [Collection::Hidden, Collection::RecentlyDeleted]
            .into_iter()
            .map(|c| card(self, c, cx))
            .collect();
        page = page.child(collections::section("Utilities", cards));

        div().size_full().child(
            div()
                .id("collections")
                .size_full()
                .overflow_y_scroll()
                .bg(theme::surface())
                .child(page.px(px(theme::CONTENT_PADDING)).pt(px(10.)).pb(px(40.))),
        )
    }
}

fn display_name(account: &Account) -> String {
    if account.full_name.is_empty() {
        account.apple_id.clone()
    } else {
        account.full_name.clone()
    }
}

/// iCloud refreshes its cookies as the app talks to it; save them every few
/// minutes and on quit so the next launch can continue this session.
fn keep_session_saved(
    auth: Arc<AuthClient>,
    cx: &mut Context<FujiApp>,
) -> (Task<()>, Subscription) {
    const EVERY: Duration = Duration::from_secs(5 * 60);
    let periodic_auth = auth.clone();
    let periodic = cx.spawn(async move |_, cx| {
        loop {
            cx.background_executor().timer(EVERY).await;
            let auth = periodic_auth.clone();
            cx.background_spawn(async move { session_store::persist(&auth) })
                .await;
        }
    });
    let on_quit = cx.on_app_quit(move |_, cx| {
        let auth = auth.clone();
        cx.background_spawn(async move { session_store::persist(&auth) })
    });
    (periodic, on_quit)
}

fn resuming() -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::text_muted())
        .text_sm()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(12.))
                .child(ui::spinner("resuming", px(22.), theme::text_muted()))
                .child("Signing in\u{2026}"),
        )
}
