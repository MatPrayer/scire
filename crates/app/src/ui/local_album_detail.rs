//! Local album detail page: header (cover art + info) + track list with play,
//! queue actions. Reads from LibraryDb — no Subsonic dependency.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, Entity, IntoElement, Render, ScrollHandle, Window, div, img, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::services::library_db::{AlbumRow, LibraryDb};
use crate::services::local_library::local_art_path;
use crate::state::player::PlayerState;
use crate::ui::format_duration;

pub struct LocalAlbumDetailView {
    db: Arc<LibraryDb>,
    player: Entity<PlayerState>,
    album_id: String,
    album: Option<AlbumRow>,
    tracks: Vec<crate::services::library_db::TrackRow>,
    art_path: Option<PathBuf>,
    scroll: ScrollHandle,
}

impl LocalAlbumDetailView {
    pub fn new(
        db: Arc<LibraryDb>,
        player: Entity<PlayerState>,
        album_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            db,
            player,
            album_id,
            album: None,
            tracks: Vec::new(),
            art_path: None,
            scroll: ScrollHandle::new(),
        };
        view.load(cx);
        view
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let id = self.album_id.clone();
        // Find the single album row
        let album = self
            .db
            .albums_by_source("local")
            .unwrap_or_default()
            .into_iter()
            .find(|a| a.id == id);
        let tracks = self.db.tracks_by_album(&id).unwrap_or_default();

        if let Some(ref a) = album
            && let Some(ref hash) = a.cover_art
            && let Some(path) = local_art_path(hash)
            && path.exists()
        {
            self.art_path = Some(path);
        }
        self.album = album;
        self.tracks = tracks;
        cx.notify();
    }

    fn play_from(&mut self, index: usize, cx: &mut Context<Self>) {
        let songs: Vec<_> = self.tracks.iter().map(|t| t.clone().into_song()).collect();
        if songs.is_empty() {
            return;
        }
        self.player.update(cx, |p, cx| {
            p.play_queue(songs, index, cx);
        });
    }

    fn play_shuffled(&mut self, cx: &mut Context<Self>) {
        let songs: Vec<_> = self.tracks.iter().map(|t| t.clone().into_song()).collect();
        if songs.is_empty() {
            return;
        }
        self.player.update(cx, |p, cx| {
            p.play_queue_shuffled(songs, cx);
        });
    }
}

impl Render for LocalAlbumDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing_id = self.player.read(cx).current_song().map(|s| s.id.clone());

        let header = {
            let (name, artist, meta) = match &self.album {
                Some(a) => {
                    let sc = if a.song_count > 0 {
                        a.song_count
                    } else {
                        self.tracks.len() as i64
                    };
                    let dur = if a.duration > 0.0 {
                        format_duration(std::time::Duration::from_secs_f64(a.duration))
                    } else {
                        String::new()
                    };
                    let year = a.year.map(|y| format!("{y} · ")).unwrap_or_default();
                    (
                        a.title.clone(),
                        a.artist.clone().unwrap_or_default(),
                        format!("{year}{sc} tracks · {dur}"),
                    )
                }
                None => ("…".into(), String::new(), String::new()),
            };
            let has_songs = !self.tracks.is_empty();

            h_flex()
                .gap_4()
                .items_start()
                .flex_wrap()
                .child(
                    div()
                        .id("local-album-cover")
                        .size(px(220.))
                        .rounded_2xl()
                        .bg(cx.theme().muted)
                        .overflow_hidden()
                        .when_some(self.art_path.clone(), |this, path| {
                            this.child(img(path).size(px(220.)).rounded_2xl())
                        }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(260.))
                        .gap_2()
                        .child(div().text_2xl().font_medium().child(name))
                        .child(div().child(artist))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(meta),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .mt_1()
                                .child(
                                    Button::new("local-album-play")
                                        .primary()
                                        .icon(app_icon(icons::PLAY))
                                        .label("Play")
                                        .disabled(!has_songs)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.play_from(0, cx)),
                                        ),
                                )
                                .child(
                                    Button::new("local-album-shuffle")
                                        .ghost()
                                        .icon(app_icon(icons::SHUFFLE))
                                        .label("Shuffle")
                                        .disabled(!has_songs)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.play_shuffled(cx)),
                                        ),
                                ),
                        ),
                )
        };

        let rows: Vec<_> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, track)| {
                let is_playing = playing_id.as_deref() == Some(track.id.as_str());
                let track_no = track.track_no.map(|t| t.to_string()).unwrap_or_default();
                let dur = track
                    .duration
                    .map(|s| format_duration(std::time::Duration::from_secs_f64(s)))
                    .unwrap_or_default();
                let song_next = track.clone().into_song();
                let song_enq = track.clone().into_song();

                h_flex()
                    .id(("local-track", i))
                    .group("trow")
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
                            .child(track_no),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(track.title.clone()),
                    )
                    // Hover actions: play-next, enqueue
                    .child(
                        h_flex()
                            .gap_0p5()
                            .opacity(0.25)
                            .group_hover("trow", |s| s.opacity(1.))
                            .child(
                                Button::new(("lt-next", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(app_icon(icons::SKIP_FORWARD))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.play_next(vec![song_next.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                Button::new(("lt-enq", i))
                                    .ghost()
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.enqueue(vec![song_enq.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .w(px(44.))
                            .flex_none()
                            .justify_end()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(dur),
                    )
                    .context_menu({
                        let view = cx.entity();
                        let track_song = track.clone().into_song();
                        move |menu, _window, _cx| {
                            let v = view.clone();
                            menu.item(PopupMenuItem::new("Play").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    v.update(cx, |v, cx| v.play_from(i, cx));
                                },
                            ))
                            .item(PopupMenuItem::new("Play next").on_click({
                                let v = view.clone();
                                let song = track_song.clone();
                                move |_, _, cx: &mut gpui::App| {
                                    v.update(cx, |v, cx| {
                                        v.player
                                            .update(cx, |p, cx| p.play_next(vec![song.clone()], cx))
                                    });
                                }
                            }))
                            .item(
                                PopupMenuItem::new("Add to queue").on_click({
                                    let v = view.clone();
                                    let song = track_song.clone();
                                    move |_, _, cx: &mut gpui::App| {
                                        v.update(cx, |v, cx| {
                                            v.player.update(cx, |p, cx| {
                                                p.enqueue(vec![song.clone()], cx)
                                            })
                                        });
                                    }
                                }),
                            )
                        }
                    })
                    .into_any_element()
            })
            .collect();

        v_flex()
            .id("local-album-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            .child(
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    .bg(cx.theme().sidebar)
                    .child(header),
            )
            .child(v_flex().gap_0p5().children(rows))
            .into_any_element()
    }
}
