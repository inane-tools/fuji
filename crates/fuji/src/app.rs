//! The main window: sidebar plus content area.

use std::sync::Arc;

use fuji_icloud::Library;
use gpui::{Context, Entity, FontWeight, SharedString, Window, div, prelude::*};

use crate::grid::PhotoGrid;
use crate::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Library,
    Favorites,
    Videos,
    Albums,
    Shared,
    RecentlyDeleted,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::Library,
        Section::Favorites,
        Section::Videos,
        Section::Albums,
        Section::Shared,
        Section::RecentlyDeleted,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Library => "Library",
            Section::Favorites => "Favorites",
            Section::Videos => "Videos",
            Section::Albums => "Albums",
            Section::Shared => "Shared Albums",
            Section::RecentlyDeleted => "Recently Deleted",
        }
    }
}

pub struct FujiApp {
    section: Section,
    grid: Entity<PhotoGrid>,
}

impl FujiApp {
    pub fn new(library: Arc<dyn Library>, cx: &mut Context<Self>) -> Self {
        Self {
            section: Section::Library,
            grid: cx.new(|_| PhotoGrid::new(library)),
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(theme::SIDEBAR_WIDTH)
            .h_full()
            .p_3()
            .gap_1()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .px_2()
                    .pb_3()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("fuji"),
            )
            .children(Section::ALL.into_iter().map(|section| {
                let active = section == self.section;
                div()
                    .id(SharedString::from(section.label()))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(if active {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .when(active, |el| el.bg(theme::selected()))
                    .hover(|el| el.text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    }))
                    .child(section.label())
            }))
    }
}

impl Render for FujiApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.section {
            Section::Library => div().size_full().child(self.grid.clone()),
            other => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child(format!("{} is not built yet", other.label())),
        };

        div()
            .flex()
            .size_full()
            .bg(theme::background())
            .text_color(theme::text())
            .child(self.render_sidebar(cx))
            .child(div().flex_1().h_full().child(content))
    }
}
