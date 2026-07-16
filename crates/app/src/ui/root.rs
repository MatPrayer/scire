//! Root view: login screen or main layout
//! (sidebar | content | optional queue panel / player bar).

use gpui::{
    Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render, Window, div,
    prelude::*,
};
use gpui_component::{ActiveTheme as _, TitleBar, h_flex, v_flex};

use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::radio::RadioState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::album_detail::AlbumDetailView;
use crate::ui::albums::{AlbumsEvent, AlbumsView};
use crate::ui::artists::{ArtistDetailEvent, ArtistDetailView, ArtistsEvent, ArtistsView};
use crate::ui::favorites::{FavoritesEvent, FavoritesView};
use crate::ui::fullscreen_player::{FullscreenEvent, FullscreenPlayer};
use crate::ui::login::LoginView;
use crate::ui::player_bar::{PlayerBar, PlayerBarEvent};
use crate::ui::playlist_detail::{PlaylistDetailEvent, PlaylistDetailView};
use crate::ui::queue_panel::QueuePanel;
use crate::ui::radio::RadioView;
use crate::ui::recent::RecentView;
use crate::ui::search::{SearchEvent, SearchView};
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
    Search(Entity<SearchView>),
    Playlist(Entity<PlaylistDetailView>),
    Radio(Entity<RadioView>),
    Settings(Entity<SettingsView>),
    Recent(Entity<RecentView>),
}

pub struct RootView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    radio: Entity<RadioState>,
    login: Entity<LoginView>,
    player_bar: Entity<PlayerBar>,
    queue_panel: Entity<QueuePanel>,
    fullscreen: Entity<FullscreenPlayer>,
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
    /// Selected library at last render, to rebuild views on change.
    last_library: Option<String>,
    focus_handle: FocusHandle,
}

impl RootView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let login = cx.new(|cx| LoginView::new(session.clone(), window, cx));
        let player_bar = cx.new(|cx| PlayerBar::new(player.clone(), session.clone(), cx));
        let queue_panel = cx.new(|cx| QueuePanel::new(player.clone(), cx));
        let radio = crate::state::radio::init(session.clone(), cx);
        let fullscreen = cx.new(|cx| FullscreenPlayer::new(player.clone(), session.clone(), cx));

        cx.subscribe(&player_bar, |this: &mut Self, _, event, cx| {
            match event {
                PlayerBarEvent::ToggleQueue => this.show_queue = !this.show_queue,
                PlayerBarEvent::ToggleFullscreen => {
                    this.show_fullscreen = !this.show_fullscreen;
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

        // React to connect/disconnect and library switches: build/tear down
        // content views and keep the player's API client fresh.
        cx.observe(&session, |this: &mut Self, session, cx| {
            let connected = session.read(cx).status == ConnectionStatus::Connected;
            let library = session.read(cx).library_id.clone();
            if connected != this.was_connected {
                this.was_connected = connected;
                let client = session.read(cx).client.clone();
                this.player.update(cx, |p, _| p.set_client(client));
                this.content = None;
                if connected {
                    this.last_library = library;
                    this.navigate(NavSection::Albums, None, cx);
                }
            } else if connected && library != this.last_library {
                // Library changed: rebuild the current catalog view.
                this.last_library = library;
                if let Some(section) = this.section {
                    this.navigate(section, None, cx);
                }
            }
            cx.notify();
        })
        .detach();

        Self {
            session,
            player,
            playlists,
            radio,
            login,
            player_bar,
            queue_panel,
            fullscreen,
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
            last_library: None,
            focus_handle: cx.focus_handle(),
        }
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
                let view = cx.new(|cx| AlbumsView::new(self.session.clone(), cx));
                cx.subscribe(&view, |this: &mut Self, _, event, cx| {
                    let AlbumsEvent::OpenAlbum(id) = event;
                    this.open_album(id.clone(), cx);
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
            NavSection::Search => {
                let Some(window) = window else {
                    return; // search needs a window for input focus
                };
                let view = cx.new(|cx| {
                    SearchView::new(self.session.clone(), self.player.clone(), window, cx)
                });
                cx.subscribe(&view, |this: &mut Self, _, event, cx| match event {
                    SearchEvent::OpenAlbum(id) => this.open_album(id.clone(), cx),
                    SearchEvent::OpenArtist(id) => this.open_artist(id.clone(), cx),
                })
                .detach();
                Content::Search(view)
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
        self.content = Some(Content::AlbumDetail(view));
        cx.notify();
    }

    fn open_artist(&mut self, id: String, cx: &mut Context<Self>) {
        self.push_history();
        self.current_entry = Some(NavEntry::Artist(id.clone()));
        let view = cx.new(|cx| ArtistDetailView::new(self.session.clone(), id, cx));
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

        if !connected {
            let client_titlebar = self.session.read(cx).settings.client_titlebar;
            return v_flex()
                .size_full()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .when(client_titlebar, |this| {
                    this.child(
                        TitleBar::new().child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Navidrome"),
                        ),
                    )
                })
                .child(div().flex_1().min_h_0().child(self.login.clone()))
                .into_any_element();
        }

        let content: gpui::AnyElement = match &self.content {
            Some(Content::Albums(v)) => v.clone().into_any_element(),
            Some(Content::Artists(v)) => v.clone().into_any_element(),
            Some(Content::ArtistDetail(v)) => v.clone().into_any_element(),
            Some(Content::AlbumDetail(v)) => v.clone().into_any_element(),
            Some(Content::Favorites(v)) => v.clone().into_any_element(),
            Some(Content::Search(v)) => v.clone().into_any_element(),
            Some(Content::Playlist(v)) => v.clone().into_any_element(),
            Some(Content::Radio(v)) => v.clone().into_any_element(),
            Some(Content::Settings(v)) => v.clone().into_any_element(),
            Some(Content::Recent(v)) => v.clone().into_any_element(),
            None => div().into_any_element(),
        };

        let sidebar_model = SidebarModel {
            active: self.section,
            active_playlist: self.active_playlist.clone(),
            playlists: self
                .playlists
                .read(cx)
                .playlists
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect(),
            libraries: self
                .session
                .read(cx)
                .music_folders
                .iter()
                .map(|f| (f.id(), f.name.clone().unwrap_or_else(|| f.id())))
                .collect(),
            active_library: self.session.read(cx).library_id.clone(),
        };

        let this = cx.entity();
        let fullscreen = self.fullscreen.clone();
        let show_fullscreen = self.show_fullscreen;
        let client_titlebar = self.session.read(cx).settings.client_titlebar;

        v_flex()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Keyboard navigation: track focus so on_key_down fires.
            .track_focus(&self.focus_handle)
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
                        if this.show_fullscreen {
                            this.show_fullscreen = false;
                            cx.notify();
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
            }))
            .when(client_titlebar, |this| {
                this.child(
                    TitleBar::new().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Navidrome"),
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
                                    root.playlists.update(cx, |p, cx| {
                                        p.create("New Playlist".into(), Vec::new(), cx);
                                    });
                                }
                                SidebarAction::SetLibrary(id) => {
                                    root.session.update(cx, |s, cx| s.set_library(id, cx));
                                }
                            });
                        },
                        cx,
                    ))
                    .child(div().flex_1().min_w_0().h_full().child(content))
                    .when(self.show_queue, |this| this.child(self.queue_panel.clone())),
            )
            .child(self.player_bar.clone())
            // Fullscreen overlay — rendered last so it sits on top.
            .when(show_fullscreen, |this| this.child(fullscreen))
            .into_any_element()
    }
}
