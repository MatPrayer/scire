//! Full-window now-playing overlay with dynamic blurred-art background.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, Context, ElementId, Entity, EventEmitter, IntoElement, ObjectFit,
    Render, StyledImage as _, Window, div, img, linear_color_stop, linear_gradient, prelude::*, px,
    relative, rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::popover::Popover;
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::{LyricLine, StructuredLyrics, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::{
    FullscreenBackground, FullscreenCoverSize, VisualizerMode, VisualizerSettings,
};
use crate::services::{artwork, lyrics, runtime};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;
use crate::ui::visualizer::Visualizer;

const ART_SIZE: u32 = 600;

/// Padding either side of the info card's text, subtracted from the card width
/// to get the width the marquee measures against.
const CARD_TEXT_PAD: f32 = 48.;
/// Tiny fetch for color extraction — low-res average is a fast palette sample.
const BG_ART_SIZE: u32 = 32;

// --- Content geometry ------------------------------------------------------
// The overlay used to be built out of constants: a 460px cover beside a 480px
// info card, a 320px panel, gaps and padding — roughly 1300x620 of window
// before anything is cut off. The overlay does not scroll, so a smaller window
// simply lost whatever fell past its edge. Sizes are derived from the window
// instead, and a window taller than it is wide stacks the cover above the card
// rather than squeezing two columns into a strip.

/// Cover art, biggest and smallest it is drawn beside the card.
const ART_MAX: f32 = 460.;
const ART_MIN: f32 = 140.;
/// Its cap in the stacked layout, where it has the whole width to itself: the
/// side-by-side cap leaves it looking small under a tall window's card.
const ART_MAX_STACKED: f32 = 560.;
/// How far past those a window with room to spare grows the cover — a share of
/// the room and a ceiling, both from `Settings::fullscreen_cover`. A bigger
/// window has the room to make the cover the subject of the page instead of a
/// fixed square sitting in the middle of a mostly empty one; how much of that
/// room it should take is a matter of taste, so it is a setting, and
/// `FullscreenCoverSize::Fixed` gives up the growth entirely.
/// Share of the cover's width the card is drawn at in the stacked layout, so
/// the column reads cover-first instead of as two equal blocks.
const CARD_STACKED_SHARE: f32 = 0.85;
/// How far the cover has to lead the card before the column reads that way.
const ART_LEAD: f32 = 24.;
/// Info card width. The minimum is what the transport row needs inside the
/// card's own padding: five large buttons and the gaps between them.
const CARD_MAX: f32 = 480.;
const CARD_MIN: f32 = 340.;
/// Card width below which the queue/visualizer/lyrics buttons drop their
/// labels. Three labelled buttons need ~370px of row and stick out of a card
/// narrower than this — the icons alone need ~160.
const TOGGLE_LABEL_MIN: f32 = 430.;
/// Height the info card needs for its own contents — title, artist, album,
/// seek row, stream-info line, transport, toggles and its own padding. Measured
/// from the rendered card rather than guessed: it decides how much vertical
/// room the stacked layout leaves the cover, and an under-estimate pushes the
/// toggles off the bottom of the window.
const CARD_MIN_H: f32 = 360.;
/// The same card with tightened padding and row gaps. Every line is still
/// there — this is spacing given up, not content.
const CARD_MIN_H_TIGHT: f32 = 325.;
/// Tight, and the album and stream-info lines dropped as well.
const CARD_MIN_H_COMPACT: f32 = 280.;
/// Card width at which the stream-info line ("FLAC · 2910 kbps · 96 kHz ·
/// 24-bit · stereo · RG -7.6 dB · album") still fits one row. Below it the line
/// wraps and the card is a row taller — which is exactly the case the stacked
/// layout lands in, since its card is a share of the cover's width.
const INFO_ONE_LINE_MIN: f32 = 430.;
/// That extra row, its gap included.
const INFO_WRAP_H: f32 = 32.;
/// Side panel width beside the card. The queue is a list of one-line rows and
/// asks for no more than it takes to read a title.
const PANEL_MAX: f32 = 320.;
const PANEL_MIN: f32 = 220.;
/// The lyrics panel is wider, because it is prose rather than a list: at the
/// queue's width the large type this panel draws wraps most lines in half, and
/// a verse broken across twice as many rows is harder to follow than one whose
/// lines sit where the writer put them.
const PANEL_LYRICS_MAX: f32 = 460.;
const PANEL_LYRICS_MIN: f32 = 280.;
/// Share of the window each takes before those bounds apply.
const PANEL_SHARE: f32 = 0.26;
const PANEL_LYRICS_SHARE: f32 = 0.34;
/// How far a lyric line falls back at one, two, and three-or-more lines from
/// the one being sung.
///
/// The look this follows (Apple Music's, and the pens that reproduce it) carries
/// distance as a *blur* as much as a fade, and gpui has no element blur — its
/// only `blur_radius` is on a `BoxShadow`, and transforms are svg-only. So the
/// whole falloff rides on opacity, which is why it drops further and faster than
/// a blurred sheet would need it to: the depth has to come from somewhere.
const LYRIC_DIM: [f32; 3] = [0.5, 0.34, 0.22];
/// Lyric type size in rems, at rest and on the line being sung.
///
/// The lit line *grows* into the second over [`LYRIC_GROW_MS`] rather than
/// stepping between two size classes. `Styled::text_size` takes any length and
/// `with_animation`'s animator runs inside `request_layout`, so interpolating it
/// is a real, continuous scale — which is the nearest thing gpui has to the
/// reference look's `transform: scale`, there being no transform on a div at all
/// (`with_transformation` is svg-only).
///
/// The line being *left* runs the same interpolation backwards over the same
/// duration, so the two read as one handover rather than as one line growing
/// beside another that has already snapped back.
const LYRIC_REM: f32 = 1.25;
const LYRIC_REM_ACTIVE: f32 = 1.5;
/// Length of that handover.
///
/// Paired with [`gpui::ease_in_out`] rather than the `ease_out_quint` the rest
/// of the app's transitions use: quint is 90% done in the first 40% of its
/// duration, which is right for something appearing (it wants to be *there*) and
/// wrong for two lines trading places — the eye is following both, and a curve
/// that spends its tail almost stationary reads as a jump however long the
/// duration is. Symmetric easing is what lets the duration actually be felt.
const LYRIC_GROW_MS: u64 = 420;
/// Queue row height. `uniform_list` needs every row the same size, and a whole
/// number of them is what keeps the panel from ending mid-row.
const QUEUE_ROW_H: f32 = 44.;
/// The queue panel's own chrome around that list: `p_4` above and below, the
/// "Queue" header, and the `gap_2` under it.
const QUEUE_CHROME_H: f32 = 16. * 2. + 20. + 8.;
/// Window padding (`px_10`) and the gap between content columns (`gap_8`).
const EDGE: f32 = 40.;
/// Vertical window padding once compact.
const EDGE_COMPACT: f32 = 16.;
const GAP: f32 = 32.;
/// Top padding in the stacked layout: the cover reaches the corner the close
/// pill floats in, so it gets that pill's height of clearance.
const STACK_TOP: f32 = 72.;
const STACK_TOP_COMPACT: f32 = 48.;
/// Width the vertical volume column occupies.
const VOLUME_W: f32 = 56.;
/// Width the cover and card need before the volume column earns its place.
const VOLUME_KEEP: f32 = 300. + CARD_MAX;
/// Widest the floating mini player is drawn.
const MINI_W: f32 = 560.;

/// How much room the info card is given.
///
/// The card is the one piece of the overlay with a height floor of its own —
/// everything else scales — so a window that cannot fit it takes it away in
/// stages, and **spacing goes before content**: `Tight` is the same card with
/// its padding and row gaps pulled in, and only a window too short for even
/// that drops the album and stream-info lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardDensity {
    Full,
    Tight,
    Compact,
}

impl CardDensity {
    /// The densest form a window of this size has room for.
    ///
    /// The card width is only an estimate here — the exact one falls out of the
    /// height that is left, which is what this is deciding — but it has to be
    /// taken into account, because a card too narrow for the stream-info line
    /// is a row taller. The estimate errs on the side of the taller card:
    /// picking a denser form than fits is content off the bottom edge, picking
    /// a tighter one costs only spacing.
    fn for_window(width: f32, height: f32) -> Self {
        let content = (width - 2. * EDGE).max(0.);
        let card = if height > width {
            // Stacked: the card is drawn to a share of the cover, which has the
            // content width.
            content.min(CARD_MAX) * CARD_STACKED_SHARE
        } else {
            (content - GAP - ART_MIN).clamp(CARD_MIN, CARD_MAX)
        };
        if height >= Self::Full.card_height(card) + 2. * EDGE {
            Self::Full
        } else if height >= Self::Tight.card_height(card) + 2. * EDGE_COMPACT {
            Self::Tight
        } else {
            Self::Compact
        }
    }

    /// Height this form of the card needs at a given width.
    fn card_height(self, card_width: f32) -> f32 {
        let base = match self {
            Self::Full => CARD_MIN_H,
            Self::Tight => CARD_MIN_H_TIGHT,
            Self::Compact => CARD_MIN_H_COMPACT,
        };
        if self.secondary_lines() && card_width < INFO_ONE_LINE_MIN {
            base + INFO_WRAP_H
        } else {
            base
        }
    }

    /// Padding around the content, and above it in the stacked layout.
    fn edge_y(self) -> f32 {
        if self == Self::Full {
            EDGE
        } else {
            EDGE_COMPACT
        }
    }

    fn stack_top(self) -> f32 {
        if self == Self::Full {
            STACK_TOP
        } else {
            STACK_TOP_COMPACT
        }
    }

    /// The card's own padding and the gap between its rows.
    fn card_pad(self) -> f32 {
        if self == Self::Full { 24. } else { 16. }
    }

    fn card_gap(self) -> f32 {
        if self == Self::Full { 20. } else { 12. }
    }

    /// Whether the album line and the stream-info line are drawn.
    fn secondary_lines(self) -> bool {
        self != Self::Compact
    }
}
/// Cover cap beside the card once the window has height to spare. Below the
/// point where the share reaches `ART_MAX` this is exactly the old cap, so a
/// window that only just fits the overlay is laid out as it always was — and
/// `Fixed`, whose share and ceiling are both zero, is clamped back to that cap
/// at every size, which is what "no growth" means here.
fn roomy_art_cap(height: f32, size: FullscreenCoverSize) -> f32 {
    let (share, max) = size.beside_card();
    (height * share).clamp(ART_MAX, max.max(ART_MAX))
}

/// The same, stacked: there the column has the height and the content width is
/// what the cover has to stay inside.
fn stacked_art_cap(content: f32, size: FullscreenCoverSize) -> f32 {
    let (share, max) = size.stacked();
    (content * share).clamp(ART_MAX_STACKED, max.max(ART_MAX_STACKED))
}

/// How many whole queue rows a panel of `max_h` holds — at least one, and never
/// more than the queue has. The panel is then drawn at exactly that many rows
/// rather than at the room it was given: a panel sized to the room ends with a
/// row sliced through the middle, which reads as a rendering fault rather than
/// as a list that carries on below the fold.
fn queue_visible_rows(max_h: f32, len: usize) -> usize {
    let fits = ((max_h - QUEUE_CHROME_H) / QUEUE_ROW_H).floor().max(1.) as usize;
    fits.min(len.max(1))
}

/// Below this window width the tuning card no longer fits beside the mini
/// player and is stacked above it instead.
const TUNING_SIDE_MIN_W: f32 = 1000.;

/// Content geometry of the overlay for one window size.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    /// Cover above the info card instead of beside it.
    stacked: bool,
    /// How much room the card gets — see `CardDensity`.
    density: CardDensity,
    /// Cover edge length; 0 when there is no vertical room for one at all.
    art: f32,
    card: f32,
    /// Side panel width; 0 when no panel is open.
    panel: f32,
    panel_max_h: f32,
    /// The opt-in volume column fits. It is the first thing dropped, being the
    /// one control the player bar already carries.
    volume: bool,
    /// Padding above and below the content.
    pad_top: f32,
    pad_bottom: f32,
}

impl Layout {
    fn resolve(
        width: f32,
        height: f32,
        panel: Option<SidePanel>,
        want_volume: bool,
        cover: FullscreenCoverSize,
    ) -> Self {
        let panel_open = panel.is_some();
        let density = CardDensity::for_window(width, height);
        // Width left for the cover and the card once padding, gaps and the
        // optional columns are taken out.
        let free = |volume: bool, panel: f32| {
            let columns = 2 + usize::from(volume) + usize::from(panel > 0.);
            width
                - 2. * EDGE
                - GAP * (columns - 1) as f32
                - panel
                - if volume { VOLUME_W } else { 0. }
        };
        let side_panel = match panel {
            Some(SidePanel::Lyrics) => {
                (width * PANEL_LYRICS_SHARE).clamp(PANEL_LYRICS_MIN, PANEL_LYRICS_MAX)
            }
            Some(SidePanel::Queue) => (width * PANEL_SHARE).clamp(PANEL_MIN, PANEL_MAX),
            None => 0.,
        };
        // The volume column is kept only while it costs neither the card its
        // full width nor the cover a reasonable size — the player bar carries a
        // volume slider anyway, and the cover is what this page is for.
        let volume = want_volume && free(true, side_panel) >= VOLUME_KEEP;
        let row_fits = free(volume, side_panel) >= ART_MIN + CARD_MIN;
        // An open panel is an explicit request; the cover is not. When the row
        // holds the card and the panel but not the cover as well, drop the
        // cover rather than stacking a column that fits neither.
        let panel_row_fits = side_panel > 0. && width - 2. * EDGE - GAP - side_panel >= CARD_MIN;

        if height > width || (!row_fits && !panel_row_fits) {
            // Stacked: one column, so the panel goes under the card at the same
            // width and the vertical volume slider has nowhere sensible to sit.
            let content = (width - 2. * EDGE).max(0.);
            let gaps = GAP * if panel_open { 2. } else { 1. };
            // The column for one form of the card. Called twice: the cover gets
            // the room the card gives up, so which form fits best is only
            // knowable by laying both out.
            let column = |density: CardDensity| {
                let pad_top = density.stack_top();
                let pad_bottom = density.edge_y();
                // Place the cover and the card given a card height. The card's
                // own height depends on its width, and its width comes from the
                // height left over, so this runs twice: once for a card that
                // fits the stream-info line on one row, then again if the card
                // it produced is too narrow for that after all.
                let place = |card_h: f32| {
                    // What is left for the cover and the panel once the card,
                    // the gaps and the padding have taken their share.
                    let left = (height - card_h - pad_bottom - pad_top - gaps).max(0.);
                    // The panel keeps a floor even where that overruns the
                    // window: a 40px-tall queue is no more use than a scrollbar,
                    // and the wrapper scrolls.
                    let panel_max_h = if panel_open {
                        (height * 0.28).clamp(140., left.max(140.))
                    } else {
                        0.
                    };
                    let room = left - panel_max_h;
                    // Not even the smallest cover worth drawing fits: leave it
                    // out rather than clamp it back up to a size the window
                    // does not have — the controls are the part that has to
                    // stay usable.
                    let art = if room < ART_MIN {
                        0.
                    } else {
                        room.min(content)
                            .clamp(ART_MIN, stacked_art_cap(content, cover))
                    };
                    // The cover leads this layout — it has the whole width to
                    // itself, and a card drawn just as wide turns the column
                    // into two equal blocks. The card is drawn to a share of the
                    // cover instead, floored at the width its transport row
                    // needs.
                    let card = if art > 0. {
                        (art * CARD_STACKED_SHARE)
                            .clamp(CARD_MIN.min(content), content.clamp(0., CARD_MAX))
                    } else {
                        content.min(CARD_MAX)
                    };
                    (art, card, panel_max_h)
                };

                let (art, card, panel_max_h) = place(density.card_height(f32::MAX));
                let (art, card, panel_max_h) =
                    if density.card_height(card) > density.card_height(f32::MAX) {
                        place(density.card_height(card))
                    } else {
                        (art, card, panel_max_h)
                    };
                Self {
                    stacked: true,
                    density,
                    art,
                    card,
                    panel: if panel_open { card } else { 0. },
                    panel_max_h,
                    volume: false,
                    pad_top,
                    pad_bottom,
                }
            };

            let full = column(density);
            // A cover no bigger than the card under it reads as two stacked
            // panels rather than a cover with its controls beneath. The card
            // gives up spacing to free that height — but not its album or
            // stream-info lines: this is a tall window, and the track's own
            // details are the point of the page.
            if density == CardDensity::Full && full.art < full.card + ART_LEAD {
                let tight = column(CardDensity::Tight);
                if tight.art > full.art {
                    return tight;
                }
            }
            return full;
        }

        // The row, for one card form.
        let row = |density: CardDensity| {
            let pad = density.edge_y();
            let panel_max_h = (height - 2. * pad).min(620.);
            // An open panel is an explicit request; the cover is not. Where the
            // row holds the card and the panel but not the cover as well, the
            // cover is what goes.
            if !row_fits {
                let free = width - 2. * EDGE - GAP - side_panel;
                return Self {
                    stacked: false,
                    density,
                    art: 0.,
                    card: free.min(CARD_MAX),
                    panel: side_panel,
                    panel_max_h,
                    volume: false,
                    pad_top: pad,
                    pad_bottom: pad,
                };
            }
            let free = free(volume, side_panel);
            // The cover is square, so the window's height caps it as well.
            let art_cap = (height - 2. * pad).min(ART_MAX);
            let art = (free - CARD_MIN).min(art_cap).max(ART_MIN);
            // A window with room to spare grows the cover past `ART_MAX`, but
            // only into width the card does not want: taking it out of the card
            // instead would push it toward `CARD_MIN` and cost the toggles their
            // labels, which is a trade a roomy window never has to make.
            let roomy =
                (free - CARD_MAX).min((height - 2. * pad).min(roomy_art_cap(height, cover)));
            let art = art.max(roomy);
            let card = (free - art).clamp(CARD_MIN, CARD_MAX);
            Self {
                stacked: false,
                density,
                art,
                card,
                panel: side_panel,
                panel_max_h,
                volume,
                pad_top: pad,
                pad_bottom: pad,
            }
        };

        // The card's height depends on its width — a narrow one wraps the
        // stream-info line — and its width is only known once the cover has
        // taken its share, so the form picked from the window alone can still
        // come out too tall. Step down through the forms until one fits.
        let mut chosen = row(density);
        for tighter in [CardDensity::Tight, CardDensity::Compact] {
            if chosen.fits_height(height) || tighter.card_height(0.) >= density.card_height(0.) {
                continue;
            }
            chosen = row(tighter);
        }
        chosen
    }

