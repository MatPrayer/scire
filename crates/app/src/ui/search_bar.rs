//! Global search (Ctrl/Cmd+K command palette): the local cache answered
//! instantly, then merged with the server's own `search3`.
//!
//! The cache is not a fallback here, it is the first answer. `LibraryDb` holds
//! the last sync's whole catalog plus every locally scanned file, so a query
//! can be served before a request would have left the machine — and served at
//! all when there is no server, which is the only search a local-only library
//! ever gets.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, ElementId, Entity, EventEmitter, Focusable as _,
    IntoElement, KeyDownEvent, Render, ScrollHandle, Stateful, Window, div, ease_out_quint, img,
    prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use subsonic::{SearchResult3, Song};

use crate::services::library_db::{CatalogSearch, LibraryDb};
use crate::services::local_library::local_art_path;
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::session::Session;

/// Wait before asking the server; see also `CACHE_DEBOUNCE`.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Wait before asking the cache — a fifth of the server's, since the answer
/// costs a local scan rather than a round trip.
const CACHE_DEBOUNCE: Duration = Duration::from_millis(60);
/// Thumbnail resolution for dropdown rows.
const ART_SIZE: u32 = 64;
/// Rows shown per section — the dropdown is a quick jump, not a browser.
const MAX_SONGS: usize = 8;
const MAX_ALBUMS: usize = 6;
const MAX_ARTISTS: usize = 5;
/// Rows pulled from the cache per category before ranking. Generous, because
/// SQL returns them alphabetically and the good match may sit anywhere in
/// that order; the list is cut to the `MAX_*` caps only after ranking.
const CACHE_FETCH: usize = 120;

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// A text that matched every word of the query, but not at any word start.
const TIER_LOOSE: u8 = 4;
/// Penalty for matching on a secondary field (an album's artist rather than
/// its title): a whole band below every primary tier, so a title match always
/// outranks one.
const SECONDARY: u8 = 5;
/// No match at all — sorts below everything, but is still rendered, since the
/// server may have returned the row for a reason the client cannot see.
const MISS: u8 = 10;

/// Which tier `text` lands in for `query`. Lower sorts first; `None` when the
/// text does not answer the query at all.
///
/// The tiers exist because a heavily-featured name matches every collaboration
/// credited as its own artist ("Skrillex & Damian Marley", "Skrillex, Diplo &
/// …") and servers rank by their own relevance, so the plain artist can land
/// past the handful of rows this dropdown shows.
fn match_rank(query: &str, text: &str) -> Option<u8> {
    let q = query.trim().to_lowercase();
    let n = text.trim().to_lowercase();
    if q.is_empty() {
        return Some(TIER_LOOSE);
    }
    if n == q {
        return Some(0);
    }
    if let Some(rest) = n.strip_prefix(&q) {
        // "Skrillex & Damian Marley" is the artist plus someone else;
        // "Skrillexia" is a different name that merely starts the same way.
        return Some(if rest.starts_with(|c: char| !c.is_alphanumeric()) {
            1
        } else {
            2
        });
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.is_empty() {
        return Some(TIER_LOOSE);
    }
    if words.iter().all(|w| starts_a_word(&n, w)) {
        return Some(3);
    }
    if words.iter().all(|w| n.contains(w)) {
        return Some(TIER_LOOSE);
    }
    None
}

/// True when `needle` begins a word of `haystack` (both already lowercased).
fn starts_a_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(needle))
}

