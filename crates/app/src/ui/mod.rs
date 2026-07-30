pub mod album_detail;
pub mod albums;
pub mod artists;
pub mod favorites;
pub mod fullscreen_player;
pub mod local_music;
pub mod player_bar;
pub mod playlist_detail;
pub mod queue_panel;
pub mod radio;
pub mod recent;
pub mod root;
pub mod search_bar;
pub mod settings;
pub mod sidebar;
pub mod visualizer;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use directories::ProjectDirs;
use gpui::{
    App, Hsla, SharedString, Window, WindowBounds, WindowDecorations, WindowOptions, div, hsla,
    prelude::*, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::TitleBar;
use gpui_component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, ThemeRegistry};

use crate::config::{ImportedThemeDefinition, ImportedThemesFile, ThemePref};

fn settings_theme_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "scire")?;
    Some(dirs.config_dir().join("theme.json"))
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
        .on_mouse_down(
            MouseButton::Left,
            move |event: &gpui::MouseDownEvent, _, cx| {
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
            },
        )
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
    // When switching away from Custom, reset stored theme configs to defaults.
    // apply_custom_theme_from_settings overwrites dark_theme/light_theme via
    // Theme::apply_config, causing Dark/Light/System to re-apply custom colors.
    if !matches!(pref, ThemePref::Custom) {
        let (light, dark) = {
            let reg = ThemeRegistry::global(cx);
            (
                reg.default_light_theme().clone(),
                reg.default_dark_theme().clone(),
            )
        };
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
    }
    let mode = match pref {
        ThemePref::Light => ThemeMode::Light,
        // Adaptive is a dark base; the cover-derived accent is layered on top
        // afterwards (by the root view, once a cover is known).
        ThemePref::Dark | ThemePref::Adaptive => ThemeMode::Dark,
        ThemePref::System | ThemePref::Custom => ThemeMode::from(window.appearance()),
    };
    // Theme::change resets every colour to the mode's defaults, wiping any
    // previously applied adaptive accent — the root re-applies it on the next
    // song / theme change.
    Theme::change(mode, Some(window), cx);
    let family = SharedString::from(
        "Noto Sans, Noto Sans JP, Noto Sans CJK SC, Noto Sans CJK KR, sans-serif",
    );
    let theme = Theme::global_mut(cx);
    theme.font_family = family.clone();
    Rc::make_mut(&mut theme.light_theme).font_family = Some(family.clone());
    Rc::make_mut(&mut theme.dark_theme).font_family = Some(family);
    if matches!(pref, ThemePref::Custom) {
        apply_custom_theme_from_settings(cx);
    }
}

/// The colour the bottom player bar is tinted with: a darkened, slightly
/// desaturated take on the cover-derived accent under the Adaptive theme, the
/// flat sidebar colour otherwise. The fullscreen overlay ends its gradient on
/// this same colour so the two surfaces read as one design.
pub fn player_tint(pref: ThemePref, cx: &App) -> Hsla {
    if pref == ThemePref::Adaptive {
        let accent = cx.theme().primary;
        Hsla {
            l: (accent.l * 0.4).clamp(0.0, 1.0),
            s: accent.s * 0.85,
            ..accent
        }
    } else {
        cx.theme().sidebar
    }
}

/// Recolour only the interactive accent surfaces — primary buttons, sliders,
/// progress/seek bar, focus ring, text selection — from a single cover-derived
/// hue. Backgrounds, text and muted surfaces are left untouched so the UI stays
/// minimal. Used by the Adaptive theme.
pub fn apply_adaptive_accent(cx: &mut App, accent: Hsla) {
    let fg = accent_foreground(accent);
    let theme = Theme::global_mut(cx);
    theme.primary = accent;
    theme.primary_hover = lighten(accent, 0.06);
    theme.primary_active = darken(accent, 0.06);
    theme.primary_foreground = fg;
    theme.slider_bar = accent;
    theme.slider_thumb = accent;
    theme.progress_bar = accent;
    theme.ring = accent;
    theme.selection = Hsla { a: 0.30, ..accent };
    cx.refresh_windows();
}

/// Derive a vivid UI accent hue from cover-art bytes. Each pixel is weighted by
/// how colourful it is (saturation², peaking at mid lightness); the weighted
/// circular-mean hue becomes the accent, with S/L pinned so it reads cleanly on
/// a dark background. Returns `None` for greyscale covers (no usable hue).
pub fn accent_from_cover_bytes(bytes: &[u8]) -> Option<Hsla> {
    let img = image::load_from_memory(bytes).ok()?.into_rgb8();
    let (mut sin, mut cos, mut wsum, mut ssum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for p in img.pixels() {
        let (h, s, l) = rgb_to_hsl(
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
        );
        if s < 0.12 {
            continue; // near-grey: no meaningful hue
        }
        // Favour saturated, mid-lightness pixels; discount near-black/near-white.
        let w = s * s * (1.0 - (2.0 * l - 1.0).powi(2));
        let ang = h * std::f32::consts::TAU;
        sin += w * ang.sin();
        cos += w * ang.cos();
        wsum += w;
        ssum += w * s;
    }
    if wsum < 1e-3 {
        return None;
    }
    let hue = sin.atan2(cos) / std::f32::consts::TAU;
    let sat = (ssum / wsum * 1.2).clamp(0.5, 0.85);
    Some(Hsla {
        h: hue.rem_euclid(1.0),
        s: sat,
        l: 0.55,
        a: 1.0,
    })
}

/// Pick black or white text for legibility on the given accent fill.
fn accent_foreground(accent: Hsla) -> Hsla {
    let rgb = gpui::Rgba::from(accent);
    let lum = 0.299 * rgb.r + 0.587 * rgb.g + 0.114 * rgb.b;
    if lum > 0.6 {
        Hsla {
            h: 0.,
            s: 0.,
            l: 0.10,
            a: 1.0,
        }
    } else {
        Hsla {
            h: 0.,
            s: 0.,
            l: 0.98,
            a: 1.0,
        }
    }
}

fn lighten(c: Hsla, amt: f32) -> Hsla {
    Hsla {
        l: (c.l + amt).min(1.0),
        ..c
    }
}

fn darken(c: Hsla, amt: f32) -> Hsla {
    Hsla {
        l: (c.l - amt).max(0.0),
        ..c
    }
}

/// Standard RGB→HSL (all channels 0..1); hue returned in turns (0..1).
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
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

/// 1px horizontal divider visible on any background.
pub fn divider() -> gpui::Div {
    div().h(px(1.)).w_full().bg(hsla(0., 0., 0.5, 0.15))
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
