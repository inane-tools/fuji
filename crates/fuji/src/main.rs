//! fuji: an iCloud Photos client for Windows and Linux.

// Release builds are a window app on Windows (no console beside it); panics
// still reach crash.log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod app;
mod assets;
mod collections;
mod crash;
mod device_search;
mod export;
mod favorite;
mod grid;
mod image_cache;
mod index;
mod input;
mod layout;
mod login;
mod net;
mod passkey;
mod platform;
mod release;
mod scrubber;
mod search;
mod session_store;
mod settings;
mod theme;
mod thumbs;
mod titlebar;
mod transfer;
mod ui;
mod updater;
mod video;
mod viewer;
mod zoom;

use gpui::{
    App, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size,
};

fn main() {
    crash::install();
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(assets::FONT_BYTES)])
                .expect("load the UI font");
            settings::Settings::init(cx);
            updater::init(settings::Settings::get(cx).auto_update, cx);
            let saved = session_store::load();
            input::bind_keys(cx);
            grid::bind_keys(cx);
            viewer::bind_keys(cx);
            app::bind_keys(cx);

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Fuji".into()),
                        // fuji draws its own title bar where it can.
                        appears_transparent: titlebar::CUSTOM,
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(640.), px(420.))),
                    // Matches the desktop entry on Linux (its icon and name).
                    app_id: Some("tools.inane.Fuji".into()),
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
