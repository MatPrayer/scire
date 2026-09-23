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
use std::time::{Duration, Instant};

use directories::ProjectDirs;
use gpui::{
    Animation, AnimationElement, AnimationExt as _, AnyElement, App, BoxShadow, ElementId, Hsla,
    IntoElement, Pixels, ScrollAnchor, SharedString, StyleRefinement, Styled, Window, WindowBounds,
    WindowDecorations, WindowOptions, div, ease_out_quint, hsla, point, prelude::*, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::TitleBar;
use gpui_component::theme::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode, ThemeRegistry};

use crate::config::{ImportedThemeDefinition, ImportedThemesFile, ThemePref, UiFontSize, UiScale};
use crate::services::library_db::LibraryStats;

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

/// Amplitude for a volume slider sitting at `position` [0,1].
///
/// The engine's volume is an amplitude multiplier, and a fader that *is* the
/// amplitude does not feel linear: loudness goes roughly as amplitude^0.6, so a
/// straight fader spends its top half on changes that are barely audible and
/// crams everything audible into the bottom of its travel. Squaring the
/// position makes perceived loudness track the handle about linearly
/// (position^2 raised to 0.6 is position^1.2) while still reaching silence at
/// the bottom and unity at the top.
pub fn volume_amplitude(position: f32) -> f32 {
    let p = position.clamp(0., 1.);
    p * p
}

