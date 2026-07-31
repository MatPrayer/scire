pub mod album_detail;
pub mod albums;
pub mod artists;
pub mod favorites;
pub mod fullscreen_player;
pub mod local_album_detail;
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

use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use directories::ProjectDirs;
use gpui::{
    Animation, AnimationElement, AnimationExt as _, App, BoxShadow, ElementId, Hsla, IntoElement,
    SharedString, Styled, Window, WindowBounds, WindowDecorations, WindowOptions, div,
    ease_out_quint, hsla, point, prelude::*, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::TitleBar;
use gpui_component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, ThemeRegistry};

use crate::config::{ImportedThemeDefinition, ImportedThemesFile, ThemePref};

fn settings_theme_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "scire")?;
    Some(dirs.config_dir().join("theme.json"))
}

/// Await `work` while calling `tick` every `interval`.
///
/// The long-running library jobs report progress by writing into atomics — they
/// run on the IO runtime and have no handle on any view — so something has to
/// sample them and repaint. `work` goes to gpui's background executor and sets a
/// flag when it lands, rather than being selected over, which keeps this to the
/// crates already in the tree. The ticker is gpui's timer and not `tokio::time`:
/// gpui tasks have no reactor in scope and `sleep` panics there.
pub async fn poll_until_done<T: Send + 'static>(
    cx: &mut gpui::AsyncApp,
    interval: Duration,
    work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    mut tick: impl FnMut(&mut gpui::AsyncApp),
) -> anyhow::Result<T> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let done = std::sync::Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let task = cx.background_spawn(async move {
        let result = work.await;
        flag.store(true, Ordering::SeqCst);
        result
    });
    while !done.load(Ordering::SeqCst) {
        cx.background_executor().timer(interval).await;
        tick(cx);
    }
    task.await
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

/// Cut `text` down to at most `max_chars`, backing up to the last word
/// boundary, with a trailing ellipsis.
pub fn truncate_at_word(text: &str, max_chars: usize) -> String {
    let byte_cut = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = &text[..byte_cut];
    let cut = head.rfind(char::is_whitespace).unwrap_or(byte_cut);
    format!("{} …", head[..cut].trim_end())
}

/// Strip HTML tags and decode the handful of entities Last.fm text uses
/// (Navidrome forwards agent bios and album notes verbatim, tags included).
pub fn strip_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
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

/// Scrolling speed and end pauses for [`scrolling_line`].
const MARQUEE_SPEED: f32 = 34.;
const MARQUEE_HOLD: f32 = 2.2;
/// Empty space left past the end of the text at the far end of the travel, so
/// the last glyph does not sit flush against the clip edge.
const MARQUEE_TAIL: f32 = 16.;

