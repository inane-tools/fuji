//! Files compiled into the binary: icons, the app mark and the UI font.
//!
//! Icons are Phosphor (MIT, see assets/icons/LICENSE-phosphor), extracted
//! from the icon font as small SVGs: regular weight when idle, fill when
//! active. A few simple ones (check, plus, sliders, ...) are drawn for fuji. Gabarito is under the OFL (assets/OFL-Gabarito.txt).

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
                // Drawn for fuji in the same style.
                "check",
                "plus",
                "minus",
                "sidebar",
                "settings",
                "settings-active",
                "open",
                "select",
                "upload",
            ]
        );
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

pub const FONT_BYTES: &[u8] = include_bytes!("../assets/Gabarito.ttf");

/// A Phosphor icon by name (see the list above), sized and tinted.
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
