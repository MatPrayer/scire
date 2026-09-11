//! Settings: window chrome, theme, playback, streaming, storage, account.

use gpui::{
    App, Context, Entity, Focusable as _, IntoElement, Render, ScrollAnchor, ScrollHandle, Window,
    div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::{
    CoverSize, DefaultPage, FullscreenBackground, FullscreenCoverSize, QueueEndBehavior,
    ReplayGainMode, ThemePref,
};
use crate::services::library_db::LibraryDb;
use crate::services::{art_precache, artwork, navidrome_sync, runtime};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::{
    apply_theme, apply_window_chrome, sync_focus_scroll, transition, with_focus_cursor,
};

/// How often the two library maintenance jobs republish their progress.
const LIBRARY_TASK_POLL: Duration = Duration::from_millis(500);

/// How long a quick-nav jump takes to reach its section.
const SECTION_SCROLL_MS: u64 = 200;
/// A jump leaves the section's card this far below the scroll viewport's top
/// edge, so its border doesn't sit flush against the pill strip above.
const SECTION_SCROLL_LEAD: f32 = 8.;
/// A section becomes the current one once its top edge has passed this far
/// into the scroll viewport. Deeper than [`SECTION_SCROLL_LEAD`] on purpose:
/// a jump has to leave its own target marked, not the section above it.
const SECTION_ACTIVE_LINE: f32 = 24.;
/// Within this much of the bottom the last section is current whatever the
/// activation line says — a short final section never reaches that line, and
/// its pill would otherwise be unreachable by scrolling.
const SECTION_BOTTOM_SLOP: f32 = 4.;

/// Widest a section card is allowed to get; past this the page reads as a
/// stretched table rather than a column of settings.
const SECTION_MAX_W: f32 = 640.;
/// Narrowest it may be squeezed to before it is allowed to overrun the window
/// instead — below this the labels are unreadable anyway.
const SECTION_MIN_W: f32 = 240.;
/// The scroll body's own horizontal padding (`px_4`), which the cards sit
/// inside of.
const SECTION_BODY_PAD: f32 = 16.;

/// The width to lay a section card out at, inside a scroll body `body` wide.
///
/// **Definite on purpose, and load-bearing.** Built the obvious way — `w_full()`
/// plus `max_w(SECTION_MAX_W)` — the card's width is still indefinite at the
/// point taffy sizes its height, so every wrapping paragraph inside it is
/// measured against an unbounded width and comes out one line tall. The card
/// then reserved less height than its own content, drew its border through the
/// last paragraph and let the rest spill into the section below. Handing it a
/// resolved number means the paragraphs are measured at the width they are
/// actually painted at.
///
/// `None` before anything has been laid out, when there is no measurement to
/// resolve one from.
fn section_width(body: f32) -> Option<f32> {
    if body <= 0. {
        return None;
    }
    Some((body - 2. * SECTION_BODY_PAD).clamp(SECTION_MIN_W, SECTION_MAX_W))
}

/// Gap between the compact grid's columns, and between the cards stacked
/// inside one (`gap_3`).
const COMPACT_GAP: f32 = 12.;
/// Height one weight unit of a card costs, and the card's own padding on top of
/// its rows — the model `compact_columns` picks a column count with. Measured
/// off the running page rather than derived: a switch row and its gap come to
/// about 30px, `p_3` adds 24, and the gap under the card another 12.
const COMPACT_ROW_H: f32 = 30.;
const COMPACT_CARD_H: f32 = 24. + COMPACT_GAP;
/// The width a column is drawn at when the page has room for it. Wider than
/// this and two columns of settings read as two pages side by side; the grid
/// simply doesn't spend the rest of the window, and is centred in it.
const COMPACT_COL_TARGET: f32 = 460.;
/// Narrowest a column may be squeezed to before the count is reduced instead.
/// A switch's label is one line whatever the room — "Album pages tint from
/// their own cover" and the switch beside it need about 330px inside the card's
/// padding, and a column any narrower than this pushes it out through the
/// card's edge instead of wrapping it.
const COMPACT_COL_MIN: f32 = 380.;
/// Past four columns the cards are short and far apart — the page reads as a
/// scattering rather than a grid.
const COMPACT_COL_MAX: usize = 4;
/// How far a column's width may fall below and rise above an even split. The
/// asymmetry is the point — a column carrying more gets more room, so its rows
/// wrap less and the columns come out closer to the same height — but a column
/// that is a third of its neighbour stops reading as the same grid.
const COMPACT_SHARE_MIN: f32 = 0.8;
const COMPACT_SHARE_MAX: f32 = 1.3;

/// The sections in document order, with a rough count of the rows each one
/// draws in compact mode (captions hidden), used only to plan the columns.
///
/// Hand-kept rather than measured: what a card actually costs is known after it
/// is built, and the column widths are needed before. Being a row or two out
/// costs a slightly uneven grid and nothing else. `section` debug-asserts the
/// titles against this list, so a section added without a weight is caught in
/// development rather than silently shifting the layout.
///
/// Account is last because it is the one that may be absent (signed out), which
/// makes "the sections that are present" a prefix of this list.
const COMPACT_SECTIONS: [(&str, u16); 7] = [
    ("Window", 4),
    ("Appearance", 14),
    ("Playback", 14),
    ("Browsing", 11),
    ("Streaming", 5),
    ("Library", 12),
    ("Account", 3),
];

/// One column of the compact grid: the sections that landed in it, in document
/// order, and the width to lay their cards out at.
#[derive(Debug, Clone, PartialEq)]
struct GridColumn {
    sections: Vec<usize>,
    width: f32,
}

/// What a card of `weight` rows costs vertically, its padding and the gap under
/// it included.
fn card_height(weight: u16) -> f32 {
    f32::from(weight) * COMPACT_ROW_H + COMPACT_CARD_H
}

/// Split the cards into `cols` **contiguous** runs, making the tallest run as
/// short as the split allows.
///
/// Contiguous is the requirement, not an implementation detail: the columns are
/// the settings page in its usual order, wrapped — read down the first column
/// and on to the next and the sections come in the order they always have.
/// Packing them by size instead balances the columns better and shuffles the
/// page.
///
/// Binary search on "could every run fit under this height", which a left-to-
/// right greedy answers in one pass, then one more pass to cut at the height
/// the search settled on.
fn split_runs(heights: &[f32], cols: usize) -> Vec<Vec<usize>> {
    let runs_under = |cap: f32| -> usize {
        let mut runs = 1;
        let mut used = 0.;
        for &h in heights {
            if used + h > cap && used > 0. {
                runs += 1;
                used = 0.;
            }
            used += h;
        }
        runs
    };

    let tallest = heights.iter().copied().fold(0., f32::max);
    let total: f32 = heights.iter().sum();
    let (mut lo, mut hi) = (tallest, total.max(tallest));
    // ~1px of resolution over any page this will ever hold; a fixed iteration
    // count keeps it obviously terminating.
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if runs_under(mid) <= cols {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    let mut runs: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = 0.;
    for (i, &h) in heights.iter().enumerate() {
        // The last run takes whatever is left: with `cols` runs already open,
        // a rounding error in the cap must not open one more.
        if used + h > hi && used > 0. && runs.len() < cols {
            runs.push(Vec::new());
            used = 0.;
        }
        runs.last_mut().expect("a run is always open").push(i);
        used += h;
    }
    runs
}

/// The width `cols` columns take inside a `usable` body, the gaps between them
/// excluded — [`COMPACT_COL_TARGET`] each where the body has room for it, and
/// what there is where it does not.
fn columns_width(cols: usize, usable: f32) -> f32 {
    let gaps = COMPACT_GAP * (cols - 1) as f32;
    (usable - gaps)
        .max(SECTION_MIN_W)
        .min(COMPACT_COL_TARGET * cols as f32)
}

/// The tallest of the runs a `cols`-way split produces.
fn tallest_run(heights: &[f32], cols: usize) -> f32 {
    split_runs(heights, cols)
        .iter()
        .map(|run| run.iter().map(|&i| heights[i]).sum::<f32>())
        .fold(0., f32::max)
}

/// How many columns to wrap the page into, or `None` where the window cannot
/// hold a grid at all.
///
/// This is also the decision of *whether* the page is a grid: the layout has no
/// setting behind it, so a count is only returned when at least two columns fit
/// in the window's width and the tallest of them fits its height. One column is
/// not a grid — it is the scrolling page with its captions taken away, which is
/// a worse page, not a denser one — and a grid that does not fit has given up
/// the only thing it was for.
///
/// Of the counts that do fit, the one that comes out **closest to square**,
/// compared as the log of the grid's aspect so that half as wide as tall and
/// twice as wide count the same. Neither extreme is the page anyone wants: the
/// widest count the width allows leaves every card in the top strip of a tall
/// window, and the fewest that fit leaves one 460px column down the middle of a
/// wide one.
///
/// What the width decides is the ceiling: how many columns of
/// [`COMPACT_COL_MIN`] fit in it, capped at [`COMPACT_COL_MAX`] and at one per
/// section.
fn compact_columns(heights: &[f32], body: f32, height: f32) -> Option<usize> {
    let usable = body - 2. * SECTION_BODY_PAD;
    let by_width = ((usable + COMPACT_GAP) / (COMPACT_COL_MIN + COMPACT_GAP)) as usize;
    let max = by_width.min(heights.len()).min(COMPACT_COL_MAX);
    (2..=max)
        .filter_map(|cols| {
            let tall = tallest_run(heights, cols);
            if tall > height {
                return None;
            }
            let wide = columns_width(cols, usable) + COMPACT_GAP * (cols - 1) as f32;
            Some(((wide / tall).ln().abs(), cols))
        })
        .min_by(|(a, _), (b, _)| a.total_cmp(b))
        .map(|(_, cols)| cols)
}

/// Plan the grid the settings page is drawn as, or nothing where the window
/// wants the scrolling column instead — see [`compact_columns`], which is where
/// that choice is made.
///
/// Widths are proportional to what each column ended up carrying, clamped
/// around an even split: the taller column is also the one whose rows are most
/// likely to wrap, so giving it the extra width buys height back where it is
/// short. The grid is laid out at [`COMPACT_COL_TARGET`] per column and only
/// squeezed below that when the window is too narrow to hold it — a wide window
/// gets a centred grid rather than cards stretched across it.
fn compact_grid(weights: &[u16], body: f32, height: f32) -> Vec<GridColumn> {
    if weights.is_empty() || body <= 0. || height <= 0. {
        return Vec::new();
    }
    let heights: Vec<f32> = weights.iter().copied().map(card_height).collect();
    let Some(cols) = compact_columns(&heights, body, height) else {
        return Vec::new();
    };
    let spread = columns_width(cols, body - 2. * SECTION_BODY_PAD);

    let runs = split_runs(&heights, cols);
    let loads: Vec<f32> = runs
        .iter()
        .map(|run| run.iter().map(|&i| heights[i]).sum())
        .collect();
    let total = loads.iter().sum::<f32>().max(1.);
    let even = 1. / cols as f32;
    let shares: Vec<f32> = loads
        .iter()
        .map(|load| (load / total).clamp(even * COMPACT_SHARE_MIN, even * COMPACT_SHARE_MAX))
        .collect();
    // Renormalized after the clamp, so the columns still spend exactly the
    // width the grid was given however far the clamp moved them.
    let spent: f32 = shares.iter().sum();
    let mut widths: Vec<f32> = shares.iter().map(|share| spread * share / spent).collect();
    // The asymmetry is a nicety and the floor is not: a column under
    // `COMPACT_COL_MIN` pushes its switch labels out through the card's edge,
    // and a count is only chosen when an even split clears the floor — so where
    // the shares would breach it, they give way and the columns are even.
    if widths.iter().any(|w| *w < COMPACT_COL_MIN) {
        widths = vec![spread / cols as f32; cols];
    }
    runs.into_iter()
        .zip(widths)
        .map(|(sections, width)| GridColumn { sections, width })
        .collect()
}

/// Which section a scroll position sits in.
///
/// `tops` holds each section card's *unscrolled* layout top in document order,
/// which is what the scroll handle records for its children; `offset` is the
/// scroll offset, zero at the top and negative going down.
fn section_for_scroll(tops: &[f32], viewport_top: f32, offset: f32, max_offset: f32) -> usize {
    if tops.is_empty() {
        return 0;
    }
    if max_offset > 0. && offset <= -max_offset + SECTION_BOTTOM_SLOP {
        return tops.len() - 1;
    }
    let line = viewport_top + SECTION_ACTIVE_LINE;
    tops.iter()
        .rposition(|top| top + offset <= line)
        .unwrap_or(0)
}

/// State of one of the maintenance jobs in the Library section.
///
/// Both are long, both can fail in ways the user needs told about (a server
/// rescan is admin-only on Navidrome), and neither has a meaningful total — so
/// they report a status line rather than a bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TaskState {
    #[default]
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

impl TaskState {
    fn is_running(&self) -> bool {
        matches!(self, TaskState::Running(_))
    }

    fn message(&self) -> Option<&str> {
        match self {
            TaskState::Idle => None,
            TaskState::Running(m) | TaskState::Done(m) | TaskState::Failed(m) => Some(m),
        }
    }
}

/// Which of the on/off switches this is — enough to toggle it through the
/// same `set_*` method the mouse path uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSwitch {
    ClientTitlebar,
    MinimalTitlebar,
    ShowNavButtons,
    AdaptiveFromPage,
    AdaptivePageGradient,
    SelectionGlow,
    FullscreenVolume,
    Scrobble,
    ResumePlayback,
    DefaultShuffle,
    WaveformSeekbar,
    StreamInfoBar,
    DetailedVolume,
    ShowQueueButton,
    ViMode,
    ReducedMotion,
    PrecacheArt,
}

/// A button-group entry or a standalone settings button.
#[derive(Clone)]
enum SettingsButton {
    Theme(ThemePref),
    FullscreenBg(FullscreenBackground),
    FullscreenCover(FullscreenCoverSize),
    ReplayGain(ReplayGainMode),
    QueueEnd(QueueEndBehavior),
    Repeat(RepeatMode),
    DefaultPage(DefaultPage),
    CoverSize(CoverSize),
    TrackInfo(TrackInfoField),
    Format(Option<&'static str>),
    Bitrate(Option<u32>),
    Cache(u32),
    ScanServer,
    RebuildCache,
    AddLocalDir,
    RemoveLocalDir(usize),
    SignOut,
}

/// Which track-info chip to toggle; maps back to the accessor the mouse path
/// passes to `toggle_track_info`.
#[derive(Clone, Copy)]
enum TrackInfoField {
    Artist,
    Album,
    Year,
    Genre,
    Bitrate,
    Plays,
}

impl TrackInfoField {
    fn toggle(self) -> fn(&mut crate::config::TrackInfo) -> &mut bool {
        match self {
            Self::Artist => |t| &mut t.artist,
            Self::Album => |t| &mut t.album,
            Self::Year => |t| &mut t.year,
            Self::Genre => |t| &mut t.genre,
            Self::Bitrate => |t| &mut t.bitrate,
            Self::Plays => |t| &mut t.plays,
        }
    }
}

/// One interactive control on the settings page, in document order.
#[derive(Clone)]
enum SettingsAction {
    Switch(SettingsSwitch),
    Button(SettingsButton),
    DirInput,
}

/// A quick-nav jump in flight: the scroll offsets it runs between, the section
/// it is heading for, and when it started.
///
/// The target section is carried so the pill strip can mark the destination
/// for the whole travel — deriving it from the scroll position alone would
/// walk the highlight through every section the jump passes over.
struct SectionScroll {
    from: f32,
    to: f32,
    section: usize,
    started: Instant,
    duration: Duration,
}

pub struct SettingsView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    dir_input: Entity<InputState>,
    library_db: Arc<LibraryDb>,
    server_scan: TaskState,
    rebuild: TaskState,
    /// Cover-art preload, when it was started from this page. A pass the root
    /// view starts after a sync runs silently and leaves this Idle.
    precache: TaskState,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    vi_cursor: Option<usize>,
    /// Cursor position the scroll has caught up to, so `render` scrolls only
    /// when the cursor actually moved (`ui::sync_focus_scroll`).
    vi_scroll_synced: Option<usize>,
    vi_actions: Vec<SettingsAction>,
    vi_count: usize,
    /// Per-section navigation: `(first_control_index, title)`, rebuilt in
    /// document order each render by `section`. The index into this list is
    /// also the section card's index among the scroll body's children, which
    /// is what `ScrollHandle::bounds_for_item` is asked about — so the pills,
    /// the vi cursor and the measured positions cannot drift apart.
    section_starts: Vec<(usize, &'static str)>,
    /// The quick-nav jump currently running, if any.
    scroll_anim: Option<SectionScroll>,
    /// Content width of the scroll body, tracked across a resize so the cards
    /// reflow on the same frame as the window (see [`crate::ui::LiveWidth`]).
    live_width: crate::ui::LiveWidth,
    /// This frame's section-card width, resolved once at the top of `render`
    /// and read by every `section` call.
    card_width: Option<f32>,
    /// Whether this frame is drawn as the grid, decided by the window's size
    /// (see `compact_grid`) and stored because every card, caption and task row
    /// is built from it.
    compact: bool,
    /// The grid's per-section card width, in document order — the width of the
    /// column each section was placed in. All zero when the page is the
    /// ordinary scrolling column.
    compact_widths: Vec<f32>,
}

impl SettingsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        library_db: Arc<LibraryDb>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let dir_input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/music"));
        let scroll = ScrollHandle::new();
        Self {
            session,
            player,
            dir_input,
            library_db,
            server_scan: TaskState::default(),
            rebuild: TaskState::default(),
            precache: TaskState::default(),
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            vi_actions: Vec::new(),
            vi_count: 0,
            section_starts: Vec::new(),
            scroll_anim: None,
            live_width: crate::ui::LiveWidth::default(),
            card_width: None,
            compact: false,
            compact_widths: Vec::new(),
        }
    }

    /// Ask Navidrome to walk its music directories.
    ///
    /// Kept out of the sidebar's Refresh: this is the server reading every file
    /// it owns, which is minutes of work and only needed when files have
    /// actually been added to disk. Refresh reconciles against what the server
    /// already knows and is the one to reach for otherwise.
    fn scan_server(&mut self, cx: &mut Context<Self>) {
        if self.server_scan.is_running() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            self.server_scan = TaskState::Failed("Not connected to a server".into());
            cx.notify();
            return;
        };
        self.server_scan = TaskState::Running("Starting scan…".into());
        cx.notify();

        let files = Arc::new(AtomicU64::new(0));
        let watched = files.clone();
        cx.spawn(async move |this, cx| {
            let work =
                runtime::spawn_io(
                    async move { navidrome_sync::run_server_scan(&client, files).await },
                );
            let result = crate::ui::poll_until_done(cx, LIBRARY_TASK_POLL, work, |cx| {
                let seen = watched.load(Ordering::Relaxed);
                let _ = this.update(cx, |this, cx| {
                    this.server_scan = TaskState::Running(if seen == 0 {
                        "Scanning…".into()
                    } else {
                        format!("Scanning… {seen} files")
                    });
                    cx.notify();
                });
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.server_scan = match result {
                    Ok(count) => TaskState::Done(format!(
                        "Server scan finished ({count} files). Refresh to pick up new albums."
                    )),
                    // Most often error 50: Navidrome only lets admins start a
                    // scan. Say what happened rather than failing silently.
                    Err(e) => TaskState::Failed(format!("Scan failed: {e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Throw away the cached catalog and re-import every album from scratch.
    ///
    /// The escape hatch behind the incremental refresh: that one re-fetches an
    /// album's tracks when the listing's track count or duration moves, so an
    /// album re-tagged without either changing is the case it cannot see.
    fn rebuild_cache(&mut self, cx: &mut Context<Self>) {
        if self.rebuild.is_running() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            self.rebuild = TaskState::Failed("Not connected to a server".into());
            cx.notify();
            return;
        };
        self.rebuild = TaskState::Running("Reading catalog…".into());
        cx.notify();

        let db = self.library_db.clone();
        let progress = Arc::new(navidrome_sync::SyncProgress::default());
        let watched = progress.clone();
        cx.spawn(async move |this, cx| {
            let work = runtime::spawn_io(async move {
                navidrome_sync::sync_navidrome(
                    db,
                    &client,
                    None,
                    progress,
                    navidrome_sync::SyncMode::Full,
                )
                .await
            });
            let result = crate::ui::poll_until_done(cx, LIBRARY_TASK_POLL, work, |cx| {
                let (done, total) = watched.snapshot();
                let _ = this.update(cx, |this, cx| {
                    this.rebuild = TaskState::Running(if total == 0 {
                        "Reading catalog…".into()
                    } else {
                        format!("Importing {done}/{total} albums")
                    });
                    cx.notify();
                });
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.rebuild = match result {
                    Ok(()) => TaskState::Done("Library cache rebuilt.".into()),
                    Err(e) => TaskState::Failed(format!("Rebuild failed: {e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Download every album and artist cover the artwork cache is missing.
    ///
    /// Started when the switch is turned on, and by the root view after each
    /// sync while it stays on. The grids otherwise only ever cache what has
    /// been scrolled past, so a jump into the middle of the library downloads
    /// a screenful before it can draw one.
    fn precache_art(&mut self, cx: &mut Context<Self>) {
        if self.precache.is_running() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            self.precache = TaskState::Failed("Not connected to a server".into());
            cx.notify();
            return;
        };
        // The rung the grids ask for, so what lands is what they look up.
        let size = artwork::bucket(self.session.read(cx).settings.cover_size.art_px());
        self.precache = TaskState::Running("Checking cache…".into());
        cx.notify();

        let db = self.library_db.clone();
        let progress = Arc::new(art_precache::PrecacheProgress::default());
        let watched = progress.clone();
        cx.spawn(async move |this, cx| {
            let work = runtime::spawn_io(async move {
                art_precache::precache_art(db, client, size, progress).await
            });
            let result = crate::ui::poll_until_done(cx, LIBRARY_TASK_POLL, work, |cx| {
                let (done, total) = watched.snapshot();
                let _ = this.update(cx, |this, cx| {
                    // `total` is 0 until the catalog walk finishes, and a walk
                    // over a warm cache never leaves that state.
                    this.precache = TaskState::Running(if total == 0 {
                        "Checking cache…".into()
                    } else {
                        format!("Caching {done}/{total} covers")
                    });
                    cx.notify();
                });
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.precache = match result {
                    Ok(outcome) => TaskState::Done(art_precache::outcome_message(outcome)),
                    Err(e) => TaskState::Failed(format!("Cover preload failed: {e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Toggling this on starts a pass right away; the root view starts one
    /// after every sync for as long as it stays on. Turning it off stops the
    /// next pass — a download already in flight finishes, since abandoning a
    /// half-written cache entry buys nothing.
    fn set_precache_art(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.precache_art = enabled);
        self.persist(cx);
        if enabled {
            self.precache_art(cx);
        } else if !self.precache.is_running() {
            self.precache = TaskState::Idle;
        }
        cx.notify();
    }

    fn persist(&self, cx: &Context<Self>) {
        self.session.read(cx).persist_settings();
    }

    fn set_theme(&mut self, pref: ThemePref, window: &mut Window, cx: &mut Context<Self>) {
        // Notifying the session is what tells the root view to re-derive the
        // Adaptive accent: `apply_theme` below resets every colour to the
        // mode's defaults, and a silent update left the accent wiped until the
        // playing track changed — indefinitely, if playback was paused.
        self.session.update(cx, |s, cx| {
            s.settings.theme = pref;
            cx.notify();
        });
        self.persist(cx);
        apply_theme(pref, window, cx);
        cx.notify();
    }

    fn apply_transcoding(&mut self, cx: &mut Context<Self>) {
        let tc = self.session.read(cx).settings.transcoding.clone();
        self.player
            .update(cx, |p, _| p.set_transcoding(tc.to_stream_options()));
        self.persist(cx);
        cx.notify();
    }

    fn set_format(&mut self, format: Option<&str>, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            s.settings.transcoding.format = format.map(String::from);
        });
        self.apply_transcoding(cx);
    }

    fn set_bitrate(&mut self, rate: Option<u32>, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            s.settings.transcoding.max_bit_rate = rate;
        });
        self.apply_transcoding(cx);
    }

    fn set_client_titlebar(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.client_titlebar = enabled);
        self.persist(cx);
        apply_window_chrome(enabled, window, cx);
        cx.notify();
    }

    fn set_minimal_titlebar(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.minimal_titlebar = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_scrobble(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.scrobble_enabled = enabled);
        self.player
            .update(cx, |p, cx| p.set_scrobble_enabled(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_resume_playback(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.resume_playback = enabled);
        self.player
            .update(cx, |p, cx| p.set_resume_playback(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_default_shuffle(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_shuffle = enabled);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, enabled, p.queue.repeat, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_default_repeat(&mut self, mode: RepeatMode, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_repeat = mode);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        let shuffle = self.session.read(cx).settings.default_shuffle;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, shuffle, mode, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn add_local_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.dir_input.read(cx).value().to_string();
        let trimmed = path.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.session.update(cx, |s, _| {
            if !s
                .settings
                .local_music_dirs
                .iter()
                .any(|p| p.to_string_lossy() == trimmed.as_str())
            {
                s.settings
                    .local_music_dirs
                    .push(std::path::PathBuf::from(&trimmed));
            }
        });
        self.dir_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.persist(cx);
        cx.notify();
    }

    fn remove_local_dir(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            if idx < s.settings.local_music_dirs.len() {
                s.settings.local_music_dirs.remove(idx);
            }
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_cache_cap(&mut self, mb: u32, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.artwork_cache_mb = mb);
        artwork::set_cache_cap_mb(mb);
        self.persist(cx);
        cx.notify();
    }

    fn set_vi_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.vi_mode = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_reduced_motion(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.reduced_motion = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_default_page(&mut self, page: DefaultPage, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_page = page);
        self.persist(cx);
        cx.notify();
    }

    fn set_cover_size(&mut self, size: CoverSize, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.cover_size = size);
        self.persist(cx);
        cx.notify();
    }

    fn set_waveform(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.waveform_seekbar = enabled);
        self.player
            .update(cx, |p, cx| p.set_waveform_enabled(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_stream_info(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.stream_info_bar = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_detailed_volume(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.detailed_volume = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_show_queue_button(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.show_queue_button = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_adaptive_from_page(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.adaptive_from_page = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_adaptive_page_gradient(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.adaptive_page_gradient = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_selection_glow(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.selection_glow = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_show_nav_buttons(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.show_nav_buttons = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_replay_gain(&mut self, mode: ReplayGainMode, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.replay_gain = mode);
        self.player.update(cx, |p, cx| p.set_replay_gain(mode, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_fullscreen_bg(&mut self, mode: FullscreenBackground, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.fullscreen_bg = mode);
        self.persist(cx);
        cx.notify();
    }

    fn set_fullscreen_cover(&mut self, size: FullscreenCoverSize, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.fullscreen_cover = size);
        self.persist(cx);
        cx.notify();
    }

    fn set_fullscreen_volume(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.fullscreen_volume = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_queue_end(&mut self, mode: QueueEndBehavior, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.queue_end = mode);
        self.player.update(cx, |p, cx| {
            p.set_clear_on_end(mode == QueueEndBehavior::Clear, cx)
        });
        self.persist(cx);
        cx.notify();
    }

    fn toggle_track_info(
        &mut self,
        toggle: fn(&mut crate::config::TrackInfo) -> &mut bool,
        cx: &mut Context<Self>,
    ) {
        self.session.update(cx, |s, _| {
            let flag = toggle(&mut s.settings.track_info);
            *flag = !*flag;
        });
        self.persist(cx);
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |s, cx| s.logout(cx));
        cx.notify();
    }

    /// Current state of a switch, for toggling it back from the vi cursor.
    fn switch_value(&self, which: SettingsSwitch, cx: &Context<Self>) -> bool {
        let s = &self.session.read(cx).settings;
        match which {
            SettingsSwitch::ClientTitlebar => s.client_titlebar,
            SettingsSwitch::MinimalTitlebar => s.minimal_titlebar,
            SettingsSwitch::ShowNavButtons => s.show_nav_buttons,
            SettingsSwitch::AdaptiveFromPage => s.adaptive_from_page,
            SettingsSwitch::AdaptivePageGradient => s.adaptive_page_gradient,
            SettingsSwitch::SelectionGlow => s.selection_glow,
            SettingsSwitch::FullscreenVolume => s.fullscreen_volume,
            SettingsSwitch::Scrobble => s.scrobble_enabled,
            SettingsSwitch::ResumePlayback => s.resume_playback,
            SettingsSwitch::DefaultShuffle => s.default_shuffle,
            SettingsSwitch::WaveformSeekbar => s.waveform_seekbar,
            SettingsSwitch::StreamInfoBar => s.stream_info_bar,
            SettingsSwitch::DetailedVolume => s.detailed_volume,
            SettingsSwitch::ShowQueueButton => s.show_queue_button,
            SettingsSwitch::ViMode => s.vi_mode,
            SettingsSwitch::ReducedMotion => s.reduced_motion,
            SettingsSwitch::PrecacheArt => s.precache_art,
        }
    }

    /// A switch the UI would currently refuse to toggle (greyed out), so the
    /// vi cursor lands on it but Enter/Space does nothing.
    fn switch_disabled(&self, which: SettingsSwitch, cx: &Context<Self>) -> bool {
        let s = &self.session.read(cx).settings;
        match which {
            SettingsSwitch::MinimalTitlebar => !s.client_titlebar,
            SettingsSwitch::AdaptiveFromPage => s.theme != ThemePref::Adaptive,
            SettingsSwitch::AdaptivePageGradient => {
                s.theme != ThemePref::Adaptive || !s.adaptive_from_page
            }
            _ => false,
        }
    }

    /// Apply one switch's new value through the same `set_*` the mouse uses.
    fn dispatch_switch(
        &mut self,
        which: SettingsSwitch,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match which {
            SettingsSwitch::ClientTitlebar => self.set_client_titlebar(value, window, cx),
            SettingsSwitch::MinimalTitlebar => self.set_minimal_titlebar(value, cx),
            SettingsSwitch::ShowNavButtons => self.set_show_nav_buttons(value, cx),
            SettingsSwitch::AdaptiveFromPage => self.set_adaptive_from_page(value, cx),
            SettingsSwitch::AdaptivePageGradient => self.set_adaptive_page_gradient(value, cx),
            SettingsSwitch::SelectionGlow => self.set_selection_glow(value, cx),
            SettingsSwitch::FullscreenVolume => self.set_fullscreen_volume(value, cx),
            SettingsSwitch::Scrobble => self.set_scrobble(value, cx),
            SettingsSwitch::ResumePlayback => self.set_resume_playback(value, cx),
            SettingsSwitch::DefaultShuffle => self.set_default_shuffle(value, cx),
            SettingsSwitch::WaveformSeekbar => self.set_waveform(value, cx),
            SettingsSwitch::StreamInfoBar => self.set_stream_info(value, cx),
            SettingsSwitch::DetailedVolume => self.set_detailed_volume(value, cx),
            SettingsSwitch::ShowQueueButton => self.set_show_queue_button(value, cx),
            SettingsSwitch::ViMode => self.set_vi_mode(value, cx),
            SettingsSwitch::ReducedMotion => self.set_reduced_motion(value, cx),
            SettingsSwitch::PrecacheArt => self.set_precache_art(value, cx),
        }
    }

    fn dispatch_button(
        &mut self,
        button: SettingsButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            SettingsButton::Theme(p) => self.set_theme(p, window, cx),
            SettingsButton::FullscreenBg(m) => self.set_fullscreen_bg(m, cx),
            SettingsButton::FullscreenCover(s) => self.set_fullscreen_cover(s, cx),
            SettingsButton::ReplayGain(m) => self.set_replay_gain(m, cx),
            SettingsButton::QueueEnd(m) => self.set_queue_end(m, cx),
            SettingsButton::Repeat(m) => self.set_default_repeat(m, cx),
            SettingsButton::DefaultPage(p) => self.set_default_page(p, cx),
            SettingsButton::CoverSize(s) => self.set_cover_size(s, cx),
            SettingsButton::TrackInfo(f) => self.toggle_track_info(f.toggle(), cx),
            SettingsButton::Format(f) => self.set_format(f, cx),
            SettingsButton::Bitrate(r) => self.set_bitrate(r, cx),
            SettingsButton::Cache(mb) => self.set_cache_cap(mb, cx),
            SettingsButton::ScanServer => self.scan_server(cx),
            SettingsButton::RebuildCache => self.rebuild_cache(cx),
            SettingsButton::AddLocalDir => self.add_local_dir(window, cx),
            SettingsButton::RemoveLocalDir(i) => self.remove_local_dir(i, cx),
            SettingsButton::SignOut => self.sign_out(cx),
        }
    }

    /// Register one control in document order and paint its focus ring when
    /// the vi cursor is on it. The wrapper carries the scroll anchor so j/k
    /// reveals the focused row like every other vi page.
    fn vi_control<E: IntoElement + 'static>(
        &mut self,
        action: SettingsAction,
        control: E,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let index = self.vi_actions.len();
        self.vi_actions.push(action);
        let focused = self.vi_cursor == Some(index);
        let frame = div()
            .id(("vi-setting", index))
            .rounded_md()
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .child(control);
        with_focus_cursor(
            format!("vi-setting-fx-{index}"),
            frame,
            focused,
            self.session.read(cx).settings.selection_glow,
            cx,
        )
    }

    /// A labelled button-group entry (`primary` when active) that dispatches
    /// through `dispatch_button` and registers for the vi cursor.
    fn label_btn(
        &mut self,
        button: SettingsButton,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let action = button;
        let dispatch = action.clone();
        let control = Button::new(id)
            .label(label)
            .when(active, |b| b.primary())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.dispatch_button(dispatch.clone(), window, cx)
            }));
        self.vi_control(SettingsAction::Button(action), control, cx)
    }

    /// A labelled switch that dispatches back through `dispatch_switch`.
    fn vi_switch(
        &mut self,
        which: SettingsSwitch,
        id: &'static str,
        checked: bool,
        disabled: bool,
        label: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let control = Switch::new(id)
            .checked(checked)
            .disabled(disabled)
            .label(label)
            .on_click(cx.listener(move |this, &checked, window, cx| {
                this.dispatch_switch(which, checked, window, cx)
            }));
        self.vi_control(SettingsAction::Switch(which), control, cx)
    }

    /// One maintenance task row (Library section), with its button registered
    /// for the vi cursor.
    fn vi_library_task(
        &mut self,
        button: SettingsButton,
        id: &'static str,
        label: &'static str,
        description: &'static str,
        state: &TaskState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let running = state.is_running();
        let message = state.message().map(|m| m.to_string());
        let failed = matches!(state, TaskState::Failed(_));
        let action = button;
        let dispatch = action.clone();
        let control = Button::new(id)
            .outline()
            .small()
            .label(label)
            .disabled(running)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.dispatch_button(dispatch.clone(), window, cx)
            }));
        v_flex()
            .gap_1p5()
            .items_start()
            // The description is a caption like `note`'s, and goes the same way
            // in compact mode.
            .when(!self.compact, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                )
            })
            .child(h_flex().child(self.vi_control(SettingsAction::Button(action), control, cx)))
            .when_some(message, |this, message| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(if failed {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(message),
                )
            })
    }

    /// Move the vi cursor by `delta` controls, clamping and revealing the
    /// focused row.
    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        if self.vi_count == 0 {
            return;
        }
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(self.vi_count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        // A section jump still running would fight the scroll-into-view for
        // the row the cursor just landed on.
        self.scroll_anim = None;
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// True while the music-folder path box has the keyboard. A path can hold
    /// a space, and the root view's shortcuts must not eat it.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        self.dir_input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Cycle sections with `[`/`]`: jump the vi cursor to the next/prev
    /// section's first control and scroll it into view.
    pub fn vi_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.section_starts.is_empty() {
            return;
        }
        // With no cursor yet the keyboard starts from what is on screen, so
        // `[`/`]` continues from where the page was scrolled to.
        let current = self
            .vi_cursor
            .and_then(|cur| {
                self.section_starts
                    .iter()
                    .rposition(|(first, _)| *first <= cur)
            })
            .unwrap_or_else(|| self.current_section());
        let next = if delta > 0 {
            (current + 1) % self.section_starts.len()
        } else {
            (current + self.section_starts.len() - 1) % self.section_starts.len()
        };
        self.vi_cursor = Some(self.section_starts[next].0);
        self.scroll_to_section(next, window, cx);
    }

    /// Which section the quick-nav marks as current.
    ///
    /// Read from the scroll position rather than the vi cursor, so an ordinary
    /// wheel scroll moves the highlight as sections come up.
    fn current_section(&self) -> usize {
        // A jump in flight marks where it is going: the click has to read as
        // handled at once, not when the travel finishes.
        if let Some(anim) = self.scroll_anim.as_ref() {
            return anim.section;
        }
        // The compact grid's children are columns, not sections, so there is
        // nothing to measure a section's top against — and nothing to scroll
        // to either, which is why the pills are not drawn there.
        if self.compact {
            return 0;
        }
        // `map_while` rather than `filter_map`: a frame laid out before all the
        // cards have been measured yields a prefix, and dropping a hole in the
        // middle instead would shift every section after it.
        let tops: Vec<f32> = (0..self.section_starts.len())
            .map_while(|i| self.scroll.bounds_for_item(i).map(|b| f32::from(b.top())))
            .collect();
        section_for_scroll(
            &tops,
            f32::from(self.scroll.bounds().top()),
            f32::from(self.scroll.offset().y),
            f32::from(self.scroll.max_offset().height),
        )
    }

    /// Start an eased scroll to a section, replacing any jump already running.
    ///
    /// Not `ScrollAnchor::scroll_to`, which sets the offset outright: the jump
    /// between two ends of a long settings page reads as a teleport, and the
    /// pills are a navigation bar, so the travel is what shows which way the
    /// page moved.
    fn scroll_to_section(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // In the compact grid every section is already on screen, and item
        // `index` is a column rather than the section asked for — `vi_tab` has
        // moved the cursor, which is the whole of the jump there.
        if self.compact {
            return;
        }
        let Some(bounds) = self.scroll.bounds_for_item(index) else {
            return;
        };
        let max = f32::from(self.scroll.max_offset().height);
        let target = (f32::from(self.scroll.bounds().top() - bounds.top()) + SECTION_SCROLL_LEAD)
            .clamp(-max, 0.);
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        self.scroll_anim = Some(SectionScroll {
            from: f32::from(self.scroll.offset().y),
            to: target,
            section: index,
            started: Instant::now(),
            duration: transition(reduced_motion, SECTION_SCROLL_MS),
        });
        self.advance_section_scroll(window, cx);
    }

    /// Move a jump one frame along, asking for the next frame while it still
    /// has ground to cover.
    ///
    /// `Context::on_next_frame` rather than `with_animation`: what is being
    /// animated is the scroll handle's offset, which no element owns as a
    /// style, and `Window::request_animation_frame` is only callable from
    /// inside paint.
    fn advance_section_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anim) = self.scroll_anim.as_ref() else {
            return;
        };
        let t = (anim.started.elapsed().as_secs_f32() / anim.duration.as_secs_f32()).clamp(0., 1.);
        // Ease-out cubic: quick off the mark, settling into the target.
        let eased = 1. - (1. - t).powi(3);
        let y = anim.from + (anim.to - anim.from) * eased;
        let mut offset = self.scroll.offset();
        offset.y = px(y);
        self.scroll.set_offset(offset);
        if t >= 1. {
            self.scroll_anim = None;
        } else {
            cx.on_next_frame(window, |this, window, cx| {
                this.advance_section_scroll(window, cx);
            });
        }
        cx.notify();
    }

    /// Enter/Space on the focused control: toggle the switch, press the
    /// button, or focus the local-directory input.
    pub fn vi_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.vi_cursor.and_then(|i| self.vi_actions.get(i)).cloned() else {
            return;
        };
        match action {
            SettingsAction::Switch(which) => {
                if self.switch_disabled(which, cx) {
                    return;
                }
                let current = self.switch_value(which, cx);
                self.dispatch_switch(which, !current, window, cx);
            }
            SettingsAction::Button(button) => self.dispatch_button(button, window, cx),
            SettingsAction::DirInput => {
                self.dir_input.update(cx, |s, cx| s.focus(window, cx));
                cx.notify();
            }
        }
    }

    /// `i` on the settings page: focus the local-directory field.
    pub fn vi_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dir_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Muted explanatory paragraph under a control.
    ///
    /// The captions are most of the page's height, so compact mode drops them —
    /// through `hidden()` (`display: none`) rather than by not building the
    /// element, since a card is a `gap`ped column and an empty child would
    /// still leave its gap behind. Taffy excludes a `Display::None` child from
    /// the flex items entirely, gaps included.
    fn note(&self, text: &str, cx: &Context<Self>) -> gpui::AnyElement {
        if self.compact {
            return div().hidden().into_any_element();
        }
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.to_string())
            .into_any_element()
    }

    /// Bold group subheading inside a section card.
    fn subheading(&self, text: &str, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .text_sm()
            .font_semibold()
            .text_color(cx.theme().foreground)
            .child(text.to_string())
            .into_any_element()
    }

    /// Register a section in document order and open its card, ready for the
    /// controls to be chained on.
    ///
    /// Registering and building are one call because the quick-nav reads both
    /// the order of these cards among the scroll body's children and the vi
    /// control index each one starts at — kept apart, a card added without its
    /// entry silently shifts every pill after it onto the wrong section.
    ///
    /// The card is its own centred column rather than living inside one: the
    /// sections have to be *direct* children of the scrolling element for
    /// `ScrollHandle::bounds_for_item` to measure them, which is what tells the
    /// pills where each section sits.
    fn section(&mut self, title: &'static str, cx: &Context<Self>) -> gpui::Div {
        let index = self.section_starts.len();
        debug_assert_eq!(
            COMPACT_SECTIONS.get(index).map(|(t, _)| *t),
            Some(title),
            "section {index} is {title}, but COMPACT_SECTIONS says otherwise — \
             the compact grid's weights are keyed by document order"
        );
        self.section_starts.push((self.vi_actions.len(), title));
        // Compact mode's width is its column's, laid out by `compact_grid`;
        // either way it is a resolved number, see `section_width`. The
        // `w_full`/`max_w` form is only the first frame's fallback, before
        // there is a measurement to resolve one from.
        let width = match self.compact {
            // A zero is a frame with nothing measured yet, and falls back to
            // `w_full` like the scrolling column's own first frame does.
            true => self.compact_widths.get(index).copied().filter(|w| *w > 0.),
            false => self.card_width,
        };
        v_flex()
            .map(|card| match width {
                Some(w) => card.w(px(w)),
                None => card.w_full().max_w(px(SECTION_MAX_W)),
            })
            .mx_auto()
            .flex_none()
            .map(|card| match self.compact {
                true => card.gap_2().p_3(),
                false => card.gap_3().p_4(),
            })
            .rounded_lg()
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .bg(cx.theme().sidebar)
            .child(div().text_sm().font_medium().child(title))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scroll-into-view runs here, not in `vi_move`: the anchor's origin
        // is only as fresh as the last paint, and from a key handler that is
        // the row the cursor just LEFT — going up, the focused row landed one
        // row above the viewport and the highlight vanished.
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        // Resolved before the cards are built, since every `section` reads it.
        // Through `LiveWidth` rather than the handle's own bounds: those are
        // last frame's layout, so during a resize drag the cards would rewrap a
        // frame behind the window edge.
        let measured = f32::from(self.scroll.bounds().size.width);
        let body = self.live_width.resolve(measured, window);
        // The body's width is only known once this frame has been laid out, and
        // the measurement landing dirties nothing — so the unmeasured frame's
        // fallback layout stays up until something else happens to repaint the
        // view, which with nothing playing is the next input. Opening the page
        // showed its fallback (one centred column — exactly what the compact
        // grid is not) for 333ms against the 20ms it takes to ask for the frame
        // that has the measurement, both timed in the running app.
        //
        // `request_animation_frame`, not `refresh`: a refresh is a no-op while
        // the window is drawing, which is exactly when a view renders — it only
        // marks the window dirty from outside a draw.
        if measured <= 0. {
            window.request_animation_frame();
        }
        self.card_width = section_width(body);
        self.vi_actions.clear();
        let (
            theme,
            format,
            bitrate,
            client_titlebar,
            minimal_titlebar,
            scrobble_enabled,
            default_shuffle,
            default_repeat,
            artwork_cache_mb,
            account,
        ) = {
            let s = &self.session.read(cx).settings;
            (
                s.theme,
                s.transcoding.format.clone(),
                s.transcoding.max_bit_rate,
                s.client_titlebar,
                s.minimal_titlebar,
                s.scrobble_enabled,
                s.default_shuffle,
                s.default_repeat,
                s.artwork_cache_mb,
                s.server
                    .as_ref()
                    .map(|srv| (srv.url.clone(), srv.username.clone())),
            )
        };
        let (default_page, cover_size, track_info, waveform, stream_info, detailed_volume) = {
            let s = &self.session.read(cx).settings;
            (
                s.default_page,
                s.cover_size,
                s.track_info.clone(),
                s.waveform_seekbar,
                s.stream_info_bar,
                s.detailed_volume,
            )
        };
        let show_queue_button = self.session.read(cx).settings.show_queue_button;
        let show_nav_buttons = self.session.read(cx).settings.show_nav_buttons;
        let adaptive_from_page = self.session.read(cx).settings.adaptive_from_page;
        let adaptive_page_gradient = self.session.read(cx).settings.adaptive_page_gradient;
        let resume_playback = self.session.read(cx).settings.resume_playback;
        let local_music_dirs = self.session.read(cx).settings.local_music_dirs.clone();
        let replay_gain = self.session.read(cx).settings.replay_gain;
        let queue_end = self.session.read(cx).settings.queue_end;
        let fullscreen_bg = self.session.read(cx).settings.fullscreen_bg;
        let fullscreen_volume = self.session.read(cx).settings.fullscreen_volume;
        let fullscreen_cover = self.session.read(cx).settings.fullscreen_cover;
        let vi_mode = self.session.read(cx).settings.vi_mode;
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        let selection_glow = self.session.read(cx).settings.selection_glow;
        let server_scan_state = self.server_scan.clone();
        let rebuild_state = self.rebuild.clone();
        let precache_art = self.session.read(cx).settings.precache_art;
        let precache_state = self.precache.clone();

        // Rebuilt from scratch each render: `section` re-registers every card
        // it opens, in the order they are laid out.
        self.section_starts.clear();

        // The grid is planned before a single card is built: each one is laid
        // out at its column's width, and `section` reads that as it goes.
        // Account is the only section that can be absent, and it is last in
        // `COMPACT_SECTIONS`, so the present sections are a prefix of it.
        //
        // The plan is also the decision: a window that can hold the whole page
        // at once gets it, and one that cannot gets the scrolling column with
        // its captions back. There is no setting — the page is one or the other
        // depending on the window it is in.
        let present = COMPACT_SECTIONS.len() - usize::from(account.is_none());
        let weights: Vec<u16> = COMPACT_SECTIONS[..present]
            .iter()
            .map(|(_, w)| *w)
            .collect();
        // The height is the scroll body's own, i.e. last frame's — it decides
        // how many columns the page wraps into, and a window resized vertically
        // re-plans a frame later. Only the *width* needs `LiveWidth`'s
        // same-frame treatment, since that is what the cards are laid out at.
        let grid = compact_grid(&weights, body, f32::from(self.scroll.bounds().size.height));
        let compact = !grid.is_empty();
        self.compact = compact;
        self.compact_widths = vec![0.; present];
        for column in &grid {
            for &section in &column.sections {
                self.compact_widths[section] = column.width;
            }
        }

        // Window
        let window_section = self
            .section("Window", cx)
            .child(self.vi_switch(
                SettingsSwitch::ClientTitlebar,
                "client-titlebar",
                client_titlebar,
                false,
                "Use in-app title bar",
                cx,
            ))
            .child(self.note(
                "Disable to use your desktop environment's native window \
                 decorations (recommended on some Linux setups).",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::MinimalTitlebar,
                "minimal-titlebar",
                minimal_titlebar,
                !client_titlebar,
                "Minimal title bar",
                cx,
            ))
            .child(self.note(
                "Drops the app name and the separator and paints the bar \
                 in the window background, leaving only the window \
                 controls. Needs the in-app title bar.",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::ShowNavButtons,
                "show-nav-buttons",
                show_nav_buttons,
                false,
                "Back / forward buttons",
                cx,
            ))
            .child(self.note(
                "Hiding them keeps history navigation on the mouse's \
                 side buttons and the keyboard ([ / ] normally, h / l \
                 in vi mode).",
                cx,
            ));

        // Appearance
        let appearance_section = self
            .section("Appearance", cx)
            .child(self.subheading("Theme", cx))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::Theme(ThemePref::System),
                        "System",
                        "System",
                        theme == ThemePref::System,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Theme(ThemePref::Light),
                        "Light",
                        "Light",
                        theme == ThemePref::Light,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Theme(ThemePref::Dark),
                        "Dark",
                        "Dark",
                        theme == ThemePref::Dark,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Theme(ThemePref::Adaptive),
                        "Adaptive (from cover)",
                        "Adaptive (from cover)",
                        theme == ThemePref::Adaptive,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Theme(ThemePref::Custom),
                        "Custom (theme.json)",
                        "Custom (theme.json)",
                        theme == ThemePref::Custom,
                        cx,
                    )),
            )
            .child(self.subheading("Album page", cx))
            .child(self.vi_switch(
                SettingsSwitch::AdaptiveFromPage,
                "adaptive-from-page",
                adaptive_from_page,
                theme != ThemePref::Adaptive,
                "Album pages tint from their own cover",
                cx,
            ))
            .child(self.note(
                "An album page takes its colour from the album \
                 you're looking at. The sidebar, player and \
                 fullscreen keep the playing track's.",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::AdaptivePageGradient,
                "adaptive-page-gradient",
                adaptive_page_gradient,
                theme != ThemePref::Adaptive || !adaptive_from_page,
                "Wash the album header in that colour",
                cx,
            ))
            .child(self.subheading("Selection", cx))
            .child(self.vi_switch(
                SettingsSwitch::SelectionGlow,
                "selection-glow",
                selection_glow,
                false,
                "Glow focused cards",
                cx,
            ))
            .child(self.note(
                "Off is just a primary border; on adds a filled background and glow.",
                cx,
            ))
            .child(self.subheading("Fullscreen", cx))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::FullscreenBg(FullscreenBackground::Gradient),
                        "Gradient",
                        "Gradient",
                        fullscreen_bg == FullscreenBackground::Gradient,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenBg(FullscreenBackground::Vibrant),
                        "Vibrant",
                        "Vibrant",
                        fullscreen_bg == FullscreenBackground::Vibrant,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenBg(FullscreenBackground::BlurredArt),
                        "Blurred art",
                        "Blurred art",
                        fullscreen_bg == FullscreenBackground::BlurredArt,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenBg(FullscreenBackground::Animated),
                        "Animated",
                        "Animated",
                        fullscreen_bg == FullscreenBackground::Animated,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenBg(FullscreenBackground::Solid),
                        "Solid",
                        "Solid",
                        fullscreen_bg == FullscreenBackground::Solid,
                        cx,
                    )),
            )
            .child(self.vi_switch(
                SettingsSwitch::FullscreenVolume,
                "fullscreen-volume",
                fullscreen_volume,
                false,
                "Volume slider in fullscreen player",
                cx,
            ))
            .child(self.subheading("Fullscreen cover size", cx))
            .child(self.note(
                "How far the cover grows on a big window. Fixed keeps it at the \
                 size a small window draws; the controls beside it keep their \
                 room whichever you pick.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::FullscreenCover(FullscreenCoverSize::Fixed),
                        "Fixed",
                        FullscreenCoverSize::Fixed.label(),
                        fullscreen_cover == FullscreenCoverSize::Fixed,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenCover(FullscreenCoverSize::Medium),
                        "Medium",
                        FullscreenCoverSize::Medium.label(),
                        fullscreen_cover == FullscreenCoverSize::Medium,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenCover(FullscreenCoverSize::Large),
                        "Large",
                        FullscreenCoverSize::Large.label(),
                        fullscreen_cover == FullscreenCoverSize::Large,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::FullscreenCover(FullscreenCoverSize::Huge),
                        "Huge",
                        FullscreenCoverSize::Huge.label(),
                        fullscreen_cover == FullscreenCoverSize::Huge,
                        cx,
                    )),
            );

        // Playback
        let playback_section = self
            .section("Playback", cx)
            .child(self.vi_switch(
                SettingsSwitch::Scrobble,
                "scrobble",
                scrobble_enabled,
                false,
                "Scrobble plays to server",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::ResumePlayback,
                "resume-playback",
                resume_playback,
                false,
                "Resume where you left off",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::DefaultShuffle,
                "default-shuffle",
                default_shuffle,
                false,
                "Shuffle on by default",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::WaveformSeekbar,
                "waveform-seekbar",
                waveform,
                false,
                "Waveform progress bar",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::StreamInfoBar,
                "stream-info-bar",
                stream_info,
                false,
                "Stream info in player bar",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::DetailedVolume,
                "detailed-volume",
                detailed_volume,
                false,
                "Detailed volume control",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::ShowQueueButton,
                "show-queue-button",
                show_queue_button,
                false,
                "Queue button in player bar",
                cx,
            ))
            .child(self.note(
                "The waveform seek bar downloads each track a second time to \
                 decode it, so it uses extra bandwidth.",
                cx,
            ))
            .child(self.subheading("ReplayGain", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.label_btn(
                        SettingsButton::ReplayGain(ReplayGainMode::Off),
                        "Off",
                        "Off",
                        replay_gain == ReplayGainMode::Off,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::ReplayGain(ReplayGainMode::Track),
                        "Track",
                        "Track",
                        replay_gain == ReplayGainMode::Track,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::ReplayGain(ReplayGainMode::Album),
                        "Album",
                        "Album",
                        replay_gain == ReplayGainMode::Album,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::ReplayGain(ReplayGainMode::Auto),
                        "Auto",
                        "Auto",
                        replay_gain == ReplayGainMode::Auto,
                        cx,
                    )),
            )
            .child(self.note(
                "Evens out perceived volume using each file's ReplayGain tags. \
                 Track normalizes every song; Album keeps an album's relative \
                 loudness; Auto uses album gain when playing a whole album and \
                 track gain otherwise. The player bar shows the applied gain \
                 (and the auto-chosen mode).",
                cx,
            ))
            .child(self.subheading("When the queue ends", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.label_btn(
                        SettingsButton::QueueEnd(QueueEndBehavior::Keep),
                        "Keep queue",
                        "Keep queue",
                        queue_end == QueueEndBehavior::Keep,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::QueueEnd(QueueEndBehavior::Clear),
                        "Clear queue",
                        "Clear queue",
                        queue_end == QueueEndBehavior::Clear,
                        cx,
                    )),
            )
            .child(self.note(
                "Keep leaves the finished queue and last track in the player \
                 bar; Clear empties the queue and resets the player bar.",
                cx,
            ))
            .child(self.subheading("Default repeat", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.label_btn(
                        SettingsButton::Repeat(RepeatMode::Off),
                        "Off",
                        "Off",
                        default_repeat == RepeatMode::Off,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Repeat(RepeatMode::All),
                        "All",
                        "All",
                        default_repeat == RepeatMode::All,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Repeat(RepeatMode::One),
                        "One",
                        "One",
                        default_repeat == RepeatMode::One,
                        cx,
                    )),
            );

        // Browsing
        let browsing_section = self
            .section("Browsing", cx)
            .child(self.subheading("Open at startup", cx))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::DefaultPage(DefaultPage::Albums),
                        "Albums",
                        "Albums",
                        default_page == DefaultPage::Albums,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::DefaultPage(DefaultPage::Artists),
                        "Artists",
                        "Artists",
                        default_page == DefaultPage::Artists,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::DefaultPage(DefaultPage::Favorites),
                        "Favorites",
                        "Favorites",
                        default_page == DefaultPage::Favorites,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::DefaultPage(DefaultPage::Recent),
                        "Recent",
                        "Recent",
                        default_page == DefaultPage::Recent,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::DefaultPage(DefaultPage::Radio),
                        "Radio",
                        "Radio",
                        default_page == DefaultPage::Radio,
                        cx,
                    )),
            )
            .child(self.subheading("Cover size", cx))
            .child(self.note(
                "Roughly how big album covers are — the exact size and the number \
                 per row adapt to the window so the grid fills its width.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::CoverSize(CoverSize::Small),
                        "Small",
                        "Small",
                        cover_size == CoverSize::Small,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::CoverSize(CoverSize::Medium),
                        "Medium",
                        "Medium",
                        cover_size == CoverSize::Medium,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::CoverSize(CoverSize::Large),
                        "Large",
                        "Large",
                        cover_size == CoverSize::Large,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::CoverSize(CoverSize::ExtraLarge),
                        "Extra large",
                        "Extra large",
                        cover_size == CoverSize::ExtraLarge,
                        cx,
                    )),
            )
            .child(self.subheading("Track info", cx))
            .child(self.note("Shown next to song titles in album and playlist views.", cx))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Artist),
                        "Artist",
                        "Artist",
                        track_info.artist,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Album),
                        "Album",
                        "Album",
                        track_info.album,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Year),
                        "Year",
                        "Year",
                        track_info.year,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Genre),
                        "Genre",
                        "Genre",
                        track_info.genre,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Bitrate),
                        "Bitrate",
                        "Bitrate",
                        track_info.bitrate,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::TrackInfo(TrackInfoField::Plays),
                        "Play count",
                        "Play count",
                        track_info.plays,
                        cx,
                    )),
            )
            .child(self.vi_switch(
                SettingsSwitch::ViMode,
                "vi-mode",
                vi_mode,
                false,
                "Vi-style keyboard navigation",
                cx,
            ))
            .child(self.vi_switch(
                SettingsSwitch::ReducedMotion,
                "reduced-motion",
                reduced_motion,
                false,
                "Reduce motion",
                cx,
            ))
            .child(self.note(
                "j/k sidebar+grid, h/l history, : commands, / search, ? help",
                cx,
            ));

        // Streaming
        let streaming_section = self
            .section("Streaming", cx)
            .child(self.subheading("Format", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.label_btn(
                        SettingsButton::Format(None),
                        "Original",
                        "Original",
                        format.is_none(),
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Format(Some("mp3")),
                        "MP3",
                        "MP3",
                        format.as_deref() == Some("mp3"),
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Format(Some("opus")),
                        "Opus",
                        "Opus",
                        format.as_deref() == Some("opus"),
                        cx,
                    )),
            )
            .child(self.note(
                "Transcoding helps low-bandwidth connections but disables accurate \
                 seeking. Original streams the source file.",
                cx,
            ))
            .child(self.subheading("Max bitrate", cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.label_btn(
                        SettingsButton::Bitrate(None),
                        "No limit",
                        "No limit",
                        bitrate.is_none(),
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Bitrate(Some(128)),
                        "128k",
                        "128k",
                        bitrate == Some(128),
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Bitrate(Some(192)),
                        "192k",
                        "192k",
                        bitrate == Some(192),
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Bitrate(Some(320)),
                        "320k",
                        "320k",
                        bitrate == Some(320),
                        cx,
                    )),
            );

        // Library: the two maintenance jobs, the local music folders, and the
        // artwork cache.
        let library_card = self.section("Library", cx);

        // Local-directory rows live in document order now, so their remove
        // buttons land in the right place for j/k navigation.
        let mut dir_rows: Vec<gpui::AnyElement> = Vec::new();
        for (i, p) in local_music_dirs.iter().enumerate() {
            let p_str = p.to_string_lossy().to_string();
            dir_rows.push(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_sm().truncate().child(p_str))
                    .child(
                        self.vi_control(
                            SettingsAction::Button(SettingsButton::RemoveLocalDir(i)),
                            Button::new(("rm-local-dir", i))
                                .ghost()
                                .xsmall()
                                .icon(gpui_component::Icon::new(gpui_component::IconName::Close))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_local_dir(i, cx);
                                })),
                            cx,
                        ),
                    )
                    .into_any_element(),
            );
        }

        let library_section = library_card
            .child(self.note(
                "Refresh in the sidebar picks up albums the server already knows \
                 about. These two are slower and rarely needed.",
                cx,
            ))
            .child(self.vi_library_task(
                SettingsButton::ScanServer,
                "scan-server",
                "Scan server library",
                "Have the server re-read its music folders. Needed after adding \
                 files to the server itself. Requires an admin account.",
                &server_scan_state,
                cx,
            ))
            // Two description-then-button blocks in a row read as one
            // paragraph without something between them.
            .child(crate::ui::divider())
            .child(self.vi_library_task(
                SettingsButton::RebuildCache,
                "rebuild-cache",
                "Rebuild local cache",
                "Re-import every album from the server. Fixes a cache that has \
                 drifted, e.g. after re-tagging music in place.",
                &rebuild_state,
                cx,
            ))
            .child(crate::ui::divider())
            .child(self.subheading("Local music", cx))
            .child(self.note("Directories scanned for local music files", cx))
            .child(v_flex().gap_1().children(dir_rows))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.vi_control(
                        SettingsAction::DirInput,
                        div().w(px(300.)).child(Input::new(&self.dir_input)),
                        cx,
                    ))
                    .child(
                        self.vi_control(
                            SettingsAction::Button(SettingsButton::AddLocalDir),
                            Button::new("add-local-dir")
                                .label("Add")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_local_dir(window, cx);
                                })),
                            cx,
                        ),
                    ),
            )
            .child(crate::ui::divider())
            .child(self.subheading("Artwork cache", cx))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.label_btn(
                        SettingsButton::Cache(64),
                        ("cache-mb", 64_u32),
                        "64 MB",
                        artwork_cache_mb == 64,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Cache(128),
                        ("cache-mb", 128_u32),
                        "128 MB",
                        artwork_cache_mb == 128,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Cache(256),
                        ("cache-mb", 256_u32),
                        "256 MB",
                        artwork_cache_mb == 256,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Cache(512),
                        ("cache-mb", 512_u32),
                        "512 MB",
                        artwork_cache_mb == 512,
                        cx,
                    ))
                    .child(self.label_btn(
                        SettingsButton::Cache(1024),
                        ("cache-mb", 1024_u32),
                        "1024 MB",
                        artwork_cache_mb == 1024,
                        cx,
                    )),
            )
            .child(crate::ui::divider())
            .child(self.vi_switch(
                SettingsSwitch::PrecacheArt,
                "precache-art",
                precache_art,
                false,
                "Preload all cover art",
                cx,
            ))
            .child(self.note(
                "Download every album and artist cover in the background, so the \
                 grids draw from disk instead of fetching as you scroll. Runs \
                 after each library sync and only fetches what is missing. Large \
                 libraries will fill the cache above — raise it if covers start \
                 reappearing.",
                cx,
            ))
            .when_some(precache_state.message().map(str::to_string), |this, msg| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(if matches!(precache_state, TaskState::Failed(_)) {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(msg),
                )
            });

        // Account (only when connected, so it stays the last section).
        let account_section = account.map(|(url, user)| {
            self.section("Account", cx)
                .child(
                    h_flex()
                        .justify_between()
                        .items_start()
                        .child(
                            v_flex()
                                .gap_1()
                                .min_w_0()
                                .child(div().text_sm().child(user))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(url),
                                ),
                        )
                        .child(
                            self.vi_control(
                                SettingsAction::Button(SettingsButton::SignOut),
                                Button::new("sign-out")
                                    .outline()
                                    .danger()
                                    .label("Sign out")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sign_out(cx);
                                    })),
                                cx,
                            ),
                        ),
                )
                .into_any_element()
        });

        // The sections in document order, which is what both layouts and the
        // compact plan index by. `Option` so a column can take its own out
        // without cloning an element.
        let mut cards: Vec<Option<gpui::AnyElement>> = vec![
            Some(window_section.into_any_element()),
            Some(appearance_section.into_any_element()),
            Some(playback_section.into_any_element()),
            Some(browsing_section.into_any_element()),
            Some(streaming_section.into_any_element()),
            Some(library_section.into_any_element()),
        ];
        cards.extend(account_section.map(Some));

        // Quick-nav pills, built from the sections that were actually
        // registered above — the filled one is whatever the page is scrolled
        // to, or a jump's destination while one is running. The grid has
        // everything on screen at once, so there is nowhere to jump.
        let current_section = self.current_section();
        let mut pills = h_flex().gap_1().flex_wrap();
        if !compact {
            for (i, (_, title)) in self.section_starts.iter().enumerate() {
                pills = pills.child(
                    Button::new(("settings-pill", i))
                        .ghost()
                        .xsmall()
                        .label(*title)
                        .when(i == current_section, |b| b.primary())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.scroll_to_section(i, window, cx);
                        })),
                );
            }
        }

        // Same header shape as the catalog pages: the page name and its nav
        // strip on one row, inset from the window edge, with the scrolling
        // content under it.
        let result = v_flex()
            .id("settings-scroll")
            .size_full()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .flex_wrap()
                    .child(div().text_lg().child("Settings"))
                    .child(pills),
            )
            .child(
                v_flex()
                    .id("settings-scroll-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    // Nothing is measured on the first frame, so which layout
                    // the window wants is not known yet either — it is laid out
                    // (that is what produces the measurement) and not painted,
                    // rather than showing a column that the next frame may
                    // replace with a grid. Opacity, not `hidden()`: the latter
                    // is `display: none`, which skips the layout this frame
                    // exists for.
                    .when(measured <= 0., |body| body.opacity(0.))
                    // The wheel is the user overruling a jump in flight, and
                    // the highlight has to follow the new position rather than
                    // the target that was abandoned. Notifying here is also
                    // what recomputes it: gpui scrolls the element without
                    // re-rendering the view.
                    .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                        this.scroll_anim = None;
                        cx.notify();
                    }))
                    .px_4()
                    .gap_4()
                    // The grid is meant to end well short of the bottom, so it
                    // does not need the scrolling column's run-out.
                    .pb(px(if compact { 16. } else { 148. }))
                    .map(|scroll_body| match compact {
                        // Columns are top-aligned rather than stretched: a
                        // short column ending level with a tall one would draw
                        // a card taller than its own contents.
                        //
                        // The scroll stays on the body as a last resort. A
                        // window too short for the grid is still a window the
                        // page has to be usable in, and clipping the bottom
                        // card is worse than the setting not quite keeping its
                        // promise there.
                        true => {
                            scroll_body.child(
                                h_flex()
                                    .items_start()
                                    // The grid is laid out at the width it wants
                                    // rather than the window's, so a wide window
                                    // leaves it centred instead of stretching
                                    // the cards across the whole page.
                                    .justify_center()
                                    .gap(px(COMPACT_GAP))
                                    .children(grid.iter().map(|column| {
                                        v_flex()
                                            .flex_none()
                                            .w(px(column.width))
                                            .gap(px(COMPACT_GAP))
                                            .children(
                                                column
                                                    .sections
                                                    .iter()
                                                    .filter_map(|&i| cards[i].take()),
                                            )
                                    })),
                            )
                        }
                        // The sections have to stay *direct* children here:
                        // `bounds_for_item` is what tells the pills where each
                        // one sits.
                        false => scroll_body.children(cards.iter_mut().filter_map(Option::take)),
                    }),
            );

        self.vi_count = self.vi_actions.len();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seven sections 300px apart in a viewport whose top edge is at 100.
    const VIEWPORT_TOP: f32 = 100.;
    fn tops() -> Vec<f32> {
        (0..7).map(|i| VIEWPORT_TOP + 300. * i as f32).collect()
    }

    #[test]
    fn unscrolled_marks_the_first_section() {
        assert_eq!(section_for_scroll(&tops(), VIEWPORT_TOP, 0., 1800.), 0);
    }

    #[test]
    fn a_section_takes_over_once_it_passes_the_activation_line() {
        let tops = tops();
        // The second card's top is 300 below the viewport's: one pixel short of
        // the line it is still the first section's, one past it and it is not.
        let just_short = -(300. - SECTION_ACTIVE_LINE) + 1.;
        assert_eq!(
            section_for_scroll(&tops, VIEWPORT_TOP, just_short, 1800.),
            0
        );
        assert_eq!(
            section_for_scroll(&tops, VIEWPORT_TOP, just_short - 2., 1800.),
            1
        );
    }

    #[test]
    fn the_bottom_of_the_page_marks_the_last_section() {
        // A final section too short to ever reach the activation line: without
        // the bottom rule its pill could not be reached by scrolling at all.
        let tops = tops();
        assert_eq!(section_for_scroll(&tops, VIEWPORT_TOP, -1800., 1800.), 6);
        assert_eq!(section_for_scroll(&tops, VIEWPORT_TOP, -1797., 1800.), 6);
    }

    #[test]
    fn a_card_fits_inside_the_scroll_body_at_every_width() {
        // Overrunning it is the failure the definite width exists to avoid, so
        // the arithmetic gets checked rather than trusted.
        for body in [300., 480., 672., 700., 1200., 3000.] {
            let w = section_width(body).expect("a measured body resolves a width");
            assert!(
                w <= body - 2. * SECTION_BODY_PAD || w == SECTION_MIN_W,
                "{body}px body produced a {w}px card"
            );
            assert!(w <= SECTION_MAX_W, "{body}px body produced a {w}px card");
        }
    }

    #[test]
    fn a_roomy_page_caps_the_card_and_a_tight_one_shrinks_it() {
        assert_eq!(section_width(3000.), Some(SECTION_MAX_W));
        // 672 = the cap plus the body's padding: the first width that fills it.
        assert_eq!(section_width(672.), Some(SECTION_MAX_W));
        assert_eq!(section_width(500.), Some(468.));
    }

    #[test]
    fn a_card_never_shrinks_below_the_floor() {
        // Past this the card is allowed to overrun instead: a 40px-wide card
        // would be unreadable, and the window is unusable at that size anyway.
        assert_eq!(section_width(80.), Some(SECTION_MIN_W));
    }

    #[test]
    fn nothing_measured_yet_resolves_no_width() {
        // The first frame has no measurement, and falls back to `w_full`.
        assert_eq!(section_width(0.), None);
        assert_eq!(section_width(-1.), None);
    }

    #[test]
    fn a_page_that_does_not_scroll_marks_the_first_section() {
        // max_offset 0 must not read as "already at the bottom".
        assert_eq!(section_for_scroll(&tops(), VIEWPORT_TOP, 0., 0.), 0);
    }

    #[test]
    fn nothing_measured_yet_marks_the_first_section() {
        assert_eq!(section_for_scroll(&[], VIEWPORT_TOP, 0., 0.), 0);
    }

    /// Every section present, i.e. signed in.
    fn weights() -> Vec<u16> {
        COMPACT_SECTIONS.iter().map(|(_, w)| *w).collect()
    }

    fn placed(grid: &[GridColumn]) -> Vec<usize> {
        grid.iter()
            .flat_map(|c| c.sections.iter().copied())
            .collect()
    }

    fn column_heights(grid: &[GridColumn], weights: &[u16]) -> Vec<f32> {
        grid.iter()
            .map(|c| c.sections.iter().map(|&i| card_height(weights[i])).sum())
            .collect()
    }

    /// A content area tall enough to hold the page in two columns.
    const TALL: f32 = 1150.;

    #[test]
    fn the_grid_places_every_section_in_page_order() {
        for (body, height) in [(1200., TALL), (2600., TALL), (2600., 900.)] {
            let grid = compact_grid(&weights(), body, height);
            assert!(!grid.is_empty(), "{body}x{height} planned no grid");
            // Flattened column by column, the grid *is* the page in order:
            // read down one column and on to the next and the sections come in
            // the order the scrolling page has them.
            assert_eq!(
                placed(&grid),
                (0..COMPACT_SECTIONS.len()).collect::<Vec<_>>(),
                "{body}x{height} reordered, dropped or duplicated a section"
            );
        }
        // Signed out, the Account card is absent and the rest still fit.
        let grid = compact_grid(&weights()[..6], 1200., TALL);
        assert_eq!(placed(&grid), (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn the_window_decides_the_layout_on_its_own() {
        let w = weights();
        // Room for the whole page at once: the grid.
        assert!(!compact_grid(&w, 1200., TALL).is_empty());
        // Too short for any column count to fit — the scrolling page, captions
        // and all, rather than a grid that has given up the one thing it is
        // for.
        assert!(compact_grid(&w, 2600., 520.).is_empty());
        // Too narrow for a second column. One column is not a grid: it is the
        // scrolling page with its captions taken away.
        assert!(compact_grid(&w, 700., 4000.).is_empty());
        // Nothing measured yet: neither answer is known.
        assert!(compact_grid(&w, 0., TALL).is_empty());
        assert!(compact_grid(&w, 1200., 0.).is_empty());
    }

    #[test]
    fn a_grid_is_never_a_single_column() {
        for body in [400., 700., 900., 1200., 2600.] {
            for height in [300., 700., TALL, 4000.] {
                let grid = compact_grid(&weights(), body, height);
                assert!(
                    grid.len() != 1,
                    "{body}x{height} produced a one-column grid"
                );
            }
        }
    }

    #[test]
    fn the_grid_comes_out_closest_to_square() {
        // The two failures the aspect rule sits between: the widest count the
        // window allows leaves every card in the top strip of a tall window,
        // and the fewest that fit leave one column down the middle of a wide
        // one. Neither end is picked here.
        let w = weights();
        let heights: Vec<f32> = w.iter().copied().map(card_height).collect();
        for (body, height) in [(1200., TALL), (2600., TALL), (1500., 2200.)] {
            let cols = compact_grid(&w, body, height).len();
            assert!(cols >= 2, "{body}x{height} planned no grid");
            let usable = body - 2. * SECTION_BODY_PAD;
            let aspect = |c: usize| {
                let wide = columns_width(c, usable) + COMPACT_GAP * (c - 1) as f32;
                (wide / tallest_run(&heights, c)).ln().abs()
            };
            for other in 2..=COMPACT_COL_MAX {
                if other != cols && tallest_run(&heights, other) <= height {
                    assert!(
                        aspect(cols) <= aspect(other),
                        "{body}x{height} took {cols} columns over a squarer {other}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_taller_window_takes_fewer_columns() {
        let w = weights();
        let tall = compact_grid(&w, 2600., TALL);
        let short = compact_grid(&w, 2600., 900.);
        assert!(
            tall.len() < short.len(),
            "{} columns in a tall window against {} in a shorter one",
            tall.len(),
            short.len()
        );
        // And every column of either is inside the height it was given —
        // fitting comes before the aspect.
        for (grid, height) in [(&tall, TALL), (&short, 900.)] {
            for h in column_heights(grid, &w) {
                assert!(h <= height, "a column came out {h}px tall in {height}px");
            }
        }
    }

    #[test]
    fn the_columns_come_out_close_to_the_same_height() {
        // A contiguous split cannot balance as well as packing by size, but it
        // must not leave one column half again its neighbour either.
        let w = weights();
        let heights = column_heights(&compact_grid(&w, 2600., 900.), &w);
        let min = heights.iter().copied().fold(f32::MAX, f32::min);
        let max = heights.iter().copied().fold(0., f32::max);
        assert!(max <= 1.5 * min, "columns came out at {heights:?}");
    }

    #[test]
    fn the_grid_never_overruns_the_page_and_is_not_stretched_across_it() {
        for (body, height) in [(1000., 4000.), (1200., TALL), (1600., 900.), (2600., 900.)] {
            let grid = compact_grid(&weights(), body, height);
            let cols = grid.len();
            assert!(cols >= 2, "{body}x{height} planned no grid");
            let spent: f32 = grid.iter().map(|c| c.width).sum::<f32>()
                + COMPACT_GAP * (cols - 1) as f32
                + 2. * SECTION_BODY_PAD;
            assert!(
                spent <= body + 0.5,
                "{cols} columns spent {spent} of a {body}px body"
            );
            for column in &grid {
                // Past the target the cards read as pages side by side; the
                // leftover width is left as margin and the grid is centred in
                // it instead. A single column may still run over it by the
                // asymmetry's share, which only moves width between columns.
                assert!(
                    column.width <= COMPACT_COL_TARGET * COMPACT_SHARE_MAX + 0.5,
                    "{body}x{height} produced a {}px column",
                    column.width
                );
            }
        }
        // A window wide enough to stretch into leaves the grid centred: it
        // spends the target and no more.
        let wide = compact_grid(&weights(), 2600., 900.);
        let spent: f32 = wide.iter().map(|c| c.width).sum();
        assert!(spent <= COMPACT_COL_TARGET * wide.len() as f32 + 0.5);
    }

    #[test]
    fn the_columns_are_asymmetric_but_not_lopsided() {
        let grid = compact_grid(&weights(), 2600., TALL);
        let widths: Vec<f32> = grid.iter().map(|c| c.width).collect();
        let min = widths.iter().copied().fold(f32::MAX, f32::min);
        let max = widths.iter().copied().fold(0., f32::max);
        assert!(max > min, "the columns came out even: {widths:?}");
        assert!(
            max / min <= COMPACT_SHARE_MAX / COMPACT_SHARE_MIN,
            "{widths:?} is wider apart than the clamp allows"
        );
    }

    #[test]
    fn columns_never_go_under_the_label_floor() {
        // A column under `COMPACT_COL_MIN` pushes its switch labels out through
        // the card, so the asymmetry gives way to an even split rather than
        // taking one column below it.
        for (body, height) in [(1000., 4000.), (1200., TALL), (1600., 900.), (2600., 700.)] {
            for column in compact_grid(&weights(), body, height) {
                assert!(
                    column.width >= COMPACT_COL_MIN,
                    "{body}x{height} produced a {}px column",
                    column.width
                );
            }
        }
    }

    #[test]
    fn never_more_columns_than_sections() {
        // Two sections cannot fill four columns, and an empty column would be
        // a gap in the middle of the grid.
        let grid = compact_grid(&weights()[..2], 2600., 900.);
        assert_eq!(grid.len(), 2);
        assert!(grid.iter().all(|c| !c.sections.is_empty()));
    }

    #[test]
    fn a_split_keeps_its_runs_contiguous_and_full() {
        // `split_runs` is what the page order rests on: every run a block of
        // consecutive cards, no run empty, however the cap lands.
        let heights: Vec<f32> = weights().iter().copied().map(card_height).collect();
        for cols in 1..=heights.len() {
            let runs = split_runs(&heights, cols);
            assert_eq!(runs.len(), cols, "{cols} columns produced {}", runs.len());
            let mut next = 0;
            for run in &runs {
                assert!(!run.is_empty(), "empty run in {runs:?}");
                assert_eq!(run[0], next, "run does not continue the page: {runs:?}");
                assert!(run.windows(2).all(|w| w[1] == w[0] + 1));
                next = run.last().expect("a non-empty run") + 1;
            }
            assert_eq!(next, heights.len());
        }
    }

    #[test]
    fn a_jump_lands_inside_its_own_targets_band() {
        // A jump leaves its section's top `SECTION_SCROLL_LEAD` below the
        // viewport edge, which has to stay short of the activation line: with
        // the two constants the other way round the pill that was just clicked
        // is not the one that ends up filled.
        let tops = tops();
        let landed = -(300. - SECTION_SCROLL_LEAD);
        assert_eq!(section_for_scroll(&tops, VIEWPORT_TOP, landed, 1800.), 1);
    }
}