/// One line of text that scrolls back and forth when it is wider than the
/// space it is given, and stands still when it fits.
///
/// The width has to be passed in: the text is measured against it here, before
/// layout, so there is nothing to ask. `id` must be unique per call site —
/// `with_animation` keys its state on the element-id path.
pub fn scrolling_line(
    id: &'static str,
    text: SharedString,
    width: gpui::Pixels,
    font_size: gpui::Pixels,
    weight: gpui::FontWeight,
    color: Option<Hsla>,
    window: &mut Window,
) -> gpui::AnyElement {
    use gpui::{Animation, AnimationExt as _, TextRun};

    // shape_line rejects newlines, and a title spanning lines is not something
    // this element could show anyway.
    let text: SharedString = if text.contains('\n') {
        text.replace('\n', " ").into()
    } else {
        text
    };
    let style = window.text_style();
    let mut font = style.font();
    font.weight = weight;
    let run = TextRun {
        len: text.len(),
        font,
        color: color.unwrap_or(style.color),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let text_width = window
        .text_system()
        .shape_line(text.clone(), font_size, &[run], None)
        .width;

    let styled = move |el: gpui::Div| {
        el.text_size(font_size)
            .font_weight(weight)
            .map(|el| match color {
                Some(c) => el.text_color(c),
                None => el,
            })
    };

    if text_width <= width {
        // Truncate anyway: the measurement is of the whole string, so anything
        // that reaches here fits, but a stale width would otherwise spill.
        return styled(div()).truncate().child(text).into_any_element();
    }

    let travel = f32::from(text_width - width) + MARQUEE_TAIL;
    let scroll = travel / MARQUEE_SPEED;
    let total = 2. * scroll + 2. * MARQUEE_HOLD;
    // Phase boundaries: hold at the start, scroll out, hold at the end, scroll
    // back. Going back rather than wrapping around keeps the start of the
    // title — the part that identifies it — on screen most of the time.
    let (f1, f2, f3) = (
        MARQUEE_HOLD / total,
        (MARQUEE_HOLD + scroll) / total,
        (2. * MARQUEE_HOLD + scroll) / total,
    );

    div()
        .w(width)
        .overflow_hidden()
        .child(
            styled(div())
                .flex_none()
                .whitespace_nowrap()
                .relative()
                .child(text)
                .with_animation(
                    id,
                    Animation::new(Duration::from_secs_f32(total)).repeat(),
                    move |this, delta| {
                        let progress = if delta < f1 {
                            0.
                        } else if delta < f2 {
                            (delta - f1) / (f2 - f1)
                        } else if delta < f3 {
                            1.
                        } else {
                            1. - (delta - f3) / (1. - f3)
                        };
                        this.left(px(-travel * progress))
                    },
                ),
        )
        .into_any_element()
}

/// "On air" indicator for live radio: a breathing dot beside the label, with
/// the time spent listening when there is room for it.
///
/// `id` must differ per call site — `with_animation` keys its state on the
/// element-id path, so two badges sharing an id share a phase and, worse,
/// restart each other whenever one of them is rebuilt.
pub fn live_badge(
    id: &'static str,
    accent: Hsla,
    elapsed: Option<Duration>,
    cx: &App,
) -> gpui::AnyElement {
    use gpui::{Animation, AnimationExt as _, pulsating_between};
    use gpui_component::{StyledExt as _, h_flex};

    h_flex()
        .gap_2()
        .items_center()
        .flex_none()
        .child(
            div().size(px(8.)).rounded_full().bg(accent).with_animation(
                id,
                Animation::new(Duration::from_secs(2))
                    .repeat()
                    .with_easing(pulsating_between(0.25, 1.0)),
                |this, delta| this.opacity(delta),
            ),
        )
        .child(
            div()
                .text_xs()
                .font_medium()
                .text_color(accent)
                .child("LIVE"),
        )
        .when_some(elapsed, |this, elapsed| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format_duration(elapsed)),
            )
        })
        .into_any_element()
}

