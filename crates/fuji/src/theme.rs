//! Design tokens. Neutral near-black surfaces, a zinc text ladder and one
//! restrained accent (the mint from the app icon), after the dotmd UI
//! guidelines. Components use these roles, never raw colors.

use gpui::{Hsla, Pixels, px, rgb, rgba};

pub const FONT: &str = "Gabarito";

// Surfaces
pub fn surface() -> Hsla {
    rgb(0x050505).into()
}
pub fn sidebar() -> Hsla {
    rgb(0x0c0c0c).into()
}
pub fn popover() -> Hsla {
    rgb(0x101010).into()
}
pub fn placeholder() -> Hsla {
    rgb(0x1c1c1c).into()
}
pub fn line() -> Hsla {
    rgb(0x262626).into()
}
/// Subtle fill for secondary buttons (white at 6%).
pub fn fill() -> Hsla {
    rgba(0xffffff0f).into()
}
pub fn fill_hover() -> Hsla {
    rgba(0xffffff1a).into()
}
/// Row hover (white at 5%).
pub fn hover() -> Hsla {
    rgba(0xffffff0d).into()
}
/// Text field fill and border.
pub fn field() -> Hsla {
    rgba(0xffffff08).into()
}
pub fn field_border() -> Hsla {
    rgba(0xffffff1a).into()
}
/// Dark scrim, the same in any theme.
pub fn scrim(alpha: f32) -> Hsla {
    gpui::black().opacity(alpha)
}

// Text ladder (zinc)
pub fn text() -> Hsla {
    rgb(0xf4f4f5).into()
}
pub fn text_title() -> Hsla {
    rgb(0xd4d4d8).into()
}
pub fn text_muted() -> Hsla {
    rgb(0xa1a1aa).into()
}
pub fn text_meta() -> Hsla {
    rgb(0x71717a).into()
}

// Accent: a fill for selection and the primary action, and a lighter
// foreground tuned for text and icons.
pub fn accent() -> Hsla {
    rgb(0x2f8f77).into()
}
pub fn accent_hover() -> Hsla {
    rgb(0x37a487).into()
}
pub fn accent_text() -> Hsla {
    rgb(0x82d4bc).into()
}
pub fn accent_soft() -> Hsla {
    rgba(0x2f8f7733).into()
}
pub fn focus_ring() -> Hsla {
    let mut color: Hsla = accent_text();
    color.a = 0.6;
    color
}

// Status
pub fn danger() -> Hsla {
    rgb(0xf87171).into()
}

// Rounding by role
pub const RADIUS_CONTROL: Pixels = px(8.);
pub const RADIUS_TILE: Pixels = px(6.);
pub const RADIUS_DIALOG: Pixels = px(16.);

// Layout
pub const SIDEBAR_WIDTH: Pixels = px(232.);
pub const CONTENT_PADDING: f32 = 24.;
pub const CONTROL_HEIGHT: Pixels = px(32.);