/// How well a row answers the query: its primary text, or a secondary one (an
/// album's artist, a song's artist) a band lower. Lower sorts first.
fn score(query: &str, primary: &str, secondary: Option<&str>) -> u8 {
    if let Some(r) = match_rank(query, primary) {
        return r;
    }
    secondary
        .and_then(|s| match_rank(query, s))
        .map(|r| r + SECONDARY)
        .unwrap_or(MISS)
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

// Every variant opens something; the shared verb is the point of the enum.
#[allow(clippy::enum_variant_names)]
pub enum SearchBarEvent {
    OpenAlbum(String),
    OpenLocalAlbum(String),
    OpenArtist(String),
}

/// Where a row's thumbnail comes from.
#[derive(Clone)]
struct Cover {
    /// Server cover id, or the local scanner's art hash.
    id: String,
    /// Cache key rows share — album-scoped for songs, since Navidrome gives
    /// every song of one record its own cover id.
    key: String,
    local: bool,
}

#[derive(Clone)]
struct ArtistHit {
    id: String,
    name: String,
    cover: Option<Cover>,
}

#[derive(Clone)]
struct AlbumHit {
    id: String,
    title: String,
    artist: Option<String>,
    cover: Option<Cover>,
    /// Locally scanned: opens the local detail view, not the server one.
    local: bool,
}

#[derive(Clone)]
struct SongHit {
    song: Song,
    cover: Option<Cover>,
    local: bool,
}

/// Everything one query found, from either source, already deduped.
#[derive(Default)]
struct Hits {
    artists: Vec<ArtistHit>,
    albums: Vec<AlbumHit>,
    songs: Vec<SongHit>,
}

impl Hits {
    fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty() && self.songs.is_empty()
    }

    /// Append what `other` holds and this does not. What is already shown keeps
    /// its place: the cache paints first, and a server response arriving 200ms
    /// later must not reshuffle the row the user is reaching for.
    fn merge(&mut self, other: Hits) {
        let artists: HashSet<String> = self.artists.iter().map(|a| a.id.clone()).collect();
        self.artists.extend(
            other
                .artists
                .into_iter()
                .filter(|a| !artists.contains(&a.id)),
        );

        let albums: HashSet<(bool, String)> = self
            .albums
            .iter()
            .map(|a| (a.local, a.id.clone()))
            .collect();
        self.albums.extend(
            other
                .albums
                .into_iter()
                .filter(|a| !albums.contains(&(a.local, a.id.clone()))),
        );

        let songs: HashSet<(bool, String)> = self
            .songs
            .iter()
            .map(|s| (s.local, s.song.id.clone()))
            .collect();
        self.songs.extend(
            other
                .songs
                .into_iter()
                .filter(|s| !songs.contains(&(s.local, s.song.id.clone()))),
        );
    }

    /// Put the rows that answer the query at the top of each section. Sorting
    /// is stable, so within a tier the cache's alphabetical order and the
    /// server's relevance order both survive.
    fn rank(&mut self, query: &str) {
        self.artists.sort_by_key(|a| score(query, &a.name, None));
        self.albums
            .sort_by_key(|a| score(query, &a.title, a.artist.as_deref()));
        self.songs
            .sort_by_key(|s| score(query, &s.song.title, s.song.artist.as_deref()));
    }
}

/// Strip the namespace `navidrome_sync` stores server ids under.
///
/// The cache keeps them as `navidrome:album:<id>`; everything downstream — the
/// detail views, the stream URL, and the dedupe against the server's own
/// results — speaks the bare id the API answers with. Left namespaced, a cached
/// album is a second copy of one already listed and opening it asks the server
/// for an id it has never heard of.
fn strip_ns(id: String, prefix: &str) -> String {
    id.strip_prefix(prefix).map(str::to_string).unwrap_or(id)
}

/// Turn the cache's rows into hits, honouring the selected libraries.
///
/// Local artists are dropped: there is no local artist page to open, so a row
/// that cannot be activated would only be in the way. Their albums and songs
/// are both reachable and playable, and stay.
fn hits_from_cache(found: CatalogSearch, libraries: &[String]) -> Hits {
    let in_selection = |source: &str, library_id: &Option<String>| {
        // A subset is only meaningful for server rows, and a row synced before
        // schema v3 has no library at all — dropping those would make a search
        // quietly miss part of the library until the next sync.
        source != "navidrome"
            || libraries.is_empty()
            || library_id.as_ref().is_none_or(|id| libraries.contains(id))
    };

    let artists = found
        .artists
        .into_iter()
        .filter(|a| a.source != "local" && in_selection(&a.source, &a.library_id))
        .map(|a| ArtistHit {
            cover: a.cover_art.map(|id| Cover {
                key: id.clone(),
                id,
                local: false,
            }),
            id: strip_ns(a.id, "navidrome:artist:"),
            name: a.name,
        })
        .collect();

    let albums = found
        .albums
        .into_iter()
        .filter(|a| in_selection(&a.source, &a.library_id))
        .map(|a| {
            let local = a.source == "local";
            AlbumHit {
                cover: a.cover_art.map(|id| Cover {
                    key: if local {
                        format!("local:{id}")
                    } else {
                        id.clone()
                    },
                    id,
                    local,
                }),
                id: strip_ns(a.id, "navidrome:album:"),
                title: a.title,
                artist: a.artist,
                local,
            }
        })
        .collect();

    let songs = found
        .tracks
        .into_iter()
        .filter(|t| in_selection(&t.source, &None))
        .map(|t| {
            let local = t.source == "local";
            let mut song = t.into_song();
            if !local {
                song.id = strip_ns(song.id, "navidrome:track:");
                // Stripped too, so the art lands on the key the album pages
                // already cache the very same cover under.
                song.album_id = song.album_id.map(|id| strip_ns(id, "navidrome:album:"));
            }
            SongHit {
                cover: song_cover(&song, local),
                song,
                local,
            }
        })
        .collect();

    Hits {
        artists,
        albums,
        songs,
    }
}

