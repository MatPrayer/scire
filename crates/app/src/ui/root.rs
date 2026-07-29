//! Root view: main app layout
//! (sidebar | content | optional queue panel / player bar).

use gpui::{
    Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton,
    NavigationDirection, Render, SharedString, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, StyledExt as _, TitleBar, h_flex, v_flex};

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{DefaultPage, ThemePref};
use crate::services::{artwork, library_db::LibraryDb, local_library::LocalScanner, navidrome_sync, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::radio::RadioState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::album_detail::{AlbumDetailEvent, AlbumDetailView};
use crate::ui::albums::{AlbumsEvent, AlbumsView};
use crate::ui::artists::{ArtistDetailEvent, ArtistDetailView, ArtistsEvent, ArtistsView};
use crate::ui::favorites::{FavoritesEvent, FavoritesView};
use crate::ui::fullscreen_player::{FullscreenEvent, FullscreenPlayer};
use crate::ui::local_music::LocalMusicView;
use crate::ui::player_bar::{PlayerBar, PlayerBarEvent};
use crate::ui::playlist_detail::{PlaylistDetailEvent, PlaylistDetailView};
use crate::ui::queue_panel::QueuePanel;
use crate::ui::radio::RadioView;
use crate::ui::recent::RecentView;
use crate::ui::search_bar::{SearchBar, SearchBarEvent};
use crate::ui::settings::SettingsView;
use crate::ui::sidebar::{NavSection, SidebarAction, SidebarModel, render_sidebar};

#[derive(Clone)]
enum NavEntry {
    Section(NavSection),
    Album(String),
    Artist(String),
    Playlist(String),
}

enum Content {
    Albums(Entity<AlbumsView>),
    Artists(Entity<ArtistsView>),
    ArtistDetail(Entity<ArtistDetailView>),
    AlbumDetail(Entity<AlbumDetailView>),
    Favorites(Entity<FavoritesView>),
    Playlist(Entity<PlaylistDetailView>),
    Radio(Entity<RadioView>),
    Settings(Entity<SettingsView>),
    Recent(Entity<RecentView>),
    LocalMusic(Entity<LocalMusicView>),
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
    /// Shared music library database.
    library_db: Arc<LibraryDb>,
    /// Whether initial local/navidrome sync has been triggered.
    scan_started: bool,
    /// New-playlist dialog state.
    new_playlist_open: bool,
    new_pl_name: Entity<InputState>,
    new_pl_desc: Entity<InputState>,
    /// Art key (album-scoped) whose colour the Adaptive theme accent was last
    /// derived from, to avoid re-extracting on every player tick.
    adaptive_cover: Option<String>,
    focus_handle: FocusHandle,

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
        let player_bar =
            cx.new(|cx| PlayerBar::new(player.clone(), session.clone(), window, cx));
        let queue_panel = cx.new(|cx| QueuePanel::new(player.clone(), cx));
        let radio = crate::state::radio::init(session.clone(), cx);
        let fullscreen = cx.new(|cx| FullscreenPlayer::new(player.clone(), session.clone(), cx));
        let search_bar = cx.new(|cx| SearchBar::new(session.clone(), player.clone(), window, cx));

        cx.subscribe(&search_bar, |this: &mut Self, _, event, cx| match event {
            SearchBarEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
            SearchBarEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
        })
        .detach();

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
        let setup_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://music.example.com"));
        let setup_user = cx.new(|cx| InputState::new(window, cx).placeholder("username"));
        let setup_pass = cx.new(|cx| InputState::new(window, cx).placeholder("password"));
        let setup_dir = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/music"));

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
                this.content = None;
                if connected {
                    this.last_libraries = libraries;
                }
            } else if connected && libraries != this.last_libraries {
                // Library selection changed: rebuild the current catalog view.
                this.last_libraries = libraries;
                if let Some(section) = this.section {
                    this.navigate(section, None, cx);
                }
            }

            let has_local = !session.read(cx).settings.local_music_dirs.is_empty();
            let has_server = session.read(cx).settings.server.is_some();
            let lm = has_local && !has_server;
            // Kick off local scanner (and Navidrome sync) once.
            // When server + local dirs are both configured, local scan
            // starts immediately without waiting for Navidrome connect.
            let should_scan = connected || has_local;
            if should_scan && !this.scan_started {
                this.scan_started = true;
                let dirs: Vec<_> = session
                    .read(cx)
                    .settings
                    .local_music_dirs
                    .clone();
                if !dirs.is_empty() {
                    let lib_db = this.library_db.clone();
                    cx.spawn(async move |this, cx| {
                        let scanner = Arc::new(LocalScanner::new(lib_db));
                        let s = scanner.clone();
                        let d = dirs.clone();
                        let _ = runtime::spawn_io(async move {
                            s.scan(&d)
                        })
                        .await;
                        let _ = this.update(cx, |_, _| {});
                        // Periodic background scan every 5 minutes.
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                            let s = scanner.clone();
                            let d = dirs.clone();
                            let _ = runtime::spawn_io(async move {
                                s.scan(&d)
                            }).await;
                            let _ = this.update(cx, |_, _| {});
                        }
                    })
                    .detach();
                }
                if session.read(cx).client.is_some() {
                    let db = this.library_db.clone();
                    let client = session.read(cx).client.clone();
                    cx.spawn(async move |this, cx| {
                        if let Some(client) = client {
                            let _ = runtime::spawn_io(async move {
                                navidrome_sync::sync_navidrome(db, &client, None).await
                            })
                            .await;
                            let _ = this.update(cx, |_, _| {});
                        }
                    })
                    .detach();
                }
            }

            // Show content when connected OR when we have a server/local-music
            // configured (so the sidebar isn't empty during connecting).
            let configured = has_server || has_local;
            let navigable = connected || lm || configured;
            if navigable && this.content.is_none() {
                let start = if lm {
                    NavSection::LocalMusic
                } else {
                    match session.read(cx).settings.default_page {
                        DefaultPage::Albums => NavSection::Albums,
                        DefaultPage::Artists => NavSection::Artists,
                        DefaultPage::Favorites => NavSection::Favorites,
                        DefaultPage::Recent => NavSection::Recent,
                        DefaultPage::Radio => NavSection::Radio,
                    }
                };
                this.navigate(start, None, cx);
            }

            // Pick up theme switches (e.g. to/from Adaptive) from Settings.
            this.maybe_update_adaptive_accent(cx);
            cx.notify();
        })
        .detach();

        Self {
            session,
            player,
            playlists,
            radio,
            player_bar,
            queue_panel,
            fullscreen,
            search_bar,
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
            scan_started: false,
            last_libraries: Vec::new(),
            libraries_collapsed: false,
            playlists_collapsed: false,
            new_playlist_open: false,
            new_pl_name,
            new_pl_desc,
            adaptive_cover: None,
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

    /// When the Adaptive theme is active, derive the accent colour from the
    /// current track's cover art and recolour the interactive surfaces. Cheap
    /// no-op for other themes or when the cover hasn't changed.
    fn maybe_update_adaptive_accent(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).settings.theme != ThemePref::Adaptive {
            self.adaptive_cover = None;
            return;
        }
        // Album-scoped key: per-song cover ids would re-extract the accent (and
        // recolour the whole UI) on every track of the same album.
        let cover = self
            .player
            .read(cx)
            .current_song()
            .and_then(artwork::song_cover);
        let key = cover.as_ref().map(|(_, key)| key.clone());
        if key == self.adaptive_cover {
            return;
        }
        self.adaptive_cover = key;
        let Some((cover_id, key)) = cover else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        cx.spawn(async move |_, cx| {
            let accent = runtime::spawn_io(async move {
                let path = artwork::fetch_as(client, cover_id, key, 64).await?;
                let bytes = std::fs::read(&path)?;
                crate::ui::accent_from_cover_bytes(&bytes)
                    .ok_or_else(|| anyhow::anyhow!("cover has no usable accent hue"))
            })
            .await;
            if let Ok(accent) = accent {
                let _ = cx.update(|cx| crate::ui::apply_adaptive_accent(cx, accent));
            }
        })
        .detach();
    }

    fn navigate(
        &mut self,
        section: NavSection,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.current_entry = Some(NavEntry::Section(section));
        self.section = Some(section);
        self.active_playlist = None;
        self.content = Some(match section {
            NavSection::Albums => {
                let view = cx.new(|cx| {
                    AlbumsView::new(
                        self.session.clone(),
                        self.player.clone(),
                        self.playlists.clone(),
                        cx,
                    )
                });
                cx.subscribe(&view, |this: &mut Self, _, event, cx| match event {
                    AlbumsEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
                    AlbumsEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
                })
                .detach();
                Content::Albums(view)
            }
            NavSection::Artists => {
                let view = cx.new(|cx| ArtistsView::new(self.session.clone(), cx));
                cx.subscribe(&view, |this: &mut Self, _, event, cx| {
                    let ArtistsEvent::OpenArtist(id) = event;
                    this.open_artist(id.clone(), cx);
                })
                .detach();
                Content::Artists(view)
            }
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
            NavSection::Recent => Content::Recent(
                cx.new(|cx| RecentView::new(self.player.clone(), self.session.clone(), cx)),
            ),
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
                    LocalMusicView::new(self.library_db.clone(), self.player.clone(), cx)
                });
                Content::LocalMusic(view)
            }
            NavSection::Settings => {
                let Some(window) = window else {
                    return;
                };
                let view = cx.new(|cx| {
                    SettingsView::new(self.session.clone(), self.player.clone(), window, cx)
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

    fn restore_entry(&mut self, entry: NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.in_history_restore = true;
        self.current_entry = Some(entry.clone());
        match entry {
            NavEntry::Section(section) => self.navigate(section, Some(window), cx),
            NavEntry::Album(id) => self.open_album(id, cx),
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
    // First-time setup wizard
    // ------------------------------------------------------------------

    fn handle_setup_add_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let val = self.setup_dir_input.read(cx).value().trim().to_string();
        if !val.is_empty() {
            self.setup_local_dirs.push(PathBuf::from(val));
            self.setup_dir_input.update(cx, |s, cx| s.set_value("", window, cx));
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

        let nav_btn_label = if self.setup_enable_navidrome { "✓ Enable Navidrome streaming" } else { "  Enable Navidrome streaming" };
        let local_btn_label = if self.setup_enable_local { "✓ Enable Local Music" } else { "  Enable Local Music" };

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
                this.child(
                    TitleBar::new().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Scirè"),
                    ),
                )
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
                    .w(px(440.))
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
        let connected = self.session.read(cx).status == ConnectionStatus::Connected;
        // Grab focus so key handlers fire immediately when the app is visible
        // and no text input has claimed it.
        if connected && !self.focus_handle.contains_focused(window, cx) {
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
        };

        let this = cx.entity();
        let fullscreen = self.fullscreen.clone();
        let show_fullscreen = self.show_fullscreen;
        let client_titlebar = self.session.read(cx).settings.client_titlebar;
        let palette_open = self.search_bar.read(cx).is_palette();

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
                    "/" if !is_text_input => {
                        this.search_bar.update(cx, |sb, cx| sb.focus(window, cx));
                        cx.stop_propagation();
                    }
                    // Ctrl+K (Cmd+K on macOS) opens the centered command palette.
                    "k" if event.keystroke.modifiers.control
                        || event.keystroke.modifiers.platform =>
                    {
                        this.search_bar
                            .update(cx, |sb, cx| sb.open_palette(window, cx));
                        cx.stop_propagation();
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
            }))
            .when(client_titlebar, |this| {
                this.child(
                    TitleBar::new().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Scirè"),
                    ),
                )
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
                                    cx.notify();
                                }
                                SidebarAction::TogglePlaylistSection => {
                                    root.playlists_collapsed = !root.playlists_collapsed;
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
                            .child(content)
                            // Global search, overlaid top right so it sits on
                            // the same row as each page's filter tabs. Hidden
                            // while the centered command palette is open (the
                            // same entity can't be mounted twice).
                            .when(!palette_open, |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top(px(14.))
                                        .right(px(16.))
                                        .child(self.search_bar.clone()),
                                )
                            }),
                    )
                    .when(self.show_queue, |this| this.child(self.queue_panel.clone())),
            )
            .child(self.player_bar.clone())
            // Fullscreen overlay — rendered last so it sits on top.
            .when(show_fullscreen, |this| this.child(fullscreen))
            // New-playlist dialog on top of everything.
            .when(self.new_playlist_open, |this| {
                this.child(self.render_new_playlist_modal(cx))
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
