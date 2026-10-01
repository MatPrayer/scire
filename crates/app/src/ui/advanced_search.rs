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
    Context, ElementId, Entity, EventEmitter, Focusable as _, IntoElement, Render, SharedString,
    UniformListScrollHandle, Window, div, img, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::services::advanced_search::{
    AdvancedResults, Facets, Filters, SearchKind, SortBy, SourceFilter,
};
use crate::services::library_db::{LibraryDb, SOURCE_LOCAL};
use crate::services::local_library::local_art_path;
use crate::services::{artwork, runtime};
use crate::state::lidarr::lidarr as lidarr_state;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::lidarr_add::{self, LidarrHit};
use crate::ui::search_bar::sort_key;
use crate::ui::{format_count, format_duration, format_playtime, with_focus_cursor};

/// Thumbnail size. The rows are list-sized rather than card-sized, so this is
/// the palette's rung rather than the grid's.
const ART_SIZE: u32 = 200;
/// Every row is the same height; `uniform_list` requires it.
const ROW_H: f32 = 56.;
/// Rows either side of the viewport whose art is fetched ahead of being drawn.
const ART_LOOKAHEAD: usize = 8;
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

/// One result row, pre-formatted.
///
/// Built when the results change rather than per frame, for the same reason
/// `recent.rs` does it: the row text is derived from a dozen `Option`s and the
/// page can hold thousands of them.
struct Row {
    id: String,
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
    /// Round thumbnail, for an artist.
    round: bool,
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

fn to_rows(results: &AdvancedResults, kind: SearchKind) -> Vec<Row> {
    match kind {
        // One list of three, in the palette's order. No section titles: the
        // list is a `uniform_list` and every item in one is the same height, so
        // a heading would have to be a row of exactly a row's height — a badge
        // on the row itself says the same thing without pretending to be one.
        SearchKind::All => SearchKind::EVERY
            .into_iter()
            .flat_map(|k| to_rows(results, k))
            .collect(),
        SearchKind::Albums => results
            .albums
            .iter()
            .enumerate()
            .map(|(index, a)| Row {
                id: a.id.clone(),
                kind,
                index,
                primary: a.title.clone().into(),
                secondary: a.artist.clone().unwrap_or_default().into(),
                trailing: year_text(a.year),
                // Album covers carry a stable id, so the key is the id itself.
                cover: a.cover_art.clone().map(|c| (c.clone(), c)),
                local: a.source == SOURCE_LOCAL,
                round: false,
            })
            .collect(),
        SearchKind::Artists => results
            .artists
            .iter()
            .enumerate()
            .map(|(index, a)| Row {
                id: a.id.clone(),
                kind,
                index,
                primary: a.name.clone().into(),
                secondary: SharedString::default(),
                trailing: SharedString::default(),
                cover: a.cover_art.clone().map(|c| (c.clone(), c)),
                local: a.source == SOURCE_LOCAL,
                round: true,
            })
            .collect(),
        SearchKind::Tracks => results
            .tracks
            .iter()
            .enumerate()
            .map(|(index, t)| Row {
                id: t.id.clone(),
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
                cover: artwork::song_cover(&t.clone().into_song()),
                local: t.source == SOURCE_LOCAL,
                round: false,
            })
            .collect(),
    }
}

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
    fetching: HashSet<String>,
    /// Last window art was requested for, so scrolling does not re-request.
    art_range: Option<(usize, usize)>,
    scroll: UniformListScrollHandle,
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
            fetching: HashSet::new(),
            art_range: None,
            scroll: UniformListScrollHandle::new(),
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