/// "MP3 · 128 kbps · Jazz" for the station now playing, or None when it did
/// not say (and when radio is not playing at all).
pub fn radio_info_line(
    player: &crate::state::player::PlayerState,
    settings: &crate::config::Settings,
) -> Option<String> {
    if !settings.stream_info_bar {
        return None;
    }
    player.radio_info_line()
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

/// Wraps a seek bar (slider or waveform) with a hover indicator: a marker line
/// under the cursor and a bubble with the time it would seek to, so clicks can
/// be aimed instead of guessed.
///
/// The hovered fraction lives in the caller's view state — gpui only re-renders
/// on entity updates — so this reports it back through `on_hover` and takes the
/// current value as `hovered`. Purely decorative overlay: no mouse handlers on
/// the marker, so clicks and drags still reach the bar underneath.
pub fn seek_hover_wrap(
    id: &'static str,
    hovered: Option<f32>,
    total: Option<Duration>,
    bar: gpui::AnyElement,
    on_hover: impl Fn(Option<f32>, &mut App) + 'static,
    cx: &App,
) -> gpui::AnyElement {
    use gpui::{MouseMoveEvent, canvas, relative};
    use std::cell::Cell;

    // Bounds captured at paint time: mouse-move events carry a window position
    // and no element bounds, so the mapping back to a fraction needs them.
    let bounds: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>> = Rc::new(Cell::new(None));
    let for_paint = bounds.clone();
    let for_move = bounds.clone();
    let on_hover = Rc::new(on_hover);
    let on_move = on_hover.clone();
    let on_leave = on_hover.clone();
    // Foreground, not the accent: the accent is also the played-region colour,
    // so an accent marker disappears on the left half of the bar.
    let marker = cx.theme().foreground;

    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .flex_1()
        .child(
            canvas(move |b, _, _| for_paint.set(Some(b)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .child(bar)
        .on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
            let next = for_move.get().and_then(|b| {
                let w = f32::from(b.size.width);
                if w <= 0. || !b.contains(&event.position) {
                    return None;
                }
                Some(((f32::from(event.position.x) - f32::from(b.origin.x)) / w).clamp(0., 1.))
            });
            on_move(next, cx);
        })
        .on_hover(move |hovered: &bool, _, cx| {
            if !*hovered {
                on_leave(None, cx);
            }
        })
        .when_some(hovered, |this, fraction| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(fraction))
                    .w(px(2.))
                    .ml(-px(1.))
                    .rounded_full()
                    .bg(marker.opacity(0.85)),
            )
            .when_some(total, |this, total| {
                // Zero-width flex box: the label overflows it symmetrically,
                // which centres the bubble on the marker without a transform.
                this.child(
                    div()
                        .absolute()
                        .left(relative(fraction))
                        .top(px(-26.))
                        .w_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .flex_shrink_0()
                                .px_1p5()
                                .py_0p5()
                                .rounded_md()
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .text_xs()
                                .text_color(cx.theme().popover_foreground)
                                .child(format_duration(seek_position(total, fraction))),
                        ),
                )
            })
        })
        .into_any_element()
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

/// Derive a UI accent from cover-art bytes. Only a decode failure yields
/// `None`: every cover that renders must also recolour the UI, or the accent
/// silently keeps belonging to the *previous* album.
///
/// Three passes, first hit wins:
/// 1. vivid — pixels weighted by saturation², peaking at mid lightness;
/// 2. relaxed — any tint at all, no lightness penalty. Near-black and
///    near-white covers (a dark photo, a white sleeve with a faint logo) fail
///    the first pass entirely, and they are exactly the covers users noticed
///    the accent sticking on;
/// 3. neutral — a genuinely monochrome cover has no hue to find, so the accent
///    is built from its overall lightness instead: still a visible change, and
///    still legible against the dark base.
pub fn accent_from_cover_bytes(bytes: &[u8]) -> Option<Hsla> {
    let img = image::load_from_memory(bytes).ok()?.into_rgb8();
    let pixels: Vec<(f32, f32, f32)> = img
        .pixels()
        .map(|p| {
            rgb_to_hsl(
                p[0] as f32 / 255.0,
                p[1] as f32 / 255.0,
                p[2] as f32 / 255.0,
            )
        })
        .collect();
    if pixels.is_empty() {
        return None;
    }
    Some(
        dominant_hue(&pixels, 0.12, true)
            .or_else(|| dominant_hue(&pixels, 0.03, false))
            .unwrap_or_else(|| {
                let mean_l = pixels.iter().map(|(_, _, l)| *l).sum::<f32>() / pixels.len() as f32;
                neutral_accent(mean_l)
            }),
    )
}

