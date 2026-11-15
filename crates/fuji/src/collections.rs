//! The Collections page: albums, shared albums, media types and utilities,
//! as cards with a cover photo, after Photos' Collections tab.
//!
//! The app owns the data (what's there, counts, covers) and loads it; this
//! module only draws it.

use std::path::Path;
use std::sync::Arc;

use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, ObjectFit, SharedString, Stateful, Window, div,
    img, prelude::*, px,
};

use crate::assets::icon;
use crate::theme;

/// Size of a card's cover.
const COVER: f32 = 168.;

/// One thing on the page.
pub struct Card {
    pub key: u64,
    pub title: SharedString,
    /// Items in it, once counted.
    pub count: Option<usize>,
    pub cover: Option<Arc<Path>>,
    /// Shown on the cover when there's no photo, and as a badge for media
    /// types.
    pub icon: &'static str,
    pub badge: bool,
}

/// A titled run of cards that wraps onto new rows.
pub fn section(title: &'static str, cards: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::BLACK)
                .text_color(theme::text())
                .child(title),
        )
        .child(div().flex().flex_wrap().gap(px(18.)).children(cards))
}

/// A cover photo with the name and count under it.
pub fn card(
    card: Card,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let cover = div()
        .relative()
        .size(px(COVER))
        .rounded(px(12.))
        .overflow_hidden()
        .bg(theme::placeholder())
        .flex()
        .items_center()
        .justify_center()
        .map(|el| match card.cover {
            Some(path) => el.child(img(path).size_full().object_fit(ObjectFit::Cover)),
            None => el.child(icon(card.icon, px(34.), theme::text_meta())),
        })
        .when(card.badge, |el| {
            // The media type's mark on its cover.
            el.child(
                div()
                    .absolute()
                    .left(px(8.))
                    .bottom(px(8.))
                    .size(px(26.))
                    .rounded_full()
                    .bg(theme::scrim(0.45))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(card.icon, px(14.), gpui::white())),
            )
        })
        // A light wash on hover.
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .group_hover("collection-card", |el| el.bg(gpui::white().opacity(0.08))),
        );
    div()
        .id(ElementId::NamedInteger("collection".into(), card.key))
        .group("collection-card")
        .w(px(COVER))
        .flex()
        .flex_col()
        .gap(px(2.))
        .cursor_pointer()
        .on_click(on_click)
        .child(cover)
        .child(
            div()
                .mt(px(6.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .truncate()
                .child(card.title),
        )
        .child(
            div()
                .h(px(16.))
                .text_size(px(12.))
                .text_color(theme::text_meta())
                .children(card.count.map(group_digits)),
        )
}

/// "12,345".
pub fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(12), "12");
    }
}
