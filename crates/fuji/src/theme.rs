//! Colors and sizes in one place.

use gpui::{Hsla, Pixels, hsla, px};

pub const SIDEBAR_WIDTH: Pixels = px(220.);
pub const TILE_SIZE: f32 = 172.;
pub const TILE_GAP: f32 = 4.;
pub const GRID_PADDING: f32 = 16.;

pub fn background() -> Hsla {
    hsla(0., 0., 0.08, 1.)
}

pub fn sidebar() -> Hsla {
    hsla(0., 0., 0.11, 1.)
}

pub fn border() -> Hsla {
    hsla(0., 0., 1., 0.07)
}

pub fn text() -> Hsla {
    hsla(0., 0., 0.92, 1.)
}

pub fn text_muted() -> Hsla {
    hsla(0., 0., 0.6, 1.)
}

pub fn accent() -> Hsla {
    hsla(0.97, 0.75, 0.62, 1.)
}

pub fn selected() -> Hsla {
    hsla(0.58, 0.6, 0.5, 0.22)
}

/// Stand-in color for a thumbnail that hasn't loaded.
pub fn placeholder(index: usize) -> Hsla {
    let hue = (index as f32 * 0.137) % 1.;
    hsla(hue, 0.35, 0.32, 1.)
}
