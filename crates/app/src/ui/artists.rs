//! Artist list (grouped by index letter) and artist detail (their albums).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle,
    SharedString, UniformListScrollHandle, Window, div, img, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::spinner::Spinner;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt, h_flex, v_flex,
};
use subsonic::{Album, ArtistIndex, ArtistInfo2, ArtistWithAlbums, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::album_info::{self, looks_truncated};
use crate::services::artist_info::{self, ArtistInfo};
use crate::services::library_db::{LibraryDb, LibraryStats};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::album_detail::{
    ABOUT_PROSE_MAX_W, AboutSource, ONLINE_WAIT, about_sources, ext_links, link_icon,
    waiting_online,
};
use crate::ui::albums::album_from_row;
use crate::ui::{
    card_inset, card_padding, strip_html, sync_focus_scroll, truncate_at_word, with_focus_cursor,
};

const ART_SIZE: u32 = 320;
/// Resolution the hero image is re-fetched at for the lightbox, matching the
/// album page's full-size cover.
const FULL_ART_SIZE: u32 = 1500;

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

/// Cover size for an artist page's album cards: its own setting, or the album
/// grid's when that setting is `Match`.
fn album_cover(session: &Entity<Session>, cx: &App) -> crate::config::CoverSize {
    let settings = &session.read(cx).settings;
    settings.artist_album_size.resolve(settings.cover_size)
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
    error: Option<crate::errors::ErrorNote>,
    /// Card index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Catalog totals shown in the header, for the selected libraries.
    stats: LibraryStats,
    /// Tracks the grid's width against the window's, so the column count
    /// follows a resize on the same frame instead of one behind it.
    live_width: crate::ui::LiveWidth,
    /// Per-artist accent colours for `Settings::selection_glow_album_color`
    /// (extracted from the artist photo here). `RefCell`: `render_card` only
    /// has `&self`/`&App`, called from `uniform_list`'s item closure.
    glow_accents: RefCell<HashMap<String, gpui::Hsla>>,
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
            glow_accents: RefCell::new(HashMap::new()),
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
                    Err(e) => view.error = Some(crate::errors::ErrorNote::new(&e)),
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
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        let accent = if self.session.read(cx).settings.selection_glow_album_color {
            art.as_ref().and_then(|p| {
                crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), id.as_ref(), p)
            })
        } else {
            None
        };
        let card_el = v_flex()
            .id(card.id.clone())
            .w(px(tile + crate::ui::card_padding()))
            .p_1p5()
            .gap_1p5()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, accent, cx)
                } else {
                    s
                }
            })
            .active(|s| s.opacity(0.8))
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
        with_focus_cursor(card.id.clone(), card_el, focused, glow, accent, cx)
    }
}

impl Render for ArtistsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pick up cover-size changes: refetch art at the new resolution. The
        // fetch resolution follows the *setting*, not the tile the window ends
        // up picking inside its range, so a resize never invalidates art.
        let cover = self.session.read(cx).settings.cover_size;
        let (min_tile, max_tile) = cover.range();
        if cover.art_px() != self.art_px {
            self.art_px = cover.art_px();
            self.refetch_art();
        }

        // The connect is part of the wait: until it lands there is no client to
        // fetch with, so `loading` is false while the grid is still stale.
        let connecting = self.session.read(cx).status == ConnectionStatus::Connecting;
        let loading = self.loading || connecting;
        let showing_cache = self.cached && loading;

        // Columns *and* the tile they're drawn at, from this frame's window
        // width: the covers grow inside the setting's range to spend what would
        // otherwise be left as gutters. Falls back to a guess on the very first
        // frame (before anything is laid out), then self-corrects.
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let (cols, tile) =
            self.live_width
                .grid(measured, min_tile, max_tile, window, FALLBACK_COLS);
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
                    .flex_wrap()
                    .gap_4()
                    .gap_y_1()
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
                    // Nothing to summarise until a sync has written rows —
                    // zeros next to a grid full of live cards read as a bug.
                    .when(self.stats.albums > 0, |this| {
                        this.child(
                            div()
                                .ml_auto()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::ui::library_summary(
                                    (self.stats.artists, "artist"),
                                    &self.stats,
                                )),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, note| {
                this.child(crate::ui::error_banner(
                    &note,
                    cx.listener(|view, _, _, cx| {
                        view.error = None;
                        view.load(cx);
                    }),
                    cx,
                ))
            })
            .child(grid)
    }
}

