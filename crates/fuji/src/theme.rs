//! Design tokens. Neutral surfaces, a zinc text ladder and one accent, after
//! the dotmd UI guidelines. Components use these roles, never raw colors.
//!
//! Two palettes, dark (near-black) and light (Apple's whites and greys), and
//! two accents: the system's (Windows' accent color, else the mint from the
//! app icon) or Apple's blue, as on icloud.com. The app sets which with
//! [`configure`] before drawing each frame.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Hsla, Pixels, px, rgb, rgba};

pub const FONT: &str = "Gabarito";

static LIGHT: AtomicBool = AtomicBool::new(false);
static APPLE_BLUE: AtomicBool = AtomicBool::new(false);

/// Use the light palette or the dark one, and Apple's blue or the system's
/// accent.
pub fn configure(light: bool, apple_blue: bool) {
    LIGHT.store(light, Ordering::Relaxed);
    APPLE_BLUE.store(apple_blue, Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// The dark color or the light one.
fn pick(dark: u32, light: u32) -> Hsla {
    rgb(if is_light() { light } else { dark }).into()
}

fn pick_alpha(dark: u32, light: u32) -> Hsla {
    rgba(if is_light() { light } else { dark }).into()
}

// Surfaces
pub fn surface() -> Hsla {
    pick(0x050505, 0xffffff)
}
/// Cards, a step up from the surface (no outlines: fills only).
pub fn panel() -> Hsla {
    pick(0x131315, 0xf2f2f7)
}
/// Menus and dialogs, a step above cards.
pub fn popover() -> Hsla {
    pick(0x1c1c1e, 0xffffff)
}
pub fn placeholder() -> Hsla {
    pick(0x1c1c1c, 0xe5e5ea)
}
pub fn line() -> Hsla {
    pick(0x262626, 0xd1d1d6)
}
/// Subtle fill for secondary buttons.
pub fn fill() -> Hsla {
    pick_alpha(0xffffff0f, 0x0000000d)
}
pub fn fill_hover() -> Hsla {
    pick_alpha(0xffffff1a, 0x00000017)
}
/// Row hover.
pub fn hover() -> Hsla {
    pick_alpha(0xffffff0d, 0x0000000a)
}
/// A stronger hover, for fills that are already there.
pub fn field_border() -> Hsla {
    pick_alpha(0xffffff1a, 0x0000001a)
}
/// Dark scrim, the same in any theme.
pub fn scrim(alpha: f32) -> Hsla {
    gpui::black().opacity(alpha)
}

// Text ladder (zinc in the dark, Apple's labels in the light)
pub fn text() -> Hsla {
    pick(0xf4f4f5, 0x1c1c1e)
}
pub fn text_title() -> Hsla {
    pick(0xd4d4d8, 0x3a3a3c)
}
pub fn text_muted() -> Hsla {
    pick(0xa1a1aa, 0x6e6e73)
}
pub fn text_meta() -> Hsla {
    pick(0x71717a, 0x8e8e93)
}

// Accent: a fill for selection and the primary action, and a foreground
// tuned for text and icons on the surfaces.
fn base_accent() -> Hsla {
    if APPLE_BLUE.load(Ordering::Relaxed) {
        // icloud.com's blue; the brighter one Apple uses on dark.
        return pick(0x0a84ff, 0x0071e3);
    }
    static SYSTEM: OnceLock<Hsla> = OnceLock::new();
    *SYSTEM.get_or_init(|| rgb(crate::platform::accent_color().unwrap_or(0x2f8f77)).into())
}
pub fn accent() -> Hsla {
    let mut color = base_accent();
    // Dark enough for white text, light enough to see on the surface.
    color.l = color.l.clamp(0.32, 0.52);
    color
}
pub fn accent_hover() -> Hsla {
    let mut color = accent();
    color.l += if is_light() { -0.05 } else { 0.06 };
    color
}
pub fn accent_text() -> Hsla {
    let mut color = base_accent();
    color.l = if is_light() {
        color.l.min(0.45)
    } else {
        color.l.max(0.68)
    };
    color
}
pub fn accent_soft() -> Hsla {
    let mut color = accent();
    color.a = if is_light() { 0.14 } else { 0.2 };
    color
}

// Status
pub fn danger() -> Hsla {
    pick(0xf87171, 0xd70015)
}
/// The disclaimer's amber: its title, its text and its card.
pub fn warn_title() -> Hsla {
    pick(0xfde68a, 0x92400e)
}
pub fn warn_text() -> Hsla {
    pick_alpha(0xfef3c7cc, 0x78350fdd)
}
pub fn warn_fill() -> Hsla {
    pick_alpha(0xf59e0b0d, 0xf59e0b1f)
}

// Rounding by role
/// Buttons, fields and tabs are pills (GPUI clamps this to half the height).
pub const RADIUS_CONTROL: Pixels = px(999.);
pub const RADIUS_TILE: Pixels = px(6.);
pub const RADIUS_DIALOG: Pixels = px(16.);

// Layout
pub const CONTENT_PADDING: f32 = 24.;
pub const CONTROL_HEIGHT: Pixels = px(32.);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_switch() {
        configure(false, true);
        let dark = (surface(), text(), accent());
        configure(true, true);
        let light = (surface(), text(), accent());
        configure(false, false);
        assert_ne!(dark.0, light.0);
        assert_ne!(dark.1, light.1);
        // Text stays readable on its surface either way.
        assert!(dark.1.l > dark.0.l && light.1.l < light.0.l);
        assert!(light.2.l <= 0.52);
    }
}
