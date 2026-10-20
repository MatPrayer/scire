//! The full-page search.
//!
//! The palette (`ui::search_bar`) and this page answer two different questions.
//! The palette is for the thing you are reaching for and already know the name
//! of: a few rows per section, ranked hard, dismissed the moment you pick one.
//! This is for the thing you are *looking* for — every row that matches, one
//! kind at a time so that a year, a genre or a format can mean something, and
//! no cap beyond the one that stops an empty query materialising a whole
//! library.
//!
//! It is cache-only; see `services::advanced_search` for why. The practical
//! consequence is that it works with no server at all and answers in a few
//! milliseconds, so the query is debounced only lightly and every filter change
//! re-runs it immediately.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, ElementId, Entity, EventEmitter, Focusable as _, Global, IntoElement,
    ListAlignment, ListState, Render, SharedString, Stateful, Window, div, img, list, prelude::*,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::config::AlbumCardStyle;
use crate::services::advanced_search::{
    AdvancedResults, Facets, Filters, SearchKind, SortBy, SourceFilter,
};
use crate::services::album_info::Description;
use crate::services::library_db::{LibraryDb, SOURCE_LOCAL, TrackRow};
use crate::services::local_library::local_art_path;
use crate::services::{artwork, genre_info, runtime};
use crate::state::lidarr::lidarr as lidarr_state;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::lidarr_add::{self, LidarrHit};
use crate::ui::search_bar::{sort_key, strip_ns};
use crate::ui::{LiveWidth, format_count, format_duration, format_playtime, with_focus_cursor};

/// Track thumbnail size. Track rows are list-sized rather than card-sized, so
/// this is the palette's rung rather than the grid's.
const ART_SIZE: u32 = 200;
/// A track row's height.
const ROW_H: f32 = 56.;
/// Lines either side of the viewport whose art is fetched ahead of being drawn.
const ART_LOOKAHEAD: usize = 4;
/// Columns guessed on the first frame, before the list has been measured.
const FALLBACK_COLS: usize = 5;
/// How far past the viewport the list lays lines out.
const LIST_OVERDRAW: f32 = 600.;
/// Card text metrics, the album and artist grids' own, so a card here is the
/// card there.
const NAME_LINE_H: f32 = 20.;
const META_LINE_H: f32 = 17.;
const ALBUM_TEXT_H: f32 = NAME_LINE_H * 2. + META_LINE_H * 2.;
const ARTIST_TEXT_H: f32 = NAME_LINE_H * 2. + META_LINE_H;
/// Wait after a keystroke before querying. Shorter than the palette's cache
/// debounce would be wasteful — this query can touch the whole track table —
/// and longer would be felt.
const DEBOUNCE: Duration = Duration::from_millis(120);
/// Column widths, fixed so they line up down the page.
const SECONDARY_W: f32 = 200.;
const TRAILING_W: f32 = 64.;
/// Lidarr hits listed per kind (artists, albums).
const MAX_LIDARR: usize = 25;
/// Lidarr row thumbnail edge.
const LIDARR_THUMB: f32 = 40.;

/// Length bands offered instead of a free-text duration, which is two more text
/// fields for a filter nobody expresses in seconds.
const DURATION_BANDS: [(&str, Option<f64>, Option<f64>); 5] = [
    ("Any length", None, None),
    ("Under 2 min", None, Some(120.)),
    ("2 – 5 min", Some(120.), Some(300.)),
    ("5 – 10 min", Some(300.), Some(600.)),
    ("Over 10 min", Some(600.), None),
];

/// Bitrate floors worth offering. Anything finer is a distinction the tags do
/// not reliably carry.
const BITRATES: [(&str, Option<i64>); 5] = [
    ("Any bitrate", None),
    ("128 kbps +", Some(128)),
    ("192 kbps +", Some(192)),
    ("256 kbps +", Some(256)),
    ("320 kbps +", Some(320)),
];

// The three variants are three destinations, not three kinds of one thing;
// the shared prefix is what the root's match arm reads by.
#[allow(clippy::enum_variant_names)]
pub enum AdvancedSearchEvent {
    OpenAlbum(String),
    OpenLocalAlbum(String),
    OpenArtist(String),
    OpenLidarr(LidarrHit),
}

impl EventEmitter<AdvancedSearchEvent> for AdvancedSearchView {}

/// Any view asks for this page narrowed to one genre — its albums, or its
/// artists from an artist page — through this global, the way `song_info::SongInfoRequest` asks for the details dialog:
/// a genre chip's click handler has an `App` and no window, and building the
/// page needs one, so `RootView` observes it with `observe_global_in`.
#[derive(Default)]
pub struct GenreSearchRequest(pub Option<(String, SearchKind)>);

impl Global for GenreSearchRequest {}

/// Open the search page listing the `kind` rows tagged `genre`.
pub fn open_genre(genre: impl Into<String>, kind: SearchKind, cx: &mut App) {
    cx.set_global(GenreSearchRequest(Some((genre.into(), kind))));
}

/// A genre as a clickable chip — the muted chip album pages already draw,
/// which opens the search page on that genre's `kind` rows (albums from an
/// album page, artists from an artist page).
pub fn genre_chip(
    id: impl Into<ElementId>,
    genre: &str,
    kind: SearchKind,
    cx: &App,
) -> Stateful<gpui::Div> {
    let name = genre.to_string();
    genre_tag(id, genre, kind, cx).on_click(move |_, _, cx| open_genre(name.clone(), kind, cx))
}

/// [`genre_chip`]'s look without its click, for a caller that routes the
/// click itself (the song details dialog closes first).
pub fn genre_tag(
    id: impl Into<ElementId>,
    genre: &str,
    kind: SearchKind,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(theme.muted)
        .text_xs()
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover({
            let bg = theme.secondary_hover;
            move |s| s.bg(bg)
        })
        .active({
            let bg = theme.secondary_active;
            move |s| s.bg(bg)
        })
        .tooltip({
            let noun = match kind {
                SearchKind::Artists => "Artists",
                SearchKind::Tracks => "Tracks",
                SearchKind::Albums | SearchKind::All => "Albums",
            };
            let tip: SharedString = format!("{noun} tagged {genre}").into();
            move |window, cx| gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .child(genre.to_string())
}

/// Undoes one filter chip's filter.
type ResetFilter = fn(&mut Filters);

/// One removable chip per active filter, in the order the dropdowns sit:
/// the label it shows and how removing it resets the filter.
fn filter_chips(f: &Filters) -> Vec<(String, ResetFilter)> {
    let mut out: Vec<(String, ResetFilter)> = Vec::new();
    if let Some(g) = &f.genre {
        out.push((format!("Genre: {g}"), |f| f.genre = None));
    }
    let years = match (f.year_min, f.year_max) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), Some(b)) => Some(format!("{a} – {b}")),
        (Some(a), None) => Some(format!("From {a}")),
        (None, Some(b)) => Some(format!("Until {b}")),
        (None, None) => None,
    };
    if let Some(years) = years {
        out.push((years, |f| {
            f.year_min = None;
            f.year_max = None;
        }));
    }
    if f.duration_min.is_some() || f.duration_max.is_some() {
        let label = DURATION_BANDS
            .iter()
            .find(|(_, min, max)| *min == f.duration_min && *max == f.duration_max)
            .map(|(l, _, _)| l.to_string())
            .unwrap_or_else(|| "Length".into());
        out.push((label, |f| {
            f.duration_min = None;
            f.duration_max = None;
        }));
    }
    if f.source != SourceFilter::All {
        out.push((f.source.label().to_string(), |f| {
            f.source = SourceFilter::All
        }));
    }
    if let Some(format) = &f.format {
        out.push((format.to_uppercase(), |f| f.format = None));
    }
    if let Some(b) = f.bitrate_min {
        out.push((format!("{b} kbps +"), |f| f.bitrate_min = None));
    }
    if f.starred_only {
        out.push(("Starred".into(), |f| f.starred_only = false));
    }
    out
}

/// One result row, pre-formatted.
///
/// Built when the results change rather than per frame, for the same reason
/// `recent.rs` does it: the row text is derived from a dozen `Option`s and the
/// page can hold thousands of them.
struct Row {
    /// The id the destination page takes: bare for a server row, namespaced
    /// for a local one — what the palette hands over too.
    id: String,
    /// The cache's own key for the row, for asking the cache about it again.
    db_id: String,
    /// What this row *is*. Under `SearchKind::All` one list holds all three,
    /// so the row carries its own kind rather than the page's.
    kind: SearchKind,
    /// Position within `results`' own vec for this kind — not the row index,
    /// which spans the three lists. What playing a track queues behind it is
    /// indexed by this.
    index: usize,
    /// Title, or an artist's name.
    primary: SharedString,
    /// Artist, or a track's album.
    secondary: SharedString,
    /// Year, or a track's length.
    trailing: SharedString,
    /// Cover id to request and the album-scoped cache key to store it under.
    cover: Option<(String, String)>,
    local: bool,
}

/// Summary line for the header: what the page is showing, and how much of it.
fn summarize(results: &AdvancedResults, kind: SearchKind) -> String {
    let n = results.len();
    if n == 0 {
        return String::new();
    }
    // Three lists in one page, so the count is broken out by kind: "8 artists ·
    // 30 albums · 412 tracks" says what is below, where "450 results" does not.
    if kind == SearchKind::All {
        let parts: Vec<String> = [
            (results.artists.len(), "artist"),
            (results.albums.len(), "album"),
            (results.tracks.len(), "track"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, noun)| {
            let plural = if n == 1 { "" } else { "s" };
            format!("{} {noun}{plural}", format_count(n as i64))
        })
        .collect();
        let mut out = parts.join(" · ");
        if results.truncated {
            out.push_str(" · showing the first matches");
        }
        return out;
    }
    let noun = match kind {
        // `All` returned above; it shares the album arms so the match is total
        // without a panic in a formatting function.
        SearchKind::All | SearchKind::Albums => {
            if n == 1 {
                "album"
            } else {
                "albums"
            }
        }
        SearchKind::Artists => {
            if n == 1 {
                "artist"
            } else {
                "artists"
            }
        }
        SearchKind::Tracks => {
            if n == 1 {
                "track"
            } else {
                "tracks"
            }
        }
    };
    let mut out = format!("{} {noun}", format_count(n as i64));
    // Artists carry no duration, so a playtime beside them would be a zero.
    let secs: f64 = match kind {
        SearchKind::All | SearchKind::Albums => results.albums.iter().map(|a| a.duration).sum(),
        SearchKind::Tracks => results.tracks.iter().filter_map(|t| t.duration).sum(),
        SearchKind::Artists => 0.,
    };
    if secs > 0. {
        out.push_str(" · ");
        out.push_str(&format_playtime(secs));
    }
    if results.truncated {
        out.push_str(" · showing the first matches");
    }
    out
}

