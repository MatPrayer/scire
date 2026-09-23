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
    Context, ElementId, Entity, EventEmitter, Focusable as _, IntoElement, KeyDownEvent, Render,
    ScrollHandle, Stateful, Window, div, img, prelude::*, px,
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

/// The whole sort key for a row: its tier, then the shorter text, then the
/// text itself.
///
/// The tier on its own leaves a collaboration credited as its own artist tied
/// with the plain one — "skrill" is a prefix of both "Skrillex" and "Skrillex
/// & Damian Marley", and only the exact and word-boundary tiers tell those
/// apart, which a half-typed query never reaches. A tie then fell to whatever
/// order the source handed the rows over in, which for the cache is
/// alphabetical and for the server is its own relevance, and the featured
/// credit landed above the artist often enough to be the complaint.
///
/// Length is what separates them: of two texts answering the query equally
/// well, the one carrying less *around* the match is the one that is the
/// answer, and everything longer is that answer plus somebody else. The final
/// field only makes the order total, so it does not depend on the source's.
/// Shared with the full-page search, which offers the same ordering under
/// the name "Relevance".
pub(crate) fn sort_key(query: &str, primary: &str, secondary: Option<&str>) -> (u8, usize, String) {
    let text = primary.trim().to_lowercase();
    (score(query, primary, secondary), text.chars().count(), text)
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

// Every variant opens something; the shared verb is the point of the enum.
#[allow(clippy::enum_variant_names)]
pub enum SearchBarEvent {
    OpenAlbum(String),
    /// Hand this query to the full-page search, which lists every match rather
    /// than the few per section the palette has room for.
    OpenAdvancedSearch(String),
    OpenLocalAlbum(String),
    OpenArtist(String),
    OpenLocalArtist(String),
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

/// Everything one query found, split by source and already deduped.
#[derive(Default)]
struct Hits {
    artists: Vec<ArtistHit>,
    albums: Vec<AlbumHit>,
    songs: Vec<SongHit>,
    local_artists: Vec<ArtistHit>,
    local_albums: Vec<AlbumHit>,
    local_songs: Vec<SongHit>,
}

impl Hits {
    fn is_empty(&self) -> bool {
        self.artists.is_empty()
            && self.albums.is_empty()
            && self.songs.is_empty()
            && self.local_artists.is_empty()
            && self.local_albums.is_empty()
            && self.local_songs.is_empty()
    }

    /// Append what `other` holds and this does not. What is already shown keeps
    /// its place: the cache paints first, and a server response arriving 200ms
    /// later must not reshuffle the row the user is reaching for — a row that
    /// moves between the press and the release of a click is a click that
    /// lands on a different track than the one that was aimed at, which is the
    /// whole of "clicking a search result sometimes does nothing".
    ///
    /// A duplicate is not discarded outright, though: the two sources do not
    /// carry the same fields. A cache row has whatever the last sync wrote and
    /// the server's has whatever the server knows, so a cover the row is
    /// missing is taken from its twin rather than the row being left with a
    /// placeholder for a cover that did arrive.
    fn merge(&mut self, other: Hits) {
        merge_artists(&mut self.artists, other.artists);
        merge_albums(&mut self.albums, other.albums);
        merge_songs(&mut self.songs, other.songs);
        merge_artists(&mut self.local_artists, other.local_artists);
        merge_albums(&mut self.local_albums, other.local_albums);
        merge_songs(&mut self.local_songs, other.local_songs);
    }

    /// Put the rows that answer the query at the top of each section, closest
    /// match first. See [`sort_key`] for what "closest" means.
    ///
    /// Only ever run on a freshly built set, never on one already on screen:
    /// re-sorting rows the user can see is exactly what [`merge`] refuses to
    /// do, and doing it a step later is the same reshuffle.
    ///
    /// [`merge`]: Self::merge
    fn rank(&mut self, query: &str) {
        self.artists.sort_by_key(|a| sort_key(query, &a.name, None));
        self.albums
            .sort_by_key(|a| sort_key(query, &a.title, a.artist.as_deref()));
        self.songs
            .sort_by_key(|s| sort_key(query, &s.song.title, s.song.artist.as_deref()));
        self.local_artists
            .sort_by_key(|a| sort_key(query, &a.name, None));
        self.local_albums
            .sort_by_key(|a| sort_key(query, &a.title, a.artist.as_deref()));
        self.local_songs
            .sort_by_key(|s| sort_key(query, &s.song.title, s.song.artist.as_deref()));
    }
}

fn merge_artists(current: &mut Vec<ArtistHit>, other: Vec<ArtistHit>) {
    let mut seen: HashMap<String, usize> = current
        .iter()
        .enumerate()
        .map(|(i, artist)| (artist.id.clone(), i))
        .collect();
    for artist in other {
        match seen.get(&artist.id) {
            Some(&i) => adopt(&mut current[i].cover, artist.cover),
            None => {
                seen.insert(artist.id.clone(), current.len());
                current.push(artist);
            }
        }
    }
}

fn merge_albums(current: &mut Vec<AlbumHit>, other: Vec<AlbumHit>) {
    let mut seen: HashMap<(bool, String), usize> = current
        .iter()
        .enumerate()
        .map(|(i, album)| ((album.local, album.id.clone()), i))
        .collect();
    for album in other {
        let key = (album.local, album.id.clone());
        match seen.get(&key) {
            Some(&i) => adopt(&mut current[i].cover, album.cover),
            None => {
                seen.insert(key, current.len());
                current.push(album);
            }
        }
    }
}

fn merge_songs(current: &mut Vec<SongHit>, other: Vec<SongHit>) {
    let mut seen: HashMap<(bool, String), usize> = current
        .iter()
        .enumerate()
        .map(|(i, song)| ((song.local, song.song.id.clone()), i))
        .collect();
    for song in other {
        let key = (song.local, song.song.id.clone());
        match seen.get(&key) {
            Some(&i) => adopt(&mut current[i].cover, song.cover),
            None => {
                seen.insert(key, current.len());
                current.push(song);
            }
        }
    }
}

/// Fill a row's cover from its twin in the other source, if it has none.
fn adopt(cover: &mut Option<Cover>, other: Option<Cover>) {
    if cover.is_none() {
        *cover = other;
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
fn hits_from_cache(found: CatalogSearch, libraries: &[String]) -> Hits {
    let in_selection = |source: &str, library_id: &Option<String>| {
        // A subset is only meaningful for server rows, and a row synced before
        // schema v3 has no library at all — dropping those would make a search
        // quietly miss part of the library until the next sync.
        source != "navidrome"
            || libraries.is_empty()
            || library_id.as_ref().is_none_or(|id| libraries.contains(id))
    };

    let mut hits = Hits::default();
    for artist in found.artists {
        if !in_selection(&artist.source, &artist.library_id) {
            continue;
        }
        let local = artist.source == "local";
        let hit = ArtistHit {
            cover: artist.cover_art.map(|id| Cover {
                key: if local {
                    format!("local:{id}")
                } else {
                    id.clone()
                },
                id,
                local,
            }),
            id: if local {
                artist.id
            } else {
                strip_ns(artist.id, "navidrome:artist:")
            },
            name: artist.name,
        };
        if local {
            hits.local_artists.push(hit);
        } else {
            hits.artists.push(hit);
        }
    }

    for album in found.albums {
        if !in_selection(&album.source, &album.library_id) {
            continue;
        }
        let local = album.source == "local";
        let hit = AlbumHit {
            cover: album.cover_art.map(|id| Cover {
                key: if local {
                    format!("local:{id}")
                } else {
                    id.clone()
                },
                id,
                local,
            }),
            id: if local {
                album.id
            } else {
                strip_ns(album.id, "navidrome:album:")
            },
            title: album.title,
            artist: album.artist,
            local,
        };
        if local {
            hits.local_albums.push(hit);
        } else {
            hits.albums.push(hit);
        }
    }

    for track in found.tracks {
        if !in_selection(&track.source, &None) {
            continue;
        }
        let local = track.source == "local";
        let mut song = track.into_song();
        if !local {
            song.id = strip_ns(song.id, "navidrome:track:");
            // Stripped too, so the art lands on the key the album pages
            // already cache the very same cover under.
            song.album_id = song.album_id.map(|id| strip_ns(id, "navidrome:album:"));
        }
        let hit = SongHit {
            cover: song_cover(&song, local),
            song,
            local,
        };
        if local {
            hits.local_songs.push(hit);
        } else {
            hits.songs.push(hit);
        }
    }
    hits
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
        ..Default::default()
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
    LocalArtist(String),
    Album {
        id: String,
        local: bool,
    },
    Song(Box<Song>),
    /// The footer row. It sits in this list rather than beside it so the
    /// arrow keys reach it: a row that can only be clicked is a dead end for
    /// anyone who opened the palette with a shortcut and never left the
    /// keyboard.
    Advanced,
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
    /// Open/close travel of the palette. Set from `open_palette`/`dismiss`
    /// rather than from `render`, because root draws the backdrop and reads
    /// this in *its* render, which runs before ours.
    reveal: crate::ui::Reveal,
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
            reveal: crate::ui::Reveal::new(150, 110),
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

    /// How far the palette's open/close travel has got, or `None` once it is
    /// gone. Root draws the backdrop behind the box, so it has to be told the
    /// palette is still leaving rather than reading [`is_palette`] and
    /// pulling the backdrop out from under the exit.
    ///
    /// [`is_palette`]: Self::is_palette
    pub fn palette_reveal(&self, cx: &gpui::App) -> Option<f32> {
        let reduced_motion = self.session.read(cx).settings.reduced_motion;
        self.reveal
            .visible(reduced_motion)
            .then(|| self.reveal.openness(reduced_motion))
    }

    /// Whether that travel still needs frames.
    pub fn palette_settling(&self, cx: &gpui::App) -> bool {
        self.reveal
            .settling(self.session.read(cx).settings.reduced_motion)
    }

    /// Open the centered command palette (Ctrl/Cmd+K) with a fresh query.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = true;
        self.reveal
            .set(true, self.session.read(cx).settings.reduced_motion);
        self.selected = 0;
        self.results = Hits::default();
        self.pending = false;
        self.searching = false;
        self.error = None;
        self.open = false;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    /// Close the dropdown/palette (root's Escape handler).
    ///
    /// The query and its results are deliberately *not* cleared here: the box
    /// is still on screen for the length of its exit, and a palette that
    /// empties itself back to "Type to search…" on the way out reads as the
    /// search being lost rather than as the palette closing. `open_palette`
    /// resets all of it, so the next one still opens fresh.
    pub fn dismiss(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.palette = false;
        self.reveal
            .set(false, self.session.read(cx).settings.reduced_motion);
        self.selected = 0;
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
        for a in self.results.local_artists.iter().take(MAX_ARTISTS) {
            v.push(PaletteItem::LocalArtist(a.id.clone()));
        }
        for a in self.results.local_albums.iter().take(MAX_ALBUMS) {
            v.push(PaletteItem::Album {
                id: a.id.clone(),
                local: true,
            });
        }
        for s in self.results.local_songs.iter().take(MAX_SONGS) {
            v.push(PaletteItem::Song(Box::new(s.song.clone())));
        }
        // Last, because it is where the list runs out: Down from the bottom
        // result lands on it, which is the order the page is reached for in.
        v.push(PaletteItem::Advanced);
        v
    }

    /// Index of the footer row in [`items`], for the highlight.
    ///
    /// [`items`]: Self::items
    fn advanced_index(&self) -> usize {
        self.items().len().saturating_sub(1)
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
            self.results.local_artists.len().min(MAX_ARTISTS),
            self.results.local_albums.len().min(MAX_ALBUMS),
            self.results.local_songs.len().min(MAX_SONGS),
        ];
        let mut child = 0;
        let mut item = 0;
        let mut local_title = false;
        for (group, n) in counts.into_iter().enumerate() {
            if n == 0 {
                continue;
            }
            if group < 3 || !local_title {
                child += 1; // server section title, or shared local title
                local_title |= group >= 3;
            }
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
            PaletteItem::LocalArtist(id) => cx.emit(SearchBarEvent::OpenLocalArtist(id)),
            PaletteItem::Album { id, local: true } => cx.emit(SearchBarEvent::OpenLocalAlbum(id)),
            PaletteItem::Album { id, local: false } => cx.emit(SearchBarEvent::OpenAlbum(id)),
            PaletteItem::Song(song) => {
                self.player
                    .update(cx, |p, cx| p.play_queue(vec![*song], 0, cx));
            }
            PaletteItem::Advanced => {
                let query = self.input.read(cx).value().trim().to_string();
                cx.emit(SearchBarEvent::OpenAdvancedSearch(query));
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
                        let query = bar.input.read(cx).value().trim().to_string();
                        let mut hits = hits_from_server(r);
                        // Ranked before the merge, never after: `merge` appends
                        // so the rows already on screen hold still, and a rank
                        // over the joined list would move them anyway.
                        hits.rank(&query);
                        bar.fetch_result_art(&hits, cx);
                        bar.results.merge(hits);
                        bar.error = None;
                    }
                    // A dead server is not a dead search — the cache's rows
                    // stay up, and the error explains what is missing.
                    Err(e) => bar.error = Some(crate::errors::error_text(&e)),
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
            )
            .chain(
                hits.local_songs
                    .iter()
                    .take(MAX_SONGS)
                    .filter_map(|s| s.cover.clone()),
            )
            .chain(
                hits.local_albums
                    .iter()
                    .take(MAX_ALBUMS)
                    .filter_map(|a| a.cover.clone()),
            )
            .chain(
                hits.local_artists
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

    /// A row's element id, keyed by what the row *is* rather than by where it
    /// sits.
    ///
    /// gpui matches a press to its release through the element id, so a row
    /// identified by its index is a different row the moment the list changes
    /// under the cursor — and it does, 300ms after the last keystroke, when
    /// the server's answer merges into the cache's. Clicks that landed on
    /// nothing and clicks that played the neighbouring track were both this.
    /// The source is in the id too, since a local file and a server track can
    /// carry the same id.
    fn row_id(prefix: &str, local: bool, id: &str) -> ElementId {
        let source = if local { 'l' } else { 'r' };
        ElementId::from(gpui::SharedString::from(format!("{prefix}-{source}-{id}")))
    }

    /// Build the result rows shared by the inline dropdown and the palette.
    /// The flat selectable index is threaded so the highlighted row matches
    /// `selected`; section titles do not advance it.
    fn result_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut idx = 0usize;

        if !self.results.artists.is_empty() {
            rows.push(Self::section_title("Artists", cx));
            for artist in self.results.artists.iter().take(MAX_ARTISTS) {
                let id = artist.id.clone();
                rows.push(
                    self.row_shell(
                        Self::row_id("sb-artist", false, &artist.id),
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
            for album in self.results.albums.iter().take(MAX_ALBUMS) {
                let id = album.id.clone();
                rows.push(
                    self.row_shell(
                        Self::row_id("sb-album", album.local, &album.id),
                        idx,
                        album.cover.as_ref(),
                        IconName::LayoutDashboard,
                        (
                            album.title.clone(),
                            Some(album.artist.clone().unwrap_or_default()),
                        ),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.emit(SearchBarEvent::OpenAlbum(id.clone()));
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
            for hit in self.results.songs.iter().take(MAX_SONGS) {
                let play = hit.song.clone();
                let enqueue = hit.song.clone();
                rows.push(
                    self.row_shell(
                        Self::row_id("sb-song", hit.local, &hit.song.id),
                        idx,
                        hit.cover.as_ref(),
                        IconName::Star,
                        (
                            hit.song.title.clone(),
                            Some(hit.song.artist.clone().unwrap_or_default()),
                        ),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.player.update(cx, |p, cx| {
                            p.play_queue(vec![play.clone()], 0, cx);
                        });
                        this.dismiss(window, cx);
                    }))
                    .child(
                        Button::new(Self::row_id("sb-enq", hit.local, &hit.song.id))
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

        if !self.results.local_artists.is_empty()
            || !self.results.local_albums.is_empty()
            || !self.results.local_songs.is_empty()
        {
            rows.push(Self::section_title("Local music", cx));
        }

        for artist in self.results.local_artists.iter().take(MAX_ARTISTS) {
            let id = artist.id.clone();
            rows.push(
                self.row_shell(
                    Self::row_id("sb-local-artist", true, &artist.id),
                    idx,
                    artist.cover.as_ref(),
                    IconName::CircleUser,
                    (artist.name.clone(), None),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.emit(SearchBarEvent::OpenLocalArtist(id.clone()));
                    this.dismiss(window, cx);
                }))
                .into_any_element(),
            );
            idx += 1;
        }

        for album in self.results.local_albums.iter().take(MAX_ALBUMS) {
            let id = album.id.clone();
            rows.push(
                self.row_shell(
                    Self::row_id("sb-local-album", true, &album.id),
                    idx,
                    album.cover.as_ref(),
                    IconName::LayoutDashboard,
                    (
                        album.title.clone(),
                        Some(album.artist.clone().unwrap_or_default()),
                    ),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.emit(SearchBarEvent::OpenLocalAlbum(id.clone()));
                    this.dismiss(window, cx);
                }))
                .into_any_element(),
            );
            idx += 1;
        }

        for hit in self.results.local_songs.iter().take(MAX_SONGS) {
            let play = hit.song.clone();
            let enqueue = hit.song.clone();
            rows.push(
                self.row_shell(
                    Self::row_id("sb-local-song", true, &hit.song.id),
                    idx,
                    hit.cover.as_ref(),
                    IconName::Star,
                    (
                        hit.song.title.clone(),
                        Some(hit.song.artist.clone().unwrap_or_default()),
                    ),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.player.update(cx, |p, cx| {
                        p.play_queue(vec![play.clone()], 0, cx);
                    });
                    this.dismiss(window, cx);
                }))
                .child(
                    Button::new(Self::row_id("sb-local-enq", true, &hit.song.id))
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
    fn render_palette(&self, fade: f32, cx: &mut Context<Self>) -> gpui::AnyElement {
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
            // The way out of the palette's caps. It is always offered, not only
            // when the results look truncated: the page is also where the
            // filters are, and "every album I have a FLAC of" is a question the
            // palette cannot be asked at all.
            .child(
                h_flex()
                    .id("palette-advanced")
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    // Same highlight the result rows carry, since arrow keys
                    // walk onto it like any other row.
                    .when(self.row_selected(self.advanced_index()), |s| {
                        s.bg(cx.theme().muted)
                            .border_l_2()
                            .border_color(cx.theme().primary)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        let query = this.input.read(cx).value().trim().to_string();
                        cx.emit(SearchBarEvent::OpenAdvancedSearch(query));
                        this.dismiss(window, cx);
                    }))
                    .child(Icon::new(IconName::Settings2).size_3())
                    .child(div().flex_1().child("Advanced search")),
            )
            // Fade off the reveal's clock rather than a `with_animation`
            // wrapper, so it plays on the way out too — an element dropped
            // from the tree the moment it closes animates nothing.
            .opacity(fade)
            .into_any_element()
    }
}

impl Render for SearchBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(fade) = self.palette_reveal(cx) {
            if self.palette_settling(cx) {
                window.request_animation_frame();
            }
            // The centered box only; root supplies the full-window backdrop.
            return self.render_palette(fade, cx);
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
    fn the_shorter_of_two_equal_matches_comes_first() {
        // Half-typed query: every one of these is a plain prefix match, so the
        // tier cannot separate them and only length can.
        let mut hits = Hits {
            artists: vec![
                artist_hit("Skrillex, Diplo & Justin Bieber"),
                artist_hit("Skrillexia"),
                artist_hit("Skrillex"),
            ],
            ..Default::default()
        };
        hits.rank("skrill");
        let names: Vec<_> = hits.artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["Skrillex", "Skrillexia", "Skrillex, Diplo & Justin Bieber",]
        );
    }

    #[test]
    fn equal_rows_order_the_same_whichever_way_round_they_arrive() {
        // The tail of the key is the text itself, so the result does not
        // depend on the order the cache or the server handed the rows over in.
        let names = |mut hits: Hits| {
            hits.rank("hits");
            hits.artists
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
        };
        let forward = names(Hits {
            artists: vec![artist_hit("Hits B"), artist_hit("Hits A")],
            ..Default::default()
        });
        let backward = names(Hits {
            artists: vec![artist_hit("Hits A"), artist_hit("Hits B")],
            ..Default::default()
        });
        assert_eq!(forward, backward);
    }

    #[test]
    fn a_shown_row_takes_the_cover_its_twin_brought() {
        // The cache has no cover for an artist until a sync has recorded one;
        // the server's answer does. Dropping the duplicate outright left the
        // row with a placeholder for art that had arrived.
        let mut hits = Hits {
            artists: vec![artist_hit("Radiohead")],
            ..Default::default()
        };
        let mut with_cover = artist_hit("Radiohead");
        with_cover.cover = Some(Cover {
            id: "ar-1".into(),
            key: "ar-1".into(),
            local: false,
        });
        hits.merge(Hits {
            artists: vec![with_cover],
            ..Default::default()
        });
        assert_eq!(hits.artists.len(), 1);
        assert_eq!(
            hits.artists[0].cover.as_ref().map(|c| c.id.as_str()),
            Some("ar-1")
        );
    }

    #[test]
    fn local_and_server_twins_stay_in_their_own_groups() {
        let mut hits = Hits {
            local_albums: vec![album_hit("Kid A", "Radiohead", true)],
            ..Default::default()
        };
        hits.merge(Hits {
            albums: vec![album_hit("Kid A", "Radiohead", false)],
            ..Default::default()
        });
        assert_eq!(hits.albums.len(), 1);
        assert_eq!(hits.local_albums.len(), 1);
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
                    ..TrackRow::default()
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
                    ..TrackRow::default()
                },
            ],
        }
    }

    #[test]
    fn cached_rows_split_into_server_and_local_groups() {
        let hits = hits_from_cache(cached_rows(), &[]);
        assert_eq!(hits.artists.len(), 1);
        assert_eq!(hits.artists[0].id, "ar-remote");
        assert_eq!(hits.local_artists.len(), 1);
        assert_eq!(hits.local_artists[0].id, "ar-local");
        assert_eq!(hits.albums.len(), 2);
        assert_eq!(hits.local_albums.len(), 1);
        assert_eq!(hits.local_albums[0].id, "al-local");
        assert!(hits.local_songs[0].local);
        assert_eq!(
            hits.local_songs[0].song.local_path.as_deref(),
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
        assert_eq!(ids, ["al-remote"]);
        assert_eq!(hits.local_albums[0].id, "al-local");
    }

    #[test]
    fn local_groups_rank_independently() {
        let mut hits = Hits {
            local_artists: vec![artist_hit("Local Hits Extra"), artist_hit("Local Hits")],
            ..Default::default()
        };
        hits.rank("local hits");
        assert_eq!(hits.local_artists[0].name, "Local Hits");
    }
}