impl ArtistsView {
    /// Columns at the current window width; falls back to a guess on the very
    /// first frame (before anything is laid out), then self-corrects.
    fn grid_cols(&mut self, window: &Window, cx: &App) -> usize {
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let (min_tile, max_tile) = self.session.read(cx).settings.cover_size.range();
        self.live_width
            .grid(measured, min_tile, max_tile, window, FALLBACK_COLS)
            .0
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
    /// The synced catalog: where the albums' library provenance and the
    /// artist's guest appearances come from — neither is in `getArtist`.
    library_db: Arc<LibraryDb>,
    artist_id: String,
    artist: Option<ArtistWithAlbums>,
    /// Albums the artist only plays on, read from the cache.
    appears_on: Vec<(Album, i64)>,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight album-cover downloads, cancelled when the view is dropped.
    art_tasks: Vec<gpui::Task<()>>,
    /// A coalesced repaint is scheduled (batches cover arrivals).
    art_repaint_pending: bool,
    artist_image_path: Option<PathBuf>,
    /// Cover id (or remote URL) the hero image came from, so the lightbox can
    /// ask for it again at full resolution.
    artist_image_source: Option<String>,
    /// The hero image is open full-window.
    show_full_art: bool,
    full_art_path: Option<PathBuf>,
    error: Option<crate::errors::ErrorNote>,
    /// Biography + image URLs from getArtistInfo2 (Navidrome's agents).
    info: Option<ArtistInfo2>,
    /// An artist-image fetch has started; stops info2's fallback from
    /// racing/overwriting the primary coverArt fetch.
    image_requested: bool,
    /// A long bio shows a preview on the page; the whole text opens in a
    /// popup rather than growing the header card down the page.
    bio_open: bool,
    /// The bio popup's open/close travel.
    bio_reveal: crate::ui::Reveal,
    /// The bio popup's own scroll, reset each time it opens.
    bio_scroll: ScrollHandle,
    /// Content-area height the bio popup was last capped at: the area is
    /// measured a frame late, so a change asks for one more frame.
    bio_popup_area_h: f32,
    /// What `services::artist_info` found outside the server (Wikipedia,
    /// MusicBrainz), once it has answered.
    online: Option<ArtistInfo>,
    /// Which services the online lookup for this page asked, once it has
    /// started — a service switched on later asks again.
    online_asked: Option<album_info::Sources>,
    /// The online lookup is in flight, since when (the bio holds for it up
    /// to `ONLINE_WAIT`).
    online_since: Option<Instant>,
    /// The bio source the user picked with the pills.
    about_pick: Option<AboutSource>,
    /// This frame's content width, for the bio column's explicit width.
    live_width: crate::ui::LiveWidth,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Album id per flattened discography card (album cards then singles/EPs),
    /// rebuilt each render to map the vi cursor index onto a card.
    discography_ids: Vec<String>,
    /// Whether the bio "Read more" button is present and can take the cursor.
    bio_toggle_focusable: bool,
    /// Discography card / bio toggle index under the vi-mode cursor.
    vi_cursor: Option<usize>,
    /// Cursor position the scroll has caught up to, so `render` scrolls only
    /// when the cursor actually moved (`ui::sync_focus_scroll`).
    vi_scroll_synced: Option<usize>,
    /// Per-album accent colours for `Settings::selection_glow_album_color`.
    glow_accents: RefCell<HashMap<String, gpui::Hsla>>,
    /// Resolution the discography covers are currently fetched at, so a
    /// cover-size change is noticed in `render` and refetched once.
    album_art_px: u32,
}

pub enum ArtistDetailEvent {
    OpenAlbum(String),
}

impl EventEmitter<ArtistDetailEvent> for ArtistDetailView {}

impl ArtistDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        library_db: Arc<LibraryDb>,
        artist_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroll = ScrollHandle::new();
        let album_art_px = album_cover(&session, cx).wrap_art_px();
        let mut this = Self {
            session,
            player,
            library_db,
            artist_id,
            artist: None,
            appears_on: Vec::new(),
            art_paths: HashMap::new(),
            art_tasks: Vec::new(),
            art_repaint_pending: false,
            artist_image_path: None,
            artist_image_source: None,
            show_full_art: false,
            full_art_path: None,
            error: None,
            info: None,
            image_requested: false,
            bio_open: false,
            bio_reveal: crate::ui::Reveal::new(170, 120),
            bio_scroll: ScrollHandle::new(),
            bio_popup_area_h: 0.,
            online: None,
            online_asked: None,
            online_since: None,
            about_pick: None,
            live_width: crate::ui::LiveWidth::default(),
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            discography_ids: Vec::new(),
            bio_toggle_focusable: false,
            vi_cursor: None,
            vi_scroll_synced: None,
            glow_accents: RefCell::new(HashMap::new()),
            album_art_px,
        };
        this.load_appears_on(cx);
        this.load(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// Pick up a cover-size change: refetch the discography art at the new
    /// resolution. Compared at `artwork::bucket`'s rung, like the album grid —
    /// two sizes sharing a rung name the very same cache entry, so dropping the
    /// paths would look up the identical files again.
    ///
    /// The paths are kept rather than cleared: `fetch_art` puts whatever is
    /// already on disk up first (`cached_best`), so a page that has to grow its
    /// covers scales the ones it has instead of blanking.
    fn refetch_art(&mut self, art_px: u32, cx: &mut Context<Self>) {
        self.album_art_px = art_px;
        let albums: Vec<_> = self
            .artist
            .iter()
            .flat_map(|a| a.album.iter())
            .chain(self.appears_on.iter().map(|(album, _)| album))
            .map(|album| (album.id.clone(), album.cover_art.clone()))
            .collect();
        for (id, cover) in albums {
            self.fetch_art(id, cover, cx);
        }
    }

    /// Re-apply the library selection to the page in place.
    ///
    /// Both sections that depend on it are rebuilt: "Appears on" is filtered
    /// out of the cache and is up immediately, while the discography needs the
    /// `getArtist` round trip again — `keep_selected_libraries` can narrow the
    /// list it already has but never widen it, so the old albums stay on screen
    /// until the fetch lands rather than being dropped and re-added.
    pub fn reload_libraries(&mut self, cx: &mut Context<Self>) {
        self.load_appears_on(cx);
        self.load(cx);
        cx.notify();
    }

    /// The artist's id as the sync namespaces it in the cache.
    fn cache_artist_id(&self) -> String {
        if self.artist_id.starts_with("navidrome:artist:") {
            self.artist_id.clone()
        } else {
            format!("navidrome:artist:{}", self.artist_id)
        }
    }

    /// Albums the artist plays on without being credited with them. Cache-only:
    /// no Subsonic endpoint answers this, and reading it here means the section
    /// is up on the first frame rather than after `getArtist` lands.
    fn load_appears_on(&mut self, cx: &mut Context<Self>) {
        let libraries = self.session.read(cx).library_ids.clone();
        let Ok(rows) = self
            .library_db
            .appears_on("navidrome", &self.cache_artist_id())
        else {
            return;
        };
        self.appears_on = rows
            .into_iter()
            .filter(|(row, _)| in_libraries(row.library_id.as_deref(), &libraries))
            .map(|(row, tracks)| (album_from_row(row), tracks))
            .collect();
        let art: Vec<_> = self
            .appears_on
            .iter()
            .map(|(album, _)| (album.id.clone(), album.cover_art.clone()))
            .collect();
        for (id, cover) in art {
            self.fetch_art(id, cover, cx);
        }
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
                    Ok(mut artist) => {
                        view.keep_selected_libraries(&mut artist.album, cx);
                        sort_discography(&mut artist.album);
                        let artist_id = artist.artist.id.clone();
                        for album in &artist.album {
                            view.fetch_art(album.id.clone(), album.cover_art.clone(), cx);
                        }
                        let cover = artist.artist.cover_art.clone();
                        view.artist = Some(artist);
                        view.fetch_artist_image(cover, cx);
                        view.fetch_artist_info(&artist_id, cx);
                    }
                    Err(e) => view.error = Some(crate::errors::ErrorNote::new(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop the albums that belong to a library the user isn't browsing.
    ///
    /// `getArtist` takes no `musicFolderId`, so the server answers with the
    /// artist's whole discography however narrow the selection is; the sync's
    /// recorded provenance is the only thing that can narrow it. An album the
    /// cache has never seen is kept rather than hidden — a cold cache must not
    /// empty the page.
    fn keep_selected_libraries(&self, albums: &mut Vec<Album>, cx: &Context<Self>) {
        let libraries = self.session.read(cx).library_ids.clone();
        if libraries.is_empty() {
            return;
        }
        let Ok(provenance) = self.library_db.album_libraries("navidrome") else {
            return;
        };
        albums.retain(|album| {
            match provenance.get(&format!("navidrome:album:{}", album.id)) {
                Some(library) => in_libraries(library.as_deref(), &libraries),
                // Never synced: nothing to judge it by, so leave it alone.
                None => true,
            }
        });
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
                        view.error = Some(crate::errors::ErrorNote::new(&e));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn fetch_art(&mut self, album_id: String, cover_art: Option<String>, cx: &mut Context<Self>) {
        let Some(cover_id) = cover_art else { return };
        let art_px = self.album_art_px;
        // Draw whatever is already on disk right now, at whatever size it was
        // cached — the albums grid usually holds this very cover, at a rung
        // that depends on the cover-size setting rather than matching this
        // view's. Rendering it instantly is the difference between a page of
        // covers and a page of empty squares.
        if let Some(path) = artwork::cached_best(&cover_id, art_px) {
            self.art_paths.insert(album_id.clone(), path);
        }
        // Only the exact size ends the job; anything else is a stand-in that
        // still needs the real one fetched behind it.
        if artwork::cached(&cover_id, art_px).is_some() {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, art_px).await {
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
        self.artist_image_source = Some(source.clone());
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

    /// Open the hero image full-window, fetching it at full resolution behind
    /// the thumbnail already on screen — same as the album page's cover.
    ///
    /// A remote photo (info2's URL) is already cached at whatever the source
    /// published, so only a server-hosted cover has a larger version to ask for.
    fn open_full_art(&mut self, cx: &mut Context<Self>) {
        if self.artist_image_path.is_none() {
            return;
        }
        self.show_full_art = true;
        if self.full_art_path.is_none()
            && let Some(source) = self.artist_image_source.clone()
            && !source.starts_with("http")
            && let Some(client) = self.client(cx)
        {
            cx.spawn(async move |this, cx| {
                if let Ok(path) = artwork::fetch(client, source, FULL_ART_SIZE).await {
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
                view.maybe_load_online(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Ask MusicBrainz / Wikipedia about the artist, once per page, behind
    /// `Settings::artist_info_wikipedia`/`artist_info_musicbrainz`.
    ///
    /// Waits for `getArtistInfo2`, since its MusicBrainz id is the one lookup
    /// that cannot pick the wrong artist, and for `getArtist`'s album titles,
    /// which tell two artists of one name apart.
    fn maybe_load_online(&mut self, cx: &mut Context<Self>) {
        let sources = artist_sources(&self.session.read(cx).settings);
        if self.info.is_none()
            || !sources.any()
            || self.online_asked.is_some_and(|asked| asked.covers(sources))
        {
            return;
        }
        let Some(artist) = self.artist.as_ref() else {
            return;
        };
        // Full albums first: a single's title is the song's, and a search by
        // it finds every cover version.
        let (albums, singles): (Vec<&Album>, Vec<&Album>) =
            artist.album.iter().partition(|a| !is_single_or_ep(a));
        let query = artist_info::Query {
            name: artist.artist.name.clone(),
            mbid: self
                .info
                .as_ref()
                .and_then(|i| i.music_brainz_id.as_deref())
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
            albums: albums
                .into_iter()
                .chain(singles)
                .map(|a| a.name.clone())
                .collect(),
            sources,
        };
        self.online_asked = Some(sources);
        self.online_since = Some(Instant::now());
        // The clock running out has to repaint, or the server's bio only
        // appears at the next unrelated notify.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ONLINE_WAIT).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(artist_info::fetch(query)).await;
            let _ = this.update(cx, |view, cx| {
                // Past the wait the server's bio is already up; swapping it
                // for Wikipedia's under somebody reading it is worse than the
                // pill that offers the switch.
                if !waiting_online(view.online_since) && view.about_pick.is_none() {
                    view.about_pick = view.server_bio().map(|_| AboutSource::Server);
                }
                view.online_since = None;
                match result {
                    Ok(info) => view.online = info,
                    Err(e) => tracing::warn!("artist info lookup failed: {e:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The server's biography, cleaned (see [`clean_server_bio`]).
    fn server_bio(&self) -> Option<(String, bool)> {
        self.info
            .as_ref()
            .and_then(|i| i.biography.as_deref())
            .and_then(clean_server_bio)
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        // With the popup up, j/k read through the bio instead of walking the
        // cards hidden behind it.
        if self.bio_open {
            let offset = self.bio_scroll.offset();
            let max = self.bio_scroll.max_offset().height;
            let y = (offset.y - px(BIO_POPUP_STEP * delta as f32)).clamp(-max, px(0.));
            self.bio_scroll.set_offset(gpui::point(offset.x, y));
            cx.notify();
            return;
        }
        let bio = self.bio_toggle_focusable as usize;
        let count = self.discography_ids.len() + bio;
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
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Enter on a focused discography card opens the album; on the bio's
    /// "Read more" it opens the whole biography, and closes it again.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        if self.bio_open {
            self.close_bio(cx);
            return;
        }
        let Some(i) = self.vi_cursor else {
            return;
        };
        if let Some(id) = self.discography_ids.get(i) {
            cx.emit(ArtistDetailEvent::OpenAlbum(id.clone()));
        } else if self.bio_toggle_focusable {
            self.open_bio(cx);
        }
    }

    fn open_bio(&mut self, cx: &mut Context<Self>) {
        self.bio_open = true;
        self.bio_scroll.set_offset(gpui::point(px(0.), px(0.)));
        cx.notify();
    }

    /// Close the bio popup; whether it was open (Escape asks, and only stops
    /// there when it was).
    pub fn close_bio(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::take(&mut self.bio_open);
        if was {
            cx.notify();
        }
        was
    }

    /// The whole biography over the page: a card sized to the content area
    /// with its own scroll, dismissed by the close button, a click on the
    /// backdrop, Escape, or Enter in vi mode. On its way out the popup no
    /// longer takes clicks.
    fn render_bio_popup(
        &mut self,
        text: String,
        source: Option<&'static str>,
        read_more: Option<String>,
        content_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let active = self.bio_open;
        let t = self
            .bio_reveal
            .openness(self.session.read(cx).settings.reduced_motion);
        // Width from this frame's viewport (`LiveWidth`); the height only
        // exists as last paint's bounds, so when it moves the next frame is
        // asked for, or a window shrunk once kept the old cap.
        let measured_h = f32::from(self.scroll.bounds().size.height);
        let area_h = if measured_h > 0. {
            measured_h
        } else {
            f32::from(window.viewport_size().height)
        };
        if area_h != self.bio_popup_area_h {
            self.bio_popup_area_h = area_h;
            window.request_animation_frame();
        }
        let width = bio_popup_width(content_w);
        let max_h = (area_h - 2. * BIO_POPUP_MARGIN).max(0.).floor();
        let name = self
            .artist
            .as_ref()
            .map(|a| a.artist.name.clone())
            .unwrap_or_default();
        div()
            .id("artist-bio-popup")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            // Half of it, the container being centred: the card rises into
            // place while the backdrop dims behind it.
            .pt(px(28. * (1. - t)))
            .occlude()
            .bg(gpui::hsla(0., 0., 0., 0.6 * t))
            .when(active, |this| {
                this.on_click(cx.listener(|this, _, _, cx| {
                    this.close_bio(cx);
                }))
            })
            .child(
                v_flex()
                    .id("artist-bio-card")
                    .w(px(width))
                    .max_h(px(max_h))
                    .overflow_hidden()
                    // Its own hitbox over the backdrop's, so a click on the
                    // text does not count as one outside it.
                    .occlude()
                    .opacity(t)
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .shadow_xl()
                    .child(
                        h_flex()
                            .flex_none()
                            .items_start()
                            .justify_between()
                            .gap_2()
                            .px_5()
                            .pt_4()
                            .pb_2()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(div().text_lg().font_semibold().child(name))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(match source {
                                                Some(source) => format!("Bio · {source}"),
                                                None => "Bio".to_string(),
                                            }),
                                    ),
                            )
                            .child(
                                Button::new("artist-bio-close")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(IconName::Close))
                                    .tooltip("Close")
                                    .when(active, |b| {
                                        b.on_click(cx.listener(|this, _, _, cx| {
                                            this.close_bio(cx);
                                        }))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("artist-bio-text")
                            // Grows to the text and gives way to the card's
                            // cap: `flex_1`'s zero basis would measure the
                            // prose at min-content width.
                            .flex_grow()
                            .flex_shrink()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.bio_scroll)
                            .px_5()
                            .pb_5()
                            .child(bio_body(Some(text), read_more, None, cx)),
                    ),
            )
            .into_any_element()
    }

    /// One discography card, focus-ringed and scroll-anchored when the vi
    /// cursor is on it.
    fn render_album_card(
        &self,
        album: &Album,
        // Index of this card across every section, unique per page: the vi
        // cursor's target, and with it the play button's element id.
        flat: usize,
        focused: bool,
        subtitle: Option<String>,
        // Cover edge, from `Settings::artist_album_size`. The card is that plus
        // its own padding, so the text column stays as wide as the art.
        tile: f32,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        // The discography is sorted by year and says so under each cover; an
        // "Appears on" card is about someone else's album, so it names them
        // instead.
        let year =
            subtitle.unwrap_or_else(|| album.year.map(|y| y.to_string()).unwrap_or_default());
        let anchor = self.focus_anchor.clone();
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        let accent = if self.session.read(cx).settings.selection_glow_album_color {
            art.as_ref().and_then(|p| {
                crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), &album.id, p)
            })
        } else {
            None
        };
        // The album grid's setting, applied to the album cards here too —
        // these are the same card at a different size.
        let flush = !self.session.read(cx).settings.classic_album_cards;
        let cover = crate::ui::card_cover_edge(tile, flush);
        let card = v_flex()
            .id(gpui::SharedString::from(format!("aalbum-{}", album.id)))
            .group("aacard")
            .w(px(tile + card_padding()))
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .map(|c| match flush {
                true => c,
                false => c.p(px(card_inset())),
            })
            .gap_1p5()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, accent, cx)
                } else {
                    s
                }
            })
            .active(|s| s.opacity(0.8))
            .when(focused, |s| s.anchor_scroll(Some(anchor)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ArtistDetailEvent::OpenAlbum(id.clone()));
            }))
            .child(
                div()
                    .size(px(cover))
                    .map(|c| crate::ui::cover_rounding(c, flush))
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .shadow_sm()
                    .relative()
                    .when_some(art, |this, path| {
                        this.child(crate::ui::cover_rounding(img(path).size(px(cover)), flush))
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
                                Button::new(("artist-album-play", flat))
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
                    // Flush, the card kept no padding for the text to sit in.
                    .map(|t| match flush {
                        true => t.px(px(card_inset())).pb(px(card_inset())),
                        false => t,
                    })
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
            );
        with_focus_cursor(
            format!("vi-artist-card-{flat}"),
            card,
            focused,
            glow,
            accent,
            cx,
        )
    }
}
/// Newest first, by the most precise date the server published.
///
/// `Album::release_key` prefers OpenSubsonic's `originalReleaseDate` over the
/// bare `year`, so two records from the same year are ordered by the month and
/// day the tags carry rather than falling to the alphabetical tie-break — which
/// on an artist who released four things in a year is an order that means
/// nothing. Vanilla servers publish only the year and get exactly the old
/// behaviour, since a yearless key sorts as `(year, 0, 0)`.
fn sort_discography(albums: &mut [Album]) {
    albums.sort_by(|a, b| {
        let key = |album: &Album| album.release_key().unwrap_or((i32::MIN, 0, 0));
        key(b)
            .cmp(&key(a))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

impl Render for ArtistDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scroll-into-view runs here, not in `vi_move`: the anchor's origin
        // is only as fresh as the last paint, and from a key handler that is
        // the row the cursor just LEFT — going up, the focused row landed one
        // row above the viewport and the highlight vanished.
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let name = self
            .artist
            .as_ref()
            .map(|a| a.artist.name.clone())
            .unwrap_or_else(|| "…".into());
        // Bio + external links: the server's `getArtistInfo2` and whatever
        // `services::artist_info` found online, picked between like the album
        // page's About card. Collapsed by truncating the string itself: gpui's
        // line_clamp lets the last line run past the container and its text
        // measurement cache ignores clamp changes, so it can't do this job.
        let settings = &self.session.read(cx).settings;
        let bios_on = settings.server_artist_bios;
        let want = artist_sources(settings);
        let online_on = want.any();
        if online_on {
            self.maybe_load_online(cx);
        }
        let server_bio = self.server_bio().filter(|_| bios_on);
        let online = self.online.as_ref().filter(|_| online_on);
        let wikipedia = online
            .and_then(|o| o.wikipedia.as_ref())
            .filter(|_| want.wikipedia);
        let annotation = online
            .and_then(|o| o.annotation.as_ref())
            .filter(|_| want.musicbrainz);
        let waiting = waiting_online(self.online_since);
        let (sources, shown) = about_sources(
            wikipedia.is_some(),
            server_bio.is_some(),
            annotation.is_some(),
            self.about_pick,
        );
        let lastfm_url = self
            .info
            .as_ref()
            .and_then(|i| i.last_fm_url.as_deref())
            .map(str::trim)
            .filter(|url| !url.is_empty() && bios_on)
            .map(str::to_string);
        let server_label = if lastfm_url.is_some() {
            "Last.fm"
        } else {
            "Server"
        };
        // The rest of a cut summary lives on Last.fm's own page.
        let read_more = match (shown, &server_bio) {
            (Some(AboutSource::Server), Some((_, true))) => lastfm_url.clone(),
            _ => None,
        };
        let bio: Option<String> = match shown {
            Some(AboutSource::Wikipedia) => wikipedia.map(|d| d.text.clone()),
            Some(AboutSource::MusicBrainz) => annotation.map(|d| d.text.clone()),
            Some(AboutSource::Server) => {
                server_bio.map(|(text, cut)| if cut { format!("{text} …") } else { text })
            }
            None => None,
        };
        // Still coming: `getArtistInfo2`, or the online lookup inside its wait
        // (or not started yet, since it needs both server answers first).
        let bio_loading = (bios_on || online_on)
            && (self.info.is_none()
                || waiting
                || (online_on && self.online.is_none() && self.online_asked.is_none()));
        let bio = bio.filter(|_| !bio_loading);
        let bio_long = bio
            .as_ref()
            .is_some_and(|b| b.chars().count() > BIO_PREVIEW_CHARS);
        // A long bio is previewed here and read whole in the popup, which is
        // also where a cut summary's Last.fm link goes.
        let popup_read_more = read_more.clone().filter(|_| bio_long);
        let read_more = read_more.filter(|_| !bio_long);
        let full_bio = bio.clone().filter(|_| bio_long);
        let bio_text: Option<String> = bio.map(|b| {
            if bio_long {
                truncate_at_word(&b, BIO_PREVIEW_CHARS)
            } else {
                b
            }
        });
        let shown_label = shown.map(|source| match source {
            AboutSource::Wikipedia => "Wikipedia",
            AboutSource::Server => server_label,
            AboutSource::MusicBrainz => "MusicBrainz",
        });
        let bio_placeholder = (bios_on || online_on).then_some(if bio_loading {
            "Looking up a biography…"
        } else {
            "No biography is available for this artist yet."
        });
        let musicbrainz_url = self
            .info
            .as_ref()
            .and_then(|i| i.music_brainz_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://musicbrainz.org/artist/{id}"));
        let online_links: Vec<album_info::Link> = online
            .map(|o| o.links.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|l| want.allows(l.kind))
            .cloned()
            .collect();
        let links = ext_links(musicbrainz_url, lastfm_url, &online_links);
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
        // The bio column carries an explicit width: left to flex, taffy
        // measures its height at a different width than it lays the prose out
        // at, and a Wikipedia intro spilled out over the rows under it.
        let content_w = self
            .live_width
            .resolve(f32::from(self.scroll.bounds().size.width), window);
        if content_w <= 0. {
            window.request_animation_frame();
        }
        let bio_w = bio_column_width(content_w);

        // Cover size for this page's cards, and a refetch when it moved. The
        // rung is what matters: two settings landing on the same one name the
        // same cache entry.
        let cover = album_cover(&self.session, cx);
        let tile = cover.wrap_tile();
        if artwork::bucket(cover.wrap_art_px()) != artwork::bucket(self.album_art_px) {
            self.refetch_art(cover.wrap_art_px(), cx);
        }

        // Build the discography in render order (album cards first, then
        // singles/EPs) and record each card's album id at its flat vi index.
        self.discography_ids.clear();
        let mut album_cards: Vec<gpui::AnyElement> = Vec::new();
        let mut single_cards: Vec<gpui::AnyElement> = Vec::new();
        if let Some(artist) = self.artist.as_ref() {
            for album in artist.album.iter() {
                if is_single_or_ep(album) {
                    continue;
                }
                let flat = self.discography_ids.len();
                self.discography_ids.push(album.id.clone());
                let focused = self.vi_cursor == Some(flat);
                album_cards.push(self.render_album_card(album, flat, focused, None, tile, cx));
            }
            for album in artist.album.iter() {
                if !is_single_or_ep(album) {
                    continue;
                }
                let flat = self.discography_ids.len();
                self.discography_ids.push(album.id.clone());
                let focused = self.vi_cursor == Some(flat);
                single_cards.push(self.render_album_card(album, flat, focused, None, tile, cx));
            }
        }
        // Guest appearances last: they are someone else's records, and the
        // cursor walks them after the artist's own. Anything the discography
        // above already shows is not one of them.
        let own = own_album_ids(self.artist.as_ref().map(|a| a.album.as_slice()));
        let mut appears_cards: Vec<gpui::AnyElement> = Vec::new();
        for (album, tracks) in self
            .appears_on
            .iter()
            .filter(|(album, _)| !own.contains(&album.id))
        {
            let flat = self.discography_ids.len();
            self.discography_ids.push(album.id.clone());
            let focused = self.vi_cursor == Some(flat);
            appears_cards.push(self.render_album_card(
                album,
                flat,
                focused,
                Some(appearance_line(album, *tracks)),
                tile,
                cx,
            ));
        }
        // The bio More/Less toggle is the last target when it exists.
        self.bio_toggle_focusable = bio_long;
        let bio_focused = bio_long && self.vi_cursor == Some(self.discography_ids.len());

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

        let page = v_flex()
            .id("artist-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            .child(
                // An explicit width and no shrinking, like the album page's
                // header card: left to the scrolling column, the card was
                // squeezed to the photo's height while the bio ran on through
                // its bottom and over the discography.
                v_flex()
                    .when_some(hero_card_width(content_w), |this, w| this.w(px(w)))
                    .flex_none()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    .bg(cx.theme().sidebar)
                    .child(
                        // Beside or under the photo, decided here rather
                        // than by `flex_wrap`: a wrapping row is measured as
                        // if it did not wrap, so once it did the bio ran out
                        // through the bottom of the card.
                        div()
                            .flex()
                            .gap_4()
                            .map(|row| match bio_w {
                                Some(BioColumn { beside: false, .. }) => row.flex_col(),
                                _ => row.flex_row().items_start(),
                            })
                            .child(
                                div()
                                    .id("artist-hero")
                                    .flex_none()
                                    .size(px(HERO_W))
                                    .rounded_2xl()
                                    .overflow_hidden()
                                    .bg(cx.theme().muted)
                                    // Only an image is worth enlarging; the
                                    // empty placeholder stays inert.
                                    .when_some(hero_art, |this, path| {
                                        this.cursor_pointer()
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.open_full_art(cx)
                                                }),
                                            )
                                            .child(img(path).size(px(HERO_W)).rounded_2xl())
                                    }),
                            )
                            .child(
                                v_flex()
                                    .map(|col| match bio_w {
                                        Some(b) => col.w(px(b.width)),
                                        None => col.flex_1().min_w(px(BIO_MIN_W)),
                                    })
                                    .gap_2()
                                    .child(div().text_2xl().font_medium().child(name))
                                    .when(bio_text.is_some() || bio_placeholder.is_some(), |this| {
                                        let pills =
                                            (bio_text.is_some() && sources.len() > 1).then(|| {
                                                h_flex().gap_1().children(sources.iter().map(
                                                    |&source| {
                                                        let label = match source {
                                                            AboutSource::Wikipedia => "Wikipedia",
                                                            AboutSource::Server => server_label,
                                                            AboutSource::MusicBrainz => {
                                                                "MusicBrainz"
                                                            }
                                                        };
                                                        Button::new(SharedString::from(format!(
                                                            "bio-src-{label}"
                                                        )))
                                                        .ghost()
                                                        .xsmall()
                                                        .label(label)
                                                        .selected(shown == Some(source))
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.about_pick = Some(source);
                                                                cx.notify();
                                                            },
                                                        ))
                                                    },
                                                ))
                                            });
                                        this.child(
                                            h_flex()
                                                .gap_2()
                                                .flex_wrap()
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child("Bio"),
                                                )
                                                .children(pills),
                                        )
                                        .child(bio_body(
                                            bio_text,
                                            read_more,
                                            bio_placeholder,
                                            cx,
                                        ))
                                    })
                                    .when(bio_long, |this| {
                                        let focused = bio_focused;
                                        let glow = self.session.read(cx).settings.selection_glow_vi;
                                        let btn = Button::new("bio-toggle")
                                            .ghost()
                                            .xsmall()
                                            .label("Read more")
                                            .icon(Icon::new(IconName::Maximize))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_bio(cx);
                                            }));
                                        this.child(h_flex().child(with_focus_cursor(
                                            "vi-bio-toggle",
                                            btn,
                                            focused,
                                            glow,
                                            None,
                                            cx,
                                        )))
                                    })
                                    .when_some(genres_line, |this, desc| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(desc),
                                        )
                                    })
                                    .when(!links.is_empty(), |this| {
                                        this.child(h_flex().gap_1().children(
                                            links.into_iter().map(|(label, url)| {
                                                Button::new(SharedString::from(format!(
                                                    "ar-link-{label}"
                                                )))
                                                .ghost()
                                                .small()
                                                .icon(link_icon(label))
                                                .tooltip(label)
                                                .on_click(move |_, _, cx| cx.open_url(&url))
                                            }),
                                        ))
                                    }),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, note| {
                this.child(crate::ui::error_banner(
                    &note,
                    cx.listener(|view, _, _, cx| {
                        view.error = None;
                        view.load(cx);
                    }),
                    cx,
                ))
            })
            .child(make_section("Albums".to_string(), album_cards))
            .child(make_section("Singles / EPs".to_string(), single_cards))
            // Unlike the other two, this section is dropped when it is empty:
            // it answers a question nobody asked, and an artist who guests on
            // nothing should not be told so.
            .when(!appears_cards.is_empty(), |this| {
                this.child(make_section("Appears on".to_string(), appears_cards))
            });

        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        // Nothing to read any more (a source switched off, the page reloaded):
        // the popup goes with it.
        if full_bio.is_none() {
            self.bio_open = false;
        }
        self.bio_reveal.set(self.bio_open, reduced_motion);
        if self.bio_reveal.settling(reduced_motion) {
            window.request_animation_frame();
        }
        let bio_popup = full_bio
            .filter(|_| self.bio_reveal.visible(reduced_motion))
            .map(|text| {
                self.render_bio_popup(text, shown_label, popup_read_more, content_w, window, cx)
            });

        div()
            .relative()
            .size_full()
            .child(page)
            .children(bio_popup)
            // Full-resolution artist photo; click anywhere to dismiss, same as
            // the album page's cover.
            .when(self.show_full_art, |this| {
                this.child(
                    div()
                        .id("artist-lightbox")
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
                            self.full_art_path
                                .clone()
                                .or_else(|| self.artist_image_path.clone()),
                            |this, path| {
                                this.child(img(path).max_w(px(820.)).max_h(px(820.)).rounded_lg())
                            },
                        ),
                )
            })
    }
}

/// What an "Appears on" card says under the cover: whose record it is, and how
/// much of it is this artist's.
fn appearance_line(album: &Album, tracks: i64) -> String {
    let who = album.artist.as_deref().map(str::trim).unwrap_or_default();
    let count = if tracks == 1 {
        "1 track".to_string()
    } else {
        format!("{tracks} tracks")
    };
    if who.is_empty() {
        count
    } else {
        format!("{who} · {count}")
    }
}

/// The albums the discography sections already show.
///
/// `LibraryDb::appears_on` answers the same question from the cache's album
/// credits, but a cache synced before those were recorded has none — an album
/// by two artists then reads as a guest spot for the second of them and the
/// record is drawn under Singles / EPs and Appears on at once. `getArtist` is
/// the authority and it is already on screen: anything it lists is the
/// artist's own, whatever the cache still believes.
fn own_album_ids(discography: Option<&[Album]>) -> HashSet<String> {
    discography
        .unwrap_or(&[])
        .iter()
        .map(|a| a.id.clone())
        .collect()
}

/// Whether a row synced from `library` belongs to what the user is browsing.
///
/// An empty selection is every library. A row with no recorded provenance —
/// synced before the column existed — is kept: it is unknown rather than
/// foreign, and dropping it would empty an artist page until the next sync.
fn in_libraries(library: Option<&str>, selected: &[String]) -> bool {
    selected.is_empty() || library.is_none_or(|id| selected.iter().any(|s| s == id))
}

/// Collapsed-bio length; roughly four lines at typical window widths.
const BIO_PREVIEW_CHARS: usize = 400;

/// The artist photo's edge.
const HERO_W: f32 = 220.;
/// Narrowest the bio column goes beside the photo before dropping under it.
const BIO_MIN_W: f32 = 260.;

/// How far one vi j/k moves the bio popup's text.
const BIO_POPUP_STEP: f32 = 64.;
/// The bio popup's widest; prose past this is a strain to read.
const BIO_POPUP_MAX_W: f32 = 680.;
/// Room the bio popup leaves around itself inside the content area.
const BIO_POPUP_MARGIN: f32 = 24.;

/// The bio popup's width in a content area `content_w` wide, a whole pixel so
/// the prose inside measures at the width it is laid out at.
fn bio_popup_width(content_w: f32) -> f32 {
    (content_w - 2. * BIO_POPUP_MARGIN)
        .clamp(0., BIO_POPUP_MAX_W)
        .floor()
}

/// The header card's width: the content area less the page's `p_4` either
/// side. `None` before anything has been measured.
fn hero_card_width(content_w: f32) -> Option<f32> {
    (content_w > 0.).then(|| (content_w - 32.).max(0.).floor())
}

/// Where the bio column goes and how wide it is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BioColumn {
    width: f32,
    /// Beside the photo; under it otherwise.
    beside: bool,
}