fn year_text(year: Option<i32>) -> SharedString {
    year.filter(|y| *y > 0)
        .map(|y| y.to_string())
        .unwrap_or_default()
        .into()
}

/// Order the rows by how well they answer the query, using the palette's own
/// ranking so the two searches agree about what "best match" means.
///
/// SQL cannot express it — the tiers are about where in the text the words
/// landed — so `SortBy::Relevance` comes back in the cheap alphabetical order
/// the statement asked for and is reordered here. It is applied to the
/// **result set** rather than to the pre-formatted rows, since the rows are
/// indexed into `results` by position (the track column, and what playing one
/// queues behind it) and reordering one without the other would play the wrong
/// track.
///
/// An empty query leaves the order alone: with nothing to match against every
/// row scores identically, and the sort would degrade to the tie-break —
/// title length — which is not an order anybody asked for.
fn rank(results: &mut AdvancedResults, kind: SearchKind, query: &str) {
    if query.is_empty() {
        return;
    }
    // Under `All` each list is ranked on its own: the three stay grouped, and
    // ranking across them would need one score to compare an artist against a
    // track, which the tiers do not express.
    if matches!(kind, SearchKind::All | SearchKind::Albums) {
        results
            .albums
            .sort_by_cached_key(|a| sort_key(query, &a.title, a.artist.as_deref()));
    }
    if matches!(kind, SearchKind::All | SearchKind::Artists) {
        results
            .artists
            .sort_by_cached_key(|a| sort_key(query, &a.name, None));
    }
    if matches!(kind, SearchKind::All | SearchKind::Tracks) {
        results
            .tracks
            .sort_by_cached_key(|t| sort_key(query, &t.title, t.artist.as_deref()));
    }
}

const ALBUM_NS: &str = "navidrome:album:";
const ARTIST_NS: &str = "navidrome:artist:";
const TRACK_NS: &str = "navidrome:track:";

/// The id a page or the player takes for a cache row. The sync stores server
/// ids namespaced (`navidrome:album:<id>`); the detail views, the stream URL
/// and scrobbling speak the bare id the API answers with. Local ids are the
/// scanner's own and stay as they are.
fn page_id(id: &str, source: &str, ns: &str) -> String {
    if source == SOURCE_LOCAL {
        id.to_string()
    } else {
        strip_ns(id.to_string(), ns)
    }
}

/// A cached track as the player wants it: bare ids for a server track, so it
/// streams, scrobbles and highlights like the same song played from its album
/// page — and its album id bare too, so its art lands on the key the album
/// pages cache that cover under.
fn track_song(t: &TrackRow) -> subsonic::Song {
    let mut song = t.clone().into_song();
    if t.source != SOURCE_LOCAL {
        song.id = strip_ns(song.id, TRACK_NS);
        song.album_id = song.album_id.map(|id| strip_ns(id, ALBUM_NS));
    }
    song
}

/// One item of the result list.
///
/// Albums and artists are drawn as their own pages draw them — a grid of
/// cards — and tracks as rows, so a line is either a run of cards or one
/// track. Under `SearchKind::All` each kind gets a heading.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    /// The genre the page is filtered by: its name and what it is.
    Intro,
    Heading {
        kind: SearchKind,
        count: usize,
    },
    /// `rows[start..end]`, one grid row.
    Cards {
        start: usize,
        end: usize,
    },
    Track(usize),
}

/// Lay `rows` out as lines: album and artist runs cut into grid rows of
/// `album_cols`/`artist_cols`, tracks one per line, a heading ahead of each
/// kind when `headings`. Rows arrive grouped by kind (`to_rows`), which is
/// what lets a run be cut without looking ahead.
fn to_lines(rows: &[Row], headings: bool, album_cols: usize, artist_cols: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let kind = rows[i].kind;
        let run = rows[i..].iter().take_while(|r| r.kind == kind).count();
        if headings {
            lines.push(Line::Heading { kind, count: run });
        }
        let end = i + run;
        match kind {
            SearchKind::Tracks | SearchKind::All => lines.extend((i..end).map(Line::Track)),
            SearchKind::Albums | SearchKind::Artists => {
                let cols = match kind {
                    SearchKind::Albums => album_cols,
                    _ => artist_cols,
                }
                .max(1);
                let mut start = i;
                while start < end {
                    let stop = (start + cols).min(end);
                    lines.push(Line::Cards { start, end: stop });
                    start = stop;
                }
            }
        }
        i = end;
    }
    lines
}

/// The line `row` is drawn on.
fn line_of(lines: &[Line], row: usize) -> Option<usize> {
    lines.iter().position(|l| match l {
        Line::Cards { start, end } => (*start..*end).contains(&row),
        Line::Track(r) => *r == row,
        Line::Heading { .. } | Line::Intro => false,
    })
}

fn to_rows(results: &AdvancedResults, kind: SearchKind) -> Vec<Row> {
    match kind {
        // One list of three, in the palette's order; `to_lines` heads each.
        SearchKind::All => SearchKind::EVERY
            .into_iter()
            .flat_map(|k| to_rows(results, k))
            .collect(),
        SearchKind::Albums => results
            .albums
            .iter()
            .enumerate()
            .map(|(index, a)| Row {
                id: page_id(&a.id, &a.source, ALBUM_NS),
                db_id: a.id.clone(),
                kind,
                index,
                primary: a.title.clone().into(),
                secondary: a.artist.clone().unwrap_or_default().into(),
                trailing: year_text(a.year),
                // Album covers carry a stable id, so the key is the id itself.
                cover: a.cover_art.clone().map(|c| (c.clone(), c)),
                local: a.source == SOURCE_LOCAL,
            })
            .collect(),
        SearchKind::Artists => results
            .artists
            .iter()
            .enumerate()
            .map(|(index, a)| Row {
                id: page_id(&a.id, &a.source, ARTIST_NS),
                db_id: a.id.clone(),
                kind,
                index,
                primary: a.name.clone().into(),
                secondary: SharedString::default(),
                trailing: SharedString::default(),
                cover: a.cover_art.clone().map(|c| (c.clone(), c)),
                local: a.source == SOURCE_LOCAL,
            })
            .collect(),
        SearchKind::Tracks => results
            .tracks
            .iter()
            .enumerate()
            .map(|(index, t)| Row {
                id: page_id(&t.id, &t.source, TRACK_NS),
                db_id: t.id.clone(),
                kind,
                index,
                primary: t.title.clone().into(),
                secondary: t.artist.clone().unwrap_or_default().into(),
                trailing: t
                    .duration
                    .map(|d| format_duration(Duration::from_secs(d.max(0.) as u64)))
                    .unwrap_or_default()
                    .into(),
                // A song's cover is scoped to its *album*: Navidrome ids one
                // per song, so keying on the song downloads the same art once
                // per track of the record.
                cover: artwork::song_cover(&track_song(t)),
                local: t.source == SOURCE_LOCAL,
            })
            .collect(),
    }
}

/// The genre heading's description, for one genre.
#[derive(Debug, Clone, PartialEq)]
enum GenreIntro {
    Loading,
    Found(Description),
    /// No article reads like this genre, Wikipedia is switched off, or the
    /// lookup failed — the heading stands on its own.
    None,
}

/// Paragraphs of a description shown before *More*.
const INTRO_PARAGRAPHS: usize = 1;
/// Widest the description runs, so a wide window keeps a readable measure.
const INTRO_MAX_W: f32 = 760.;
/// Sample prose shaping the description's placeholder.
const INTRO_SAMPLE: &str = "A genre of popular music that originated in the late twentieth \
     century, characterised by its instrumentation and a sound of its own, and \
     drawing on several older styles that came before it in the same scenes.";

pub struct AdvancedSearchView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    db: Arc<LibraryDb>,
    input: Entity<InputState>,
    query: String,
    filters: Filters,
    facets: Facets,
    results: AdvancedResults,
    rows: Vec<Row>,
    summary: SharedString,
    /// Whether a query is in flight, so the header can say so rather than
    /// showing "No results" over a search that has not run yet.
    searching: bool,
    /// Bumped per query; a result carrying a stale generation is dropped, so a
    /// slow query cannot overwrite a newer one's rows.
    generation: u64,
    error: Option<SharedString>,
    art_paths: HashMap<String, PathBuf>,
    /// Keys whose `art_paths` entry is the card rung — anything else drawn on
    /// a card is a stand-in still being replaced.
    art_exact: HashSet<String>,
    fetching: HashSet<String>,
    /// Last window art was requested for, so scrolling does not re-request.
    art_range: Option<(usize, usize)>,
    /// Rung the cards ask for, from `Settings::cover_size`.
    card_art_px: u32,
    /// `rows` as drawn: grid rows of cards, track rows, headings.
    lines: Vec<Line>,
    /// What `lines` was cut for — rows generation, album and artist columns —
    /// so it is recut only when one moves.
    lines_shape: (u64, usize, usize),
    /// Bumped whenever `rows` is replaced.
    rows_version: u64,
    /// The genre filter `rows` answer, which heads the list.
    rows_genre: Option<String>,
    /// The heading's description and the genre it describes.
    genre_intro: Option<(String, GenreIntro)>,
    /// When the description was asked for, for the placeholder's delay.
    genre_since: Option<std::time::Instant>,
    genre_expanded: bool,
    list: ListState,
    live_width: LiveWidth,
    /// Cover-derived glow colours, per art key, like the album grid's.
    glow_accents: std::cell::RefCell<HashMap<String, gpui::Hsla>>,
    vi_cursor: Option<usize>,
    /// Searching Lidarr's metadata instead of the library. Lookups go out to
    /// MusicBrainz, so they run on Enter or the button, not per keystroke.
    lidarr_mode: bool,
    lidarr_hits: Vec<LidarrHit>,
    lidarr_searching: bool,
    lidarr_error: Option<String>,
    /// The query the hits answer.
    lidarr_asked: Option<String>,
    lidarr_art: HashMap<String, PathBuf>,
    lidarr_generation: u64,
    lidarr_scroll: gpui::ScrollHandle,
}

