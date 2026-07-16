//! Playlist page: rename, delete, play, remove songs.

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use subsonic::{PlaylistWithSongs, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::runtime;
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::Session;
use crate::ui::{format_duration, track_extras};

pub enum PlaylistDetailEvent {
    /// Playlist was deleted — navigate away.
    Deleted,
}

pub struct PlaylistDetailView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    playlist_id: String,
    playlist: Option<PlaylistWithSongs>,
    rename_input: Entity<InputState>,
    renaming: bool,
    error: Option<String>,
}

impl EventEmitter<PlaylistDetailEvent> for PlaylistDetailView {}

impl PlaylistDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        playlist_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Playlist name"));
        cx.subscribe(
            &rename_input,
            |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.commit_rename(cx);
                }
            },
        )
        .detach();

        // Reload when the shared playlists state changes (e.g. song added).
        cx.observe(&playlists.clone(), |this: &mut Self, _, cx| this.load(cx))
            .detach();
        // Re-render when the playing song changes so the highlight stays fresh.
        cx.observe(&player.clone(), |_, _, cx| cx.notify()).detach();

        let mut this = Self {
            session,
            player,
            playlists,
            playlist_id,
            playlist: None,
            rename_input,
            renaming: false,
            error: None,
        };
        this.load(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.playlist_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_playlist(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(pl) => view.playlist = Some(pl),
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let name = self.rename_input.read(cx).value().trim().to_string();
        self.renaming = false;
        if name.is_empty() {
            cx.notify();
            return;
        }
        self.playlists.update(cx, |state, cx| {
            state.rename(self.playlist_id.clone(), name, cx);
        });
        cx.notify();
    }

    fn play_from(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(pl) = &self.playlist else { return };
        let songs = pl.songs.clone();
        self.player
            .update(cx, |p, cx| p.play_queue(songs, index, cx));
    }

    fn play_shuffled(&mut self, cx: &mut Context<Self>) {
        let Some(pl) = &self.playlist else { return };
        let songs = pl.songs.clone();
        self.player
            .update(cx, |p, cx| p.play_queue_shuffled(songs, cx));
    }
}

impl Render for PlaylistDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing_id = self.player.read(cx).current_song().map(|s| s.id.clone());
        let name = self
            .playlist
            .as_ref()
            .map(|p| p.playlist.name.clone())
            .unwrap_or_else(|| "…".into());
        let count = self
            .playlist
            .as_ref()
            .map(|p| p.songs.len())
            .unwrap_or_default();

        let header = h_flex()
            .gap_3()
            .items_center()
            .child(if self.renaming {
                div()
                    .w(px(320.))
                    .child(Input::new(&self.rename_input))
                    .into_any_element()
            } else {
                div().text_xl().child(name.clone()).into_any_element()
            })
            .child(if self.renaming {
                Button::new("rename-save")
                    .primary()
                    .xsmall()
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, cx| this.commit_rename(cx)))
            } else {
                Button::new("rename")
                    .ghost()
                    .xsmall()
                    .label("Rename")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.renaming = true;
                        let name = this
                            .playlist
                            .as_ref()
                            .map(|p| p.playlist.name.clone())
                            .unwrap_or_default();
                        this.rename_input.update(cx, |input, cx| {
                            input.set_value(name, window, cx);
                            input.focus(window, cx);
                        });
                        cx.notify();
                    }))
            })
            .child(
                Button::new("play-all")
                    .primary()
                    .xsmall()
                    .icon(app_icon(icons::PLAY))
                    .label("Play")
                    .disabled(count == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.play_from(0, cx))),
            )
            .child(
                Button::new("shuffle-all")
                    .ghost()
                    .xsmall()
                    .icon(app_icon(icons::SHUFFLE))
                    .label("Shuffle")
                    .disabled(count == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.play_shuffled(cx))),
            )
            .child(
                Button::new("delete")
                    .danger()
                    .xsmall()
                    .label("Delete")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.playlists.update(cx, |state, cx| {
                            state.delete(this.playlist_id.clone(), cx);
                        });
                        cx.emit(PlaylistDetailEvent::Deleted);
                    })),
            );

        let info_prefs = self.session.read(cx).settings.track_info.clone();

        let rows: Vec<_> = self
            .playlist
            .iter()
            .flat_map(|p| p.songs.iter())
            .enumerate()
            .map(|(i, song)| {
                let is_playing = playing_id.as_deref() == Some(song.id.as_str());
                let extras = track_extras(song, &info_prefs, true);
                let dur = song
                    .duration
                    .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                    .unwrap_or_default();
                h_flex()
                    .id(("pl-track", i))
                    .px_2()
                    .py_1()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .when(is_playing, |s| {
                        s.bg(cx.theme().muted)
                            .border_l_2()
                            .border_color(cx.theme().primary)
                            .text_color(cx.theme().primary)
                    })
                    .on_click(cx.listener(move |view, _, _, cx| view.play_from(i, cx)))
                    .child(
                        div()
                            .w(px(28.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}", i + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(song.title.clone()),
                    )
                    .when(!extras.is_empty(), |this| {
                        this.child(
                            div()
                                .max_w(px(360.))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(extras),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(dur),
                    )
                    .child(
                        Button::new(("pl-rm", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.playlists.update(cx, |state, cx| {
                                    state.remove_songs(
                                        this.playlist_id.clone(),
                                        vec![i as u32],
                                        cx,
                                    );
                                });
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let _ = window;
        v_flex()
            .id("playlist-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(header)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{count} tracks")),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(v_flex().gap_0p5().children(rows))
    }
}