    /// Whether the row this layout describes fits the window's height. The card
    /// is the only piece with a floor, so this is the check that decides
    /// whether a tighter card is needed.
    fn fits_height(&self, height: f32) -> bool {
        let body = self.density.card_height(self.card).max(self.art);
        self.pad_top + body + self.pad_bottom <= height
    }
}

/// Entrance duration. Long enough for the zoom to read, short enough that the
/// overlay never feels like it is loading.
const ENTER: Duration = Duration::from_millis(340);
/// Exit is deliberately faster than the entrance — dismissals should feel
/// immediate. `begin_close` waits this out (plus a frame) before unmounting.
const EXIT: Duration = Duration::from_millis(220);

/// Decelerating curve — fast start, soft landing.
fn out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Decelerating curve that overshoots 1.0 slightly before settling. Only used
/// for geometry (never for opacity, and never as an `Animation` easing — gpui
/// asserts easings stay inside 0..=1).
fn out_back(t: f32) -> f32 {
    const C1: f32 = 1.7;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

/// One tunable of the visualizer. Everything the tuning card needs to render
/// and write a knob lives here, so adding one is a single entry in
/// [`VizKnob::ALL`] rather than a new field, slider and setter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VizKnob {
    Sensitivity,
    Smoothing,
    Intensity,
    Motion,
    SwitchSensitivity,
    SwitchHold,
}

impl VizKnob {
    const ALL: [VizKnob; 6] = [
        VizKnob::Sensitivity,
        VizKnob::Smoothing,
        VizKnob::Intensity,
        VizKnob::Motion,
        VizKnob::SwitchSensitivity,
        VizKnob::SwitchHold,
    ];

    fn id(self) -> &'static str {
        match self {
            VizKnob::Sensitivity => "viz-sensitivity",
            VizKnob::Smoothing => "viz-smoothing",
            VizKnob::Intensity => "viz-intensity",
            VizKnob::Motion => "viz-motion",
            VizKnob::SwitchSensitivity => "viz-switch-sensitivity",
            VizKnob::SwitchHold => "viz-switch-hold",
        }
    }

    fn label(self) -> &'static str {
        match self {
            VizKnob::Sensitivity => "Sensitivity",
            VizKnob::Smoothing => "Smoothing",
            VizKnob::Intensity => "Reaction depth",
            VizKnob::Motion => "Motion speed",
            VizKnob::SwitchSensitivity => "Auto switch",
            VizKnob::SwitchHold => "Scene hold",
        }
    }

    /// Only shown for the two Auto-mode knobs, which do nothing on a pinned
    /// scene and would otherwise look broken.
    fn auto_only(self) -> bool {
        matches!(self, VizKnob::SwitchSensitivity | VizKnob::SwitchHold)
    }

    /// (min, max, step) for the slider.
    fn range(self) -> (f32, f32, f32) {
        match self {
            VizKnob::Sensitivity => (0.4, 3.0, 0.05),
            VizKnob::Smoothing => (0.0, 1.0, 0.02),
            VizKnob::Intensity => (0.3, 2.5, 0.05),
            VizKnob::Motion => (0.2, 2.5, 0.05),
            VizKnob::SwitchSensitivity => (0.4, 2.5, 0.05),
            VizKnob::SwitchHold => (3.0, 40.0, 1.0),
        }
    }

    fn get(self, s: &VisualizerSettings) -> f32 {
        match self {
            VizKnob::Sensitivity => s.sensitivity,
            VizKnob::Smoothing => s.smoothing,
            VizKnob::Intensity => s.intensity,
            VizKnob::Motion => s.motion,
            VizKnob::SwitchSensitivity => s.switch_sensitivity,
            VizKnob::SwitchHold => s.switch_hold,
        }
    }

    fn set(self, s: &mut VisualizerSettings, v: f32) {
        let (min, max, _) = self.range();
        let v = v.clamp(min, max);
        match self {
            VizKnob::Sensitivity => s.sensitivity = v,
            VizKnob::Smoothing => s.smoothing = v,
            VizKnob::Intensity => s.intensity = v,
            VizKnob::Motion => s.motion = v,
            VizKnob::SwitchSensitivity => s.switch_sensitivity = v,
            VizKnob::SwitchHold => s.switch_hold = v,
        }
    }

    fn format(self, v: f32) -> String {
        match self {
            VizKnob::SwitchHold => format!("{v:.0}s"),
            _ => format!("{v:.2}×"),
        }
    }
}

/// Remap `t` onto the sub-range `start..end` of the timeline, clamped.
fn phase(t: f32, start: f32, end: f32) -> f32 {
    ((t - start) / (end - start)).clamp(0.0, 1.0)
}

/// Name of the scene a mode pins, or `None` for the two that pin nothing.
fn pinned_scene_label(mode: VisualizerMode) -> Option<&'static str> {
    VisualizerMode::SCENES
        .contains(&mode)
        .then(|| mode.menu_label())
}

/// Pick the document to display out of everything the server holds for a song.
///
/// A song can carry several — a synced and an unsynced copy of the same words,
/// or one per language. Synced wins, since it is the same text with timings
/// attached and nothing is lost by taking it; otherwise the first one the
/// server listed. Documents with no lines at all are dropped: Navidrome
/// answers with an empty `lyricsList` for a song it has nothing for, but a
/// stray empty entry would otherwise draw a blank panel instead of "no
/// lyrics found".
fn best_lyrics(docs: Vec<StructuredLyrics>) -> Option<StructuredLyrics> {
    let mut docs: Vec<StructuredLyrics> = docs
        .into_iter()
        .filter(|d| d.lines.iter().any(|l| !l.value.trim().is_empty()))
        .collect();
    let pick = docs.iter().position(|d| d.synced).unwrap_or(0);
    (pick < docs.len()).then(|| docs.swap_remove(pick))
}

/// Wrap classic getLyrics' one text blob as an unsynced document, so the panel
/// has a single shape to draw whichever endpoint answered.
fn plain_lyrics(text: String) -> StructuredLyrics {
    StructuredLyrics {
        lines: text
            .lines()
            .map(|l| LyricLine {
                start: None,
                value: l.to_string(),
            })
            .collect(),
        ..Default::default()
    }
}

/// Where the lyrics on screen came from.
///
/// Worth saying out loud: the two paths disagree about what a miss means. A
/// library hit is the file's own words and will be there offline forever; an
/// online hit is someone else's transcription of a track the library has
/// nothing for, and is the one to doubt when it drifts or belongs to a
/// different recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LyricsSource {
    /// The file's own words: its tags or a sidecar `.lrc`, whether the server
    /// indexed them or — for a local file — we read them off disk ourselves.
    Library,
    /// Looked up on LRCLIB, because nothing in the library had any.
    Online,
}

impl LyricsSource {
    fn badge(self) -> &'static str {
        match self {
            Self::Library => "Library",
            Self::Online => "LRCLIB",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Library => "From your library — the file's tags or a sidecar .lrc",
            Self::Online => "Fetched from LRCLIB; your library has none for this track",
        }
    }
}

/// Where down the panel the line being sung is parked, as a share of its
/// height. Above the middle, not at the top: the lines already sung are what
/// places the one being sung, and a line pinned to the top edge arrives with
/// nothing under it to read into.
const LYRICS_LEAD: f32 = 0.35;

/// Index of the line being sung at `position`.
///
/// The last line whose start has passed, which is `None` before the first one
/// (an intro has no line to light) and for an unsynced document, whose lines
/// carry no start at all. `offset` is the document's own correction, added
/// rather than subtracted — OpenSubsonic defines it as the offset to *apply*,
/// and `parse_lrc` converts an LRC `[offset:]` tag into that same sense.
///
/// A scan rather than a binary search: the list is a few dozen lines, it is
/// consulted twice a second, and a server-supplied document is not guaranteed
/// to be in order.
fn active_line(lines: &[LyricLine], offset: i64, position: Duration) -> Option<usize> {
    let pos = position.as_millis() as i64;
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.start.is_some_and(|start| start + offset <= pos))
        .map(|(ix, _)| ix)
        .next_back()
}

/// Where a click on a lyric line puts the playhead.
///
/// The inverse of what [`active_line`] does, and deliberately written next to
/// it: that adds the document's `offset` to each line's start before comparing
/// it against the position, so the point at which a line *becomes* the lit one
/// is `start + offset` and seeking anywhere else would light a different line
/// than the one clicked. A document whose offset drags the first line before
/// zero seeks to the beginning instead, there being nowhere earlier to go.
fn lyric_seek_target(start: i64, offset: i64) -> Duration {
    Duration::from_millis(start.saturating_add(offset).max(0) as u64)
}

/// How present a lyric line is drawn, by how far it sits from the one being
/// sung — see [`LYRIC_DIM`] for why the distance is carried by opacity alone.
///
/// An **unsynced** document is drawn flat at full strength: there is no line to
/// be on, and a sheet where everything is faded reads as the panel being
/// switched off rather than as lyrics. So does an intro, before the first line
/// has arrived — there the whole sheet sits at the nearest tier, waiting, rather
/// than at the bottom of the ramp where it would look switched off too.
fn line_emphasis(active: Option<usize>, ix: usize, synced: bool) -> f32 {
    if !synced {
        return 1.;
    }
    let Some(active) = active else {
        return LYRIC_DIM[0];
    };
    match active.abs_diff(ix) {
        0 => 1.,
        1 => LYRIC_DIM[0],
        2 => LYRIC_DIM[1],
        _ => LYRIC_DIM[2],
    }
}

/// Fraction of the panel's width a lyric line is laid out in, at a given type
/// size.
///
/// This is what stops the words rewrapping as a line grows. Where a line breaks
/// is decided by its advance width against the width it has to fill, and advance
/// width scales with the type size — so a line set at [`LYRIC_REM`] inside
/// `LYRIC_REM / LYRIC_REM_ACTIVE` of the panel breaks at exactly the words it
/// will break at once it has grown to [`LYRIC_REM_ACTIVE`] and the full width.
/// Held across the whole interpolation, the break points never move at all, and
/// the line simply gets bigger.
///
/// The cost is that a resting line wraps a word or two earlier than it strictly
/// needs to — which is the trade: the alternative is the last word of a line
/// dropping to a row of its own halfway through the animation and shoving the
/// rest of the sheet down with it. Text is left-aligned, so the width the line
/// gives up is on the right, where there is nothing to see.
fn lyric_wrap(size_rem: f32) -> f32 {
    size_rem / LYRIC_REM_ACTIVE
}

/// A lyric line's colour part-way through the handover, `t` of the way from
/// `rest` to `lit`.
///
/// gpui has no colour interpolation of its own — `Hsla::blend` is alpha
/// compositing — but compositing `lit` over `rest` at alpha `t` *is* the
/// mix, and it runs through `Rgba`, so it takes the short way between two
/// colours instead of the long way round the hue wheel that lerping H would.
/// The endpoints are exact: `blend` returns `other` at alpha 1 and `self` at 0
/// rather than a converted near-miss.
///
/// The result keeps `rest`'s alpha, which is 1 — the line's own fade is the
/// element's `opacity`, and folding it in here would apply it twice.
fn lyric_color(rest: gpui::Hsla, lit: gpui::Hsla, t: f32) -> gpui::Hsla {
    rest.blend(lit.opacity(t))
}

pub enum FullscreenEvent {
    Close,
}

/// Optional side panel next to the controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidePanel {
    Queue,
    Lyrics,
}

impl EventEmitter<FullscreenEvent> for FullscreenPlayer {}

pub struct FullscreenPlayer {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
    art_path: Option<PathBuf>,
    bg_art_path: Option<PathBuf>,
    gradient_palette: Option<Vec<gpui::Rgba>>,
    /// Album-scoped art key the loaded art belongs to (see `artwork::song_cover`).
    last_art_key: Option<String>,
    panel: Option<SidePanel>,
    /// Scroll handle of the queue panel's list, so the playing track can be
    /// scrolled back into view when it changes.
    queue_scroll: gpui::UniformListScrollHandle,
    /// Queue position the panel has already followed. Only a change scrolls, so
    /// a user reading further down the queue is not dragged back every frame;
    /// cleared when the panel closes, so reopening re-centres on the track.
    queue_followed: Option<usize>,
    /// Lyrics for the song in `lyrics_for`; None while loading and when the
    /// server has none.
    lyrics: Option<StructuredLyrics>,
    lyrics_for: Option<String>,
    lyrics_loading: bool,
    /// Which of the two paths answered, for the panel's badge. `Some` exactly
    /// when `lyrics` is.
    lyrics_source: Option<LyricsSource>,
    /// Scroll handle of the lyrics list, so the line being sung can be kept on
    /// screen. A plain `ScrollHandle` rather than a uniform list's: the lines
    /// wrap, so they are not all the same height.
    lyrics_scroll: gpui::ScrollHandle,
    /// The line lit this frame, recomputed by `sync_lyrics_scroll`.
    lyrics_active: Option<usize>,
    /// The line that *was* lit before the highlight last moved, so it can be
    /// animated back down instead of snapping. Held until the highlight moves
    /// again — a line only ever draws its shrink once, since the wrapper id it
    /// is drawn under (and so the animation behind it) is dropped the frame it
    /// stops being the previous line.
    lyrics_prev: Option<usize>,
    /// Line the panel has already scrolled to — and *settled* on, since the
    /// scroll is eased rather than jumped. Only a change scrolls, so between two
    /// lines a user reading ahead is left alone; cleared when the panel closes
    /// or the song changes, so either one re-centres.
    lyrics_followed: Option<usize>,
    /// The eased scroll in flight: where the list was when the highlight moved,
    /// and when it moved. `None` between lines, which is what leaves a user
    /// scrolling by hand alone.
    lyrics_scroll_anim: Option<(gpui::Pixels, Instant)>,
    /// Waveform peaks for the track in `waveform_for` (when the waveform
    /// seek bar is enabled and the decode finished).
    waveform: Option<Vec<f32>>,
    waveform_for: Option<String>,
    /// Fraction of the seek bar under the cursor, for the hover indicator.
    seek_hover: Option<f32>,
    /// Spectrum analysis + scene state for the 3D visualizer. Ticked once per
    /// frame while a scene is running; idle (and unread) when it is off.
    visualizer: Visualizer,
    /// One slider state per visualizer knob, in `VizKnob::ALL` order.
    viz_knobs: Vec<Entity<SliderState>>,
    /// Tuning card open over the mini player.
    viz_tuning_open: bool,
    /// The side panel currently being *drawn*, which lags `panel` by the exit:
    /// a panel dropped from the tree the moment it is closed plays no exit at
    /// all, and the layout would hand its room back before it had left.
    panel_drawn: Option<SidePanel>,
    /// Open/close travel of the side panel, driven from `panel` in `render`.
    panel_reveal: crate::ui::Reveal,
    /// Open/close travel of the tuning card over the mini player.
    viz_tuning_reveal: crate::ui::Reveal,
    /// Start of the entrance, reset on every open.
    opened_at: Instant,
    /// Set when the exit starts; `Some` also means "closing", i.e. the overlay
    /// is playing its exit and will unmount when the timer in `begin_close`
    /// fires. Both transitions are driven from these instants rather than
    /// `with_animation`: an animation's state is keyed by its ancestor
    /// element-id path, so swapping an enter/exit wrapper around the whole
    /// overlay would reset every nested animation (content stagger, and the
    /// 40s background sweep, which would visibly jump).
    closing_at: Option<Instant>,
}

