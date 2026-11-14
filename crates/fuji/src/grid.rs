//! The photo grid: a virtualized list of rows, so only visible tiles are built.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use fuji_icloud::{Asset, AssetKind, Library};
use gpui::{
    Animation, AnimationExt as _, Context, ElementId, FontWeight, Window, div, ease_out_quint,
    prelude::*, px, uniform_list,
};

use crate::theme;

pub struct PhotoGrid {
    library: Arc<dyn Library>,
}

impl PhotoGrid {
    pub fn new(library: Arc<dyn Library>) -> Self {
        Self { library }
    }
}

/// How many tiles fit across `width` pixels.
fn columns_for(width: f32) -> usize {
    let usable = (width - theme::GRID_PADDING * 2.).max(theme::TILE_SIZE);
    ((usable + theme::TILE_GAP) / (theme::TILE_SIZE + theme::TILE_GAP)).floor() as usize
}

impl Render for PhotoGrid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grid_width = f32::from(window.viewport_size().width) - f32::from(theme::SIDEBAR_WIDTH);
        let columns = columns_for(grid_width).max(1);
        let total = self.library.len();
        let rows = total.div_ceil(columns);

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(theme::GRID_PADDING))
                    .pt_4()
                    .pb_2()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Library"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text_muted())
                            .child(format!("{total} items")),
                    ),
            )
            .child(
                uniform_list(
                    "photo-grid",
                    rows,
                    cx.processor(move |this, range: Range<usize>, _window, _cx| {
                        range
                            .map(|row| {
                                let start = row * columns;
                                let end = (start + columns).min(this.library.len());
                                div()
                                    .flex()
                                    .gap(px(theme::TILE_GAP))
                                    .px(px(theme::GRID_PADDING))
                                    .pb(px(theme::TILE_GAP))
                                    .children((start..end).filter_map(|ix| {
                                        this.library.asset(ix).map(|asset| tile(ix, &asset))
                                    }))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1(),
            )
    }
}

fn tile(index: usize, asset: &Asset) -> impl IntoElement + use<> {
    let badge = match asset.kind {
        AssetKind::Photo => None,
        AssetKind::LivePhoto => Some("LIVE".to_string()),
        AssetKind::Video { duration_secs } => {
            Some(format!("{}:{:02}", duration_secs / 60, duration_secs % 60))
        }
    };

    div()
        .id(ElementId::NamedInteger("tile".into(), index as u64))
        .size(px(theme::TILE_SIZE))
        .flex_none()
        .relative()
        .rounded_sm()
        .overflow_hidden()
        .bg(theme::placeholder(index))
        .cursor_pointer()
        .hover(|el| el.opacity(0.85))
        .when_some(badge, |el, badge| {
            el.child(
                div()
                    .absolute()
                    .bottom_1()
                    .right_1()
                    .px_1()
                    .rounded_sm()
                    .bg(gpui::black().opacity(0.45))
                    .text_xs()
                    .child(badge),
            )
        })
        .when(asset.favorite, |el| {
            el.child(
                div()
                    .absolute()
                    .bottom_1()
                    .left_1()
                    .text_xs()
                    .text_color(theme::favorite())
                    .child("\u{2665}"),
            )
        })
        // Fade tiles in as they scroll into view.
        .with_animation(
            ElementId::NamedInteger("tile-fade".into(), index as u64),
            Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
            |el, delta| el.opacity(delta),
        )
}

#[cfg(test)]
mod tests {
    use super::columns_for;

    #[test]
    fn at_least_one_column() {
        assert_eq!(columns_for(10.), 1);
    }

    #[test]
    fn columns_grow_with_width() {
        assert!(columns_for(1400.) > columns_for(700.));
    }
}
