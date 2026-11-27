//! User preferences and the Settings page.
//!
//! Preferences are kept in `settings.json` in the config folder
//! (`%APPDATA%\fuji` on Windows, `~/.config/fuji` on Linux).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Context, EventEmitter, FontWeight, Global, ImageSource, Resource, ScrollHandle,
    SharedString, Subscription, Window, div, img, prelude::*, px,
};
use serde::{Deserialize, Serialize};

use crate::pairing::PairingCard;
use crate::{anim, release, theme, ui};

pub const MIN_ROW_HEIGHT: f32 = 72.;
pub const MAX_ROW_HEIGHT: f32 = 420.;
pub const DEFAULT_ROW_HEIGHT: f32 = 200.;
/// One press of the thumbnail size buttons.
const ROW_HEIGHT_STEP: f32 = 1.2;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPOSITORY: &str = "https://github.com/inane-tools/fuji";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Skip animations and smooth scrolling.
    pub reduce_motion: bool,
    /// Height of the gallery's rows: the grid zoom.
    pub row_height: f32,
    /// Light, dark, or as the system is.
    pub appearance: Appearance,
    /// The system's accent color, or Apple's blue.
    pub accent: Accent,
    /// Look for new versions on GitHub, and get them ready to install.
    pub auto_update: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    /// Windows' accent color (the app's mint elsewhere).
    #[default]
    System,
    /// Apple's blue, as on icloud.com.
    Blue,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reduce_motion: false,
            row_height: DEFAULT_ROW_HEIGHT,
            appearance: Appearance::default(),
            accent: Accent::default(),
            auto_update: true,
        }
    }
}

impl Settings {
    /// Set the theme for this frame: light or dark (the system's when
    /// following it) and the accent.
    pub fn apply_theme(&self, system_light: bool) {
        let light = match self.appearance {
            Appearance::System => system_light,
            Appearance::Light => true,
            Appearance::Dark => false,
        };
        crate::theme::configure(light, self.accent == Accent::Blue);
    }
}

impl Global for Settings {}

/// Where a row height sits on a zoom slider, 0..=1 (even steps per doubling).
pub fn zoom_fraction(row_height: f32) -> f32 {
    ((row_height / MIN_ROW_HEIGHT).ln() / (MAX_ROW_HEIGHT / MIN_ROW_HEIGHT).ln()).clamp(0., 1.)
}

/// The row height at a zoom slider position.
pub fn row_height_at(fraction: f32) -> f32 {
    MIN_ROW_HEIGHT * (MAX_ROW_HEIGHT / MIN_ROW_HEIGHT).powf(fraction.clamp(0., 1.))
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("fuji").join("settings.json"))
}

fn sanitize(mut settings: Settings) -> Settings {
    settings.row_height = if settings.row_height.is_finite() {
        settings.row_height.clamp(MIN_ROW_HEIGHT, MAX_ROW_HEIGHT)
    } else {
        DEFAULT_ROW_HEIGHT
    };
    settings
}

fn parse(bytes: &[u8]) -> Settings {
    sanitize(serde_json::from_slice(bytes).unwrap_or_default())
}

impl Settings {
    /// Read the saved settings (defaults if there are none) and make them
    /// available to every view.
    pub fn init(cx: &mut App) {
        let settings = path()
            .and_then(|path| std::fs::read(path).ok())
            .map_or_else(Settings::default, |bytes| parse(&bytes));
        anim::set_reduced(settings.reduce_motion);
        cx.set_global(settings);
    }

    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    /// Change settings; views observing them redraw, and the file is saved.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let mut settings = Self::get(cx).clone();
        change(&mut settings);
        let settings = sanitize(settings);
        if &settings == Self::get(cx) {
            return;
        }
        anim::set_reduced(settings.reduce_motion);
        crate::updater::set_automatic(settings.auto_update, cx);
        cx.set_global(settings.clone());
        // Colors may have changed: redraw everything.
        cx.refresh_windows();
        cx.background_spawn(async move {
            let (Some(path), Ok(json)) = (path(), serde_json::to_vec_pretty(&settings)) else {
                return;
            };
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        })
        .detach();
    }
}

// ---- The page -----------------------------------------------------------------