impl AdvancedSearchView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        db: Arc<LibraryDb>,
        initial_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Search the library…");
            if !initial_query.is_empty() {
                state.set_value(initial_query.clone(), window, cx);
            }
            state
        });
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => this.on_query_changed(cx),
                InputEvent::PressEnter { .. } if this.lidarr_mode => this.run_lidarr(cx),
                _ => {}
            },
        )
        .detach();

        // The library selection is the sidebar's, not this page's: a page that
        // searched libraries the rest of the app is hiding would be the one
        // place they reappear.
        let filters = Filters {
            library_ids: session.read(cx).settings.library_ids.clone(),
            ..Filters::default()
        };

        let mut this = Self {
            session,
            player,
            db,
            input,
            query: initial_query,
            filters,
            facets: Facets::default(),
            results: AdvancedResults::default(),
            rows: Vec::new(),
            summary: SharedString::default(),
            searching: false,
            generation: 0,
            error: None,
            art_paths: HashMap::new(),
            art_exact: HashSet::new(),
            fetching: HashSet::new(),
            art_range: None,
            card_art_px: 0,
            lines: Vec::new(),
            lines_shape: (u64::MAX, 0, 0),
            rows_version: 0,
            rows_genre: None,
            genre_intro: None,
            genre_since: None,
            genre_expanded: false,
            list: ListState::new(0, ListAlignment::Top, px(LIST_OVERDRAW)),
            live_width: LiveWidth::default(),
            glow_accents: Default::default(),
            vi_cursor: None,
            lidarr_mode: false,
            lidarr_hits: Vec::new(),
            lidarr_searching: false,
            lidarr_error: None,
            lidarr_asked: None,
            lidarr_art: HashMap::new(),
            lidarr_generation: 0,
            lidarr_scroll: gpui::ScrollHandle::new(),
        };
        this.load_facets(cx);
        this.run_search(cx);
        this
    }

    /// Narrow the page to the `kind` rows tagged `genre`, dropping every other
    /// filter: a genre chip asks "what else is like this", not "this, within
    /// whatever was last searched".
    pub fn show_genre(
        &mut self,
        genre: String,
        kind: SearchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lidarr_mode = false;
        self.filters.clear();
        self.filters.kind = kind;
        self.filters.genre = Some(genre);
        self.query.clear();
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.art_range = None;
        self.vi_cursor = None;
        self.refilter(cx);
    }

    /// Focus the query field — what opening the page from the sidebar or from
    /// the palette should leave the user able to type into.
    pub fn focus_query(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.read(cx).focus_handle(cx).focus(window);
    }

    pub fn is_typing(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    fn load_facets(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let facets =
                runtime::spawn_blocking_io(move || db.search_facets().map_err(anyhow::Error::from))
                    .await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(facets) = facets {
                    view.facets = facets;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.query = self.input.read(cx).value().to_string();
        if self.lidarr_mode {
            // The button's label carries the query.
            cx.notify();
            return;
        }
        self.run_search(cx);
    }

    fn lidarr_enabled(cx: &gpui::App) -> bool {
        lidarr_state(cx).read(cx).enabled()
    }

    fn set_lidarr_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.lidarr_mode == on {
            return;
        }
        self.lidarr_mode = on;
        self.vi_cursor = None;
        if on {
            // Coming in with a query already typed: ask straight away, which
            // is what pressing the mode button means.
            if self.lidarr_asked.as_deref() != Some(self.query.trim()) {
                self.run_lidarr(cx);
            }
        } else {
            self.run_search(cx);
        }
        cx.notify();
    }

    /// Ask Lidarr's metadata search for the query.
    fn run_lidarr(&mut self, cx: &mut Context<Self>) {
        let query = self.query.trim().to_string();
        let Some(client) = lidarr_state(cx).read(cx).client.clone() else {
            return;
        };
        if query.is_empty() {
            return;
        }
        self.lidarr_generation += 1;
        let generation = self.lidarr_generation;
        self.lidarr_searching = true;
        self.lidarr_error = None;
        self.lidarr_asked = Some(query.clone());
        self.vi_cursor = None;
        cx.notify();
        let lookup = client.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                Ok::<_, anyhow::Error>(lidarr_add::lookup(lookup, query, MAX_LIDARR).await)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.lidarr_generation != generation {
                    return;
                }
                view.lidarr_searching = false;
                let (hits, error) = result.unwrap_or_else(|e| {
                    (
                        Vec::new(),
                        Some(format!("Lidarr: {}", crate::errors::error_text(&e))),
                    )
                });
                lidarr_add::want_hit_art(
                    &mut view.lidarr_art,
                    &client,
                    &hits,
                    (LIDARR_THUMB * 2.) as u32,
                    |view: &mut Self| &mut view.lidarr_art,
                    cx,
                );
                view.lidarr_hits = hits;
                view.lidarr_error = error;
                cx.notify();
            });
        })
        .detach();
    }

    fn lidarr_summary(&self) -> String {
        if self.lidarr_asked.is_none() || self.lidarr_searching {
            return String::new();
        }
        let artists = self
            .lidarr_hits
            .iter()
            .filter(|h| matches!(h, LidarrHit::Artist(_)))
            .count();
        let albums = self.lidarr_hits.len() - artists;
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        format!(
            "{} · {} from Lidarr",
            plural(artists, "artist", "artists"),
            plural(albums, "album", "albums")
        )
    }

    fn render_lidarr_row(
        &self,
        ix: usize,
        hit: &LidarrHit,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let focused = self.vi_cursor == Some(ix);
        let path = self.lidarr_art.get(&hit.art_key()).cloned();
        let round = matches!(hit, LidarrHit::Artist(_));
        let fallback = if round {
            IconName::CircleUser
        } else {
            IconName::LayoutDashboard
        };
        let tracked = hit.in_lidarr();
        let open = hit.clone();
        let thumb = div()
            .flex_none()
            .size(px(LIDARR_THUMB))
            .overflow_hidden()
            .bg(cx.theme().muted)
            .map(|d| {
                if round {
                    d.rounded_full()
                } else {
                    d.rounded_md()
                }
            })
            .map(|d| match path {
                Some(path) => d.child(img(path).size(px(LIDARR_THUMB))),
                None => d
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(muted)
                    .child(Icon::new(fallback).small()),
            });
        let row = h_flex()
            .id(("adv-lidarr", ix))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_3()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(AdvancedSearchEvent::OpenLidarr(open.clone()))
            }))
            .child(thumb)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(hit.title()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(hit.subtitle()),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(if tracked { cx.theme().success } else { muted })
                    .when(!tracked, |this| {
                        this.child(Icon::new(IconName::Plus).size_3())
                    })
                    .child(if tracked { "In Lidarr" } else { "Add" }),
            );
        with_focus_cursor(
            format!("adv-lidarr-focus-{ix}"),
            row,
            focused,
            glow,
            None,
            cx,
        )
    }

    fn render_lidarr(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let note = |text: String| {
            div()
                .px_4()
                .pt_2()
                .text_sm()
                .text_color(muted)
                .child(text)
                .into_any_element()
        };
        if self.lidarr_searching {
            return note("Searching Lidarr…".into());
        }
        let mut column = v_flex()
            .id("adv-lidarr-scroll")
            .flex_1()
            .min_h_0()
            .px_4()
            .gap_1()
            .overflow_y_scroll()
            .track_scroll(&self.lidarr_scroll);
        if let Some(why) = &self.lidarr_error {
            column = column.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(why.clone()),
            );
        }
        if self.lidarr_asked.is_none() {
            return note(
                "Type an artist or album and press Enter to search Lidarr's metadata.".into(),
            );
        }
        if self.lidarr_hits.is_empty() && self.lidarr_error.is_none() {
            return note("Lidarr found nothing.".into());
        }
        let split = self
            .lidarr_hits
            .iter()
            .position(|h| matches!(h, LidarrHit::Album(_)))
            .unwrap_or(self.lidarr_hits.len());
        let title = |label: &'static str| {
            div()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(muted)
                .child(label)
        };
        for (ix, hit) in self.lidarr_hits.iter().enumerate() {
            if ix == 0 && split > 0 {
                column = column.child(title("Artists"));
            }
            if ix == split {
                column = column.child(title("Albums"));
            }
            column = column.child(self.render_lidarr_row(ix, hit, cx));
        }
        column.into_any_element()
    }

    /// Re-run after a filter change. Immediate: a click is not a keystroke and
    /// has nothing to debounce.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.run_search_after(Duration::ZERO, cx);
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        self.run_search_after(DEBOUNCE, cx);
    }

    fn run_search_after(&mut self, delay: Duration, cx: &mut Context<Self>) {
        // With neither words nor filters there is no question. Listing the
        // whole library is what the album grid is for, and the empty page says
        // so rather than dumping it here.
        if self.query.trim().is_empty() && self.filters.is_empty() {
            self.generation += 1;
            self.searching = false;
            self.error = None;
            self.apply(AdvancedResults::default(), cx);
            return;
        }

        self.generation += 1;
        let generation = self.generation;
        self.searching = true;
        cx.notify();

        let db = self.db.clone();
        let query = self.query.trim().to_string();
        let filters = self.filters.clone();
        cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                // `cx.background_executor().timer()`, not `tokio::time::sleep`:
                // a gpui task has no reactor in scope.
                cx.background_executor().timer(delay).await;
            }
            // Stale before the query was even issued: the user typed on.
            if this
                .read_with(cx, |view, _| view.generation != generation)
                .unwrap_or(true)
            {
                return;
            }
            let found = runtime::spawn_blocking_io(move || {
                db.search_advanced(&query, &filters)
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.searching = false;
                match found {
                    Ok(results) => {
                        view.error = None;
                        view.apply(results, cx);
                    }
                    Err(err) => {
                        view.error = Some(crate::errors::error_text(&err).into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn apply(&mut self, mut results: AdvancedResults, cx: &mut Context<Self>) {
        if self.filters.sort == SortBy::Relevance
            || !self.filters.sort.applies_to(self.filters.kind)
        {
            rank(&mut results, self.filters.kind, self.query.trim());
        }
        self.summary = summarize(&results, self.filters.kind).into();
        self.rows = to_rows(&results, self.filters.kind);
        self.rows_version += 1;
        self.rows_genre = self.filters.genre.clone();
        self.results = results;
        // A new result set is a new window, whatever the scroll offset says.
        self.art_range = None;
        // And a new list, so the cursor is dropped rather than clamped: index
        // N is now a different row than the one it was put on, and Enter on it
        // would open something the user never looked at. Clamping only catches
        // the shorter-list case and leaves exactly that bug for every query
        // that returns as many rows as the last one.
        self.vi_cursor = None;
        cx.notify();
    }

    /// Ask for the description of the genre the rows are filtered by, once
    /// per genre. Behind `Settings::album_info_wikipedia`, the switch that
    /// already decides whether Wikipedia is asked about the library.
    fn sync_genre_intro(&mut self, cx: &mut Context<Self>) {
        let Some(genre) = self.rows_genre.clone() else {
            self.genre_intro = None;
            return;
        };
        if self.genre_intro.as_ref().is_some_and(|(g, _)| *g == genre) {
            return;
        }
        self.genre_expanded = false;
        if !self.session.read(cx).settings.album_info_wikipedia {
            self.genre_intro = Some((genre, GenreIntro::None));
            self.remeasure_intro();
            return;
        }
        self.genre_intro = Some((genre.clone(), GenreIntro::Loading));
        self.genre_since = Some(std::time::Instant::now());
        self.remeasure_intro();
        // Repaint when the placeholder is due, in case nothing else does.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::ui::PLACEHOLDER_DELAY)
                .await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let found = runtime::spawn_io(genre_info::fetch(genre.clone())).await;
            let _ = this.update(cx, |view, cx| {
                // A newer genre may have replaced this one meanwhile.
                let Some((current, state)) = view.genre_intro.as_mut() else {
                    return;
                };
                if *current != genre {
                    return;
                }
                *state = match found {
                    Ok(Some(desc)) => GenreIntro::Found(desc),
                    Ok(None) => GenreIntro::None,
                    Err(err) => {
                        tracing::debug!("genre description for {genre}: {err:#}");
                        GenreIntro::None
                    }
                };
                view.genre_since = None;
                view.remeasure_intro();
                cx.notify();
            });
        })
        .detach();
    }

    /// The intro line changed height; the list keeps item heights, so it has
    /// to be told.
    fn remeasure_intro(&self) {
        if self.lines.first() == Some(&Line::Intro) {
            self.list.splice(0..1, 1);
        }
    }

    /// The genre heading: its name, and what Wikipedia says it is.
    fn render_intro(&self, entity: &Entity<Self>, cx: &gpui::App) -> gpui::AnyElement {
        let Some((genre, state)) = self.genre_intro.as_ref() else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let expanded = self.genre_expanded;
        let body = match state {
            GenreIntro::Loading => Some(crate::ui::skeleton_block(
                "adv-genre-sk",
                crate::ui::placeholding(self.genre_since),
                INTRO_SAMPLE,
                cx,
            )),
            GenreIntro::None => None,
            GenreIntro::Found(desc) => {
                let paragraphs: Vec<&str> = desc.text.lines().collect();
                let more = paragraphs.len() > INTRO_PARAGRAPHS;
                let shown = if expanded {
                    paragraphs.len()
                } else {
                    INTRO_PARAGRAPHS
                };
                let url = desc.url.clone();
                let view = entity.clone();
                Some(
                    v_flex()
                        .gap_2()
                        .children(paragraphs.into_iter().take(shown).map(|p| {
                            div()
                                .text_sm()
                                .line_height(gpui::relative(1.55))
                                .child(p.to_string())
                        }))
                        // Links in the text's own column rather than ghost
                        // buttons, whose padding pushed them off its edge.
                        .child(
                            h_flex()
                                .gap_4()
                                .text_xs()
                                .font_medium()
                                .when(more, |this| {
                                    this.child(
                                        div()
                                            .id("adv-genre-more")
                                            .cursor_pointer()
                                            .text_color(cx.theme().primary)
                                            .hover(|s| s.opacity(0.8))
                                            .child(if expanded { "Less" } else { "More" })
                                            .on_click(move |_, _, cx: &mut App| {
                                                view.update(cx, |this, cx| {
                                                    this.genre_expanded = !this.genre_expanded;
                                                    this.remeasure_intro();
                                                    cx.notify();
                                                });
                                            }),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .id("adv-genre-source")
                                        .gap_1()
                                        .items_center()
                                        .cursor_pointer()
                                        .text_color(muted)
                                        .hover(|s| s.opacity(0.8))
                                        .child(app_icon(icons::BRAND_WIKIPEDIA).size_3())
                                        .child("Wikipedia")
                                        .on_click(move |_, _, cx: &mut App| cx.open_url(&url)),
                                ),
                        )
                        .into_any_element(),
                )
            }
        };
        v_flex()
            .w_full()
            .max_w(px(INTRO_MAX_W))
            .pt_2()
            .pb_4()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .text_color(muted)
                    .child("GENRE"),
            )
            .child(div().text_2xl().font_semibold().child(genre.clone()))
            .children(body)
            .into_any_element()
    }

    /// Recut `rows` into lines when the rows or the column counts moved.
    /// A recut is a new list, so the scroll goes back to the top only when
    /// the rows themselves changed — a resize keeps the place.
    fn sync_lines(&mut self, album_cols: usize, artist_cols: usize) {
        let shape = (self.rows_version, album_cols, artist_cols);
        if shape == self.lines_shape {
            return;
        }
        let new_rows = shape.0 != self.lines_shape.0;
        let anchor = self.list.logical_scroll_top();
        // The row the top line starts with, so a reflow keeps it on screen.
        let top_row = self.lines.get(anchor.item_ix).and_then(|l| match l {
            Line::Cards { start, .. } => Some(*start),
            Line::Track(r) => Some(*r),
            Line::Heading { .. } | Line::Intro => None,
        });
        self.lines_shape = shape;
        self.lines = to_lines(
            &self.rows,
            self.filters.kind == SearchKind::All,
            album_cols,
            artist_cols,
        );
        if self.rows_genre.is_some() {
            self.lines.insert(0, Line::Intro);
        }
        self.list.reset(self.lines.len());
        self.art_range = None;
        if !new_rows && let Some(line) = top_row.and_then(|r| line_of(&self.lines, r)) {
            self.list.scroll_to(gpui::ListOffset {
                item_ix: line,
                offset_in_item: px(0.),
            });
        }
    }

    /// Fetch art for the lines on screen and a little either side.
    ///
    /// The result set runs to thousands of rows, so fetching per result would
    /// queue a library's worth of downloads ahead of the twenty being looked
    /// at. Lines are of varying height, so the window is counted from the top
    /// line in track rows — the shortest line — which over-covers card rows a
    /// little and never under-covers.
    fn ensure_art_for_viewport(&mut self, cx: &mut Context<Self>) {
        if self.lines.is_empty() {
            return;
        }
        let top = self.list.logical_scroll_top().item_ix;
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let visible = if viewport > 0. {
            (viewport / ROW_H).ceil() as usize + 1
        } else {
            ART_LOOKAHEAD * 4
        };
        let first = top.saturating_sub(ART_LOOKAHEAD);
        let last = (top + visible + ART_LOOKAHEAD).min(self.lines.len());
        if first >= last || self.art_range == Some((first, last)) {
            return;
        }
        self.art_range = Some((first, last));

        let mut rows: Vec<usize> = Vec::new();
        for line in &self.lines[first..last] {
            match line {
                Line::Cards { start, end } => rows.extend(*start..*end),
                Line::Track(r) => rows.push(*r),
                Line::Heading { .. } | Line::Intro => {}
            }
        }
        let card_px = self.card_art_px;
        let wanted: Vec<(String, String, bool, Option<u32>)> = rows
            .into_iter()
            .filter_map(|ix| self.rows.get(ix))
            .filter_map(|r| {
                // Cards want their grid's rung; a track's 44px thumb takes
                // whatever is cached and never asks for a second size.
                let exact = (r.kind != SearchKind::Tracks).then_some(card_px);
                r.cover
                    .as_ref()
                    .map(|(id, key)| (id.clone(), key.clone(), r.local, exact))
            })
            .filter(|(_, key, _, exact)| {
                !self.fetching.contains(key)
                    && match exact {
                        Some(_) => !self.art_exact.contains(key),
                        None => !self.art_paths.contains_key(key),
                    }
            })
            .collect();

        for (id, key, local, exact) in wanted {
            if local {
                // The scanner already extracted it; nothing to download.
                if let Some(path) = local_art_path(&id).filter(|p| p.exists()) {
                    self.art_paths.insert(key.clone(), path);
                    self.art_exact.insert(key);
                }
                continue;
            }
            if let Some(size) = exact
                && let Some(path) = artwork::cached(&key, size)
            {
                self.art_paths.insert(key.clone(), path);
                self.art_exact.insert(key);
                continue;
            }
            // Synchronous cache hit: draws with this frame, no task — and
            // `cached_best`, not `cached`, so *any* rung already on disk
            // answers. A track's thumbnail is 44px, so the grid's 512 looks
            // identical to a 256 in it, and asking for this page's own rung
            // would re-download the whole library's art at a second size. A
            // card paints it as a stand-in while its own rung is fetched.
            let best = artwork::cached_best(&key, exact.unwrap_or(ART_SIZE));
            if let Some(path) = best.clone() {
                self.art_paths.insert(key.clone(), path);
            }
            if exact.is_none() && best.is_some() {
                continue;
            }
            let Some(client) = self.session.read(cx).client.clone() else {
                continue;
            };
            let size = exact.unwrap_or(ART_SIZE);
            self.fetching.insert(key.clone());
            cx.spawn(async move |this, cx| {
                let fetched =
                    runtime::spawn_io(artwork::fetch_as(client, id, key.clone(), size)).await;
                let _ = this.update(cx, |view, cx| {
                    if let Ok(path) = fetched {
                        view.art_paths.insert(key.clone(), path);
                        if exact.is_some() {
                            view.art_exact.insert(key.clone());
                        }
                    }
                    view.fetching.remove(&key);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    // -----------------------------------------------------------------------
    // Activation
    // -----------------------------------------------------------------------

    /// Open or play the row at `ix` — what a click and vi-mode Enter share.
    fn activate(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        let id = row.id.clone();
        let local = row.local;
        // The row's own kind, not the page's: under `All` one list holds all
        // three, and the page's kind would open an album for an artist row.
        match row.kind {
            SearchKind::Albums => {
                if local {
                    cx.emit(AdvancedSearchEvent::OpenLocalAlbum(id));
                } else {
                    cx.emit(AdvancedSearchEvent::OpenAlbum(id));
                }
            }
            // There is no local artist page, so a local artist row is inert
            // rather than opening a page that does not exist.
            SearchKind::Artists => {
                if !local {
                    cx.emit(AdvancedSearchEvent::OpenArtist(id));
                }
            }
            SearchKind::Tracks | SearchKind::All => self.play_from(ix, cx),
        }
    }

    /// Play the track the row at `ix` names, queueing the rest of the result
    /// set's tracks behind it — the result set is a listing, and a listing you
    /// can only play one track out of is a worse listing.
    fn play_from(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        // `ix` walks the whole row list; the queue is indexed into the track
        // vec, which under `All` starts partway down it.
        let ix = row.index;
        if row.kind != SearchKind::Tracks || ix >= self.results.tracks.len() {
            return;
        }
        let songs: Vec<_> = self.results.tracks.iter().map(track_song).collect();
        self.player.update(cx, |p, cx| p.play_queue(songs, ix, cx));
    }

    /// Play the album the row at `ix` names, from the cache's copy of its
    /// tracks — the page is cache-only, and so is this.
    fn play_album(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).filter(|r| r.kind == SearchKind::Albums) else {
            return;
        };
        let db = self.db.clone();
        let key = row.db_id.clone();
        let player = self.player.clone();
        cx.spawn(async move |this, cx| {
            let tracks = runtime::spawn_blocking_io(move || {
                db.tracks_by_album(&key).map_err(anyhow::Error::from)
            })
            .await;
            match tracks {
                Ok(tracks) if !tracks.is_empty() => {
                    let songs: Vec<_> = tracks.iter().map(track_song).collect();
                    let _ = player.update(cx, |p, cx| p.play_queue(songs, 0, cx));
                }
                Ok(_) => {}
                Err(err) => {
                    let _ = this.update(cx, |view, cx| {
                        view.error = Some(crate::errors::error_text(&err).into());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    // -----------------------------------------------------------------------
    // Filter controls
    // -----------------------------------------------------------------------

    fn set_kind(&mut self, kind: SearchKind, cx: &mut Context<Self>) {
        if self.lidarr_mode {
            self.lidarr_mode = false;
            self.vi_cursor = None;
            if self.filters.kind == kind {
                self.run_search(cx);
                return;
            }
        }
        if self.filters.kind == kind {
            return;
        }
        self.filters.kind = kind;
        // The art keys are per cover id and the rows are rebuilt anyway, but
        // the window they were fetched for belongs to the old list.
        self.art_range = None;
        self.refilter(cx);
    }

    /// One dropdown, built from a list of (label, value) pairs. `set` marks one
    /// narrowing the list, which is drawn selected.
    ///
    /// Every filter but the star is this shape, and writing each one out
    /// separately is six copies of the same twenty lines.
    #[allow(clippy::too_many_arguments)]
    fn choice<T, F>(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        width: f32,
        set: bool,
        options: Vec<(SharedString, T)>,
        current: T,
        apply: F,
        cx: &mut Context<Self>,
    ) -> impl IntoElement
    where
        T: PartialEq + Clone + 'static,
        F: Fn(&mut Self, T, &mut Context<Self>) + Clone + 'static,
    {
        let view = cx.entity();
        Button::new(id)
            .label(label.into())
            .dropdown_caret(true)
            .outline()
            .small()
            .h(px(32.))
            .text_size(px(13.))
            .min_w(px(width))
            // A filter that is set says so, rather than reading as one more
            // dropdown left at its default.
            .selected(set)
            .dropdown_menu(move |menu, _window, _cx| {
                let menu = menu.max_h(px(320.)).scrollable(true);
                options.iter().cloned().fold(menu, |menu, (text, value)| {
                    let view = view.clone();
                    let apply = apply.clone();
                    let selected = value == current;
                    menu.item(PopupMenuItem::new(text).checked(selected).on_click(
                        move |_, _, cx: &mut gpui::App| {
                            let value = value.clone();
                            let apply = apply.clone();
                            view.update(cx, |this, cx| apply(this, value, cx));
                        },
                    ))
                })
            })
    }

    /// The kind tabs, with the Lidarr stop after them when it is configured.
    fn kind_tabs(&self, cx: &mut Context<Self>) -> gpui::Div {
        let kind = self.filters.kind;
        // Kind: a segmented row rather than a dropdown. It is not one filter
        // among the others — it decides which of the others exist at all.
        let view = cx.entity();
        let tabs = h_flex()
            .gap_0p5()
            .p_0p5()
            .rounded_lg()
            .bg(cx.theme().muted)
            .children(SearchKind::ALL.map(|k| {
                let view = view.clone();
                let active = k == kind && !self.lidarr_mode;
                let button = Button::new(ElementId::from(SharedString::from(format!(
                    "adv-kind-{}",
                    k.label()
                ))))
                .label(k.label())
                .small()
                .h(px(28.))
                .px_3()
                .text_size(px(13.))
                .on_click(move |_, _, cx: &mut gpui::App| {
                    view.update(cx, |this, cx| this.set_kind(k, cx));
                });
                if active {
                    button.primary()
                } else {
                    button.ghost()
                }
            }));
        let lidarr_on = Self::lidarr_enabled(cx);
        h_flex()
            .gap_2()
            .items_center()
            .child(tabs)
            .when(lidarr_on, |row| {
                let view = view.clone();
                let button = Button::new("adv-kind-lidarr")
                    .icon(app_icon(icons::DOWNLOAD))
                    .label("Lidarr")
                    .tooltip("Find music to add with Lidarr")
                    .small()
                    .h(px(32.))
                    .text_size(px(13.))
                    .on_click(move |_, _, cx: &mut gpui::App| {
                        view.update(cx, |this, cx| this.set_lidarr_mode(true, cx));
                    });
                row.child(if self.lidarr_mode {
                    button.primary()
                } else {
                    button.ghost()
                })
            })
    }

    /// Sort field and direction, which sit at the right end of the tab row:
    /// they arrange what is listed rather than narrowing it.
    fn sort_controls(&self, cx: &mut Context<Self>) -> gpui::Div {
        let f = &self.filters;
        let kind = f.kind;
        let sorts: Vec<(SharedString, SortBy)> = SortBy::ALL
            .into_iter()
            .filter(|s| s.applies_to(kind))
            .map(|s| (SharedString::from(s.label()), s))
            .collect();
        let sort = if f.sort.applies_to(kind) {
            f.sort
        } else {
            SortBy::Relevance
        };
        let descending = f.descending;
        let dir_view = cx.entity();
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Sort"),
            )
            .child(self.choice(
                "adv-sort",
                sort.label(),
                128.,
                false,
                sorts,
                sort,
                |this, v, cx| {
                    this.filters.sort = v;
                    this.refilter(cx);
                },
                cx,
            ))
            // Direction is a separate toggle rather than doubling every sort
            // entry into an ascending and a descending copy. Relevance has no
            // direction, so it is not offered there at all.
            .when(sort != SortBy::Relevance, |this| {
                this.child(
                    Button::new("adv-sort-dir")
                        .icon(if descending {
                            IconName::SortDescending
                        } else {
                            IconName::SortAscending
                        })
                        .ghost()
                        .small()
                        .h(px(32.))
                        .tooltip(if descending {
                            "Descending"
                        } else {
                            "Ascending"
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            dir_view.update(cx, |this, cx| {
                                this.filters.descending = !this.filters.descending;
                                this.refilter(cx);
                            });
                        }),
                )
            })
    }

    fn filter_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let f = self.filters.clone();
        let kind = f.kind;

        if self.lidarr_mode {
            let query = self.query.trim().to_string();
            let label = if self.lidarr_searching {
                "Searching Lidarr…".to_string()
            } else if query.is_empty() {
                "Search Lidarr".to_string()
            } else {
                format!("Search Lidarr for “{query}”")
            };
            return v_flex()
                .w_full()
                .gap_3()
                .child(self.kind_tabs(cx))
                .child(
                    h_flex().child(
                        Button::new("adv-lidarr-search")
                            .outline()
                            .small()
                            .h(px(32.))
                            .icon(Icon::new(IconName::Search))
                            .label(label)
                            .loading(self.lidarr_searching)
                            .disabled(query.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.run_lidarr(cx))),
                    ),
                )
                .into_any_element();
        }

        let genres: Vec<(SharedString, Option<String>)> =
            std::iter::once((SharedString::from("Any genre"), None))
                .chain(
                    self.facets
                        .genres
                        .iter()
                        .map(|g| (SharedString::from(g.clone()), Some(g.clone()))),
                )
                .collect();
        let genre_label = f
            .genre
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| "Genre".into());

        let formats: Vec<(SharedString, Option<String>)> =
            std::iter::once((SharedString::from("Any format"), None))
                .chain(
                    self.facets
                        .formats
                        .iter()
                        .map(|s| (SharedString::from(s.to_uppercase()), Some(s.clone()))),
                )
                .collect();
        let format_label = f
            .format
            .clone()
            .map(|s| SharedString::from(s.to_uppercase()))
            .unwrap_or_else(|| "Format".into());

        // Year bounds come from the cache, so a library that stops at 2014 is
        // never offered 2025.
        let (lo, hi) = (
            self.facets.year_min.unwrap_or(1900),
            self.facets.year_max.unwrap_or(2100),
        );
        // Newest first: the years a library is actually asked about are at the
        // recent end, and a list opening at 1900 makes them a long scroll away.
        let years = |first: &str| -> Vec<(SharedString, Option<i32>)> {
            std::iter::once((SharedString::from(first.to_string()), None))
                .chain(
                    (lo..=hi)
                        .rev()
                        .map(|y| (SharedString::from(y.to_string()), Some(y))),
                )
                .collect()
        };
        let year_from_label: SharedString = f
            .year_min
            .map(|y| SharedString::from(format!("From {y}")))
            .unwrap_or_else(|| "From".into());
        let year_to_label: SharedString = f
            .year_max
            .map(|y| SharedString::from(format!("To {y}")))
            .unwrap_or_else(|| "To".into());

        let duration_label: SharedString = DURATION_BANDS
            .iter()
            .skip(1)
            .find(|(_, min, max)| *min == f.duration_min && *max == f.duration_max)
            .map(|(label, _, _)| SharedString::from(*label))
            .unwrap_or_else(|| "Length".into());

        let bitrate_label: SharedString = BITRATES
            .iter()
            .skip(1)
            .find(|(_, v)| *v == f.bitrate_min)
            .map(|(label, _)| SharedString::from(*label))
            .unwrap_or_else(|| "Bitrate".into());

        let source_label: SharedString = if f.source == SourceFilter::All {
            "Source".into()
        } else {
            f.source.label().into()
        };

        let star_view = cx.entity();

        // Filters the kind cannot express are left out rather than greyed:
        // a row of dead dropdowns is noise, and the kind tabs right above say
        // why the row changed.
        let dropdowns = h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            .items_center()
            .when(
                kind.supports_genre() && !self.facets.genres.is_empty(),
                |row| {
                    row.child(self.choice(
                        "adv-genre",
                        genre_label,
                        150.,
                        f.genre.is_some(),
                        genres,
                        f.genre.clone(),
                        |this, v, cx| {
                            this.filters.genre = v;
                            this.refilter(cx);
                        },
                        cx,
                    ))
                },
            )
            .when(kind.supports_year(), |row| {
                row.child(self.choice(
                    "adv-year-from",
                    year_from_label,
                    104.,
                    f.year_min.is_some(),
                    years("Any year"),
                    f.year_min,
                    |this, v, cx| {
                        this.filters.year_min = v;
                        // A range that crosses itself matches nothing, and
                        // silently matching nothing reads as a bug — so the
                        // other end is carried along.
                        if let (Some(a), Some(b)) = (v, this.filters.year_max)
                            && a > b
                        {
                            this.filters.year_max = Some(a);
                        }
                        this.refilter(cx);
                    },
                    cx,
                ))
                .child(self.choice(
                    "adv-year-to",
                    year_to_label,
                    104.,
                    f.year_max.is_some(),
                    years("Any year"),
                    f.year_max,
                    |this, v, cx| {
                        this.filters.year_max = v;
                        if let (Some(a), Some(b)) = (this.filters.year_min, v)
                            && a > b
                        {
                            this.filters.year_min = Some(b);
                        }
                        this.refilter(cx);
                    },
                    cx,
                ))
            })
            .when(kind.supports_duration(), |row| {
                row.child(
                    self.choice(
                        "adv-length",
                        duration_label,
                        128.,
                        f.duration_min.is_some() || f.duration_max.is_some(),
                        DURATION_BANDS
                            .iter()
                            .map(|(l, min, max)| (SharedString::from(*l), (*min, *max)))
                            .collect(),
                        (f.duration_min, f.duration_max),
                        |this, (min, max), cx| {
                            this.filters.duration_min = min;
                            this.filters.duration_max = max;
                            this.refilter(cx);
                        },
                        cx,
                    ),
                )
            })
            .child(
                self.choice(
                    "adv-source",
                    source_label,
                    128.,
                    f.source != SourceFilter::All,
                    SourceFilter::ALL
                        .into_iter()
                        .map(|s| (SharedString::from(s.label()), s))
                        .collect(),
                    f.source,
                    |this, v, cx| {
                        this.filters.source = v;
                        this.refilter(cx);
                    },
                    cx,
                ),
            )
            .when(
                kind.supports_technical() && !self.facets.formats.is_empty(),
                |row| {
                    row.child(self.choice(
                        "adv-format",
                        format_label,
                        112.,
                        f.format.is_some(),
                        formats,
                        f.format.clone(),
                        |this, v, cx| {
                            this.filters.format = v;
                            this.refilter(cx);
                        },
                        cx,
                    ))
                },
            )
            .when(kind.supports_technical(), |row| {
                row.child(
                    self.choice(
                        "adv-bitrate",
                        bitrate_label,
                        120.,
                        f.bitrate_min.is_some(),
                        BITRATES
                            .iter()
                            .map(|(l, v)| (SharedString::from(*l), *v))
                            .collect(),
                        f.bitrate_min,
                        |this, v, cx| {
                            this.filters.bitrate_min = v;
                            this.refilter(cx);
                        },
                        cx,
                    ),
                )
            })
            // Starred is a switch rather than a dropdown: it has two states
            // and one of them is "don't care".
            .when(kind.supports_starred(), |row| {
                row.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .h(px(32.))
                        .px_1()
                        .child(Switch::new("adv-starred").checked(f.starred_only).on_click(
                            move |checked, _, cx: &mut gpui::App| {
                                let checked = *checked;
                                star_view.update(cx, |this, cx| {
                                    this.filters.starred_only = checked;
                                    this.refilter(cx);
                                });
                            },
                        ))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child("Starred only"),
                        ),
                )
            });

        v_flex()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(self.kind_tabs(cx))
                    .child(div().flex_1())
                    .child(self.sort_controls(cx)),
            )
            .child(dropdowns)
            .when_some(self.active_chips(cx), |this, chips| this.child(chips))
            .into_any_element()
    }

    /// The active filters as removable chips, with *Clear all* after them —
    /// what is narrowing the list, readable at a glance and undone in one
    /// click each, rather than a count on a button.
    fn active_chips(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let chips = filter_chips(&self.filters);
        if chips.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let view = cx.entity();
        let clear_view = view.clone();
        let many = chips.len() > 1;
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1p5()
                .items_center()
                .children(chips.into_iter().enumerate().map(|(ix, (label, reset))| {
                    let view = view.clone();
                    h_flex()
                        .id(("adv-chip", ix))
                        .gap_1()
                        .items_center()
                        .h(px(26.))
                        .pl_2p5()
                        .pr_1p5()
                        .rounded_full()
                        .bg(theme.primary.opacity(0.14))
                        .border_1()
                        .border_color(theme.primary.opacity(0.35))
                        .text_xs()
                        .cursor_pointer()
                        .hover({
                            let bg = theme.primary.opacity(0.24);
                            move |s| s.bg(bg)
                        })
                        .tooltip(|window, cx| {
                            gpui_component::tooltip::Tooltip::new("Remove filter").build(window, cx)
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            view.update(cx, |this, cx| {
                                reset(&mut this.filters);
                                this.refilter(cx);
                            });
                        })
                        .child(label)
                        .child(
                            Icon::new(IconName::Close)
                                .size_3()
                                .text_color(theme.muted_foreground),
                        )
                }))
                .when(many, |row| {
                    row.child(
                        Button::new("adv-clear")
                            .label("Clear all")
                            .ghost()
                            .xsmall()
                            .text_size(px(12.))
                            .on_click(move |_, _, cx: &mut gpui::App| {
                                clear_view.update(cx, |this, cx| {
                                    this.filters.clear();
                                    this.refilter(cx);
                                });
                            }),
                    )
                }),
        )
    }

    // -----------------------------------------------------------------------
    // Rows
    // -----------------------------------------------------------------------

    /// A track row. Albums and artists are cards (`render_album_card`,
    /// `render_artist_card`).
    fn render_row(
        &self,
        entity: &Entity<Self>,
        ix: usize,
        focused: bool,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let row = &self.rows[ix];
        let art = row
            .cover
            .as_ref()
            .and_then(|(_, key)| self.art_paths.get(key))
            .cloned();
        let settings = &self.session.read(cx).settings;
        let glow = settings.selection_glow_vi;
        let hover_glow = settings.selection_glow_hover;
        let view = entity.clone();
        let radius = px(6.);

        let el = h_flex()
            .id(("adv-row", ix))
            // Sized to the line rather than its content, or the trailing
            // column lands at a different x on every line.
            .w_full()
            .h(px(ROW_H))
            .px_2()
            .gap_3()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, None, cx)
                } else {
                    s
                }
            })
            .on_click(move |_, _, cx: &mut gpui::App| {
                view.update(cx, |this, cx| this.activate(ix, cx));
            })
            .child(
                div()
                    .size(px(44.))
                    .rounded(radius)
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|this| match art {
                        Some(path) => this.child(img(path).size(px(44.)).rounded(radius)),
                        None => this.child(
                            Icon::new(IconName::LayoutDashboard)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        ),
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(row.primary.clone()))
                    .when(!row.secondary.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(row.secondary.clone()),
                        )
                    }),
            )
            // A local row says so, since the two sources open different pages
            // and one of them works with the server off.
            .when(row.local, |this| {
                this.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .flex_shrink_0()
                        .child("Local"),
                )
            })
            .child(
                div()
                    .w(px(SECONDARY_W))
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .truncate()
                    .child(
                        self.results
                            .tracks
                            .get(row.index)
                            .and_then(|t| t.album.clone())
                            .unwrap_or_default(),
                    ),
            )
            .when(!row.trailing.is_empty(), |this| {
                this.child(
                    div()
                        .w(px(TRAILING_W))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .text_right()
                        .child(row.trailing.clone()),
                )
            });

        with_focus_cursor(format!("adv-focus-{ix}"), el, focused, glow, None, cx)
    }

    /// Glow colour for a card, from its cover, when the setting asks for it.
    fn card_accent(&self, row: &Row, art: Option<&PathBuf>, cx: &gpui::App) -> Option<gpui::Hsla> {
        if !self.session.read(cx).settings.selection_glow_album_color {
            return None;
        }
        let key = row.cover.as_ref().map(|(_, k)| k.as_str())?;
        crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), key, art?)
    }

    fn row_art(&self, row: &Row) -> Option<PathBuf> {
        row.cover
            .as_ref()
            .and_then(|(_, key)| self.art_paths.get(key))
            .cloned()
    }

    /// An album as the album grid draws it: a card (flush or classic, per
    /// `Settings::classic_album_cards`) or, under `AlbumCardStyle::Gallery`, a
    /// bare tile — with the hover play button either way.
    fn render_album_card(
        &self,
        entity: &Entity<Self>,
        ix: usize,
        tile: f32,
        gallery: bool,
        focused: bool,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let row = &self.rows[ix];
        let art = self.row_art(row);
        let settings = &self.session.read(cx).settings;
        let glow = settings.selection_glow_vi;
        let hover_glow = settings.selection_glow_hover;
        let flush = !settings.classic_album_cards;
        let accent = self.card_accent(row, art.as_ref(), cx);
        let open_view = entity.clone();
        let play_view = entity.clone();
        let play = |button: Button| {
            button
                .primary()
                .icon(app_icon(icons::PLAY))
                .on_click(move |_, _, cx: &mut App| {
                    play_view.update(cx, |this, cx| this.play_album(ix, cx));
                    cx.stop_propagation();
                })
        };
        let open = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            open_view.update(cx, |this, cx| this.activate(ix, cx));
        };

        if gallery {
            let tile_el = crate::ui::gallery_tile(
                ("adv-tile", ix),
                tile,
                art,
                row.primary.clone(),
                row.secondary.clone(),
                Some(play(Button::new(("adv-tile-play", ix)).xsmall()).into_any_element()),
                false,
                cx,
            )
            .on_click(open);
            return with_focus_cursor(
                format!("adv-focus-{ix}"),
                tile_el,
                focused,
                glow,
                accent,
                cx,
            );
        }

        let cover = crate::ui::card_cover_edge(tile, flush);
        let local = row.local;
        let card = v_flex()
            .id(("adv-album", ix))
            .group("adv-card")
            .w(px(tile + crate::ui::card_padding()))
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .map(|c| match flush {
                true => c,
                false => c.p(px(crate::ui::card_inset())),
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
            .on_click(open)
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
                    .child(
                        div()
                            .absolute()
                            .bottom_2()
                            .right_2()
                            .opacity(0.)
                            .group_hover("adv-card", |s| s.opacity(1.))
                            .child(play(Button::new(("adv-card-play", ix)))),
                    ),
            )
            .child(
                v_flex()
                    .h(px(ALBUM_TEXT_H))
                    .map(|t| match flush {
                        true => t
                            .px(px(crate::ui::card_inset()))
                            .pb(px(crate::ui::card_inset())),
                        false => t,
                    })
                    .gap_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .max_h(px(NAME_LINE_H * 2.))
                            .overflow_hidden()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .child(row.primary.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(row.secondary.clone()),
                    )
                    // Year, and whether the album is a local one — the two
                    // sources open different pages.
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(match (row.trailing.is_empty(), local) {
                                (false, true) => format!("{} · Local", row.trailing),
                                (true, true) => "Local".to_string(),
                                _ => row.trailing.to_string(),
                            }),
                    ),
            );
        with_focus_cursor(format!("adv-focus-{ix}"), card, focused, glow, accent, cx)
    }

    /// An artist as the artist grid draws one: round photo, name under it.
    fn render_artist_card(
        &self,
        entity: &Entity<Self>,
        ix: usize,
        tile: f32,
        focused: bool,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let row = &self.rows[ix];
        let art = self.row_art(row);
        let settings = &self.session.read(cx).settings;
        let glow = settings.selection_glow_vi;
        let hover_glow = settings.selection_glow_hover;
        let accent = self.card_accent(row, art.as_ref(), cx);
        let view = entity.clone();
        let initial: SharedString = row
            .primary
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default()
            .into();
        let card = v_flex()
            .id(("adv-artist", ix))
            .w(px(tile + crate::ui::card_padding()))
            .p_1p5()
            .gap_1p5()
            .items_center()
            .rounded_lg()
            // A local artist has no page to open.
            .when(!row.local, |this| this.cursor_pointer())
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, accent, cx)
                } else {
                    s
                }
            })
            .active(|s| s.opacity(0.8))
            .on_click(move |_, _, cx: &mut App| {
                view.update(cx, |this, cx| this.activate(ix, cx));
            })
            .child(
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
                                .child(initial),
                        ),
                    }),
            )
            .child(
                v_flex()
                    .h(px(ARTIST_TEXT_H))
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
                            .child(row.primary.clone()),
                    )
                    .when(row.local, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .line_height(px(META_LINE_H))
                                .text_color(cx.theme().muted_foreground)
                                .child("Local"),
                        )
                    }),
            );
        with_focus_cursor(format!("adv-focus-{ix}"), card, focused, glow, accent, cx)
    }

    /// What the body says when there are no rows — three different situations
    /// that a single "No results" would collapse into one: a headline, a hint,
    /// and whether clearing the filters is the way out.
    fn empty_state(&self) -> (IconName, &'static str, &'static str, bool) {
        if self.searching {
            (IconName::Search, "Searching…", "", false)
        } else if self.results.is_empty() && self.query.trim().is_empty() && self.filters.is_empty()
        {
            (
                IconName::Search,
                "Search your library",
                "Type a title, an artist or an album — or pick a filter to browse by.",
                false,
            )
        } else {
            let filtered = !self.filters.is_empty();
            (
                IconName::Inbox,
                "Nothing matched",
                if filtered {
                    "Try fewer filters, or another kind."
                } else {
                    "Try fewer words, or another kind."
                },
                filtered,
            )
        }
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (icon, title, hint, clear) = self.empty_state();
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_2()
            .pb_16()
            .child(
                div()
                    .size(px(56.))
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(muted)
                    .child(Icon::new(icon).size_6()),
            )
            .child(div().pt_1().text_base().font_medium().child(title))
            .when(!hint.is_empty(), |this| {
                this.child(div().text_sm().text_color(muted).child(hint))
            })
            .when(clear, |this| {
                this.child(
                    Button::new("adv-empty-clear")
                        .label("Clear filters")
                        .outline()
                        .small()
                        .mt_2()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filters.clear();
                            this.refilter(cx);
                        })),
                )
            })
            .into_any_element()
    }

    // -----------------------------------------------------------------------
    // vi mode
    // -----------------------------------------------------------------------

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = if self.lidarr_mode {
            self.lidarr_hits.len()
        } else {
            self.rows.len()
        };
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
        if self.lidarr_mode {
            // Children are interleaved with section titles: one before the
            // artists (when any), one before the albums.
            let artists = self
                .lidarr_hits
                .iter()
                .filter(|h| matches!(h, LidarrHit::Artist(_)))
                .count();
            let error = usize::from(self.lidarr_error.is_some());
            let titles = usize::from(artists > 0) + usize::from(next >= artists);
            self.lidarr_scroll.scroll_to_item(next + titles + error);
        } else if let Some(mut line) = line_of(&self.lines, next) {
            // Going up into a section, bring its heading along.
            if delta < 0 && line > 0 && matches!(self.lines[line - 1], Line::Heading { .. }) {
                line -= 1;
            }
            self.list.scroll_to_reveal_item(line);
        }
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.vi_cursor else {
            return;
        };
        if self.lidarr_mode {
            if let Some(hit) = self.lidarr_hits.get(ix) {
                cx.emit(AdvancedSearchEvent::OpenLidarr(hit.clone()));
            }
        } else {
            self.activate(ix, cx);
        }
    }

    /// Space: play the focused row where that means anything.
    pub fn vi_play(&mut self, cx: &mut Context<Self>) {
        if self.lidarr_mode {
            return;
        }
        if let Some(ix) = self.vi_cursor {
            self.play_from(ix, cx);
        }
    }

    /// `i`: put the cursor in the query field, which is what this page is for.
    pub fn vi_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_query(window, cx);
    }

    /// `[` / `]`: cycle the kind, the page's own tab strip.
    /// Lidarr, when on, is the last stop after the kinds.
    pub fn vi_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let all = SearchKind::ALL;
        let stops = all.len() + usize::from(Self::lidarr_enabled(cx));
        let cur = if self.lidarr_mode {
            all.len()
        } else {
            all.iter()
                .position(|k| *k == self.filters.kind)
                .unwrap_or(0)
        };
        let next = (cur as isize + delta).rem_euclid(stops as isize) as usize;
        match all.get(next) {
            Some(kind) => self.set_kind(*kind, cx),
            None => self.set_lidarr_mode(true, cx),
        }
    }
}

