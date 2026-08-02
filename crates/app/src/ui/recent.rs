//! Recently-played song list with cover thumbnails.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    Context, Entity, IntoElement, Render, SharedString, UniformListScrollHandle, Window, div, img,
    prelude::*, px, uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use subsonic::{Song, SubsonicClient};

use crate::services::artwork;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::format_duration;

const ART_SIZE: u32 = 200;
/// Fixed row height — `uniform_list` requires every row to be the same size.
/// 44px thumbnail plus the row's vertical padding.
const ROW_H: f32 = 56.;

/// One row's pre-formatted contents.
///
/// Built when the recently-played list changes, not per frame: this view
/// observes `PlayerState`, which notifies on every position tick, so anything
/// done per render happens several times a second for as long as the page is
/// open.
struct Row {
    title: SharedString,
    artist: SharedString,
    album: SharedString,
    duration: SharedString,
    /// Album-scoped artwork key (Navidrome ids covers per *song*, so keying on
    /// the album is what stops one download per track of the same album).
    art_key: Option<String>,
}

fn to_rows(songs: &[Song]) -> Vec<Row> {
    songs
        .iter()
        .map(|song| Row {
            title: song.title.clone().into(),
            artist: song.artist.clone().unwrap_or_default().into(),
            album: song.album.clone().unwrap_or_default().into(),
            duration: song
                .duration
                .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                .unwrap_or_default()
                .into(),
            art_key: artwork::song_cover(song).map(|(_, key)| key),
        })
        .collect()
}

/// Cheap stand-in for "the list changed": it is only ever pushed to at the
/// front and capped, so length plus the head id can't miss an edit.
fn signature(songs: &[Song]) -> (usize, Option<String>) {
    (songs.len(), songs.first().map(|s| s.id.clone()))
}

pub struct RecentView {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    songs: Vec<Song>,
    rows: Vec<Row>,
    /// Signature of `songs`, so a position tick doesn't rebuild everything.
    signature: (usize, Option<String>),
    /// Whether a client existed at the last art fetch, so one appearing later
    /// retries it exactly once.
    art_had_client: bool,
    art_paths: HashMap<String, PathBuf>, // album-scoped art key → cached path
    fetching: HashSet<String>,
    scroll: UniformListScrollHandle,
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
            songs: Vec::new(),
            rows: Vec::new(),
            signature: (0, None),
            art_had_client: false,
            art_paths: HashMap::new(),
            fetching: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
        };
        this.refresh(cx);
        cx.observe(&this.player.clone(), |this, _, cx| {
            // PlayerState notifies on every position tick; only a real change
            // to the list is worth a rebuild and a repaint.
            if this.refresh(cx) {
                cx.notify();
            }
        })
        .detach();
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// Resync from the player. Returns whether anything changed.
    fn refresh(&mut self, cx: &mut Context<Self>) -> bool {
        let signature = signature(&self.player.read(cx).recently_played);
        // A client appearing is also a change: the view can be built before the
        // connect lands, and that first art fetch is a no-op without one.
        let has_client = self.client(cx).is_some();
        if signature == self.signature && has_client == self.art_had_client {
            return false;
        }
        self.signature = signature;
        self.art_had_client = has_client;
        self.songs = self.player.read(cx).recently_played.clone();
        self.rows = to_rows(&self.songs);
        self.fetch_missing_art(cx);
        true
    }

    fn fetch_missing_art(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        // Keyed by album, so a run of tracks off one album downloads its art
        // once instead of once per file (Navidrome ids covers per song).
        let covers: Vec<(String, String)> = self
            .songs
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

    fn render_row(&self, entity: &Entity<Self>, ix: usize, cx: &gpui::App) -> gpui::AnyElement {
        let row = &self.rows[ix];
        let art = row
            .art_key
            .as_ref()
            .and_then(|key| self.art_paths.get(key))
            .cloned();
        let view = entity.clone();
        h_flex()
            .id(("recent-row", ix))
            .h(px(ROW_H))
            .px_2()
            .gap_3()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .on_click(move |_, _, cx: &mut gpui::App| {
                view.update(cx, |this, cx| {
                    let Some(song) = this.songs.get(ix).cloned() else {
                        return;
                    };
                    this.player
                        .update(cx, |p, cx| p.play_queue(vec![song], 0, cx));
                });
            })
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
                    .child(div().text_sm().truncate().child(row.title.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(row.artist.clone()),
                    ),
            )
            // Album
            .child(
                div()
                    .w(px(180.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(row.album.clone()),
            )
            // Duration
            .child(
                div()
                    .w(px(48.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .text_right()
                    .child(row.duration.clone()),
            )
            .into_any_element()
    }
}

impl Render for RecentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.rows.is_empty();
        let entity = cx.entity();
        let list = uniform_list("recent-list", self.rows.len(), move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|ix| view.render_row(&entity, ix, cx))
                .collect::<Vec<_>>()
        })
        .flex_1()
        .px_4()
        .track_scroll(self.scroll.clone());

        v_flex()
            .id("recent-scroll")
            .size_full()
            .pt_4()
            .gap_2()
            .child(div().px_4().text_lg().child("Recently Played"))
            .when(empty, |this| {
                this.child(
                    div()
                        .px_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Nothing played yet"),
                )
            })
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, title: &str) -> Song {
        serde_json::from_value(serde_json::json!({ "id": id, "title": title }))
            .expect("minimal song")
    }

    #[test]
    fn signature_ignores_replays_of_the_head_track() {
        let list = vec![song("a", "A"), song("b", "B")];
        // Same list on a later position tick: no rebuild, no repaint.
        assert_eq!(signature(&list), signature(&list.clone()));
    }

    #[test]
    fn signature_catches_a_new_track_at_the_front() {
        let before = vec![song("a", "A")];
        let after = vec![song("b", "B"), song("a", "A")];
        assert_ne!(signature(&before), signature(&after));
    }

    #[test]
    fn signature_catches_a_reorder_at_the_capped_length() {
        // At the 50-song cap the length stops moving, so the head id is what
        // distinguishes "played something already in the list" from no change.
        let before = vec![song("a", "A"), song("b", "B")];
        let after = vec![song("b", "B"), song("a", "A")];
        assert_ne!(signature(&before), signature(&after));
    }

    #[test]
    fn rows_render_missing_metadata_as_empty_not_placeholder_text() {
        let rows = to_rows(&[song("a", "Title")]);
        assert_eq!(rows[0].title, "Title");
        assert!(rows[0].artist.is_empty());
        assert!(rows[0].album.is_empty());
        assert!(rows[0].duration.is_empty());
    }
}