/// Weighted circular mean of the hues of the pixels above `min_sat`, with S/L
/// pinned so the result reads cleanly on a dark background. `favour_mid`
/// discounts near-black/near-white pixels — worth doing when there is colour to
/// spare, worth skipping when there is barely any.
fn dominant_hue(pixels: &[(f32, f32, f32)], min_sat: f32, favour_mid: bool) -> Option<Hsla> {
    let (mut sin, mut cos, mut wsum, mut ssum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &(h, s, l) in pixels {
        if s < min_sat {
            continue;
        }
        let mut w = s * s;
        if favour_mid {
            w *= 1.0 - (2.0 * l - 1.0).powi(2);
        }
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
    // Faint tints get pushed up to a usable saturation: the point is a visible
    // accent, not a faithful sample of a nearly grey cover.
    let sat = (ssum / wsum * 1.2).clamp(0.5, 0.85);
    Some(Hsla {
        h: hue.rem_euclid(1.0),
        s: sat,
        l: 0.55,
        a: 1.0,
    })
}

/// Accent for a cover with no hue at all. Greys can't be tinted without
/// inventing a colour, so the lightness of the artwork drives the lightness of
/// the accent instead: bright sleeves get a near-white accent, black ones a
/// dim slate, and either is clearly different from the colour left over from
/// the last album.
fn neutral_accent(mean_l: f32) -> Hsla {
    Hsla {
        h: 0.0,
        s: 0.0,
        // Kept off both extremes: pure white loses the hover/active states,
        // pure black disappears into the surface behind it.
        l: (0.30 + mean_l * 0.55).clamp(0.30, 0.85),
        a: 1.0,
    }
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

/// Outer glow used by the vi-mode focus cursor and card hover highlight.
pub fn focus_glow(cx: &App) -> Vec<BoxShadow> {
    let c = cx.theme().primary;
    vec![BoxShadow {
        color: hsla(c.h, c.s, c.l, 0.45),
        offset: point(px(0.), px(0.)),
        blur_radius: px(18.),
        spread_radius: px(0.),
    }]
}

/// Entry animation for a focused list item: the glow grows in over ~180ms
/// each time the vi cursor lands on the item. The wrapper element id is
/// per-item, so it mounts when focused and unmounts when the cursor moves —
/// the animation replays on every jump.
pub fn with_focus_animation<E: IntoElement + Styled + 'static>(
    id: impl Into<SharedString>,
    el: E,
    cx: &App,
) -> AnimationElement<E> {
    let c = cx.theme().primary;
    el.with_animation(
        ElementId::Name(id.into()),
        Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
        move |el, t| {
            el.shadow(vec![BoxShadow {
                color: hsla(c.h, c.s, c.l, 0.45 * t),
                offset: point(px(0.), px(0.)),
                blur_radius: px(18. * t),
                spread_radius: px(0.),
            }])
        },
    )
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

#[cfg(test)]
mod tests {
    use super::{accent_from_cover_bytes, strip_html, truncate_at_word};

    /// A `w`×1 PNG of one solid colour, in the encoded form the cache holds.
    fn png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([r, g, b]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn vivid_cover_keeps_its_hue() {
        let accent = accent_from_cover_bytes(&png(220, 40, 40)).unwrap();
        // Red sits at either end of the hue circle.
        assert!(accent.h < 0.05 || accent.h > 0.95, "hue was {}", accent.h);
        assert!(accent.s >= 0.5);
    }

    #[test]
    fn near_black_cover_still_yields_an_accent() {
        // Fails the vivid pass (the mid-lightness weight is ~0) but has a tint.
        let accent = accent_from_cover_bytes(&png(14, 6, 22)).unwrap();
        assert!(accent.s >= 0.5, "expected a usable saturation");
    }

    #[test]
    fn monochrome_covers_track_their_lightness() {
        let black = accent_from_cover_bytes(&png(0, 0, 0)).unwrap();
        let white = accent_from_cover_bytes(&png(255, 255, 255)).unwrap();
        assert_eq!(black.s, 0.0);
        assert_eq!(white.s, 0.0);
        // The two must not land on the same accent, or switching between a
        // black and a white sleeve would leave the UI unchanged.
        assert!(white.l > black.l + 0.2, "{} vs {}", white.l, black.l);
        assert!(black.l >= 0.30 && white.l <= 0.85);
    }

    #[test]
    fn undecodable_bytes_have_no_accent() {
        assert!(accent_from_cover_bytes(b"not an image").is_none());
    }

    #[test]
    fn strip_html_drops_tags_and_entities() {
        assert_eq!(
            strip_html("<p>Rock &amp; roll <a href=\"x\">more</a></p>"),
            "Rock & roll more"
        );
    }

    #[test]
    fn truncate_at_word_backs_up_to_a_boundary() {
        assert_eq!(truncate_at_word("one two three", 9), "one two …");
    }
}