pub enum SettingsEvent {
    SignOut,
    /// The thumbnail and preview caches were deleted.
    CacheCleared,
}

pub struct SettingsView {
    name: SharedString,
    apple_id: SharedString,
    picture: Option<Arc<Path>>,
    /// Folders holding this account's cached thumbnails and previews.
    cache_dirs: Vec<PathBuf>,
    cache_bytes: Option<u64>,
    clearing: bool,
    /// The switch flipped last: the one whose knob slides.
    flipped: Option<&'static str>,
    /// The Open Source list is open.
    credits_open: bool,
    /// Pairing with the iPhone for search.
    pairing: gpui::Entity<PairingCard>,
    scroll: ScrollHandle,
    _settings: Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(
        name: SharedString,
        apple_id: SharedString,
        cache_dirs: Vec<PathBuf>,
        pairing: gpui::Entity<PairingCard>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            name,
            apple_id,
            picture: None,
            cache_dirs,
            cache_bytes: None,
            clearing: false,
            flipped: None,
            credits_open: false,
            pairing,
            scroll: ScrollHandle::new(),
            _settings: cx.observe_global::<Settings>(|_, cx| cx.notify()),
        };
        view.measure_cache(cx);
        view
    }

    pub fn set_picture(&mut self, picture: Option<Arc<Path>>, cx: &mut Context<Self>) {
        self.picture = picture;
        cx.notify();
    }

    /// Work out how much disk the cache uses (again, when the page is shown).
    pub fn measure_cache(&mut self, cx: &mut Context<Self>) {
        let dirs = self.cache_dirs.clone();
        cx.spawn(async move |this, cx| {
            let bytes = cx
                .background_spawn(async move { dirs.iter().map(|dir| dir_size(dir)).sum::<u64>() })
                .await;
            this.update(cx, |this, cx| {
                this.cache_bytes = Some(bytes);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        if self.clearing {
            return;
        }
        self.clearing = true;
        cx.notify();
        let dirs = self.cache_dirs.clone();
        cx.spawn(async move |this, cx| {
            cx.background_spawn(async move {
                for dir in dirs {
                    let _ = std::fs::remove_dir_all(&dir);
                    let _ = std::fs::create_dir_all(&dir);
                }
            })
            .await;
            this.update(cx, |this, cx| {
                this.clearing = false;
                this.cache_bytes = Some(0);
                cx.emit(SettingsEvent::CacheCleared);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The About card's updates: where updating stands, what can be done
    /// about it, and whether Fuji looks by itself.
    fn render_updates(
        &self,
        settings: &Settings,
        flipped: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        use crate::updater::{self, State};
        let state = updater::state(cx);
        let busy = matches!(
            state,
            State::Checking | State::Downloading { .. } | State::Installing
        );
        let (status, error): (SharedString, bool) = match &state {
            State::Idle if settings.auto_update => (
                "Checks at startup. Downloads and installation start when you choose.".into(),
                false,
            ),
            State::Idle => (
                "Checking by itself is off. Downloads and installation start when you choose."
                    .into(),
                false,
            ),
            State::Checking => ("Checking for updates\u{2026}".into(), false),
            State::UpToDate => ("You\u{2019}re up to date.".into(), false),
            State::Available {
                version,
                asset: Some(_),
            } => (format!("Fuji {version} is available.").into(), false),
            State::Available {
                version,
                asset: None,
            } => (
                format!("Fuji {version} is available on the releases page.").into(),
                false,
            ),
            State::Downloading { percent, .. } => {
                (format!("Downloading \u{00b7} {percent}%").into(), false)
            }
            State::Ready { version, .. } => (
                format!("Fuji {version} is ready. Installing will close and restart Fuji.").into(),
                false,
            ),
            State::Installing => ("Installing and restarting\u{2026}".into(), false),
            State::Failed(message) => (message.clone(), true),
        };

        let main_action = match &state {
            State::Ready { .. } => Some(ui::filled_button(
                "update-install",
                "Install and restart",
                false,
                |_, _, cx| updater::install(cx),
            )),
            State::Available { asset: Some(_), .. } => Some(ui::filled_button(
                "update-download",
                "Download",
                false,
                |_, _, cx| updater::download(cx),
            )),
            State::Available { asset: None, .. } => Some(ui::filled_button(
                "update-page",
                "Releases page",
                false,
                |_, _, cx| cx.open_url(&updater::releases_page()),
            )),
            _ => None,
        };
        let check = ui::text_button(
            "update-check",
            None,
            if busy {
                "Please wait\u{2026}"
            } else {
                "Check for updates"
            },
            move |_, _, cx| {
                if !busy {
                    updater::check(true, cx)
                }
            },
        )
        .when(busy, |el| el.opacity(0.5).cursor_default());

        let progress = match &state {
            State::Downloading { percent, .. } => Some(*percent),
            _ => None,
        };

        div()
            .mt(px(16.))
            .pt(px(16.))
            .border_t_1()
            .border_color(theme::hover())
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child("Updates"),
                    )
                    .when(updater::pending(cx).is_some(), |el| {
                        el.child(crate::assets::icon("update", px(14.), theme::update()))
                    }),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(if error {
                        theme::danger()
                    } else {
                        theme::text_muted()
                    })
                    .child(status),
            )
            .when_some(progress, |el, percent| {
                el.child(
                    div()
                        .h(px(6.))
                        .w_full()
                        .flex()
                        .rounded_full()
                        .overflow_hidden()
                        .bg(theme::fill_hover())
                        .child(
                            div()
                                .flex_none()
                                .h(px(6.))
                                .w(gpui::relative(f32::from(percent.max(2)) / 100.))
                                .rounded_full()
                                .bg(theme::accent_text()),
                        ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .children(main_action)
                    .child(check)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::text_meta())
                            .child("Check automatically"),
                    )
                    .child(ui::switch(
                        "auto-update",
                        settings.auto_update,
                        flipped == Some("auto-update"),
                        cx.listener(|this, _, _, cx| {
                            this.flip("auto-update", cx, |s| s.auto_update = !s.auto_update)
                        }),
                    )),
            )
    }

    /// Bring the updates (at the bottom, in About) into view.
    pub fn show_updates(&self) {
        self.scroll.scroll_to_bottom();
    }

    fn flip(&mut self, id: &'static str, cx: &mut Context<Self>, change: fn(&mut Settings)) {
        self.flipped = Some(id);
        Settings::update(cx, change);
        cx.notify();
    }

    fn resize(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.flipped = None;
        Settings::update(cx, |settings| settings.row_height *= factor);
    }
}

/// What Fuji is built on, for the Open Source section: name, what it does,
/// license, and where it lives.
const CREDITS: &[(&str, &str, &str, &str)] = &[
    (
        "GPUI",
        "UI framework",
        "Apache-2.0",
        "https://github.com/zed-industries/zed",
    ),
    (
        "Gabarito",
        "typeface",
        "OFL-1.1",
        "https://github.com/naipefoundry/gabarito",
    ),
    (
        "Framework7 Icons",
        "interface icons",
        "MIT",
        "https://github.com/framework7io/framework7-icons",
    ),
    (
        "Boxicons",
        "GitHub mark",
        "MIT",
        "https://github.com/box-icons/boxicons",
    ),
    (
        "Bluesky",
        "Bluesky mark",
        "MIT",
        "https://github.com/bluesky-social/social-app",
    ),
    (
        "ureq",
        "HTTP client",
        "MIT / Apache-2.0",
        "https://github.com/algesten/ureq",
    ),
    (
        "rustls",
        "TLS",
        "Apache-2.0 / MIT / ISC",
        "https://github.com/rustls/rustls",
    ),
    (
        "tungstenite",
        "WebSocket for device search",
        "MIT / Apache-2.0",
        "https://github.com/snapview/tungstenite-rs",
    ),
    (
        "RustCrypto",
        "AES, P-256, HKDF, HMAC, SHA, scrypt, RSA",
        "MIT / Apache-2.0",
        "https://github.com/RustCrypto",
    ),
    (
        "serde",
        "JSON and settings",
        "MIT / Apache-2.0",
        "https://github.com/serde-rs/serde",
    ),
    (
        "plist",
        "Apple property lists",
        "MIT",
        "https://github.com/ebarnard/rust-plist",
    ),
    (
        "image",
        "thumbnail decoding",
        "MIT / Apache-2.0",
        "https://github.com/image-rs/image",
    ),
    (
        "jiff",
        "dates and time zones",
        "MIT / Unlicense",
        "https://github.com/BurntSushi/jiff",
    ),
    (
        "keyring",
        "the system keychain",
        "MIT / Apache-2.0",
        "https://github.com/open-source-cooperative/keyring-rs",
    ),
    (
        "flate2",
        "compressed replies",
        "MIT / Apache-2.0",
        "https://github.com/rust-lang/flate2-rs",
    ),
    (
        "windows-rs",
        "Windows APIs",
        "MIT / Apache-2.0",
        "https://github.com/microsoft/windows-rs",
    ),
];

const DISCLAIMER: &str = "Fuji is an unofficial client and is not affiliated with, endorsed \
    by, or associated with Apple Inc. Apple, iCloud, iCloud Photos and iPhone are trademarks \
    of Apple Inc. Fuji signs in with your own Apple Account and talks to Apple's services the \
    way icloud.com does. You are responsible for how you use Fuji and for any consequences, \
    including any effect on your account or your photos. The software is provided as is, \
    without warranty of any kind.";

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = Settings::get(cx).clone();
        let flipped = self.flipped;

        let account = section(Tone::Default).child(
            div()
                .flex()
                .items_center()
                .gap(px(14.))
                .child(ui::avatar(self.picture.clone(), &self.name, px(44.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(18.))
                                .font_weight(FontWeight::EXTRA_BOLD)
                                .text_color(theme::text())
                                .truncate()
                                .child(self.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::text_meta())
                                .truncate()
                                .child(self.apple_id.clone()),
                        ),
                )
                .child(ui::text_button(
                    "settings-sign-out",
                    Some("sign-out"),
                    "Log Out",
                    cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::SignOut)),
                )),
        );

        let appearance = section(Tone::Default)
            .child(heading("Appearance", None))
            .child(row(
                "Thumbnail size",
                "Ctrl + scroll in the gallery zooms too.",
                false,
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(ui::icon_button(
                        "thumbs-smaller",
                        "minus",
                        cx.listener(|this, _, _, cx| this.resize(1. / ROW_HEIGHT_STEP, cx)),
                    ))
                    .child(ui::slider(
                        "thumbs-size",
                        zoom_fraction(settings.row_height),
                        px(140.),
                        |fraction, _, cx| {
                            Settings::update(cx, |s| s.row_height = row_height_at(fraction))
                        },
                    ))
                    .child(ui::icon_button(
                        "thumbs-larger",
                        "plus",
                        cx.listener(|this, _, _, cx| this.resize(ROW_HEIGHT_STEP, cx)),
                    )),
            ))
            .child(row(
                "Appearance",
                "Light, dark, or as Windows is set.",
                true,
                ui::segmented(
                    "appearance",
                    &["System", "Light", "Dark"],
                    match settings.appearance {
                        Appearance::System => 0,
                        Appearance::Light => 1,
                        Appearance::Dark => 2,
                    },
                    |choice, _, cx| {
                        Settings::update(cx, |s| {
                            s.appearance = match choice {
                                1 => Appearance::Light,
                                2 => Appearance::Dark,
                                _ => Appearance::System,
                            }
                        })
                    },
                ),
            ))
            .child(row(
                "Accent color",
                "Windows\u{2019} accent color, or Apple\u{2019}s blue as on iCloud.com.",
                true,
                ui::segmented(
                    "accent",
                    &["System", "Apple Blue"],
                    match settings.accent {
                        Accent::System => 0,
                        Accent::Blue => 1,
                    },
                    |choice, _, cx| {
                        Settings::update(cx, |s| {
                            s.accent = if choice == 1 {
                                Accent::Blue
                            } else {
                                Accent::System
                            }
                        })
                    },
                ),
            ))
            .child(row(
                "Reduce motion",
                "Turn off animations and smooth scrolling.",
                true,
                ui::switch(
                    "reduce-motion",
                    settings.reduce_motion,
                    flipped == Some("reduce-motion"),
                    cx.listener(|this, _, _, cx| {
                        this.flip("reduce-motion", cx, |s| s.reduce_motion = !s.reduce_motion)
                    }),
                ),
            ));

        let cache_size = match (self.clearing, self.cache_bytes) {
            (true, _) => "Clearing\u{2026}".to_string(),
            (false, Some(bytes)) => format!("Using {} on this computer.", format_bytes(bytes)),
            (false, None) => "Measuring\u{2026}".to_string(),
        };
        let updates = self.render_updates(&settings, flipped, cx);

        let storage = section(Tone::Default)
            .child(heading("Storage", None))
            .child(row(
                "Cached photos",
                cache_size,
                false,
                ui::text_button(
                    "clear-cache",
                    None,
                    "Clear",
                    cx.listener(|this, _, _, cx| this.clear_cache(cx)),
                ),
            ));

        let open = self.credits_open;
        let credits = section(Tone::Default)
            .child(
                div()
                    .id("credits-toggle")
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.credits_open = !this.credits_open;
                        cx.notify();
                    }))
                    .child(div().flex_1().child(heading(
                        "Open Source",
                        Some("The projects Fuji is built on."),
                    )))
                    .child(crate::assets::icon(
                        if open { "caret-up" } else { "caret-down" },
                        px(14.),
                        theme::text_meta(),
                    )),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .mt(px(14.))
                        .flex()
                        .flex_wrap()
                        .gap_x(px(24.))
                        .children(CREDITS.iter().map(|&(name, purpose, license, url)| {
                            div()
                                .w(px(280.))
                                .py(px(5.))
                                .flex()
                                .items_baseline()
                                .justify_between()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .id(name)
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme::text_title())
                                        .cursor_pointer()
                                        .hover(|el| el.text_color(theme::text()))
                                        .tooltip(ui::tooltip(purpose))
                                        .on_click(move |_, _, cx| cx.open_url(url))
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_meta())
                                        .truncate()
                                        .child(license),
                                )
                        })),
                )
            });

        let disclaimer = section(Tone::Warn)
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::EXTRA_BOLD)
                    .text_color(theme::warn_title())
                    .child("Disclaimer"),
            )
            .child(
                div()
                    .mt(px(10.))
                    .text_size(px(12.))
                    .line_height(px(19.))
                    .text_color(theme::warn_text())
                    .child(DISCLAIMER),
            );

        // "Fuji 1.0 Great Wave": the version and its codename.
        let view = release::view(VERSION);
        let about = section(Tone::Accent)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        img(ImageSource::Resource(Resource::Embedded(
                            "icon-96.png".into(),
                        )))
                        .size(px(44.))
                        .rounded(px(10.))
                        .flex_none(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap(px(8.))
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::BLACK)
                                    .text_color(theme::text())
                                    .child(format!("Fuji {}", release::major_minor(VERSION)))
                                    .when_some(view, |el, (codename, _)| {
                                        el.child(
                                            div().text_color(theme::accent_text()).child(codename),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text_meta())
                                    .child(match view {
                                        Some((_, title)) => format!(
                                            "v{VERSION} \u{00b7} after Hokusai\u{2019}s \u{201c}{title}\u{201d}"
                                        ),
                                        None => format!("v{VERSION}"),
                                    }),
                            ),
                    )
                    .child(ui::text_button(
                        "about-releases",
                        Some("github"),
                        "Releases",
                        |_, _, cx| cx.open_url(&format!("{REPOSITORY}/releases")),
                    )),
            )
            .child(updates)
            .child(
                div()
                    .mt(px(16.))
                    .pt(px(16.))
                    .border_t_1()
                    .border_color(theme::hover())
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(social(
                        "about-bluesky",
                        "bluesky",
                        "@inane.tools on Bluesky",
                        "https://bsky.app/profile/inane.tools",
                    ))
                    .child(social(
                        "about-x",
                        "x",
                        "@inanetools on X",
                        "https://x.com/inanetools",
                    ))
                    .child(social(
                        "about-web",
                        "globe",
                        "inane.tools",
                        "https://inane.tools",
                    ))
                    .child(social(
                        "about-github",
                        "github",
                        "Fuji on GitHub",
                        REPOSITORY,
                    ))
                    .child(div().flex_1())
                    .child(
                        gpui::svg()
                            .path("icons/inane.svg")
                            .w(px(69.))
                            .h(px(24.))
                            .text_color(theme::text_muted()),
                    ),
            );

        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(theme::surface())
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(680.))
                    .px(px(theme::CONTENT_PADDING))
                    .pt(px(12.))
                    .pb(px(40.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .mb(px(8.))
                            .text_size(px(28.))
                            .font_weight(FontWeight::BLACK)
                            .text_color(theme::text())
                            .child("Settings"),
                    )
                    .child(account)
                    .child(self.pairing.clone())
                    .child(appearance)
                    .child(storage)
                    .child(credits)
                    .child(disclaimer)
                    .child(about),
            )
    }
}

