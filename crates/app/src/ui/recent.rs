//! Recently-played song list with cover thumbnails.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{Context, Entity, IntoElement, Render, Window, div, img, prelude::*, px};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use subsonic::SubsonicClient;

use crate::services::artwork;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::format_duration;

const ART_SIZE: u32 = 200;

pub struct RecentView {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    art_paths: HashMap<String, PathBuf>, // album-scoped art key → cached path
    fetching: HashSet<String>,
}

impl RecentView {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            player,
            session,
            art_paths: HashMap::new(),
            fetching: HashSet::new(),
        };
        // Fetch art for songs already in recently_played on first open.
        this.fetch_missing_art(cx);
        cx.observe(&this.player.clone(), |this, _, cx| {
            this.fetch_missing_art(cx);
            cx.notify();
        })
        .detach();
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn fetch_missing_art(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        // Keyed by album, so a run of tracks off one album downloads its art
        // once instead of once per file (Navidrome ids covers per song).
        let covers: Vec<(String, String)> = self
            .player
            .read(cx)
            .recently_played
            .iter()
            .filter_map(artwork::song_cover)
            .filter(|(_, key)| !self.art_paths.contains_key(key) && !self.fetching.contains(key))
            // key → cover id: one entry (one download) per album.
            .map(|(cover_id, key)| (key, cover_id))
            .collect::<std::collections::HashMap<_, _>>()
            .into_iter()
            .collect();

        for (key, cover_id) in covers {
            // Synchronous cache hit: render instantly on restart, no task.
            if let Some(path) = artwork::cached(&key, ART_SIZE) {
                self.art_paths.insert(key, path);
                continue;
            }
            self.fetching.insert(key.clone());
            let client = client.clone();
            cx.spawn(async move |this, cx| {
                let fetched = artwork::fetch_as(client, cover_id, key.clone(), ART_SIZE).await;
                let _ = this.update(cx, |view, cx| {
                    if let Ok(path) = fetched {
                        view.art_paths.insert(key.clone(), path);
                    }
                    view.fetching.remove(&key);
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

impl Render for RecentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let songs: Vec<_> = self.player.read(cx).recently_played.clone();
        let player = self.player.clone();

        let rows: Vec<_> = songs
            .into_iter()
            .enumerate()
            .map(|(i, song)| {
                let art = artwork::song_cover(&song)
                    .and_then(|(_, key)| self.art_paths.get(&key))
                    .cloned();
                let dur = song
                    .duration
                    .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                    .unwrap_or_default();
                let player = player.clone();
                let song_c = song.clone();
                h_flex()
                    .id(("recent-row", i))
                    .group("rrow")
                    .px_2()
                    .py_1p5()
                    .gap_3()
                    .items_center()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(cx.listener(move |_this, _, _, cx| {
                        player.update(cx, |p, cx| {
                            p.play_queue(vec![song_c.clone()], 0, cx);
                        });
                    }))
                    // Thumbnail
                    .child(
                        div()
                            .size(px(44.))
                            .rounded_md()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .flex_shrink_0()
                            .when_some(art, |this, path| {
                                this.child(img(path).size(px(44.)).rounded_md())
                            }),
                    )
                    // Title + artist
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0()
                            .child(div().text_sm().truncate().child(song.title.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(song.artist.clone().unwrap_or_default()),
                            ),
                    )
                    // Album
                    .child(
                        div()
                            .w(px(180.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(song.album.clone().unwrap_or_default()),
                    )
                    // Duration
                    .child(
                        div()
                            .w(px(48.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .text_right()
                            .child(dur),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .id("recent-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(div().text_lg().child("Recently Played"))
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing played yet"),
                )
            })
            .children(rows)
    }
}