impl AdvancedSearchView {
    /// A section heading under `All`: the kind and how many of it.
    fn render_heading(kind: SearchKind, count: usize, cx: &gpui::App) -> gpui::AnyElement {
        let title = match kind {
            SearchKind::Artists => "Artists",
            SearchKind::Tracks => "Tracks",
            _ => "Albums",
        };
        h_flex()
            .w_full()
            .items_baseline()
            .gap_2()
            .pt_4()
            .pb_2()
            .child(div().text_base().font_semibold().child(title))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format_count(count as i64)),
            )
            .into_any_element()
    }
}

impl Render for AdvancedSearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Lidarr turned off under the page: back to the library.
        if self.lidarr_mode && !Self::lidarr_enabled(cx) {
            self.lidarr_mode = false;
            self.run_search(cx);
        }

        // The grids' own sizing: the cover-size setting's range, filled to the
        // width, gallery tiles when the album grid draws them.
        let settings = &self.session.read(cx).settings;
        let cover = settings.cover_size;
        let gallery = settings.album_card_style == AlbumCardStyle::Gallery;
        let (min_tile, max_tile) = cover.range();
        let card_px = artwork::bucket(cover.art_px());
        if card_px != self.card_art_px {
            self.card_art_px = card_px;
            self.art_exact.clear();
            self.art_range = None;
        }
        let measured = f32::from(self.list.viewport_bounds().size.width);
        let (album_cols, album_tile) =
            self.live_width
                .grid(measured, min_tile, max_tile, window, FALLBACK_COLS, gallery);
        let (artist_cols, artist_tile) =
            self.live_width
                .grid(measured, min_tile, max_tile, window, FALLBACK_COLS, false);
        self.sync_lines(album_cols, artist_cols);
        self.sync_genre_intro(cx);
        self.ensure_art_for_viewport(cx);

        let entity = cx.entity();
        let empty = self.rows.is_empty();
        let lidarr_mode = self.lidarr_mode;
        let searching = if lidarr_mode {
            self.lidarr_searching
        } else {
            self.searching
        };
        let summary: SharedString = if lidarr_mode {
            self.lidarr_summary().into()
        } else {
            self.summary.clone()
        };
        let lines = self.lines.clone();
        // One gutter for the whole page, the grids' own, so the controls, the
        // heading and the cards share a left edge — and the column maths
        // (`LiveWidth::grid` takes `grid_padding_x` off the list's width)
        // matches what is drawn.
        let gutter = px(crate::ui::grid_padding_x() / 2.);
        let list = list(self.list.clone(), move |ix, _window, cx| {
            let view = entity.read(cx);
            // Padded per line rather than on the list: gpui's `list` narrows
            // its items by its padding but lays them out from its left edge,
            // so list padding comes out as a right gutter only.
            let line = match lines.get(ix) {
                Some(Line::Intro) => view.render_intro(&entity, cx),
                Some(Line::Heading { kind, count }) => Self::render_heading(*kind, *count, cx),
                Some(Line::Track(row)) => view
                    .render_row(&entity, *row, view.vi_cursor == Some(*row), cx)
                    .into_any_element(),
                Some(Line::Cards { start, end }) => {
                    let albums = view.rows.get(*start).map(|r| r.kind) == Some(SearchKind::Albums);
                    let gallery = albums && gallery;
                    let cards: Vec<_> = (*start..*end)
                        .map(|i| {
                            let focused = view.vi_cursor == Some(i);
                            if albums {
                                view.render_album_card(&entity, i, album_tile, gallery, focused, cx)
                            } else {
                                view.render_artist_card(&entity, i, artist_tile, focused, cx)
                            }
                        })
                        .collect();
                    h_flex()
                        .w_full()
                        .gap(px(crate::ui::grid_item_gap(gallery)))
                        .pb(px(crate::ui::grid_row_gap(gallery)))
                        .children(cards)
                        .into_any_element()
                }
                None => div().into_any_element(),
            };
            div().w_full().px(gutter).child(line).into_any_element()
        })
        .flex_1()
        .min_h_0()
        .pb_2();

        v_flex()
            .size_full()
            .pt_5()
            .gap_3()
            .child(
                v_flex()
                    .px(gutter)
                    .gap_3()
                    .child(div().text_xl().font_semibold().child("Search"))
                    .child(
                        Input::new(&self.input).cleanable(true).prefix(
                            Icon::new(IconName::Search)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        ),
                    )
                    .child(self.filter_bar(cx)),
            )
            .when_some(self.error.clone(), |this, err| {
                // A cache query that failed is a broken database rather than a
                // dropped connection, so there is nothing a Retry would do
                // differently.
                this.child(div().px(gutter).child(crate::ui::error_banner(
                    &crate::errors::ErrorNote {
                        text: err.to_string(),
                        retryable: false,
                    },
                    |_, _, _| {},
                    cx,
                )))
            })
            // What is listed, between the controls and the list: a rule with
            // the count on it, so the list reads as the answer to the row of
            // controls above it.
            .child(
                h_flex()
                    .mx(gutter)
                    .pb_1()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .min_h(px(24.))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(summary)
                    .when(searching, |this| {
                        this.child(
                            gpui_component::spinner::Spinner::new()
                                .xsmall()
                                .color(cx.theme().muted_foreground),
                        )
                    }),
            )
            .when(lidarr_mode, |this| this.child(self.render_lidarr(cx)))
            .when(!lidarr_mode && empty, |this| {
                // The genre heading still says what was asked for when
                // nothing in the library answers it.
                let intro = self
                    .rows_genre
                    .is_some()
                    .then(|| div().px(gutter).child(self.render_intro(&cx.entity(), cx)));
                this.children(intro).child(self.render_empty(cx))
            })
            .when(!lidarr_mode && !empty, |this| this.child(list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::library_db::{AlbumRow, ArtistRow, TrackRow};

    fn album(id: &str, title: &str, year: Option<i32>, secs: f64) -> AlbumRow {
        AlbumRow {
            id: id.into(),
            source: "navidrome".into(),
            title: title.into(),
            year,
            duration: secs,
            ..Default::default()
        }
    }

    #[test]
    fn the_summary_counts_and_times_what_is_listed() {
        let results = AdvancedResults {
            albums: vec![
                album("a", "One", Some(1998), 1800.),
                album("b", "Two", None, 1800.),
            ],
            ..Default::default()
        };
        assert_eq!(summarize(&results, SearchKind::Albums), "2 albums · 1h 0m");
    }

    #[test]
    fn the_summary_leaves_artists_untimed() {
        // An artist row carries no duration; "· 0m" beside it would be a lie
        // dressed as a total.
        let results = AdvancedResults {
            artists: vec![ArtistRow {
                id: "x".into(),
                name: "Someone".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(summarize(&results, SearchKind::Artists), "1 artist");
    }

    #[test]
    fn a_truncated_result_says_so() {
        let results = AdvancedResults {
            albums: vec![album("a", "One", None, 0.)],
            truncated: true,
            ..Default::default()
        };
        assert!(
            summarize(&results, SearchKind::Albums).ends_with("showing the first matches"),
            "a capped result must not read as a complete one"
        );
    }

    #[test]
    fn an_empty_result_draws_no_summary_at_all() {
        // Zeros beside an empty list read as a bug rather than as an answer.
        assert!(summarize(&AdvancedResults::default(), SearchKind::Tracks).is_empty());
    }

    #[test]
    fn everything_counts_each_kind_rather_than_totalling_them() {
        // "450 results" over a mixed list says nothing about what is in it.
        let results = AdvancedResults {
            albums: vec![album("a", "One", None, 0.)],
            artists: vec![ArtistRow {
                id: "x".into(),
                name: "Someone".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(summarize(&results, SearchKind::All), "1 artist · 1 album");
    }

    #[test]
    fn everything_lists_the_three_kinds_and_keeps_each_row_addressable() {
        // One flat list, but every row still knows what it is and where it sits
        // in its own vec — the page's kind cannot answer either question here,
        // and getting it wrong opens an album for an artist or plays the wrong
        // track.
        let results = AdvancedResults {
            albums: vec![album("a", "Record", None, 0.)],
            artists: vec![ArtistRow {
                id: "x".into(),
                name: "Someone".into(),
                ..Default::default()
            }],
            tracks: vec![
                TrackRow {
                    id: "t1".into(),
                    title: "First".into(),
                    ..Default::default()
                },
                TrackRow {
                    id: "t2".into(),
                    title: "Second".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let rows = to_rows(&results, SearchKind::All);
        let shape: Vec<(SearchKind, usize)> = rows.iter().map(|r| (r.kind, r.index)).collect();
        assert_eq!(
            shape,
            vec![
                (SearchKind::Artists, 0),
                (SearchKind::Albums, 0),
                (SearchKind::Tracks, 0),
                (SearchKind::Tracks, 1),
            ]
        );
        // The second track is row 3; playing it must queue from index 1.
        assert_eq!(rows[3].id, "t2");
    }

    fn rows_of(kinds: &[(SearchKind, usize)]) -> Vec<Row> {
        kinds
            .iter()
            .flat_map(|&(kind, n)| {
                (0..n).map(move |index| Row {
                    id: String::new(),
                    db_id: String::new(),
                    kind,
                    index,
                    primary: SharedString::default(),
                    secondary: SharedString::default(),
                    trailing: SharedString::default(),
                    cover: None,
                    local: false,
                })
            })
            .collect()
    }

    #[test]
    fn albums_and_artists_are_cut_into_grid_rows_and_tracks_stay_rows() {
        let rows = rows_of(&[
            (SearchKind::Artists, 3),
            (SearchKind::Albums, 5),
            (SearchKind::Tracks, 2),
        ]);
        let lines = to_lines(&rows, true, 2, 4);
        assert_eq!(
            lines,
            vec![
                Line::Heading {
                    kind: SearchKind::Artists,
                    count: 3
                },
                Line::Cards { start: 0, end: 3 },
                Line::Heading {
                    kind: SearchKind::Albums,
                    count: 5
                },
                Line::Cards { start: 3, end: 5 },
                Line::Cards { start: 5, end: 7 },
                Line::Cards { start: 7, end: 8 },
                Line::Heading {
                    kind: SearchKind::Tracks,
                    count: 2
                },
                Line::Track(8),
                Line::Track(9),
            ]
        );
        // The cursor walks rows; scrolling to it needs the line it is on.
        assert_eq!(line_of(&lines, 6), Some(4));
        assert_eq!(line_of(&lines, 9), Some(8));
    }

    #[test]
    fn one_kind_draws_no_headings() {
        let rows = rows_of(&[(SearchKind::Albums, 3)]);
        assert_eq!(
            to_lines(&rows, false, 3, 3),
            vec![Line::Cards { start: 0, end: 3 }]
        );
    }

    #[test]
    fn server_rows_open_by_their_bare_id_and_local_ones_as_stored() {
        // The cache namespaces server ids; the pages and the stream URL speak
        // the API's own.
        let results = AdvancedResults {
            albums: vec![
                album("navidrome:album:abc", "One", None, 0.),
                AlbumRow {
                    id: "local:album:x".into(),
                    source: SOURCE_LOCAL.into(),
                    title: "Two".into(),
                    ..Default::default()
                },
            ],
            tracks: vec![TrackRow {
                id: "navidrome:track:t".into(),
                source: "navidrome".into(),
                album_id: Some("navidrome:album:abc".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = to_rows(&results, SearchKind::All);
        assert_eq!(rows[0].id, "abc");
        assert_eq!(rows[0].db_id, "navidrome:album:abc");
        assert_eq!(rows[1].id, "local:album:x");
        let song = track_song(&results.tracks[0]);
        assert_eq!(song.id, "t");
        assert_eq!(song.album_id.as_deref(), Some("abc"));
    }

    #[test]
    fn rows_carry_the_year_for_albums_and_the_length_for_tracks() {
        let albums = AdvancedResults {
            albums: vec![album("a", "One", Some(1998), 10.)],
            ..Default::default()
        };
        assert_eq!(to_rows(&albums, SearchKind::Albums)[0].trailing, "1998");

        let tracks = AdvancedResults {
            tracks: vec![TrackRow {
                id: "t".into(),
                title: "Song".into(),
                duration: Some(125.),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(to_rows(&tracks, SearchKind::Tracks)[0].trailing, "2:05");
    }

    #[test]
    fn a_zero_year_is_blank_rather_than_drawn() {
        // The scanner writes 0 for a file with no date; "0" in the year column
        // reads as a year.
        assert!(year_text(Some(0)).is_empty());
        assert!(year_text(None).is_empty());
        assert_eq!(year_text(Some(1998)), "1998");
    }

    #[test]
    fn relevance_puts_the_typed_title_above_the_one_that_merely_contains_it() {
        // The statement returns these alphabetically; "Queen" must not sit
        // below "Queen of Denmark" just because Q comes before Q-u-e-e-n-space.
        let mut results = AdvancedResults {
            albums: vec![
                album("a", "Queen of Denmark", None, 0.),
                album("b", "Queen", None, 0.),
            ],
            ..Default::default()
        };
        rank(&mut results, SearchKind::Albums, "queen");
        assert_eq!(results.albums[0].title, "Queen");
    }

    #[test]
    fn relevance_leaves_an_empty_query_in_the_order_it_arrived() {
        // Nothing to score against, so the sort could only fall through to the
        // tie-break and reorder the list by title length.
        let mut results = AdvancedResults {
            albums: vec![
                album("a", "Something Very Long Indeed", None, 0.),
                album("b", "Short", None, 0.),
            ],
            ..Default::default()
        };
        rank(&mut results, SearchKind::Albums, "");
        assert_eq!(results.albums[0].id, "a");
    }

    #[test]
    fn a_local_row_is_marked_so_it_opens_the_right_page() {
        let results = AdvancedResults {
            albums: vec![AlbumRow {
                id: "a".into(),
                source: SOURCE_LOCAL.into(),
                title: "On Disk".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(to_rows(&results, SearchKind::Albums)[0].local);
    }

    #[test]
    fn every_active_filter_gets_a_chip_that_removes_only_itself() {
        let mut f = Filters {
            genre: Some("Jazz".into()),
            year_min: Some(1990),
            year_max: Some(1999),
            starred_only: true,
            ..Default::default()
        };
        let chips = filter_chips(&f);
        let labels: Vec<_> = chips.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Genre: Jazz", "1990 – 1999", "Starred"]);
        (chips[1].1)(&mut f);
        assert_eq!((f.year_min, f.year_max), (None, None));
        assert_eq!(f.genre.as_deref(), Some("Jazz"));
        assert!(f.starred_only);
    }

    #[test]
    fn an_open_year_range_says_which_end_is_set() {
        let from = Filters {
            year_min: Some(2001),
            ..Default::default()
        };
        assert_eq!(filter_chips(&from)[0].0, "From 2001");
        let until = Filters {
            year_max: Some(1979),
            ..Default::default()
        };
        assert_eq!(filter_chips(&until)[0].0, "Until 1979");
        assert!(filter_chips(&Filters::default()).is_empty());
    }
}
