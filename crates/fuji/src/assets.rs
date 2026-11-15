//! Files compiled into the binary: icons, the app mark and the UI font.
//!
//! Icons are Framework7 Icons (MIT, see assets/icons/LICENSE-framework7),
//! drawn to match Apple's SF Symbols: outline when idle, fill when active.
//! The Live Photo mark is Phosphor (MIT, assets/icons/LICENSE-phosphor) and
//! the spinner is drawn for Fuji. The UI font, Hanken Grotesk, is under the
//! OFL (assets/OFL-HankenGrotesk.txt), with its letter spacing tightened by
//! 2% of an em (scripts/track_font.py), since GPUI can't set tracking.

use std::borrow::Cow;

use gpui::{AssetSource, Hsla, Pixels, SharedString, Svg, prelude::*, svg};

pub struct Assets;

macro_rules! embedded {
    ($path:expr, [$($name:literal),* $(,)?]) => {
        match $path {
            $(concat!("icons/", $name, ".svg") => Some(include_bytes!(concat!("../assets/icons/", $name, ".svg")).as_slice()),)*
            "icon-96.png" => Some(include_bytes!("../assets/icon-96.png").as_slice()),
            _ => None,
        }
    };
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let bytes = embedded!(
            path,
            [
                "library",
                "library-active",
                "favorites",
                "favorites-active",
                "videos",
                "videos-active",
                "albums",
                "albums-active",
                "shared",
                "shared-active",
                "deleted",
                "deleted-active",
                "sign-out",
                "close",
                "previous",
                "next",
                "copy",
                "download",
                "heart-fill",
                "play-fill",
                "live",
                "warning",
                "check",
                "plus",
                "minus",
                "caret-down",
                "search",
                "spinner",
                "settings",
                "settings-active",
                "open",
                "select",
                "upload",
                "person",
                "sync",
                "info",
                "info-active",
            ]
        );
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

pub const FONT_BYTES: &[u8] = include_bytes!("../assets/HankenGrotesk.ttf");

/// An icon by name (see the list above), sized and tinted.
pub fn icon(name: &str, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(size)
        .flex_none()
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads() {
        for name in [
            "library",
            "favorites-active",
            "close",
            "play-fill",
            "warning",
            "check",
            "settings-active",
            "upload",
        ] {
            let bytes = Assets.load(&format!("icons/{name}.svg")).unwrap().unwrap();
            assert!(bytes.starts_with(b"<svg"));
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
        assert!(Assets.load("icon-96.png").unwrap().is_some());
    }
}
