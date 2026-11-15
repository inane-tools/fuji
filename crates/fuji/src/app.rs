//! The main window: title bar, sign-in, then sidebar plus content area.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Account, Album, AuthClient, PhotosClient, PhotosError, SavedSession, UserAlbum};
use gpui::{
    Animation, AnimationExt as _, Context, ElementId, Entity, Focusable, FontWeight, ImageSource,
    Resource, SharedString, Subscription, Task, Window, bounce, div, ease_in_out, img, prelude::*,
    px,
};

use crate::anim::{self, Eased};
use crate::assets::icon;
use crate::grid::{GridEvent, PhotoGrid};
use crate::login::{LoginEvent, LoginView};
use crate::settings::{Settings, SettingsEvent, SettingsView};
use crate::{index, net, session_store, theme, thumbs, titlebar, ui};

/// Height of a sidebar row plus the gap below it.
const NAV_PITCH: f32 = 34.;
/// How quickly the sidebar and its selection ease (per second).
const SIDEBAR_RATE: f32 = 16.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Library,
    Favorites,
    Videos,
    Shared,
    RecentlyDeleted,
    /// A user album, by its place in `FujiApp::albums`.
    Album(usize),
    Settings,
}

impl Section {
    /// The fixed sections at the top of the sidebar. The user's albums
    /// follow, under an "Albums" heading one row tall.
    const NAV: [Section; 5] = [
        Section::Library,
        Section::Favorites,
        Section::Videos,
        Section::Shared,
        Section::RecentlyDeleted,
    ];