impl FullscreenPlayer {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_volume = player.read(cx).volume;
        let visualizer = Visualizer::new(player.read(cx).spectrum_tap());
        let seek = cx.new(|_| SliderState::new().min(0.).max(1.).step(0.001));
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                .step(0.01)
                .default_value(crate::ui::volume_position(initial_volume))
        });

        cx.subscribe(&seek, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let fraction = value.start();
            this.player.update(cx, |p, _| {
                if let Some(total) = p.duration {
                    p.seek(crate::ui::seek_position(total, fraction));
                }
            });
        })
        .detach();

        cx.subscribe(&volume, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let v = crate::ui::volume_amplitude(value.start());
            this.player.update(cx, |p, cx| p.set_volume(v, cx));
        })
        .detach();

        // Visualizer knobs. Each writes straight into the session settings,
        // which `tick` re-reads every frame — so a drag retunes the scene as
        // it happens rather than on the next open.
        let tuning = session.read(cx).settings.visualizer_tuning;
        let viz_knobs: Vec<Entity<SliderState>> = VizKnob::ALL
            .into_iter()
            .map(|knob| {
                let (min, max, step) = knob.range();
                let state = cx.new(|_| {
                    SliderState::new()
                        .min(min)
                        .max(max)
                        .step(step)
                        .default_value(knob.get(&tuning))
                });
                cx.subscribe(&state, move |this: &mut Self, _, event, cx| {
                    let SliderEvent::Change(value) = event;
                    let v = value.start();
                    this.session.update(cx, |s, _| {
                        knob.set(&mut s.settings.visualizer_tuning, v);
                        s.persist_settings();
                    });
                    cx.notify();
                })
                .detach();
                state
            })
            .collect();

        // Watch player for song changes to update background art and lyrics.
        // Keyed on the album-scoped art key, not the song's cover id: Navidrome
        // gives each track its own, so comparing those would reload identical
        // art (and re-extract the gradient palette) on every track change.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let cover = player.read(cx).current_song().and_then(artwork::song_cover);
            let key = cover.as_ref().map(|(_, key)| key.clone());
            if key != this.last_art_key {
                this.last_art_key = key;
                this.art_path = None;
                this.bg_art_path = None;
                this.gradient_palette = None;
                if let Some((cover_id, key)) = cover {
                    this.fetch_art(cover_id, key, cx);
                }
            }
            this.maybe_fetch_lyrics(cx);
            this.maybe_fetch_waveform(cx);
            cx.notify();
        })
        .detach();

        // Settings toggle for the waveform seek bar lives on the session.
        cx.observe(&session, |this: &mut Self, _, cx| {
            this.maybe_fetch_waveform(cx);
            cx.notify();
        })
        .detach();

        Self {
            player,
            session,
            seek,
            volume,
            art_path: None,
            bg_art_path: None,
            gradient_palette: None,
            last_art_key: None,
            panel: None,
            queue_scroll: gpui::UniformListScrollHandle::new(),
            queue_followed: None,
            lyrics: None,
            lyrics_for: None,
            lyrics_loading: false,
            lyrics_source: None,
            lyrics_scroll: gpui::ScrollHandle::new(),
            lyrics_active: None,
            lyrics_prev: None,
            lyrics_followed: None,
            lyrics_scroll_anim: None,
            waveform: None,
            waveform_for: None,
            seek_hover: None,
            visualizer,
            viz_knobs,
            viz_tuning_open: false,
            panel_drawn: None,
            panel_reveal: crate::ui::Reveal::new(220, 150),
            viz_tuning_reveal: crate::ui::Reveal::new(170, 120),
            opened_at: Instant::now(),
            closing_at: None,
        }
    }

    /// Start the exit animation, then emit Close so the overlay unmounts.
    pub fn begin_close(&mut self, cx: &mut Context<Self>) {
        if self.closing_at.is_some() {
            return;
        }
        self.closing_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(EXIT + Duration::from_millis(16))
                .await;
            let _ = this.update(cx, |this, cx| {
                // Skip if a quick reopen already cancelled the close.
                if this.closing_at.is_some() {
                    this.closing_at = None;
                    cx.emit(FullscreenEvent::Close);
                }
            });
        })
        .detach();
    }

    /// Reset state so a fresh open plays the entrance (not a stale exit).
    pub fn reset_for_open(&mut self, cx: &mut Context<Self>) {
        self.closing_at = None;
        self.opened_at = Instant::now();
        // A track is usually already loaded when the overlay opens. The art
        // observer only fires on song *changes*, and a paused player never
        // notifies at all, so without this the cover stays blank until the
        // next track starts.
        if self.art_path.is_none()
            && let Some((cover_id, key)) = self
                .player
                .read(cx)
                .current_song()
                .and_then(artwork::song_cover)
        {
            self.last_art_key = Some(key.clone());
            self.fetch_art(cover_id, key, cx);
        }
        cx.notify();
    }

    /// Kick off a waveform decode when the current track changed (and the
    /// setting is on). Same flow as the player bar; the on-disk peak cache
    /// makes the second consumer effectively free.
    fn maybe_fetch_waveform(&mut self, cx: &mut Context<Self>) {
        let enabled = self.session.read(cx).settings.waveform_seekbar;
        let song_id = {
            let p = self.player.read(cx);
            if !enabled || p.is_radio() {
                None
            } else {
                p.current_song().map(|s| s.id.clone())
            }
        };
        let Some(id) = song_id else {
            self.waveform = None;
            self.waveform_for = None;
            return;
        };
        if self.waveform_for.as_deref() == Some(id.as_str()) {
            return;
        }
        let opts = crate::services::waveform::stream_options();
        let url = self
            .session
            .read(cx)
            .client
            .as_ref()
            .and_then(|c| c.stream_url(&id, &opts).ok().map(|u| u.to_string()));
        let Some(url) = url else { return };
        self.waveform = None;
        self.waveform_for = Some(id.clone());
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(crate::services::waveform::fetch_peaks(url, id.clone())).await;
            let _ = this.update(cx, |view, cx| {
                // Ignore results for a track that is no longer current.
                if view.waveform_for.as_deref() == Some(id.as_str()) {
                    match result {
                        Ok(peaks) => view.waveform = Some(peaks),
                        Err(e) => tracing::warn!("waveform peaks failed: {e:#}"),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        self.panel = if self.panel == Some(panel) {
            None
        } else {
            Some(panel)
        };
        // A reopened queue starts on the playing track again, wherever the
        // list was left scrolled, and a reopened lyrics panel on the line
        // being sung.
        self.queue_followed = None;
        self.lyrics_followed = None;
        self.lyrics_prev = None;
        self.lyrics_scroll_anim = None;
        self.maybe_fetch_lyrics(cx);
        cx.notify();
    }

    /// Keep the playing track visible in the queue panel.
    ///
    /// Called from `render` rather than from the player observer: the list only
    /// exists while the panel is open, and a scroll requested for a list that
    /// is not being drawn is one the handle applies to whatever it holds next.
    /// Only a *change* of position scrolls — see `queue_followed`.
    fn sync_queue_scroll(&mut self, panel_max_h: f32, cx: &Context<Self>) {
        if self.panel != Some(SidePanel::Queue) {
            return;
        }
        let (pos, len) = {
            let q = &self.player.read(cx).queue;
            match q.current_pos() {
                Some(pos) => (pos, q.len()),
                None => return,
            }
        };
        if self.queue_followed == Some(pos) {
            return;
        }
        self.queue_followed = Some(pos);
        // Scrolled in whole rows: `ScrollStrategy::Top` puts a row's own top at
        // the list's, so which row is sent there is what places the playing
        // track, and every row on screen stays whole. Centring on the track
        // itself would leave half a row at each edge, which is the thing the
        // panel's row-snapped height is there to avoid.
        let visible = queue_visible_rows(panel_max_h, len);
        let top = pos
            .saturating_sub(visible / 2)
            .min(len.saturating_sub(visible));
        self.queue_scroll
            .scroll_to_item(top, gpui::ScrollStrategy::Top);
    }

    /// Light the line being sung and keep it on screen.
    ///
    /// Called from `render` for the same reason `sync_queue_scroll` is: a
    /// `ScrollHandle`'s child bounds are the *last* paint's, so an offset
    /// computed from a key handler or an event places the line the panel has
    /// since moved past. Setting the offset here, before the tree is built,
    /// means this frame paints at it — and unlike a `window.refresh()`, which
    /// is a no-op while the window is drawing, that actually happens.
    ///
    /// The offset is computed rather than delegated to `scroll_to_item`, whose
    /// only strategies are "first visible" and "at the top": lyrics want the
    /// line part-way down with what came before it still readable, and the
    /// lines wrap, so a lead counted in rows is not a lead in pixels.
    ///
    /// It is also **eased** rather than set, over the same [`LYRIC_GROW_MS`] and
    /// with the same curve as the lines' own handover. A `ScrollHandle` has no
    /// animated scroll — `set_offset` is a jump — and a jump is what the whole
    /// transition is judged by: the type can ease as gently as it likes while
    /// the sheet it is set on teleports under it. The two have to be one
    /// movement. The target is recomputed from the live child bounds on every
    /// frame rather than captured at the start, which is what keeps the eased
    /// scroll honest while the lines either side of it are still changing size
    /// underneath it.
    fn sync_lyrics_scroll(&mut self, cx: &Context<Self>) {
        let was = self.lyrics_active;
        self.lyrics_active = None;
        if self.panel != Some(SidePanel::Lyrics) {
            self.lyrics_followed = None;
            self.lyrics_prev = None;
            self.lyrics_scroll_anim = None;
            return;
        }
        let Some(doc) = &self.lyrics else { return };
        // The smoothed position, not the engine's: its tick is 500ms, and a
        // line lit off it arrives up to that late against words being sung.
        // `lyrics_following` asks for the frames this resolution needs.
        let position = self.player.read(cx).smooth_position();
        self.lyrics_active = active_line(&doc.lines, doc.offset, position);
        // The handover: the line being left keeps a claim on the frame after
        // it, so it can shrink back rather than snap. This is recomputed every
        // frame and only a *change* touches it, so the outgoing line holds its
        // place for as long as it takes the highlight to move on — which is one
        // line of the song, comfortably longer than the animation.
        if self.lyrics_active != was {
            if was.is_some() {
                self.lyrics_prev = was;
            }
            // A travel still running when the highlight moves again is restarted
            // from wherever the list has got to, not continued: its start offset
            // and its clock both belong to the line before last, and easing
            // toward the new target off those would cover the remaining distance
            // in whatever was left of the old duration — a snap, at the one
            // moment (two lines in quick succession) the movement is most
            // visible.
            self.lyrics_scroll_anim = None;
        }
        let Some(active) = self.lyrics_active else {
            return;
        };
        if self.lyrics_followed == Some(active) {
            return;
        }
        // Child bounds are empty until the list has been painted once, so the
        // first frame with a panel open cannot place anything. Leaving
        // `lyrics_followed` alone is what makes the next position tick retry
        // instead of counting this as done.
        let Some(line) = self.lyrics_scroll.bounds_for_item(active) else {
            return;
        };
        // Child bounds are the unscrolled layout, and the painted position is
        // `child.top() + offset.y` — so this is the offset that puts the line
        // `lead` below the top of the panel.
        let viewport = self.lyrics_scroll.bounds();
        let lead = viewport.size.height * LYRICS_LEAD;
        let y = viewport.top() + lead - line.top();
        // Offsets run from 0 (the top) down to -max; without the clamp the last
        // verse would be dragged up past the end of the list.
        let mut offset = self.lyrics_scroll.offset();
        let target = y.clamp(-self.lyrics_scroll.max_offset().height, px(0.));
        // Where the list was when the highlight moved. Recorded once and eased
        // *from*, so a frame that arrives late moves the list further rather
        // than restarting the travel — a per-frame step off the current offset
        // would make the duration depend on how many frames happened to land.
        let (from, started) = *self
            .lyrics_scroll_anim
            .get_or_insert((offset.y, Instant::now()));
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        let span = crate::ui::transition(reduced_motion, LYRIC_GROW_MS).as_secs_f32();
        let t = (started.elapsed().as_secs_f32() / span).clamp(0., 1.);
        offset.y = from + (target - from) * gpui::ease_in_out(t);
        self.lyrics_scroll.set_offset(offset);
        // Only a finished travel counts as followed: until then this runs again
        // next frame, and `lyrics_following` is what supplies them.
        if t >= 1. {
            self.lyrics_followed = Some(active);
            self.lyrics_scroll_anim = None;
        }
    }

    /// A timed lyrics document is up with audio moving under it — the one state
    /// in which the panel is worth a frame per frame.
    ///
    /// Nothing else here runs off the wall clock: the engine's position tick is
    /// what brings the overlay back the rest of the time, and twice a second is
    /// plenty for a seek bar. An untimed document has no line to light, and a
    /// paused one is not going anywhere.
    /// An eased scroll counts too: it is driven from `render` rather than by
    /// gpui's animation element (which asks for its own frames), so without this
    /// a travel started by opening the panel on a paused track would stop
    /// wherever the last frame left it.
    fn lyrics_following(&self, cx: &Context<Self>) -> bool {
        self.panel == Some(SidePanel::Lyrics)
            && self.lyrics.as_ref().is_some_and(|doc| doc.synced)
            && (self.player.read(cx).playing || self.lyrics_scroll_anim.is_some())
    }

    /// Advance the visualizer to the next scene (and off again), persisting the
    /// choice so the overlay reopens on the same one.
    fn cycle_visualizer(&mut self, cx: &mut Context<Self>) {
        let next = self.session.read(cx).settings.visualizer.next();
        self.set_visualizer(next, cx);
    }

    /// Jump straight to a mode — what the mini player's scene picker uses, so
    /// changing scene is one click rather than a walk around the cycle.
    fn set_visualizer(&mut self, mode: VisualizerMode, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.visualizer = mode);
        self.session.read(cx).persist_settings();
        cx.notify();
    }

    /// Put every visualizer knob back to its default, sliders included — a
    /// slider owns its own value, so resetting the settings alone would leave
    /// the handles where the user dragged them.
    fn reset_viz_tuning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults = VisualizerSettings::default();
        self.session.update(cx, |s, _| {
            s.settings.visualizer_tuning = defaults;
            s.persist_settings();
        });
        for (knob, state) in VizKnob::ALL.into_iter().zip(self.viz_knobs.clone()) {
            state.update(cx, |slider, cx| {
                slider.set_value(knob.get(&defaults), window, cx);
            });
        }
        cx.notify();
    }

    /// The tuning card: the visualizer's own settings, floating over the scene
    /// so a knob can be dragged while watching what it does. Lives here rather
    /// than on the settings page because every one of these is judged by eye.
    fn viz_tuning_card(&self, mode: VisualizerMode, cx: &mut Context<Self>) -> impl IntoElement {
        let tuning = self.session.read(cx).settings.visualizer_tuning;
        let auto = mode == VisualizerMode::Auto;

        let mut rows = v_flex().gap_2p5();
        for (knob, state) in VizKnob::ALL.into_iter().zip(self.viz_knobs.iter()) {
            // Auto-only knobs stay visible on a pinned scene (so the card does
            // not reflow when the mode changes) but read as inactive.
            let dim = knob.auto_only() && !auto;
            rows = rows.child(
                v_flex()
                    .gap_0p5()
                    .when(dim, |s| s.opacity(0.45))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().child(knob.label()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(knob.format(knob.get(&tuning))),
                            ),
                    )
                    .child(
                        h_flex()
                            .id(knob.id())
                            .w_full()
                            .child(Slider::new(state).disabled(dim)),
                    ),
            );
        }

        v_flex()
            .id("viz-tuning-card")
            .w(px(260.))
            .px_5()
            .py_4()
            .gap_2()
            .rounded_2xl()
            .shadow_xl()
            // Same translucent shell as the mini player: the two read as one
            // control surface, and the scene keeps running behind both.
            .bg(cx.theme().background.opacity(0.72))
            .border_1()
            .border_color(cx.theme().border.opacity(0.6))
            // The card is a control surface floating on the scene: clicks in it
            // must not reach the overlay behind (which closes panels).
            .occlude()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().font_medium().child("Visualizer"))
                    .child(
                        Button::new("viz-tuning-reset")
                            .ghost()
                            .xsmall()
                            .label("Reset")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_viz_tuning(window, cx);
                            })),
                    ),
            )
            .child(rows)
    }

    /// Menu listing scenes, for the mini player's "More" button. The scenes
    /// outgrew a row of buttons — five of them crowded the card and made the
    /// two that are not scenes at all (Off, Auto) harder to pick out.
    fn scene_menu(&self, trigger: Button, current: VisualizerMode) -> impl IntoElement {
        let session = self.session.clone();
        Popover::new("viz-scene-menu")
            // Opens upward: the trigger sits in a card near the bottom of the
            // window, and the default downward menu covers the transport.
            .anchor(gpui::Corner::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let mut menu = v_flex().gap_0p5().min_w(px(150.));
                for (i, mode) in VisualizerMode::SCENES.into_iter().enumerate() {
                    let session = session.clone();
                    menu = menu.child(
                        div()
                            .id(("viz-scene", i))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| s.bg(cx.theme().muted))
                            .when(mode == current, |s| s.text_color(cx.theme().primary))
                            .on_click(cx.listener(move |state, _, window, cx| {
                                // Reached through the session rather than this
                                // view: inside a popover the listener's view is
                                // the popover's own state. The fullscreen
                                // player observes the session, so it re-renders
                                // from this anyway.
                                session.update(cx, |s, cx| {
                                    s.settings.visualizer = mode;
                                    cx.notify();
                                });
                                session.read(cx).persist_settings();
                                state.dismiss(window, cx);
                            }))
                            .child(mode.menu_label()),
                    );
                }
                menu
            })
    }

    /// Fetch lyrics for the current song when the lyrics panel is open.
    fn maybe_fetch_lyrics(&mut self, cx: &mut Context<Self>) {
        if self.panel != Some(SidePanel::Lyrics) {
            return;
        }
        let online = self.session.read(cx).settings.online_lyrics;
        let (id, query, local_path) = {
            let p = self.player.read(cx);
            let Some(song) = p.current_song() else {
                self.lyrics = None;
                self.lyrics_source = None;
                self.lyrics_for = None;
                return;
            };
            (
                song.id.clone(),
                lyrics::Query {
                    title: song.title.clone(),
                    artist: song.artist.clone(),
                    album: song.album.clone(),
                    duration: song.duration,
                },
                song.local_path.clone(),
            )
        };
        if self.lyrics_for.as_deref() == Some(id.as_str()) {
            return;
        }
        self.lyrics = None;
        self.lyrics_source = None;
        self.lyrics_for = Some(id.clone());
        // A new song's lines are a new list; whatever line number the last one
        // was on means nothing here.
        self.lyrics_followed = None;
        self.lyrics_prev = None;
        self.lyrics_scroll_anim = None;
        // A local file's id is the scanner's, not the server's — asking the
        // server about it can only ever be a miss, so those read their own file
        // instead.
        let server = local_path.is_none().then(|| self.client(cx)).flatten();
        if server.is_none() && local_path.is_none() && !online {
            self.lyrics_loading = false;
            cx.notify();
            return;
        }
        self.lyrics_loading = true;
        cx.spawn(async move |this, cx| {
            let by_id = id.clone();
            let found = runtime::spawn_io(async move {
                // A local file's own words: a sidecar .lrc beside it, or its
                // LYRICS tag. This is the library lookup for a file no server
                // has ever seen, so it is what the Library badge means here.
                // On the blocking pool — it opens and parses a file, and the
                // two IO workers must stay free for the requests below.
                if let Some(path) = local_path {
                    let docs = runtime::spawn_blocking_io(move || {
                        anyhow::Ok(lyrics::from_file(std::path::Path::new(&path)))
                    })
                    .await
                    .unwrap_or_default();
                    if let Some(doc) = best_lyrics(docs) {
                        return anyhow::Ok(Some((doc, LyricsSource::Library)));
                    }
                }
                if let Some(client) = server {
                    // getLyricsBySongId is what actually answers: Navidrome
                    // collects lyrics at scan time out of the file's tags and
                    // out of a sidecar .lrc, and only publishes them under the
                    // song's own id. Classic getLyrics searches the library by
                    // artist/title instead, which misses everything whose tags
                    // don't match what we sent — most of the library. It is
                    // kept as the fallback for servers without the songLyrics
                    // extension, which answer the by-id call with error 70.
                    let structured = client
                        .get_lyrics_by_song_id(&by_id)
                        .await
                        .unwrap_or_default();
                    if let Some(doc) = best_lyrics(structured) {
                        return anyhow::Ok(Some((doc, LyricsSource::Library)));
                    }
                    let classic = client
                        .get_lyrics(query.artist.as_deref(), Some(query.title.as_str()))
                        .await
                        .ok()
                        .and_then(|l| l.value)
                        .map(|text| vec![plain_lyrics(text)])
                        .and_then(best_lyrics);
                    if let Some(doc) = classic {
                        return anyhow::Ok(Some((doc, LyricsSource::Library)));
                    }
                }
                // Nothing in the library. LRCLIB has what was never tagged,
                // and answers with an LRC document when it has one, so this
                // fallback can come back better than the server's own copy.
                if online {
                    // A lookup that fails is not an answer — leave it uncached
                    // and let the next open try again.
                    let found = lyrics::fetch(query).await.ok().flatten();
                    return anyhow::Ok(found.map(|doc| (doc, LyricsSource::Online)));
                }
                anyhow::Ok(None)
            })
            .await
            .ok()
            .flatten();
            let _ = this.update(cx, |view, cx| {
                if view.lyrics_for.as_deref() == Some(id.as_str()) {
                    view.lyrics_loading = false;
                    let (doc, source) = found.unzip();
                    view.lyrics = doc;
                    view.lyrics_source = source;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// Queue list: click a row to jump to it. Sized by the caller — the panel
    /// sits beside the info card in a wide window and under it in a tall one.
    fn render_queue_panel(
        &self,
        width: f32,
        max_h: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (rows, current) = {
            let p = self.player.read(cx);
            let rows: Vec<(usize, String, String)> = p
                .queue
                .iter_ordered()
                .map(|(pos, s)| (pos, s.title.clone(), s.artist.clone().unwrap_or_default()))
                .collect();
            (rows, p.queue.current_pos())
        };
        let player = self.player.clone();
        // Drawn at a whole number of rows, so the panel never ends halfway
        // through one; `uniform_list` also virtualizes a long queue and is what
        // `sync_queue_scroll` steers to follow the playing track.
        let height = QUEUE_CHROME_H + queue_visible_rows(max_h, rows.len()) as f32 * QUEUE_ROW_H;
        let list = gpui::uniform_list(
            "fs-queue-list",
            rows.len(),
            move |range, _window, cx: &mut gpui::App| {
                range
                    .filter_map(|ix| rows.get(ix).cloned())
                    .map(|(pos, title, artist)| {
                        let is_current = current == Some(pos);
                        let player = player.clone();
                        h_flex()
                            .id(("fsq", pos))
                            // `uniform_list` sizes items to their content: a row
                            // that does not claim the width lands at whatever
                            // its own text needs.
                            .w_full()
                            .h(px(QUEUE_ROW_H))
                            .px_2()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .border_l_2()
                            .border_color(gpui::transparent_black())
                            .hover(|s| s.bg(cx.theme().muted.opacity(0.6)))
                            .when(is_current, |s| {
                                s.bg(cx.theme().primary.opacity(0.12))
                                    .border_color(cx.theme().primary)
                                    .text_color(cx.theme().primary)
                            })
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                player.update(cx, |p, cx| p.jump_to(pos, cx));
                                cx.stop_propagation();
                            })
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .truncate()
                                            .when(is_current, |s| s.font_medium())
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(artist),
                                    ),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0()
        .track_scroll(self.queue_scroll.clone());

        v_flex()
            .w(px(width))
            .flex_none()
            // Same card as the info column and the mini player, so an open
            // panel reads as part of the player rather than as a list dropped
            // onto the backdrop.
            .h(px(height))
            .p_4()
            .gap_2()
            .rounded_2xl()
            .bg(cx.theme().background.opacity(0.55))
            .border_1()
            .border_color(cx.theme().border.opacity(0.5))
            .shadow_xl()
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child("Queue"),
            )
            .child(list)
            .into_any_element()
    }

    /// Right-hand lyrics panel. A timed document is followed line by line —
    /// the one being sung is lit and the rest step back; an untimed one is the
    /// same list drawn flat, since there is no line to be on.
    ///
    /// The lines are the scrolling element's own children rather than a column
    /// inside it, because `sync_lyrics_scroll` places one of them by index and
    /// a `ScrollHandle` only records the bounds of what it is tracking
    /// directly.
    fn render_lyrics_panel(
        &self,
        width: f32,
        max_h: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        let body: Vec<gpui::AnyElement> = if self.lyrics_loading {
            vec![
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading…")
                    .into_any_element(),
            ]
        } else {
            match &self.lyrics {
                Some(doc) => doc
                    .lines
                    .iter()
                    .enumerate()
                    .map(|(ix, line)| {
                        let text = line.value.trim_end();
                        let is_active = self.lyrics_active == Some(ix);
                        // The line the highlight has just left, which runs the
                        // grow backwards. Without it the sheet only animates in
                        // one direction: the incoming line eases up over
                        // `LYRIC_GROW_MS` while the outgoing one is already back
                        // at its resting size on the very first frame of it,
                        // which is read as the new line shoving the old one
                        // rather than as the two trading places.
                        let is_prev = !is_active && self.lyrics_prev == Some(ix);
                        let emphasis = line_emphasis(self.lyrics_active, ix, doc.synced);
                        // Where each line comes to rest. The animated ones
                        // override it every frame; it is what they are heading
                        // for, and what they hold once they arrive.
                        let rest = if is_active {
                            LYRIC_REM_ACTIVE
                        } else {
                            LYRIC_REM
                        };
                        // Read out here rather than in the animators: those
                        // closures outlive the borrow of `cx`, and an `Hsla` is
                        // two words of `Copy`.
                        let lit = cx.theme().primary;
                        let unlit = cx.theme().foreground;
                        // Where a click on this line moves the playhead. The
                        // document's `offset` is applied here exactly as
                        // `active_line` applies it, or a click would land at a
                        // point that lights a *different* line — the correction
                        // has to be in the same sense on both sides of the round
                        // trip. A line with no start is inert: an untimed sheet
                        // has no points to seek to, and a pointer cursor over one
                        // promises something that cannot happen.
                        let seek_to = line.start.map(|start| lyric_seek_target(start, doc.offset));
                        let el = div()
                            .id(("fs-lyric", ix))
                            .when_some(seek_to, |this, to| {
                                this.cursor_pointer().on_click(cx.listener(
                                    move |this: &mut Self, _, _, cx| {
                                        // Notify rather than lean on the
                                        // per-frame repaint: that only runs while
                                        // audio is moving, and clicking a line of
                                        // a paused track has to light it too.
                                        this.player.update(cx, |p, cx| {
                                            p.seek(to);
                                            cx.notify();
                                        });
                                    },
                                ))
                            })
                            // Large and heavy throughout, the way a lyrics
                            // sheet is set: these are words to be read from
                            // across a room while something else plays, not a
                            // list to be scanned. The panel is widened to
                            // match — see `PANEL_LYRICS_MAX`. `BLACK` rather
                            // than `BOLD` because a face that stops at 700 is
                            // matched back to 700 anyway, so asking for the
                            // heaviest weight costs nothing where there is none
                            // and gets it wherever the font has one.
                            .font_weight(gpui::FontWeight::BLACK)
                            .line_height(relative(1.3))
                            .text_size(rems(rest))
                            // Laid out in the share of the panel that keeps the
                            // words breaking where they will break at full size
                            // — see `lyric_wrap`.
                            .w(relative(lyric_wrap(rest)))
                            // `primary` like every other playing-track mark —
                            // it is playback this follows, and under the
                            // Adaptive theme that is the cover's own colour.
                            .text_color(if is_active { lit } else { unlit })
                            .opacity(emphasis)
                            // An empty line is a gap the timings asked for, so
                            // it keeps its height instead of collapsing.
                            .child(if text.is_empty() { " " } else { text }.to_string());
                        // The two halves of the handover. Each animates under a
                        // wrapper id of its own — the line's index *and* which
                        // direction it is going — so the animation restarts as
                        // the highlight steps, where one shared id would run
                        // once per song and a single id per line would leave the
                        // shrink holding the grow's finished state. gpui drops
                        // the state behind an id that goes unrendered, so
                        // changing it is what rewinds the clock.
                        //
                        // Both grow/shrink and brighten/fade together: the size
                        // change costs the lines under it a few pixels of shift,
                        // which `sync_lyrics_scroll` takes back out on the next
                        // frame, and it is most of what makes the sheet read as
                        // following the song rather than as a list with one row
                        // coloured in.
                        let anim = |ms| {
                            Animation::new(crate::ui::transition(reduced_motion, ms))
                                .with_easing(gpui::ease_in_out)
                        };
                        if is_active {
                            el.with_animation(
                                ElementId::Name(format!("fs-lyric-in-{ix}").into()),
                                anim(LYRIC_GROW_MS),
                                move |el, t| {
                                    let size = LYRIC_REM + (LYRIC_REM_ACTIVE - LYRIC_REM) * t;
                                    el.text_size(rems(size))
                                        .w(relative(lyric_wrap(size)))
                                        // Into the accent over the same curve as
                                        // everything else, so the line is not
                                        // already the colour of a line being sung
                                        // while it is still on its way to being
                                        // one.
                                        .text_color(lyric_color(unlit, lit, t))
                                        .opacity(LYRIC_DIM[0] + (1. - LYRIC_DIM[0]) * t)
                                },
                            )
                            .into_any_element()
                        } else if is_prev {
                            el.with_animation(
                                ElementId::Name(format!("fs-lyric-out-{ix}").into()),
                                anim(LYRIC_GROW_MS),
                                move |el, t| {
                                    let size =
                                        LYRIC_REM_ACTIVE + (LYRIC_REM - LYRIC_REM_ACTIVE) * t;
                                    el.text_size(rems(size))
                                        .w(relative(lyric_wrap(size)))
                                        // And back out of it, so the two lines
                                        // cross through each other's colour
                                        // rather than swapping at a frame.
                                        .text_color(lyric_color(lit, unlit, t))
                                        // Down to whatever tier it has landed
                                        // on, not to a fixed one: the highlight
                                        // can step by more than a line after a
                                        // seek, and fading to the wrong tier
                                        // then snapping is the bug this whole
                                        // branch exists to avoid.
                                        .opacity(1. + (emphasis - 1.) * t)
                                },
                            )
                            .into_any_element()
                        } else {
                            el.into_any_element()
                        }
                    })
                    .collect(),
                None => vec![
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No lyrics found")
                        .into_any_element(),
                ],
            }
        };
        v_flex()
            .w(px(width))
            .flex_none()
            // Same card as the info column and the mini player, so an open
            // panel reads as part of the player rather than as a list dropped
            // onto the backdrop.
            .max_h(px(max_h))
            .p_4()
            .gap_2()
            .rounded_2xl()
            .bg(cx.theme().background.opacity(0.55))
            .border_1()
            .border_color(cx.theme().border.opacity(0.5))
            .shadow_xl()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child("Lyrics"),
                    )
                    // Only once something has landed: a badge beside "Loading…"
                    // or "No lyrics found" would be naming the source of
                    // nothing.
                    .when_some(self.lyrics_source, |this, source| {
                        this.child(
                            div()
                                .id("fs-lyrics-source")
                                .flex_none()
                                .px_1p5()
                                .rounded_sm()
                                .bg(cx.theme().muted)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(source.tooltip()).build(window, cx)
                                })
                                .child(source.badge()),
                        )
                    }),
            )
            .child(
                v_flex()
                    .id("fs-lyrics-scroll")
                    .track_scroll(&self.lyrics_scroll)
                    .flex_1()
                    .min_h_0()
                    // Wider gap than a list wants: at this size the lines need
                    // to read as separate lines rather than as a paragraph.
                    .gap_3()
                    .overflow_y_scroll()
                    .children(body),
            )
            .into_any_element()
    }

    /// The background layer for the chosen mode (behind the readability scrim).
    /// `vw`/`vh` are the window's size — the animated mode's blob field is laid
    /// out in pixels, so it needs them rather than percentages.
    fn render_background(
        &self,
        mode: FullscreenBackground,
        vw: f32,
        vh: f32,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let base = || div().absolute().left_0().top_0().size_full();
        let palette: Vec<gpui::Rgba> = self.gradient_palette.clone().unwrap_or_else(|| {
            vec![
                gpui::Rgba::from(cx.theme().muted),
                gpui::Rgba::from(cx.theme().background),
            ]
        });
        let top = palette[0];
        // Design continuity: every colour-driven background ends on the exact
        // tint the bottom player bar fades from, so the overlay and the bar
        // read as the same surface.
        let tint: gpui::Rgba =
            crate::ui::player_tint(self.session.read(cx).settings.theme, cx).into();
        match mode {
            FullscreenBackground::Solid => base().bg(cx.theme().background).into_any_element(),
            FullscreenBackground::Gradient => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.5), 0.),
                    linear_color_stop(tint, 1.),
                ))
                .into_any_element(),
            FullscreenBackground::Vibrant => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.9), 0.),
                    linear_color_stop(tint, 1.),
                ))
                .into_any_element(),
            FullscreenBackground::BlurredArt => base()
                .bg(cx.theme().muted)
                .overflow_hidden()
                // The tiny 32px art scaled to full size reads as a soft blur.
                // Square art in a landscape window would otherwise sit between
                // two bands of bare `muted` — cover the window and let the
                // parent's `overflow_hidden` clip what hangs over.
                .when_some(self.bg_art_path.clone(), |this, path| {
                    this.child(img(path).size_full().object_fit(ObjectFit::Cover))
                })
                .into_any_element(),
            FullscreenBackground::Animated => {
                // Boosted saturation and lifted a little, so mono covers still
                // show some spread and the blobs read as colour rather than as
                // shading. The player-bar tint is deliberately *not* one of
                // them: it is near black, and a near-black blob over a
                // near-black base is a hole, not a colour.
                let ring: Vec<gpui::Rgba> = {
                    let r: Vec<gpui::Rgba> = palette
                        .iter()
                        .map(|&c| scale_rgb(vivid(c, 1.6), 1.15))
                        .collect();
                    if r.len() < 2 {
                        let b = *r
                            .first()
                            .unwrap_or(&gpui::Rgba::from(cx.theme().background));
                        vec![b, scale_rgb(b, 1.5), scale_rgb(b, 0.6)]
                    } else {
                        r
                    }
                };
                let short = vw.min(vh);
                base()
                    .bg(blob_base(&ring, tint))
                    .overflow_hidden()
                    .children(
                        blob_field(vw, vh, &ring)
                            .into_iter()
                            .enumerate()
                            .map(|(i, b)| render_blob(i, b, vw, vh, short)),
                    )
                    .into_any_element()
            }
        }
    }

    fn fetch_art(&mut self, cover_id: String, key: String, cx: &mut Context<Self>) {
        // Local track: resolve cover from local_art_path directly. Local ids
        // are 16-hex hashes; server ids (`al-/ar-/mf-…`) never collide, so
        // this resolves local tracks even when a server is connected.
        if let Some(path) =
            crate::services::local_library::local_art_path(&cover_id).filter(|p| p.exists())
        {
            self.art_path = Some(path.clone());
            self.bg_art_path = Some(path.clone());
            self.gradient_palette = extract_palette(&path);
            cx.notify();
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let client2 = client.clone();
        let cover_id2 = cover_id.clone();
        let key2 = key.clone();
        // Full-res for the center art card.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch_as(client, cover_id, key, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
        // Tiny version for color extraction.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch_as(client2, cover_id2, key2, BG_ART_SIZE).await {
                let colors = extract_palette(&path);
                let _ = this.update(cx, |view, cx| {
                    view.bg_art_path = Some(path);
                    view.gradient_palette = colors;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

// --- Animated background: blurred blob field --------------------------------
// The animated mode used to be two translucent linear gradients at crossing
// angles, cycling through the palette. A ramp between an album's (usually
// low-variance) colours has no shape to it, so however the phases moved it read
// as one diagonal wash sliding across the window. The reference look is a mesh
// gradient: a field of soft-edged ellipses in the cover's colours, each
// drifting and breathing on its own clock, overlapping into a field with no
// visible band lines at all.
//
// gpui exposes no blur filter, so a blob is an *empty* rounded element whose
// only paint is a `BoxShadow` — gpui blurs shadows with a real gaussian in the
// shader (four samples, analytic along x, so the per-pixel cost is fixed), and
// with no background set the blurred shadow is all that lands.
//
// Cost is the one thing to watch: the shader's quad is dilated by 3× the blur
// radius on every side, so a blob costs `(size + 6·blur)²` pixels. Blur is
// therefore a *ratio of the blob* (`BLOB_BLUR`) rather than a constant, and the
// grid is sized so the blobs overlap about twice over — enough that the base
// colour never shows through as a hole, without paying for a third layer.

/// Blur radius as a fraction of a blob's mean diameter. Higher merges the field
/// into one soft wash but squares up the fill cost fast (see above).
const BLOB_BLUR: f32 = 0.12;
/// How far a blob wanders from its cell, as a fraction of the window's short
/// side.
const BLOB_DRIFT: (f32, f32) = (0.04, 0.10);
/// Peak size swing of the breathing cycle, either side of the base radius.
const BLOB_BREATH: f32 = 0.16;
/// Seconds for one full drift/breath cycle. Every blob gets its own, so the
/// field never pulses in lockstep.
const BLOB_PERIOD: (f32, f32) = (26., 46.);
/// Rows of the jittered grid the blobs are placed on; columns follow the
/// window's aspect so the cells stay roughly square.
const BLOB_ROWS: usize = 4;
/// The grid spills past the window on every side — a blob centred on the edge
/// is what keeps the corners from falling back to the base colour.
const BLOB_BLEED: f32 = 0.12;

/// One blob of the animated background's field.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Blob {
    /// Centre in window fractions. May fall outside `0..1` (see `BLOB_BLEED`).
    cx: f32,
    cy: f32,
    /// Half-extents as a fraction of the window's short side. Kept near-square:
    /// gpui clamps a corner radius to half the shorter side, so `rounded_full`
    /// on a lopsided quad is a stadium, not an ellipse.
    rx: f32,
    ry: f32,
    color: gpui::Rgba,
    alpha: f32,
    /// Drift amplitude, fraction of the short side.
    drift: f32,
    /// Seconds for one full cycle.
    period: f32,
    /// Whole cycles per period for the x drift, y drift and breath. Integers so
    /// every term is periodic in the animation's 0→1 delta and the repeat is
    /// seamless; `fx != fy` so the path is a Lissajous wander, not a line.
    fx: f32,
    fy: f32,
    fb: f32,
    /// Phase of each of those three cycles, in turns.
    px: f32,
    py: f32,
    pb: f32,
}

/// Deterministic `0..1` from an integer. A field reshuffled per frame would
/// boil, and one seeded from the clock could not be tested.
fn hash01(seed: u32) -> f32 {
    let mut x = seed.wrapping_add(0x9E37_79B9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85EB_CA6B);
    x = (x ^ (x >> 13)).wrapping_mul(0xC2B2_AE35);
    x ^= x >> 16;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// The flat colour the blobs composite over. Mostly a darkened mean of the
/// field's own colours, pulled a little toward the player-bar tint for design
/// continuity with the bar: the tint alone is near black, and every gap between
/// blobs then read as a dark blotch rather than as part of the gradient.
fn blob_base(colors: &[gpui::Rgba], tint: gpui::Rgba) -> gpui::Rgba {
    if colors.is_empty() {
        return tint;
    }
    let n = colors.len() as f32;
    let mean = gpui::Rgba {
        r: colors.iter().map(|c| c.r).sum::<f32>() / n,
        g: colors.iter().map(|c| c.g).sum::<f32>() / n,
        b: colors.iter().map(|c| c.b).sum::<f32>() / n,
        a: 1.0,
    };
    let floor = scale_rgb(mean, 0.55);
    gpui::Rgba {
        r: tint.r * 0.35 + floor.r * 0.65,
        g: tint.g * 0.35 + floor.g * 0.65,
        b: tint.b * 0.35 + floor.b * 0.65,
        a: 1.0,
    }
}

/// Lay the animated background's blobs out for a window of `vw`×`vh`, drawing
/// colours from `colors` in order so neighbouring blobs differ.
fn blob_field(vw: f32, vh: f32, colors: &[gpui::Rgba]) -> Vec<Blob> {
    if colors.is_empty() || vw <= 0. || vh <= 0. {
        return Vec::new();
    }
    let aspect = (vw / vh).clamp(0.4, 3.0);
    let rows = BLOB_ROWS;
    let cols = ((rows as f32 * aspect).round() as usize).clamp(3, 8);
    let short = vw.min(vh);
    let span = 1. + 2. * BLOB_BLEED;
    let (cw, ch) = (span / cols as f32, span / rows as f32);
    let mut blobs = Vec::with_capacity(cols * rows);
    // The colour walk always advances by at least one, so no two blobs laid
    // down in a row share a hue — a repeat between neighbours merges them into
    // one shapeless patch. Picking at random instead clusters and does exactly
    // that.
    let mut hue = 0usize;
    for r in 0..rows {
        for c in 0..cols {
            let i = (r * cols + c) as u32;
            let s = |k: u32| hash01(i.wrapping_mul(97).wrapping_add(k));
            // Jittered inside the middle half of its cell rather than placed
            // freely: blobs dropped at random leave bald patches, and even a
            // full-cell jitter can pull two neighbours far enough apart to open
            // one — which reads as a gradient with a hole in it, not a field.
            let cx = -BLOB_BLEED + cw * (c as f32 + 0.25 + 0.5 * s(1));
            let cy = -BLOB_BLEED + ch * (r as f32 + 0.25 + 0.5 * s(2));
            // Radius in short-side fractions. Wider than the cell, so
            // neighbours overlap before their blur has even started.
            let base = cw * vw / short * 0.60;
            // Integer cycle counts; fy is nudged off fx so the two never agree.
            let fx = 1. + (s(7) * 2.).floor();
            let fy = {
                let f = 1. + (s(8) * 3.).floor();
                if f == fx { f + 1. } else { f }
            };
            hue = (hue + 1 + (s(5) * (colors.len() - 1) as f32) as usize) % colors.len();
            blobs.push(Blob {
                cx,
                cy,
                rx: base * (0.85 + 0.45 * s(3)),
                ry: base * (0.85 + 0.45 * s(4)),
                color: colors[hue],
                // High enough that a single-covered patch still reads as its
                // own colour: at a low alpha only the overlaps came up to the
                // palette and everything between them sank toward the base.
                alpha: 0.6 + 0.3 * s(6),
                drift: BLOB_DRIFT.0 + (BLOB_DRIFT.1 - BLOB_DRIFT.0) * s(9),
                period: BLOB_PERIOD.0 + (BLOB_PERIOD.1 - BLOB_PERIOD.0) * s(10),
                fx,
                fy,
                fb: 2. + (s(11) * 2.).floor(),
                px: s(12),
                py: s(13),
                pb: s(14),
            });
        }
    }
    blobs
}

/// One blob as an animated element: an empty rounded quad carrying a single
/// blurred shadow, moved and resized by its own repeating cycle.
fn render_blob(i: usize, b: Blob, vw: f32, vh: f32, short: f32) -> impl IntoElement {
    let (bw, bh) = (b.rx * short, b.ry * short);
    let (ox, oy) = (b.cx * vw, b.cy * vh);
    div().absolute().rounded_full().with_animation(
        ("fs-blob", i),
        Animation::new(Duration::from_secs_f32(b.period)).repeat(),
        move |this, delta| {
            let turn =
                |cycles: f32, phase: f32| (std::f32::consts::TAU * (cycles * delta + phase)).sin();
            let dx = b.drift * short * turn(b.fx, b.px);
            let dy = b.drift * short * turn(b.fy, b.py);
            let breath = 1. + BLOB_BREATH * turn(b.fb, b.pb);
            let (w, h) = (bw * breath, bh * breath);
            this.left(px(ox + dx - w))
                .top(px(oy + dy - h))
                .w(px(w * 2.))
                .h(px(h * 2.))
                .shadow(vec![gpui::BoxShadow {
                    color: gpui::Rgba {
                        a: b.alpha,
                        ..b.color
                    }
                    .into(),
                    offset: gpui::point(px(0.), px(0.)),
                    blur_radius: px((w + h) * BLOB_BLUR),
                    spread_radius: px(0.),
                }])
        },
    )
}

/// Average colour of each horizontal band, top→bottom. More bands = richer
/// palette for the animated gradient to sweep through. Empty bands are skipped.
fn extract_palette(path: &std::path::Path) -> Option<Vec<gpui::Rgba>> {
    const BANDS: usize = 5;
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgb8();
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return None;
    }
    let mut acc = [[0u64; 3]; BANDS];
    let mut n = [0u64; BANDS];
    for (_, y, pixel) in img.enumerate_pixels() {
        let band = ((y as u64 * BANDS as u64) / h as u64).min(BANDS as u64 - 1) as usize;
        acc[band][0] += pixel[0] as u64;
        acc[band][1] += pixel[1] as u64;
        acc[band][2] += pixel[2] as u64;
        n[band] += 1;
    }
    // Raw averages; the render darkens/brightens per background mode.
    let palette: Vec<gpui::Rgba> = (0..BANDS)
        .filter(|&i| n[i] > 0)
        .map(|i| gpui::Rgba {
            r: acc[i][0] as f32 / n[i] as f32 / 255.0,
            g: acc[i][1] as f32 / n[i] as f32 / 255.0,
            b: acc[i][2] as f32 / n[i] as f32 / 255.0,
            a: 1.0,
        })
        .collect();
    if palette.is_empty() {
        return None;
    }
    Some(palette)
}

/// Scale an RGB colour's brightness (keeps alpha opaque).
fn scale_rgb(c: gpui::Rgba, f: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: (c.r * f).clamp(0., 1.),
        g: (c.g * f).clamp(0., 1.),
        b: (c.b * f).clamp(0., 1.),
        a: 1.0,
    }
}

/// Push a colour's channels away from their mean to boost saturation.
fn vivid(c: gpui::Rgba, amt: f32) -> gpui::Rgba {
    let mean = (c.r + c.g + c.b) / 3.0;
    gpui::Rgba {
        r: (mean + (c.r - mean) * amt).clamp(0., 1.),
        g: (mean + (c.g - mean) * amt).clamp(0., 1.),
        b: (mean + (c.b - mean) * amt).clamp(0., 1.),
        a: 1.0,
    }
}

impl Render for FullscreenPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            title,
            artist,
            album,
            position,
            duration,
            playing,
            buffering,
            has_track,
            is_radio,
            shuffle,
            repeat,
        ) = {
            let p = self.player.read(cx);
            let np = p.now_playing();
            let album = p.current_song().and_then(|s| s.album.clone());
            (
                np.as_ref().map(|(t, _)| t.clone()),
                np.as_ref().map(|(_, a)| a.clone()),
                album,
                p.position,
                p.duration,
                p.playing,
                p.buffering,
                np.is_some(),
                p.is_radio(),
                p.queue.shuffle,
                p.queue.repeat,
            )
        };

        // Sync seek slider.
        if let Some(total) = duration
            && total > Duration::ZERO
        {
            let fraction = (position.as_secs_f32() / total.as_secs_f32()).clamp(0., 1.);
            self.seek
                .update(cx, |s, cx| s.set_value(fraction, window, cx));
        }

        let seek_fraction = match duration {
            Some(total) if total > Duration::ZERO => {
                (position.as_secs_f32() / total.as_secs_f32()).clamp(0., 1.)
            }
            _ => 0.,
        };
        let waveform_enabled = self.session.read(cx).settings.waveform_seekbar;
        let show_volume = self.session.read(cx).settings.fullscreen_volume;
        let detailed_volume = self.session.read(cx).settings.detailed_volume;
        // The handle's own position, not the amplitude: the slider is tapered,
        // and a readout in amplitude would disagree with where the handle sits.
        let volume_level = crate::ui::volume_position(self.player.read(cx).volume);
        // Keep it in step with changes made elsewhere (player bar, media keys).
        self.volume
            .update(cx, |s, cx| s.set_value(volume_level, window, cx));
        let replay_gain = self.player.read(cx).replay_gain_active();
        let stream_info = if is_radio {
            crate::ui::radio_info_line(self.player.read(cx), &self.session.read(cx).settings)
        } else {
            crate::ui::stream_info_line(self.player.read(cx), &self.session.read(cx).settings)
        };

        let time_now = format_duration(position);
        let time_total = duration
            .map(format_duration)
            .unwrap_or_else(|| "-:--".into());

        let bg_mode = self.session.read(cx).settings.fullscreen_bg;
        // Mean perceptual luminance of the current palette (0 = black, 1 = white).
        // Bright covers wash out the grey text, so scale the dark scrim up with it.
        let luma = self
            .gradient_palette
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|p| {
                p.iter()
                    .map(|c| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b)
                    .sum::<f32>()
                    / p.len() as f32
            })
            .unwrap_or(0.0);
        // Dark scrim opacity for readability, tuned per background mode. The color-
        // dependent modes add a luminance term so bright palettes stay legible.
        let scrim = match bg_mode {
            FullscreenBackground::Solid => 0.0,
            FullscreenBackground::Gradient => 0.4 + 0.3 * luma,
            FullscreenBackground::Vibrant => 0.18 + 0.4 * luma,
            FullscreenBackground::BlurredArt => 0.55,
            // The blob field carries more of the palette than the gradient it
            // replaced, so it needs a touch more scrim under the text.
            FullscreenBackground::Animated => 0.35 + 0.4 * luma,
        };
        // The visualizer draws light geometry on the theme background, so the
        // per-mode scrim (tuned for album-art backdrops) would only mute it.
        // Keep a thin one — enough to seat the text over moving geometry.
        let viz_mode = self.session.read(cx).settings.visualizer;
        let scrim = if viz_mode.is_on() { 0.15 } else { scrim };
        let (mini_title, mini_artist) = (title.clone(), artist.clone());
        let (mini_time_now, mini_time_total) = (time_now.clone(), time_total.clone());
        // Everything below sizes itself off the window: the overlay has no
        // scrollbar of its own beyond the fallback wrapper, so a fixed layout
        // is content lost off the edge on any window smaller than the one it
        // was drawn for.
        let viewport = window.viewport_size();
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));

        // --- Side panel open/close ----------------------------------------
        // The travel is state rather than a `with_animation` wrapper, so it
        // plays in both directions: an element dropped from the tree on close
        // animates nothing. `panel_drawn` is what is on screen, which lags
        // `panel` by the exit — and it is what the layout is sized from, or
        // the cover would grow into the panel's room while the panel is still
        // in it.
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        // Switching Queue <-> Lyrics is a swap, not a re-open: the outgoing
        // panel leaves first and the arriving one then plays its own entrance.
        // Replacing the contents where they stand instead cuts the outgoing
        // panel off mid-travel, which reads as a flicker rather than a swap.
        if matches!((self.panel, self.panel_drawn), (Some(p), Some(d)) if p != d) {
            self.panel_reveal.set(false, reduced_motion);
            if self.panel_reveal.openness(reduced_motion) <= 0. {
                self.panel_drawn = self.panel;
                self.panel_reveal.replay();
            }
        } else {
            if self.panel.is_some() {
                self.panel_drawn = self.panel;
            }
            self.panel_reveal.set(self.panel.is_some(), reduced_motion);
        }
        let panel_open = self.panel_reveal.openness(reduced_motion);
        let drawn_panel = self
            .panel_reveal
            .visible(reduced_motion)
            .then_some(self.panel_drawn)
            .flatten();
        if self.panel_reveal.settling(reduced_motion) {
            window.request_animation_frame();
        }

        let layout = Layout::resolve(
            vw,
            vh,
            drawn_panel,
            show_volume && !is_radio,
            self.session.read(cx).settings.fullscreen_cover,
        );
        // Follow the playing track in the queue panel, now that the room the
        // panel gets — and so how many rows it holds — is known.
        self.sync_queue_scroll(layout.panel_max_h, cx);
        // And the line being sung in the lyrics panel, which the position tick
        // brings us back here for twice a second.
        self.sync_lyrics_scroll(cx);
        // Labelled toggles stick out of a narrow card; the icons carry the
        // meaning on their own, and tooltips are not the point here.
        let toggle_labels = layout.card >= TOGGLE_LABEL_MIN;

        // --- Open/close transition ---------------------------------------
        // One clock, read straight from state, so the element tree keeps the
        // same shape (and the same ids) in both directions. Ask for another
        // frame until it lands; gpui only redraws on demand.
        let closing = self.closing_at.is_some();
        let elapsed = self.closing_at.unwrap_or(self.opened_at).elapsed();
        let span = if closing { EXIT } else { ENTER };
        let raw = elapsed.as_secs_f32() / span.as_secs_f32();
        if raw < 1.0 {
            window.request_animation_frame();
        }
        let t = raw.clamp(0.0, 1.0);

        // Visualizer runs off the same on-demand redraw: one FFT + scene step
        // per frame, and a standing request for the next one while a scene is
        // up. Off costs nothing — no tick, no frame request, no canvas.
        if viz_mode.is_on() && !closing {
            let tuning = self.session.read(cx).settings.visualizer_tuning;
            self.visualizer.tick(viz_mode, tuning);
            window.request_animation_frame();
        }

        // The lit line is placed from the playback clock rather than from the
        // engine's tick, which only pays off if the overlay is redrawn at
        // something better than that tick's rate. Nothing is recomputed by the
        // request itself — `sync_lyrics_scroll` above is the whole cost.
        if !closing && self.lyrics_following(cx) {
            window.request_animation_frame();
        }

        // gpui has no transform, so the zoom is four animated insets on an
        // otherwise unsized absolute element; the extra vertical term slides
        // the overlay up from / down toward the player bar. Opacity and
        // geometry run on separate curves — flat on one curve reads as a fade,
        // not as depth.
        let (fade, inset, shift) = if closing {
            // Motion starts immediately (linear term) and accelerates away;
            // a pure ease-in stalls for the first ~80ms and reads as lag.
            let motion = 0.35 * t + 0.65 * t * t;
            // Opacity holds through the first half so the drop is actually
            // seen — fade it out early and the exit degenerates into a
            // cross-fade, which is what the travel distance is for.
            (1.0 - t * t, motion * 26., motion * 64.)
        } else {
            // Opacity lands at ~60% of the timeline so the overlay is solid
            // while the geometry is still easing. `out_back` overshoots, so
            // the insets go a hair negative at the end and settle back.
            let motion = out_back(t);
            (
                out_cubic(phase(t, 0.0, 0.6)),
                (1.0 - motion) * 34.,
                (1.0 - motion) * 46.,
            )
        };
        // Positive top / negative bottom offsets the whole overlay downward:
        // the entrance shrinks that offset to zero (rises into place), the exit
        // grows it from zero (drops away).
        let (top_inset, bottom_inset) = (inset + shift, inset - shift);
        // Content lags the backdrop on the way in and is inert on the way out.
        let (content_fade, content_rise) = if closing {
            (1.0, 0.)
        } else {
            (
                out_cubic(phase(t, 0.10, 0.75)),
                (1.0 - out_cubic(phase(t, 0.06, 1.0))) * 22.,
            )
        };

        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .large()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        // Floating mini player: while a scene runs it replaces the big cover +
        // info column, so the visualizer gets the whole window and the controls
        // sit on top of it in one compact card — including the scene picker,
        // which is otherwise buried in the cycle button that just got hidden.
        // Built before the mini player's own closures: they borrow `cx`
        // immutably for their listeners, and the card needs it mutably.
        // Same clock as the side panel, for the same reason: dropped from the
        // tree on close it would pop out where it slid in.
        self.viz_tuning_reveal
            .set(viz_mode.is_on() && self.viz_tuning_open, reduced_motion);
        let tuning_t = self.viz_tuning_reveal.openness(reduced_motion);
        if self.viz_tuning_reveal.settling(reduced_motion) {
            window.request_animation_frame();
        }
        let tuning_card = self
            .viz_tuning_reveal
            .visible(reduced_motion)
            .then(|| self.viz_tuning_card(viz_mode, cx).into_any_element());

        let mini_player = viz_mode.is_on().then(|| {
            let mode_btn = |mode: VisualizerMode, current: VisualizerMode| {
                let id = match mode {
                    VisualizerMode::Off => "mini-viz-off",
                    _ => "mini-viz-auto",
                };
                Button::new(id)
                    .ghost()
                    .small()
                    .label(mode.menu_label())
                    .when(mode == current, |b| b.primary())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_visualizer(mode, cx);
                        cx.stop_propagation();
                    }))
            };
            // Scenes live behind one button: five of them in the row crowded
            // the card and buried Off and Auto, which are not scenes at all.
            // The button shows the running scene, so the row still reads as
            // the current state at a glance.
            let scene_trigger = Button::new("mini-viz-more")
                .ghost()
                .small()
                .icon(Icon::new(IconName::ChevronDown).xsmall())
                .label(pinned_scene_label(viz_mode).unwrap_or("More"))
                .when(pinned_scene_label(viz_mode).is_some(), |b| b.primary());

            div()
                .absolute()
                .bottom(px(36.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    v_flex()
                        // Never wider than the window it floats over.
                        .w(px(MINI_W.min(vw - 2. * EDGE).max(240.)))
                        .max_w_full()
                        // The card hangs off the right edge as an absolute
                        // child rather than sitting in the flow: in the flow it
                        // would drag the mini player off centre whenever it
                        // opened, and the mini player is the thing that has to
                        // stay put.
                        .relative()
                        .when_some(tuning_card, |this, card| {
                            // Beside the mini player where the window is wide
                            // enough for it; directly above it otherwise, since
                            // off to the right of a narrow window is off-screen.
                            let beside = vw >= TUNING_SIDE_MIN_W;
                            this.child(
                                div()
                                    .absolute()
                                    .map(|this| {
                                        // Slides in from whichever edge it
                                        // hangs off, so the motion reads as the
                                        // card coming out of the mini player.
                                        let slide = px(12. + 14. * (1. - tuning_t));
                                        if beside {
                                            this.left(gpui::relative(1.)).ml(slide).bottom_0()
                                        } else {
                                            this.left_0().bottom(gpui::relative(1.)).mb(slide)
                                        }
                                    })
                                    .opacity(tuning_t)
                                    .child(card),
                            )
                        })
                        .child(
                            v_flex()
                                .w_full()
                                .gap_3()
                                .px_5()
                                .py_4()
                                .rounded_2xl()
                                .shadow_xl()
                                // Translucent so the scene keeps running behind it,
                                // opaque enough to read against moving geometry.
                                .bg(cx.theme().background.opacity(0.72))
                                .border_1()
                                .border_color(cx.theme().border.opacity(0.6))
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(
                                            div()
                                                .size(px(56.))
                                                .flex_none()
                                                .rounded_lg()
                                                .bg(cx.theme().muted)
                                                .overflow_hidden()
                                                .when_some(
                                                    self.art_path.clone().filter(|_| !is_radio),
                                                    |this, path| {
                                                        this.child(
                                                            img(path).size(px(56.)).rounded_lg(),
                                                        )
                                                    },
                                                )
                                                .when(is_radio, |this| {
                                                    this.flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_color(cx.theme().primary)
                                                        .child(app_icon(icons::RADIO))
                                                }),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_0p5()
                                                .child(
                                                    div()
                                                        .font_semibold()
                                                        .truncate()
                                                        .when(!has_track, |s: gpui::Div| {
                                                            s.text_color(
                                                                cx.theme().muted_foreground,
                                                            )
                                                        })
                                                        .child(mini_title.unwrap_or_else(|| {
                                                            "Nothing playing".into()
                                                        })),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .truncate()
                                                        .child(mini_artist.unwrap_or_default()),
                                                ),
                                        )
                                        .child(
                                            h_flex()
                                                .flex_none()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    icon_btn("mini-prev", icons::SKIP_BACK, false)
                                                        .disabled(!has_track || is_radio)
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.player
                                                                .update(cx, |p, cx| p.previous(cx));
                                                            cx.stop_propagation();
                                                        })),
                                                )
                                                .child(
                                                    Button::new("mini-play")
                                                        .primary()
                                                        .icon(if playing {
                                                            app_icon(icons::PAUSE)
                                                        } else {
                                                            app_icon(icons::PLAY)
                                                        })
                                                        .loading(buffering)
                                                        .disabled(!has_track)
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.player.update(cx, |p, cx| {
                                                                p.toggle_play(cx)
                                                            });
                                                            cx.stop_propagation();
                                                        })),
                                                )
                                                .child(
                                                    icon_btn(
                                                        "mini-next",
                                                        icons::SKIP_FORWARD,
                                                        false,
                                                    )
                                                    .disabled(!has_track || is_radio)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.player.update(cx, |p, cx| p.next(cx));
                                                        cx.stop_propagation();
                                                    })),
                                                ),
                                        ),
                                )
                                // Seek row — live radio has nothing to seek.
                                .when(!is_radio, |this| {
                                    this.child(
                                        h_flex()
                                            .w_full()
                                            .gap_3()
                                            .items_center()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(mini_time_now),
                                            )
                                            .map(|this| {
                                                let bar =
                                                    match (waveform_enabled, self.waveform.clone())
                                                    {
                                                        (true, Some(peaks)) => {
                                                            crate::ui::waveform_seek_bar(
                                                                &peaks,
                                                                seek_fraction,
                                                                22.,
                                                                cx.theme().primary,
                                                                cx.theme()
                                                                    .muted_foreground
                                                                    .opacity(0.35),
                                                                self.player.clone(),
                                                            )
                                                        }
                                                        _ => div()
                                                            .flex_1()
                                                            .child(Slider::new(&self.seek))
                                                            .into_any_element(),
                                                    };
                                                let view = cx.entity();
                                                this.child(crate::ui::seek_hover_wrap(
                                                    "fs-mini-seek-hover",
                                                    self.seek_hover,
                                                    duration,
                                                    bar,
                                                    move |fraction, cx| {
                                                        view.update(cx, |p: &mut Self, cx| {
                                                            if p.seek_hover != fraction {
                                                                p.seek_hover = fraction;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                    cx,
                                                ))
                                            })
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(mini_time_total),
                                            ),
                                    )
                                })
                                .when(is_radio, |this| {
                                    this.child(h_flex().w_full().justify_center().child(
                                        crate::ui::live_badge(
                                            "mini-live",
                                            cx.theme().primary,
                                            Some(position),
                                            cx,
                                        ),
                                    ))
                                })
                                // Scene picker.
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .justify_center()
                                        .pt_1()
                                        .border_t_1()
                                        .border_color(cx.theme().border.opacity(0.4))
                                        .child(mode_btn(VisualizerMode::Off, viz_mode))
                                        .child(mode_btn(VisualizerMode::Auto, viz_mode))
                                        .child(self.scene_menu(scene_trigger, viz_mode))
                                        .child(
                                            Button::new("mini-viz-tune")
                                                .ghost()
                                                .small()
                                                .icon(Icon::new(IconName::Settings2).xsmall())
                                                .when(self.viz_tuning_open, |b| b.primary())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.viz_tuning_open = !this.viz_tuning_open;
                                                    cx.notify();
                                                    cx.stop_propagation();
                                                })),
                                        ),
                                ),
                        ),
                )
        });

        // No `size_full`: an absolute element with all four insets set and no
        // explicit size stretches between them, which is what lets the insets
        // act as a zoom.
        div()
            .absolute()
            .left(px(inset))
            .right(px(inset))
            .top(px(top_inset))
            .bottom(px(bottom_inset))
            .opacity(fade)
            // Swallow mouse events so clicks don't fall through to the UI below.
            .occlude()
            .bg(cx.theme().background)
            // Visualizer takes over the backdrop when running: it needs the
            // whole window to read as 3D, and the scrim + content still sit
            // on top of it.
            .child(if viz_mode.is_on() {
                self.visualizer.render(cx.theme().primary)
            } else {
                self.render_background(bg_mode, vw, vh, cx)
            })
            // Readability scrim.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full()
                    .bg(cx.theme().background)
                    .opacity(scrim),
            )
            // Close button — clearly labelled pill, top right.
            .child(
                div()
                    .absolute()
                    .top_4()
                    .right_4()
                    .rounded_full()
                    .bg(cx.theme().muted.opacity(0.55))
                    .child(
                        Button::new("fs-close")
                            .ghost()
                            // Pill rounding to match the wrapper: the button's
                            // default (theme radius) hover fill is squarer than
                            // the rounded-full backdrop, so the shape appeared
                            // to change on hover.
                            .rounded(px(9999.))
                            .icon(Icon::new(IconName::ChevronDown))
                            .label("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.begin_close(cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            // Main content: album art beside info + controls, optional side
            // panel + vertical volume at the far right — or, in a window taller
            // than it is wide, the same pieces stacked in one column.
            // Stood down while a scene runs — the visualizer wants the whole
            // window, and the mini player carries the controls instead.
            .when(!viz_mode.is_on(), |root| {
                root.child({
                    let art_size = layout.art;
                    let content = div()
                        .flex()
                        .map(|this| {
                            if layout.stacked {
                                this.flex_col()
                            } else {
                                this.flex_row()
                            }
                        })
                        .w_full()
                        // A minimum height rather than a full one: inside the
                        // scrolling wrapper below, a box fixed to the window's
                        // height would centre content that overruns it and clip
                        // the top out of reach, while this one grows past the
                        // window and scrolls. It has to be an absolute height —
                        // a percentage resolves against the scroll container's
                        // *content*, which is this element, so it collapses to
                        // the content height and the centring is lost.
                        .min_h(px(vh))
                        .items_center()
                        .justify_center()
                        .gap_8()
                        .px_10()
                        // Vertical padding comes from the layout: it is part of
                        // the height budget the cover is sized against, and in
                        // the stacked layout the top also has to clear the close
                        // pill the cover would otherwise run into.
                        .pt(px(layout.pad_top))
                        .pb(px(layout.pad_bottom))
                        // Album art. Dropped outright in a window too short to
                        // give it any meaningful size — the controls are the
                        // part that has to stay usable.
                        .when(art_size > 0., |this| {
                            this.child(
                                div()
                                    .size(px(art_size))
                                    .flex_none()
                                    .rounded_2xl()
                                    .bg(cx.theme().muted)
                                    .overflow_hidden()
                                    .shadow_xl()
                                    .when_some(
                                        self.art_path.clone().filter(|_| !is_radio),
                                        |this, path| {
                                            this.child(img(path).size(px(art_size)).rounded_2xl())
                                        },
                                    )
                                    // A station has no artwork; mark the slot as
                                    // radio rather than leaving a blank square the
                                    // size of a record sleeve.
                                    .when(is_radio, |this| {
                                        this.flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(cx.theme().primary)
                                            .child(
                                                app_icon(icons::RADIO)
                                                    .with_size(px(art_size * 0.28)),
                                            )
                                    }),
                            )
                        })
                        // Info + controls column — right of the cover. Same
                        // card treatment as the mini player and the same
                        // internal order (info, seek, transport, then a ruled
                        // row of toggles), so the three players read as one
                        // design at three sizes rather than three designs.
                        .child(
                            v_flex()
                                .flex_none()
                                .w(px(layout.card))
                                .justify_center()
                                // Tighter in a short window: the card is the one
                                // piece with a floor of its own, so its padding
                                // and row gaps are part of what has to give.
                                .gap(px(layout.density.card_gap()))
                                .p(px(layout.density.card_pad()))
                                .rounded_2xl()
                                .bg(cx.theme().background.opacity(0.55))
                                .border_1()
                                .border_color(cx.theme().border.opacity(0.5))
                                .shadow_xl()
                                // Track info.
                                .child(
                                    v_flex()
                                        // Bounded by the card, not by a wider
                                        // guess: a max wider than the card
                                        // lets `truncate` clip at the border
                                        // with no ellipsis, which reads as a
                                        // rendering fault rather than a long
                                        // title (radio titles are long).
                                        .w_full()
                                        .gap_1()
                                        // Scrolls when it does not fit the
                                        // card, rather than being cut off.
                                        .child(crate::ui::scrolling_line(
                                            "fs-title-text",
                                            title
                                                .clone()
                                                .unwrap_or_else(|| "Nothing playing".into())
                                                .into(),
                                            px(layout.card - CARD_TEXT_PAD),
                                            window.rem_size() * 1.875,
                                            gpui::FontWeight::SEMIBOLD,
                                            (!has_track).then(|| cx.theme().muted_foreground),
                                            window,
                                        ))
                                        .child(
                                            div()
                                                .text_lg()
                                                .text_color(cx.theme().muted_foreground)
                                                .truncate()
                                                .child(artist.unwrap_or_default()),
                                        )
                                        // Album line and the stream-info line
                                        // below are what a short window sheds
                                        // first: the title and artist name the
                                        // track, and the player bar still
                                        // carries both lines in full.
                                        .when_some(
                                            album.filter(|_| layout.density.secondary_lines()),
                                            |this, alb| {
                                                this.child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .truncate()
                                                        .child(alb),
                                                )
                                            },
                                        ),
                                )
                                // Seek bar.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .max_w(px(640.))
                                        .gap_3()
                                        .items_center()
                                        .when(is_radio, |this| {
                                            this.justify_center().child(crate::ui::live_badge(
                                                "fs-live",
                                                cx.theme().primary,
                                                Some(position),
                                                cx,
                                            ))
                                        })
                                        .when(!is_radio, |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(time_now),
                                            )
                                            .map(|this| {
                                                let bar =
                                                    match (waveform_enabled, self.waveform.clone())
                                                    {
                                                        (true, Some(peaks)) => {
                                                            crate::ui::waveform_seek_bar(
                                                                &peaks,
                                                                seek_fraction,
                                                                34.,
                                                                cx.theme().primary,
                                                                cx.theme()
                                                                    .muted_foreground
                                                                    .opacity(0.35),
                                                                self.player.clone(),
                                                            )
                                                        }
                                                        _ => div()
                                                            .flex_1()
                                                            .child(Slider::new(&self.seek))
                                                            .into_any_element(),
                                                    };
                                                let view = cx.entity();
                                                this.child(crate::ui::seek_hover_wrap(
                                                    "fs-seek-hover",
                                                    self.seek_hover,
                                                    duration,
                                                    bar,
                                                    move |fraction, cx| {
                                                        view.update(cx, |p: &mut Self, cx| {
                                                            if p.seek_hover != fraction {
                                                                p.seek_hover = fraction;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                    cx,
                                                ))
                                            })
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(time_total),
                                            )
                                        }),
                                )
                                // Stream info + ReplayGain: quiet, centered line.
                                // Wraps rather than running out of the card: the
                                // codec line is long and the card is only as wide
                                // as the window allows.
                                .when(
                                    layout.density.secondary_lines()
                                        && (stream_info.is_some() || replay_gain.is_some()),
                                    |this| {
                                        this.child(
                                            h_flex()
                                                .w_full()
                                                .flex_wrap()
                                                .gap_3()
                                                .items_center()
                                                .justify_center()
                                                .text_xs()
                                                .text_color(
                                                    cx.theme().muted_foreground.opacity(0.8),
                                                )
                                                .when_some(stream_info, |this, info| {
                                                    this.child(div().child(info))
                                                })
                                                .when_some(replay_gain, |this, (label, db)| {
                                                    let text = match db {
                                                        Some(db) => {
                                                            format!("RG {db:+.1} dB · {label}")
                                                        }
                                                        None => format!("RG · {label}"),
                                                    };
                                                    this.child(div().child(text))
                                                }),
                                        )
                                    },
                                )
                                // Transport controls. Wrapping is the last
                                // resort in a window narrower than the row: a
                                // second line of buttons beats a repeat button
                                // clipped off the card's edge.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .flex_wrap()
                                        .gap_4()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            icon_btn(
                                                "fs-shuffle",
                                                icons::SHUFFLE,
                                                shuffle && !is_radio,
                                            )
                                            .disabled(is_radio)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.toggle_shuffle(cx));
                                                    cx.stop_propagation();
                                                }),
                                            ),
                                        )
                                        .child(
                                            icon_btn("fs-prev", icons::SKIP_BACK, false)
                                                .disabled(!has_track || is_radio)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player.update(cx, |p, cx| p.previous(cx));
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-play")
                                                .primary()
                                                .large()
                                                .icon(if playing {
                                                    app_icon(icons::PAUSE)
                                                } else {
                                                    app_icon(icons::PLAY)
                                                })
                                                .loading(buffering)
                                                .disabled(!has_track)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.toggle_play(cx));
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            icon_btn("fs-next", icons::SKIP_FORWARD, false)
                                                .disabled(!has_track || is_radio)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player.update(cx, |p, cx| p.next(cx));
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            icon_btn(
                                                "fs-repeat",
                                                if repeat == RepeatMode::One {
                                                    icons::REPEAT_1
                                                } else {
                                                    icons::REPEAT
                                                },
                                                repeat != RepeatMode::Off && !is_radio,
                                            )
                                            .disabled(is_radio)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.cycle_repeat(cx));
                                                    cx.stop_propagation();
                                                }),
                                            ),
                                        ),
                                )
                                // Queue / visualizer / lyrics toggles, ruled
                                // off below the transport exactly as the mini
                                // player rules off its scene picker.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .flex_wrap()
                                        .gap_3()
                                        .items_center()
                                        .justify_center()
                                        .pt_4()
                                        .border_t_1()
                                        .border_color(cx.theme().border.opacity(0.4))
                                        .child(
                                            Button::new("fs-queue-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::PanelRight))
                                                .when(toggle_labels, |b| b.label("Queue"))
                                                .when(self.panel == Some(SidePanel::Queue), |b| {
                                                    b.primary()
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.toggle_panel(SidePanel::Queue, cx);
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-viz-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::Frame))
                                                .when(toggle_labels, |b| b.label(viz_mode.label()))
                                                .when(viz_mode.is_on(), |b| b.primary())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cycle_visualizer(cx);
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-lyrics-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::BookOpen))
                                                .when(toggle_labels, |b| b.label("Lyrics"))
                                                .when(self.panel == Some(SidePanel::Lyrics), |b| {
                                                    b.primary()
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.toggle_panel(SidePanel::Lyrics, cx);
                                                    cx.stop_propagation();
                                                })),
                                        ),
                                ),
                        )
                        // Short vertical volume slider, right of the controls —
                        // off by default (the player bar already has one), opt-in
                        // via settings; always hidden during live radio, and
                        // dropped by the layout when the window is too narrow to
                        // carry it beside the cover and the card.
                        .when(layout.volume, |this| {
                            this.child(
                                v_flex()
                                    .h_full()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_HIGH)),
                                    )
                                    .child(
                                        // gpui-component's vertical slider is a fixed
                                        // 120px tall; match it so the high/low icons sit
                                        // symmetrically at each end (no dead space).
                                        div()
                                            .h(px(120.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(Slider::new(&self.volume).vertical()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_LOW)),
                                    )
                                    .when(detailed_volume, |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{}%",
                                                    (volume_level * 100.).round() as u32
                                                )),
                                        )
                                    }),
                            )
                        })
                        // Optional side panel — beside the card, or under it in
                        // the stacked layout, at whatever width and height the
                        // window leaves for it.
                        .when_some(drawn_panel, |this, panel| {
                            // Margin + fade so the motion is visible (gpui
                            // 0.2.2 has no translate/transform), off the
                            // reveal's clock so it plays on the way out too.
                            this.child(
                                div()
                                    .h_full()
                                    .opacity(panel_open)
                                    .ml(px(28. * (1. - panel_open)))
                                    .child(match panel {
                                        SidePanel::Queue => self.render_queue_panel(
                                            layout.panel,
                                            layout.panel_max_h,
                                            cx,
                                        ),
                                        SidePanel::Lyrics => self.render_lyrics_panel(
                                            layout.panel,
                                            layout.panel_max_h,
                                            cx,
                                        ),
                                    }),
                            )
                        });

                    // Content trails the backdrop and travels further, so the
                    // overlay reads as art rising into place rather than one flat
                    // layer sliding. Entrance only: on the way out it stays put
                    // while the whole overlay drops, which keeps the exit from
                    // looking like two things leaving at different speeds.
                    //
                    // The scrolling wrapper is the last-resort fit: the sizes
                    // above shrink to the window, but a window shorter than the
                    // controls themselves has nowhere left to shrink to, and
                    // scrolling past the edge beats being cut off at it.
                    div()
                        .id("fs-content")
                        .size_full()
                        .overflow_y_scroll()
                        .child(content)
                        .opacity(content_fade)
                        .top(px(content_rise))
                })
            })
            .when_some(mini_player, |root, mini| root.child(mini))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ART_LEAD, ART_MAX, ART_MAX_STACKED, ART_MIN, BLOB_BLEED, CARD_MAX, CARD_MIN, CardDensity,
        EDGE, GAP, LYRIC_DIM, LYRIC_REM, LYRIC_REM_ACTIVE, Layout, PANEL_LYRICS_MAX, PANEL_MAX,
        QUEUE_CHROME_H, QUEUE_ROW_H, SidePanel, VOLUME_W, active_line, best_lyrics, blob_base,
        blob_field, hash01, line_emphasis, lyric_color, lyric_seek_target, lyric_wrap,
        plain_lyrics, queue_visible_rows,
    };
    use crate::config::FullscreenCoverSize;
    use std::time::Duration;
    use subsonic::{LyricLine, StructuredLyrics};

    fn doc(synced: bool, lines: &[&str]) -> StructuredLyrics {
        StructuredLyrics {
            synced,
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, v)| LyricLine {
                    start: synced.then_some(i as i64 * 1000),
                    value: (*v).to_string(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn no_lyrics_documents_means_none() {
        assert!(best_lyrics(Vec::new()).is_none());
    }

    /// A document with nothing but blank lines must read as "no lyrics" rather
    /// than drawing an empty panel.
    #[test]
    fn blank_documents_are_dropped() {
        assert!(best_lyrics(vec![doc(false, &[]), doc(false, &["", "  "])]).is_none());
    }

    /// Synced and unsynced copies of the same words are both offered; the
    /// synced one carries strictly more information.
    #[test]
    fn synced_wins_over_unsynced() {
        let picked = best_lyrics(vec![doc(false, &["a"]), doc(true, &["a"])]).unwrap();
        assert!(picked.synced);
        assert_eq!(picked.lines[0].start, Some(0));
    }

    /// With no timings anywhere, the server's own order decides.
    #[test]
    fn first_listed_wins_when_none_are_synced() {
        let picked = best_lyrics(vec![doc(false, &["first"]), doc(false, &["second"])]).unwrap();
        assert_eq!(picked.lines[0].value, "first");
    }

    /// A blank document ahead of a real one must not be picked, and must not
    /// shift which of the rest is.
    #[test]
    fn blanks_do_not_displace_the_pick() {
        let picked = best_lyrics(vec![
            doc(false, &[""]),
            doc(false, &["real"]),
            doc(true, &["timed"]),
        ])
        .unwrap();
        assert_eq!(picked.lines[0].value, "timed");
    }

    #[test]
    fn plain_text_becomes_one_line_per_line() {
        let d = plain_lyrics("one\n\ntwo".to_string());
        assert!(!d.synced);
        assert_eq!(d.lines.len(), 3);
        assert_eq!(d.lines[2].value, "two");
        assert!(d.lines.iter().all(|l| l.start.is_none()));
    }

    #[test]
    fn the_lit_line_is_the_last_one_that_has_started() {
        let d = doc(true, &["a", "b", "c"]);
        assert_eq!(active_line(&d.lines, 0, Duration::from_millis(0)), Some(0));
        assert_eq!(
            active_line(&d.lines, 0, Duration::from_millis(999)),
            Some(0)
        );
        assert_eq!(
            active_line(&d.lines, 0, Duration::from_millis(1000)),
            Some(1)
        );
        // Past the last line it stays lit; there is nothing after it to move to.
        assert_eq!(active_line(&d.lines, 0, Duration::from_secs(600)), Some(2));
    }

    #[test]
    fn nothing_is_lit_before_the_first_line() {
        let mut d = doc(true, &["a", "b"]);
        // An intro: the words start well after the track does.
        d.lines[0].start = Some(8_000);
        d.lines[1].start = Some(12_000);
        assert_eq!(active_line(&d.lines, 0, Duration::from_secs(3)), None);
        assert_eq!(active_line(&d.lines, 0, Duration::from_secs(9)), Some(0));
    }

    #[test]
    fn an_untimed_document_lights_nothing() {
        let d = doc(false, &["a", "b"]);
        assert_eq!(active_line(&d.lines, 0, Duration::from_secs(30)), None);
        assert_eq!(active_line(&[], 0, Duration::from_secs(1)), None);
    }

    #[test]
    fn the_document_offset_shifts_every_line() {
        let d = doc(true, &["a", "b"]);
        // +500 delays the words, so at 1.2s the second line has not arrived.
        assert_eq!(
            active_line(&d.lines, 500, Duration::from_millis(1200)),
            Some(0)
        );
        // -500 pulls them forward, and it has.
        assert_eq!(
            active_line(&d.lines, -500, Duration::from_millis(600)),
            Some(1)
        );
    }

    /// Clicking a line must light *that* line, at any offset — the seek and the
    /// highlight both correct by `offset`, and they only agree if they correct
    /// in the same direction.
    #[test]
    fn clicking_a_lyric_line_seeks_to_where_it_lights() {
        let d = doc(true, &["a", "b", "c"]);
        for offset in [0, 500, -500] {
            for (ix, line) in d.lines.iter().enumerate() {
                let to = lyric_seek_target(line.start.unwrap(), offset);
                assert_eq!(
                    active_line(&d.lines, offset, to),
                    Some(ix),
                    "line {ix} at offset {offset}"
                );
            }
        }
    }

    #[test]
    fn a_lyric_seek_never_goes_before_the_start_of_the_song() {
        assert_eq!(lyric_seek_target(200, -900), Duration::ZERO);
        assert_eq!(lyric_seek_target(0, i64::MIN), Duration::ZERO);
    }

    /// The layout at the default cover size — every case that is not about the
    /// setting itself. The queue is the narrower of the two panels, so it is
    /// what the room-for-a-panel cases are asked about.
    fn resolve(width: f32, height: f32, panel_open: bool, want_volume: bool) -> Layout {
        Layout::resolve(
            width,
            height,
            panel_open.then_some(SidePanel::Queue),
            want_volume,
            FullscreenCoverSize::default(),
        )
    }

    fn rgba(r: f32, g: f32, b: f32) -> gpui::Rgba {
        gpui::Rgba { r, g, b, a: 1.0 }
    }

    fn palette() -> Vec<gpui::Rgba> {
        vec![
            rgba(0.8, 0.2, 0.3),
            rgba(0.2, 0.6, 0.9),
            rgba(0.9, 0.8, 0.2),
            rgba(0.3, 0.9, 0.5),
        ]
    }

    #[test]
    fn hash01_is_stable_and_in_range() {
        for i in 0..500u32 {
            let v = hash01(i);
            assert!((0. ..1.).contains(&v), "hash01({i}) = {v}");
            assert_eq!(v, hash01(i), "hash01 is not deterministic");
        }
        // A field whose cells all landed on the same value would be a grid.
        let spread = (0..64).map(hash01).fold(f32::MIN, f32::max)
            - (0..64).map(hash01).fold(f32::MAX, f32::min);
        assert!(spread > 0.8, "hash01 barely varies: spread {spread}");
    }

    #[test]
    fn the_blob_field_is_empty_without_a_window_or_colours() {
        assert!(blob_field(0., 0., &palette()).is_empty());
        assert!(blob_field(1920., 0., &palette()).is_empty());
        assert!(blob_field(1920., 1080., &[]).is_empty());
    }

    #[test]
    fn blobs_bleed_past_every_edge() {
        // Corners fall back to the flat base colour unless the field spills
        // out of the window — the jitter alone does not guarantee it, the
        // blobs' own radii have to carry them over.
        for &(w, h) in &[(2560., 1440.), (1280., 800.), (700., 1200.), (900., 900.)] {
            let f = blob_field(w, h, &palette());
            assert!(!f.is_empty(), "{w}x{h} produced no blobs");
            let short = w.min(h);
            let (mut left, mut right, mut top, mut bottom) =
                (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            for b in &f {
                let (ex, ey) = (b.rx * short / w, b.ry * short / h);
                left = left.min(b.cx - ex);
                right = right.max(b.cx + ex);
                top = top.min(b.cy - ey);
                bottom = bottom.max(b.cy + ey);
            }
            assert!(left < 0. && top < 0., "{w}x{h}: {left},{top}");
            assert!(right > 1. && bottom > 1., "{w}x{h}: {right},{bottom}");
            // Centres stay inside the bleed band; a blob further out than that
            // is fill cost paid entirely off-screen.
            let far = f
                .iter()
                .map(|b| (b.cx - 0.5).abs().max((b.cy - 0.5).abs()))
                .fold(0., f32::max);
            assert!(far <= 0.5 + BLOB_BLEED + 0.01, "{w}x{h}: centre at {far}");
        }
    }

    #[test]
    fn blob_cells_stay_roughly_square() {
        // Columns follow the aspect ratio; cells far off square give stripes.
        for &(w, h) in &[(2560., 1440.), (1600., 1200.), (800., 1400.)] {
            let cols = blob_field(w, h, &palette()).len() / super::BLOB_ROWS;
            let cell_w = w / cols as f32;
            let cell_h = h / super::BLOB_ROWS as f32;
            let ratio = (cell_w / cell_h).max(cell_h / cell_w);
            assert!(ratio < 1.8, "{w}x{h}: {cols} cols, cell ratio {ratio}");
        }
    }

    #[test]
    fn blob_motion_is_seamless_across_the_loop() {
        // Every cycle count must be a whole number of turns per period, or the
        // repeating animation snaps back to its start on each wrap.
        for b in blob_field(2560., 1440., &palette()) {
            for f in [b.fx, b.fy, b.fb] {
                assert_eq!(f.fract(), 0., "non-integer cycle count {f}");
                assert!(f >= 1., "cycle count {f} below one turn");
            }
            assert_ne!(b.fx, b.fy, "equal drift rates collapse the path to a line");
            assert!(b.period > 0. && b.rx > 0. && b.ry > 0., "{b:?}");
            assert!((0. ..=1.).contains(&b.alpha), "{b:?}");
        }
    }

    #[test]
    fn neighbouring_blobs_do_not_share_a_colour() {
        // A repeated hue between adjacent cells reads as a blob-shaped hole.
        let f = blob_field(2560., 1440., &palette());
        for pair in f.windows(2) {
            assert_ne!(pair[0].color, pair[1].color, "adjacent blobs match");
        }
    }

    #[test]
    fn the_base_sits_under_the_field_without_going_black() {
        // Gaps between blobs show the base. Left on the near-black player-bar
        // tint they read as blotches rather than as part of the gradient.
        let tint = rgba(0.06, 0.06, 0.08);
        let base = blob_base(&palette(), tint);
        let lum = |c: gpui::Rgba| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
        assert!(
            lum(base) > lum(tint),
            "base {base:?} no lighter than the tint"
        );
        // …but it still has to sit under the blobs, not compete with them.
        let brightest = palette().into_iter().map(lum).fold(0., f32::max);
        assert!(
            lum(base) < brightest,
            "base {base:?} is as bright as a blob"
        );
        // No colours at all is the pre-cover state: fall back to the tint.
        assert_eq!(blob_base(&[], tint), tint);
    }

    #[test]
    fn the_field_is_deterministic() {
        assert_eq!(
            blob_field(2560., 1440., &palette()),
            blob_field(2560., 1440., &palette()),
        );
    }

    /// Total width the landscape layout asks for, padding and gaps included.
    /// A dropped cover is not a column and takes no gap with it.
    fn row_width(l: &Layout, volume: f32) -> f32 {
        let present = [l.art, l.card, l.panel, volume]
            .iter()
            .filter(|w| **w > 0.)
            .count() as f32;
        2. * EDGE + GAP * (present - 1.).max(0.) + l.art + l.card + l.panel + volume
    }

    /// Height the card is expected to need in this layout's form, at the width
    /// this layout draws it.
    fn card_height(l: &Layout) -> f32 {
        l.density.card_height(l.card)
    }

    /// Total height the content asks for.
    fn content_height(l: &Layout) -> f32 {
        let body = if l.stacked {
            let gaps = GAP * if l.panel > 0. { 2. } else { 1. };
            l.art + gaps + card_height(l) + l.panel_max_h
        } else {
            l.art.max(card_height(l))
        };
        l.pad_top + body + l.pad_bottom
    }

    #[test]
    fn a_roomy_window_keeps_the_full_size_layout() {
        let l = resolve(1400., 900., false, false);
        assert!(!l.stacked);
        assert!(l.art >= ART_MAX);
        assert_eq!(l.card, CARD_MAX);
    }

    #[test]
    fn a_big_window_grows_the_cover_without_costing_the_card() {
        // The window that only just holds the overlay is laid out as before…
        assert_eq!(resolve(1000., 640., false, false).art, ART_MAX);
        // …and past that the cover grows with the window rather than leaving
        // the room around it empty — never by taking width off the card.
        let mut last = ART_MAX;
        for &(w, h) in &[(1400., 900.), (1920., 1080.), (2560., 1440.)] {
            let l = resolve(w, h, false, false);
            assert!(!l.stacked, "{w}x{h} should not stack");
            assert!(l.art > last, "{w}x{h} did not grow the cover: {l:?}");
            assert_eq!(l.card, CARD_MAX, "{w}x{h} squeezed the card: {l:?}");
            assert!(
                l.art <= FullscreenCoverSize::default().beside_card().1 + 0.5,
                "{w}x{h}: {l:?}"
            );
            assert!(row_width(&l, 0.) <= w + 0.5, "{w}x{h} overruns: {l:?}");
            assert!(content_height(&l) <= h + 0.5, "{w}x{h} overruns: {l:?}");
            last = l.art;
        }
        // With the panel and the volume column open too, on the same window.
        let l = resolve(2560., 1440., true, true);
        assert!(l.volume && l.panel > 0.);
        assert!(l.art > ART_MAX && l.card == CARD_MAX, "{l:?}");
        assert!(row_width(&l, VOLUME_W) <= 2560. + 0.5, "{l:?}");
    }

    #[test]
    fn a_big_portrait_window_grows_the_cover_too() {
        let mut last = ART_MAX_STACKED;
        for &(w, h) in &[(800., 1200.), (1100., 1700.), (1440., 2560.)] {
            let l = resolve(w, h, false, false);
            assert!(l.stacked, "{w}x{h} should stack");
            assert!(l.art > last, "{w}x{h} did not grow the cover: {l:?}");
            assert!(
                l.art <= FullscreenCoverSize::default().stacked().1 + 0.5,
                "{w}x{h}: {l:?}"
            );
            assert!(l.art <= w - 2. * EDGE + 0.5, "{w}x{h}: {l:?}");
            assert!(l.art >= l.card + ART_LEAD, "{w}x{h}: {l:?}");
            assert!(content_height(&l) <= h + 0.5, "{w}x{h} overruns: {l:?}");
            last = l.art;
        }
    }

    #[test]
    fn the_cover_size_setting_orders_the_sizes_and_fixed_gives_up_the_growth() {
        use FullscreenCoverSize::{Fixed, Huge, Large, Medium};
        for &(w, h) in &[(1920., 1080.), (2560., 1440.), (3840., 2160.)] {
            let sizes: Vec<f32> = [Fixed, Medium, Large, Huge]
                .iter()
                .map(|&s| Layout::resolve(w, h, None, false, s).art)
                .collect();
            // Fixed draws the cover a window that only just holds the overlay
            // draws, however much room this one has.
            assert_eq!(sizes[0], ART_MAX, "{w}x{h}: {sizes:?}");
            for pair in sizes.windows(2) {
                assert!(pair[1] > pair[0], "{w}x{h}: {sizes:?}");
            }
            // Whatever the size, the row still fits the window and the card
            // keeps its full width.
            for &size in &[Fixed, Medium, Large, Huge] {
                let l = Layout::resolve(w, h, None, false, size);
                assert_eq!(l.card, CARD_MAX, "{w}x{h} {size:?}: {l:?}");
                assert!(row_width(&l, 0.) <= w + 0.5, "{w}x{h} {size:?}: {l:?}");
                assert!(content_height(&l) <= h + 0.5, "{w}x{h} {size:?}: {l:?}");
            }
        }
    }

    #[test]
    fn the_cover_size_setting_applies_to_the_stacked_layout_too() {
        use FullscreenCoverSize::{Fixed, Huge, Large, Medium};
        for &(w, h) in &[(1100., 1700.), (1440., 2560.)] {
            let sizes: Vec<f32> = [Fixed, Medium, Large, Huge]
                .iter()
                .map(|&s| Layout::resolve(w, h, None, false, s).art)
                .collect();
            assert_eq!(sizes[0], ART_MAX_STACKED, "{w}x{h}: {sizes:?}");
            for pair in sizes.windows(2) {
                assert!(pair[1] >= pair[0], "{w}x{h}: {sizes:?}");
            }
            for &size in &[Fixed, Medium, Large, Huge] {
                let l = Layout::resolve(w, h, None, false, size);
                assert!(l.stacked, "{w}x{h} {size:?}: {l:?}");
                assert!(l.art <= w - 2. * EDGE + 0.5, "{w}x{h} {size:?}: {l:?}");
                assert!(content_height(&l) <= h + 0.5, "{w}x{h} {size:?}: {l:?}");
            }
        }
    }

    #[test]
    fn a_window_with_no_room_to_spare_ignores_the_cover_size() {
        // The setting governs the room *above* what the overlay needs, so a
        // window that only just fits it draws the same cover at every size.
        use FullscreenCoverSize::{Fixed, Huge, Large, Medium};
        for &(w, h) in &[(1000., 640.), (760., 600.), (700., 500.)] {
            let base = Layout::resolve(w, h, None, false, Fixed);
            for &size in &[Medium, Large, Huge] {
                assert_eq!(
                    Layout::resolve(w, h, None, false, size),
                    base,
                    "{w}x{h} {size:?}"
                );
            }
        }
    }

    #[test]
    fn the_queue_panel_shows_whole_rows_only() {
        // Whatever room the panel is given, it draws a whole number of rows —
        // a row cut through the middle reads as a bug, not as a longer list.
        for max_h in [140., 260., 333., 620., 900.] {
            let rows = queue_visible_rows(max_h, 200);
            let drawn = QUEUE_CHROME_H + rows as f32 * QUEUE_ROW_H;
            assert!(drawn <= max_h + 0.5, "{max_h}: {rows} rows is {drawn}");
            // …and it uses the room it has: one more row would not fit.
            assert!(
                drawn + QUEUE_ROW_H > max_h,
                "{max_h}: {rows} rows wastes a row"
            );
        }
        // A short queue shrinks the panel instead of padding it out.
        assert_eq!(queue_visible_rows(620., 3), 3);
        // A panel too short for even one row still draws one rather than
        // collapsing to its header.
        assert_eq!(queue_visible_rows(0., 10), 1);
        // An empty queue keeps the panel a panel.
        assert_eq!(queue_visible_rows(620., 0), 1);
    }

    /// The sheet falls away from the line being sung in both directions — a
    /// verse already past is no more present than one still coming.
    #[test]
    fn the_sheet_falls_away_from_the_lit_line() {
        let e = |ix| line_emphasis(Some(4), ix, true);
        assert_eq!(e(4), 1.);
        assert_eq!(e(3), e(5));
        assert!(e(4) > e(3) && e(3) > e(2) && e(2) > e(1));
        // Past three lines out it stops falling, or a long song fades to
        // nothing and the sheet reads as ending.
        assert_eq!(e(1), e(0));
    }

    /// A document with no timings has no line to be on, and an intro has not
    /// reached its first — neither should look switched off.
    #[test]
    fn an_unfollowable_sheet_is_not_dimmed_to_nothing() {
        assert_eq!(line_emphasis(None, 7, false), 1.);
        assert_eq!(line_emphasis(Some(2), 7, false), 1.);
        assert_eq!(line_emphasis(None, 7, true), LYRIC_DIM[0]);
    }

    /// The handover's colour has to land exactly on both ends — a lit line left
    /// a shade off `primary`, or a resting one a shade off `foreground`, is a
    /// sheet where no two lines are quite the same colour.
    #[test]
    fn a_lyric_line_crosses_cleanly_between_its_two_colours() {
        let rest = gpui::hsla(0.6, 0.2, 0.9, 1.);
        let lit = gpui::hsla(0.1, 0.8, 0.5, 1.);
        assert_eq!(lyric_color(rest, lit, 0.), rest);
        assert_eq!(lyric_color(rest, lit, 1.), lit);
        // And travels: halfway is neither end, and is the same colour whichever
        // direction it is approached from, so the two lines trading places meet
        // rather than cross at an angle.
        let mid = lyric_color(rest, lit, 0.5);
        assert_ne!(mid, rest);
        assert_ne!(mid, lit);
        let back = lyric_color(lit, rest, 0.5);
        for (a, b) in [(mid.h, back.h), (mid.s, back.s), (mid.l, back.l)] {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    /// The point of `lyric_wrap`: a line's advance width and the width it is
    /// laid out in are scaled by the same factor at every size the grow passes
    /// through, so the ratio between them — which is what decides where the
    /// words break — never changes.
    #[test]
    fn a_growing_lyric_line_keeps_its_line_breaks() {
        // The full panel is the lit line's, and every smaller size gives back
        // exactly the width its type no longer needs.
        assert_eq!(lyric_wrap(LYRIC_REM_ACTIVE), 1.);
        assert!(lyric_wrap(LYRIC_REM) < 1.);
        for step in 0..=10 {
            let t = step as f32 / 10.;
            let size = LYRIC_REM + (LYRIC_REM_ACTIVE - LYRIC_REM) * t;
            let ratio = size / lyric_wrap(size);
            assert!(
                (ratio - LYRIC_REM_ACTIVE).abs() < 1e-5,
                "size {size} wraps at a different width ratio ({ratio})"
            );
        }
    }

    /// Lyrics are prose set large; the queue is one-line rows. Sharing a width
    /// meant either the verses wrapped in half or the list was padded out.
    #[test]
    fn the_lyrics_panel_is_wider_than_the_queue() {
        let panel = |p| Layout::resolve(1920., 1080., p, false, FullscreenCoverSize::default());
        let queue = panel(Some(SidePanel::Queue)).panel;
        let lyrics = panel(Some(SidePanel::Lyrics)).panel;
        assert!(lyrics > queue, "lyrics {lyrics} vs queue {queue}");
        assert_eq!(panel(None).panel, 0.);
        // Both are capped, or a wide window spends its width on the panel
        // rather than on the cover the page is for.
        let wide = |p| Layout::resolve(3840., 2160., p, false, FullscreenCoverSize::default());
        assert_eq!(wide(Some(SidePanel::Queue)).panel, PANEL_MAX);
        assert_eq!(wide(Some(SidePanel::Lyrics)).panel, PANEL_LYRICS_MAX);
    }

    #[test]
    fn landscape_content_never_overruns_the_window() {
        for &(w, h) in &[
            (1400., 900.),
            (1280., 800.),
            (1100., 700.),
            (900., 600.),
            (820., 520.),
        ] {
            // The lyrics panel is the wider of the two, so it is the one that
            // can push the row past the window if the widths are wrong.
            for panel in [None, Some(SidePanel::Queue), Some(SidePanel::Lyrics)] {
                let l = Layout::resolve(w, h, panel, false, FullscreenCoverSize::default());
                if l.stacked {
                    continue;
                }
                assert!(
                    row_width(&l, 0.) <= w + 0.5,
                    "{w}x{h} panel={panel:?} overruns: {l:?}"
                );
                // The cover is square: it has to fit the height as well.
                assert!(l.art <= h - 2. * EDGE + 0.5, "{w}x{h}: {l:?}");
            }
        }
    }

    #[test]
    fn the_volume_column_is_dropped_before_the_cover_is_squeezed() {
        // Wide enough for cover + card + volume, panel open or not.
        assert!(resolve(1400., 900., false, true).volume);
        assert!(resolve(1400., 900., true, true).volume);
        // Not wide enough: the column goes, the row survives.
        let tight = resolve(760., 600., false, true);
        assert!(!tight.volume);
        assert!(!tight.stacked);
        assert!(tight.art >= ART_MIN && tight.card >= CARD_MIN);
    }

    #[test]
    fn a_window_taller_than_it_is_wide_stacks() {
        let l = resolve(800., 1200., false, true);
        assert!(l.stacked);
        // One column, both pieces inside the window's width.
        assert!(l.card <= 800. - 2. * EDGE);
        assert!(l.art > 0. && l.art <= 800. - 2. * EDGE);
        // The vertical volume slider belongs beside the controls, not under.
        assert!(!l.volume);
    }

    #[test]
    fn the_stacked_cover_leads_the_card() {
        for &(w, h) in &[
            (520., 860.),
            (480., 900.),
            (700., 1100.),
            (900., 1400.),
            (1000., 1300.),
        ] {
            let l = resolve(w, h, false, false);
            assert!(l.stacked, "{w}x{h} should stack");
            assert!(
                l.art >= l.card + ART_LEAD,
                "{w}x{h} cover does not lead the card: {l:?}"
            );
            // And the column still fits the window it was sized for.
            assert!(content_height(&l) <= h + 0.5, "{w}x{h}: {l:?}");
        }
    }

    #[test]
    fn a_window_too_narrow_for_two_columns_stacks_as_well() {
        // Still room for a small cover beside the card: keep the row, since
        // stacking this would leave the cover no vertical room at all.
        let row = resolve(700., 500., false, false);
        assert!(!row.stacked);
        assert!(row.art >= ART_MIN);
        // Below that the row cannot hold both columns.
        assert!(resolve(560., 500., false, false).stacked);
        assert!(resolve(500., 460., false, false).stacked);
    }

    #[test]
    fn a_tight_window_with_a_panel_open_drops_the_cover_not_the_panel() {
        // Cover + card + panel does not fit; card + panel does. The panel was
        // asked for, the cover was not.
        let l = resolve(760., 520., true, false);
        assert!(!l.stacked);
        assert_eq!(l.art, 0.);
        assert!(l.panel > 0. && l.card >= CARD_MIN);
        assert!(row_width(&l, 0.) <= 760. + 0.5, "{l:?}");
    }

    #[test]
    fn a_short_window_drops_the_cover_rather_than_the_controls() {
        let l = resolve(420., 460., false, false);
        assert!(l.stacked);
        assert_eq!(l.art, 0.);
        assert!(l.card > 0.);
    }

    #[test]
    fn the_content_fits_the_window_height() {
        for &(w, h) in &[
            (1400., 900.),
            (1150., 380.),
            (760., 520.),
            (900., 600.),
            (520., 860.),
            (700., 1100.),
            (480., 900.),
        ] {
            let l = resolve(w, h, false, false);
            assert!(
                content_height(&l) <= h + 0.5,
                "{w}x{h} overruns the height: {l:?}"
            );
            // With a panel open the same has to hold wherever the window can
            // hold the panel at all; below that the wrapper scrolls, and the
            // panel keeps a usable height instead of being squeezed to nothing.
            let l = resolve(w, h, true, false);
            assert!(l.panel_max_h >= 140., "{w}x{h} squeezed the panel: {l:?}");
            if content_height(&l) > h + 0.5 {
                assert!(l.art == 0., "{w}x{h} overruns with a cover still on: {l:?}");
            }
        }
    }

    #[test]
    fn spacing_is_given_up_before_the_track_details_are() {
        // Room for the full card.
        assert_eq!(
            resolve(1400., 900., false, false).density,
            CardDensity::Full
        );
        assert_eq!(resolve(760., 520., false, false).density, CardDensity::Full);
        // Too short for the full card, but the tight one still fits: the album
        // and stream-info lines stay, the padding goes.
        let short = resolve(1150., 380., false, false);
        assert_eq!(short.density, CardDensity::Tight);
        assert!(short.density.secondary_lines());
        // Too short for even that: now the lines go.
        assert_eq!(
            resolve(1150., 300., false, false).density,
            CardDensity::Compact
        );
    }

    #[test]
    fn a_portrait_window_keeps_the_track_details() {
        // The stacked layout frees height for the cover by tightening the
        // card's spacing, never by dropping what the card says.
        for &(w, h) in &[(520., 860.), (480., 900.), (700., 1100.), (760., 880.)] {
            let l = resolve(w, h, false, false);
            assert!(l.stacked, "{w}x{h} should stack");
            assert!(
                l.density.secondary_lines(),
                "{w}x{h} dropped the track details: {l:?}"
            );
            assert!(l.art >= l.card + ART_LEAD, "{w}x{h}: {l:?}");
        }
    }

    #[test]
    fn the_stacked_column_fits_its_width() {
        for &(w, h) in &[(700., 1100.), (480., 900.), (1000., 1400.)] {
            for &panel in &[false, true] {
                let l = resolve(w, h, panel, false);
                assert!(l.stacked, "{w}x{h} should stack");
                assert!(l.card <= w - 2. * EDGE + 0.5, "{w}x{h}: {l:?}");
                assert!(l.art <= w - 2. * EDGE + 0.5, "{w}x{h}: {l:?}");
                assert!(l.panel <= w - 2. * EDGE + 0.5, "{w}x{h}: {l:?}");
            }
        }
    }
}