/// The bio column for a content area `content_w` wide: beside the photo where
/// it fits, under it otherwise. `None` before anything has been measured. The
/// page and the header card each pad by `p_4` either side, and the photo and
/// column sit `gap_4` apart. A couple of px are left spare, since a row filled
/// to the exact pixel is at the mercy of rounding.
fn bio_column_width(content_w: f32) -> Option<BioColumn> {
    const SLACK: f32 = 2.;
    if content_w <= 0. {
        return None;
    }
    let inner = (content_w - 64. - SLACK).max(0.);
    let beside = inner - HERO_W - 16.;
    // Floored to a whole pixel: gpui rounds bounds, and a fractional width
    // measured back through the scroll handle oscillates.
    Some(if beside >= BIO_MIN_W {
        BioColumn {
            width: beside.floor(),
            beside: true,
        }
    } else {
        BioColumn {
            width: inner.floor(),
            beside: false,
        }
    })
}

/// The bio's prose, one element per paragraph (a Wikipedia intro is several),
/// ending in the Last.fm link for a cut summary; or the muted line standing in
/// for it.
fn bio_body(
    text: Option<String>,
    read_more: Option<String>,
    placeholder: Option<&'static str>,
    cx: &App,
) -> gpui::AnyElement {
    let Some(text) = text else {
        return div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .children(placeholder)
            .into_any_element();
    };
    v_flex()
        .max_w(px(ABOUT_PROSE_MAX_W))
        .gap_2()
        .text_sm()
        .children(
            text.split('\n')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| div().child(p.to_string()))
                .collect::<Vec<_>>(),
        )
        .children(read_more.map(|url| {
            h_flex().child(
                Button::new("bio-read-more")
                    .link()
                    .xsmall()
                    .label("Read more on Last.fm")
                    .icon(Icon::new(IconName::ExternalLink))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
        }))
        .into_any_element()
}