fn hits_from_server(result: SearchResult3) -> Hits {
    Hits {
        artists: result
            .artist
            .into_iter()
            .map(|a| ArtistHit {
                cover: a.cover_art.map(|id| Cover {
                    key: id.clone(),
                    id,
                    local: false,
                }),
                id: a.id,
                name: a.name,
            })
            .collect(),
        albums: result
            .album
            .into_iter()
            .map(|a| AlbumHit {
                cover: a.cover_art.map(|id| Cover {
                    key: id.clone(),
                    id,
                    local: false,
                }),
                id: a.id,
                title: a.name,
                artist: a.artist,
                local: false,
            })
            .collect(),
        songs: result
            .song
            .into_iter()
            .map(|song| SongHit {
                cover: song_cover(&song, false),
                song,
                local: false,
            })
            .collect(),
    }
}

/// A song's thumbnail: the album-scoped cache key for server songs, the
/// scanner's art hash for local ones.
fn song_cover(song: &Song, local: bool) -> Option<Cover> {
    if local {
        return song.cover_art.clone().map(|id| Cover {
            key: format!("local:{id}"),
            id,
            local: true,
        });
    }
    artwork::song_cover(song).map(|(id, key)| Cover {
        id,
        key,
        local: false,
    })
}

/// A keyboard-selectable result, in the same order rows are rendered
/// (artists, then albums, then songs). Index into this list == `selected`.
enum PaletteItem {
    Artist(String),
    Album { id: String, local: bool },
    Song(Box<Song>),
}

pub struct SearchBar {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    library_db: Arc<LibraryDb>,
    input: Entity<InputState>,
    results: Hits,
    /// Dropdown visibility; results stay cached while hidden so reopening
    /// the same query is instant.
    open: bool,
    /// Centered command-palette mode (Ctrl/Cmd+K) vs. the inline top-right bar.
    palette: bool,
    /// Highlighted row for arrow-key navigation (palette mode).
    selected: usize,
    /// Scroll handle for the palette results, so arrow keys can scroll the
    /// highlighted row into view.
    results_scroll: ScrollHandle,
    /// A query has been typed and neither source has answered it yet.
    pending: bool,
    /// A server request is in flight; the cache's rows are already up.
    searching: bool,
    error: Option<String>,
    /// Album-scoped art key (or plain cover id for albums/artists) → path.
    art_paths: HashMap<String, PathBuf>,
    generation: u64,
}

impl EventEmitter<SearchBarEvent> for SearchBar {}

