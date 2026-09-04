//! Artist list (grouped by index letter) and artist detail (their albums).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, SharedString, UniformListScrollHandle,
    Window, div, img, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::link::Link;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt, h_flex, v_flex};
use subsonic::{Album, ArtistIndex, ArtistInfo2, ArtistWithAlbums, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::library_db::{LibraryDb, LibraryStats};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::{focus_glow, strip_html, truncate_at_word, with_focus_animation};

const ART_SIZE: u32 = 320;

/// Card text metrics — matched to the album grid so the two pages line up.
/// Fixed height because the virtualized rows must all be the same size.
const NAME_LINE_H: f32 = 20.;
const META_LINE_H: f32 = 17.;
const TEXT_BLOCK_H: f32 = NAME_LINE_H * 2. + META_LINE_H;
/// Rows of covers fetched beyond the visible range, so scrolling doesn't
/// chase the art. Also the pre-layout guess, before a viewport is measured.
const ART_LOOKAHEAD_ROWS: usize = 4;

/// Column guess for the very first frame, before anything has been laid out.
const FALLBACK_COLS: usize = 5;

pub enum ArtistsEvent {
    OpenArtist(String),
}

/// One card's pre-formatted contents. Built when the list changes rather than
/// per frame: the index runs to thousands of entries and re-deriving the
/// strings on every repaint is what made switching to this page hitch.
struct Card {
    id: SharedString,
    name: SharedString,
    albums: SharedString,
    /// First character of the name, drawn in the empty circle when the artist
    /// has no image.
    initial: SharedString,
}

fn to_cards(artists: &[subsonic::Artist]) -> Vec<Card> {
    artists
        .iter()
        .map(|artist| Card {
            id: artist.id.clone().into(),
            name: artist.name.clone().into(),
            albums: artist
                .album_count
                .map(|n| {
                    if n == 1 {
                        "1 album".into()
                    } else {
                        format!("{n} albums")
                    }
                })
                .unwrap_or_default()
                .into(),
            initial: artist
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default()
                .into(),
        })
        .collect()
}

/// Flatten the server's index buckets into one alphabetical list. The grid has
/// no letter headings, and `getArtists` already sorts within each bucket.
fn flatten_index(index: Vec<ArtistIndex>) -> Vec<subsonic::Artist> {
    let mut artists: Vec<subsonic::Artist> =
        index.into_iter().flat_map(|bucket| bucket.artist).collect();
    artists.sort_by_key(|a| a.name.to_lowercase());
    artists
}

pub struct ArtistsView {
    session: Entity<Session>,
    /// Last Navidrome sync's artists, painted while the request runs.
    library_db: Arc<LibraryDb>,
    artists: Vec<subsonic::Artist>,
    cards: Vec<Card>,
    /// `cards` is the cached copy, not the server's answer.
    cached: bool,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight cover downloads, kept so they're cancelled when this view is
    /// dropped on navigation instead of starving the next page.
    art_tasks: Vec<gpui::Task<()>>,
    /// A coalesced repaint is scheduled; batches a burst of cover arrivals into
    /// one re-render instead of one per completed download.
    art_repaint_pending: bool,
    /// Resolution thumbnails are currently fetched at, so a cover-size change
    /// can drop stale art and refetch.
    art_px: u32,
    /// Card range whose covers were last requested; skips redoing the work on
    /// every frame when the viewport hasn't moved.
    art_range: Option<(usize, usize)>,
    scroll: UniformListScrollHandle,
    loading: bool,
    error: Option<String>,
    /// Card index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Catalog totals shown in the header, for the selected libraries.
    stats: LibraryStats,
    /// Tracks the grid's width against the window's, so the column count
    /// follows a resize on the same frame instead of one behind it.
    live_width: crate::ui::LiveWidth,
}

impl EventEmitter<ArtistsEvent> for ArtistsView {}

impl ArtistsView {
    pub fn new(
        session: Entity<Session>,
        library_db: Arc<LibraryDb>,
        cx: &mut Context<Self>,
    ) -> Self {
        let art_px = session.read(cx).settings.cover_size.art_px();
        let mut this = Self {
            session,
            library_db,
            artists: Vec::new(),
            cards: Vec::new(),
            cached: false,
            art_paths: HashMap::new(),
            art_tasks: Vec::new(),
            art_repaint_pending: false,
            art_px,
            art_range: None,
            scroll: UniformListScrollHandle::new(),
            loading: false,
            error: None,
            vi_cursor: None,
            stats: LibraryStats::default(),
            live_width: crate::ui::LiveWidth::default(),
        };
        this.refresh_stats(cx);
        this.seed_from_cache(cx);
        this.load(cx);
        this
    }

    /// Fill the grid from the last Navidrome sync so it paints on the first
    /// frame instead of after `getArtists` answers. Same shape as the album
    /// grid's seed: gated on a *configured* server (not a connected one) so it
    /// also covers the pre-connect wait, and filtered by the sync's recorded
    /// library provenance so a subset selection shows only its own artists.
    fn seed_from_cache(&mut self, cx: &mut Context<Self>) {
        if self.session.read(cx).settings.server.is_none() || !self.artists.is_empty() {
            return;
        }
        let Ok(rows) = self.library_db.artists_by_source("navidrome") else {
            return;
        };
        // Rows from a sync that predates the provenance column carry no library
        // id; with a subset selected they're skipped rather than guessed at.
        let libraries = self.session.read(cx).library_ids.clone();
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|row| {
                libraries.is_empty()
                    || row
                        .library_id
                        .as_ref()
                        .is_some_and(|id| libraries.contains(id))
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        let counts = self
            .library_db
            .album_counts_by_artist("navidrome")
            .unwrap_or_default();
        self.artists = rows
            .into_iter()
            .map(|row| subsonic::Artist {
                album_count: counts.get(&row.id).map(|n| *n as u32),
                // Ids are stored namespaced by the sync; strip it back off so a
                // placeholder card opens the same artist the live list would,
                // and so its cover survives the swap instead of re-downloading.
                id: row
                    .id
                    .strip_prefix("navidrome:artist:")
                    .unwrap_or(&row.id)
                    .to_string(),
                name: row.name,
                cover_art: row.cover_art,
                artist_image_url: None,
                biography: None,
                starred: None,
            })
            .collect();
        self.cards = to_cards(&self.artists);
        self.cached = true;
        // Covers are fetched from `render`, driven by the viewport.
    }

    /// A client exists now. This view is built during the pre-connect window,
    /// where `load` had nothing to fetch with and bailed — without this it
    /// would keep showing the seeded cache for the rest of the session.
    /// Re-read the header totals from the cache; see `AlbumsView::refresh_stats`.
    fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        let libraries = self.session.read(cx).library_ids.clone();
        if let Ok(stats) = self.library_db.library_stats("navidrome", &libraries) {
            self.stats = stats;
        }
    }

    pub fn client_ready(&mut self, cx: &mut Context<Self>) {
        self.refresh_stats(cx);
        if self.cached || self.artists.is_empty() {
            self.load(cx);
        }
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let libraries = self.session.read(cx).library_query_ids();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                // One request per selected library; merge the index buckets
                // and dedupe artists that live in several libraries.
                let mut merged: Vec<subsonic::ArtistIndex> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for lib in &libraries {
                    let index = client
                        .get_artists(lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    for bucket in index {
                        let artists: Vec<_> = bucket
                            .artist
                            .into_iter()
                            .filter(|a| seen.insert(a.id.clone()))
                            .collect();
                        if artists.is_empty() {
                            continue;
                        }
                        match merged.iter_mut().find(|b| b.name == bucket.name) {
                            Some(existing) => existing.artist.extend(artists),
                            None => merged.push(subsonic::ArtistIndex {
                                name: bucket.name,
                                artist: artists,
                            }),
                        }
                    }
                }
                Ok::<_, anyhow::Error>(merged)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.loading = false;
                match result {
                    Ok(index) => {
                        view.artists = flatten_index(index);
                        view.cards = to_cards(&view.artists);
                        view.cached = false;
                        // Covers follow from `render`; `getArtists` returns the
                        // entire library in one response and fetching all of it
                        // here would stat the disk thousands of times on the
                        // main thread.
                        view.art_range = None;
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_art(&mut self, artist: &subsonic::Artist, cx: &mut Context<Self>) {
        if self.art_paths.contains_key(&artist.id) {
            return;
        }
        let Some(cover_id) = artist.cover_art.clone() else {
            return;
        };
        // Synchronous cache hit: show it immediately, no async round-trip.
        if let Some(path) = artwork::cached(&cover_id, self.art_px) {
            self.art_paths.insert(artist.id.clone(), path);
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let artist_id = artist.id.clone();
        let art_px = self.art_px;
        // Soft-cap the bag: the oldest entries are covers scrolled past long
        // ago and already downloaded, so dropping their handles just frees
        // memory.
        if self.art_tasks.len() > 256 {
            self.art_tasks.drain(0..128);
        }
        let task = cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, art_px).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_paths.insert(artist_id, path);
                    view.schedule_art_repaint(cx);
                });
            }
        });
        self.art_tasks.push(task);
    }

    /// Coalesce cover-arrival repaints: a fast scroll completes many downloads
    /// in quick succession, and re-rendering the grid per completion is wasted
    /// work.
    fn schedule_art_repaint(&mut self, cx: &mut Context<Self>) {
        if self.art_repaint_pending {
            return;
        }
        self.art_repaint_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.art_repaint_pending = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop cached thumbnail paths and refetch at the current `art_px` (called
    /// when the cover-size setting changes).
    fn refetch_art(&mut self) {
        self.art_paths.clear();
        // Cancel in-flight downloads at the old resolution.
        self.art_tasks.clear();
        self.art_range = None;
    }

    /// Fetch covers for the rows on screen, plus a few past the edge.
    ///
    /// `getArtists` hands over the whole library at once, so fetching every
    /// cover when the list lands would stat the disk once per artist on the
    /// main thread and queue a download per miss. Driving it from the viewport
    /// keeps the work proportional to what is actually on screen.
    fn ensure_art_for_viewport(&mut self, row_count: usize, cols: usize, cx: &mut Context<Self>) {
        if self.cards.is_empty() || cols == 0 || row_count == 0 {
            return;
        }
        let base = self.scroll.0.borrow().base_handle.clone();
        let viewport = f32::from(base.bounds().size.height);
        let content = f32::from(base.max_offset().height) + viewport;
        let (first_row, last_row) = if viewport > 0. && content > 0. {
            let row_h = content / row_count as f32;
            let scrolled = f32::from(-base.offset().y).max(0.);
            (
                (scrolled / row_h).floor() as usize,
                ((scrolled + viewport) / row_h).ceil() as usize,
            )
        } else {
            // Pre-layout: no measured viewport yet, so cover a guessed screenful
            // rather than nothing — the next frame corrects it.
            (0, ART_LOOKAHEAD_ROWS)
        };
        let start = first_row.saturating_sub(ART_LOOKAHEAD_ROWS) * cols;
        let end = ((last_row + 1 + ART_LOOKAHEAD_ROWS) * cols).min(self.cards.len());
        if start >= end || self.art_range == Some((start, end)) {
            return;
        }
        self.art_range = Some((start, end));
        let window: Vec<subsonic::Artist> = self.artists[start..end].to_vec();
        for artist in &window {
            self.fetch_art(artist, cx);
        }
    }

    fn render_card(
        &self,
        entity: &Entity<Self>,
        card: &Card,
        tile: f32,
        focused: bool,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let art = self.art_paths.get(card.id.as_ref()).cloned();
        let id = card.id.clone();
        let view = entity.clone();
        let card_el = v_flex()
            .id(card.id.clone())
            .w(px(tile + 12.))
            .p_1p5()
            .gap_1p5()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .active(|s| s.opacity(0.8))
            .when(focused, |s| {
                s.border_1()
                    .border_color(cx.theme().primary)
                    .shadow(focus_glow(cx))
            })
            .on_click(move |_, _, cx: &mut gpui::App| {
                let id = id.clone();
                view.update(cx, |_, cx| {
                    cx.emit(ArtistsEvent::OpenArtist(id.to_string()))
                });
            })
            .child(
                // Round, unlike the album grid's square tiles — the shape is
                // what tells the two pages apart at a glance.
                div()
                    .size(px(tile))
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .shadow_sm()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|this| match art {
                        Some(path) => this.child(img(path).size(px(tile)).rounded_full()),
                        None => this.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .text_size(px(tile * 0.34))
                                .child(card.initial.clone()),
                        ),
                    }),
            )
            .child(
                v_flex()
                    .h(px(TEXT_BLOCK_H))
                    .w_full()
                    .gap_0()
                    .items_center()
                    .text_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .max_h(px(NAME_LINE_H * 2.))
                            .overflow_hidden()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .child(card.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .child(card.albums.clone()),
                    ),
            );
        let card_el = if focused {
            with_focus_animation(card.id.clone(), card_el, cx).into_any_element()
        } else {
            card_el.into_any_element()
        };
        card_el
    }
}

