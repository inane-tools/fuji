//! The main window: sign-in, then sidebar plus content area.

use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Account, AuthClient, Library, SavedSession};
use gpui::{
    Animation, AnimationExt as _, Context, Entity, FontWeight, SharedString, Subscription, Window,
    bounce, div, ease_in_out, ease_out_quint, prelude::*,
};

use crate::grid::PhotoGrid;
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
}

enum Screen {
    /// Trying the saved session before showing anything else.
    Resuming,
    Login(Entity<LoginView>),
    Main(Account),
}

pub struct FujiApp {
    screen: Screen,
    section: Section,
    grid: Entity<PhotoGrid>,
    _login_subscription: Option<Subscription>,
}

impl FujiApp {
    pub fn new(
        library: Arc<dyn Library>,
        saved: Option<SavedSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            screen: Screen::Resuming,
            section: Section::Library,
            grid: cx.new(|_| PhotoGrid::new(library)),
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
                    let account = task_auth.resume();
                    if account.is_ok() {
                        // Apple rotates the tokens; keep the new ones.
                        session_store::save(&task_auth.saved_session());
                    }
                    account
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(account) => this.show_main(account, cx),
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
        let login = cx.new(|cx| LoginView::new(apple_id, auth, notice, window, cx));
        self._login_subscription = Some(cx.subscribe(&login, |this, _, event, cx| match event {
            LoginEvent::SignedIn(account) => this.show_main(account.clone(), cx),
        }));
        self.screen = Screen::Login(login);
        cx.notify();
    }

    fn show_main(&mut self, account: Account, cx: &mut Context<Self>) {
        self._login_subscription = None;
        self.screen = Screen::Main(account);
        cx.notify();
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::Main(account) = &self.screen else {
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
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(theme::SIDEBAR_WIDTH)
            .h_full()
            .p_3()
            .gap_1()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .px_2()
                    .pb_3()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("fuji"),
            )
            .children(Section::ALL.into_iter().map(|section| {
                let active = section == self.section;
                div()
                    .id(SharedString::from(section.label()))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(if active {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .when(active, |el| el.bg(theme::selected()))
                    .hover(|el| el.text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    }))
                    .child(section.label())
            }))
            .child(div().flex_1())
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme::border())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().truncate().child(name))
                    .child(
                        div()
                            .id("sign-out")
                            .text_xs()
                            .text_color(theme::text_muted())
                            .cursor_pointer()
                            .hover(|el| el.text_color(theme::text()))
                            .on_click(cx.listener(|this, _, window, cx| this.sign_out(window, cx)))
                            .child("Sign out"),
                    ),
            )
    }
}

impl Render for FujiApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let account = match &self.screen {
            Screen::Resuming => return resuming().into_any_element(),
            Screen::Login(login) => return login.clone().into_any_element(),
            Screen::Main(account) => account.clone(),
        };

        let content = match self.section {
            Section::Library => div().size_full().child(self.grid.clone()),
            other => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child(format!("{} is not built yet", other.label())),
        };

        div()
            .flex()
            .size_full()
            .bg(theme::background())
            .text_color(theme::text())
            .child(self.render_sidebar(&account, cx))
            .child(div().flex_1().h_full().child(content))
            .with_animation(
                "main-fade-in",
                Animation::new(Duration::from_millis(280)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    }
}

fn resuming() -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::background())
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