impl SearchBar {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        library_db: Arc<LibraryDb>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // No `clean_on_escape`: Escape must reach our own handlers to close the
        // palette / dropdown, not be swallowed to clear the field.
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));

        cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_query_changed(cx);
            }
        })
        .detach();

        Self {
            session,
            player,
            library_db,
            input,
            results: Hits::default(),
            open: false,
            palette: false,
            selected: 0,
            results_scroll: ScrollHandle::new(),
            pending: false,
            searching: false,
            error: None,
            art_paths: HashMap::new(),
            generation: 0,
        }
    }

    /// Focus the input (wired to the `/` shortcut in the root view).
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// True while the query field holds the keyboard, in either form of the
    /// bar (top-right dropdown or centred palette).
    ///
    /// The root view's shortcuts stand down while this is true. A space typed
    /// into a query is a space, and the shortcut does not merely win the key —
    /// stopping propagation for it means gpui never falls through to text
    /// insertion, so the character is swallowed outright.
    pub fn is_typing(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    pub fn is_palette(&self) -> bool {
        self.palette
    }

    /// Open the centered command palette (Ctrl/Cmd+K) with a fresh query.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = true;
        self.selected = 0;
        self.results = Hits::default();
        self.open = false;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    /// Close the dropdown/palette and clear the query (root's Escape handler).
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.palette = false;
        self.selected = 0;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.results = Hits::default();
        self.pending = false;
        self.searching = false;
        self.error = None;
        cx.notify();
    }

    /// Flat list of selectable rows, in render order. `selected` indexes this.
    fn items(&self) -> Vec<PaletteItem> {
        let mut v = Vec::new();
        for a in self.results.artists.iter().take(MAX_ARTISTS) {
            v.push(PaletteItem::Artist(a.id.clone()));
        }
        for a in self.results.albums.iter().take(MAX_ALBUMS) {
            v.push(PaletteItem::Album {
                id: a.id.clone(),
                local: a.local,
            });
        }
        for s in self.results.songs.iter().take(MAX_SONGS) {
            v.push(PaletteItem::Song(Box::new(s.song.clone())));
        }
        v
    }

    /// Move the highlight by `delta`, wrapping at the ends.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.items().len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        if let Some(child) = self.selected_child_index() {
            self.results_scroll.scroll_to_item(child);
        }
        cx.notify();
    }

    /// Index of the selected row among the scroll container's children (which
    /// interleave section titles with rows), so we can scroll it into view.
    fn selected_child_index(&self) -> Option<usize> {
        let counts = [
            self.results.artists.len().min(MAX_ARTISTS),
            self.results.albums.len().min(MAX_ALBUMS),
            self.results.songs.len().min(MAX_SONGS),
        ];
        let mut child = 0;
        let mut item = 0;
        for n in counts {
            if n == 0 {
                continue;
            }
            child += 1; // section title
            for _ in 0..n {
                if item == self.selected {
                    return Some(child);
                }
                child += 1;
                item += 1;
            }
        }
        None
    }

    /// Activate the highlighted row (Enter in palette mode).
    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.items();
        let Some(item) = items.into_iter().nth(self.selected) else {
            return;
        };
        match item {
            PaletteItem::Artist(id) => cx.emit(SearchBarEvent::OpenArtist(id)),
            PaletteItem::Album { id, local: true } => cx.emit(SearchBarEvent::OpenLocalAlbum(id)),
            PaletteItem::Album { id, local: false } => cx.emit(SearchBarEvent::OpenAlbum(id)),
            PaletteItem::Song(song) => {
                self.player
                    .update(cx, |p, cx| p.play_queue(vec![*song], 0, cx));
            }
        }
        self.dismiss(window, cx);
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.selected = 0;
        let generation = self.generation;
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.open = false;
            self.results = Hits::default();
            self.pending = false;
            self.searching = false;
            self.error = None;
            cx.notify();
            return;
        }
        self.pending = true;
        self.open = true;
        self.search_cache(query, generation, cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |bar, cx| {
                if bar.generation == generation {
                    bar.run_search(cx);
                }
            });
        })
        .detach();
    }

    /// Answer the query out of `LibraryDb`, near-instantly.
    ///
    /// `CACHE_DEBOUNCE` rather than the server's `DEBOUNCE`: the point of the
    /// cache is that rows appear as the user types, not a third of a second
    /// after they stop, and the query is three unindexed LIKE scans — cheap,
    /// but not so cheap that every keystroke of a fast typist should start one
    /// against the same connection the scanner writes through. Long enough to
    /// coalesce a burst, short enough to read as no wait at all.
    fn search_cache(&mut self, query: String, generation: u64, cx: &mut Context<Self>) {
        let db = self.library_db.clone();
        let libraries = self.session.read(cx).library_ids.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CACHE_DEBOUNCE).await;
            if this.read_with(cx, |bar, _| bar.generation).ok() != Some(generation) {
                return;
            }
            let found = runtime::spawn_blocking_io(move || {
                db.search_catalog(&query, CACHE_FETCH)
                    .map_err(anyhow::Error::from)
            })
            .await;
            let Ok(found) = found else { return };
            let _ = this.update(cx, |bar, cx| {
                if bar.generation != generation {
                    return;
                }
                let query = bar.input.read(cx).value().trim().to_string();
                let mut hits = hits_from_cache(found, &libraries);
                hits.rank(&query);
                bar.fetch_result_art(&hits, cx);
                bar.results = hits;
                bar.pending = false;
                bar.selected = 0;
                cx.notify();
            });
        })
        .detach();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            // No server: the cache is the whole answer, and it has landed.
            self.pending = false;
            cx.notify();
            return;
        };
        let libraries = self.session.read(cx).library_query_ids();
        let generation = self.generation;
        self.searching = true;
        self.open = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                // Search each selected library and merge, deduping items
                // that appear in more than one.
                let mut merged: Option<SearchResult3> = None;
                let mut seen = HashSet::new();
                for lib in &libraries {
                    let mut r = client
                        .search3(&query, lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    r.artist.retain(|a| seen.insert(format!("ar:{}", a.id)));
                    r.album.retain(|a| seen.insert(format!("al:{}", a.id)));
                    r.song.retain(|s| seen.insert(format!("s:{}", s.id)));
                    match &mut merged {
                        Some(m) => {
                            m.artist.extend(r.artist);
                            m.album.extend(r.album);
                            m.song.extend(r.song);
                        }
                        None => merged = Some(r),
                    }
                }
                Ok::<_, anyhow::Error>(merged.unwrap_or_default())
            })
            .await;
            let _ = this.update(cx, |bar, cx| {
                if bar.generation != generation {
                    return;
                }
                bar.searching = false;
                bar.pending = false;
                match result {
                    Ok(r) => {
                        let hits = hits_from_server(r);
                        bar.fetch_result_art(&hits, cx);
                        bar.results.merge(hits);
                        let query = bar.input.read(cx).value().trim().to_string();
                        bar.results.rank(&query);
                        bar.error = None;
                    }
                    // A dead server is not a dead search — the cache's rows
                    // stay up, and the error explains what is missing.
                    Err(e) => bar.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Resolve thumbnails for the rows we will actually show.
    fn fetch_result_art(&mut self, hits: &Hits, cx: &mut Context<Self>) {
        let covers = hits
            .songs
            .iter()
            .take(MAX_SONGS)
            .filter_map(|s| s.cover.clone())
            .chain(
                hits.albums
                    .iter()
                    .take(MAX_ALBUMS)
                    .filter_map(|a| a.cover.clone()),
            )
            .chain(
                hits.artists
                    .iter()
                    .take(MAX_ARTISTS)
                    .filter_map(|a| a.cover.clone()),
            );
        for cover in covers {
            if self.art_paths.contains_key(&cover.key) {
                continue;
            }
            if cover.local {
                // The scanner already extracted it; nothing to download.
                if let Some(path) = local_art_path(&cover.id).filter(|p| p.exists()) {
                    self.art_paths.insert(cover.key, path);
                }
                continue;
            }
            // Synchronous cache hit: no task, renders with the results.
            if let Some(path) = artwork::cached(&cover.key, ART_SIZE) {
                self.art_paths.insert(cover.key, path);
                continue;
            }
            let Some(client) = self.session.read(cx).client.clone() else {
                return;
            };
            let Cover { id, key, .. } = cover;
            cx.spawn(async move |this, cx| {
                if let Ok(path) =
                    runtime::spawn_io(artwork::fetch_as(client, id, key.clone(), ART_SIZE)).await
                {
                    let _ = this.update(cx, |bar, cx| {
                        bar.art_paths.insert(key, path);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    fn thumb(
        &self,
        cover: Option<&Cover>,
        fallback: IconName,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let path = cover.and_then(|c| self.art_paths.get(&c.key).cloned());
        div()
            .size(px(32.))
            .flex_none()
            .rounded_sm()
            .bg(cx.theme().muted)
            .overflow_hidden()
            .map(|this| match path {
                Some(path) => this.child(img(path).size(px(32.)).rounded_sm()),
                None => this
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(fallback).small()),
            })
            .into_any_element()
    }

    fn section_title(label: &'static str, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .px_2()
            .pt_2()
            .pb_1()
            .text_xs()
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element()
    }

    /// Marks a row that came from the local scanner, so an album present both
    /// on disk and on the server can be told apart before it is opened.
    fn local_badge(cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .px_1p5()
            .rounded_sm()
            .bg(cx.theme().muted)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child("Local")
    }

    /// Selected-row highlight (palette arrow-key navigation). Mirrors the
    /// album track-list convention: muted fill + a `primary` left border.
    fn row_selected(&self, idx: usize) -> bool {
        self.palette && idx == self.selected
    }

    /// The chrome every result row shares: thumbnail, title over an optional
    /// subtitle, hover fill and the selection highlight. Callers add the click
    /// handler and whatever trails the text.
    fn row_shell(
        &self,
        id: impl Into<ElementId>,
        idx: usize,
        cover: Option<&Cover>,
        fallback: IconName,
        labels: (String, Option<String>),
        cx: &Context<Self>,
    ) -> Stateful<gpui::Div> {
        let (title, subtitle) = labels;
        let sel = self.row_selected(idx);
        h_flex()
            .id(id)
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(sel, |s| {
                s.bg(cx.theme().muted)
                    .border_l_2()
                    .border_color(cx.theme().primary)
            })
            .child(self.thumb(cover, fallback, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(title))
                    .when_some(subtitle, |this, sub| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(sub),
                        )
                    }),
            )
    }

    /// Build the result rows shared by the inline dropdown and the palette.
    /// The flat selectable index is threaded so the highlighted row matches
    /// `selected`; section titles do not advance it.
    fn result_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut idx = 0usize;

        if !self.results.artists.is_empty() {
            rows.push(Self::section_title("Artists", cx));
            for (i, artist) in self.results.artists.iter().take(MAX_ARTISTS).enumerate() {
                let id = artist.id.clone();
                rows.push(
                    self.row_shell(
                        ("sb-artist", i),
                        idx,
                        artist.cover.as_ref(),
                        IconName::CircleUser,
                        (artist.name.clone(), None),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.emit(SearchBarEvent::OpenArtist(id.clone()));
                        this.dismiss(window, cx);
                    }))
                    .into_any_element(),
                );
                idx += 1;
            }
        }

        if !self.results.albums.is_empty() {
            rows.push(Self::section_title("Albums", cx));
            for (i, album) in self.results.albums.iter().take(MAX_ALBUMS).enumerate() {
                let id = album.id.clone();
                let local = album.local;
                rows.push(
                    self.row_shell(
                        ("sb-album", i),
                        idx,
                        album.cover.as_ref(),
                        IconName::LayoutDashboard,
                        (
                            album.title.clone(),
                            Some(album.artist.clone().unwrap_or_default()),
                        ),
                        cx,
                    )
                    .when(local, |this| this.child(Self::local_badge(cx)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.emit(if local {
                            SearchBarEvent::OpenLocalAlbum(id.clone())
                        } else {
                            SearchBarEvent::OpenAlbum(id.clone())
                        });
                        this.dismiss(window, cx);
                    }))
                    .into_any_element(),
                );
                idx += 1;
            }
        }

        // Songs: click plays, `+` enqueues.
        if !self.results.songs.is_empty() {
            rows.push(Self::section_title("Songs", cx));
            for (i, hit) in self.results.songs.iter().take(MAX_SONGS).enumerate() {
                let play = hit.song.clone();
                let enqueue = hit.song.clone();
                rows.push(
                    self.row_shell(
                        ("sb-song", i),
                        idx,
                        hit.cover.as_ref(),
                        IconName::Star,
                        (
                            hit.song.title.clone(),
                            Some(hit.song.artist.clone().unwrap_or_default()),
                        ),
                        cx,
                    )
                    .when(hit.local, |this| this.child(Self::local_badge(cx)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.player.update(cx, |p, cx| {
                            p.play_queue(vec![play.clone()], 0, cx);
                        });
                        this.dismiss(window, cx);
                    }))
                    .child(
                        Button::new(("sb-enq", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Plus))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.player
                                    .update(cx, |p, cx| p.enqueue(vec![enqueue.clone()], cx));
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element(),
                );
                idx += 1;
            }
        }

        if rows.is_empty() {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.pending || self.searching {
                        "Searching…"
                    } else {
                        "No results"
                    })
                    .into_any_element(),
            );
        }
        if let Some(e) = &self.error {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(e.clone())
                    .into_any_element(),
            );
        }

        rows
    }

    /// Centered command-palette box: large input on top, scrollable results
    /// below. Arrow/Enter/Escape are handled in the capture phase so they
    /// drive selection instead of reaching the input or the root shortcuts.
    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let has_query = !self.input.read(cx).value().trim().is_empty();
        let rows = self.result_rows(cx);
        // The cache answers first, so a spinner belongs to the server pass
        // only, and only while it has something left to add.
        let updating = self.searching && !self.results.is_empty();
        v_flex()
            .id("search-palette")
            .occlude()
            .w(px(620.))
            .max_h(px(560.))
            .rounded_xl()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .capture_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "down" => {
                        this.move_selection(1, cx);
                        cx.stop_propagation();
                    }
                    "up" => {
                        this.move_selection(-1, cx);
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.activate_selected(window, cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.dismiss(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(IconName::Search)),
                    )
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .when(updating, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Updating…"),
                        )
                    }),
            )
            .when(has_query, |this| {
                this.child(
                    v_flex()
                        .id("palette-scroll")
                        .max_h(px(480.))
                        .overflow_y_scroll()
                        .track_scroll(&self.results_scroll)
                        .p_1()
                        .children(rows),
                )
            })
            .when(!has_query, |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Type to search artists, albums and songs…"),
                )
            })
            .with_animation(
                ElementId::Name("search-palette-anim".into()),
                Animation::new(crate::ui::transition(
                    self.session.read(cx).settings.reduced_motion,
                    150,
                ))
                .with_easing(ease_out_quint()),
                |this, t| this.opacity(t),
            )
            .into_any_element()
    }
}