    /// Label for the fixed sections; albums are named by `FujiApp::label`.
    fn fixed_label(self) -> &'static str {
        match self {
            Section::Library => "Library",
            Section::Favorites => "Favorites",
            Section::Videos => "Videos",
            Section::Shared => "Shared Albums",
            Section::RecentlyDeleted => "Recently Deleted",
            Section::Album(_) => "Album",
            Section::Settings => "Settings",
        }
    }

    /// Icon name; the `-active` (filled) variant marks the selection.
    fn icon(self) -> &'static str {
        match self {
            Section::Library => "library",
            Section::Favorites => "favorites",
            Section::Videos => "videos",
            Section::Shared => "shared",
            Section::RecentlyDeleted => "deleted",
            Section::Album(_) => "albums",
            Section::Settings => "settings",
        }
    }

    /// Row of the sidebar's list this section is on, counting the heading.
    fn nav_position(self) -> Option<usize> {
        match self {
            Section::Album(i) => Some(Self::NAV.len() + 1 + i),
            _ => Self::NAV.iter().position(|s| *s == self),
        }
    }

    /// Distinct per section, for element ids and animations.
    fn key(self) -> u64 {
        match self {
            Section::Album(i) => 100 + i as u64,
            _ => self.nav_position().unwrap_or(99) as u64,
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
    /// The user's albums, listed under the fixed sections.
    albums: Vec<UserAlbum>,
    /// The account picture, once it's been found or fetched.
    avatar: Option<Arc<Path>>,
    /// The sidebar's width, easing between full and compact.
    sidebar_width: Eased,
    /// Where the selection highlight is, in rows from the top.
    highlight: Eased,
    _login_subscription: Option<Subscription>,
    _settings: Subscription,
}

fn sidebar_target(cx: &gpui::App) -> f32 {
    f32::from(if Settings::get(cx).compact_sidebar {
        theme::SIDEBAR_COMPACT_WIDTH
    } else {
        theme::SIDEBAR_WIDTH
    })
}

impl FujiApp {
    pub fn new(saved: Option<SavedSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            screen: Screen::Resuming,
            section: Section::Library,
            albums: Vec::new(),
            avatar: None,
            sidebar_width: Eased::new(sidebar_target(cx)),
            highlight: Eased::new(0.),
            _login_subscription: None,
            _settings: cx.observe_global::<Settings>(|this, cx| {
                this.sidebar_width.set(sidebar_target(cx));
                cx.notify();
            }),
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
        // The album list is reloaded for this account.
        self.albums.clear();
        if matches!(self.section, Section::Settings | Section::Album(_)) {
            self.section = Section::Library;
        }
        let (album, title) = self
            .section_album(self.section)
            .unwrap_or((Album::All, "Library".into()));
        let client = PhotosClient::new(&auth, &account).ok().map(Arc::new);
        let grid = client
            .clone()
            .map(|client| cx.new(|cx| PhotoGrid::new(client, &account.dsid, album, title, cx)));
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
        self.highlight
            .jump(self.section.nav_position().unwrap_or(0) as f32);
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

    /// What a section shows in the gallery, and its heading; `None` for
    /// pages that aren't the gallery.
    fn section_album(&self, section: Section) -> Option<(Album, SharedString)> {
        let album = match section {
            Section::Library => Album::All,
            Section::Favorites => Album::Favorites,
            Section::Videos => Album::Videos,
            Section::RecentlyDeleted => Album::RecentlyDeleted,
            Section::Album(i) => {
                let album = self.albums.get(i)?;
                return Some((album.album(), album.name.clone().into()));
            }
            Section::Shared | Section::Settings => return None,
        };
        Some((album, section.fixed_label().into()))
    }

    fn label(&self, section: Section) -> SharedString {
        match section {
            Section::Album(i) => self
                .albums
                .get(i)
                .map(|album| SharedString::from(album.name.clone()))
                .unwrap_or_default(),
            _ => section.fixed_label().into(),
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
            Section::Album(i) => self.albums.get(i).map(|a| a.id.clone()),
            _ => None,
        };
        self.albums = albums;
        if let Some(id) = selected {
            let section = match self.albums.iter().position(|a| a.id == id) {
                Some(i) => Section::Album(i),
                None => Section::Library,
            };
            self.section = section;
            if let Some(position) = section.nav_position() {
                self.highlight.set(position as f32);
            }
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
            grid.update(cx, |grid, cx| grid.set_album(album, title, cx));
            return true;
        }
        false
    }

    fn select(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;
        if let Some(position) = section.nav_position() {
            self.highlight.set(position as f32);
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

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        Settings::update(cx, |settings| {
            settings.compact_sidebar = !settings.compact_sidebar
        });
    }

    /// One sidebar row. `open` is how far the sidebar is open (0 compact, 1
    /// full): labels fade with it.
    fn nav_row(
        &self,
        section: Section,
        filled: bool,
        open: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = section == self.section;
        let glyph = if active {
            format!("{}-active", section.icon())
        } else {
            section.icon().to_string()
        };
        let label = self.label(section);
        div()
            .id(ElementId::NamedInteger("nav".into(), section.key()))
            .h(theme::CONTROL_HEIGHT)
            .flex_none()
            .px(px(9.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(theme::RADIUS_CONTROL)
            .cursor_pointer()
            .overflow_hidden()
            .text_size(px(13.))
            .font_weight(FontWeight::MEDIUM)
            .map(|el| {
                if active {
                    // Selection uses the accent, not a stronger grey.
                    el.text_color(theme::text())
                        .when(filled, |el| el.bg(theme::accent_soft()))
                } else {
                    el.text_color(theme::text_muted())
                        .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| this.select(section, window, cx)))
            .child(icon(
                &glyph,
                px(18.),
                if active {
                    theme::accent_text()
                } else {
                    theme::text_meta()
                },
            ))
            .when(open > 0.05, |el| {
                el.child(div().min_w_0().truncate().opacity(open).child(label))
            })
    }

    /// The "Albums" heading: one row tall so rows keep a steady pitch for
    /// the sliding highlight. A short rule in the compact sidebar.
    fn albums_heading(open: f32) -> impl IntoElement {
        div()
            .h(theme::CONTROL_HEIGHT)
            .flex_none()
            .flex()
            .items_end()
            .px(px(9.))
            .pb(px(4.))
            .map(|el| {
                if open > 0.05 {
                    el.child(
                        div()
                            .opacity(open)
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_meta())
                            .child("Albums"),
                    )
                } else {
                    el.items_center()
                        .pb_0()
                        .child(div().w(px(18.)).h(px(1.)).bg(theme::line()))
                }
            })
    }

    fn render_sidebar(
        &self,
        account: &Account,
        width: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let compact = f32::from(theme::SIDEBAR_COMPACT_WIDTH);
        let open = ((width - compact) / (f32::from(theme::SIDEBAR_WIDTH) - compact)).clamp(0., 1.);
        let name = display_name(account);

        // The highlight slides between rows rather than jumping.
        let highlight = self.section.nav_position().map(|_| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(self.highlight.get() * NAV_PITCH))
                .h(theme::CONTROL_HEIGHT)
                .rounded(theme::RADIUS_CONTROL)
                .bg(theme::accent_soft())
        });
        let albums = (0..self.albums.len())
            .map(Section::Album)
            .collect::<Vec<_>>();
        let nav = div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(NAV_PITCH) - theme::CONTROL_HEIGHT)
            .children(highlight)
            .children(
                Section::NAV
                    .into_iter()
                    .map(|section| self.nav_row(section, false, open, cx)),
            )
            .when(!albums.is_empty(), |el| {
                el.child(Self::albums_heading(open)).children(
                    albums
                        .into_iter()
                        .map(|section| self.nav_row(section, false, open, cx)),
                )
            });
        // Many albums scroll; the settings row and account stay put.
        let nav = div()
            .id("nav")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(nav);

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .overflow_hidden()
            .px(px(12.))
            .pt(px(if titlebar::CUSTOM { 8. } else { 16. }))
            .pb(px(12.))
            .bg(theme::sidebar())
            // Without the custom title bar, the brand lives here.
            .when(!titlebar::CUSTOM, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(6.))
                        .pb(px(16.))
                        .child(
                            img(ImageSource::Resource(Resource::Embedded(
                                "icon-96.png".into(),
                            )))
                            .size(px(24.))
                            .flex_none(),
                        )
                        .when(open > 0.05, |el| {
                            el.child(
                                div()
                                    .opacity(open)
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::EXTRA_BOLD)
                                    .text_color(theme::text())
                                    .child("fuji"),
                            )
                        }),
                )
            })
            .child(nav)
            .child(
                div()
                    .pt(px(8.))
                    .child(self.nav_row(Section::Settings, true, open, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mt(px(10.))
                    .pl(px(4.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_color(theme::line())
                    .child(ui::avatar(self.avatar.clone(), &name, px(28.)))
                    .when(open > 0.05, |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .opacity(open)
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::text_title())
                                        .truncate()
                                        .child(name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_meta())
                                        .truncate()
                                        .child(account.apple_id.clone()),
                                ),
                        )
                    })
                    .when(open > 0.6, |el| {
                        el.child(
                            div()
                                .id("sign-out")
                                .size(theme::CONTROL_HEIGHT)
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme::RADIUS_CONTROL)
                                .cursor_pointer()
                                .opacity(open)
                                .hover(|el| el.bg(theme::hover()))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.sign_out(window, cx)),
                                )
                                .child(icon("sign-out", px(16.), theme::text_muted())),
                        )
                    }),
            )
    }

    /// The title bar. Over the main screen its left end continues the
    /// sidebar and holds the sidebar's toggle.
    fn render_titlebar(
        &self,
        sidebar: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // The toggle sits at the end of the sidebar's stretch of the bar, or
        // just past it when the sidebar is too narrow to hold it.
        let toggle = |cx: &mut Context<Self>| {
            ui::icon_button(
                "toggle-sidebar",
                "sidebar",
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            )
            // A button, not part of the caption.
            .occlude()
            .size(px(28.))
        };
        let compact = sidebar.is_some_and(|width| width < 150.);
        let left = match sidebar {
            Some(width) => titlebar::drag_area()
                .flex_none()
                .w(px(width))
                .overflow_hidden()
                // The mark lines up with the sidebar's icons below it.
                .pl(px(21.))
                .pr(px(10.))
                .bg(theme::sidebar())
                .child(titlebar::brand(!compact))
                .child(div().flex_1())
                .when(!compact, |el| el.child(toggle(cx))),
            None => titlebar::drag_area()
                .flex_none()
                .pl(px(14.))
                .child(titlebar::brand(true)),
        };
        div()
            .flex()
            .flex_none()
            .h(titlebar::HEIGHT)
            .w_full()
            .bg(theme::surface())
            .child(left)
            .child(
                titlebar::drag_area()
                    .flex_1()
                    .pl(px(8.))
                    .when(compact, |el| el.child(toggle(cx))),
            )
            .child(titlebar::controls(window))
    }
}

