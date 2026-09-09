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
    CoverSize, DefaultPage, FullscreenBackground, QueueEndBehavior, ReplayGainMode, ThemePref,
};
use crate::services::library_db::LibraryDb;
use crate::services::{artwork, navidrome_sync, runtime};
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

/// Which of the 16 on/off switches this is — enough to toggle it through the
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
}

/// A button-group entry or a standalone settings button.
#[derive(Clone)]
enum SettingsButton {
    Theme(ThemePref),
    FullscreenBg(FullscreenBackground),
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
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            )
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
    fn note(&self, text: &str, cx: &Context<Self>) -> gpui::AnyElement {
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
        self.section_starts.push((self.vi_actions.len(), title));
        v_flex()
            // See `section_width`: the resolved number is what makes the card
            // tall enough for its own wrapping paragraphs. The `w_full`/`max_w`
            // form is only the first frame's fallback, before there is a
            // measurement to resolve one from.
            .map(|card| match self.card_width {
                Some(w) => card.w(px(w)),
                None => card.w_full().max_w(px(SECTION_MAX_W)),
            })
            .mx_auto()
            .flex_none()
            .gap_3()
            .p_4()
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
        let body = self
            .live_width
            .resolve(f32::from(self.scroll.bounds().size.width), window);
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
        let vi_mode = self.session.read(cx).settings.vi_mode;
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        let selection_glow = self.session.read(cx).settings.selection_glow;
        let server_scan_state = self.server_scan.clone();
        let rebuild_state = self.rebuild.clone();

        // Rebuilt from scratch each render: `section` re-registers every card
        // it opens, in the order they are laid out.
        self.section_starts.clear();

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
            ));

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
                "Album cover size — albums per row adapt to the window width.",
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
            );

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

        // Quick-nav pills, built from the sections that were actually
        // registered above — the filled one is whatever the page is scrolled
        // to, or a jump's destination while one is running.
        let current_section = self.current_section();
        let mut pills = h_flex().gap_1().flex_wrap();
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
                    .pb(px(148.))
                    .child(window_section)
                    .child(appearance_section)
                    .child(playback_section)
                    .child(browsing_section)
                    .child(streaming_section)
                    .child(library_section)
                    .when_some(account_section, |this, section| this.child(section)),
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