pub(crate) enum Tone {
    Default,
    /// The disclaimer.
    Warn,
    /// The About card.
    Accent,
}

/// A flat card: fills, no outlines.
pub(crate) fn section(tone: Tone) -> gpui::Div {
    let card = div()
        .flex()
        .flex_col()
        .rounded(px(14.))
        .px(px(20.))
        .py(px(16.));
    match tone {
        Tone::Default => card.bg(theme::panel()),
        Tone::Warn => card.bg(theme::warn_fill()),
        Tone::Accent => {
            let mut from = theme::accent();
            from.a = 0.28;
            let mut to = theme::accent();
            to.a = 0.08;
            card.bg(gpui::linear_gradient(
                180.,
                gpui::linear_color_stop(from, 0.),
                gpui::linear_color_stop(to, 1.),
            ))
        }
    }
}

/// A section's title, and what it's about.
pub(crate) fn heading(title: &'static str, description: Option<&'static str>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::EXTRA_BOLD)
                .text_color(theme::text())
                .child(title),
        )
        .when_some(description, |el, description| {
            el.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_meta())
                    .child(description),
            )
        })
}

/// A round button with a brand mark, opening a page.
fn social(
    id: &'static str,
    glyph: &'static str,
    title: &'static str,
    url: &'static str,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme::fill())
        .cursor_pointer()
        .hover(|el| el.bg(theme::fill_hover()))
        .tooltip(ui::tooltip(title))
        .on_click(move |_, _, cx| cx.open_url(url))
        .child(crate::assets::icon(glyph, px(15.), theme::text_title()))
}