    /// Fetch art for the rows on screen and a little either side.
    ///
    /// The result set runs to thousands of rows, so fetching per result would
    /// queue a library's worth of downloads ahead of the twenty being looked
    /// at. Derived from the scroll handle's measured bounds, which are the
    /// previous frame's — one frame behind is invisible for artwork.
    fn ensure_art_for_viewport(&mut self, cx: &mut Context<Self>) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let base = self.scroll.0.borrow().base_handle.clone();
        let viewport = f32::from(base.bounds().size.height);
        let (first, last) = if viewport > 0. {
            let scrolled = f32::from(-base.offset().y).max(0.);
            (
                (scrolled / ROW_H).floor() as usize,
                ((scrolled + viewport) / ROW_H).ceil() as usize,
            )
        } else {
            // Pre-layout: cover a guessed screenful rather than nothing.
            (0, ART_LOOKAHEAD * 2)
        };
        let start = first.saturating_sub(ART_LOOKAHEAD);
        let end = (last + 1 + ART_LOOKAHEAD).min(count);
        if start >= end || self.art_range == Some((start, end)) {
            return;
        }
        self.art_range = Some((start, end));

        let wanted: Vec<(String, String, bool)> = self.rows[start..end]
            .iter()
            .filter_map(|r| {
                r.cover
                    .as_ref()
                    .map(|(id, key)| (id.clone(), key.clone(), r.local))
            })
            .filter(|(_, key, _)| !self.art_paths.contains_key(key) && !self.fetching.contains(key))
            .collect();