/// The online services the artist page may ask, per the settings.
fn artist_sources(settings: &crate::config::Settings) -> album_info::Sources {
    album_info::Sources {
        wikipedia: settings.artist_info_wikipedia,
        musicbrainz: settings.artist_info_musicbrainz,
    }
}

/// The server's biography as text, and whether it was cut short.
///
/// Navidrome forwards Last.fm's *summary*, which stops at a fixed length and
/// ends in a "Read more on Last.fm" anchor; with the tags stripped, the
/// anchor's words are left dangling after the cut as if they were part of the
/// sentence. They are dropped here and the cut reported instead, so the page
/// can mark the text as cut and offer the link as a link.
fn clean_server_bio(raw: &str) -> Option<(String, bool)> {
    const READ_MORE: &str = "read more on last.fm";
    let text = strip_html(raw);
    let trimmed = text.trim_end();
    let cut_at = trimmed.len().checked_sub(READ_MORE.len());
    let (body, anchored) = match cut_at {
        Some(at)
            if trimmed.is_char_boundary(at) && trimmed[at..].eq_ignore_ascii_case(READ_MORE) =>
        {
            (&trimmed[..at], true)
        }
        _ => (trimmed, false),
    };
    let body = body.trim();
    (!body.is_empty()).then(|| (body.to_string(), anchored || looks_truncated(body)))
}

