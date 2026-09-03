//! Root view: main app layout
//! (sidebar | content | optional queue panel / player bar).

use gpui::{
    Animation, AnimationExt as _, AsyncWindowContext, Context, ElementId, Entity, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, MouseButton, NavigationDirection, Render, SharedString,
    WeakEntity, Window, div, ease_out_quint, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{DefaultPage, ThemePref};
use crate::services::{
    artwork, library_db::LibraryDb, local_library::LocalScanner, navidrome_sync, runtime,
};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::radio::RadioState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::album_detail::{AlbumDetailEvent, AlbumDetailView};
use crate::ui::albums::{AlbumsEvent, AlbumsView};
use crate::ui::artists::{ArtistDetailEvent, ArtistDetailView, ArtistsEvent, ArtistsView};
use crate::ui::favorites::{FavoritesEvent, FavoritesView};
use crate::ui::fullscreen_player::{FullscreenEvent, FullscreenPlayer};
use crate::ui::local_album_detail::LocalAlbumDetailView;
use crate::ui::local_music::{LocalMusicEvent, LocalMusicView};
use crate::ui::player_bar::{PlayerBar, PlayerBarEvent};
use crate::ui::playlist_detail::{PlaylistDetailEvent, PlaylistDetailView};
use crate::ui::queue_panel::QueuePanel;
use crate::ui::radio::RadioView;
use crate::ui::recent::RecentView;
use crate::ui::search_bar::{SearchBar, SearchBarEvent};
use crate::ui::settings::SettingsView;
use crate::ui::sidebar::{NavSection, SidebarAction, SidebarModel, render_sidebar};

/// How often a running refresh resamples its workers' progress counters.
const REFRESH_POLL: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq)]
enum FocusRegion {
    Content,
    Sidebar,
    PlayerBar,
}

#[derive(Clone, Copy, PartialEq)]
enum KeyboardMode {
    Normal,
    Insert,
    Command,
}

/// Navigation-section order for vi-mode j/k cycling.
const NAV_ORDER: &[NavSection] = &[
    NavSection::Albums,
    NavSection::Artists,
    NavSection::Favorites,
    NavSection::Recent,
    NavSection::Radio,
    NavSection::LocalMusic,
    NavSection::Settings,
];

#[derive(Clone)]
enum NavEntry {
    Section(NavSection),
    Album(String),
    LocalAlbum(String),
    Artist(String),
    Playlist(String),
}

enum Content {
    Albums(Entity<AlbumsView>),
    Artists(Entity<ArtistsView>),
    ArtistDetail(Entity<ArtistDetailView>),
    AlbumDetail(Entity<AlbumDetailView>),
    Favorites(Entity<FavoritesView>),
    LocalAlbumDetail(Entity<LocalAlbumDetailView>),
    Playlist(Entity<PlaylistDetailView>),
    Radio(Entity<RadioView>),
    Settings(Entity<SettingsView>),
    Recent(Entity<RecentView>),
    LocalMusic(Entity<LocalMusicView>),
}

/// Where a manual library refresh has got to.
///
/// The steps have different shapes — a disk walk with an open-ended file count,
/// a listing pass with nothing to count at all, and a per-album fetch with a
/// real denominator — so the sidebar needs to know which one it is looking at
/// rather than just "busy".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshStage {
    #[default]
    Idle,
    /// Walking the local music directories; carries files seen so far.
    LocalScan(usize),
    /// Fetching the album listing and diffing it against the cache. Usually
    /// over in a second or two, and the only stage a refresh that finds nothing
    /// new ever reaches.
    Comparing,
    /// Fetching tracks for the albums that changed; `(done, total)`.
    Import(usize, usize),
}