        for (id, key, local) in wanted {
            if local {
                // The scanner already extracted it; nothing to download.
                if let Some(path) = local_art_path(&id).filter(|p| p.exists()) {
                    self.art_paths.insert(key, path);
                }
                continue;
            }
            // Synchronous cache hit: draws with this frame, no task — and
            // `cached_best`, not `cached`, so *any* rung already on disk
            // answers. The row's thumbnail is 44px, so the grid's 512 looks
            // identical to a 256 in it, and asking only for this page's own
            // rung would re-download the whole library's art at a second size
            // for a browsed library where none of it is missing. Nothing
            // replaces it afterwards either: the fetch below is for covers the
            // cache has never held, not for a better copy of one it has.
            if let Some(path) = artwork::cached_best(&key, ART_SIZE) {
                self.art_paths.insert(key, path);
                continue;
            }
            let Some(client) = self.session.read(cx).client.clone() else {
                continue;
            };
            self.fetching.insert(key.clone());
            cx.spawn(async move |this, cx| {
                let fetched =
                    runtime::spawn_io(artwork::fetch_as(client, id, key.clone(), ART_SIZE)).await;
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
        let songs: Vec<_> = self
            .results
            .tracks
            .iter()
            .cloned()
            .map(|t| t.into_song())
            .collect();
        self.player.update(cx, |p, cx| p.play_queue(songs, ix, cx));
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

    /// One dropdown, built from a list of (label, value) pairs.
    ///
    /// Every filter but the star is this shape, and writing each one out
    /// separately is six copies of the same twenty lines.
    #[allow(clippy::too_many_arguments)]
    fn choice<T, F>(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        width: f32,
        enabled: bool,
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
            .w(px(width))
            .disabled(!enabled)
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

    fn filter_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let f = self.filters.clone();
        let kind = f.kind;

        // Kind: a segmented row rather than a dropdown. It is not one filter
        // among the others — it decides which of the others exist at all.
        let view = cx.entity();
        let kinds = h_flex().gap_1().children(SearchKind::ALL.map(|k| {
            let view = view.clone();
            let active = k == kind && !self.lidarr_mode;
            let button = Button::new(ElementId::from(SharedString::from(format!(
                "adv-kind-{}",
                k.label()
            ))))
            .label(k.label())
            .small()
            .h(px(32.))
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
        let kinds = kinds.when(lidarr_on, |row| {
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
            row.child(div().w(px(8.))).child(if self.lidarr_mode {
                button.primary()
            } else {
                button.ghost()
            })
        });
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
                .gap_2()
                .child(kinds)
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
            .unwrap_or_else(|| "Any genre".into());

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
            .unwrap_or_else(|| "Any format".into());

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
            .map(|y| SharedString::from(y.to_string()))
            .unwrap_or_else(|| "From".into());
        let year_to_label: SharedString = f
            .year_max
            .map(|y| SharedString::from(y.to_string()))
            .unwrap_or_else(|| "To".into());

        let duration_label: SharedString = DURATION_BANDS
            .iter()
            .find(|(_, min, max)| *min == f.duration_min && *max == f.duration_max)
            .map(|(label, _, _)| SharedString::from(*label))
            .unwrap_or_else(|| "Any length".into());

        let bitrate_label: SharedString = BITRATES
            .iter()
            .find(|(_, v)| *v == f.bitrate_min)
            .map(|(label, _)| SharedString::from(*label))
            .unwrap_or_else(|| "Any bitrate".into());

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

        let star_view = cx.entity();
        let dir_view = cx.entity();
        let clear_view = cx.entity();
        let active = f.active_count();

        v_flex()
            .w_full()
            .gap_2()
            .child(kinds)
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(self.choice(
                        "adv-genre",
                        genre_label,
                        150.,
                        kind.supports_genre() && !self.facets.genres.is_empty(),
                        genres,
                        f.genre.clone(),
                        |this, v, cx| {
                            this.filters.genre = v;
                            this.refilter(cx);
                        },
                        cx,
                    ))
                    .child(self.choice(
                        "adv-year-from",
                        year_from_label,
                        92.,
                        kind.supports_year(),
                        years("From"),
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
                        92.,
                        kind.supports_year(),
                        years("To"),
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
                    .child(
                        self.choice(
                            "adv-length",
                            duration_label,
                            128.,
                            kind.supports_duration(),
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
                    .child(
                        self.choice(
                            "adv-source",
                            f.source.label(),
                            128.,
                            true,
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
                    .child(self.choice(
                        "adv-format",
                        format_label,
                        128.,
                        kind.supports_technical() && !self.facets.formats.is_empty(),
                        formats,
                        f.format.clone(),
                        |this, v, cx| {
                            this.filters.format = v;
                            this.refilter(cx);
                        },
                        cx,
                    ))
                    .child(
                        self.choice(
                            "adv-bitrate",
                            bitrate_label,
                            128.,
                            kind.supports_technical(),
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
                    // Starred is a switch rather than a dropdown: it has two
                    // states and one of them is "don't care".
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .h(px(32.))
                            .px_1()
                            .child(
                                Switch::new("adv-starred")
                                    .checked(f.starred_only)
                                    .disabled(!kind.supports_starred())
                                    .on_click(move |checked, _, cx: &mut gpui::App| {
                                        let checked = *checked;
                                        star_view.update(cx, |this, cx| {
                                            this.filters.starred_only = checked;
                                            this.refilter(cx);
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(if kind.supports_starred() {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().muted_foreground.opacity(0.5)
                                    })
                                    .child("Starred"),
                            ),
                    )
                    .child(div().flex_1())
                    .child(self.choice(
                        "adv-sort",
                        sort.label(),
                        150.,
                        true,
                        sorts,
                        sort,
                        |this, v, cx| {
                            this.filters.sort = v;
                            this.refilter(cx);
                        },
                        cx,
                    ))
                    // Direction is a separate toggle rather than doubling every
                    // sort entry into an ascending and a descending copy.
                    .child(
                        Button::new("adv-sort-dir")
                            .icon(if f.descending {
                                IconName::SortDescending
                            } else {
                                IconName::SortAscending
                            })
                            .outline()
                            .small()
                            .h(px(32.))
                            .disabled(f.sort == SortBy::Relevance)
                            .tooltip(if f.descending {
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
                    .when(active > 0, |this| {
                        this.child(
                            Button::new("adv-clear")
                                .label(format!("Clear {active}"))
                                .ghost()
                                .small()
                                .h(px(32.))
                                .text_size(px(13.))
                                .on_click(move |_, _, cx: &mut gpui::App| {
                                    clear_view.update(cx, |this, cx| {
                                        this.filters.clear();
                                        this.refilter(cx);
                                    });
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    // -----------------------------------------------------------------------
    // Rows
    // -----------------------------------------------------------------------

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
        let radius = if row.round { px(22.) } else { px(6.) };

        let el = h_flex()
            .id(("adv-row", ix))
            // `uniform_list` sizes items to their content; without this the
            // trailing column lands at a different x on every line.
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
                            Icon::new(if row.round {
                                IconName::CircleUser
                            } else {
                                IconName::LayoutDashboard
                            })
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
            // What kind of thing the row is, where the list holds all three.
            // Round art already tells an artist apart; an album and a track
            // look identical without it.
            .when(self.filters.kind == SearchKind::All, |this| {
                this.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .flex_shrink_0()
                        .child(match row.kind {
                            SearchKind::Artists => "Artist",
                            SearchKind::Tracks => "Track",
                            _ => "Album",
                        }),
                )
            })
            // Under `All` the column is drawn empty for the kinds that have no
            // album, rather than dropped: the trailing column sits after it,
            // and a column only some rows carry puts the year and the length at
            // a different x down one page.
            .when(
                row.kind == SearchKind::Tracks || self.filters.kind == SearchKind::All,
                |this| {
                    let album = if row.kind == SearchKind::Tracks {
                        self.results
                            .tracks
                            .get(row.index)
                            .and_then(|t| t.album.clone())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    this.child(
                        div()
                            .w(px(SECONDARY_W))
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(album),
                    )
                },
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

    /// What the body says when there are no rows — three different situations
    /// that a single "No results" would collapse into one.
    fn empty_text(&self) -> &'static str {
        if self.searching {
            "Searching…"
        } else if self.results.is_empty() && self.query.trim().is_empty() && self.filters.is_empty()
        {
            "Type a query, or pick a filter to browse by."
        } else {
            "Nothing matched. Try fewer filters."
        }
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
        } else {
            self.scroll.scroll_to_item(next, gpui::ScrollStrategy::Top);
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

impl Render for AdvancedSearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_art_for_viewport(cx);

        // Lidarr turned off under the page: back to the library.
        if self.lidarr_mode && !Self::lidarr_enabled(cx) {
            self.lidarr_mode = false;
            self.run_search(cx);
        }
        let entity = cx.entity();
        let empty = self.rows.is_empty();
        let lidarr_mode = self.lidarr_mode;
        let summary: SharedString = if lidarr_mode {
            self.lidarr_summary().into()
        } else {
            self.summary.clone()
        };
        let list = uniform_list("adv-list", self.rows.len(), move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|ix| view.render_row(&entity, ix, view.vi_cursor == Some(ix), cx))
                .collect::<Vec<_>>()
        })
        .flex_1()
        .min_h_0()
        .px_4()
        .track_scroll(self.scroll.clone());

        v_flex()
            .size_full()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .px_4()
                    .items_center()
                    .gap_4()
                    .child(div().text_lg().child("Search"))
                    .child(div().flex_1())
                    .when(!summary.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(summary),
                        )
                    }),
            )
            .child(div().px_4().child(Input::new(&self.input).cleanable(true)))
            .child(div().px_4().child(self.filter_bar(cx)))
            .when_some(self.error.clone(), |this, err| {
                // A cache query that failed is a broken database rather than a
                // dropped connection, so there is nothing a Retry would do
                // differently.
                this.child(div().px_4().child(crate::ui::error_banner(
                    &crate::errors::ErrorNote {
                        text: err.to_string(),
                        retryable: false,
                    },
                    |_, _, _| {},
                    cx,
                )))
            })
            .when(lidarr_mode, |this| this.child(self.render_lidarr(cx)))
            .when(!lidarr_mode && empty, |this| {
                this.child(
                    div()
                        .px_4()
                        .pt_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(self.empty_text()),
                )
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
}
