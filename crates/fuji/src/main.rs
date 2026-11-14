//! fuji: an iCloud Photos client for Windows and Linux.

mod app;
mod grid;
mod input;
mod login;
mod session_store;
mod theme;

use std::sync::Arc;

use fuji_icloud::MockLibrary;
use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        // Placeholder data until the iCloud client exists.
        let library = Arc::new(MockLibrary::new(5_000));
        let saved = session_store::load();
        input::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("fuji".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.), px(420.))),
                ..Default::default()
            },
            |window, cx| {
                cx.activate(true);
                cx.new(|cx| app::FujiApp::new(library, saved, window, cx))
            },
        )
        .expect("failed to open the main window");
    });
}