impl RefreshStage {
    /// Fraction complete, or `None` for the stages with no known total — those
    /// get an indeterminate bar instead of a lying one.
    pub fn fraction(self) -> Option<f32> {
        match self {
            RefreshStage::Import(done, total) if total > 0 => {
                Some((done as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            RefreshStage::Idle => "Refreshing…".into(),
            RefreshStage::LocalScan(0) => "Scanning local files…".into(),
            RefreshStage::LocalScan(n) => format!("Scanning local files… {n}"),
            RefreshStage::Comparing => "Checking for new music…".into(),
            // Zero albums to fetch means the listing found nothing new, so this
            // stage passes straight through without ever showing a count.
            RefreshStage::Import(_, 0) => "Checking for new music…".into(),
            RefreshStage::Import(done, total) => format!("Fetching {done}/{total} albums"),
        }
    }
}

pub struct RootView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    radio: Entity<RadioState>,
    player_bar: Entity<PlayerBar>,
    queue_panel: Entity<QueuePanel>,
    fullscreen: Entity<FullscreenPlayer>,
    search_bar: Entity<SearchBar>,
    content: Option<Content>,
    section: Option<NavSection>,
    active_playlist: Option<String>,
    history: Vec<NavEntry>,
    forward_stack: Vec<NavEntry>,
    current_entry: Option<NavEntry>,
    in_history_restore: bool,
    show_queue: bool,
    show_fullscreen: bool,
    was_connected: bool,
    /// Library selection at last render, to rebuild views on change.
    last_libraries: Vec<String>,
    /// Sidebar library switcher folded away.
    libraries_collapsed: bool,
    /// Sidebar playlist list folded away.
    playlists_collapsed: bool,
    /// Whole sidebar folded down to an icon rail.
    sidebar_collapsed: bool,
    /// Window orientation at the last frame. A window turning portrait folds
    /// the sidebar for itself — there is no room for a 210px rail beside the
    /// content — and turning landscape again restores whatever the user had
    /// persisted, so the automatic fold never eats their choice.
    portrait: Option<bool>,
    /// Shared music library database.
    library_db: Arc<LibraryDb>,
    /// The two heavy catalog views, kept alive across navigation. Rebuilding
    /// them on every tab switch re-fetched the entire listing and dropped the
    /// scroll position; they're dropped only when the data behind them changes
    /// (connect/disconnect, library selection) — see `invalidate_catalog_views`.
    albums_view: Option<Entity<AlbumsView>>,
    artists_view: Option<Entity<ArtistsView>>,
    recent_view: Option<Entity<RecentView>>,
    /// Whether initial local/navidrome sync has been triggered.
    scan_started: bool,
    sync_started: bool,
    /// A manual library refresh is in flight (sidebar row is disabled).
    refreshing: bool,
    /// What that refresh is doing right now, for the sidebar's progress bar.
    refresh_stage: RefreshStage,
    /// New-playlist dialog state.
    new_playlist_open: bool,
    new_pl_name: Entity<InputState>,
    new_pl_desc: Entity<InputState>,
    /// Art key (album-scoped) whose colour the Adaptive theme accent was last
    /// derived from, to avoid re-extracting on every player tick. The outer
    /// `None` means nothing has been resolved yet and the next tick should try
    /// again — distinct from `Some(None)`, a track that genuinely has no cover.
    /// Collapsing the two made a coverless track (a local file with no art, a
    /// radio stream) match the initial state and short-circuit forever, so the
    /// accent was never applied at all.
    adaptive_cover: Option<Option<String>>,
    /// The accent last extracted and applied. Kept because `Theme::change`
    /// resets every colour to the mode's defaults, and nothing re-derives the
    /// accent afterwards — see `restore_adaptive_accent`.
    adaptive_accent: Option<gpui::Hsla>,
    focus_handle: FocusHandle,
    // -- Vi-mode state --
    vi_enabled: bool,
    mode: KeyboardMode,
    focus_region: FocusRegion,
    cmd_input: Entity<InputState>,
    vi_selected: usize,
    show_vi_help: bool,
    /// Transient feedback for `:` commands, shown next to the mode indicator.
    vi_status: Option<String>,

    // -- First-time setup wizard state --
    setup_enable_navidrome: bool,
    setup_server_url: Entity<InputState>,
    setup_username: Entity<InputState>,
    setup_password: Entity<InputState>,
    setup_enable_local: bool,
    setup_local_dirs: Vec<PathBuf>,
    setup_dir_input: Entity<InputState>,
    setup_error: Option<String>,
}

impl RootView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        library_db: Arc<LibraryDb>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let player_bar = cx.new(|cx| PlayerBar::new(player.clone(), session.clone(), window, cx));
        let queue_panel = cx.new(|cx| QueuePanel::new(player.clone(), cx));
        let radio = crate::state::radio::init(session.clone(), cx);
        let fullscreen = cx.new(|cx| FullscreenPlayer::new(player.clone(), session.clone(), cx));
        let search_bar = cx.new(|cx| SearchBar::new(session.clone(), player.clone(), window, cx));

        cx.subscribe(&search_bar, |this: &mut Self, _, event, cx| match event {
            SearchBarEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
            SearchBarEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
        })
        .detach();

        let cmd_input = cx.new(|cx| InputState::new(window, cx));

        cx.subscribe(&player_bar, |this: &mut Self, _, event, cx| {
            match event {
                PlayerBarEvent::ToggleQueue => this.show_queue = !this.show_queue,
                PlayerBarEvent::ToggleFullscreen => {
                    if this.show_fullscreen {
                        // Animate out; the Close event flips the flag off.
                        this.fullscreen.update(cx, |f, cx| f.begin_close(cx));
                    } else {
                        this.show_fullscreen = true;
                        this.fullscreen.update(cx, |f, cx| f.reset_for_open(cx));
                    }
                }
                PlayerBarEvent::OpenAlbum(id) => {
                    this.show_fullscreen = false;
                    this.open_album(id.clone(), cx);
                }
                PlayerBarEvent::OpenArtist(id) => {
                    this.show_fullscreen = false;
                    this.open_artist(id.clone(), cx);
                }
            }
            cx.notify();
        })
        .detach();

        cx.subscribe(&fullscreen, |this: &mut Self, _, event, cx| {
            let FullscreenEvent::Close = event;
            this.show_fullscreen = false;
            cx.notify();
        })
        .detach();

        // Re-render the sidebar's playlist list when playlists change.
        cx.observe(&playlists, |_, _, cx| cx.notify()).detach();

        // -- First-time setup inputs --
        let setup_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://music.example.com"));
        let setup_user = cx.new(|cx| InputState::new(window, cx).placeholder("username"));
        let setup_pass = cx.new(|cx| InputState::new(window, cx).placeholder("password"));
        let setup_dir = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/music"));

        // Enter in command-mode input runs the command.
        cx.subscribe(&cmd_input, |this: &mut Self, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.execute_command(cx);
            }
        })
        .detach();

        // Adaptive theme: recolour accents when the playing track changes.
        cx.observe(&player, |this: &mut Self, _, cx| {
            this.maybe_update_adaptive_accent(cx);
        })
        .detach();

        let new_pl_name = cx.new(|cx| InputState::new(window, cx).placeholder("Playlist name"));
        let new_pl_desc =
            cx.new(|cx| InputState::new(window, cx).placeholder("Description (optional)"));
        // Enter in the name field creates the playlist.
        cx.subscribe(
            &new_pl_name,
            |this: &mut Self, _, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit_new_playlist(cx);
                }
            },
        )
        .detach();

        // React to connect/disconnect and library switches: build/tear down
        // content views and keep the player's API client fresh.
        cx.observe(&session, |this: &mut Self, session, cx| {
            let connected = session.read(cx).status == ConnectionStatus::Connected;
            let libraries = session.read(cx).library_ids.clone();
            if connected != this.was_connected {
                this.was_connected = connected;
                let client = session.read(cx).client.clone();
                this.player.update(cx, |p, cx| p.set_client(client, cx));
                if connected {
                    this.last_libraries = libraries;
                    // The catalog views were built before the connect and are
                    // already painting the cached listing; hand them the client
                    // instead of rebuilding. A rebuild would blank their covers
                    // and reset the scroll a second after the window appeared.
                    this.resume_catalog_views(cx);
                } else {
                    this.content = None;
                    this.invalidate_catalog_views();
                }
            } else if connected && libraries != this.last_libraries {
                // Library selection changed: rebuild the current catalog view.
                this.last_libraries = libraries;
                this.invalidate_catalog_views();
                if let Some(section) = this.section {
                    this.navigate(section, None, cx);
                }
            }

            // Pick up vi-mode toggle from settings.
            this.sync_vi_mode(session.read(cx).settings.vi_mode, cx);

            let has_local = !session.read(cx).settings.local_music_dirs.is_empty();
            // Kick off the local scanner once. When server + local dirs are
            // both configured, the local scan starts immediately without
            // waiting for the Navidrome connect.
            let should_scan = connected || has_local;
            if should_scan && !this.scan_started {
                this.scan_started = true;
                let dirs: Vec<_> = session.read(cx).settings.local_music_dirs.clone();
                if !dirs.is_empty() {
                    let lib_db = this.library_db.clone();
                    cx.spawn(async move |this, cx| {
                        let scanner = Arc::new(LocalScanner::new(lib_db));
                        loop {
                            let s = scanner.clone();
                            let d = dirs.clone();
                            // The scan is synchronous (walkdir + tag parse +
                            // SQLite): it must go to the blocking pool, or it
                            // holds an IO worker for its whole duration and
                            // the first catalog request queues behind it.
                            let _ = runtime::spawn_blocking_io(move || s.scan(&d)).await;
                            let _ = this.update(cx, |_, _| {});
                            // Periodic background rescan every 5 minutes. Timed
                            // on gpui's executor — this task has no tokio
                            // reactor, so `tokio::time::sleep` would panic here.
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(300))
                                .await;
                        }
                    })
                    .detach();
                }
            }

            // Navidrome sync is gated separately from the local scan: it needs
            // a live client, and the scan flag flips on the pre-connect pass
            // whenever local dirs are configured — sharing one flag meant the
            // sync never ran at all for those users.
            if connected
                && !this.sync_started
                && let Some(client) = session.read(cx).client.clone()
            {
                this.sync_started = true;
                let db = this.library_db.clone();
                cx.spawn(async move |this, cx| {
                    // Held back so it doesn't compete with the album grid's own
                    // first page. The grid renders the rows this wrote on the
                    // *previous* run while it waits. Incremental, so on a
                    // settled library it costs the listing and nothing else —
                    // the first run against an empty cache is the expensive one.
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(30))
                        .await;
                    let progress = Arc::new(navidrome_sync::SyncProgress::default());
                    let _ = runtime::spawn_io(async move {
                        navidrome_sync::sync_navidrome(
                            db,
                            &client,
                            None,
                            progress,
                            navidrome_sync::SyncMode::Incremental,
                        )
                        .await
                    })
                    .await;
                    let _ = this.update(cx, |_, _| {});
                })
                .detach();
            }

            this.open_default_page(None, cx);

            // Pick up theme switches (e.g. to/from Adaptive) from Settings.
            this.maybe_update_adaptive_accent(cx);
            cx.notify();
        })
        .detach();

        // Paint the start page on the first frame. The session observer would
        // also open it, but it only fires when the connect resolves — which is
        // a network round-trip away, and until then the window sat empty even
        // though the cached catalog was right there to draw.
        cx.defer_in(window, |this: &mut Self, window, cx| {
            this.open_default_page(Some(window), cx);
            // Same reason: the queue is restored inside the player's own
            // constructor, before this view exists, so no notify ever reaches
            // the observer above and the restored track's accent was missing
            // until the user touched playback.
            this.maybe_update_adaptive_accent(cx);
        });

        // Sidebar fold state is persisted, so restore it instead of reopening
        // every section on each start.
        let libraries_collapsed = session.read(cx).settings.sidebar_libraries_collapsed;
        let playlists_collapsed = session.read(cx).settings.sidebar_playlists_collapsed;
        let sidebar_collapsed = session.read(cx).settings.sidebar_collapsed;

        let vi_enabled = session.read(cx).settings.vi_mode;
        Self {
            session,
            player,
            playlists,
            radio,
            player_bar,
            queue_panel,
            fullscreen,
            search_bar,
            vi_enabled,
            mode: KeyboardMode::Normal,
            focus_region: FocusRegion::Content,
            cmd_input,
            vi_selected: 0,
            show_vi_help: false,
            vi_status: None,
            content: None,
            section: None,
            active_playlist: None,
            history: Vec::new(),
            forward_stack: Vec::new(),
            current_entry: None,
            in_history_restore: false,
            show_queue: false,
            show_fullscreen: false,
            was_connected: false,
            library_db,
            albums_view: None,
            artists_view: None,
            recent_view: None,
            scan_started: false,
            sync_started: false,
            refreshing: false,
            refresh_stage: RefreshStage::Idle,
            last_libraries: Vec::new(),
            libraries_collapsed,
            playlists_collapsed,
            sidebar_collapsed,
            portrait: None,
            new_playlist_open: false,
            new_pl_name,
            new_pl_desc,
            adaptive_cover: None,
            adaptive_accent: None,
            focus_handle: cx.focus_handle(),
            setup_enable_navidrome: true,
            setup_server_url: setup_url,
            setup_username: setup_user,
            setup_password: setup_pass,
            setup_enable_local: true,
            setup_local_dirs: Vec::new(),
            setup_dir_input: setup_dir,
            setup_error: None,
        }
    }

    /// Fold the sidebar down to its icon rail, or unfold it, and remember the
    /// choice. Persisted even in a portrait window: the value is what a return
    /// to landscape restores.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        let collapsed = self.sidebar_collapsed;
        self.session.update(cx, |session, _| {
            session.settings.sidebar_collapsed = collapsed;
            session.persist_settings();
        });
        cx.notify();
    }

    /// Fold the sidebar when the window turns portrait, unfold it back to the
    /// persisted preference when it turns landscape again.
    ///
    /// Only the *transition* acts, never every frame: otherwise the automatic
    /// fold would fight the user the moment they unfolded it in a portrait
    /// window. A landscape start therefore keeps the persisted value rather
    /// than forcing the sidebar open.
    fn apply_orientation(&mut self, window: &Window, cx: &mut Context<Self>) {
        let size = window.viewport_size();
        // A zero-sized viewport is a frame laid out before the window has one;
        // treating it as landscape would fire a spurious transition.
        if size.width <= px(0.) || size.height <= px(0.) {
            return;
        }
        let portrait = size.height > size.width;
        if self.portrait == Some(portrait) {
            return;
        }
        let first_frame = self.portrait.is_none();
        self.portrait = Some(portrait);
        if portrait {
            self.sidebar_collapsed = true;
        } else if !first_frame {
            self.sidebar_collapsed = self.session.read(cx).settings.sidebar_collapsed;
        }
    }

    /// Rescan everything on demand: the server's own media scan, the local
    /// directories, then the catalog re-import.
    ///
    /// The background schedules (5 min local rescan, one Navidrome resync per
    /// session) are the only thing that ever refreshed the DB, so music added
    /// after launch needed a restart to appear.
    ///
    /// This deliberately does *not* ask the server to rescan its disk. That is
    /// the slow, occasional operation and it lives in Settings; refreshing the
    /// local cache against what the server already knows is the fast, frequent
    /// one, and pairing them made every refresh cost a full server walk.
    ///
    /// The steps run in sequence — the local scan is blocking work and the
    /// import issues one `getAlbum` per changed album, and letting them overlap
    /// starves the two IO workers.
    fn refresh_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.refresh_stage = RefreshStage::Idle;
        cx.notify();
        let dirs: Vec<PathBuf> = self.session.read(cx).settings.local_music_dirs.clone();
        let client = self.session.read(cx).client.clone();
        let db = self.library_db.clone();
        cx.spawn_in(window, async move |this, cx| {
            if !dirs.is_empty() {
                let scanner = Arc::new(LocalScanner::new(db.clone()));
                let watched = scanner.clone();
                let scan = runtime::spawn_blocking_io(move || scanner.scan(&dirs));
                Self::drive_progress(&this, cx, scan, move || {
                    RefreshStage::LocalScan(watched.progress())
                })
                .await;
            }
            if let Some(client) = client {
                let _ = this.update(cx, |this, cx| {
                    this.refresh_stage = RefreshStage::Comparing;
                    cx.notify();
                });
                let progress = Arc::new(navidrome_sync::SyncProgress::default());
                let watched = progress.clone();
                let sync = runtime::spawn_io(async move {
                    navidrome_sync::sync_navidrome(
                        db,
                        &client,
                        None,
                        progress,
                        navidrome_sync::SyncMode::Incremental,
                    )
                    .await
                });
                Self::drive_progress(&this, cx, sync, move || {
                    let (done, total) = watched.snapshot();
                    // `total` is 0 until the listing pass finishes; showing
                    // "0/0 albums" for that stretch reads as broken.
                    if total == 0 {
                        RefreshStage::Comparing
                    } else {
                        RefreshStage::Import(done, total)
                    }
                })
                .await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.refreshing = false;
                this.refresh_stage = RefreshStage::Idle;
                // The catalog views hold the rows they were built from and
                // never reload on their own; drop them so the next visit
                // re-seeds from what the refresh just wrote.
                let on_catalog_page = matches!(
                    this.content,
                    Some(
                        Content::Albums(_)
                            | Content::Artists(_)
                            | Content::Recent(_)
                            | Content::LocalMusic(_)
                    )
                );
                this.invalidate_catalog_views();
                // Redraw the current page only if it is one of those listings —
                // a detail page the user opened meanwhile must not be replaced
                // by a grid because a background refresh finished.
                if on_catalog_page && let Some(section) = this.section {
                    this.navigate(section, Some(window), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Await `work` while republishing `stage()` into the sidebar a few times a
    /// second.
    async fn drive_progress<T: Send + 'static>(
        this: &WeakEntity<Self>,
        cx: &mut AsyncWindowContext,
        work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
        stage: impl Fn() -> RefreshStage,
    ) {
        let result = crate::ui::poll_until_done(cx, REFRESH_POLL, work, |cx| {
            let stage = stage();
            let _ = this.update(cx, |this, cx| {
                if this.refresh_stage != stage {
                    this.refresh_stage = stage;
                    cx.notify();
                }
            });
        })
        .await;
        if let Err(e) = result {
            tracing::warn!("library refresh step failed: {e}");
        }
    }

    /// When the Adaptive theme is active, derive the accent colour from the
    /// current track's cover art and recolour the interactive surfaces. Cheap
    /// no-op for other themes or when the cover hasn't changed.
    ///
    /// This is always the *playing* track: the app's chrome — sidebar, player
    /// bar, sliders, fullscreen overlay — belongs to playback. An album page
    /// showing a different album tints itself locally instead
    /// (`Settings::adaptive_from_page`, handled in `album_detail.rs`).
    fn maybe_update_adaptive_accent(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).settings.theme != ThemePref::Adaptive {
            self.adaptive_cover = None;
            self.adaptive_accent = None;
            return;
        }
        // Before any of the short-circuits below, since every one of them would
        // otherwise leave a wiped theme wiped.
        self.restore_adaptive_accent(cx);
        let song = self.player.read(cx).current_song().cloned();
        // Local tracks carry a cache hash instead of a server cover id, and no
        // client to fetch from — resolve those straight off disk, or the accent
        // would keep whatever the last streamed album left behind.
        if let Some(song) = song.as_ref()
            && song.local_path.is_some()
        {
            let path = song
                .cover_art
                .as_deref()
                .and_then(crate::services::local_library::local_art_path)
                .filter(|p| p.exists());
            let key = song.cover_art.clone();
            if self.adaptive_cover.as_ref() == Some(&key) {
                return;
            }
            self.adaptive_cover = Some(key);
            // No art on disk: the accent restored above stays. Dropping to the
            // theme default would repaint the whole UI stock near-white, which
            // reads as the feature having broken.
            let Some(path) = path else { return };
            self.accent_from_file(path, cx);
            return;
        }

        // Album-scoped key: per-song cover ids would re-extract the accent (and
        // recolour the whole UI) on every track of the same album.
        let cover = song.as_ref().and_then(artwork::song_cover);
        let key = cover.as_ref().map(|(_, key)| key.clone());
        if self.adaptive_cover.as_ref() == Some(&key) {
            return;
        }
        self.adaptive_cover = Some(key);
        // Nothing playing, or a track with no cover: keep the last accent.
        let Some((cover_id, key)) = cover else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            // Pre-connect. Leave the key unresolved or the session observer's
            // pass once the client lands would short-circuit on it and the
            // accent would never be derived for the restored track.
            self.adaptive_cover = None;
            return;
        };
        cx.spawn(async move |this, cx| {
            let accent = runtime::spawn_io(async move {
                let path = artwork::fetch_as(client, cover_id, key, 64).await?;
                let bytes = std::fs::read(&path)?;
                crate::ui::accent_from_cover_bytes(&bytes)
                    .ok_or_else(|| anyhow::anyhow!("cover art did not decode"))
            })
            .await;
            Self::apply_or_retry(this, accent, cx).await;
        })
        .detach();
    }

    /// Extract an accent from a cover already on disk. The read and the decode
    /// both block, so they go to the blocking pool.
    fn accent_from_file(&self, path: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let accent = runtime::spawn_blocking_io(move || {
                let bytes = std::fs::read(&path)?;
                crate::ui::accent_from_cover_bytes(&bytes)
                    .ok_or_else(|| anyhow::anyhow!("cover art did not decode"))
            })
            .await;
            Self::apply_or_retry(this, accent, cx).await;
        })
        .detach();
    }

    /// Put the last extracted accent back if the global theme has drifted off
    /// it. `Theme::change` — which every theme re-apply goes through, including
    /// picking Adaptive in Settings — resets *every* colour to the mode's
    /// defaults, and nothing else re-derives the accent: the UI sat on the
    /// stock near-white primary until the playing track changed. Comparing
    /// against the stored accent keeps this a no-op (and skips the window
    /// refresh) on the ticks where nothing was wiped.
    fn restore_adaptive_accent(&self, cx: &mut Context<Self>) {
        if let Some(accent) = self.adaptive_accent
            && cx.theme().primary != accent
        {
            crate::ui::apply_adaptive_accent(cx, accent);
        }
    }

    /// Apply an extracted accent, or clear the remembered cover key so the next
    /// player notify retries. Without the reset a single failed fetch pinned the
    /// UI to the previous album's colour until the track changed again.
    async fn apply_or_retry(
        this: gpui::WeakEntity<Self>,
        accent: anyhow::Result<gpui::Hsla>,
        cx: &mut gpui::AsyncApp,
    ) {
        match accent {
            Ok(accent) => {
                let _ = this.update(cx, |view, _| view.adaptive_accent = Some(accent));
                let _ = cx.update(|cx| crate::ui::apply_adaptive_accent(cx, accent));
            }
            Err(_) => {
                let _ = this.update(cx, |view, _| view.adaptive_cover = None);
            }
        }
    }

    /// Drop the retained catalog views so the next visit rebuilds and refetches
    /// them. Call whenever what they'd fetch changes — connect/disconnect or a
    /// library-selection change — since they never reload on their own.
    fn invalidate_catalog_views(&mut self) {
        self.albums_view = None;
        self.artists_view = None;
        self.recent_view = None;
    }

    /// Tell the retained catalog views a client exists now. They fetch once at
    /// construction and never again on their own, so a view built during the
    /// pre-connect window would otherwise sit on its cached rows forever.
    fn resume_catalog_views(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.albums_view.clone() {
            view.update(cx, |v, cx| v.client_ready(cx));
        }
        if let Some(view) = self.artists_view.clone() {
            view.update(cx, |v, cx| v.client_ready(cx));
        }
    }

    /// Open the configured start page if nothing is showing yet.
    ///
    /// Called once at construction *and* from the session observer: the connect
    /// is async, so waiting for the observer to fire left the window empty for
    /// as long as the ping took. Gated on a configured server or local dirs —
    /// with neither, `render` shows the setup wizard instead.
    fn open_default_page(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.content.is_some() {
            return;
        }
        let s = self.session.read(cx);
        let has_local = !s.settings.local_music_dirs.is_empty();
        let has_server = s.settings.server.is_some();
        if !has_local && !has_server {
            return;
        }
        let start = if has_local && !has_server {
            NavSection::LocalMusic
        } else {
            match s.settings.default_page {
                DefaultPage::Albums => NavSection::Albums,
                DefaultPage::Artists => NavSection::Artists,
                DefaultPage::Favorites => NavSection::Favorites,
                DefaultPage::Recent => NavSection::Recent,
                DefaultPage::Radio => NavSection::Radio,
            }
        };
        self.navigate(start, window, cx);
    }

    fn navigate(
        &mut self,
        section: NavSection,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.vi_clear_cursors(cx);
        self.current_entry = Some(NavEntry::Section(section));
        self.section = Some(section);
        self.active_playlist = None;
        self.content = Some(match section {
            // Reused across visits: subscribe only on first construction,
            // otherwise every switch would stack another event handler.
            NavSection::Albums => Content::Albums(match self.albums_view.clone() {
                Some(view) => view,
                None => {
                    let view = cx.new(|cx| {
                        AlbumsView::new(
                            self.session.clone(),
                            self.player.clone(),
                            self.playlists.clone(),
                            self.library_db.clone(),
                            cx,
                        )
                    });
                    cx.subscribe(&view, |this: &mut Self, _, event, cx| match event {
                        AlbumsEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
                        AlbumsEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
                    })
                    .detach();
                    self.albums_view = Some(view.clone());
                    view
                }
            }),
            NavSection::Artists => Content::Artists(match self.artists_view.clone() {
                Some(view) => view,
                None => {
                    let view = cx.new(|cx| {
                        ArtistsView::new(self.session.clone(), self.library_db.clone(), cx)
                    });
                    cx.subscribe(&view, |this: &mut Self, _, event, cx| {
                        let ArtistsEvent::OpenArtist(id) = event;
                        this.open_artist(id.clone(), cx);
                    })
                    .detach();
                    self.artists_view = Some(view.clone());
                    view
                }
            }),
            NavSection::Favorites => {
                let view =
                    cx.new(|cx| FavoritesView::new(self.session.clone(), self.player.clone(), cx));
                cx.subscribe(&view, |this: &mut Self, _, event, cx| match event {
                    FavoritesEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
                    FavoritesEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
                })
                .detach();
                Content::Favorites(view)
            }
            NavSection::Recent => Content::Recent(match self.recent_view.clone() {
                Some(view) => view,
                None => {
                    let view =
                        cx.new(|cx| RecentView::new(self.player.clone(), self.session.clone(), cx));
                    self.recent_view = Some(view.clone());
                    view
                }
            }),
            NavSection::Radio => {
                let Some(window) = window else {
                    return; // radio's add-station form needs a window
                };
                let view = cx
                    .new(|cx| RadioView::new(self.radio.clone(), self.player.clone(), window, cx));
                Content::Radio(view)
            }
            NavSection::LocalMusic => {
                let view = cx.new(|cx| {
                    LocalMusicView::new(
                        self.library_db.clone(),
                        self.player.clone(),
                        self.session.clone(),
                        cx,
                    )
                });
                cx.subscribe(&view, |this: &mut Self, _, event, cx| {
                    let LocalMusicEvent::OpenAlbum(id) = event;
                    this.open_local_album(id.clone(), cx);
                })
                .detach();
                Content::LocalMusic(view)
            }
            NavSection::Settings => {
                let Some(window) = window else {
                    return;
                };
                let view = cx.new(|cx| {
                    SettingsView::new(
                        self.session.clone(),
                        self.player.clone(),
                        self.library_db.clone(),
                        window,
                        cx,
                    )
                });
                Content::Settings(view)
            }
        });
        cx.notify();
    }

    fn push_history(&mut self) {
        if self.in_history_restore {
            return;
        }
        if let Some(entry) = self.current_entry.clone() {
            self.history.push(entry);
            self.forward_stack.clear();
        }
    }

    fn navigate_push(&mut self, section: NavSection, window: &mut Window, cx: &mut Context<Self>) {
        self.push_history();
        self.navigate(section, Some(window), cx);
    }

    fn nav_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prev) = self.history.pop() else {
            return;
        };
        if let Some(current) = self.current_entry.take() {
            self.forward_stack.push(current);
        }
        self.restore_entry(prev, window, cx);
    }

    fn nav_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.forward_stack.pop() else {
            return;
        };
        if let Some(current) = self.current_entry.take() {
            self.history.push(current);
        }
        self.restore_entry(next, window, cx);
    }

    /// Back/forward history buttons drawn above the content area.
    ///
    /// Purely a second way in to `nav_back`/`nav_forward` — the mouse's
    /// navigation buttons and the `[`/`]` keys stay wired whether or not this
    /// row is shown, so hiding it costs no navigation.
    fn render_nav_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_back = !self.history.is_empty();
        let can_forward = !self.forward_stack.is_empty();
        h_flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .pt_2()
            .child(
                Button::new("nav-back")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ArrowLeft))
                    .tooltip("Back")
                    .disabled(!can_back)
                    .on_click(cx.listener(|this, _, window, cx| this.nav_back(window, cx))),
            )
            .child(
                Button::new("nav-forward")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ArrowRight))
                    .tooltip("Forward")
                    .disabled(!can_forward)
                    .on_click(cx.listener(|this, _, window, cx| this.nav_forward(window, cx))),
            )
    }

    fn restore_entry(&mut self, entry: NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.in_history_restore = true;
        self.current_entry = Some(entry.clone());
        match entry {
            NavEntry::Section(section) => self.navigate(section, Some(window), cx),
            NavEntry::Album(id) => self.open_album(id, cx),
            NavEntry::LocalAlbum(id) => self.open_local_album(id, cx),
            NavEntry::Artist(id) => self.open_artist(id, cx),
            NavEntry::Playlist(id) => self.open_playlist(id, window, cx),
        }
        self.in_history_restore = false;
    }

    fn open_album(&mut self, id: String, cx: &mut Context<Self>) {
        self.push_history();
        self.current_entry = Some(NavEntry::Album(id.clone()));
        let view = cx.new(|cx| {
            AlbumDetailView::new(
                self.session.clone(),
                self.player.clone(),
                self.playlists.clone(),
                self.library_db.clone(),
                id,
                cx,
            )
        });
        cx.subscribe(&view, |this: &mut Self, _, event, cx| {
            let AlbumDetailEvent::OpenArtist(id) = event;
            this.open_artist(id.clone(), cx);
        })
        .detach();
        self.content = Some(Content::AlbumDetail(view));
        cx.notify();
    }

    fn open_local_album(&mut self, id: String, cx: &mut Context<Self>) {
        self.push_history();
        self.current_entry = Some(NavEntry::LocalAlbum(id.clone()));
        let view = cx.new(|cx| {
            LocalAlbumDetailView::new(
                self.library_db.clone(),
                self.player.clone(),
                self.session.clone(),
                id,
                cx,
            )
        });
        self.content = Some(Content::LocalAlbumDetail(view));
        cx.notify();
    }

    fn open_artist(&mut self, id: String, cx: &mut Context<Self>) {
        self.push_history();
        self.current_entry = Some(NavEntry::Artist(id.clone()));
        let view =
            cx.new(|cx| ArtistDetailView::new(self.session.clone(), self.player.clone(), id, cx));
        cx.subscribe(&view, |this: &mut Self, _, event, cx| {
            let ArtistDetailEvent::OpenAlbum(id) = event;
            this.open_album(id.clone(), cx);
        })
        .detach();
        self.content = Some(Content::ArtistDetail(view));
        cx.notify();
    }

    fn open_playlist(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.push_history();
        self.current_entry = Some(NavEntry::Playlist(id.clone()));
        self.section = None;
        self.active_playlist = Some(id.clone());
        let view = cx.new(|cx| {
            PlaylistDetailView::new(
                self.session.clone(),
                self.player.clone(),
                self.playlists.clone(),
                id,
                window,
                cx,
            )
        });
        cx.subscribe(&view, |this: &mut Self, _, event, cx| {
            let PlaylistDetailEvent::Deleted = event;
            this.navigate(NavSection::Albums, None, cx);
        })
        .detach();
        self.content = Some(Content::Playlist(view));
        cx.notify();
    }

    fn open_new_playlist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_pl_name
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.new_pl_desc
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.new_playlist_open = true;
        self.new_pl_name.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn submit_new_playlist(&mut self, cx: &mut Context<Self>) {
        let name = self.new_pl_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let desc = self.new_pl_desc.read(cx).value().trim().to_string();
        let description = (!desc.is_empty()).then_some(desc);
        self.playlists
            .update(cx, |p, cx| p.create(name, description, Vec::new(), cx));
        self.new_playlist_open = false;
        cx.notify();
    }

    fn cancel_new_playlist(&mut self, cx: &mut Context<Self>) {
        self.new_playlist_open = false;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Vi-mode handling
    // ------------------------------------------------------------------

    fn sync_vi_mode(&mut self, vi_mode: bool, cx: &mut Context<Self>) {
        let was = self.vi_enabled;
        self.vi_enabled = vi_mode;
        if !self.vi_enabled {
            self.mode = KeyboardMode::Normal;
            if was {
                self.vi_clear_cursors(cx);
            }
        }
    }

    fn sidebar_select_next(&mut self) {
        self.vi_selected = (self.vi_selected + 1) % NAV_ORDER.len();
    }

    fn sidebar_select_prev(&mut self) {
        self.vi_selected = if self.vi_selected == 0 {
            NAV_ORDER.len() - 1
        } else {
            self.vi_selected - 1
        };
    }

    fn content_vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        match &self.content {
            Some(Content::Albums(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::Artists(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::AlbumDetail(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::Favorites(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::LocalAlbumDetail(v)) => {
                v.update(cx, |v, cx| v.vi_move(delta, window, cx))
            }
            Some(Content::Playlist(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::Recent(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            Some(Content::LocalMusic(v)) => v.update(cx, |v, cx| v.vi_move(delta, window, cx)),
            _ => {}
        }
    }

    /// Enter: open/play the focused item in the content region.
    fn content_vi_activate(&mut self, cx: &mut Context<Self>) {
        match &self.content {
            Some(Content::Albums(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::Artists(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::AlbumDetail(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::Favorites(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::LocalAlbumDetail(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::Playlist(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::Recent(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            Some(Content::LocalMusic(v)) => v.update(cx, |v, cx| v.vi_activate(cx)),
            _ => {}
        }
    }

    /// Cycle the album filter tab (`[`/`]`). No-op outside the album grid.
    fn content_vi_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(Content::Albums(v)) = &self.content {
            v.update(cx, |v, cx| v.vi_tab(delta, cx));
        }
    }

    /// Forget the per-view vi cursor when the mode is turned off, so no
    /// stale highlight survives a settings toggle.
    fn vi_clear_cursors(&mut self, cx: &mut Context<Self>) {
        match &self.content {
            Some(Content::Albums(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::Artists(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::AlbumDetail(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::Favorites(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::LocalAlbumDetail(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::Playlist(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::Recent(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            Some(Content::LocalMusic(v)) => v.update(cx, |v, cx| v.vi_clear(cx)),
            _ => {}
        }
    }

    fn sidebar_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vi_selected < NAV_ORDER.len() {
            let section = NAV_ORDER[self.vi_selected];
            self.navigate_push(section, window, cx);
        }
    }

    fn handle_vi_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
        _is_text_input: bool,
    ) {
        match self.mode {
            KeyboardMode::Normal => self.handle_vi_normal(event, window, cx),
            KeyboardMode::Insert => self.handle_vi_insert(event, window, cx),
            KeyboardMode::Command => self.handle_vi_command(event, window, cx),
        }
    }

    fn handle_vi_normal(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        // Shift+/ → "?" help toggle (some layouts report "/" with shift)
        if !ctrl && key == "/" && event.keystroke.modifiers.shift {
            self.show_vi_help = !self.show_vi_help;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match (ctrl, key) {
            // Region switching (vim-window style)
            (true, "h") => {
                self.focus_region = FocusRegion::Sidebar;
                cx.notify();
                cx.stop_propagation();
            }
            (true, "j") => {
                self.focus_region = FocusRegion::PlayerBar;
                cx.notify();
                cx.stop_propagation();
            }
            (true, "k") | (true, "l") => {
                self.focus_region = FocusRegion::Content;
                cx.notify();
                cx.stop_propagation();
            }
            // j/k dispatch by current region
            (false, "j") => match self.focus_region {
                FocusRegion::Sidebar => {
                    self.sidebar_select_next();
                    cx.notify();
                    cx.stop_propagation();
                }
                FocusRegion::Content => {
                    self.content_vi_move(1, window, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
                FocusRegion::PlayerBar => {
                    let v = (self.player.read(cx).volume - 0.05).max(0.0);
                    self.player.update(cx, |p, cx| p.set_volume(v, cx));
                    cx.stop_propagation();
                }
            },
            (false, "k") => match self.focus_region {
                FocusRegion::Sidebar => {
                    self.sidebar_select_prev();
                    cx.notify();
                    cx.stop_propagation();
                }
                FocusRegion::Content => {
                    self.content_vi_move(-1, window, cx);
                    cx.notify();
                    cx.stop_propagation();
                }
                FocusRegion::PlayerBar => {
                    let v = (self.player.read(cx).volume + 0.05).min(1.0);
                    self.player.update(cx, |p, cx| p.set_volume(v, cx));
                    cx.stop_propagation();
                }
            },
            // h/l nav back/forward (any region)
            (false, "h") => {
                self.nav_back(window, cx);
                cx.stop_propagation();
            }
            (false, "l") => {
                self.nav_forward(window, cx);
                cx.stop_propagation();
            }
            // Enter dispatches by region
            (false, "enter") => match self.focus_region {
                FocusRegion::Sidebar => {
                    self.sidebar_activate(window, cx);
                    cx.stop_propagation();
                }
                FocusRegion::Content => {
                    self.content_vi_activate(cx);
                    cx.stop_propagation();
                }
                FocusRegion::PlayerBar => {
                    self.player.update(cx, |p, cx| p.toggle_play(cx));
                    cx.stop_propagation();
                }
            },
            // Media keys
            (false, "space") => {
                self.player.update(cx, |p, cx| p.toggle_play(cx));
                cx.stop_propagation();
            }
            (false, "left") => {
                self.player.update(cx, |p, cx| p.previous(cx));
                cx.stop_propagation();
            }
            (false, "right") => {
                self.player.update(cx, |p, cx| p.next(cx));
                cx.stop_propagation();
            }
            // Album filter tabs (new/random/...) in the grid.
            (false, "[") => {
                self.content_vi_tab(-1, cx);
                cx.stop_propagation();
            }
            (false, "]") => {
                self.content_vi_tab(1, cx);
                cx.stop_propagation();
            }
            (false, "i") => {
                self.mode = KeyboardMode::Insert;
                cx.notify();
                cx.stop_propagation();
            }
            (false, ":") => {
                self.mode = KeyboardMode::Command;
                self.cmd_input
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.cmd_input.update(cx, |s, cx| s.focus(window, cx));
                cx.notify();
                cx.stop_propagation();
            }
            (false, "/") => {
                self.search_bar
                    .update(cx, |sb, cx| sb.open_palette(window, cx));
                cx.stop_propagation();
            }
            (false, "?") => {
                self.show_vi_help = !self.show_vi_help;
                cx.notify();
                cx.stop_propagation();
            }
            (false, "escape") => {
                if self.show_vi_help {
                    self.show_vi_help = false;
                    cx.notify();
                    cx.stop_propagation();
                } else if self.new_playlist_open {
                    self.cancel_new_playlist(cx);
                    cx.stop_propagation();
                } else if self.show_fullscreen {
                    self.fullscreen.update(cx, |f, cx| f.begin_close(cx));
                    cx.stop_propagation();
                } else if self.search_bar.read(cx).is_palette()
                    || self.search_bar.read(cx).is_open()
                {
                    self.search_bar.update(cx, |sb, cx| sb.dismiss(window, cx));
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
    }

    fn handle_vi_insert(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.mode = KeyboardMode::Normal;
                cx.notify();
                cx.stop_propagation();
            }
            // Media keys still work in insert mode.
            "space" => {
                self.player.update(cx, |p, cx| p.toggle_play(cx));
                cx.stop_propagation();
            }
            "left" => {
                self.player.update(cx, |p, cx| p.previous(cx));
                cx.stop_propagation();
            }
            "right" => {
                self.player.update(cx, |p, cx| p.next(cx));
                cx.stop_propagation();
            }
            "up" => {
                self.player.update(cx, |p, cx| {
                    p.set_volume((p.volume + 0.05).min(1.0), cx);
                });
                cx.stop_propagation();
            }
            "down" => {
                self.player.update(cx, |p, cx| {
                    p.set_volume((p.volume - 0.05).max(0.0), cx);
                });
                cx.stop_propagation();
            }
            _ => {} // pass through to text input
        }
    }

    fn handle_vi_command(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key.as_str() == "escape" {
            self.mode = KeyboardMode::Normal;
            cx.notify();
            cx.stop_propagation();
        }
        // InputState handles text entry
    }

    fn execute_command(&mut self, cx: &mut Context<Self>) {
        let cmd = self.cmd_input.read(cx).value().to_string();
        let trimmed = cmd.trim();
        self.mode = KeyboardMode::Normal;
        cx.notify();
        if trimmed.is_empty() {
            return;
        }
        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap_or_default().to_lowercase();
        let args: Vec<&str> = parts.collect();
        match cmd.as_str() {
            "q" | "quit" => cx.quit(),
            "help" => {
                self.show_vi_help = true;
            }
            "newpl" | "newplaylist" => {
                if !args.is_empty() {
                    let name = args.join(" ");
                    self.playlists
                        .update(cx, |p, cx| p.create(name.clone(), None, Vec::new(), cx));
                    self.set_vi_status(format!("Created playlist \"{name}\""), cx);
                } else {
                    self.set_vi_status("Usage: :newpl <name>".into(), cx);
                }
            }
            "pl" | "playlist" => match args.first().copied() {
                Some("add") if args.len() >= 2 => {
                    let pl_name = args[1..].join(" ");
                    let song = self.player.read(cx).current_song().map(|s| s.id.clone());
                    let found = self
                        .playlists
                        .read(cx)
                        .playlists
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(&pl_name))
                        .cloned();
                    match (song, found) {
                        (Some(song_id), Some(pl)) => {
                            self.playlists
                                .update(cx, |p, cx| p.add_song(pl.id.clone(), song_id, cx));
                            self.set_vi_status(
                                format!("Added current song to \"{}\"", pl.name),
                                cx,
                            );
                        }
                        (None, _) => self.set_vi_status("No current song".into(), cx),
                        (_, None) => {
                            self.set_vi_status(format!("Playlist \"{pl_name}\" not found"), cx)
                        }
                    }
                }
                Some("list") => {
                    let names: Vec<String> = self
                        .playlists
                        .read(cx)
                        .playlists
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();
                    self.set_vi_status(
                        if names.is_empty() {
                            "No playlists".into()
                        } else {
                            format!("Playlists: {}", names.join(", "))
                        },
                        cx,
                    );
                }
                _ => self.set_vi_status("Usage: :pl add <name> | :pl list".into(), cx),
            },
            _ => {
                self.set_vi_status(format!("Unknown command: {cmd}"), cx);
            }
        }
    }

    fn set_vi_status(&mut self, msg: String, cx: &mut Context<Self>) {
        self.vi_status = Some(msg);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(4))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.vi_status = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn render_mode_indicator(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = match self.mode {
            KeyboardMode::Normal => "NORMAL",
            KeyboardMode::Insert => "INSERT",
            KeyboardMode::Command => "COMMAND",
        };
        div()
            .text_xs()
            .font_bold()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(cx.theme().muted)
            .text_color(cx.theme().primary)
            .child(label)
    }

    fn render_command_bar(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .w_full()
            .h(px(40.))
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .flex()
            .items_center()
            .px_3()
            .gap_2()
            .capture_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "enter" => {
                        this.execute_command(cx);
                        window.focus(&this.focus_handle);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.mode = KeyboardMode::Normal;
                        this.cmd_input
                            .update(cx, |s, cx| s.set_value("", window, cx));
                        window.focus(&this.focus_handle);
                        cx.notify();
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .text_sm()
                    .font_bold()
                    .text_color(cx.theme().primary)
                    .child(":"),
            )
            .child(div().flex_1().child(Input::new(&self.cmd_input)))
            .into_any_element()
    }

    fn render_vi_help(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let key = |k: SharedString, desc: SharedString| {
            h_flex()
                .gap_4()
                .child(
                    div()
                        .w(px(120.))
                        .text_sm()
                        .font_bold()
                        .text_color(cx.theme().primary)
                        .child(k.clone()),
                )
                .child(div().text_sm().child(desc.clone()))
                .into_any_element()
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .bg(gpui::hsla(0., 0., 0., 0.6))
            .child(
                v_flex()
                    // Clamped so the dialog cannot overrun a narrow window.
                    .w(px(460.))
                    .max_w_full()
                    .gap_3()
                    .p_5()
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(div().text_lg().font_semibold().child("Vi-mode help"))
                    .child(key(
                        "j / k".into(),
                        "Scroll / navigate current region".into(),
                    ))
                    .child(key(
                        "Ctrl+h/j/k/l".into(),
                        "Focus sidebar / player / content".into(),
                    ))
                    .child(key("Enter".into(), "Open/play focused item".into()))
                    .child(key("h / l".into(), "Back / forward in history".into()))
                    .child(key("Space".into(), "Toggle play/pause".into()))
                    .child(key("← / →".into(), "Previous / next track".into()))
                    .child(key("[ / ]".into(), "Album tabs: new/random/…".into()))
                    .child(key("i".into(), "Insert mode (pass keys to input)".into()))
                    .child(key(":".into(), "Command mode (:q / :help)".into()))
                    .child(key(":newpl <name>".into(), "Create playlist".into()))
                    .child(key(
                        ":pl add <name>".into(),
                        "Add current song to playlist".into(),
                    ))
                    .child(key(":pl list".into(), "List playlists".into()))
                    .child(key("/".into(), "Search (Ctrl+K)".into()))
                    .child(key("Ctrl+B".into(), "Collapse / expand sidebar".into()))
                    .child(key("? / Esc".into(), "Toggle help / Dismiss".into()))
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Press Esc or ? to close"),
                    ),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // First-time setup wizard
    // ------------------------------------------------------------------

    fn handle_setup_add_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let val = self.setup_dir_input.read(cx).value().trim().to_string();
        if !val.is_empty() {
            self.setup_local_dirs.push(PathBuf::from(val));
            self.setup_dir_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            self.setup_error = None;
        }
    }

    fn handle_setup_remove_dir(&mut self, idx: usize) {
        self.setup_local_dirs.remove(idx);
    }

    fn handle_setup_submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.setup_enable_navidrome && !self.setup_enable_local {
            self.setup_error = Some("Select at least Navidrome or Local Music".into());
            cx.notify();
            return;
        }

        if self.setup_enable_navidrome {
            let url = self.setup_server_url.read(cx).value().trim().to_string();
            let username = self.setup_username.read(cx).value().trim().to_string();
            let password = self.setup_password.read(cx).value().to_string();
            if url.is_empty() {
                self.setup_error = Some("Enter a Navidrome server URL".into());
                cx.notify();
                return;
            }
            let mut settings = self.session.read(cx).settings.clone();
            settings.local_music_dirs = self.setup_local_dirs.clone();
            self.session.update(cx, |s, cx| {
                s.settings = settings;
                s.persist_settings(); // write NOW so first_run becomes false
                s.connect(url, username, password, true, cx);
            });
        } else {
            let dirs = self.setup_local_dirs.clone();
            self.session.update(cx, |s, cx| {
                let mut settings = s.settings.clone();
                settings.local_music_dirs = dirs;
                s.settings = settings;
                s.persist_settings();
                cx.notify();
            });
        }
    }

    fn render_setup(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let client_titlebar = self.session.read(cx).settings.client_titlebar;
        let minimal_titlebar = self.session.read(cx).settings.minimal_titlebar;
        let field = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(Input::new(input))
        };

        let nav_btn_label = if self.setup_enable_navidrome {
            "✓ Enable Navidrome streaming"
        } else {
            "  Enable Navidrome streaming"
        };
        let local_btn_label = if self.setup_enable_local {
            "✓ Enable Local Music"
        } else {
            "  Enable Local Music"
        };

        let dir_rows: Vec<gpui::AnyElement> = self
            .setup_local_dirs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let idx = i;
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .flex_1()
                            .child(p.to_string_lossy().to_string()),
                    )
                    .child(
                        Button::new(SharedString::from(format!("rm-dir-{i}")))
                            .ghost()
                            .label("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.handle_setup_remove_dir(idx);
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let content = v_flex()
            .gap_5()
            .max_w(px(560.))
            .w_full()
            .child(
                v_flex().gap_1().child(
                    div()
                        .text_2xl()
                        .font_semibold()
                        .child("Welcome to Scirè"),
                ).child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Choose how you want to use Scirè. You can change these later in Settings."),
                ),
            )
            // Navidrome section
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new("toggle-nav")
                                    .ghost()
                                    .label(nav_btn_label)
                                    .when(self.setup_enable_navidrome, |b| b.primary())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.setup_enable_navidrome = !this.setup_enable_navidrome;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.setup_enable_navidrome, |this| {
                        this.child(field("Server URL", &self.setup_server_url))
                            .child(field("Username", &self.setup_username))
                            .child(field("Password", &self.setup_password))
                    }),
            )
            // Local music section
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("toggle-local")
                            .ghost()
                            .label(local_btn_label)
                            .when(self.setup_enable_local, |b| b.primary())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.setup_enable_local = !this.setup_enable_local;
                                cx.notify();
                            })),
                    )
                    .when(self.setup_enable_local, |this| {
                        this
                            .child(Input::new(&self.setup_dir_input))
                            .child(
                                Button::new("setup-add-dir")
                                    .ghost()
                                    .label("Add directory")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.handle_setup_add_dir(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .children(dir_rows)
                    }),
            )
            // Error
            .when_some(self.setup_error.clone(), |this, err| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(gpui::red())
                        .child(err),
                )
            })
            // Submit
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("setup-submit")
                            .primary()
                            .label("Get Started")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.handle_setup_submit(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("You can change these settings anytime in Settings → Account / Local Music."),
            );

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when(client_titlebar, |this| {
                this.child(crate::ui::title_bar(minimal_titlebar, cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_new_playlist_modal(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let field = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(Input::new(input))
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .bg(gpui::hsla(0., 0., 0., 0.6))
            .child(
                v_flex()
                    // Clamped so the dialog cannot overrun a narrow window.
                    .w(px(440.))
                    .max_w_full()
                    .gap_4()
                    .p_5()
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(div().text_lg().font_semibold().child("New playlist"))
                    .child(field("Name", &self.new_pl_name))
                    .child(field("Description", &self.new_pl_desc))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("np-cancel").ghost().label("Cancel").on_click(
                                cx.listener(|this, _, _, cx| this.cancel_new_playlist(cx)),
                            ))
                            .child(Button::new("np-create").primary().label("Create").on_click(
                                cx.listener(|this, _, _, cx| this.submit_new_playlist(cx)),
                            )),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for RootView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_orientation(window, cx);
        let connected = self.session.read(cx).status == ConnectionStatus::Connected;
        let palette_open = self.search_bar.read(cx).is_palette();
        let modal_open = self.new_playlist_open
            || self.show_fullscreen
            || palette_open
            || self.mode == KeyboardMode::Command
            || (self.vi_enabled && self.show_vi_help);
        // Grab focus so key handlers fire immediately when the app is visible
        // and no text input has claimed it.
        let grab_focus = self.mode != KeyboardMode::Insert;
        if connected && !modal_open && grab_focus && !self.focus_handle.contains_focused(window, cx)
        {
            window.focus(&self.focus_handle);
        }

        // First launch: show setup until the user has configured either a
        // server or local dirs.  Uses in-memory content (not just file existence)
        // so a failed persist doesn't force the wizard on every restart.
        let s = self.session.read(cx);
        let has_server = s.settings.server.is_some();
        let has_local = !s.settings.local_music_dirs.is_empty();
        let configured = has_server || has_local;
        if !connected && !configured {
            return self.render_setup(window, cx);
        }
        // Not connected but configured → show main layout (sidebar + empty
        // content).  The user can reach Settings via sidebar to fix credentials.

        let content: gpui::AnyElement = match &self.content {
            Some(Content::Albums(v)) => v.clone().into_any_element(),
            Some(Content::Artists(v)) => v.clone().into_any_element(),
            Some(Content::ArtistDetail(v)) => v.clone().into_any_element(),
            Some(Content::AlbumDetail(v)) => v.clone().into_any_element(),
            Some(Content::LocalAlbumDetail(v)) => v.clone().into_any_element(),
            Some(Content::Favorites(v)) => v.clone().into_any_element(),
            Some(Content::Playlist(v)) => v.clone().into_any_element(),
            Some(Content::Radio(v)) => v.clone().into_any_element(),
            Some(Content::Settings(v)) => v.clone().into_any_element(),
            Some(Content::Recent(v)) => v.clone().into_any_element(),
            Some(Content::LocalMusic(v)) => v.clone().into_any_element(),
            None => div().into_any_element(),
        };

        // Playlists whose owner differs from the logged-in user are "shared".
        let current_user = self
            .session
            .read(cx)
            .settings
            .server
            .as_ref()
            .map(|s| s.username.clone());
        let sidebar_model = SidebarModel {
            active: self.section,
            active_playlist: self.active_playlist.clone(),
            playlists: self
                .playlists
                .read(cx)
                .playlists
                .iter()
                .map(|p| {
                    let shared = match (p.owner.as_ref(), current_user.as_ref()) {
                        (Some(owner), Some(me)) => owner != me,
                        _ => false,
                    };
                    (p.id.clone(), p.name.clone(), shared)
                })
                .collect(),
            libraries: self
                .session
                .read(cx)
                .music_folders
                .iter()
                .map(|f| (f.id(), f.name.clone().unwrap_or_else(|| f.id())))
                .collect(),
            selected_libraries: self.session.read(cx).library_ids.clone(),
            libraries_collapsed: self.libraries_collapsed,
            playlists_collapsed: self.playlists_collapsed,
            refreshing: self.refreshing,
            refresh_stage: self.refresh_stage,
            collapsed: self.sidebar_collapsed,
            vi_selected_section: if self.vi_enabled
                && self.mode == KeyboardMode::Normal
                && self.focus_region == FocusRegion::Sidebar
            {
                Some(NAV_ORDER[self.vi_selected])
            } else {
                None
            },
        };

        let this = cx.entity();
        let fullscreen = self.fullscreen.clone();
        let show_fullscreen = self.show_fullscreen;
        let client_titlebar = self.session.read(cx).settings.client_titlebar;
        let minimal_titlebar = self.session.read(cx).settings.minimal_titlebar;
        let show_nav_buttons = self.session.read(cx).settings.show_nav_buttons;

        v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Keyboard navigation: track focus so on_key_down fires.
            .track_focus(&self.focus_handle)
            // Mouse back/forward buttons mirror the [ and ] history keys.
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.nav_back(window, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.nav_forward(window, cx)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Fold the sidebar. Ahead of the mode split so it works the
                // same in vi mode, but not while a text input has focus —
                // Ctrl+B is a cursor key there.
                if event.keystroke.key == "b"
                    && (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
                    && window.focused(cx).is_none_or(|f| f == this.focus_handle)
                {
                    this.toggle_sidebar(cx);
                    cx.stop_propagation();
                    return;
                }
                if this.vi_enabled {
                    this.handle_vi_key(event, window, cx, false);
                } else {
                    // Ctrl+K (Cmd+K) opens the command palette (vi-mode excluded).
                    if event.keystroke.key == "k"
                        && (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
                    {
                        this.search_bar
                            .update(cx, |sb, cx| sb.open_palette(window, cx));
                        cx.stop_propagation();
                        return;
                    }
                    let focused = window.focused(cx);
                    let is_text_input = focused.is_some_and(|focus| focus != this.focus_handle);
                    match event.keystroke.key.as_str() {
                        "space" if !is_text_input => {
                            this.player.update(cx, |p, cx| p.toggle_play(cx));
                            cx.stop_propagation();
                        }
                        "left" => {
                            this.player.update(cx, |p, cx| p.previous(cx));
                            cx.stop_propagation();
                        }
                        "right" => {
                            this.player.update(cx, |p, cx| p.next(cx));
                            cx.stop_propagation();
                        }
                        "up" => {
                            this.player.update(cx, |p, cx| {
                                p.set_volume((p.volume + 0.05).min(1.0), cx);
                            });
                            cx.stop_propagation();
                        }
                        "down" => {
                            this.player.update(cx, |p, cx| {
                                p.set_volume((p.volume - 0.05).max(0.0), cx);
                            });
                            cx.stop_propagation();
                        }
                        "escape" => {
                            if this.new_playlist_open {
                                this.cancel_new_playlist(cx);
                                cx.stop_propagation();
                            } else if this.show_fullscreen {
                                this.fullscreen.update(cx, |f, cx| f.begin_close(cx));
                                cx.stop_propagation();
                            } else if this.search_bar.read(cx).is_palette()
                                || this.search_bar.read(cx).is_open()
                            {
                                this.search_bar.update(cx, |sb, cx| sb.dismiss(window, cx));
                                cx.stop_propagation();
                            }
                        }
                        "[" => {
                            this.nav_back(window, cx);
                            cx.stop_propagation();
                        }
                        "]" => {
                            this.nav_forward(window, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }
            }))
            .when(client_titlebar, |this| {
                this.child(crate::ui::title_bar(minimal_titlebar, cx))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(render_sidebar(
                        sidebar_model,
                        move |action, window, cx| {
                            this.update(cx, |root, cx| match action {
                                SidebarAction::Select(section) => {
                                    root.navigate_push(section, window, cx)
                                }
                                SidebarAction::OpenPlaylist(id) => {
                                    root.open_playlist(id, window, cx)
                                }
                                SidebarAction::NewPlaylist => {
                                    root.open_new_playlist(window, cx);
                                }
                                SidebarAction::ToggleLibrary(id) => {
                                    root.session.update(cx, |s, cx| s.toggle_library(id, cx));
                                }
                                SidebarAction::AllLibraries => {
                                    root.session.update(cx, |s, cx| s.select_all_libraries(cx));
                                }
                                SidebarAction::ToggleLibrarySection => {
                                    root.libraries_collapsed = !root.libraries_collapsed;
                                    let collapsed = root.libraries_collapsed;
                                    root.session.update(cx, |session, _| {
                                        session.settings.sidebar_libraries_collapsed = collapsed;
                                        session.persist_settings();
                                    });
                                    cx.notify();
                                }
                                SidebarAction::RefreshLibrary => {
                                    root.refresh_library(window, cx);
                                }
                                SidebarAction::ToggleSidebar => {
                                    root.toggle_sidebar(cx);
                                }
                                SidebarAction::TogglePlaylistSection => {
                                    root.playlists_collapsed = !root.playlists_collapsed;
                                    let collapsed = root.playlists_collapsed;
                                    root.session.update(cx, |session, _| {
                                        session.settings.sidebar_playlists_collapsed = collapsed;
                                        session.persist_settings();
                                    });
                                    cx.notify();
                                }
                            });
                        },
                        cx,
                    ))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                // The nav row sits above the page rather than
                                // floating over it: every content view draws
                                // its own header in the top-left corner, so an
                                // overlay would land on one.
                                v_flex()
                                    .size_full()
                                    .when(show_nav_buttons, |this| {
                                        this.child(self.render_nav_buttons(cx))
                                    })
                                    .child(div().flex_1().min_h_0().child(content)),
                            )
                            // Mode indicator + transient command feedback
                            // (replaces the old inline search bar).
                            .when(!palette_open && self.vi_enabled, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top(px(14.))
                                        .right(px(16.))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(self.render_mode_indicator(cx))
                                        .when_some(self.vi_status.clone(), |this, msg| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_md()
                                                    .bg(cx.theme().muted)
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(msg),
                                            )
                                        }),
                                )
                            }),
                    )
                    .when(self.show_queue, |this| {
                        this.child(
                            div().flex().child(self.queue_panel.clone()).with_animation(
                                ElementId::Name("queue-slide-in".into()),
                                Animation::new(std::time::Duration::from_millis(220))
                                    .with_easing(ease_out_quint()),
                                |this, t| this.opacity(t).ml(px(20. * (1. - t))),
                            ),
                        )
                    }),
            )
            .child(self.player_bar.clone())
            // Fullscreen overlay — rendered last so it sits on top.
            .when(show_fullscreen, |this| this.child(fullscreen))
            // New-playlist dialog on top of everything.
            .when(self.new_playlist_open, |this| {
                this.child(self.render_new_playlist_modal(cx))
            })
            // Command-mode input bar at bottom.
            .when(self.mode == KeyboardMode::Command, |this| {
                this.child(self.render_command_bar(window, cx))
            })
            // Vi-mode help overlay.
            .when(self.show_vi_help, |this| {
                this.child(self.render_vi_help(cx))
            })
            // Centered command palette (Ctrl/Cmd+K): dimmed full-window backdrop
            // with the search box near the top. Backdrop click dismisses; the
            // box itself occludes so inner clicks don't fall through.
            .when(palette_open, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .bg(gpui::hsla(0., 0., 0., 0.55))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.search_bar.update(cx, |sb, cx| sb.dismiss(window, cx));
                            }),
                        )
                        .child(div().mt(px(96.)).child(self.search_bar.clone())),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::RefreshStage;

    #[test]
    fn only_the_import_stage_has_a_fraction() {
        // The others count upward against no known total, so a bar drawn for
        // them could only be guessing.
        assert_eq!(RefreshStage::Idle.fraction(), None);
        assert_eq!(RefreshStage::Comparing.fraction(), None);
        assert_eq!(RefreshStage::LocalScan(120).fraction(), None);
        assert_eq!(RefreshStage::Import(1, 4).fraction(), Some(0.25));
    }

    #[test]
    fn import_fraction_survives_a_zero_or_overshooting_total() {
        // `total` is 0 for the whole listing pass, and `done` can reach it
        // exactly — neither may produce NaN or a bar wider than its track.
        assert_eq!(RefreshStage::Import(0, 0).fraction(), None);
        assert_eq!(RefreshStage::Import(7, 0).fraction(), None);
        assert_eq!(RefreshStage::Import(9, 4).fraction(), Some(1.0));
    }

    #[test]
    fn labels_drop_a_zero_count_instead_of_showing_it() {
        assert_eq!(RefreshStage::LocalScan(0).label(), "Scanning local files…");
        assert_eq!(
            RefreshStage::LocalScan(42).label(),
            "Scanning local files… 42"
        );
        // A refresh that finds nothing new never leaves this wording.
        assert_eq!(RefreshStage::Comparing.label(), "Checking for new music…");
        assert_eq!(
            RefreshStage::Import(0, 0).label(),
            "Checking for new music…"
        );
        assert_eq!(RefreshStage::Import(3, 10).label(), "Fetching 3/10 albums");
    }
}