impl Render for ArtistsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pick up cover-size changes: refetch art at the new resolution.
        let cover = self.session.read(cx).settings.cover_size;
        let tile = cover.px();
        if cover.art_px() != self.art_px {
            self.art_px = cover.art_px();
            self.refetch_art();
        }

        // The connect is part of the wait: until it lands there is no client to
        // fetch with, so `loading` is false while the grid is still stale.
        let connecting = self.session.read(cx).status == ConnectionStatus::Connecting;
        let loading = self.loading || connecting;
        let showing_cache = self.cached && loading;

        // Columns from this frame's window width; falls back to a guess on the
        // very first frame (before anything is laid out), then self-corrects.
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let width = self.live_width.resolve(measured, window);
        let cols = crate::ui::grid_columns(width, tile).unwrap_or(FALLBACK_COLS);
        let row_count = self.cards.len().div_ceil(cols);
        self.ensure_art_for_viewport(row_count, cols, cx);

        let entity = cx.entity();
        // Virtualized over rows, like the album grid: only what's on screen is
        // built and uploaded.
        let grid = uniform_list("artists-grid", row_count, move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|row| {
                    let start = row * cols;
                    let end = ((row + 1) * cols).min(view.cards.len());
                    let cards: Vec<_> = view.cards[start..end]
                        .iter()
                        .enumerate()
                        .map(|(j, card)| {
                            let card_index = start + j;
                            let focused = view.vi_cursor == Some(card_index);
                            view.render_card(&entity, card, tile, focused, cx)
                        })
                        .collect();
                    h_flex()
                        .w_full()
                        .gap_4()
                        .justify_center()
                        .pb_3()
                        .children(cards)
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .px_4()
        .track_scroll(self.scroll.clone());

        v_flex()
            .id("artists-scroll")
            .size_full()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .child(div().text_lg().child("Artists"))
                    .when(loading, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .child(Spinner::new().xsmall().color(cx.theme().muted_foreground))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if showing_cache {
                                            "Updating from server…"
                                        } else {
                                            "Loading…"
                                        }),
                                ),
                        )
                    })
                    .child(div().flex_1())
                    // Nothing to summarise until a sync has written rows —
                    // zeros next to a grid full of live cards read as a bug.
                    .when(self.stats.albums > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::ui::library_summary(
                                    (self.stats.artists, "artist"),
                                    &self.stats,
                                )),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    div()
                        .px_4()
                        .text_color(cx.theme().danger)
                        .text_sm()
                        .child(e),
                )
            })
            .child(grid)
    }
}

