//! The window's own title bar, drawn in the app's colors.
//!
//! On Windows the system title bar is hidden and this replaces it. The bar
//! and its buttons are reported to Windows as the real caption and caption
//! buttons, so everything the system does with a title bar still works:
//! dragging, double-click to maximize, snapping, the snap layouts flyout on
//! the maximize button, and the system menu on right-click. The button
//! glyphs come from Windows' own icon font.
//!
//! Elsewhere the system draws the title bar and none of this is shown.

use gpui::{
    Div, Hsla, ImageSource, Pixels, Resource, Stateful, Window, WindowControlArea, div, img,
    prelude::*, px, rgb,
};

use crate::theme;

pub const HEIGHT: Pixels = px(36.);
const BUTTON_WIDTH: Pixels = px(46.);
/// The caption-button icons of Windows 11, and of Windows 10 as a fallback.
const ICON_FONT: &str = "Segoe Fluent Icons";
const ICON_FONT_FALLBACK: &str = "Segoe MDL2 Assets";

/// Whether fuji draws the title bar on this system.
pub const CUSTOM: bool = cfg!(windows);

/// A stretch of the bar that drags the window (and maximizes on double-click).
pub fn drag_area() -> Div {
    div()
        .h(HEIGHT)
        .flex()
        .items_center()
        .window_control_area(WindowControlArea::Drag)
}

/// The app's mark, for the left end of the bar.
pub fn brand() -> Div {
    div().flex().items_center().child(
        img(ImageSource::Resource(Resource::Embedded(
            "icon-96.png".into(),
        )))
        .size(px(18.))
        .flex_none(),
    )
}

/// Minimize, maximize or restore, and close.
pub fn controls(window: &Window) -> Div {
    let maximize = if window.is_maximized() {
        "\u{e923}"
    } else {
        "\u{e922}"
    };
    div()
        .flex()
        .flex_none()
        .h(HEIGHT)
        .child(control(
            "window-minimize",
            "\u{e921}",
            WindowControlArea::Min,
            theme::hover(),
            theme::text(),
        ))
        .child(control(
            "window-maximize",
            maximize,
            WindowControlArea::Max,
            theme::hover(),
            theme::text(),
        ))
        .child(control(
            "window-close",
            "\u{e8bb}",
            WindowControlArea::Close,
            // The red every Windows close button turns.
            rgb(0xc42b1c).into(),
            gpui::white(),
        ))
}

fn control(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    hover_fill: Hsla,
    hover_ink: Hsla,
) -> Stateful<Div> {
    div()
        .id(id)
        // Above the drag area: the pointer here is on a button, not the caption.
        .occlude()
        .w(BUTTON_WIDTH)
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .font_family(ICON_FONT)
        .font_fallbacks_named(ICON_FONT_FALLBACK)
        .text_size(px(10.))
        .text_color(theme::text_muted())
        .hover(move |el| el.bg(hover_fill).text_color(hover_ink))
        .window_control_area(area)
        .child(glyph)
}

/// The fallback-font call above, on anything styled.
trait FontFallback: Styled + Sized {
    fn font_fallbacks_named(mut self, family: &'static str) -> Self {
        self.text_style()
            .get_or_insert_with(Default::default)
            .font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![family.to_string()]));
        self
    }
}

impl<T: Styled> FontFallback for T {}
