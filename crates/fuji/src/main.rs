//! fuji: an iCloud Photos client for Windows and Linux.

mod anim;
mod app;
mod assets;
mod export;
mod grid;
mod image_cache;
mod index;
mod input;
mod layout;
mod login;
mod net;
mod platform;
mod scrubber;
mod session_store;
mod settings;
mod theme;
mod thumbs;
mod titlebar;
mod transfer;
mod ui;
mod viewer;
mod zoom;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

fn main() {
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(assets::FONT_BYTES)])
                .expect("load the UI font");
            settings::Settings::init(cx);
            let saved = session_store::load();
            input::bind_keys(cx);
            grid::bind_keys(cx);
            viewer::bind_keys(cx);

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("fuji".into()),
                        // fuji draws its own title bar where it can.
                        appears_transparent: titlebar::CUSTOM,
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