impl ArtistsView {
    /// Columns at the current window width; falls back to a guess on the very
    /// first frame (before anything is laid out), then self-corrects.
    fn grid_cols(&mut self, window: &Window, cx: &App) -> usize {
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let width = self.live_width.resolve(measured, window);
        let tile = self.session.read(cx).settings.cover_size.px();
        crate::ui::grid_columns(width, tile).unwrap_or(FALLBACK_COLS)
    }

    /// Move the vi-mode cursor by `delta` cards, clamping and scrolling the
    /// focused card into view.
    pub fn vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.cards.len();
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
        let cols = self.grid_cols(window, cx).max(1);
        self.scroll
            .scroll_to_item(next / cols, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Open the artist under the vi-mode cursor.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(card) = self.vi_cursor.and_then(|c| self.cards.get(c)) else {
            return;
        };
        cx.emit(ArtistsEvent::OpenArtist(card.id.to_string()));
    }
}

/// One artist's albums.
pub struct ArtistDetailView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    artist_id: String,
    artist: Option<ArtistWithAlbums>,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight album-cover downloads, cancelled when the view is dropped.
    art_tasks: Vec<gpui::Task<()>>,
    /// A coalesced repaint is scheduled (batches cover arrivals).
    art_repaint_pending: bool,
    artist_image_path: Option<PathBuf>,
    error: Option<String>,
    /// Biography + image URLs from getArtistInfo2 (Navidrome's agents).
    info: Option<ArtistInfo2>,
    /// An artist-image fetch has started; stops info2's fallback from
    /// racing/overwriting the primary coverArt fetch.
    image_requested: bool,
    /// Long bios render clamped to a few lines until expanded.
    bio_expanded: bool,
}

