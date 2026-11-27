//! The Collections page: memories, albums, shared albums, media types and
//! utilities,
//! as cards with a cover photo, after Photos' Collections tab.
//!
//! The app owns the data (what's there, counts, covers) and loads it; this
//! module only draws it.

use std::path::Path;
use std::sync::Arc;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Div, ElementId, FontWeight,
    ObjectFit, SharedString, Stateful, Window, div, img, prelude::*, pulsating_between, px,
};

use crate::assets::icon;
use crate::{anim, theme};

/// Size of a card's cover.
const COVER: f32 = 168.;
/// Space between cards.
const GAP: f32 = 18.;
/// A card's width and the space between cards, for counting what fits.
pub const CARD_SPACING: (f32, f32) = (COVER, GAP);

/// One thing on the page.
pub struct Card {
    pub key: u64,
    pub title: SharedString,
    /// Items in it, once counted.
    pub count: Option<usize>,
    /// Said under the title instead of the count (a memory's date).
    pub subtitle: Option<SharedString>,
    pub cover: Option<Arc<Path>>,
    /// Shown on the cover when there's no photo, and as a badge for media
    /// types.
    pub icon: &'static str,
    pub badge: bool,
    /// The cover photo is on its way.
    pub loading: bool,
}

/// Something gently pulsing: it's loading. Still when motion is reduced.
fn pulse(id: ElementId, el: Div) -> AnyElement {
    if anim::reduced() {
        return el.into_any_element();
    }
    el.with_animation(
        id,
        Animation::new(std::time::Duration::from_millis(1100))
            .repeat()
            .with_easing(pulsating_between(0.35, 1.)),
        |el, t| el.opacity(t),
    )
    .into_any_element()
}

/// A stand-in for a card that's still loading: a pulsing cover and lines
/// for its title and date.
pub fn skeleton_card(key: u64) -> Stateful<Div> {
    let bar = |width: f32, height: f32| {
        div()
            .w(px(width))
            .h(px(height))
            .rounded(px(4.))
            .bg(theme::placeholder())
    };
    div()
        .id(ElementId::NamedInteger("collection-skeleton".into(), key))
        .w(px(COVER))
        .child(pulse(
            ElementId::NamedInteger("skeleton-pulse".into(), key),
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .size(px(COVER))
                        .rounded(px(12.))
                        .bg(theme::placeholder()),
                )
                .child(div().mt(px(4.)).child(bar(COVER * 0.6, 11.)))
                .child(bar(COVER * 0.4, 10.)),
        ))
}

/// A titled run of cards that wraps onto new rows.
pub fn section(title: &'static str, cards: Vec<Stateful<Div>>) -> Div {
    section_with(title, None, cards)
}

/// The same, with something at the right of the title ("Show All").
pub fn section_with(
    title: &'static str,
    action: Option<AnyElement>,
    cards: Vec<Stateful<Div>>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(FontWeight::BLACK)
                        .text_color(theme::text())
                        .child(title),
                )
                // Centred, then down to the heading's baseline: the smaller
                // text sits higher in its line.
                .children(action.map(|a| div().relative().top(px(2.)).child(a))),
        )
        .child(div().flex().flex_wrap().gap(px(GAP)).children(cards))
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
            // Rounded itself: the clip doesn't round an image's corners.
            Some(path) => el.child(
                img(path)
                    .size_full()
                    .rounded(px(12.))
                    .object_fit(ObjectFit::Cover),
            ),
            None if card.loading => el.child(pulse(
                ElementId::NamedInteger("cover-pulse".into(), card.key),
                div()
                    .size_full()
                    .rounded(px(12.))
                    .bg(theme::text_meta().opacity(0.12)),
            )),
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
                .rounded(px(12.))
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
                .truncate()
                .map(|el| match card.subtitle {
                    Some(subtitle) => el.child(subtitle),
                    None => el.children(card.count.map(group_digits)),
                }),
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