/// Inverse of `volume_amplitude`: where the handle sits for a given amplitude.
/// The stored/persisted volume is the amplitude, so this is what the sliders
/// resync from.
pub fn volume_position(amplitude: f32) -> f32 {
    amplitude.clamp(0., 1.).sqrt()
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

/// Thousands-separated integer, for the library header's counts.
pub fn format_count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Total playtime as days/hours, for the library header.
///
/// A whole library is days long, so the unit pair slides down with the size
/// rather than printing "0d 0h" for a handful of albums.
pub fn format_playtime(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (d, h, m) = (total / 86_400, (total % 86_400) / 3600, (total % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// The standard error line for a content view: the sentence, and a Retry
/// button when trying again could plausibly work.
///
/// Every page used to draw a bare line of red text, which for the two failures
/// a user actually hits — the server asleep, the network gone — is a dead end:
/// the only way back was to navigate away and return, and on a retained view
/// (the album and artist grids are kept across navigation) even that did not
/// re-request. The button is deliberately absent for permanent failures; one
/// that cannot work is worse than none, which is why `ErrorNote` carries the
/// distinction rather than the call site guessing at it.
pub fn error_banner(
    note: &crate::errors::ErrorNote,
    on_retry: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    use gpui_component::button::{Button, ButtonVariants as _};
    use gpui_component::{Sizable as _, h_flex};

    h_flex()
        .px_4()
        .py_1()
        .gap_2()
        .items_center()
        .flex_wrap()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(cx.theme().danger)
                .text_sm()
                .child(note.text.clone()),
        )
        .when(note.retryable, |this| {
            this.child(
                Button::new("error-retry")
                    .ghost()
                    .xsmall()
                    .icon(crate::assets::app_icon(crate::assets::icons::REFRESH))
                    .label("Retry")
                    .on_click(on_retry),
            )
        })
}

/// One-line library summary for a catalog page header: the count of whatever
/// that page lists, then the totals behind it.
///
/// `primary` is `(count, singular noun)` — the albums page counts albums, the
/// artists page artists — so the number next to the noun always matches what
/// the grid below is showing.
pub fn library_summary(primary: (i64, &str), stats: &LibraryStats) -> String {
    let noun = if primary.0 == 1 {
        primary.1.to_string()
    } else {
        format!("{}s", primary.1)
    };
    let tracks = if stats.tracks == 1 { "track" } else { "tracks" };
    format!(
        "{} {noun} · {} {tracks} · {}",
        format_count(primary.0),
        format_count(stats.tracks),
        format_playtime(stats.duration_secs),
    )
}

/// Gap between grid cards, and the chrome a card adds around its cover.
/// Shared by the album and artist grids so the two pages line up column for
/// column at every width.
///
/// `card_padding()` is the card's inset (6px a side at 100%) *plus* its 1px border:
/// taffy lays out border-box, so both come out of the width the card is given
/// and a card only `tile + 12` wide squeezes its own cover by 2px. The column
/// maths has to use the same number the card is built with, which is why the
/// cards take their width from this constant rather than repeating a literal.
const GRID_GAP_BASE: f32 = 16.;
/// Inset between a card's edge and its cover, per side, at 100% scale.
const CARD_INSET_BASE: f32 = 6.;
/// The card's border. **Not** scaled: a hairline is a hairline at every size,
/// and a 1.25px border lands on a fractional boundary and renders unevenly.
const CARD_BORDER: f32 = 1.;

/// The grid's gutter and a card's total horizontal chrome, at the current UI
/// scale.
///
/// Functions rather than constants because `Settings::ui_scale` multiplies
/// them: the column maths, the cards themselves and the art request all have to
/// agree on one number, and a constant left beside a scaled call site is
/// exactly the disagreement that leaves a row overflowing its grid. At 100%
/// they are the 16 and 14 the grids were built with, which is what the layout
/// tests below assert.
pub fn grid_gap() -> f32 {
    scaled(GRID_GAP_BASE)
}

/// A card's inset, per side — what a card puts between its edge and its cover.
pub fn card_inset() -> f32 {
    scaled(CARD_INSET_BASE)
}

/// Everything a card adds around its cover: its inset either side plus its
/// border either side. Taffy lays out border-box, so both come out of the width
/// the card is given.
pub fn card_padding() -> f32 {
    card_inset() * 2. + CARD_BORDER * 2.
}

/// The live [`UiScale`] factor, as `f32` bits.
///
/// A process global rather than something read off the `App`, because the
/// metrics it multiplies are consumed by *pure* layout helpers — `grid_fit`,
/// `player_bar::side_width`, `fullscreen_player::Layout::resolve`,
/// `album_side_panel` — which take a width and return a layout and have no
/// `App` to read a setting from. Threading a factor through every one of them
/// would put the same number in every signature in this module for a value
/// that changes about once in a session.
///
/// Relaxed ordering: a scale change that lands a frame late is a frame drawn at
/// the old size, and `set_ui_scale` refreshes the windows behind it anyway.
static UI_SCALE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(f32::to_bits(1.0));

/// Apply a scale and repaint. Like `apply_font_size`, the store on its own
/// dirties nothing — every view holds a layout computed from the old factor
/// until something happens to redraw it.
pub fn set_ui_scale(scale: UiScale, cx: &mut App) {
    UI_SCALE.store(
        scale.factor().to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
    cx.refresh_windows();
}

/// Set the scale without a repaint, for the one call before any window exists.
pub fn init_ui_scale(scale: UiScale) {
    UI_SCALE.store(
        scale.factor().to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn ui_scale() -> f32 {
    f32::from_bits(UI_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

/// A chrome length in logical pixels, at the current UI scale.
///
/// Every scaled metric in the app goes through this, and every one of them is
/// defined as its 100% value — so at the default scale this is the identity and
/// the layout is numerically what it has always been, which is what the layout
/// tests assert.
pub fn scaled(px: f32) -> f32 {
    px * ui_scale()
}

/// Edge of the cover drawn inside a grid card `card_padding()` wider than it.
///
/// `flush` is `Settings::flush_album_covers`: the cover takes the card's
/// padding and border for itself instead of sitting inside them. The **card**
/// keeps the width the column maths gave it either way — the cover grows into
/// the chrome rather than the card shrinking around a bigger cover — so
/// `grid_fit` needs to know nothing about the setting and toggling it cannot
/// change the column count or reflow the page. The requested art resolution is
/// unaffected for the same reason: it is keyed off the *setting's* maximum
/// tile, which a card's chrome is not part of, so nothing is re-fetched.
pub fn card_cover_edge(tile: f32, flush: bool) -> f32 {
    match flush {
        true => tile + card_padding(),
        false => tile,
    }
}

/// Horizontal padding the grids place *inside* their scrolling element
/// (`px_4` a side).
///
/// A tracked `ScrollHandle` reports the element's own bounds, padding
/// included, while the rows are laid out inside that padding. Feeding the raw
/// bounds to `grid_columns` therefore buys a column the row cannot fit: the
/// row overflows and, because it is centred, is clipped at *both* edges. It
/// only happens in the 32px band either side of a column boundary, so dragging
/// the window edge flickers in and out of it.
const GRID_PADDING_X_BASE: f32 = 32.;

pub fn grid_padding_x() -> f32 {
    scaled(GRID_PADDING_X_BASE)
}

/// Columns that fit in `width` for a cover `tile` px wide. Zero width means
/// nothing has been laid out yet — the caller's guess stands.
pub fn grid_columns(width: f32, tile: f32) -> Option<usize> {
    if width <= 0. {
        return None;
    }
    let card = tile + card_padding();
    Some((((width + grid_gap()) / (card + grid_gap())).floor() as usize).max(1))
}

/// Columns that fit inside a scrolling grid element `element_width` wide,
/// taking off the padding the rows are laid out within.
pub fn grid_columns_padded(element_width: f32, tile: f32) -> Option<usize> {
    grid_columns(element_width - grid_padding_x(), tile)
}

/// Columns *and* the tile size to draw them at, for a cover size given as a
/// range.
///
/// Columns are whole cards, so a fixed tile always leaves the division's
/// remainder as gutters — on a wide 16:9 window that is most of a card's width
/// of emptiness either side, while a narrower pane whose width happens to
/// divide evenly looks right. Taking the column count at `min_tile` and then
/// spending the leftover on the tiles themselves removes the remainder instead
/// of centring it: the grid fills the width until the covers hit `max_tile`,
/// past which the gutters come back rather than the art growing without limit.
pub fn grid_fit(width: f32, min_tile: f32, max_tile: f32) -> Option<(usize, f32)> {
    let cols = grid_columns(width, min_tile)?;
    // Inverse of `grid_columns`: the row is `cols` cards plus `cols - 1` gaps,
    // so each card gets `(width + grid_gap()) / cols` and the tile is what's left
    // of it once the gap and the card's own padding are taken off.
    let tile = ((width + grid_gap()) / cols as f32 - grid_gap() - card_padding())
        .clamp(min_tile, max_tile);
    Some((cols, tile))
}

/// [`grid_fit`] inside a scrolling grid element, minus the padding the rows are
/// laid out within.
pub fn grid_fit_padded(element_width: f32, min_tile: f32, max_tile: f32) -> Option<(usize, f32)> {
    grid_fit(element_width - grid_padding_x(), min_tile, max_tile)
}

/// Narrowest window the album page's side panel is offered on. Under this the
/// panel and a readable track list cannot both be had, whatever the ratio.
const SIDE_PANEL_MIN_WINDOW_W: f32 = 1100.;
/// How wide against its height the *window* has to be. "Landscape widescreen"
/// is the condition the layout was asked for, and a 4:3 or portrait window
/// keeps the stacked page even when it is wide enough in pixels.
const SIDE_PANEL_MIN_ASPECT: f32 = 1.3;
/// Room the track list keeps for itself: title, the hover actions, play count
/// and duration all sit on one row, and below this they start colliding.
const SIDE_PANEL_TRACKS_MIN: f32 = 520.;
/// Share of the content width the panel takes, and the range it is held to —
/// the cover is drawn at the panel's inner width, so this is really how big
/// the art gets.
const SIDE_PANEL_SHARE: f32 = 0.34;
const SIDE_PANEL_MIN_W: f32 = 340.;
const SIDE_PANEL_MAX_W: f32 = 560.;
/// Everything between the panel's edge and the cover's: the panel's own `p_4`
/// a side *and* the header card's `p_4` inside it. Counting only the panel's
/// ran the cover through the card's right padding and out of the window.
const SIDE_PANEL_PADDING: f32 = 64.;
/// Share of the window height the cover may take. The details sit under it and
/// the panel scrolls, but a cover taller than this pushes the play button off
/// the bottom of a short window, which is the one thing the panel is for.
const SIDE_PANEL_ART_SHARE: f32 = 0.52;
/// A cover smaller than this is not worth the panel — the stacked page draws a
/// bigger one.
const SIDE_PANEL_ART_MIN: f32 = 200.;
/// Top padding the track column takes in the side-panel layout, in place of its
/// `p_4`. Half of `SIDE_PANEL_PADDING`, and for the same reason: the cover
/// starts a header card's padding *inside* the panel's own, so a track list
/// padded like the panel begins above the cover it sits beside — which reads as
/// the two columns being out of step rather than as a list with room over it.
pub const SIDE_PANEL_TRACKS_TOP: f32 = SIDE_PANEL_PADDING / 2.;

/// The album page's side panel: its width, and the cover size inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlbumSidePanel {
    /// Panel width, padding included — the track list gets the rest.
    pub width: f32,
    /// Edge of the square cover drawn at the top of the panel.
    pub art: f32,
}

/// Size the album page's side panel, or `None` where the window should keep the
/// stacked page.
///
/// Pure so the fallbacks can be tested without a window: the layout is a
/// *preference*, and every way it can fail to fit (too narrow, too square, a
/// track list that would be squeezed, a cover too small to be worth the trade)
/// has to come back as the stacked page rather than as a bad panel.
///
/// `content_w` is the width the page itself has (the window minus the sidebar
/// and chrome), while the aspect test is on the whole window — folding the
/// sidebar away should not change what shape the window is.
pub fn album_side_panel(content_w: f32, window_w: f32, window_h: f32) -> Option<AlbumSidePanel> {
    // Nothing measured yet: the caller's stacked fallback stands, as it does
    // for every other first-frame layout here.
    if content_w <= 0. || window_h <= 0. {
        return None;
    }
    if window_w < SIDE_PANEL_MIN_WINDOW_W || window_w / window_h < SIDE_PANEL_MIN_ASPECT {
        return None;
    }
    // Floored to a whole pixel, and that is load-bearing rather than tidy. The
    // page measures its own width as the track column's bounds *plus* this
    // panel, and gpui rounds an element's bounds to whole pixels — so a
    // fractional panel made the sum `round(content - width) + width`, which is
    // not the content width. `LiveWidth` then relearned its chrome from a
    // measurement that moved every frame, the panel resized by a fraction of a
    // pixel, the rounding flipped the other way, and the two states alternated
    // for as long as the page was open: at 1180x700 the panel sat at 383.40 and
    // 383.66 on alternate frames, which is the cover and every line under it
    // visibly bobbing. An integral width makes the track column's bounds exact
    // and the sum a fixed point.
    let width = (content_w * SIDE_PANEL_SHARE)
        .clamp(SIDE_PANEL_MIN_W, SIDE_PANEL_MAX_W)
        .min(content_w - SIDE_PANEL_TRACKS_MIN)
        .floor();
    if width < SIDE_PANEL_MIN_W {
        return None;
    }
    let art = (width - SIDE_PANEL_PADDING)
        .min(window_h * SIDE_PANEL_ART_SHARE)
        .floor();
    (art >= SIDE_PANEL_ART_MIN).then_some(AlbumSidePanel { width, art })
}

/// Frame-accurate width for a layout that reflows with the window.
///
/// A `ScrollHandle`'s bounds are last frame's layout, so a grid whose column
/// count comes from them reflows one frame behind the window: drag the edge and
/// the cards visibly trail the cursor. `Window::viewport_size` is this frame's,
/// but it covers the whole window rather than the grid. The chrome around the
/// grid (sidebar, padding, an open panel) keeps its width while the window
/// changes, so the difference between the two is stable — remember it from the
/// measured frame and subtract it from the current viewport.
///
/// Chrome that *does* change width (a panel opening) is picked up on the next
/// frame, exactly as the measured width alone would have been.
#[derive(Default)]
pub struct LiveWidth {
    /// Viewport width at the previous call — the frame the measurement arriving
    /// now was laid out in.
    prev_viewport: f32,
    /// That viewport minus the measured element width.
    chrome: f32,
    /// The measurement `chrome` was learned from, so a second call inside one
    /// frame re-reads it instead of relearning against the wrong viewport.
    measured: f32,
}

impl LiveWidth {
    /// Feed the element's width as measured last frame, get its width for this
    /// one. Zero until the first measurement lands.
    pub fn resolve(&mut self, measured: f32, window: &Window) -> f32 {
        self.resolve_at(measured, f32::from(window.viewport_size().width))
    }

    /// [`Self::resolve`] against an explicit viewport, so the frame sequence a
    /// resize produces can be unit-tested without a window.
    ///
    /// Chrome is only relearned when the measurement actually moves. Callers
    /// ask more than once per frame (the vi-mode cursor needs the column count
    /// outside `render`), and relearning from an unchanged measurement would
    /// pair it with *this* frame's viewport rather than the one it was laid out
    /// in — during a resize drag that folds the frame's delta into the chrome
    /// and the grid falls a frame behind, which is the trailing this type
    /// exists to avoid.
    ///
    /// `prev_viewport` advances on **every** call, including the ones that
    /// don't relearn. Advancing it only alongside a relearn was a bug with a
    /// visible tail: the frame that shrinks the window carries the *old*
    /// measurement (bounds are a frame behind), so it takes the no-relearn path
    /// and leaves `prev_viewport` at the pre-resize width. The next frame's
    /// measurement — laid out at the new, smaller viewport — is then subtracted
    /// from that stale one, and the chrome comes out roughly a whole window too
    /// wide. `viewport - chrome` clamps to 0, `grid_columns` reads 0 as
    /// "nothing laid out yet", and the grid falls back to its first-frame guess
    /// of five columns inside a pane that fits one. The row overflows, and
    /// since it is centred the overflow splits: half of it disappears under the
    /// sidebar. Nothing repaints once a resize settles, so it stays there.
    fn resolve_at(&mut self, measured: f32, viewport: f32) -> f32 {
        if measured > 0. && self.prev_viewport > 0. && measured != self.measured {
            self.chrome = (self.prev_viewport - measured).max(0.);
            self.measured = measured;
        }
        self.prev_viewport = viewport;
        if measured <= 0. {
            return 0.;
        }
        (viewport - self.chrome).max(0.)
    }

    /// Columns and tile size for a card grid whose covers may be drawn anywhere
    /// in `min_tile..=max_tile`, laid out inside the measured element's
    /// `grid_padding_x()`. `fallback` stands in on the first frame, before
    /// anything has been laid out.
    ///
    /// The fallback is capped at what the *whole window* could hold: it is a
    /// guess made before the chrome is known, and a guess wider than the window
    /// itself overflows a centred row out past both edges — the one failure
    /// this type exists to prevent. Guessing too few only leaves a gap for the
    /// frame it takes the real measurement to arrive, and the fallback tile is
    /// the range's minimum for the same reason: too small is a gutter for one
    /// frame, too big is an overflowing row.
    pub fn grid(
        &mut self,
        measured: f32,
        min_tile: f32,
        max_tile: f32,
        window: &Window,
        fallback: usize,
    ) -> (usize, f32) {
        let viewport = f32::from(window.viewport_size().width);
        let width = self.resolve_at(measured, viewport);
        grid_fit_padded(width, min_tile, max_tile).unwrap_or_else(|| {
            let cols = grid_columns_padded(viewport, min_tile).unwrap_or(fallback);
            (fallback.min(cols), min_tile)
        })
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

/// Technical summary of an album's files: formats, bitrate range, sample rate
/// and bit depth, channels, and total size.
pub fn album_quality_chips(songs: &[subsonic::Song]) -> Vec<String> {
    let mut chips = Vec::new();
    let mut formats = Vec::new();
    for song in songs {
        let raw = song.suffix.as_deref().or_else(|| {
            song.content_type
                .as_deref()
                .and_then(|content_type| content_type.rsplit('/').next())
        });
        if let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) {
            let format = raw.to_uppercase();
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
    }
    if !formats.is_empty() {
        chips.push(formats.join(" / "));
    }

    let bitrates: Vec<u32> = songs
        .iter()
        .filter_map(|song| song.bit_rate)
        .filter(|bitrate| *bitrate > 0)
        .collect();
    if let (Some(&low), Some(&high)) = (bitrates.iter().min(), bitrates.iter().max()) {
        chips.push(if low == high {
            format!("{low} kbps")
        } else {
            format!("{low}–{high} kbps")
        });
    }

    let rate = songs
        .iter()
        .filter_map(|song| song.sampling_rate)
        .filter(|rate| *rate > 0)
        .max();
    let depth = songs
        .iter()
        .filter_map(|song| song.bit_depth)
        .filter(|depth| *depth > 0)
        .max();
    match (rate, depth) {
        (Some(rate), Some(depth)) => chips.push(format!("{} · {depth} bit", format_khz(rate))),
        (Some(rate), None) => chips.push(format_khz(rate)),
        (None, Some(depth)) => chips.push(format!("{depth} bit")),
        (None, None) => {}
    }

    if let Some(channels) = songs
        .iter()
        .filter_map(|song| song.channel_count)
        .filter(|channels| *channels > 0)
        .max()
    {
        chips.push(match channels {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            count => format!("{count} ch"),
        });
    }

    let total: u64 = songs.iter().filter_map(|song| song.size).sum();
    if total > 0 {
        chips.push(format_bytes(total));
    }
    chips
}

/// ReplayGain album value and track range, absent when no track has tags.
pub fn album_replaygain_line(songs: &[subsonic::Song]) -> Option<String> {
    let gains: Vec<&subsonic::ReplayGain> = songs
        .iter()
        .filter_map(|song| song.replay_gain.as_ref())
        .collect();
    if gains.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(album_gain) = gains.iter().find_map(|gain| gain.album_gain) {
        parts.push(format!("album {album_gain:+.2} dB"));
    }
    if let Some(peak) = gains
        .iter()
        .filter_map(|gain| gain.album_peak)
        .reduce(f32::max)
    {
        parts.push(format!("peak {peak:.2}"));
    }
    let track_gains: Vec<f32> = gains.iter().filter_map(|gain| gain.track_gain).collect();
    if let (Some(low), Some(high)) = (
        track_gains.iter().copied().reduce(f32::min),
        track_gains.iter().copied().reduce(f32::max),
    ) {
        parts.push(if (high - low).abs() < 0.005 {
            format!("tracks {low:+.2} dB")
        } else {
            format!("tracks {low:+.2} … {high:+.2} dB")
        });
    }
    (!parts.is_empty()).then(|| format!("ReplayGain: {}", parts.join(" · ")))
}

/// ISO date-time to date. Unexpected values pass through unchanged.
pub fn format_added_date(created: &str) -> String {
    created
        .split(['T', ' '])
        .next()
        .unwrap_or(created)
        .to_string()
}

fn format_khz(hz: u32) -> String {
    let khz = hz as f32 / 1000.0;
    if (khz - khz.round()).abs() < 0.05 {
        format!("{} kHz", khz.round() as u32)
    } else {
        format!("{khz:.1} kHz")
    }
}

fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
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

    let width = if std::env::var("SCIRE_MARQUEE").is_ok() {
        width * 0.4
    } else {
        width
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

/// Every artist a song or album is credited to, as `(name, id)` pairs.
///
/// OpenSubsonic's `artists` array is the only place a collaboration is spelled
/// out — `artist`/`artistId` collapse it to the one artist the server picked as
/// primary. Vanilla servers send no array and fall back to the single pair,
/// which yields exactly the one credit they always did.
pub fn artist_credits(
    artists: &[subsonic::ArtistRef],
    name: Option<&str>,
    id: Option<&str>,
) -> Vec<(String, Option<String>)> {
    if !artists.is_empty() {
        return artists
            .iter()
            .map(|a| (a.name.clone(), Some(a.id.clone())))
            .collect();
    }
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => vec![(name.to_string(), id.map(str::to_string))],
        None => Vec::new(),
    }
}

/// The subset of [`artist_credits`] that can actually be navigated to — a
/// credit with no id has no artist page, and offering it in a menu is offering
/// a row that does nothing.
pub fn artist_links(
    artists: &[subsonic::ArtistRef],
    name: Option<&str>,
    id: Option<&str>,
) -> Vec<(String, String)> {
    artist_credits(artists, name, id)
        .into_iter()
        .filter_map(|(n, id)| id.map(|id| (n, id)))
        .collect()
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
pub fn apply_theme(pref: ThemePref, font_size: UiFontSize, window: &mut Window, cx: &mut App) {
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
    apply_font_size(font_size, cx);
}

/// Apply persisted interface type scale and keep it across later theme changes.
///
/// Writing it into both mode themes as well as the live one is what survives a
/// later `Theme::change`, which rebuilds the whole theme from the mode's config
/// and would otherwise put the size back to 16px on the next theme pick.
pub fn apply_font_size(size: UiFontSize, cx: &mut App) {
    let value = size.px();
    let theme = Theme::global_mut(cx);
    theme.font_size = px(value);
    Rc::make_mut(&mut theme.light_theme).font_size = Some(value);
    Rc::make_mut(&mut theme.dark_theme).font_size = Some(value);
    // The theme is a global and mutating it dirties nothing, so notifying the
    // view that made the change repaints that view alone: the sidebar, player
    // bar and whatever page is open keep the old scale until some unrelated
    // event happens to redraw them. `Theme::change` takes a window for this
    // reason; the size can be set from a menu item holding only an `App`.
    cx.refresh_windows();
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

/// Button styling that matches the theme's primary button but on a colour of
/// the caller's choosing.
///
/// The theme is a single global, so a page that wants its own accent (the album
/// detail page under `Settings::adaptive_from_page`) cannot scope one — it
/// paints its own accent surfaces instead, and this keeps them looking like the
/// primary buttons everywhere else.
pub fn accent_button(accent: Hsla, cx: &App) -> gpui_component::button::ButtonCustomVariant {
    gpui_component::button::ButtonCustomVariant::new(cx)
        .color(accent)
        .foreground(accent_foreground(accent))
        .hover(lighten(accent, 0.06))
        .active(darken(accent, 0.06))
}

/// The wash a page tints its header with when it carries its own accent:
/// darkened and slightly desaturated, the same treatment `player_tint` gives
/// the bottom bar, so the two never read as competing designs.
pub fn page_tint(accent: Hsla) -> Hsla {
    Hsla {
        l: (accent.l * 0.4).clamp(0.0, 1.0),
        s: accent.s * 0.85,
        ..accent
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

/// Edge length every adaptive accent is extracted from.
///
/// The accent has to come out of one rendition of the cover everywhere, or the
/// chrome and the album page land on different colours for the same album: the
/// hue is a saturation-weighted circular mean over the pixels, and a
/// server-side resize moves those weights enough to tip a cover with two strong
/// hues from one to the other. The player took the 64 it fetches, the album
/// page took whatever rung happened to be cached (64 through 1500), so the two
/// only agreed by luck. Small on purpose: it is the cheapest rung, and the
/// playing album's page then re-uses the file the player already fetched.
pub const ACCENT_ART_SIZE: u32 = 64;

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

/// Weight of the hues of the pixels above `min_sat`, bucketed into 24 bins
/// (15° each) so a cover with two strong, separated hues (a split-tone
/// sleeve) doesn't get averaged into a colour present in neither of them —
/// a plain circular mean over both hues can land squarely between them,
/// which reads as flat-out wrong. The dominant bin is found first, then the
/// circular mean is taken only over pixels within one bin-width of it, which
/// keeps the mean's sub-bin precision for a cover with one real peak while
/// no longer blending two.
const HUE_BINS: usize = 24;

/// Weighted circular mean of the hues of the pixels above `min_sat`, with S/L
/// pinned so the result reads cleanly on a dark background. `favour_mid`
/// discounts near-black/near-white pixels — worth doing when there is colour to
/// spare, worth skipping when there is barely any.
fn dominant_hue(pixels: &[(f32, f32, f32)], min_sat: f32, favour_mid: bool) -> Option<Hsla> {
    let weight = |s: f32, l: f32| {
        let mut w = s * s;
        if favour_mid {
            w *= 1.0 - (2.0 * l - 1.0).powi(2);
        }
        w
    };

    let mut bins = [0.0f32; HUE_BINS];
    for &(h, s, l) in pixels {
        if s < min_sat {
            continue;
        }
        let bin = ((h.rem_euclid(1.0) * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        bins[bin] += weight(s, l);
    }
    let (peak_bin, &peak_w) = bins
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    if peak_w < 1e-3 {
        return None;
    }
    let peak_hue = (peak_bin as f32 + 0.5) / HUE_BINS as f32;

    let (mut sin, mut cos, mut wsum, mut ssum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &(h, s, l) in pixels {
        if s < min_sat {
            continue;
        }
        // Shortest signed distance from the peak, in turns.
        let mut d = h.rem_euclid(1.0) - peak_hue;
        d -= d.round();
        if d.abs() > 1.0 / HUE_BINS as f32 {
            continue;
        }
        let w = weight(s, l);
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

/// Like `dominant_hue`, but hands back the pixels' own colour instead of a
/// fixed-S/L accent — for callers (the fullscreen background's per-band
/// palette sample) that want a representative sample rather than a UI tint.
/// A plain RGB mean has the same failure `dominant_hue` was fixed for: a band
/// split between two strong, separated hues averages into a colour present in
/// neither. Falls back to a plain mean when the pixels carry too little
/// colour to bin (a band that is mostly grey, black or white).
pub(crate) fn dominant_rgb(pixels: &[(f32, f32, f32)]) -> (f32, f32, f32) {
    if pixels.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = || {
        let n = pixels.len() as f32;
        let (mut r, mut g, mut b) = (0.0f32, 0.0f32, 0.0f32);
        for &(pr, pg, pb) in pixels {
            r += pr;
            g += pg;
            b += pb;
        }
        (r / n, g / n, b / n)
    };
    let hsl: Vec<(f32, f32, f32)> = pixels
        .iter()
        .map(|&(r, g, b)| rgb_to_hsl(r, g, b))
        .collect();
    let mut bins = [0.0f32; HUE_BINS];
    for &(h, s, _) in &hsl {
        if s < 0.08 {
            continue;
        }
        let bin = ((h.rem_euclid(1.0) * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        bins[bin] += s * s;
    }
    let (peak_bin, &peak_w) = bins
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    if peak_w < 1e-3 {
        return mean();
    }
    let peak_hue = (peak_bin as f32 + 0.5) / HUE_BINS as f32;
    let (mut r, mut g, mut b, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for (i, &(h, s, _)) in hsl.iter().enumerate() {
        if s < 0.08 {
            continue;
        }
        let mut d = h.rem_euclid(1.0) - peak_hue;
        d -= d.round();
        if d.abs() > 1.0 / HUE_BINS as f32 {
            continue;
        }
        let (pr, pg, pb) = pixels[i];
        r += pr;
        g += pg;
        b += pb;
        n += 1.0;
    }
    if n < 1.0 {
        mean()
    } else {
        (r / n, g / n, b / n)
    }
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

/// How long a non-essential transition should run for.
///
/// One place decides it so `Settings::reduced_motion` cannot be honoured by
/// some entrance animations and quietly ignored by the rest, which is what
/// happened when each call site spelled its own duration out.
///
/// Reduced motion collapses the animation to a single frame rather than
/// removing it: `with_animation` needs the wrapper either way, and rebuilding
/// each site to drop it would double every one of them. The floor is 1ms and
/// not 0 on purpose — gpui divides the elapsed time by the duration, so a zero
/// duration yields `0.0 / 0.0` on a frame landing in the same clock tick as
/// the animation's start, and the `debug_assert!` on the resulting delta trips
/// on the NaN.
pub fn transition(reduced_motion: bool, ms: u64) -> Duration {
    Duration::from_millis(if reduced_motion { 1 } else { ms })
}

/// Eased openness of a [`Reveal`]: `from` toward its target, `elapsed` into a
/// travel of `span`.
///
/// The easing is applied to the *travel*, not to the openness, which is what
/// makes the exit the mirror of the entrance rather than a stall followed by a
/// collapse: `ease_out_quint` spends 90% of its curve in the first 40% of its
/// input, so reading it at a falling openness leaves a panel sitting still for
/// half its exit and then vanishing.
fn reveal_openness(from: f32, open: bool, elapsed: Duration, span: Duration) -> f32 {
    let target = if open { 1. } else { 0. };
    let t = (elapsed.as_secs_f32() / span.as_secs_f32()).clamp(0., 1.);
    from + (target - from) * ease_out_quint()(t)
}

/// Open/close clock for an element that is only mounted while it is open.
///
/// `with_animation` runs only while its element is in the tree, so anything
/// dropped from the tree on close plays no exit at all — which is why the
/// panels in this app used to slide in and then simply blink out. A `Reveal`
/// keeps the transition's state *outside* the element: the view asks it
/// whether to draw at all ([`visible`](Reveal::visible)), how far along the
/// travel is ([`openness`](Reveal::openness)) and whether another frame is
/// owed ([`settling`](Reveal::settling)), then applies the styles itself.
///
/// Driving it from state is also what keeps nested animations alive. gpui keys
/// an animation's start instant on the whole ancestor element-id path, so a
/// `with_animation` wrapper swapped between an enter id and an exit id restarts
/// every animated element underneath it.
///
/// Toggle it from `render` against whatever bool already owns the panel
/// (`set` is a no-op unless that is a change) so the two cannot drift apart.
#[derive(Debug, Clone, Copy)]
pub struct Reveal {
    /// Where the travel is headed.
    open: bool,
    /// Openness when the direction last changed — 0 closed, 1 fully open.
    from: f32,
    /// When it changed.
    since: Instant,
    enter_ms: u64,
    exit_ms: u64,
}

impl Reveal {
    /// A closed reveal. Exits are usually given less time than entrances: an
    /// element arriving is being read, one leaving is already out of the way.
    pub fn new(enter_ms: u64, exit_ms: u64) -> Self {
        Self {
            open: false,
            from: 0.,
            since: Instant::now(),
            enter_ms,
            exit_ms,
        }
    }

    /// A reveal that is already fully open, with no travel to play.
    ///
    /// For an element whose *closed* state is the exceptional one — the player
    /// bar is on screen the moment the window is, and a bar sliding up on
    /// every launch is an animation nobody asked for.
    pub fn opened(enter_ms: u64, exit_ms: u64) -> Self {
        Self {
            open: true,
            from: 1.,
            since: Instant::now(),
            enter_ms,
            exit_ms,
        }
    }

    /// Point the reveal at `open`. A no-op unless that is a change, so it can
    /// be called unconditionally from `render`.
    ///
    /// A reversal starts from where the travel had got to rather than from the
    /// end it never reached, so a panel toggled twice in quick succession
    /// turns around instead of jumping open and then closing.
    pub fn set(&mut self, open: bool, reduced_motion: bool) {
        if self.open == open {
            return;
        }
        self.from = self.openness(reduced_motion);
        self.open = open;
        self.since = Instant::now();
    }

    /// Replay the entrance from nothing.
    ///
    /// For a panel whose *contents* are swapped while it is open: the new
    /// contents should read as arriving, not as the old ones being overwritten
    /// where they stand.
    pub fn replay(&mut self) {
        self.open = true;
        self.from = 0.;
        self.since = Instant::now();
    }

    /// 0 closed … 1 fully open, eased — the opacity, and the fraction of any
    /// offset the transition slides through.
    pub fn openness(&self, reduced_motion: bool) -> f32 {
        reveal_openness(
            self.from,
            self.open,
            self.since.elapsed(),
            self.span(reduced_motion),
        )
    }

    /// Whether the element belongs in the tree at all: open, or still leaving.
    pub fn visible(&self, reduced_motion: bool) -> bool {
        self.open || self.openness(reduced_motion) > 0.
    }

    /// Whether the travel still needs frames. gpui only redraws on demand, so
    /// a view driving a `Reveal` must ask for the next frame while this holds.
    pub fn settling(&self, reduced_motion: bool) -> bool {
        self.since.elapsed() < self.span(reduced_motion)
    }

    /// The travel's duration, scaled by how far it actually has to go: a
    /// reversal covering a third of the distance should not take as long as
    /// the full trip, which reads as the panel crawling.
    fn span(&self, reduced_motion: bool) -> Duration {
        let ms = if self.open {
            self.enter_ms
        } else {
            self.exit_ms
        };
        let target = if self.open { 1. } else { 0. };
        // Never zero: `reveal_openness` divides by it.
        let scaled = (ms as f32 * (target - self.from).abs()).round().max(1.);
        transition(reduced_motion, scaled as u64)
    }
}

/// Outer glow used by the vi-mode focus cursor.
///
/// Private on purpose: [`with_focus_cursor`] is the only way in, so the glow
/// cannot be painted somewhere that forgot to ask whether the user wants it.
fn focus_glow(c: Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(c.h, c.s, c.l, 0.28),
        offset: point(px(0.), px(0.)),
        blur_radius: px(9.),
        spread_radius: px(0.),
    }]
}

/// Entry animation for a focused list item: the glow grows in over ~180ms
/// each time the vi cursor lands on the item. The wrapper element id is
/// per-item, so it mounts when focused and unmounts when the cursor moves —
/// the animation replays on every jump.
fn with_focus_animation<E: IntoElement + Styled + 'static>(
    id: impl Into<SharedString>,
    el: E,
    c: Hsla,
) -> AnimationElement<E> {
    el.with_animation(
        ElementId::Name(id.into()),
        Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
        move |el, t| {
            el.shadow(vec![BoxShadow {
                color: hsla(c.h, c.s, c.l, 0.28 * t),
                offset: point(px(0.), px(0.)),
                blur_radius: px(9. * t),
                spread_radius: px(0.),
            }])
        },
    )
}

/// Mark `el` as the item the vi cursor is on.
///
/// The border is the cursor itself and is always drawn: something has to say
/// where the keyboard is. The fill, the glow and the glow's entry animation are
/// the flourish behind `Settings::selection_glow`, off by default — every j/k
/// step lighting up was a lot of motion for a text cursor.
///
/// One function decides this so the setting cannot be honoured by the card
/// grids and quietly ignored by every list of rows, which is exactly what
/// happened while each call site spelled the styling out for itself. `id` must
/// differ per item — `with_animation` keys its state on the element-id path.
///
/// Scroll-into-view is *not* here: `anchor_scroll` belongs to the stateful
/// element traits and takes a per-view anchor, so it stays at the call site,
/// where it must run whether or not the glow is on.
///
/// `color` overrides the theme's primary colour — `Settings::selection_glow_album_color`
/// passes the hovered/focused album's own accent where a call site has one;
/// `None` keeps the theme colour, same as before that setting existed.
pub fn with_focus_cursor<E: IntoElement + Styled + 'static>(
    id: impl Into<SharedString>,
    el: E,
    focused: bool,
    glow: bool,
    color: Option<Hsla>,
    cx: &App,
) -> AnyElement {
    if !focused {
        return el.into_any_element();
    }
    let c = color.unwrap_or(cx.theme().primary);
    let el = el.border_1().border_color(c);
    if !glow {
        return el.into_any_element();
    }
    with_focus_animation(id, el.bg(cx.theme().muted).shadow(focus_glow(c)), c).into_any_element()
}

/// The same border + glow as [`with_focus_cursor`], behind
/// `Settings::selection_glow_hover`, for whichever card or row the pointer is
/// over — minus the entry animation, since the mouse doesn't jump onto an
/// item the way j/k does, so there's no transition to animate.
///
/// Meant to be composed into a call site's existing `.hover` closure rather
/// than added as a second `.hover`: gpui panics if `hover_style` is already
/// set on an element, and every card/row already has one for its muted-bg
/// hover fill. `color` is the same album-accent override as `with_focus_cursor`.
pub fn hover_glow_style(s: StyleRefinement, color: Option<Hsla>, cx: &App) -> StyleRefinement {
    let c = color.unwrap_or(cx.theme().primary);
    s.border_1().border_color(c).shadow(focus_glow(c))
}

/// Lazily decodes and caches a per-item accent colour from cover-art bytes
/// already on disk, for `Settings::selection_glow_album_color` in card grids
/// (`albums.rs`, `local_music.rs`, `artists.rs`, `recent.rs`): the art is
/// already cached by the time a card renders, so this costs nothing but a
/// decode, and only on the first render of a given key.
pub fn album_glow_accent(
    cache: &mut std::collections::HashMap<String, Hsla>,
    key: &str,
    path: &std::path::Path,
) -> Option<Hsla> {
    if let Some(c) = cache.get(key) {
        return Some(*c);
    }
    let bytes = std::fs::read(path).ok()?;
    let accent = accent_from_cover_bytes(&bytes)?;
    cache.insert(key.to_string(), accent);
    Some(accent)
}

/// Scroll the vi-focused element into view, from `render`.
///
/// A `ScrollAnchor`'s origin is only as fresh as the last paint, and
/// `ScrollAnchor::scroll_to` applies it at the start of the *next* frame. From
/// a key handler (input dispatch, before this frame's draw) that origin is
/// still the row the cursor just LEFT: going down the new row merely rides one
/// below the top edge, but going up it lands one row above the viewport and
/// the highlight vanishes. Called from `render` instead, the callback fires a
/// frame after the focused element's own paint, with its fresh origin; the
/// `refresh` is what makes that frame draw at all, since a bare scroll offset
/// dirties nothing and would otherwise sit unpainted until the next repaint.
///
/// `synced` tracks the cursor position the scroll has caught up to, so a
/// repaint that didn't move the cursor doesn't scroll again.
pub fn sync_focus_scroll(
    anchor: &ScrollAnchor,
    cursor: Option<usize>,
    synced: &mut Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    if *synced == cursor {
        return;
    }
    *synced = cursor;
    if cursor.is_some() {
        anchor.scroll_to(window, cx);
        window.refresh();
    }
}

/// Height of the minimal title bar. macOS's traffic lights are 12px tall and
/// `TitleBar::title_bar_options` puts them at (9, 9), so 30px is the shortest
/// bar that still leaves them evenly inset. gpui-component's own bar is 34px
/// and carries a label the minimal one drops.
pub const MINIMAL_TITLE_BAR_HEIGHT: Pixels = px(30.);

/// The in-app title bar. `minimal` strips it to the window controls: no app
/// name, no bottom rule, the app background rather than the title-bar tint,
/// and the shorter height above — on macOS that reads as the traffic lights
/// floating on the app itself. Dragging and double-click zoom still work,
/// since both live on the bar element either way.
pub fn title_bar(minimal: bool, cx: &App) -> TitleBar {
    if minimal {
        TitleBar::new()
            .h(MINIMAL_TITLE_BAR_HEIGHT)
            .bg(cx.theme().background)
            .border_b_0()
    } else {
        TitleBar::new().child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Scirè"),
        )
    }
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
    #[test]
    fn credits_spell_a_collaboration_out_and_fall_back_to_the_single_pair() {
        use super::{artist_credits, artist_links};
        let refs = |pairs: &[(&str, &str)]| -> Vec<subsonic::ArtistRef> {
            pairs
                .iter()
                .map(|(id, name)| subsonic::ArtistRef {
                    id: (*id).into(),
                    name: (*name).into(),
                })
                .collect()
        };
        // The array wins: it is the only place both names are written down.
        let both = refs(&[("a1", "irossa"), ("a2", "Amore Audio")]);
        assert_eq!(
            artist_links(&both, Some("irossa • Amore Audio"), Some("a1")),
            vec![
                ("irossa".to_string(), "a1".to_string()),
                ("Amore Audio".to_string(), "a2".to_string())
            ]
        );
        // A vanilla server sends no array: exactly the one credit as before.
        assert_eq!(
            artist_links(&[], Some("The Beatles"), Some("a9")),
            vec![("The Beatles".to_string(), "a9".to_string())]
        );
        // A credit with no id has no page to open, so it is not offered —
        // but it is still a credit, and the text side keeps it.
        assert!(artist_links(&[], Some("Unknown"), None).is_empty());
        assert_eq!(
            artist_credits(&[], Some("Unknown"), None),
            vec![("Unknown".to_string(), None)]
        );
        assert!(artist_credits(&[], Some("  "), Some("a1")).is_empty());
        assert!(artist_credits(&[], None, Some("a1")).is_empty());
    }

    #[test]
    fn a_reveal_travels_between_its_two_ends_and_stops_there() {
        use super::reveal_openness;
        use std::time::Duration;
        let span = Duration::from_millis(200);
        // Both ends are exact: a panel left a shade short of 1 is one that
        // never quite arrives, and one left a shade above 0 keeps its element
        // in the tree forever.
        assert_eq!(reveal_openness(0., true, Duration::ZERO, span), 0.);
        assert_eq!(reveal_openness(0., true, span, span), 1.);
        assert_eq!(reveal_openness(1., false, Duration::ZERO, span), 1.);
        assert_eq!(reveal_openness(1., false, span, span), 0.);
        // Past the end it stays put rather than overshooting.
        assert_eq!(reveal_openness(1., false, span * 3, span), 0.);
    }

    #[test]
    fn a_reveals_exit_mirrors_its_entrance() {
        use super::reveal_openness;
        use std::time::Duration;
        let span = Duration::from_millis(200);
        // Easing the travel rather than the openness is what makes the two
        // directions the same movement: the exit at `t` is the entrance at
        // `1 - t`. Read at a falling openness instead, the exit would barely
        // move for half its duration and then drop.
        for step in 0..=10 {
            let at = span.mul_f32(step as f32 / 10.);
            let entering = reveal_openness(0., true, at, span);
            let leaving = reveal_openness(1., false, at, span);
            assert!((entering + leaving - 1.).abs() < 1e-5, "at {step}");
        }
        // And it is front-loaded: over a third of the way in the first tenth.
        assert!(reveal_openness(0., true, span.mul_f32(0.1), span) > 0.33);
    }

    #[test]
    fn a_reversal_starts_from_where_the_travel_had_got_to() {
        use super::reveal_openness;
        use std::time::Duration;
        let span = Duration::from_millis(200);
        // Half-open and sent back: it leaves from 0.5, not from 1.
        assert_eq!(reveal_openness(0.5, false, Duration::ZERO, span), 0.5);
        assert_eq!(reveal_openness(0.5, false, span, span), 0.);
        assert_eq!(reveal_openness(0.5, true, span, span), 1.);
        assert!(reveal_openness(0.5, true, span.mul_f32(0.5), span) > 0.5);
    }

    #[test]
    fn a_reveal_is_visible_while_it_is_still_leaving() {
        use super::Reveal;
        let mut r = Reveal::new(200, 120);
        assert!(!r.visible(false));
        r.set(true, false);
        assert!(r.visible(false));
        r.set(false, false);
        // Closed as far as the flag driving it is concerned, but still drawn —
        // that is the whole point of the type.
        assert!(r.visible(false));
        assert!(r.settling(false));
        // Reduced motion collapses the travel to a single frame, so the very
        // next read is already done with it.
        let mut quick = Reveal::new(200, 120);
        quick.set(true, true);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(quick.openness(true), 1.);
        assert!(!quick.settling(true));
    }

    #[test]
    fn a_replay_starts_the_entrance_over_from_nothing() {
        use super::Reveal;
        let mut r = Reveal::new(200, 120);
        r.set(true, false);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let grown = r.openness(false);
        assert!(grown > 0.);
        // A panel whose contents were swapped arrives, rather than being
        // overwritten where it stands: the travel goes back to the start.
        r.replay();
        assert!(r.openness(false) < grown);
        assert!(r.settling(false));
    }

    #[test]
    fn the_volume_taper_round_trips_and_keeps_its_ends() {
        use super::{volume_amplitude, volume_position};
        assert_eq!(volume_amplitude(0.), 0.);
        assert_eq!(volume_amplitude(1.), 1.);
        // Mid-handle is well under half amplitude — that is the whole point.
        assert!((volume_amplitude(0.5) - 0.25).abs() < 1e-6);
        for step in 0..=10 {
            let p = step as f32 / 10.;
            assert!((volume_position(volume_amplitude(p)) - p).abs() < 1e-5);
        }
        // Out-of-range input is clamped, not propagated as a silly amplitude.
        assert_eq!(volume_amplitude(2.), 1.);
        assert_eq!(volume_position(-1.), 0.);
    }

    use super::{
        LiveWidth, SIDE_PANEL_MAX_W, SIDE_PANEL_MIN_W, SIDE_PANEL_PADDING, SIDE_PANEL_TRACKS_MIN,
        accent_from_cover_bytes, album_side_panel, card_padding, format_count, format_playtime,
        grid_columns, grid_columns_padded, grid_fit, grid_fit_padded, grid_gap, grid_padding_x,
        strip_html, truncate_at_word,
    };

    /// The layout the setting asks for, on the window it was asked for: a
    /// 2560x1440 desktop with the sidebar open.
    #[test]
    fn the_side_panel_takes_a_third_of_a_widescreen_window() {
        let panel = album_side_panel(2300., 2560., 1440.).expect("a 16:9 desktop fits the panel");
        assert!(panel.width <= SIDE_PANEL_MAX_W, "{}", panel.width);
        assert!(panel.width >= SIDE_PANEL_MIN_W);
        // Cover at the panel's inner width, well past the 220px the stacked
        // header draws — the whole point of the trade.
        assert_eq!(panel.art, panel.width - SIDE_PANEL_PADDING);
        assert!(panel.art > 220.);
    }

    /// Every way the window can fail to hold the panel comes back as the
    /// stacked page, never as a squeezed one.
    #[test]
    fn a_window_without_the_room_keeps_the_stacked_page() {
        // Nothing measured yet.
        assert!(album_side_panel(0., 2560., 1440.).is_none());
        // Wide enough in pixels, too square: the layout was asked for on a
        // *widescreen* window.
        assert!(album_side_panel(1300., 1400., 1400.).is_none());
        // Portrait.
        assert!(album_side_panel(1000., 1080., 1920.).is_none());
        // Landscape but small.
        assert!(album_side_panel(800., 1000., 700.).is_none());
    }

    /// The panel's width is a whole number of pixels, and the page's own
    /// measurement — the track column's rounded bounds plus the panel — is
    /// therefore a fixed point rather than a value that alternates every frame.
    /// A fractional panel had the cover and every line under it bobbing for as
    /// long as the page was open.
    #[test]
    fn the_panel_width_is_whole_pixels() {
        for content in [1128., 1216., 1301.5, 900.25, 2300.] {
            let Some(panel) = album_side_panel(content, content + 52., 700.) else {
                continue;
            };
            assert_eq!(panel.width, panel.width.floor(), "content {content}");
            // What the page feeds back into its own width next frame.
            let measured = (content - panel.width).round() + panel.width;
            assert_eq!(measured, content.round(), "content {content}");
            assert_eq!(
                album_side_panel(measured, measured + 52., 700.).map(|p| p.width),
                Some(panel.width),
                "content {content} must settle"
            );
        }
    }

    /// The track list's floor outranks the panel: a content column that can't
    /// give the panel its minimum without squeezing the rows gets neither.
    #[test]
    fn the_track_list_keeps_its_width() {
        let content = SIDE_PANEL_TRACKS_MIN + SIDE_PANEL_MIN_W;
        let panel = album_side_panel(content, 1600., 900.).expect("exactly enough");
        assert_eq!(panel.width, SIDE_PANEL_MIN_W);
        assert!(album_side_panel(content - 1., 1600., 900.).is_none());
    }

    /// A wide but short window (a half-height window on a 1440p screen) would
    /// draw a cover taller than the panel's own room, so the art is capped
    /// against the height — and where that leaves it smaller than the stacked
    /// header's, the panel is dropped.
    #[test]
    fn a_short_window_caps_the_cover_and_then_gives_up() {
        let panel = album_side_panel(2300., 2560., 700.).expect("still widescreen");
        assert_eq!(panel.art, 700. * super::SIDE_PANEL_ART_SHARE);
        assert!(panel.art < panel.width - SIDE_PANEL_PADDING);
        // Shorter still: the cap puts the cover under `SIDE_PANEL_ART_MIN`, and
        // the stacked page draws a bigger one than that.
        assert!(album_side_panel(2300., 2560., 300.).is_none());
    }

    /// Chrome between the window edge and the grid: sidebar plus the content
    /// column's own padding. Constant while the window is dragged, which is the
    /// assumption `LiveWidth` is built on.
    const CHROME: f32 = 262.;

    /// Drive one frame: the grid is measured at whatever it was laid out at
    /// last frame, and asks for the width to lay out at now.
    fn frame(live: &mut LiveWidth, last_layout: f32, viewport: f32) -> f32 {
        live.resolve_at(last_layout, viewport)
    }

    #[test]
    fn live_width_holds_still_when_the_window_does() {
        let mut live = LiveWidth::default();
        // First frame: nothing laid out yet, so no width to offer.
        assert_eq!(frame(&mut live, 0., 1200.), 0.);
        // The measurement lands and the chrome is learned from it.
        assert_eq!(frame(&mut live, 1200. - CHROME, 1200.), 1200. - CHROME);
        // A settled window keeps answering the same width.
        assert_eq!(frame(&mut live, 1200. - CHROME, 1200.), 1200. - CHROME);
    }

    /// The regression: a shrink is two frames, and the second one carries the
    /// first one's measurement. Subtracting it from the pre-resize viewport put
    /// the chrome a window too wide, `viewport - chrome` clamped to zero, and
    /// the grid read that as "not laid out yet" and fell back to five columns
    /// inside a pane that fits one — which is the row that ended up half under
    /// the sidebar.
    #[test]
    fn live_width_survives_the_frame_a_resize_lands_on() {
        let mut live = LiveWidth::default();
        frame(&mut live, 0., 1200.);
        frame(&mut live, 1200. - CHROME, 1200.);

        // The window is now 620 wide, but the grid was measured at 1200.
        assert_eq!(frame(&mut live, 1200. - CHROME, 620.), 620. - CHROME);
        // ...and now the measurement catches up. This is the frame that broke.
        assert_eq!(frame(&mut live, 620. - CHROME, 620.), 620. - CHROME);
        // Still right once everything has settled.
        assert_eq!(frame(&mut live, 620. - CHROME, 620.), 620. - CHROME);
    }

    /// A drag is a run of those, every frame both a new viewport and a stale
    /// measurement. The width must track the window, never the measurement.
    #[test]
    fn live_width_tracks_a_drag_without_trailing() {
        let mut live = LiveWidth::default();
        frame(&mut live, 0., 1400.);
        let mut laid_out = frame(&mut live, 1400. - CHROME, 1400.);
        for viewport in [1300., 1200., 1100., 1000., 900., 800.] {
            laid_out = frame(&mut live, laid_out, viewport);
            assert_eq!(laid_out, viewport - CHROME, "at viewport {viewport}");
        }
    }

    /// The vi cursor asks for the column count outside `render`, so a second
    /// call inside one frame must not relearn the chrome against a measurement
    /// that has already been folded into it.
    #[test]
    fn live_width_is_stable_across_repeated_calls_in_one_frame() {
        let mut live = LiveWidth::default();
        frame(&mut live, 0., 1200.);
        frame(&mut live, 1200. - CHROME, 1200.);
        let measured = 1200. - CHROME;
        assert_eq!(frame(&mut live, measured, 900.), 900. - CHROME);
        assert_eq!(frame(&mut live, measured, 900.), 900. - CHROME);
        assert_eq!(frame(&mut live, measured, 900.), 900. - CHROME);
    }

    /// At the default scale every scaled metric is its own base, which is what
    /// makes the rest of the layout tests here a regression check on the whole
    /// scaling change: they assert the numbers the grids were built with.
    ///
    /// The scale is a process global, so nothing in this module may *set* it —
    /// tests run in parallel and a scale left on would be read by every other
    /// layout test in the file. `UiScale::factor` is pure and carries the rest.
    #[test]
    fn the_default_scale_is_the_identity() {
        use super::{card_inset, ui_scale};
        assert_eq!(ui_scale(), 1.0);
        assert_eq!(super::scaled(123.), 123.);
        assert_eq!(grid_gap(), 16.);
        assert_eq!(card_inset(), 6.);
        assert_eq!(card_padding(), 14.);
        assert_eq!(grid_padding_x(), 32.);
    }

    /// The border is deliberately left out of the scaling, so a card's chrome
    /// is its scaled inset plus a hairline that stays a hairline.
    #[test]
    fn a_cards_chrome_scales_its_inset_but_not_its_border() {
        use crate::config::UiScale;
        for scale in UiScale::ALL {
            let f = scale.factor();
            let inset = 6. * f;
            assert!(
                (inset * 2. + 2. - (6. * f * 2. + 2.)).abs() < 1e-6,
                "{scale:?}"
            );
        }
        assert_eq!(UiScale::Normal.factor(), 1.0);
        assert!(UiScale::Snug.factor() < 1.0);
        assert!(UiScale::Roomy.factor() > 1.0);
        assert!(UiScale::Large.factor() > UiScale::Roomy.factor());
    }

    /// Flush hands the card's whole chrome to the cover; the card's width is
    /// untouched either way, which is what keeps the column count fixed.
    #[test]
    fn a_flush_cover_takes_exactly_the_cards_chrome() {
        use super::card_cover_edge;
        assert_eq!(card_cover_edge(160., false), 160.);
        assert_eq!(card_cover_edge(160., true), 160. + card_padding());
    }

    #[test]
    fn grid_columns_fit_the_cards_and_their_gaps() {
        // 168px cover + 14px card padding and border = 182 wide, 16 between.
        assert_eq!(grid_columns(182., 168.), Some(1));
        // One more card needs its gap too: 182 + 16 + 182.
        assert_eq!(grid_columns(379., 168.), Some(1));
        assert_eq!(grid_columns(380., 168.), Some(2));
        // Never zero, however narrow.
        assert_eq!(grid_columns(20., 168.), Some(1));
        // Nothing laid out yet — the caller's guess stands.
        assert_eq!(grid_columns(0., 168.), None);
    }

    /// The grid's own `px_4` is inside the bounds a scroll handle reports, so
    /// the last column has to be bought out of the padded width — measuring
    /// from the raw bounds fits a card the row then clips.
    #[test]
    fn grid_columns_leave_room_for_the_grid_padding() {
        // Two 182-wide cards, one 16 gap, and the grid's 32 of padding.
        assert_eq!(grid_columns_padded(412., 168.), Some(2));
        assert_eq!(grid_columns_padded(411., 168.), Some(1));
        // The unpadded maths is exactly one column too eager over that band.
        assert_eq!(grid_columns(411., 168.), Some(2));
        assert_eq!(grid_columns_padded(0., 168.), None);
    }

    /// Row width for `cols` cards of `tile`, gaps included — what the grid
    /// actually lays out, so the fit can be checked against the width it was
    /// given rather than against the formula that produced it.
    fn row_width(cols: usize, tile: f32) -> f32 {
        cols as f32 * (tile + card_padding()) + (cols - 1) as f32 * grid_gap()
    }

    /// The point of the range: whatever the width, the row fills it rather than
    /// leaving the division's remainder as gutters.
    #[test]
    fn grid_fit_spends_the_leftover_on_the_tiles() {
        // A 16:9 window's content pane. At a fixed 150 tile this fits 12
        // columns and leaves ~154px of gutter; the tiles take it instead.
        let (cols, tile) = grid_fit(2298., 150., 198.).unwrap();
        assert_eq!(cols, 12);
        assert!(tile > 150. && tile <= 198., "tile {tile}");
        assert!((row_width(cols, tile) - 2298.).abs() < 0.5);

        // Same setting in a laptop-sized pane: fewer columns, still flush.
        let (cols, tile) = grid_fit(1200., 150., 198.).unwrap();
        assert_eq!(cols, 6);
        assert!((row_width(cols, tile) - 1200.).abs() < 0.5);
    }

    /// The column count comes from the minimum, so the range only ever grows
    /// covers into space a further column could not have used.
    #[test]
    fn grid_fit_never_trades_a_column_for_a_bigger_tile() {
        for width in (400..3000).step_by(7) {
            let width = width as f32;
            let (cols, tile) = grid_fit(width, 150., 198.).unwrap();
            assert_eq!(cols, grid_columns(width, 150.).unwrap(), "at {width}");
            assert!((150. ..=198.).contains(&tile), "tile {tile} at {width}");
            assert!(row_width(cols, tile) <= width + 0.5, "overflow at {width}");
        }
    }

    /// A window too wide for the largest tile keeps the gutters rather than
    /// letting the art grow without limit — the maximum is the setting's
    /// promise about how big "medium" gets.
    #[test]
    fn grid_fit_stops_growing_at_the_maximum() {
        let (cols, tile) = grid_fit(2298., 150., 160.).unwrap();
        assert_eq!(tile, 160.);
        assert!(row_width(cols, tile) < 2298.);
    }

    #[test]
    fn grid_fit_takes_the_grid_padding_off_like_the_column_maths_does() {
        assert_eq!(grid_fit_padded(412., 168., 168.), Some((2, 168.)));
        assert_eq!(grid_fit_padded(411., 168., 168.), Some((1, 168.)));
        assert_eq!(grid_fit_padded(0., 168., 200.), None);
        // The tile fills the padded width, not the element's own.
        let (cols, tile) = grid_fit_padded(1200., 150., 198.).unwrap();
        assert!((row_width(cols, tile) - (1200. - grid_padding_x())).abs() < 0.5);
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }

    #[test]
    fn playtime_drops_to_the_units_it_has() {
        assert_eq!(format_playtime(0.0), "0m");
        assert_eq!(format_playtime(90.0), "1m");
        assert_eq!(format_playtime(3600.0 * 5.5), "5h 30m");
        assert_eq!(format_playtime(86_400.0 * 34.0 + 3600.0 * 5.0), "34d 5h");
    }

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
