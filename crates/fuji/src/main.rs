//! fuji: an iCloud Photos client for Windows and Linux.

mod app;
mod grid;
mod image_cache;
mod input;
mod login;
mod net;
mod session_store;
mod theme;
mod thumbs;
mod viewer;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        let saved = session_store::load();
        input::bind_keys(cx);
        viewer::bind_keys(cx);

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
                cx.new(|cx| app::FujiApp::new(saved, window, cx))
            },
        )
        .expect("failed to open the main window");
    });
}
