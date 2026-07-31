//! Favorites: starred songs / albums / artists (getStarred2), with unstar.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, Window, div,
    prelude::*,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use subsonic::{Starred, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::runtime;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::{focus_glow, with_focus_animation};

pub enum FavoritesEvent {
    OpenAlbum(String),
    OpenArtist(String),
}

pub struct FavoritesView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    starred: Option<Starred>,
    error: Option<String>,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Flat item index across songs → albums → artists (None = hidden).
    vi_cursor: Option<usize>,
}

impl EventEmitter<FavoritesEvent> for FavoritesView {}

impl FavoritesView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            player,
            starred: None,
            error: None,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
        };
        cx.observe(&this.player.clone(), |_, _, cx| cx.notify())
            .detach();
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
        let libraries = self.session.read(cx).library_query_ids();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                // Fetch starred items per selected library and merge,
                // deduping anything starred in several libraries.
                let mut merged: Option<subsonic::Starred> = None;
                let mut seen = std::collections::HashSet::new();
                for lib in &libraries {
                    let mut s = client
                        .get_starred2(lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    s.artist.retain(|a| seen.insert(format!("ar:{}", a.id)));
                    s.album.retain(|a| seen.insert(format!("al:{}", a.id)));
                    s.song.retain(|x| seen.insert(format!("s:{}", x.id)));
                    match &mut merged {
                        Some(m) => {
                            m.artist.extend(s.artist);
                            m.album.extend(s.album);
                            m.song.extend(s.song);
                        }
                        None => merged = Some(s),
                    }
                }
                Ok::<_, anyhow::Error>(merged)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(starred) => view.starred = starred,
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn unstar(&mut self, param: &'static str, id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.unstar(param, &id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(()) => view.load(cx),
                Err(e) => {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for FavoritesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing_id = self.player.read(cx).current_song().map(|s| s.id.clone());
        let mut rows: Vec<gpui::AnyElement> = Vec::new();

        if let Some(starred) = self.starred.clone() {
            let ns = starred.song.len();
            let na = starred.album.len();
            if !starred.song.is_empty() {
                rows.push(section_title("Songs", cx));
                let all_songs = starred.song.clone();
                for (i, song) in starred.song.iter().enumerate() {
                    let id = song.id.clone();
                    let songs = all_songs.clone();
                    let is_playing = playing_id.as_deref() == Some(song.id.as_str());
                    let focused = self.vi_cursor == Some(i);
                    let anchor = self.focus_anchor.clone();
                    let row = h_flex()
                        .id(("fav-song", i))
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .when(is_playing, |s| {
                            s.bg(cx.theme().muted)
                                .border_l_2()
                                .border_color(cx.theme().accent)
                                .text_color(cx.theme().accent)
                        })
                        .when(focused, |s| {
                            s.bg(cx.theme().muted)
                                .border_1()
                                .border_color(cx.theme().primary)
                                .shadow(focus_glow(cx))
                                .anchor_scroll(Some(anchor))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Play all starred songs starting here.
                            this.player.update(cx, |p, cx| {
                                p.play_queue(songs.clone(), i, cx);
                            });
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(song.title.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(song.artist.clone().unwrap_or_default()),
                        )
                        .child(
                            Button::new(("fav-unstar-s", i))
                                .ghost()
                                .xsmall()
                                .icon(app_icon(icons::STAR_FILLED))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.unstar("id", id.clone(), cx);
                                    cx.stop_propagation();
                                })),
                        );
                    let row = if focused {
                        with_focus_animation(format!("vi-focus-{i}"), row, cx).into_any_element()
                    } else {
                        row.into_any_element()
                    };
                    rows.push(row);
                }
            }

            if !starred.album.is_empty() {
                rows.push(section_title("Albums", cx));
                for (i, album) in starred.album.iter().enumerate() {
                    let unstar_id = album.id.clone();
                    let open_id = unstar_id.clone();
                    let focused = self.vi_cursor == Some(ns + i);
                    let anchor = self.focus_anchor.clone();
                    let row = h_flex()
                        .id(("fav-album", i))
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .when(focused, |s| {
                            s.bg(cx.theme().muted)
                                .border_1()
                                .border_color(cx.theme().primary)
                                .shadow(focus_glow(cx))
                                .anchor_scroll(Some(anchor))
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FavoritesEvent::OpenAlbum(open_id.clone()));
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(album.name.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(album.artist.clone().unwrap_or_default()),
                        )
                        .child(
                            Button::new(("fav-unstar-a", i))
                                .ghost()
                                .xsmall()
                                .icon(app_icon(icons::STAR_FILLED))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.unstar("albumId", unstar_id.clone(), cx);
                                    cx.stop_propagation();
                                })),
                        );
                    let row = if focused {
                        with_focus_animation(format!("vi-focus-{}", ns + i), row, cx)
                            .into_any_element()
                    } else {
                        row.into_any_element()
                    };
                    rows.push(row);
                }
            }

            if !starred.artist.is_empty() {
                rows.push(section_title("Artists", cx));
                for (i, artist) in starred.artist.iter().enumerate() {
                    let unstar_id = artist.id.clone();
                    let open_id = unstar_id.clone();
                    let focused = self.vi_cursor == Some(ns + na + i);
                    let anchor = self.focus_anchor.clone();
                    let row = h_flex()
                        .id(("fav-artist", i))
                        .px_2()
                        .py_1()
                        .gap_2()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .when(focused, |s| {
                            s.bg(cx.theme().muted)
                                .border_1()
                                .border_color(cx.theme().primary)
                                .shadow(focus_glow(cx))
                                .anchor_scroll(Some(anchor))
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FavoritesEvent::OpenArtist(open_id.clone()));
                        }))
                        .child(div().flex_1().child(artist.name.clone()))
                        .child(
                            Button::new(("fav-unstar-r", i))
                                .ghost()
                                .xsmall()
                                .icon(app_icon(icons::STAR_FILLED))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.unstar("artistId", unstar_id.clone(), cx);
                                    cx.stop_propagation();
                                })),
                        );
                    let row = if focused {
                        with_focus_animation(format!("vi-focus-{}", ns + na + i), row, cx)
                            .into_any_element()
                    } else {
                        row.into_any_element()
                    };
                    rows.push(row);
                }
            }

            if starred.song.is_empty() && starred.album.is_empty() && starred.artist.is_empty() {
                rows.push(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing starred yet")
                        .into_any_element(),
                );
            }
        }

        v_flex()
            .id("favorites-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_1()
            .child(div().text_lg().child("Favorites"))
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .children(rows)
    }
}

