mod assets;
mod config;
mod errors;
mod services;
mod state;
mod ui;

use std::borrow::Cow;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Menu, MenuItem, WindowBounds, actions,
    point, px, size,
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

/// Size of the window on a machine that has never opened one.
const DEFAULT_WINDOW: gpui::Size<gpui::Pixels> = gpui::size(px(1100.), px(720.));

/// Where to open the window: where it was left, or centred at the default size.
///
/// A saved rect is only honoured while it still lands on an attached display.
/// The failure that guards against is a monitor unplugged between sessions:
/// the rect then names coordinates on no screen at all and the window opens
/// invisible, with no way to reach it short of deleting the settings file.
///
/// Wayland compositors place windows themselves and do not tell a client where
/// it is, so on Wayland this restores the *size* and the maximized state and
/// the origin is the compositor's to pick. Nothing here depends on the origin
/// being honoured.
fn startup_bounds(settings: &config::Settings, cx: &App) -> WindowBounds {
    let displays: Vec<(f32, f32, f32, f32)> = cx
        .displays()
        .iter()
        .map(|d| {
            let b = d.bounds();
            (
                f32::from(b.origin.x),
                f32::from(b.origin.y),
                f32::from(b.size.width),
                f32::from(b.size.height),
            )
        })
        .collect();
    match settings.window.filter(|w| w.usable_on(&displays)) {
        Some(w) => {
            let rect = Bounds {
                origin: point(px(w.x), px(w.y)),
                size: size(px(w.width), px(w.height)),
            };
            if w.maximized {
                WindowBounds::Maximized(rect)
            } else {
                WindowBounds::Windowed(rect)
            }
        }
        None => WindowBounds::Windowed(Bounds::centered(None, DEFAULT_WINDOW, cx)),
    }
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
            // One-off: square art cached before covers were cropped on the way
            // in. Whole-file decodes, so it goes to the blocking pool and is
            // left to run — the views paint from the same cache meanwhile and
            // pick the cropped files up as they are asked for again.
            cx.background_spawn(services::runtime::spawn_blocking_io(|| {
                services::artwork::squarify_cached_art();
                Ok(())
            }))
            .detach();
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

            // Before the window exists, so the very first frame is laid out at
            // the persisted scale rather than at 100% and then corrected.
            ui::init_ui_scale(settings.ui_scale);

            cx.open_window(
                ui::window_options(settings.client_titlebar, startup_bounds(&settings, cx)),
                |window, cx| {
                    ui::apply_theme(settings.theme, settings.font_size, window, cx);
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