/// Whether an album belongs under "Singles / EPs" rather than "Albums".
///
/// The server's own `releaseTypes` decide it when present: they come from the
/// files' tags, i.e. from whoever released the record, and no guess made from
/// the title or the length can overrule that. Only an untagged album (or a
/// vanilla server) falls through to the heuristic.
fn is_single_or_ep(album: &Album) -> bool {
    if !album.release_types.is_empty() {
        return album
            .release_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case("single") || t.eq_ignore_ascii_case("ep"));
    }
    titled_single_or_ep(&album.name) || short_release(album.song_count, album.duration)
}

/// A title *ending* in "EP" or "Single" as a word of its own — "Title - EP",
/// "Title (Single)", "Title [EP]". Matching the letters anywhere filed "Deep",
/// "Sleep" and "Epic" under EPs.
fn titled_single_or_ep(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric())
        .rfind(|w| !w.is_empty())
        .is_some_and(|w| w.eq_ignore_ascii_case("ep") || w.eq_ignore_ascii_case("single"))
}

/// Length rule for untagged releases, after the common store convention:
/// under 30 minutes and at most six tracks is an EP (three or fewer, a
/// single). Without a duration only the single's track count is trusted, and
/// an album with no track count at all is kept an album — unknown is not short.
fn short_release(song_count: Option<u32>, duration: Option<u32>) -> bool {
    const MAX_SECS: u32 = 30 * 60;
    match (song_count, duration) {
        (Some(n), Some(d)) => n > 0 && n <= 6 && d < MAX_SECS,
        (Some(n), None) => n > 0 && n <= 3,
        (None, _) => false,
    }
}