impl FavoritesView {
    /// Move the vi-mode cursor by `delta` items across the song/album/artist
    /// sections, clamping and scrolling the focused row into view.
    pub fn vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(starred) = &self.starred else {
            return;
        };
        let count = starred.song.len() + starred.album.len() + starred.artist.len();
        if count == 0 {
            return;
        }
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        self.focus_anchor.scroll_to(window, cx);
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Act on the item under the vi-mode cursor: play a song, open an album
    /// or artist depending on which section the cursor is in.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(starred) = &self.starred else {
            return;
        };
        let Some(cur) = self.vi_cursor else {
            return;
        };
        let ns = starred.song.len();
        if cur < ns {
            let songs = starred.song.clone();
            if !songs.is_empty() {
                self.player.update(cx, |p, cx| p.play_queue(songs, cur, cx));
            }
            return;
        }
        let na = starred.album.len();
        if cur < ns + na {
            if let Some(album) = starred.album.get(cur - ns) {
                cx.emit(FavoritesEvent::OpenAlbum(album.id.clone()));
            }
            return;
        }
        if let Some(artist) = starred.artist.get(cur - ns - na) {
            cx.emit(FavoritesEvent::OpenArtist(artist.id.clone()));
        }
    }
}

fn section_title(label: &'static str, cx: &Context<FavoritesView>) -> gpui::AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .mt_2()
        .child(label)
        .into_any_element()
}
