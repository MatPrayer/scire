//! Album page: header (artwork, star, rating) + track list with play,
//! queue and playlist actions.

use std::path::PathBuf;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, img, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use subsonic::{AlbumWithSongs, Song, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::Session;
use crate::ui::{format_duration, track_extras};

const ART_SIZE: u32 = 600;

pub enum AlbumDetailEvent {
    OpenArtist(String),
}

pub struct AlbumDetailView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    album_id: String,
    album: Option<AlbumWithSongs>,
    art_path: Option<PathBuf>,
    error: Option<String>,
    /// Last observed playing-song id; used to refresh play counts when a track
    /// from this album finishes (its scrobble updates the server count).
    last_playing_id: Option<String>,
    /// Full-resolution cover lightbox open.
    show_full_art: bool,
    /// High-res cover for the lightbox (fetched lazily on first open).
    full_art_path: Option<PathBuf>,
}

impl EventEmitter<AlbumDetailEvent> for AlbumDetailView {}

impl AlbumDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        album_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        // Highlight the playing track as it changes; refresh play counts when
        // playback moves off a track belonging to this album.
        cx.observe(&player.clone(), |this: &mut Self, player, cx| {
            let cur = player.read(cx).current_song().map(|s| s.id.clone());
            if cur != this.last_playing_id {
                let prev = this.last_playing_id.take();
                this.last_playing_id = cur.clone();
                let in_album = |id: &Option<String>| {
                    id.as_ref().is_some_and(|id| {
                        this.album
                            .as_ref()
                            .is_some_and(|a| a.song.iter().any(|s| &s.id == id))
                    })
                };
                if in_album(&prev) || in_album(&cur) {
                    this.load(cx);
                }
            }
            cx.notify();
        })
        .detach();
        let last_playing_id = player.read(cx).current_song().map(|s| s.id.clone());
        let mut this = Self {
            session,
            player,
            playlists,
            album_id,
            album: None,
            art_path: None,
            error: None,
            last_playing_id,
            show_full_art: false,
            full_art_path: None,
        };
        this.load(cx);
        this
    }

    /// Open the full-resolution cover lightbox, fetching a large version once.
    fn open_full_art(&mut self, cx: &mut Context<Self>) {
        self.show_full_art = true;
        if self.full_art_path.is_none()
            && let Some(client) = self.client(cx)
            && let Some(cover) = self.album.as_ref().and_then(|a| a.album.cover_art.clone())
        {
            cx.spawn(async move |this, cx| {
                if let Ok(path) = artwork::fetch(client, cover, 1500).await {
                    let _ = this.update(cx, |view, cx| {
                        view.full_art_path = Some(path);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_album(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(album) => {
                        if let Some(cover) = album.album.cover_art.clone() {
                            view.fetch_art(cover, cx);
                        }
                        view.album = Some(album);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_art(&self, cover_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn play_from(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(album) = &self.album else { return };
        let songs = album.song.clone();
        self.player.update(cx, |player, cx| {
            player.play_queue(songs, index, cx);
        });
    }

    fn play_shuffled(&mut self, cx: &mut Context<Self>) {
        let Some(album) = &self.album else { return };
        let songs = album.song.clone();
        self.player.update(cx, |player, cx| {
            player.play_queue_shuffled(songs, cx);
        });
    }

    /// Toggle star on the album (optimistic local update).
    fn toggle_album_star(&mut self, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = album.album.id.clone();
        let starred = album.album.starred.is_some();
        album.album.starred = if starred { None } else { Some(String::new()) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if starred {
                    client.unstar("albumId", &id).await
                } else {
                    client.star("albumId", &id).await
                }
                .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Toggle star on one track (optimistic local update).
    fn toggle_song_star(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(song) = album.song.get_mut(index) else {
            return;
        };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = song.id.clone();
        let starred = song.starred.is_some();
        song.starred = if starred { None } else { Some(String::new()) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if starred {
                    client.unstar("id", &id).await
                } else {
                    client.star("id", &id).await
                }
                .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Rate the album 1-5; clicking the current rating clears it.
    fn rate_album(&mut self, rating: u8, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = album.album.id.clone();
        let new = if album.album.user_rating == Some(rating) {
            0
        } else {
            rating
        };
        album.album.user_rating = if new == 0 { None } else { Some(new) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .set_rating(&id, new)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Per-track "add to playlist" popover.
    fn playlist_popover(&self, index: usize, song: &Song, _cx: &Context<Self>) -> impl IntoElement {
        let playlists = self.playlists.clone();
        let song_id = song.id.clone();
        Popover::new(("addpl", index))
            .trigger(
                Button::new(("addpl-btn", index))
                    .ghost()
                    .xsmall()
                    .icon(app_icon(icons::LIST_PLUS)),
            )
            .content(move |state, _window, cx| {
                let entries: Vec<(String, String)> = playlists
                    .read(cx)
                    .playlists
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect();
                let playlists = playlists.clone();
                let song_id = song_id.clone();
                let mut menu = v_flex().gap_0p5().min_w(px(180.));
                if entries.is_empty() {
                    menu = menu.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No playlists yet"),
                    );
                }
                for (i, (pl_id, pl_name)) in entries.into_iter().enumerate() {
                    let playlists = playlists.clone();
                    let song_id = song_id.clone();
                    menu = menu.child(
                        div()
                            .id(("pl-opt", i))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |state, _, window, cx| {
                                playlists.update(cx, |p, cx| {
                                    p.add_song(pl_id.clone(), song_id.clone(), cx);
                                });
                                state.dismiss(window, cx);
                            }))
                            .child(pl_name),
                    );
                }
                let _ = state;
                menu
            })
    }
}

impl Render for AlbumDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing_id = self.player.read(cx).current_song().map(|s| s.id.clone());

        let (album_starred, album_rating) = self
            .album
            .as_ref()
            .map(|a| (a.album.starred.is_some(), a.album.user_rating.unwrap_or(0)))
            .unwrap_or((false, 0));

        let header = {
            let (name, artist, artist_id, meta) = match &self.album {
                Some(a) => {
                    let songs = a.album.song_count.unwrap_or(a.song.len() as u32);
                    let dur = a
                        .album
                        .duration
                        .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                        .unwrap_or_default();
                    let year = a.album.year.map(|y| format!("{y} · ")).unwrap_or_default();
                    (
                        a.album.name.clone(),
                        a.album.artist.clone().unwrap_or_default(),
                        a.album.artist_id.clone(),
                        format!("{year}{songs} tracks · {dur}"),
                    )
                }
                None => ("…".into(), String::new(), None, String::new()),
            };
            let has_songs = self.album.as_ref().is_some_and(|a| !a.song.is_empty());

            let rating_stars = h_flex().gap_0p5().children((1..=5u8).map(|r| {
                div()
                    .id(("rate", r as usize))
                    .cursor_pointer()
                    .text_color(if r <= album_rating {
                        cx.theme().accent
                    } else {
                        cx.theme().muted_foreground
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.rate_album(r, cx)))
                    .child(app_icon(if r <= album_rating {
                        icons::STAR_FILLED
                    } else {
                        icons::STAR_OUTLINE
                    }))
            }));

            h_flex()
                .gap_4()
                .items_start()
                .flex_wrap()
                .child(
                    div()
                        .id("album-cover")
                        .size(px(220.))
                        .rounded_2xl()
                        .bg(cx.theme().muted)
                        .overflow_hidden()
                        .when_some(self.art_path.clone(), |this, path| {
                            // Click to view the cover at full resolution.
                            this.cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.open_full_art(cx)))
                                .child(img(path).size(px(220.)).rounded_2xl())
                        }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(260.))
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().text_2xl().font_medium().child(name))
                                .child(
                                    Button::new("album-star")
                                        .ghost()
                                        .xsmall()
                                        .icon(app_icon(if album_starred {
                                            icons::STAR_FILLED
                                        } else {
                                            icons::STAR_OUTLINE
                                        }))
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.toggle_album_star(cx)
                                            }),
                                        ),
                                ),
                        )
                        .child(match artist_id {
                            // Artist name links to the artist page.
                            Some(id) => div()
                                .id("album-artist")
                                .cursor_pointer()
                                .hover(|s| s.text_color(cx.theme().accent))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(AlbumDetailEvent::OpenArtist(id.clone()));
                                }))
                                .child(artist)
                                .into_any_element(),
                            None => div().child(artist).into_any_element(),
                        })
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(meta),
                        )
                        .child(rating_stars)
                        .child(
                            h_flex()
                                .gap_2()
                                .mt_1()
                                .child(
                                    Button::new("album-play")
                                        .primary()
                                        .icon(app_icon(icons::PLAY))
                                        .label("Play")
                                        .disabled(!has_songs)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.play_from(0, cx)),
                                        ),
                                )
                                .child(
                                    Button::new("album-shuffle")
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

        let info_prefs = self.session.read(cx).settings.track_info.clone();

        let rows: Vec<_> = self
            .album
            .clone()
            .iter()
            .flat_map(|a| a.song.iter())
            .enumerate()
            .map(|(i, song)| {
                let is_playing = playing_id.as_deref() == Some(song.id.as_str());
                let starred = song.starred.is_some();
                let track_no = song.track.map(|t| t.to_string()).unwrap_or_default();
                let extras = track_extras(song, &info_prefs, false);
                let dur = song
                    .duration
                    .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                    .unwrap_or_default();
                let plays = song
                    .play_count
                    .filter(|&p| p > 0)
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let song_next = song.clone();
                let song_enq = song.clone();
                // Right-click context menu data.
                let menu_song = song.clone();
                let menu_artist_id = song.artist_id.clone();
                let menu_view = cx.entity();
                let menu_song_id = song.id.clone();
                let menu_playlists = self.playlists.clone();
                let menu_pl_list: Vec<(String, String)> = self
                    .playlists
                    .read(cx)
                    .playlists
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect();
                h_flex()
                    .id(("track", i))
                    .group("trow")
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .when(is_playing, |s| {
                        // `primary` is the vivid theme colour; `accent` is a
                        // background tint with poor text contrast.
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
                            .child(song.title.clone()),
                    )
                    .when(!extras.is_empty(), |this| {
                        this.child(
                            div()
                                .max_w(px(320.))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(extras),
                        )
                    })
                    // Hover actions: play-next, enqueue, star, add-to-playlist.
                    .child(
                        h_flex()
                            .gap_0p5()
                            .opacity(0.25)
                            .group_hover("trow", |s| s.opacity(1.))
                            .child(
                                Button::new(("t-next", i))
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
                                Button::new(("t-enq", i))
                                    .ghost()
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.enqueue(vec![song_enq.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                Button::new(("t-star", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(app_icon(if starred {
                                        icons::STAR_FILLED
                                    } else {
                                        icons::STAR_OUTLINE
                                    }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_song_star(i, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(self.playlist_popover(i, song, cx)),
                    )
                    // Play count and duration: fixed right-aligned columns,
                    // same text size, extra margin between them.
                    .child(
                        h_flex()
                            .w(px(40.))
                            .flex_none()
                            .justify_end()
                            .mr_3()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(plays),
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
                    .context_menu(move |menu, window, cx| {
                        let play_view = menu_view.clone();
                        let next_view = menu_view.clone();
                        let next_song = menu_song.clone();
                        let enq_view = menu_view.clone();
                        let enq_song = menu_song.clone();
                        let star_view = menu_view.clone();
                        // Clone per open: the outer builder is called repeatedly.
                        let pl_list = menu_pl_list.clone();
                        let playlists = menu_playlists.clone();
                        let song_id = menu_song_id.clone();
                        let mut menu = menu
                            .item(PopupMenuItem::new("Play").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    play_view.update(cx, |v, cx| v.play_from(i, cx));
                                },
                            ))
                            .item(PopupMenuItem::new("Play next").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let song = next_song.clone();
                                    next_view.update(cx, |v, cx| {
                                        v.player.update(cx, |p, cx| p.play_next(vec![song], cx))
                                    });
                                },
                            ))
                            .item(PopupMenuItem::new("Add to queue").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let song = enq_song.clone();
                                    enq_view.update(cx, |v, cx| {
                                        v.player.update(cx, |p, cx| p.enqueue(vec![song], cx))
                                    });
                                },
                            ))
                            .submenu("Save to playlist", window, cx, move |sub, _w, _c| {
                                if pl_list.is_empty() {
                                    return sub.item(
                                        PopupMenuItem::new("No playlists yet").disabled(true),
                                    );
                                }
                                let mut sub = sub;
                                for (pid, pname) in &pl_list {
                                    let playlists = playlists.clone();
                                    let pid = pid.clone();
                                    let song_id = song_id.clone();
                                    sub = sub.item(PopupMenuItem::new(pname.clone()).on_click(
                                        move |_, _, cx: &mut gpui::App| {
                                            playlists.update(cx, |pl, cx| {
                                                pl.add_song(pid.clone(), song_id.clone(), cx)
                                            });
                                        },
                                    ));
                                }
                                sub
                            })
                            .item(
                                PopupMenuItem::new(if starred { "Unstar" } else { "Star" })
                                    .on_click(move |_, _, cx: &mut gpui::App| {
                                        star_view.update(cx, |v, cx| v.toggle_song_star(i, cx));
                                    }),
                            );
                        if let Some(aid) = menu_artist_id.clone() {
                            let artist_view = menu_view.clone();
                            menu = menu.item(PopupMenuItem::separator()).item(
                                PopupMenuItem::new("Go to artist").on_click(
                                    move |_, _, cx: &mut gpui::App| {
                                        artist_view.update(cx, |_, cx| {
                                            cx.emit(AlbumDetailEvent::OpenArtist(aid.clone()))
                                        });
                                    },
                                ),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            })
            .collect();

        let scroll = v_flex()
            .id("album-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            // Header card matches the artist page framing.
            .child(
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    .bg(cx.theme().sidebar)
                    .child(header),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(v_flex().gap_0p5().children(rows));

        div()
            .relative()
            .size_full()
            .child(scroll)
            // Full-resolution cover lightbox; click anywhere to dismiss.
            .when(self.show_full_art, |this| {
                this.child(
                    div()
                        .id("album-lightbox")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_8()
                        .occlude()
                        .cursor_pointer()
                        .bg(gpui::hsla(0., 0., 0., 0.88))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_full_art = false;
                            cx.notify();
                        }))
                        .when_some(
                            self.full_art_path.clone().or_else(|| self.art_path.clone()),
                            |this, path| {
                                this.child(
                                    img(path).max_w(px(820.)).max_h(px(820.)).rounded_lg(),
                                )
                            },
                        ),
                )
            })
    }
}
