pub mod album_detail;
pub mod albums;
pub mod artists;
pub mod favorites;
pub mod fullscreen_player;
pub mod login;
pub mod player_bar;
pub mod playlist_detail;
pub mod queue_panel;
pub mod radio;
pub mod recent;
pub mod root;
pub mod search_bar;
pub mod settings;
pub mod sidebar;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use directories::ProjectDirs;
use gpui::{App, SharedString, Window, WindowBounds, WindowDecorations, WindowOptions};
use gpui_component::TitleBar;
use gpui_component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};

use crate::config::{ImportedThemeDefinition, ImportedThemesFile, ThemePref};

fn settings_theme_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("com", "mirko", "navidrome-rusty-client")?;
    Some(dirs.config_dir().join("themes.json"))
}

/// Seek position for a `fraction` [0,1] of `total`, guarding against NaN /
/// non-finite / overflow inputs (which would panic `Duration::from_secs_f32`).
pub fn seek_position(total: Duration, fraction: f32) -> Duration {
    let total_secs = total.as_secs_f32();
    let secs = total_secs * fraction.clamp(0.0, 1.0);
    if secs.is_finite() && secs >= 0.0 {
        Duration::from_secs_f32(secs.min(total_secs))
    } else {
        Duration::ZERO
    }
}

/// mm:ss (or h:mm:ss) formatting for track times.
pub fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Extra track-row fields selected in settings, joined for display next to
/// the song title. `include_album` is false on album pages where the album
/// name is redundant.
pub fn track_extras(
    song: &subsonic::Song,
    prefs: &crate::config::TrackInfo,
    include_album: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if prefs.artist
        && let Some(a) = &song.artist
    {
        parts.push(a.clone());
    }
    if prefs.album
        && include_album
        && let Some(a) = &song.album
    {
        parts.push(a.clone());
    }
    if prefs.year
        && let Some(y) = song.year
    {
        parts.push(y.to_string());
    }
    if prefs.genre
        && let Some(g) = &song.genre
    {
        parts.push(g.clone());
    }
    if prefs.bitrate
        && let Some(b) = song.bit_rate
    {
        parts.push(format!("{b} kbps"));
    }
    if prefs.plays
        && let Some(p) = song.play_count
    {
        parts.push(format!("{p} plays"));
    }
    parts.join(" · ")
}