pub enum ArtistDetailEvent {
    OpenAlbum(String),
}

impl EventEmitter<ArtistDetailEvent> for ArtistDetailView {}

impl ArtistDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        artist_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session,
            player,
            artist_id,
            artist: None,
            art_paths: HashMap::new(),
            art_tasks: Vec::new(),
            art_repaint_pending: false,
            artist_image_path: None,
            error: None,
            info: None,
            image_requested: false,
            bio_expanded: false,
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
        let id = self.artist_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_artist(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(artist) => {
                        let artist_id = artist.artist.id.clone();
                        for album in &artist.album {
                            view.fetch_art(album.id.clone(), album.cover_art.clone(), cx);
                        }
                        let cover = artist.artist.cover_art.clone();
                        view.artist = Some(artist);
                        view.fetch_artist_image(cover, cx);
                        view.fetch_artist_info(&artist_id, cx);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetch an album's songs and start playing them.
    fn play_album(&mut self, album_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let player = self.player.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album(&album_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            match result {
                Ok(album) => {
                    let _ = player.update(cx, |p, cx| p.play_queue(album.song, 0, cx));
                }
                Err(e) => {
                    let _ = this.update(cx, |view, cx| {
                        view.error = Some(format!("{e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn fetch_art(&mut self, album_id: String, cover_art: Option<String>, cx: &mut Context<Self>) {
        let Some(cover_id) = cover_art else { return };
        // Draw whatever is already on disk right now, at whatever size it was
        // cached — the albums grid usually holds this very cover, at a rung
        // that depends on the cover-size setting rather than matching this
        // view's. Rendering it instantly is the difference between a page of
        // covers and a page of empty squares.
        if let Some(path) = artwork::cached_best(&cover_id, ART_SIZE) {
            self.art_paths.insert(album_id.clone(), path);
        }
        // Only the exact size ends the job; anything else is a stand-in that
        // still needs the real one fetched behind it.
        if artwork::cached(&cover_id, ART_SIZE).is_some() {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_paths.insert(album_id, path);
                    view.schedule_art_repaint(cx);
                });
            }
        });
        self.art_tasks.push(task);
    }

    /// Coalesce cover-arrival repaints into ~one re-render per burst.
    fn schedule_art_repaint(&mut self, cx: &mut Context<Self>) {
        if self.art_repaint_pending {
            return;
        }
        self.art_repaint_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.art_repaint_pending = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_artist_image(&mut self, source: Option<String>, cx: &mut Context<Self>) {
        let Some(source) = source else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        self.image_requested = true;
        let is_remote = source.starts_with("http://") || source.starts_with("https://");
        // Synchronous cache hit: no empty-frame flash on revisit.
        if !is_remote && let Some(path) = artwork::cached(&source, ART_SIZE) {
            self.artist_image_path = Some(path);
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if is_remote {
                    download_remote_image(&source).await
                } else {
                    artwork::fetch(client, source, ART_SIZE).await
                }
            })
            .await;
            if let Ok(path) = result {
                let _ = this.update(cx, |view, cx| {
                    view.artist_image_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Biography and artist image from Navidrome (getArtistInfo2). Falls back
    /// to the artist's own image fields when info2 has no usable image.
    fn fetch_artist_info(&self, artist_id: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let artist_id = artist_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_artist_info2(&artist_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                let info = result.unwrap_or_default();
                // The primary image (artist coverArt) started in load();
                // info2 URLs are only a fallback for artists without one.
                if !view.image_requested {
                    let image = info.image_url().map(str::to_string).or_else(|| {
                        view.artist
                            .as_ref()
                            .and_then(|a| a.artist.artist_image_url.clone())
                    });
                    view.fetch_artist_image(image, cx);
                }
                view.info = Some(info);
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for ArtistDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .artist
            .as_ref()
            .map(|a| a.artist.name.clone())
            .unwrap_or_else(|| "…".into());
        let bio = self
            .info
            .as_ref()
            .and_then(|i| i.biography.as_deref())
            .map(strip_html)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "No biography is available for this artist yet.".into());
        // Collapse long bios by truncating the string itself: gpui's
        // line_clamp lets the last line run past the container and its text
        // measurement cache ignores clamp changes, so it can't do this job.
        let bio_long = bio.chars().count() > BIO_PREVIEW_CHARS;
        let bio_text = if self.bio_expanded || !bio_long {
            bio
        } else {
            truncate_at_word(&bio, BIO_PREVIEW_CHARS)
        };
        // External links from getArtistInfo2 (same sources as Navidrome's UI).
        let musicbrainz_url = self
            .info
            .as_ref()
            .and_then(|i| i.music_brainz_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://musicbrainz.org/artist/{id}"));
        let lastfm_url = self
            .info
            .as_ref()
            .and_then(|i| i.last_fm_url.as_deref())
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        let genres = self.artist.as_ref().map(|a| {
            let mut seen = std::collections::HashSet::new();
            a.album
                .iter()
                .filter_map(|album| album.genre.as_deref())
                .map(str::trim)
                .filter(|g| !g.is_empty() && seen.insert(g.to_lowercase()))
                .map(str::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        });
        let genres_line = genres
            .filter(|g| !g.is_empty())
            .map(|g| format!("Genres: {g}"));
        let hero_art = self.artist_image_path.clone();

        let mut album_cards: Vec<gpui::AnyElement> = Vec::new();
        let mut single_cards: Vec<gpui::AnyElement> = Vec::new();
        if let Some(artist) = self.artist.as_ref() {
            for (index, album) in artist.album.iter().enumerate() {
                let id = album.id.clone();
                let play_id = album.id.clone();
                let art = self.art_paths.get(&album.id).cloned();
                let year = album.year.map(|y| y.to_string()).unwrap_or_default();
                let card = v_flex()
                    .id(gpui::SharedString::from(format!("aalbum-{}", album.id)))
                    .group("aacard")
                    .w(px(172.))
                    .p_1p5()
                    .gap_1p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .active(|s| s.opacity(0.8))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ArtistDetailEvent::OpenAlbum(id.clone()));
                    }))
                    .child(
                        div()
                            .size(px(160.))
                            .rounded_lg()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .shadow_sm()
                            .relative()
                            .when_some(art, |this, path| {
                                this.child(img(path).size(px(160.)).rounded_lg())
                            })
                            // Hover play button over the artwork, same as the
                            // album grid's cards.
                            .child(
                                div()
                                    .absolute()
                                    .bottom_2()
                                    .right_2()
                                    .opacity(0.)
                                    .group_hover("aacard", |s| s.opacity(1.))
                                    .child(
                                        Button::new(("artist-album-play", index))
                                            .primary()
                                            .icon(app_icon(icons::PLAY))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.play_album(play_id.clone(), cx);
                                                cx.stop_propagation();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            // Explicit line heights: the default line box clips
                            // descenders (y, g, j) inside truncated text.
                            .child(
                                div()
                                    .text_sm()
                                    .line_height(px(20.))
                                    .truncate()
                                    .child(album.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .line_height(px(17.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(year),
                            ),
                    )
                    .into_any_element();
                if is_single_or_ep(album) {
                    single_cards.push(card);
                } else {
                    album_cards.push(card);
                }
            }
        }

        let make_section = |title: String, cards: Vec<gpui::AnyElement>| {
            let title_text = title.clone();
            let mut section = v_flex()
                .gap_2()
                .child(div().text_sm().font_medium().child(title_text.clone()));
            if cards.is_empty() {
                section = section.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("No {} yet.", title_text.to_lowercase())),
                );
            } else {
                section = section.child(h_flex().flex_wrap().gap_4().children(cards));
            }
            section.into_any_element()
        };

        v_flex()
            .id("artist-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    .bg(cx.theme().sidebar)
                    .child(
                        h_flex()
                            .items_start()
                            .gap_4()
                            .flex_wrap()
                            .child(
                                div()
                                    .size(px(220.))
                                    .rounded_2xl()
                                    .overflow_hidden()
                                    .bg(cx.theme().muted)
                                    .when_some(hero_art, |this, path| {
                                        this.child(img(path).size(px(220.)).rounded_2xl())
                                    }),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(260.))
                                    .gap_2()
                                    .child(div().text_2xl().font_medium().child(name))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Bio"),
                                    )
                                    .child(div().text_sm().child(bio_text))
                                    .when(bio_long, |this| {
                                        let expanded = self.bio_expanded;
                                        this.child(
                                            h_flex().child(
                                                Button::new("bio-toggle")
                                                    .ghost()
                                                    .xsmall()
                                                    .label(if expanded { "Less" } else { "More" })
                                                    .icon(Icon::new(if expanded {
                                                        IconName::ChevronUp
                                                    } else {
                                                        IconName::ChevronDown
                                                    }))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.bio_expanded = !this.bio_expanded;
                                                        cx.notify();
                                                    })),
                                            ),
                                        )
                                    })
                                    .when_some(genres_line, |this, desc| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(desc),
                                        )
                                    })
                                    .when(
                                        musicbrainz_url.is_some() || lastfm_url.is_some(),
                                        |this| {
                                            this.child(
                                                h_flex()
                                                    .gap_3()
                                                    .text_sm()
                                                    .when_some(musicbrainz_url, |this, url| {
                                                        this.child(
                                                            Link::new("mb-link")
                                                                .href(url)
                                                                .child("MusicBrainz"),
                                                        )
                                                    })
                                                    .when_some(lastfm_url, |this, url| {
                                                        this.child(
                                                            Link::new("lastfm-link")
                                                                .href(url)
                                                                .child("Last.fm"),
                                                        )
                                                    }),
                                            )
                                        },
                                    ),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(make_section("Albums".to_string(), album_cards))
            .child(make_section("Singles / EPs".to_string(), single_cards))
    }
}

/// Collapsed-bio length; roughly four lines at typical window widths.
const BIO_PREVIEW_CHARS: usize = 400;

fn is_single_or_ep(album: &Album) -> bool {
    let name = album.name.to_lowercase();
    let song_count = album.song_count.unwrap_or_default();
    name.contains("single") || name.contains("ep") || song_count <= 4
}

async fn download_remote_image(url: &str) -> anyhow::Result<PathBuf> {
    let dir = crate::config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{}.img", crate::config::sanitize(url), ART_SIZE));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    // Temp file + rename so a partial download never poisons the cache.
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::is_single_or_ep;
    use subsonic::Album;

    #[test]
    fn singles_and_eps_are_grouped_separately() {
        let single = Album {
            id: "1".into(),
            name: "Single".into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: Some(2),
            duration: None,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
        };
        let album = Album {
            id: "2".into(),
            name: "Studio Album".into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: Some(10),
            duration: None,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
        };
        assert!(is_single_or_ep(&single));
        assert!(!is_single_or_ep(&album));
    }
}

#[cfg(test)]
mod grid_tests {
    use super::*;

    fn artist(name: &str) -> subsonic::Artist {
        subsonic::Artist {
            id: name.into(),
            name: name.into(),
            cover_art: None,
            album_count: None,
            artist_image_url: None,
            biography: None,
            starred: None,
        }
    }

    #[test]
    fn index_buckets_flatten_into_one_alphabetical_list() {
        let index = vec![
            ArtistIndex {
                name: "B".into(),
                artist: vec![artist("Burial"), artist("Boards")],
            },
            ArtistIndex {
                name: "A".into(),
                artist: vec![artist("aphex")],
            },
        ];
        let names: Vec<_> = flatten_index(index).into_iter().map(|a| a.name).collect();
        // Case-insensitive, and across buckets — the grid has no headings to
        // fall back on, so a bucket arriving out of order must still sort in.
        assert_eq!(names, ["aphex", "Boards", "Burial"]);
    }

    #[test]
    fn album_count_is_pluralized_and_omitted_when_unknown() {
        let mut one = artist("Aphex");
        one.album_count = Some(1);
        let mut many = artist("Autechre");
        many.album_count = Some(12);
        let cards = to_cards(&[one, many, artist("Unknown")]);
        assert_eq!(cards[0].albums, "1 album");
        assert_eq!(cards[1].albums, "12 albums");
        assert!(cards[2].albums.is_empty());
    }

    #[test]
    fn card_initial_is_uppercased_for_the_empty_circle() {
        let cards = to_cards(&[artist("aphex twin"), artist("65daysofstatic")]);
        assert_eq!(cards[0].initial, "A");
        assert_eq!(cards[1].initial, "6");
    }

    #[test]
    fn nameless_artist_yields_no_initial_instead_of_panicking() {
        let cards = to_cards(&[artist("")]);
        assert!(cards[0].initial.is_empty());
    }
}
