//! The main window: sign-in, then sidebar plus content area.

use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Account, Album, AuthClient, PhotosClient, SavedSession};
use gpui::{
    Animation, AnimationExt as _, Context, Entity, FontWeight, ImageSource, Resource, SharedString,
    Subscription, Task, Window, bounce, div, ease_in_out, ease_out_quint, img, prelude::*, px,
};

use crate::assets::icon;
use crate::grid::{GridEvent, PhotoGrid};
use crate::login::{LoginEvent, LoginView};
use crate::session_store;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Library,
    Favorites,
    Videos,
    Albums,
    Shared,
    RecentlyDeleted,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::Library,
        Section::Favorites,
        Section::Videos,
        Section::Albums,
        Section::Shared,
        Section::RecentlyDeleted,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Library => "Library",
            Section::Favorites => "Favorites",
            Section::Videos => "Videos",
            Section::Albums => "Albums",
            Section::Shared => "Shared Albums",
            Section::RecentlyDeleted => "Recently Deleted",
        }
    }

    /// Icon name; the `-active` (filled) variant marks the selection.
    fn icon(self) -> &'static str {
        match self {
            Section::Library => "library",
            Section::Favorites => "favorites",
            Section::Videos => "videos",
            Section::Albums => "albums",
            Section::Shared => "shared",
            Section::RecentlyDeleted => "deleted",
        }
    }

    /// The library view this section shows, if it's one of the built-in ones.
    fn album(self) -> Option<Album> {
        match self {
            Section::Library => Some(Album::All),
            Section::Favorites => Some(Album::Favorites),
            Section::Videos => Some(Album::Videos),
            Section::RecentlyDeleted => Some(Album::RecentlyDeleted),
            Section::Albums | Section::Shared => None,
        }
    }
}

enum Screen {
    /// Trying the saved session before showing anything else.
    Resuming,
    Login(Entity<LoginView>),
    Main {
        account: Account,
        /// `None` if iCloud Photos isn't available for the account.
        grid: Option<Entity<PhotoGrid>>,
        _grid_subscription: Option<Subscription>,
        /// Saves the session's cookies now and then, and when the app quits.
        _persist: (Task<()>, Subscription),
    },
}

pub struct FujiApp {
    screen: Screen,
    section: Section,
    _login_subscription: Option<Subscription>,
}

impl FujiApp {
    pub fn new(saved: Option<SavedSession>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            screen: Screen::Resuming,
            section: Section::Library,
            _login_subscription: None,
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
        let album = self.section.album().unwrap_or(Album::All);
        let grid = PhotosClient::new(&auth, &account)
            .ok()
            .map(|client| cx.new(|cx| PhotoGrid::new(Arc::new(client), &account.dsid, album, cx)));
        let subscription = grid.as_ref().map(|grid| {
            let apple_id = account.apple_id.clone();
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
            )
        });
        self.screen = Screen::Main {
            account,
            grid,
            _grid_subscription: subscription,
            _persist: keep_session_saved(auth, cx),
        };
        cx.notify();
    }

    fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        if let (
            Some(album),
            Screen::Main {
                grid: Some(grid), ..
            },
        ) = (section.album(), &self.screen)
        {
            grid.update(cx, |grid, cx| grid.set_album(album, cx));
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

    fn render_sidebar(&self, account: &Account, cx: &mut Context<Self>) -> impl IntoElement {
        let name = if account.full_name.is_empty() {
            account.apple_id.clone()
        } else {
            account.full_name.clone()
        };
        let nav = Section::ALL.into_iter().map(|section| {
            let active = section == self.section;
            let glyph = if active {
                format!("{}-active", section.icon())
            } else {
                section.icon().to_string()
            };
            div()
                .id(SharedString::from(section.label()))
                .h(theme::CONTROL_HEIGHT)
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(theme::RADIUS_CONTROL)
                .cursor_pointer()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .map(|el| {
                    if active {
                        // Selection uses the accent, not a stronger grey.
                        el.bg(theme::accent_soft()).text_color(theme::text())
                    } else {
                        el.text_color(theme::text_muted())
                            .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                    }
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx)))
                .child(icon(
                    &glyph,
                    px(18.),
                    if active {
                        theme::accent_text()
                    } else {
                        theme::text_meta()
                    },
                ))
                .child(section.label())
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(theme::SIDEBAR_WIDTH)
            .h_full()
            .px(px(12.))
            .pt(px(16.))
            .pb(px(12.))
            .gap(px(2.))
            .bg(theme::sidebar())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(8.))
                    .pb(px(16.))
                    .child(
                        img(ImageSource::Resource(Resource::Embedded(
                            "icon-96.png".into(),
                        )))
                        .size(px(24.))
                        .flex_none(),
                    )
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_color(theme::text())
                            .child("fuji"),
                    ),
            )
            .children(nav)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(10.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_color(theme::line())
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
                                    .text_color(theme::text_title())
                                    .truncate()
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text_meta())
                                    .truncate()
                                    .child(account.apple_id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .id("sign-out")
                            .size(theme::CONTROL_HEIGHT)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::RADIUS_CONTROL)
                            .cursor_pointer()
                            .hover(|el| el.bg(theme::hover()))
                            .on_click(cx.listener(|this, _, window, cx| this.sign_out(window, cx)))
                            .child(icon("sign-out", px(16.), theme::text_muted())),
                    ),
            )
    }
}

impl Render for FujiApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One font and base color for every screen.
        let root = div()
            .size_full()
            .font_family(theme::FONT)
            .bg(theme::surface())
            .text_color(theme::text());
        let (account, grid) = match &self.screen {
            Screen::Resuming => return root.child(resuming()),
            Screen::Login(login) => return root.child(login.clone()),
            Screen::Main { account, grid, .. } => (account.clone(), grid.clone()),
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
        let content = match (self.section.album(), grid) {
            (Some(_), Some(grid)) => div().size_full().child(grid),
            (Some(_), None) => message(fuji_icloud::PhotosError::NotAvailable.to_string()),
            (None, _) => message(format!(
                "{} aren't available in fuji yet.",
                self.section.label()
            )),
        };

        root.child(
            div()
                .flex()
                .size_full()
                .child(self.render_sidebar(&account, cx))
                .child(div().flex_1().min_w_0().h_full().child(content))
                .with_animation(
                    "main-fade-in",
                    Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
                    |el, t| el.opacity(t),
                ),
        )
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