async fn download_remote_image(url: &str) -> anyhow::Result<PathBuf> {
    let dir = crate::config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{}.img", crate::config::sanitize(url), ART_SIZE));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    // Artist photos are rarely square and every view draws them in a circle or
    // a square tile, so crop before caching — off the two IO workers, since a
    // decode would hold one of them.
    let bytes = tokio::task::spawn_blocking(move || {
        artwork::square_crop(&bytes).unwrap_or_else(|| bytes.to_vec())
    })
    .await?;
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
    fn the_header_card_and_bio_popup_fit_the_content_area() {
        use super::{BIO_POPUP_MAX_W, bio_popup_width, hero_card_width};
        assert_eq!(hero_card_width(0.), None);
        assert_eq!(hero_card_width(1200.5), Some(1168.));
        assert_eq!(bio_popup_width(2400.), BIO_POPUP_MAX_W);
        assert_eq!(bio_popup_width(500.5), 452.);
        assert_eq!(bio_popup_width(10.), 0.);
    }

    #[test]
    fn the_bio_goes_under_the_photo_only_when_it_must() {
        use super::{BioColumn, bio_column_width};
        assert_eq!(bio_column_width(0.), None);
        assert_eq!(
            bio_column_width(1748.),
            Some(BioColumn {
                width: 1446.,
                beside: true
            })
        );
        // 64 of padding, 220 of photo and 16 of gap leave under 260.
        assert_eq!(
            bio_column_width(500.),
            Some(BioColumn {
                width: 434.,
                beside: false
            })
        );
    }

    #[test]
    fn the_last_fm_anchor_is_dropped_and_read_as_a_cut() {
        use super::clean_server_bio;
        let raw = "Radiohead are an English rock band formed in Abingdon in \
                   <a href=\"https://www.last.fm/music/Radiohead\">Read more on Last.fm</a>";
        assert_eq!(
            clean_server_bio(raw),
            Some((
                "Radiohead are an English rock band formed in Abingdon in".into(),
                true
            ))
        );
        assert_eq!(
            clean_server_bio("They formed in 1985."),
            Some(("They formed in 1985.".into(), false))
        );
        // A summary cut without the anchor is still a cut.
        assert_eq!(
            clean_server_bio("formed over more"),
            Some(("formed over more".into(), true))
        );
        assert_eq!(clean_server_bio(" <a>Read more on Last.fm</a>"), None);
    }

    fn release(name: &str, songs: Option<u32>, secs: Option<u32>, types: &[&str]) -> Album {
        Album {
            id: name.into(),
            name: name.into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: songs,
            duration: secs,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
            artists: Vec::new(),
            original_release_date: None,
            release_date: None,
            release_types: types.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn singles_and_eps_are_grouped_separately() {
        assert!(is_single_or_ep(&release("Single", Some(2), None, &[])));
        assert!(!is_single_or_ep(&release(
            "Studio Album",
            Some(10),
            None,
            &[]
        )));
    }

    #[test]
    fn release_types_decide_when_present() {
        // Tagged EP with a long runtime, and a tagged album with two tracks:
        // the tags win over the length rule both ways.
        assert!(is_single_or_ep(&release(
            "Anything",
            Some(8),
            Some(3600),
            &["EP"]
        )));
        assert!(is_single_or_ep(&release(
            "Anything",
            Some(1),
            None,
            &["single"]
        )));
        assert!(!is_single_or_ep(&release(
            "Drone",
            Some(2),
            Some(4000),
            &["Album"]
        )));
        assert!(!is_single_or_ep(&release(
            "Hits - EP",
            Some(3),
            None,
            &["Album", "Compilation"]
        )));
    }

    #[test]
    fn ep_in_the_title_must_be_its_own_trailing_word() {
        assert!(is_single_or_ep(&release("Something - EP", None, None, &[])));
        assert!(is_single_or_ep(&release("Something (EP)", None, None, &[])));
        assert!(is_single_or_ep(&release("Song [Single]", None, None, &[])));
        for name in [
            "Deep Purple",
            "Sleep",
            "Epic",
            "Repeat",
            "EP Collection",
            "The Singles",
        ] {
            assert!(!is_single_or_ep(&release(name, None, None, &[])), "{name}");
        }
    }

    #[test]
    fn length_rule_for_untagged_releases() {
        // Six tracks in 22 minutes: an EP the old four-track cutoff missed.
        assert!(is_single_or_ep(&release(
            "Short",
            Some(6),
            Some(22 * 60),
            &[]
        )));
        // Three long tracks: an album, not a single.
        assert!(!is_single_or_ep(&release(
            "Long",
            Some(3),
            Some(45 * 60),
            &[]
        )));
        // No duration: only the single's count is trusted.
        assert!(is_single_or_ep(&release("A", Some(3), None, &[])));
        assert!(!is_single_or_ep(&release("B", Some(5), None, &[])));
        // Unknown track count is not short.
        assert!(!is_single_or_ep(&release("C", None, None, &[])));
        assert!(!is_single_or_ep(&release("D", Some(0), Some(0), &[])));
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

    fn album(name: &str, year: Option<i32>) -> Album {
        Album {
            id: name.to_string(),
            name: name.to_string(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: None,
            duration: None,
            created: None,
            year,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
            artists: Vec::new(),
            original_release_date: None,
            release_date: None,
            release_types: Vec::new(),
        }
    }

    #[test]
    fn discography_runs_newest_first_with_undated_albums_last() {
        let mut albums = vec![
            album("Debut", Some(1999)),
            album("Unknown", None),
            album("Later", Some(2020)),
            album("Split B", Some(2020)),
        ];
        sort_discography(&mut albums);
        let names: Vec<_> = albums.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Later", "Split B", "Debut", "Unknown"]);
    }

    fn dated(name: &str, year: i32, month: u32, day: u32) -> Album {
        let mut album = album(name, Some(year));
        album.original_release_date = Some(subsonic::ItemDate {
            year: Some(year),
            month: Some(month),
            day: Some(day),
        });
        album
    }

    #[test]
    fn same_year_releases_are_ordered_by_month_and_day() {
        let mut albums = vec![
            dated("March", 2020, 3, 4),
            dated("November", 2020, 11, 2),
            dated("March later", 2020, 3, 20),
        ];
        sort_discography(&mut albums);
        let names: Vec<_> = albums.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["November", "March later", "March"]);
    }

    /// A precise date outranks a bare year within the same year: the album the
    /// server can place is the one that earns the newer slot.
    #[test]
    fn a_dated_release_leads_a_year_only_one_from_the_same_year() {
        let mut albums = vec![album("Year only", Some(2020)), dated("Dated", 2020, 6, 1)];
        sort_discography(&mut albums);
        let names: Vec<_> = albums.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Dated", "Year only"]);
    }

    /// `releaseDate` (this edition) only answers where `originalReleaseDate`
    /// (the work) is absent, so a reissue sorts with the record it reissues.
    #[test]
    fn the_original_release_date_wins_over_the_edition_date() {
        let mut album = album("Remaster", Some(2021));
        album.original_release_date = Some(subsonic::ItemDate {
            year: Some(1979),
            month: Some(8),
            day: None,
        });
        album.release_date = Some(subsonic::ItemDate {
            year: Some(2021),
            month: Some(5),
            day: Some(3),
        });
        assert_eq!(album.release_key(), Some((1979, 8, 0)));
    }

    /// A server that sends the element with no year in it is answering
    /// "unknown", and must not outrank the plain `year` field beside it.
    #[test]
    fn an_empty_item_date_falls_back_to_the_year() {
        let mut album = album("Sparse", Some(2004));
        album.original_release_date = Some(subsonic::ItemDate::default());
        assert_eq!(album.release_key(), Some((2004, 0, 0)));
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;
    use crate::services::library_db::{AlbumRow, LibraryDb};

    fn album(name: &str, artist: Option<&str>) -> Album {
        let mut album = album_from_row(AlbumRow::new(
            &format!("navidrome:album:{name}"),
            "navidrome",
            name,
        ));
        album.artist = artist.map(str::to_string);
        album
    }

    #[test]
    fn an_empty_selection_is_every_library() {
        assert!(in_libraries(Some("1"), &[]));
        assert!(in_libraries(None, &[]));
    }

    #[test]
    fn a_selection_keeps_its_own_libraries_and_drops_the_rest() {
        let selected = ["1".to_string(), "3".to_string()];
        assert!(in_libraries(Some("3"), &selected));
        assert!(!in_libraries(Some("2"), &selected));
    }

    /// Rows synced before the provenance column exist with no library at all.
    /// Unknown is not foreign: dropping them would empty an artist page for
    /// anyone who hasn't resynced since.
    #[test]
    fn a_row_with_no_recorded_library_is_kept() {
        assert!(in_libraries(None, &["1".to_string()]));
    }

    /// A record by two artists is in both their discographies, and a cache with
    /// no album credits yet reads it as a guest spot for the one the server's
    /// `artistId` does not name. The page drew it in both sections at once.
    #[test]
    fn an_album_the_discography_lists_is_not_an_appearance() {
        let own = vec![album("Sancu", Some("Nico Arezzo • Amore Audio"))];
        let ids = own_album_ids(Some(&own));
        // `album_from_row` strips the sync's namespace, so both sides of the
        // comparison are the server's own ids.
        assert!(ids.contains("Sancu"));
        assert!(!ids.contains("Potomac"));
    }

    /// Before `getArtist` lands there is no discography to compare against, and
    /// the cached appearances are shown as they are rather than all dropped.
    #[test]
    fn nothing_is_excluded_while_the_discography_is_still_loading() {
        assert!(own_album_ids(None).is_empty());
    }

    #[test]
    fn an_appearance_names_the_album_artist_and_counts_the_tracks() {
        assert_eq!(
            appearance_line(&album("Blue Note", Some("Art Blakey")), 2),
            "Art Blakey · 2 tracks"
        );
        assert_eq!(
            appearance_line(&album("Blue Note", Some("Art Blakey")), 1),
            "Art Blakey · 1 track"
        );
        // A compilation row the sync never got an artist for still says how
        // much of it belongs to this artist.
        assert_eq!(appearance_line(&album("Blue Note", None), 3), "3 tracks");
    }

    /// An album is "appeared on" when the artist plays on it but is credited to
    /// someone else — the artist's own albums are `getArtist`'s job, and
    /// listing them twice is what the section must not do.
    #[test]
    fn appears_on_finds_guest_spots_and_skips_the_artists_own_albums() {
        let db = LibraryDb::open_in_memory().unwrap();
        let mut own = AlbumRow::new("navidrome:album:own", "navidrome", "Own Record");
        own.artist_id = Some("navidrome:artist:me".into());
        let mut guest = AlbumRow::new("navidrome:album:guest", "navidrome", "Their Record");
        guest.artist = Some("Them".into());
        guest.artist_id = Some("navidrome:artist:them".into());
        db.upsert_album(&own).unwrap();
        db.upsert_album(&guest).unwrap();
        // `primary` is what the server leads with (`artistId`); `credits` is
        // everyone OpenSubsonic names on the track.
        let track = |id: &str, album: &str, primary: &str, credits: &[&str]| {
            db.upsert_track(
                id,
                "navidrome",
                "Song",
                None,
                Some(primary),
                None,
                Some(album),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let credits: Vec<String> = credits.iter().map(|c| (*c).to_string()).collect();
            db.set_track_artists(id, &credits).unwrap();
        };
        let me = "navidrome:artist:me";
        let them = "navidrome:artist:them";
        track("navidrome:track:1", "navidrome:album:own", me, &[me]);
        track("navidrome:track:2", "navidrome:album:guest", me, &[me]);
        // The case the whole table exists for: the server files the collaboration
        // under the album artist, and only the credits name the guest.
        track(
            "navidrome:track:3",
            "navidrome:album:guest",
            them,
            &[them, me],
        );
        track("navidrome:track:4", "navidrome:album:guest", them, &[them]);

        let found = db.appears_on("navidrome", me).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.id, "navidrome:album:guest");
        // Only the tracks they are credited on are counted, not the album's.
        assert_eq!(found[0].1, 2);

        // Their own record with a co-artist: the album row carries the other
        // artist's id and both names joined, so only the album credits can say
        // it is theirs — and an album that is theirs is not an appearance.
        db.upsert_catalog(
            "navidrome",
            &[],
            &[],
            &[(
                "navidrome:album:guest".to_string(),
                vec![them.to_string(), me.to_string()],
            )],
        )
        .unwrap();
        assert!(db.appears_on("navidrome", me).unwrap().is_empty());
    }
}
