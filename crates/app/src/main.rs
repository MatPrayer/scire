mod assets;
mod config;
mod services;
mod state;
mod ui;

use std::borrow::Cow;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Menu, MenuItem, WindowBounds, actions,
    px, size,
};
use gpui_component::Root;
use services::library_db::LibraryDb;

actions!(scire, [Quit, CloseWindow]);

/// Quit / close-window keys and the macOS menu bar. gpui binds nothing by
/// itself: without this, cmd-q and cmd-w are dead keys.
fn init_app_keys(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &CloseWindow, cx: &mut App| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
    ]);
    cx.set_menus(vec![Menu {
        name: "Scirè".into(),
        items: vec![
            MenuItem::action("Close Window", CloseWindow),
            MenuItem::separator(),
            MenuItem::action("Quit Scirè", Quit),
        ],
    }]);
    // Single-window app with no way to reopen one, so the last window closing
    // (cmd-w or the traffic light) has to end the process, not orphan it.
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn".into()),
        )
        .init();

    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            init_app_keys(cx);

            cx.text_system()
                .add_fonts(vec![
                    Cow::Borrowed(assets::NOTO_SANS),
                    Cow::Borrowed(assets::NOTO_SANS_JP),
                ])
                .expect("failed to load Noto fonts");

            let session = state::session::init(cx);
            let settings = session.read(cx).settings.clone();
            let player = state::player::init(&settings, cx);
            player.update(cx, |p, _| {
                p.set_transcoding(settings.transcoding.to_stream_options())
            });
            services::artwork::set_cache_cap_mb(settings.artwork_cache_mb);
            let playlists = state::playlists::init(session.clone(), cx);

            // Music library database — shared between local scanner, navidrome
            // sync, and future local-music views.
            let library_db = Arc::new(
                crate::config::library_db_path()
                    .ok()
                    .map(|p| {
                        LibraryDb::open(&p).unwrap_or_else(|e| {
                            tracing::warn!("failed to open library db: {e}; using in-memory");
                            LibraryDb::open_in_memory().unwrap()
                        })
                    })
                    .unwrap_or_else(|| LibraryDb::open_in_memory().unwrap()),
            );

            let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
            cx.open_window(
                ui::window_options(settings.client_titlebar, WindowBounds::Windowed(bounds)),
                |window, cx| {
                    ui::apply_theme(settings.theme, window, cx);
                    ui::apply_window_chrome(settings.client_titlebar, window, cx);
                    let root_view = cx.new(|cx| {
                        ui::root::RootView::new(session, player, playlists, library_db, window, cx)
                    });
                    cx.new(|cx| Root::new(root_view, window, cx))
                },
            )
            .expect("failed to open window");
            cx.activate(true);
        });
}