impl Render for FujiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sidebar_width.tick(SIDEBAR_RATE, 0.5) | self.highlight.tick(SIDEBAR_RATE, 0.01) {
            window.request_animation_frame();
        }
        let width = self.sidebar_width.get();

        // One font and base color for every screen.
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(theme::FONT)
            .bg(theme::surface())
            .text_color(theme::text());
        let is_main = matches!(self.screen, Screen::Main { .. });
        let root = root.when(titlebar::CUSTOM, |el| {
            el.child(self.render_titlebar(is_main.then_some(width), window, cx))
        });
        let body = div().flex_1().min_h_0();

        let (account, grid, settings) = match &self.screen {
            Screen::Resuming => return root.child(body.child(resuming())),
            Screen::Login(login) => return root.child(body.child(login.clone())),
            Screen::Main {
                account,
                grid,
                settings,
                ..
            } => (account.clone(), grid.clone(), settings.clone()),
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
            (_, true, Some(grid)) => div().size_full().child(grid),
            (_, true, None) => message(PhotosError::NotAvailable.to_string()),
            (_, false, _) => message(format!(
                "{} aren't available in fuji yet.",
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

        root.child(
            body.child(
                div()
                    .flex()
                    .size_full()
                    .child(self.render_sidebar(&account, width, cx))
                    .child(div().flex_1().min_w_0().h_full().child(content))
                    .with_animation("main-fade-in", anim::ease(anim::NORMAL), |el, t| {
                        el.opacity(t)
                    }),
            ),
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
        .child("Signing in\u{2026}")
        .with_animation(
            "resuming-pulse",
            Animation::new(Duration::from_millis(1100))
                .repeat()
                .with_easing(bounce(ease_in_out)),
            |el, t| el.opacity(0.4 + 0.6 * t),
        )
}