impl Render for SearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.palette {
            // The centered box only; root supplies the full-window backdrop.
            return self.render_palette(cx);
        }
        // ponytail: inline search bar removed. Only palette mode (Ctrl+K) remains.
        div().into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::library_db::{AlbumRow, ArtistRow, TrackRow};

    fn artist_hit(name: &str) -> ArtistHit {
        ArtistHit {
            id: name.to_string(),
            name: name.to_string(),
            cover: None,
        }
    }

    fn album_hit(title: &str, artist: &str, local: bool) -> AlbumHit {
        AlbumHit {
            id: format!("{title}-{artist}"),
            title: title.to_string(),
            artist: Some(artist.to_string()),
            cover: None,
            local,
        }
    }

    #[test]
    fn the_typed_artist_outranks_its_collaborations() {
        let mut hits = Hits {
            artists: vec![
                artist_hit("Skrillex & Damian Marley"),
                artist_hit("Skrillexia"),
                artist_hit("Boys Noize & Skrillex"),
                artist_hit("Skrillex"),
            ],
            ..Default::default()
        };
        hits.rank("skrillex");
        let names: Vec<_> = hits.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Skrillex",
                "Skrillex & Damian Marley",
                "Skrillexia",
                "Boys Noize & Skrillex",
            ]
        );
    }

    #[test]
    fn ranking_ignores_case_and_surrounding_space() {
        assert_eq!(match_rank(" SKRILLEX ", "skrillex"), Some(0));
    }

    #[test]
    fn words_may_be_out_of_order_but_word_starts_rank_higher() {
        // Every word present, each starting a word of the title.
        assert_eq!(
            match_rank("moon dark", "The Dark Side of the Moon"),
            Some(3)
        );
        // Present, but mid-word.
        assert_eq!(match_rank("oon ark", "The Dark Side of the Moon"), Some(4));
        assert_eq!(
            match_rank("dark moon rain", "The Dark Side of the Moon"),
            None
        );
    }

    #[test]
    fn a_title_match_outranks_an_artist_one() {
        let mut hits = Hits {
            albums: vec![
                album_hit("Greatest Hits", "Queen", false),
                album_hit("Queen of Denmark", "John Grant", false),
            ],
            ..Default::default()
        };
        hits.rank("queen");
        assert_eq!(hits.albums[0].title, "Queen of Denmark");
    }

    #[test]
    fn merging_keeps_the_rows_already_shown() {
        let mut hits = Hits {
            albums: vec![album_hit("Kid A", "Radiohead", false)],
            ..Default::default()
        };
        hits.merge(Hits {
            albums: vec![
                album_hit("Kid A", "Radiohead", false),
                album_hit("Amnesiac", "Radiohead", false),
            ],
            ..Default::default()
        });
        let titles: Vec<_> = hits.albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, ["Kid A", "Amnesiac"]);
    }

    #[test]
    fn a_local_album_and_its_server_twin_are_both_kept() {
        // Same record, two sources: opening one is not opening the other, so
        // the id alone cannot dedupe them.
        let mut hits = Hits {
            albums: vec![album_hit("Kid A", "Radiohead", true)],
            ..Default::default()
        };
        hits.merge(Hits {
            albums: vec![album_hit("Kid A", "Radiohead", false)],
            ..Default::default()
        });
        assert_eq!(hits.albums.len(), 2);
    }

    fn cached_rows() -> CatalogSearch {
        let mut remote = AlbumRow::new("navidrome:album:al-remote", "navidrome", "Remote Album");
        remote.library_id = Some("1".into());
        let mut other_library =
            AlbumRow::new("navidrome:album:al-other", "navidrome", "Other Album");
        other_library.library_id = Some("2".into());
        CatalogSearch {
            artists: vec![
                ArtistRow {
                    id: "ar-local".into(),
                    source: "local".into(),
                    name: "Local Artist".into(),
                    cover_art: None,
                    library_id: None,
                },
                ArtistRow {
                    id: "navidrome:artist:ar-remote".into(),
                    source: "navidrome".into(),
                    name: "Remote Artist".into(),
                    cover_art: None,
                    library_id: Some("1".into()),
                },
            ],
            albums: vec![
                AlbumRow::new("al-local", "local", "Local Album"),
                remote,
                other_library,
            ],
            tracks: vec![
                TrackRow {
                    id: "t1".into(),
                    source: "local".into(),
                    title: "Local Song".into(),
                    artist: None,
                    album: None,
                    duration: None,
                    local_path: Some("/music/a.flac".into()),
                    cover_art: None,
                    track_no: None,
                    file_modified: None,
                    album_id: Some("local:album:x".into()),
                },
                TrackRow {
                    id: "navidrome:track:t2".into(),
                    source: "navidrome".into(),
                    title: "Remote Song".into(),
                    artist: None,
                    album: None,
                    duration: None,
                    local_path: None,
                    cover_art: Some("mf-t2_abc".into()),
                    track_no: None,
                    file_modified: None,
                    album_id: Some("navidrome:album:al-remote".into()),
                },
            ],
        }
    }

    #[test]
    fn cached_rows_become_hits_without_unopenable_local_artists() {
        let hits = hits_from_cache(cached_rows(), &[]);
        // The local artist is dropped: there is no local artist page to open.
        assert_eq!(hits.artists.len(), 1);
        assert_eq!(hits.artists[0].id, "ar-remote");
        assert_eq!(hits.albums.len(), 3);
        assert!(hits.albums.iter().any(|a| a.local && a.id == "al-local"));
        assert!(hits.songs[0].local);
        assert_eq!(
            hits.songs[0].song.local_path.as_deref(),
            Some("/music/a.flac")
        );
    }

    #[test]
    fn cached_server_ids_lose_the_syncs_namespace() {
        // Namespaced, these are ids the server has never heard of: the album
        // would not open, the track would not stream, and neither would dedupe
        // against the same row coming back from `search3`.
        let hits = hits_from_cache(cached_rows(), &[]);
        assert_eq!(hits.artists[0].id, "ar-remote");
        assert!(hits.albums.iter().any(|a| a.id == "al-remote"));
        let remote = hits.songs.iter().find(|s| !s.local).unwrap();
        assert_eq!(remote.song.id, "t2");
        // The cover is keyed by album, so one download serves every hit off
        // that record — and the album page has usually cached it already.
        assert_eq!(
            remote.cover.as_ref().map(|c| c.key.as_str()),
            Some("album-al-remote")
        );
    }

    #[test]
    fn a_library_subset_hides_other_libraries_but_keeps_local_files() {
        let hits = hits_from_cache(cached_rows(), &["1".to_string()]);
        let ids: Vec<_> = hits.albums.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["al-local", "al-remote"]);
    }
}
