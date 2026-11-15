//! User preferences and the Settings page.
//!
//! Preferences are kept in `settings.json` in the config folder
//! (`%APPDATA%\fuji` on Windows, `~/.config/fuji` on Linux).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Context, EventEmitter, FontWeight, Global, ImageSource, Resource, SharedString,
    Subscription, Window, div, img, prelude::*, px,
};
use serde::{Deserialize, Serialize};

use crate::{anim, theme, ui};

pub const MIN_ROW_HEIGHT: f32 = 72.;
pub const MAX_ROW_HEIGHT: f32 = 420.;
pub const DEFAULT_ROW_HEIGHT: f32 = 200.;
/// One press of the thumbnail size buttons.
const ROW_HEIGHT_STEP: f32 = 1.2;

const REPOSITORY: &str = "https://github.com/inane-tools/fuji";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Skip animations and smooth scrolling.
    pub reduce_motion: bool,
    /// Height of the gallery's rows: the grid zoom.
    pub row_height: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reduce_motion: false,
            row_height: DEFAULT_ROW_HEIGHT,
        }
    }
}

impl Global for Settings {}

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
        cx.set_global(settings.clone());
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
    _settings: Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(
        name: SharedString,
        apple_id: SharedString,
        cache_dirs: Vec<PathBuf>,
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

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = Settings::get(cx).clone();
        let flipped = self.flipped;

        let appearance = group("Appearance")
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
                    .child(
                        div()
                            .w(px(52.))
                            .text_center()
                            .text_size(px(12.))
                            .text_color(theme::text_muted())
                            .child(format!("{} px", settings.row_height.round())),
                    )
                    .child(ui::icon_button(
                        "thumbs-larger",
                        "plus",
                        cx.listener(|this, _, _, cx| this.resize(ROW_HEIGHT_STEP, cx)),
                    )),
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
        let storage = group("Storage").child(row(
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

        let account = group("Account").child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(16.))
                .py(px(14.))
                .child(ui::avatar(self.picture.clone(), &self.name, px(40.)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
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
                    "Sign out",
                    cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::SignOut)),
                )),
        );

        let about = group("About").child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .px(px(16.))
                .py(px(16.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .child(
                            img(ImageSource::Resource(Resource::Embedded(
                                "icon-96.png".into(),
                            )))
                            .size(px(44.))
                            .flex_none(),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(18.))
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        .text_color(theme::text())
                                        .child("Fuji"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(theme::text_meta())
                                        .child(concat!("Version ", env!("CARGO_PKG_VERSION"))),
                                ),
                        ),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_muted())
                        .child(
                            "A fast, native iCloud Photos client for Windows and Linux, \
                             written in Rust with GPUI.",
                        ),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_meta())
                        .child(
                            "Set in Gabarito (SIL Open Font License), with Phosphor icons \
                             (MIT). Fuji is an independent project and is not affiliated \
                             with Apple. iCloud is a trademark of Apple Inc.",
                        ),
                )
                .child(div().flex().text_size(px(13.)).child(ui::link(
                    "about-source",
                    "Source code on GitHub",
                    |_, _, cx| cx.open_url(REPOSITORY),
                ))),
        );

        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .bg(theme::surface())
            .child(
                div()
                    .max_w(px(680.))
                    .px(px(theme::CONTENT_PADDING))
                    .pt(px(12.))
                    .pb(px(40.))
                    .flex()
                    .flex_col()
                    .gap(px(24.))
                    .child(
                        div()
                            .text_size(px(26.))
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_color(theme::text())
                            .child("Settings"),
                    )
                    .child(appearance)
                    .child(storage)
                    .child(account)
                    .child(about),
            )
    }
}

/// A titled card of rows.
struct Group {
    title: &'static str,
    card: gpui::Div,
}

fn group(title: &'static str) -> Group {
    Group {
        title,
        card: div()
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(theme::panel())
            .border_1()
            .border_color(theme::line()),
    }
}

impl Group {
    fn child(mut self, child: impl IntoElement) -> Self {
        self.card = self.card.child(child);
        self
    }
}

impl IntoElement for Group {
    type Element = gpui::Div;

    fn into_element(self) -> Self::Element {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .px(px(4.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_meta())
                    .child(self.title),
            )
            .child(self.card)
    }
}

/// A setting: what it is on the left, its control on the right.
fn row(
    title: &'static str,
    description: impl Into<SharedString>,
    divided: bool,
    control: impl IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(16.))
        .px(px(16.))
        .py(px(12.))
        .when(divided, |el| el.border_t_1().border_color(theme::line()))
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
                        .font_weight(FontWeight::MEDIUM)
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
    fn bad_values_are_repaired() {
        assert_eq!(parse(b"not json"), Settings::default());
        assert_eq!(parse(br#"{ "row_height": 5 }"#).row_height, MIN_ROW_HEIGHT);
        assert_eq!(
            parse(br#"{ "row_height": 99999 }"#).row_height,
            MAX_ROW_HEIGHT
        );
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(format_bytes(0), "0.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
        assert_eq!(format_bytes(412 * 1024 * 1024), "412 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