/// A setting: what it is on the left, its control on the right.
pub(crate) fn row(
    title: &'static str,
    description: impl Into<SharedString>,
    divided: bool,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .pt(px(12.))
        .when(divided, |el| {
            el.mt(px(12.)).border_t_1().border_color(theme::hover())
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_meta())
                        .child(description.into()),
                ),
        )
        .child(control)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024. * 1024.;
    let mb = bytes as f64 / MB;
    if mb >= 1024. {
        format!("{:.1} GB", mb / 1024.)
    } else if mb >= 10. {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        // Includes a setting that no longer exists.
        let settings = parse(br#"{ "compact_sidebar": true }"#);
        assert!(!settings.reduce_motion);
        assert_eq!(settings.row_height, DEFAULT_ROW_HEIGHT);
    }

    #[test]
    fn appearance_is_saved_by_name() {
        let settings = parse(br#"{ "appearance": "light", "accent": "blue" }"#);
        assert_eq!(settings.appearance, Appearance::Light);
        assert_eq!(settings.accent, Accent::Blue);
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(json.contains(r#""appearance":"system""#), "{json}");
    }

    #[test]
    fn bad_values_are_repaired() {
        assert_eq!(parse(b"not json"), Settings::default());
        assert_eq!(parse(br#"{ "row_height": 5 }"#).row_height, MIN_ROW_HEIGHT);
        assert_eq!(
            parse(br#"{ "row_height": 99999 }"#).row_height,
            MAX_ROW_HEIGHT
        );
    }

    #[test]
    fn zoom_slider_round_trips() {
        assert_eq!(zoom_fraction(MIN_ROW_HEIGHT), 0.);
        assert!((zoom_fraction(MAX_ROW_HEIGHT) - 1.).abs() < 1e-5);
        for h in [
            MIN_ROW_HEIGHT,
            120.,
            DEFAULT_ROW_HEIGHT,
            333.,
            MAX_ROW_HEIGHT,
        ] {
            assert!((row_height_at(zoom_fraction(h)) - h).abs() < 0.01);
        }
        assert_eq!(zoom_fraction(1.), 0.);
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_bytes(0), "0.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(format_bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