/// "FLAC · 1017 kbps · 44.1 kHz · 16-bit · stereo" line for the current
/// track, or None when disabled in settings / radio / no track.
pub fn stream_info_line(
    player: &crate::state::player::PlayerState,
    settings: &crate::config::Settings,
) -> Option<String> {
    if !settings.stream_info_bar || player.is_radio() {
        return None;
    }
    let song = player.current_song()?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(suffix) = song.suffix.as_deref().map(str::trim)
        && !suffix.is_empty()
    {
        parts.push(suffix.to_uppercase());
    }
    if let Some(kbps) = song.bit_rate.filter(|&b| b > 0) {
        parts.push(format!("{kbps} kbps"));
    }
    if let Some(hz) = song.sampling_rate.filter(|&r| r > 0) {
        let khz = hz as f32 / 1000.;
        if khz.fract() == 0. {
            parts.push(format!("{khz:.0} kHz"));
        } else {
            parts.push(format!("{khz:.1} kHz"));
        }
    }
    if let Some(bits) = song.bit_depth.filter(|&b| b > 0) {
        parts.push(format!("{bits}-bit"));
    }
    match song.channel_count {
        Some(1) => parts.push("mono".into()),
        Some(2) => parts.push("stereo".into()),
        Some(n) if n > 2 => parts.push(format!("{n} ch")),
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Continuous filled waveform seek bar: a symmetric amplitude envelope built
/// as a single polygon per region (played / remaining) painted on a canvas.
/// Click seeks the player to the fraction under the cursor.
pub fn waveform_seek_bar(
    peaks: &[f32],
    fraction: f32,
    height: f32,
    played_color: gpui::Hsla,
    rest_color: gpui::Hsla,
    player: gpui::Entity<crate::state::player::PlayerState>,
) -> gpui::AnyElement {
    use gpui::{MouseButton, div, prelude::*, px};
    use std::cell::Cell;
    use std::rc::Rc;

    let peaks: Rc<Vec<f32>> = Rc::new(peaks.to_vec());
    // Canvas bounds captured at paint time so the click handler can map the
    // mouse x back to a seek fraction.
    let bounds_cell: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>> = Rc::new(Cell::new(None));
    let bounds_for_paint = bounds_cell.clone();
    let bounds_for_click = bounds_cell.clone();

    // Build the envelope polygon for buckets [from, to): across the top edge,
    // then back along the mirrored bottom edge.
    fn envelope(
        peaks: &[f32],
        from: usize,
        to: usize,
        bounds: gpui::Bounds<gpui::Pixels>,
    ) -> Option<gpui::Path<gpui::Pixels>> {
        use gpui::px;
        if to <= from {
            return None;
        }
        let n = peaks.len().max(1) as f32;
        let w = f32::from(bounds.size.width);
        let h = f32::from(bounds.size.height);
        let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let cy = y0 + h / 2.;
        let x_at = |i: usize| x0 + w * i as f32 / n;
        let half = |p: f32| (p * h / 2.).max(0.75);

        let mut pb = gpui::PathBuilder::fill();
        pb.move_to(gpui::point(px(x_at(from)), px(cy - half(peaks[from]))));
        let buckets = || peaks.iter().enumerate().take(to).skip(from);
        for (i, &peak) in buckets() {
            // Two points per bucket keep the envelope step-accurate without
            // lyon having to interpolate long diagonals.
            let y = cy - half(peak);
            pb.line_to(gpui::point(px(x_at(i)), px(y)));
            pb.line_to(gpui::point(px(x_at(i + 1)), px(y)));
        }
        for (i, &peak) in buckets().rev() {
            let y = cy + half(peak);
            pb.line_to(gpui::point(px(x_at(i + 1)), px(y)));
            pb.line_to(gpui::point(px(x_at(i)), px(y)));
        }
        pb.build().ok()
    }

    div()
        .flex_1()
        .h(px(height))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event: &gpui::MouseDownEvent, _, cx| {
            let Some(bounds) = bounds_for_click.get() else {
                return;
            };
            let w = f32::from(bounds.size.width);
            if w <= 0. {
                return;
            }
            let x = f32::from(event.position.x) - f32::from(bounds.origin.x);
            let target = (x / w).clamp(0., 1.);
            player.update(cx, |player, _| {
                if let Some(total) = player.duration {
                    player.seek(seek_position(total, target));
                }
            });
        })
        .child(
            gpui::canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    bounds_for_paint.set(Some(bounds));
                    let n = peaks.len();
                    if n == 0 {
                        return;
                    }
                    let split = ((fraction.clamp(0., 1.) * n as f32).round() as usize).min(n);
                    if let Some(path) = envelope(&peaks, split, n, bounds) {
                        window.paint_path(path, rest_color);
                    }
                    if let Some(path) = envelope(&peaks, 0, split, bounds) {
                        window.paint_path(path, played_color);
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}

/// Apply the theme preference. `System` follows the OS appearance.
pub fn apply_theme(pref: ThemePref, window: &mut Window, cx: &mut App) {
    let mode = match pref {
        ThemePref::Light => ThemeMode::Light,
        ThemePref::Dark => ThemeMode::Dark,
        ThemePref::System | ThemePref::Custom => ThemeMode::from(window.appearance()),
    };
    Theme::change(mode, Some(window), cx);
    let family = SharedString::from(
        "Noto Sans CJK JP, Noto Sans CJK SC, Noto Sans CJK KR, sans-serif",
    );
    let theme = Theme::global_mut(cx);
    theme.font_family = family.clone();
    Rc::make_mut(&mut theme.light_theme).font_family = Some(family.clone());
    Rc::make_mut(&mut theme.dark_theme).font_family = Some(family);
    if matches!(pref, ThemePref::Custom) {
        apply_custom_theme_from_settings(cx);
    }
}

pub fn apply_custom_theme_from_settings(cx: &mut App) {
    let path = settings_theme_path();
    let Some(path) = path else {
        return;
    };
    let Ok(file) = ImportedThemesFile::load_from_path(&path) else {
        return;
    };
    let Some(theme) = file.themes.first() else {
        return;
    };
    let mode = match theme.mode.to_ascii_lowercase().as_str() {
        "dark" => ThemeMode::Dark,
        _ => ThemeMode::Light,
    };
    let config = ThemeConfig {
        name: SharedString::from(theme.name.clone()),
        mode,
        colors: imported_theme_colors(theme),
        ..Default::default()
    };
    Theme::global_mut(cx).apply_config(&Rc::new(config));
}

fn imported_theme_colors(theme: &ImportedThemeDefinition) -> ThemeConfigColors {
    let mut colors = ThemeConfigColors::default();
    let set = |value: &Option<String>| {
        value
            .as_ref()
            .map(|value| SharedString::from(value.clone()))
    };
    colors.background = set(&theme.background);
    colors.foreground = set(&theme.foreground);
    colors.border = set(&theme.border);
    colors.muted = set(&theme.muted);
    colors.muted_foreground = set(&theme.muted_foreground);
    colors.primary = set(&theme.primary);
    colors.primary_foreground = set(&theme.primary_foreground);
    colors.secondary = set(&theme.secondary);
    colors.secondary_foreground = set(&theme.secondary_foreground);
    colors.accent = set(&theme.accent);
    colors.accent_foreground = set(&theme.accent_foreground);
    colors.sidebar = set(&theme.sidebar);
    colors.sidebar_foreground = set(&theme.sidebar_foreground);
    colors.success = set(&theme.success);
    colors.success_foreground = set(&theme.success_foreground);
    colors.warning = set(&theme.warning);
    colors.warning_foreground = set(&theme.warning_foreground);
    colors.danger = set(&theme.danger);
    colors.danger_foreground = set(&theme.danger_foreground);
    colors.selection = set(&theme.selection);
    colors.scrollbar_thumb = set(&theme.scrollbar_thumb);
    colors.scrollbar_thumb_hover = set(&theme.scrollbar_thumb_hover);
    colors
}

/// Window open options derived from the client-titlebar preference.
pub fn window_options(client_titlebar: bool, bounds: WindowBounds) -> WindowOptions {
    if client_titlebar {
        #[cfg(target_os = "linux")]
        let decorations = Some(WindowDecorations::Client);
        #[cfg(not(target_os = "linux"))]
        let decorations = None;

        WindowOptions {
            window_bounds: Some(bounds),
            titlebar: Some(TitleBar::title_bar_options()),
            window_decorations: decorations,
            ..Default::default()
        }
    } else {
        WindowOptions {
            window_bounds: Some(bounds),
            titlebar: None,
            window_decorations: Some(WindowDecorations::Server),
            ..Default::default()
        }
    }
}

/// Apply native vs client window chrome at runtime (e.g. from Settings).
pub fn apply_window_chrome(client_titlebar: bool, window: &mut Window, _cx: &mut App) {
    if client_titlebar {
        #[cfg(target_os = "linux")]
        window.request_decorations(WindowDecorations::Client);
    } else {
        window.request_decorations(WindowDecorations::Server);
    }
    window.set_window_title("Scirè");
}
