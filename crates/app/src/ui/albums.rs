//! Album grid with cover art, pagination, and sort/filter tabs.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, ListAlignment, ListOffset, ListState, Render,
    UniformListScrollHandle, Window, div, img, list, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use subsonic::{Album, AlbumListType, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::{AlbumCardStyle, AlbumSort, TimelineGrouping};
use crate::services::library_db::{AlbumRow, LibraryDb, LibraryStats};
use crate::services::{artwork, runtime};
use crate::state::lidarr::Incoming;
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::{ConnectionStatus, Session};
use crate::ui::lidarr::Covers;
use crate::ui::with_focus_cursor;

const PAGE_SIZE: u32 = 100;
/// Load the next page when scrolled within this many pixels of the bottom.
const LOAD_AHEAD_PX: f32 = 600.;
/// How many cached placeholder cards get their cover looked up on seed.
/// The cache list can run to thousands of rows and every lookup is a stat;
/// the rest pick up art as the live pages overwrite them.
const CACHE_ART_PREFETCH: usize = 300;

/// Column guess for the very first frame, before anything has been laid out.
const FALLBACK_COLS: usize = 5;

/// Rows of covers fetched beyond each edge of the viewport.
const ART_LOOKAHEAD_ROWS: usize = 2;

/// Card text metrics. The line heights are explicit because gpui's default
/// line box for these font sizes clips descenders; the block height is fixed
/// so every card is the same size (a requirement of the virtualized rows).
const NAME_LINE_H: f32 = 20.;
const META_LINE_H: f32 = 17.;
const TEXT_BLOCK_H: f32 = NAME_LINE_H * 2. + META_LINE_H * 2.;

/// Corner radius of a download card's lit front.
const FRONT_RADIUS: f32 = 8.;

/// All selectable filters, in display order.
const TABS: &[AlbumSort] = &[
    AlbumSort::All,
    AlbumSort::New,
    AlbumSort::Recent,
    AlbumSort::Frequent,
    AlbumSort::Random,
    AlbumSort::Starred,
    AlbumSort::Timeline,
];

fn tab_label(sort: AlbumSort) -> &'static str {
    match sort {
        AlbumSort::All => "All",
        AlbumSort::New => "New",
        AlbumSort::Recent => "Recent",
        AlbumSort::Frequent => "Frequent",
        AlbumSort::Random => "Random",
        AlbumSort::Starred => "Starred",
        AlbumSort::Timeline => "Timeline",
    }
}

fn tab_list_type(sort: AlbumSort) -> AlbumListType {
    match sort {
        AlbumSort::All => AlbumListType::AlphabeticalByName,
        AlbumSort::New | AlbumSort::Timeline => AlbumListType::Newest,
        AlbumSort::Recent => AlbumListType::Recent,
        AlbumSort::Frequent => AlbumListType::Frequent,
        AlbumSort::Random => AlbumListType::Random,
        AlbumSort::Starred => AlbumListType::Starred,
    }
}

#[derive(Default)]
struct TabState {
    /// Emitted albums, in final display order.
    albums: Vec<Album>,
    /// Per-library fetched-but-not-yet-emitted albums (server order).
    /// Held back until a globally-ordered merge can emit them safely.
    buffers: Vec<VecDeque<Album>>,
    /// How many albums each library has contributed to `albums` (fair
    /// interleave tie-break when the sort key can't decide).
    lib_emitted: Vec<usize>,
    loading: bool,
    exhausted: bool,
    /// Pages fetched so far; each page requests PAGE_SIZE per selected
    /// library, so per-library offsets stay aligned across the merge.
    page: u32,
    /// Per-library exhaustion, indexed like the selection at fetch time.
    lib_exhausted: Vec<bool>,
    /// `albums` still holds placeholders seeded from the last Navidrome sync;
    /// live pages overwrite them from the front instead of appending.
    cached: bool,
    /// How many entries of `albums` came from the server (only meaningful
    /// while `cached` — it's the write cursor for the overwrite).
    live_len: usize,
    /// Bumped whenever `albums` changes, so the timeline knows to regroup.
    version: u64,
}

/// Display order between two albums for a tab (mirrors the server's order
/// for the corresponding getAlbumList2 type, so the per-library sorted
/// streams can be merge-sorted client-side).
fn album_cmp(tab: AlbumSort, a: &Album, b: &Album) -> Ordering {
    match tab {
        AlbumSort::All => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        AlbumSort::New | AlbumSort::Timeline => b.created.cmp(&a.created),
        AlbumSort::Frequent => b.play_count.cmp(&a.play_count),
        AlbumSort::Starred => b.starred.cmp(&a.starred),
        // No client-visible key; keep each library's order and let the
        // fair-interleave tie-break weave the streams together.
        AlbumSort::Recent | AlbumSort::Random => Ordering::Equal,
    }
}

/// Pop every album that can already be emitted in globally-correct order.
/// An album is only safe to emit while all non-exhausted libraries still
/// have buffered items — otherwise an unfetched item could sort earlier.
fn merge_ready(state: &mut TabState, tab: AlbumSort) -> Vec<Album> {
    let mut out = Vec::new();
    loop {
        let blocked = state
            .buffers
            .iter()
            .enumerate()
            .any(|(i, b)| b.is_empty() && !state.lib_exhausted[i]);
        if blocked {
            break;
        }
        let mut best: Option<usize> = None;
        for (i, buf) in state.buffers.iter().enumerate() {
            let Some(head) = buf.front() else { continue };
            let better = match best {
                None => true,
                Some(j) => match album_cmp(tab, head, state.buffers[j].front().unwrap()) {
                    Ordering::Less => true,
                    Ordering::Greater => false,
                    Ordering::Equal => state.lib_emitted[i] < state.lib_emitted[j],
                },
            };
            if better {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        state.lib_emitted[i] += 1;
        out.push(state.buffers[i].pop_front().unwrap());
    }
    out
}

/// Render a cached DB row as an `Album` placeholder.
///
/// The sync stores ids namespaced (`navidrome:album:<id>`); strip that back off
/// so a placeholder card navigates and fetches art with the same id the live
/// listing would use — that's also what lets the cover survive the swap instead
/// of blanking and re-downloading.
pub(crate) fn album_from_row(row: AlbumRow) -> Album {
    let strip = |id: &str, prefix: &str| id.strip_prefix(prefix).unwrap_or(id).to_string();
    Album {
        id: strip(&row.id, "navidrome:album:"),
        name: row.title,
        artist: row.artist,
        artist_id: row
            .artist_id
            .as_deref()
            .map(|id| strip(id, "navidrome:artist:")),
        cover_art: row.cover_art,
        song_count: Some(row.song_count as u32),
        duration: Some(row.duration as u32),
        year: row.year,
        // Mirrored by the sync so the placeholder can be sorted the way the
        // tab that shows it sorts — see `seed_from_cache`.
        created: row.created,
        starred: row.starred,
        play_count: row.play_count.map(|c| c as u64),
        // The artist page files a cached card under its section by these.
        release_types: row.release_types,
        // Not stored by the sync; nothing on a card reads them.
        genre: None,
        user_rating: None,
        artists: Vec::new(),
        original_release_date: None,
        release_date: None,
    }
}

/// Merge a freshly-fetched page into a tab's display list.
///
/// Normally an append. While the tab still holds placeholders seeded from the
/// last sync, the page *overwrites* them from the front instead: the row count
/// stays put, so the scrollbar doesn't jump under the user while live pages
/// stream in. Once the server's list ends, any cached tail (albums deleted
/// server-side since the sync) is dropped.
///
/// `state.exhausted` must already be set for this page.
fn apply_live_page(state: &mut TabState, page: &[Album]) {
    state.version += 1;
    if !state.cached {
        state.albums.extend_from_slice(page);
        return;
    }
    let start = state.live_len;
    let end = (start + page.len()).min(state.albums.len());
    state.albums.splice(start..end, page.iter().cloned());
    state.live_len += page.len();
    if state.exhausted {
        state.albums.truncate(state.live_len);
        state.cached = false;
    }
}

/// Width of the year scrubber beside the timeline.
const TIMELINE_RAIL_W: f32 = 56.;
/// Rows of the timeline left below the viewport before the next page is asked
/// for.
const TIMELINE_LOAD_AHEAD_ROWS: usize = 12;
/// Pixels the timeline lays out past each edge of the viewport.
const TIMELINE_OVERDRAW: f32 = 600.;

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// A calendar date, `(year, 1..=12, 1..=31)`.
type Date = (i32, u32, u32);

/// One row of the timeline: a date bucket's heading, or a line of its covers.
#[derive(Debug, Clone, PartialEq)]
enum TimelineRow {
    /// `bucket` is the first day of the heading's day, week, month or year;
    /// `None` groups albums with no added date.
    Header { bucket: Option<Date>, count: usize },
    /// Albums `start..end` of the tab's list.
    Covers { start: usize, end: usize },
}

/// The date an album was added, from its ISO-8601 `created` stamp. Read as
/// written — no time zone is applied, like the album page's Added chip.
fn added_date(created: Option<&str>) -> Option<Date> {
    let s = created?;
    let year = s.get(0..4)?.parse().ok()?;
    if s.get(4..5)? != "-" || s.get(7..8)? != "-" {
        return None;
    }
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some((year, month, day))
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil((y, m, d): Date) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = ((mp + 2) % 12 + 1) as u32;
    ((yoe + era * 400 + i64::from(m <= 2)) as i32, m, d)
}

/// 0 = Monday. 1970-01-01 was a Thursday.
fn weekday(date: Date) -> usize {
    (days_from_civil(date) + 3).rem_euclid(7) as usize
}

/// First day of the bucket `date` falls in.
fn bucket_of(date: Date, grouping: TimelineGrouping) -> Date {
    let (y, m, _) = date;
    match grouping {
        TimelineGrouping::Day => date,
        TimelineGrouping::Week => civil_from_days(days_from_civil(date) - weekday(date) as i64),
        TimelineGrouping::Month => (y, m, 1),
        TimelineGrouping::Year => (y, 1, 1),
    }
}

fn bucket_label(bucket: Option<Date>, grouping: TimelineGrouping) -> String {
    let Some(date @ (y, m, d)) = bucket else {
        return "Date unknown".into();
    };
    let month = MONTHS[m as usize - 1];
    match grouping {
        TimelineGrouping::Day => format!("{}, {d} {month} {y}", WEEKDAYS[weekday(date)]),
        TimelineGrouping::Week => format!("Week of {d} {month} {y}"),
        TimelineGrouping::Month => format!("{month} {y}"),
        TimelineGrouping::Year => y.to_string(),
    }
}

/// Group a newest-first album list into date headings and rows of `cols`
/// covers. Grouping is by runs, so it relies on the list already being sorted
/// by date — which is the order the timeline's `getAlbumList2` type returns.
fn timeline_rows(albums: &[Album], cols: usize, grouping: TimelineGrouping) -> Vec<TimelineRow> {
    let cols = cols.max(1);
    let bucket = |a: &Album| added_date(a.created.as_deref()).map(|d| bucket_of(d, grouping));
    let mut rows = Vec::new();
    let mut start = 0;
    while start < albums.len() {
        let key = bucket(&albums[start]);
        let end = albums[start..]
            .iter()
            .position(|a| bucket(a) != key)
            .map_or(albums.len(), |n| start + n);
        rows.push(TimelineRow::Header {
            bucket: key,
            count: end - start,
        });
        let mut row = start;
        while row < end {
            let next = (row + cols).min(end);
            rows.push(TimelineRow::Covers {
                start: row,
                end: next,
            });
            row = next;
        }
        start = end;
    }
    rows
}

/// The covers row holding album `index`.
fn timeline_row_of(rows: &[TimelineRow], index: usize) -> Option<usize> {
    rows.iter().position(
        |r| matches!(r, TimelineRow::Covers { start, end } if (*start..*end).contains(&index)),
    )
}

/// The first album drawn at or after row `row` — what a regroup keeps on
/// screen.
fn timeline_album_at(rows: &[TimelineRow], row: usize) -> Option<usize> {
    rows.get(row..)?.iter().find_map(|r| match r {
        TimelineRow::Covers { start, .. } => Some(*start),
        TimelineRow::Header { .. } => None,
    })
}

#[allow(clippy::enum_variant_names)]
pub enum AlbumsEvent {
    OpenAlbum(String),
    OpenArtist(String),
    /// A download card: Lidarr's page for the album.
    OpenLidarrAlbum(i64),
}

/// How a context-menu action should enqueue an album's songs.
#[derive(Clone, Copy)]
enum QueueMode {
    Play,
    Shuffle,
    PlayNext,
    Enqueue,
}

pub struct AlbumsView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    /// Last Navidrome sync's albums, painted while the server request runs.
    library_db: Arc<LibraryDb>,
    /// Albums for each filter tab, loaded independently and lazily.
    tabs: HashMap<AlbumSort, TabState>,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight cover downloads, kept so they're cancelled when this view is
    /// dropped on navigation (instead of leaking and starving the next page).
    art_tasks: Vec<gpui::Task<()>>,
    /// A coalesced repaint is scheduled; batches a burst of cover arrivals
    /// into one re-render instead of one per completed download.
    art_repaint_pending: bool,
    active_tab: AlbumSort,
    /// Rung thumbnails are currently fetched at — the *bucketed* size, not the
    /// setting's raw width, so a cover-size change that resolves to the same
    /// cache entry doesn't drop art it would only look up again.
    art_px: u32,
    /// Card range covers were last requested for, so a repaint that hasn't
    /// scrolled doesn't walk the viewport again.
    art_range: Option<(usize, usize)>,
    /// Virtualized row scroll handle: only visible rows are built/uploaded.
    pub scroll: UniformListScrollHandle,
    error: Option<crate::errors::ErrorNote>,
    /// Card index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Catalog totals shown in the header, for the selected libraries.
    stats: LibraryStats,
    /// Tracks the grid's width against the window's, so the column count
    /// follows a resize on the same frame instead of one behind it.
    live_width: crate::ui::LiveWidth,
    /// Playlist ids/names for the cards' context menu, shared by every card
    /// instead of collected per card per frame.
    menu_playlists: Rc<Vec<(String, String)>>,
    /// Per-album accent colours for `Settings::selection_glow_album_color`.
    /// `RefCell` because `render_card` only has `&self`/`&App` (it's called
    /// from `uniform_list`'s item closure, which reads the entity rather than
    /// updating it) — the cache is filled in lazily behind that shared ref.
    glow_accents: RefCell<HashMap<String, gpui::Hsla>>,
    /// Variable-height list behind the Timeline tab: month headings and
    /// cover rows differ in height, which `uniform_list` can't hold.
    timeline_list: ListState,
    timeline_rows: Rc<Vec<TimelineRow>>,
    /// `(tab version, album count, columns)` the rows were grouped for.
    timeline_sig: Option<(u64, usize, usize, TimelineGrouping, bool)>,
    /// The timeline's own width tracker: its list sits beside the year rail,
    /// so it measures differently from the card grid.
    timeline_width: crate::ui::LiveWidth,
    /// Lidarr downloads drawn ahead of the albums on the All and New tabs;
    /// refreshed from `LidarrState::incoming` each frame.
    incoming: Rc<Vec<Incoming>>,
    lidarr_covers: Covers,
    _lidarr_watch: gpui::Subscription,
}

impl EventEmitter<AlbumsEvent> for AlbumsView {}

impl AlbumsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        library_db: Arc<LibraryDb>,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_tab = session.read(cx).settings.album_sort;
        let art_px = artwork::bucket(session.read(cx).settings.cover_size.art_px());
        let mut this = Self {
            session,
            player,
            playlists,
            library_db,
            tabs: HashMap::new(),
            art_paths: HashMap::new(),
            art_tasks: Vec::new(),
            art_repaint_pending: false,
            active_tab,
            art_px,
            art_range: None,
            scroll: UniformListScrollHandle::new(),
            error: None,
            vi_cursor: None,
            stats: LibraryStats::default(),
            live_width: crate::ui::LiveWidth::default(),
            menu_playlists: Rc::new(Vec::new()),
            glow_accents: RefCell::new(HashMap::new()),
            timeline_list: ListState::new(0, ListAlignment::Top, px(TIMELINE_OVERDRAW)),
            timeline_rows: Rc::new(Vec::new()),
            timeline_sig: None,
            timeline_width: crate::ui::LiveWidth::default(),
            incoming: Rc::new(Vec::new()),
            lidarr_covers: Covers::default(),
            _lidarr_watch: cx.observe(&crate::state::lidarr::lidarr(cx), |_, _, cx| cx.notify()),
        };
        this.refresh_stats(cx);
        this.seed_from_cache(active_tab, cx);
        this.load_more(active_tab, cx);
        this
    }

    /// Fill a tab with the last Navidrome sync's albums so the grid is
    /// populated on the first frame instead of after the server answers.
    ///
    /// The sync mirrors each tab's sort key onto the row (`created`,
    /// `play_count`, `starred`) and records which library it came from, so the
    /// seed can reproduce the tab's ordering and honour a library subset. Rows
    /// are re-sorted with the same `album_cmp` the live merge uses, which is
    /// what makes the in-place overwrite line up.
    fn seed_from_cache(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        // Recent is "recently played" and Random has no order at all — neither
        // key exists in the DB, and rows under a heading that doesn't describe
        // them are worse than an empty grid.
        if matches!(tab, AlbumSort::Recent | AlbumSort::Random) {
            return;
        }
        // Gated on a *configured* server, not a connected one: this view is
        // built during the pre-connect wait, which is exactly the slow window
        // worth filling.
        if self.session.read(cx).settings.server.is_none() {
            return;
        }
        if self.tabs.get(&tab).is_some_and(|t| !t.albums.is_empty()) {
            return;
        }
        let Ok(rows) = self.library_db.albums_by_source("navidrome") else {
            return;
        };
        // Rows synced before the provenance column existed carry no library id;
        // with a subset selected they're skipped rather than guessed at, and
        // the next sync fills them in.
        let libraries = self.session.read(cx).library_ids.clone();
        let mut albums: Vec<Album> = rows
            .into_iter()
            .filter(|row| {
                libraries.is_empty()
                    || row
                        .library_id
                        .as_ref()
                        .is_some_and(|id| libraries.contains(id))
            })
            .map(album_from_row)
            .collect();
        if tab == AlbumSort::Starred {
            albums.retain(|a| a.starred.is_some());
        }
        // `albums_by_source` already returns `title COLLATE NOCASE`, which is
        // the All tab's order.
        if tab != AlbumSort::All {
            albums.sort_by(|a, b| album_cmp(tab, a, b));
        }
        if albums.is_empty() {
            return;
        }
        let state = self.tabs.entry(tab).or_default();
        state.cached = true;
        state.live_len = 0;
        state.version += 1;
        state.albums = albums.clone();
        for album in albums.iter().take(CACHE_ART_PREFETCH) {
            self.fetch_art(album, cx);
        }
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// A client exists now. This view is built during the pre-connect window,
    /// where `load_more` had nothing to fetch with and bailed — without this it
    /// would keep showing the seeded cache until the user scrolled.
    /// Re-read the header totals from the cache. Cheap — two aggregates over
    /// the album/artist tables — so it runs wherever the cache may have moved
    /// under the view, rather than being recomputed per frame.
    pub fn refresh_stats(&mut self, cx: &mut Context<Self>) {
        let libraries = self.session.read(cx).library_ids.clone();
        if let Ok(stats) = self.library_db.library_stats("navidrome", &libraries) {
            self.stats = stats;
        }
    }

    pub fn client_ready(&mut self, cx: &mut Context<Self>) {
        self.refresh_stats(cx);
        let tab = self.active_tab;
        if self.tabs.get(&tab).is_none_or(|t| t.page == 0) {
            self.load_more(tab, cx);
        }
        cx.notify();
    }

    fn select_tab(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        self.active_tab = tab;
        // The range is per tab's list; the new tab's cards at those indices are
        // different albums.
        self.art_range = None;
        if self.tabs.get(&tab).is_none_or(|t| t.albums.is_empty()) {
            self.seed_from_cache(tab, cx);
            self.load_more(tab, cx);
        }
        self.session.update(cx, |session, _| {
            session.settings.album_sort = tab;
            session.persist_settings();
        });
        cx.notify();
    }

    /// Ask for the page that failed again. The page counter is only advanced
    /// on success, so this re-requests exactly what was lost rather than
    /// skipping past it.
    fn retry_load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let tab = self.active_tab;
        self.load_more(tab, cx);
        cx.notify();
    }

    fn load_more(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let libraries = self.session.read(cx).library_query_ids();
        let state = self.tabs.entry(tab).or_default();
        if state.loading || state.exhausted {
            return;
        }
        if state.lib_exhausted.len() != libraries.len() {
            state.lib_exhausted = vec![false; libraries.len()];
            state.buffers = vec![VecDeque::new(); libraries.len()];
            state.lib_emitted = vec![0; libraries.len()];
        }
        let offset = state.page * PAGE_SIZE;
        let pending: Vec<(usize, Option<String>)> = libraries
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !state.lib_exhausted[*i])
            .collect();
        state.loading = true;
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            // One page per selected library at the same offset, merged in
            // selection order (the API takes one musicFolderId per request).
            let result = runtime::spawn_io(async move {
                let mut batches = Vec::with_capacity(pending.len());
                for (lib_index, lib) in pending {
                    let batch = client
                        .get_album_list2(tab_list_type(tab), PAGE_SIZE, offset, lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    batches.push((lib_index, batch));
                }
                Ok::<_, anyhow::Error>(batches)
            })
            .await;

            let _ = this.update(cx, |view, cx| {
                let mut new_albums = Vec::new();
                let state = view.tabs.entry(tab).or_default();
                state.loading = false;
                match result {
                    Ok(batches) => {
                        state.page += 1;
                        for (lib_index, batch) in batches {
                            if batch.len() < PAGE_SIZE as usize
                                && let Some(flag) = state.lib_exhausted.get_mut(lib_index)
                            {
                                *flag = true;
                            }
                            if tab == AlbumSort::Random {
                                // No order to preserve — shuffle the combined
                                // page below instead of buffering.
                                new_albums.extend(batch);
                            } else {
                                state.buffers[lib_index].extend(batch);
                            }
                        }
                        if tab == AlbumSort::Random {
                            use rand::seq::SliceRandom;
                            new_albums.shuffle(&mut rand::rng());
                        } else {
                            new_albums = merge_ready(state, tab);
                        }
                        state.exhausted = state.lib_exhausted.iter().all(|&e| e)
                            && state.buffers.iter().all(|b| b.is_empty());
                        apply_live_page(state, &new_albums);
                    }
                    Err(e) => view.error = Some(crate::errors::ErrorNote::new(&e)),
                }
                for album in &new_albums {
                    view.fetch_art(album, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the next page when the grid is scrolled near its bottom.
    fn maybe_load_more_on_scroll(&mut self, cx: &mut Context<Self>) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let scrolled = -base.offset().y;
        let max = base.max_offset().height;
        if max - scrolled < px(LOAD_AHEAD_PX) {
            self.load_more(self.active_tab, cx);
        }
    }

    /// Refresh the shared context-menu playlist list when it has actually
    /// changed. One comparison pass per frame replaces a clone of the whole
    /// list per visible card per frame.
    fn sync_menu_playlists(&mut self, cx: &App) {
        let playlists = &self.playlists.read(cx).playlists;
        let unchanged = playlists.len() == self.menu_playlists.len()
            && playlists
                .iter()
                .zip(self.menu_playlists.iter())
                .all(|(p, (id, name))| &p.id == id && &p.name == name);
        if !unchanged {
            self.menu_playlists = Rc::new(
                playlists
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect(),
            );
        }
    }

    /// Number of grid columns at the current window width.
    fn grid_cols(&mut self, window: &Window, cx: &App) -> usize {
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let settings = &self.session.read(cx).settings;
        let (min_tile, max_tile) = settings.cover_size.range();
        let gallery = settings.album_card_style == AlbumCardStyle::Gallery;
        self.live_width
            .grid(measured, min_tile, max_tile, window, FALLBACK_COLS, gallery)
            .0
    }

    /// Move the vi-mode cursor by `delta` grid positions, clamping and
    /// scrolling the focused card into view.
    pub fn vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self
            .tabs
            .get(&self.active_tab)
            .map(|t| t.albums.len())
            .unwrap_or(0);
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
        if self.active_tab == AlbumSort::Timeline {
            if let Some(mut row) = timeline_row_of(&self.timeline_rows, next) {
                // Going up into a month, bring its heading along.
                if delta < 0
                    && row > 0
                    && matches!(self.timeline_rows[row - 1], TimelineRow::Header { .. })
                {
                    row -= 1;
                }
                self.timeline_list.scroll_to_reveal_item(row);
            }
        } else {
            let cols = self.grid_cols(window, cx).max(1);
            // Download cards sit ahead of the albums in the same rows.
            let lead = self.incoming.len();
            self.scroll
                .scroll_to_item((next + lead) / cols, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Open the album under the vi-mode cursor.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .vi_cursor
            .and_then(|c| {
                self.tabs
                    .get(&self.active_tab)
                    .and_then(|t| t.albums.get(c))
            })
            .map(|a| a.id.clone())
        else {
            return;
        };
        cx.emit(AlbumsEvent::OpenAlbum(id));
    }

    pub fn vi_play(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.queue_album(id, QueueMode::Play, cx);
        }
    }

    pub fn vi_shuffle(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.queue_album(id, QueueMode::Shuffle, cx);
        }
    }

    fn focused_album_id(&self) -> Option<String> {
        self.vi_cursor
            .and_then(|c| {
                self.tabs
                    .get(&self.active_tab)
                    .and_then(|t| t.albums.get(c))
            })
            .map(|a| a.id.clone())
    }

    /// Cycle the filter tab (new/random/...) by `delta`.
    pub fn vi_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let idx = TABS.iter().position(|t| *t == self.active_tab).unwrap_or(0);
        let next = if delta > 0 {
            (idx + 1) % TABS.len()
        } else {
            (idx + TABS.len() - 1) % TABS.len()
        };
        self.select_tab(TABS[next], cx);
        cx.notify();
    }

    /// Fetch the album's songs and act on them (play / shuffle / queue).
    fn queue_album(&mut self, album_id: String, mode: QueueMode, cx: &mut Context<Self>) {
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
                    let _ = player.update(cx, |p, cx| match mode {
                        QueueMode::Play => p.play_queue(album.song, 0, cx),
                        QueueMode::Shuffle => p.play_queue_shuffled(album.song, cx),
                        QueueMode::PlayNext => p.play_next(album.song, cx),
                        QueueMode::Enqueue => p.enqueue(album.song, cx),
                    });
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

    /// Fetch the album's songs and append them all to a playlist.
    fn add_album_to_playlist(
        &mut self,
        album_id: String,
        playlist_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let playlists = self.playlists.clone();
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
                    let ids: Vec<String> = album.song.iter().map(|s| s.id.clone()).collect();
                    let _ = playlists.update(cx, |pl, cx| pl.add_songs(playlist_id, ids, cx));
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

    fn fetch_art(&mut self, album: &Album, cx: &mut Context<Self>) {
        if self.art_paths.contains_key(&album.id) {
            return;
        }
        let Some(cover_id) = album.cover_art.clone() else {
            return;
        };
        // Synchronous cache hit: show it immediately, no async round-trip.
        // This is what makes covers appear instantly on app restart instead
        // of blanking then popping in one task at a time.
        if let Some(path) = artwork::cached(&cover_id, self.art_px) {
            self.art_paths.insert(album.id.clone(), path);
            return;
        }
        // Miss: download in the background.
        let Some(client) = self.client(cx) else {
            return;
        };
        // Meanwhile, draw whatever rendition of this cover is already on disk.
        // The case that matters is the cover-size setting moving to another
        // rung: every card would otherwise blank until its new download lands,
        // for art that differs only in how many pixels it is scaled from. The
        // task below replaces it when the right size arrives.
        if let Some(path) = artwork::cached_best(&cover_id, self.art_px) {
            self.art_paths.insert(album.id.clone(), path);
        }
        let album_id = album.id.clone();
        let art_px = self.art_px;
        // Soft-cap the bag: oldest entries are the earliest-scrolled covers,
        // long since downloaded, so dropping their handles just frees memory.
        if self.art_tasks.len() > 256 {
            self.art_tasks.drain(0..128);
        }
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

    /// Coalesce cover-arrival repaints: a fast scroll completes many downloads
    /// in quick succession; batch them into ~one re-render per frame-ish rather
    /// than re-rendering the whole grid on every single completion.
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

    /// Drop cached thumbnail paths so the next frame refetches at the current
    /// `art_px` (called when the cover-size setting lands on a new rung).
    ///
    /// Only clears: the tab holds every album loaded so far, and refetching all
    /// of them queued a lookup — and on a miss a download — for thousands of
    /// cards nobody is looking at, ahead of the ones on screen.
    /// `ensure_art_for_viewport` refills what is visible.
    fn refetch_art(&mut self) {
        self.art_paths.clear();
        // Cancel in-flight downloads at the old resolution.
        self.art_tasks.clear();
        self.art_range = None;
    }

    /// Fetch covers for the rows on screen, plus a few past the edge.
    ///
    /// The grid's own art is normally fetched as pages land, which covers
    /// scrolling; this is what repopulates it after a resolution change, and
    /// what carries the seeded cache rows past `CACHE_ART_PREFETCH`.
    fn ensure_art_for_viewport(&mut self, row_count: usize, cols: usize, cx: &mut Context<Self>) {
        if cols == 0 || row_count == 0 {
            return;
        }
        let album_count = self
            .tabs
            .get(&self.active_tab)
            .map(|t| t.albums.len())
            .unwrap_or(0);
        if album_count == 0 {
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
        // Grid positions to album indices: download cards come first.
        let lead = self.incoming.len();
        let start = (first_row.saturating_sub(ART_LOOKAHEAD_ROWS) * cols).saturating_sub(lead);
        let end = ((last_row + 1 + ART_LOOKAHEAD_ROWS) * cols)
            .saturating_sub(lead)
            .min(album_count);
        if start >= end || self.art_range == Some((start, end)) {
            return;
        }
        self.art_range = Some((start, end));
        let window: Vec<Album> = self
            .tabs
            .get(&self.active_tab)
            .map(|t| t.albums[start..end].to_vec())
            .unwrap_or_default();
        for album in &window {
            self.fetch_art(album, cx);
        }
    }

    /// Regroup the timeline when its albums or column count moved, keeping
    /// the album at the top of the viewport where it was.
    fn sync_timeline(&mut self, cols: usize, grouping: TimelineGrouping, gallery: bool) {
        let (version, len) = self
            .tabs
            .get(&AlbumSort::Timeline)
            .map(|t| (t.version, t.albums.len()))
            .unwrap_or((0, 0));
        let sig = (version, len, cols, grouping, gallery);
        if self.timeline_sig == Some(sig) {
            return;
        }
        // Row heights only hold while the column count and card style do.
        let same_cols = self
            .timeline_sig
            .is_some_and(|(_, _, c, _, g)| c == cols && g == gallery);
        self.timeline_sig = Some(sig);

        let top = self.timeline_list.logical_scroll_top();
        let anchor = timeline_album_at(&self.timeline_rows, top.item_ix);
        let on_header = matches!(
            self.timeline_rows.get(top.item_ix),
            Some(TimelineRow::Header { .. })
        );
        let rows = self
            .tabs
            .get(&AlbumSort::Timeline)
            .map(|t| timeline_rows(&t.albums, cols, grouping))
            .unwrap_or_default();
        // `reset` forgets the scroll position; put it back by album rather
        // than by row, since a regroup moves every row index after a change.
        self.timeline_list.reset(rows.len());
        let scrolled = top.item_ix > 0 || top.offset_in_item > px(0.);
        if scrolled
            && let Some(album) = anchor
            && let Some(mut row) = timeline_row_of(&rows, album)
        {
            if on_header && row > 0 && matches!(rows[row - 1], TimelineRow::Header { .. }) {
                row -= 1;
            }
            self.timeline_list.scroll_to(ListOffset {
                item_ix: row,
                // Row heights only hold while the tile size does.
                offset_in_item: if same_cols {
                    top.offset_in_item
                } else {
                    px(0.)
                },
            });
        }
        self.timeline_rows = Rc::new(rows);
        self.art_range = None;
    }

    /// Timeline rows on screen (plus lookahead), as a row range, estimated
    /// from the scroll top and the height of a covers row — headings are
    /// shorter, so this errs towards too many.
    fn timeline_visible_rows(&self, row_h: f32) -> (usize, usize) {
        let top = self.timeline_list.logical_scroll_top().item_ix;
        let viewport = f32::from(self.timeline_list.viewport_bounds().size.height);
        let visible = if viewport > 0. && row_h > 0. {
            (viewport / row_h).ceil() as usize + 1
        } else {
            ART_LOOKAHEAD_ROWS * 2
        };
        (
            top.saturating_sub(ART_LOOKAHEAD_ROWS),
            (top + visible + ART_LOOKAHEAD_ROWS).min(self.timeline_rows.len()),
        )
    }

    fn ensure_timeline_art(&mut self, row_h: f32, cx: &mut Context<Self>) {
        let (first, last) = self.timeline_visible_rows(row_h);
        let rows = self.timeline_rows.clone();
        let mut covers = rows
            .get(first..last)
            .into_iter()
            .flatten()
            .filter_map(|r| match r {
                TimelineRow::Covers { start, end } => Some((*start, *end)),
                TimelineRow::Header { .. } => None,
            });
        let Some((start, mut end)) = covers.next() else {
            return;
        };
        if let Some((_, e)) = covers.next_back() {
            end = e;
        }
        if self.art_range == Some((start, end)) {
            return;
        }
        self.art_range = Some((start, end));
        let window: Vec<Album> = self
            .tabs
            .get(&AlbumSort::Timeline)
            .and_then(|t| t.albums.get(start..end))
            .map(<[Album]>::to_vec)
            .unwrap_or_default();
        for album in &window {
            self.fetch_art(album, cx);
        }
    }

    /// One gallery tile — the Timeline tab's, and every album in the grid
    /// under `AlbumCardStyle::Gallery`.
    fn render_tile(
        &self,
        entity: &Entity<Self>,
        index: usize,
        album: &Album,
        tile: f32,
        focused: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let settings = &self.session.read(cx).settings;
        let glow = settings.selection_glow_vi;
        let accent = if settings.selection_glow_album_color {
            art.as_ref().and_then(|p| {
                crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), &album.id, p)
            })
        } else {
            None
        };
        let open_view = entity.clone();
        let play_view = entity.clone();
        let play = Button::new(("tile-play", index))
            .primary()
            .xsmall()
            .icon(app_icon(icons::PLAY))
            .on_click(move |_, _, cx: &mut App| {
                play_view.update(cx, |this, cx| {
                    this.queue_album(play_id.clone(), QueueMode::Play, cx);
                });
                cx.stop_propagation();
            });
        let tile_el = crate::ui::gallery_tile(
            gpui::SharedString::from(format!("tile-album-{}", album.id)),
            tile,
            art,
            album.name.clone(),
            album.artist.clone().unwrap_or_default(),
            Some(play.into_any_element()),
            false,
            cx,
        )
        .on_click(move |_, _, cx: &mut App| {
            open_view.update(cx, |_, cx| cx.emit(AlbumsEvent::OpenAlbum(id.clone())));
        })
        .context_menu(self.album_menu(entity, album));
        with_focus_cursor(
            format!("vi-focus-{index}"),
            tile_el,
            focused,
            glow,
            accent,
            cx,
        )
    }

    /// The Timeline tab: date headings (`Settings::timeline_grouping`) over
    /// dense cover rows, newest first, with a year scrubber down the right
    /// edge.
    fn render_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = &self.session.read(cx).settings;
        let (min_tile, max_tile) = settings.cover_size.range();
        let grouping = settings.timeline_grouping;
        // Follows `album_card_style` like every other album grid.
        let gallery = settings.album_card_style == AlbumCardStyle::Gallery;
        let gap = crate::ui::grid_item_gap(gallery);
        let row_gap = crate::ui::grid_row_gap(gallery);
        let pad = crate::ui::grid_padding_x();
        let measured = f32::from(self.timeline_list.viewport_bounds().size.width);
        let width = self.timeline_width.resolve(measured, window) - pad;
        let fit = |width: f32, max_tile: f32| match gallery {
            true => crate::ui::tile_fit(width, min_tile, max_tile, gap),
            false => crate::ui::grid_fit(width, min_tile, max_tile),
        };
        let (cols, tile) = fit(width, max_tile).unwrap_or_else(|| {
            // First frame: guess low, like the card grid's fallback.
            let viewport = f32::from(window.viewport_size().width) - pad - TIMELINE_RAIL_W;
            let cols = fit(viewport, min_tile).map_or(1, |(c, _)| c);
            (cols.min(FALLBACK_COLS), min_tile)
        });
        self.sync_timeline(cols, grouping, gallery);
        // A card is its tile plus padding, the gap over its text and the text
        // block; only an estimate for how many rows are on screen.
        let item_w = match gallery {
            true => tile,
            false => tile + crate::ui::card_padding(),
        };
        let item_h = match gallery {
            true => tile,
            false => tile + crate::ui::card_padding() + 6. + TEXT_BLOCK_H,
        };
        let row_h = item_h + row_gap;
        self.ensure_timeline_art(row_h, cx);

        // Near the end of what's loaded: ask for more. Also what fills a
        // viewport the first page doesn't reach the bottom of.
        let (_, last_visible) = self.timeline_visible_rows(row_h);
        if self.error.is_none()
            && self.timeline_rows.len().saturating_sub(last_visible) < TIMELINE_LOAD_AHEAD_ROWS
        {
            self.load_more(AlbumSort::Timeline, cx);
        }

        let rows = self.timeline_rows.clone();
        let row_w = cols as f32 * item_w + cols.saturating_sub(1) as f32 * gap;
        let entity = cx.entity();
        let list_rows = rows.clone();
        let timeline = list(self.timeline_list.clone(), move |ix, _window, cx| {
            let view = entity.read(cx);
            match list_rows.get(ix) {
                Some(TimelineRow::Header { bucket, count }) => h_flex()
                    .w_full()
                    .justify_center()
                    .pt_4()
                    .pb_2()
                    .child(
                        h_flex()
                            .w(px(row_w))
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .text_base()
                                    .font_semibold()
                                    .child(bucket_label(*bucket, grouping)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "{count} album{}",
                                        if *count == 1 { "" } else { "s" }
                                    )),
                            ),
                    )
                    .into_any_element(),
                Some(TimelineRow::Covers { start, end }) => {
                    let tiles: Vec<_> = view
                        .tabs
                        .get(&AlbumSort::Timeline)
                        .and_then(|t| t.albums.get(*start..*end))
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .map(|(j, album)| {
                            let index = start + j;
                            let focused = view.vi_cursor == Some(index);
                            match gallery {
                                true => view.render_tile(&entity, index, album, tile, focused, cx),
                                false => view.render_card(&entity, index, album, tile, focused, cx),
                            }
                        })
                        .collect();
                    // Left-aligned inside a centred block, so a month's short
                    // last row lines up under the one above like a gallery's.
                    h_flex()
                        .w_full()
                        .justify_center()
                        .pb(px(row_gap))
                        .child(h_flex().w(px(row_w)).gap(px(gap)).children(tiles))
                        .into_any_element()
                }
                None => div().into_any_element(),
            }
        })
        .flex_1()
        .h_full()
        .px(px(pad / 2.));

        // Year scrubber: the first heading of each year, marking the one the
        // viewport is in.
        let top = self.timeline_list.logical_scroll_top().item_ix;
        let current_year = rows
            .get(..=top.min(rows.len().saturating_sub(1)))
            .into_iter()
            .flatten()
            .rev()
            .find_map(|r| match r {
                TimelineRow::Header {
                    bucket: Some((y, _, _)),
                    ..
                } => Some(*y),
                _ => None,
            });
        let mut years: Vec<(i32, usize)> = Vec::new();
        for (ix, row) in rows.iter().enumerate() {
            if let TimelineRow::Header {
                bucket: Some((y, _, _)),
                ..
            } = row
                && years.last().is_none_or(|(last, _)| last != y)
            {
                years.push((*y, ix));
            }
        }
        let rail = (years.len() > 1).then(|| {
            v_flex()
                .id("timeline-years")
                .flex_none()
                .w(px(TIMELINE_RAIL_W))
                .h_full()
                .overflow_y_scroll()
                .items_end()
                .pr_3()
                .pt_4()
                .gap_0p5()
                .children(years.into_iter().map(|(year, row)| {
                    let current = current_year == Some(year);
                    div()
                        .id(("timeline-year", row))
                        .px_1p5()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .map(|d| match current {
                            true => d.font_semibold().text_color(cx.theme().foreground),
                            false => d.text_color(cx.theme().muted_foreground),
                        })
                        .child(year.to_string())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.timeline_list.scroll_to(ListOffset {
                                item_ix: row,
                                offset_in_item: px(0.),
                            });
                            cx.notify();
                        }))
                }))
        });

        h_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(timeline)
            .children(rail)
            .into_any_element()
    }

    /// Right-click menu for one album, shared by the grid cards and the
    /// timeline tiles.
    fn album_menu(
        &self,
        entity: &Entity<Self>,
        album: &Album,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let view = entity.clone();
        let menu_id = album.id.clone();
        let menu_artists = crate::ui::artist_links(
            &album.artists,
            album.artist.as_deref(),
            album.artist_id.as_deref(),
        );
        // Shared, not rebuilt per card: the "Save to playlist" submenu needs the
        // whole list, and cloning every name into every visible card on every
        // frame is a resize's worth of allocations for a menu that is usually
        // closed.
        let menu_pl_list = self.menu_playlists.clone();
        move |menu, window, cx| {
            let act = |mode: QueueMode| {
                let view = view.clone();
                let id = menu_id.clone();
                move |_: &_, _: &mut Window, cx: &mut gpui::App| {
                    view.update(cx, |v, cx| v.queue_album(id.clone(), mode, cx));
                }
            };
            let pl_list = menu_pl_list.clone();
            let pl_view = view.clone();
            let pl_album = menu_id.clone();
            let mut menu = menu
                .item(PopupMenuItem::new("Play").on_click(act(QueueMode::Play)))
                .item(PopupMenuItem::new("Shuffle").on_click(act(QueueMode::Shuffle)))
                .item(PopupMenuItem::new("Play next").on_click(act(QueueMode::PlayNext)))
                .item(PopupMenuItem::new("Add to queue").on_click(act(QueueMode::Enqueue)))
                .submenu("Save to playlist", window, cx, move |sub, _w, _c| {
                    if pl_list.is_empty() {
                        return sub.item(PopupMenuItem::new("No playlists yet").disabled(true));
                    }
                    let mut sub = sub;
                    for (pid, pname) in pl_list.iter() {
                        let view = pl_view.clone();
                        let pid = pid.clone();
                        let album = pl_album.clone();
                        sub = sub.item(PopupMenuItem::new(pname.clone()).on_click(
                            move |_, _, cx: &mut gpui::App| {
                                view.update(cx, |v, cx| {
                                    v.add_album_to_playlist(album.clone(), pid.clone(), cx)
                                });
                            },
                        ));
                    }
                    sub
                });
            // One credit goes straight to that artist; a collaboration asks
            // which one, since `artistId` names only the primary credit and
            // silently sending every name there is the bug this replaces.
            match menu_artists.as_slice() {
                [] => {}
                [(_, aid)] => {
                    let view = view.clone();
                    let aid = aid.clone();
                    menu = menu.item(PopupMenuItem::separator()).item(
                        PopupMenuItem::new("Go to artist").on_click(
                            move |_, _, cx: &mut gpui::App| {
                                view.update(cx, |_, cx| {
                                    cx.emit(AlbumsEvent::OpenArtist(aid.clone()))
                                });
                            },
                        ),
                    );
                }
                _ => {
                    let artists = menu_artists.clone();
                    let art_view = view.clone();
                    menu = menu.item(PopupMenuItem::separator()).submenu(
                        "Go to artist",
                        window,
                        cx,
                        move |sub, _w, _c| {
                            let mut sub = sub;
                            for (name, aid) in artists.iter() {
                                let view = art_view.clone();
                                let aid = aid.clone();
                                sub = sub.item(PopupMenuItem::new(name.clone()).on_click(
                                    move |_, _, cx: &mut gpui::App| {
                                        view.update(cx, |_, cx| {
                                            cx.emit(AlbumsEvent::OpenArtist(aid.clone()))
                                        });
                                    },
                                ));
                            }
                            sub
                        },
                    );
                }
            }
            menu
        }
    }

    /// Lidarr's downloads for the All and New tabs, their covers asked for.
    fn sync_incoming(&mut self, cx: &mut Context<Self>) {
        let lidarr = crate::state::lidarr::lidarr(cx);
        let incoming = match self.active_tab {
            AlbumSort::All | AlbumSort::New => lidarr.read(cx).incoming(),
            _ => Vec::new(),
        };
        if *self.incoming != incoming {
            // Every album moved along the grid; the cover window with it.
            if incoming.len() != self.incoming.len() {
                self.art_range = None;
            }
            self.incoming = Rc::new(incoming);
        }
        let Some(client) = lidarr.read(cx).client.clone() else {
            return;
        };
        let size = self.art_px;
        for item in self.incoming.clone().iter() {
            self.lidarr_covers.want(
                &client,
                &item.album,
                size,
                |v: &mut Self| &mut v.lidarr_covers,
                cx,
            );
        }
    }

    /// A download's card: the cover dimmed, lit from the left as far as the
    /// download has got; title, artist and what Lidarr is doing under it.
    /// Same footprint as an album card, so the rows stay uniform.
    fn render_incoming(
        &self,
        entity: &Entity<Self>,
        item: &Incoming,
        tile: f32,
        gallery: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let album_id = item.album.id;
        let art = self.lidarr_covers.get(album_id);
        let flush = !self.session.read(cx).settings.classic_album_cards;
        let round = move |el: gpui::Div| match gallery {
            true => el.rounded_md(),
            false => crate::ui::cover_rounding(el, flush),
        };
        let round_img = move |el: gpui::Img| match gallery {
            true => el.rounded_md(),
            false => crate::ui::cover_rounding(el, flush),
        };
        let edge = match gallery {
            true => tile,
            false => crate::ui::card_cover_edge(tile, flush),
        };
        let progress = item.progress.clamp(0., 1.);
        // One front runs across the whole card, cover and text block alike;
        // the cover is lit as far as the front has reached into it. Whole
        // pixels, or the edge shimmers as the width creeps.
        let card_w = match gallery {
            true => tile,
            false => tile + crate::ui::card_padding(),
        };
        let front = (card_w * progress).floor();
        // Where the cover starts inside the card: past the 1px border, and
        // the inset too when the cover doesn't take it.
        let cover_left = match (gallery, flush) {
            (true, _) => 0.,
            (false, true) => 1.,
            (false, false) => 1. + crate::ui::card_inset(),
        };
        let lit = (front - cover_left).clamp(0., edge);
        // The cover's own colour, once it is here.
        let accent = art
            .as_ref()
            .and_then(|path| {
                crate::ui::album_glow_accent(
                    &mut self.glow_accents.borrow_mut(),
                    &format!("lidarr-{album_id}"),
                    path,
                )
            })
            .unwrap_or(cx.theme().primary);
        let status: gpui::SharedString = match progress {
            p if p > 0. && p < 1. => format!("{} · {}%", item.label, (p * 100.) as u32).into(),
            _ => item.label.into(),
        };
        let status_color = match item.problem {
            true => cx.theme().danger,
            false => cx.theme().muted_foreground,
        };

        // The cover's corners, per side: a gallery tile is rounded all round,
        // a card cover at the top (and the bottom too unless flush).
        let square_bottom = !gallery && crate::ui::cover_square_bottom(flush);
        let round_left = move |el: gpui::Div| match (gallery, square_bottom) {
            (true, _) => el.rounded_tl_md().rounded_bl_md(),
            (false, true) => el.rounded_tl_lg(),
            (false, false) => el.rounded_tl_lg().rounded_bl_lg(),
        };
        let round_right = move |el: gpui::Div| match (gallery, square_bottom) {
            (true, _) => el.rounded_tr_md().rounded_br_md(),
            (false, true) => el.rounded_tr_lg(),
            (false, false) => el.rounded_tr_lg().rounded_br_lg(),
        };
        let muted = cx.theme().muted;
        // The front's own corners: the top one on a card, where the card's
        // fill carries the front on down; both on a gallery tile.
        let fillet = FRONT_RADIUS.min(lit).min(edge - lit);
        let moving = fillet > 0.;

        // Bright cover with the part still to come dimmed by the well's own
        // colour at 0.7 — the same pixels as the art at 0.3 over the well. An
        // overlay, not a clipped bright copy: clipping is rectangular, and
        // the overlay's fillets are what round the lit part's front.
        let well = round(div())
            .flex_none()
            .size(px(edge))
            .relative()
            .overflow_hidden()
            .bg(muted)
            .when(!gallery, |w| w.shadow_sm())
            .map(|w| match art {
                Some(path) => w
                    .child(round_img(img(path).size(px(edge))))
                    .when(lit < edge, |w| {
                        let overlay = div()
                            .absolute()
                            .top_0()
                            .left(px(lit))
                            .w(px(edge - lit))
                            .h(px(edge))
                            .bg(muted.opacity(0.7));
                        // Hardly started: its left edge is still inside the
                        // cover's own corners.
                        let overlay = match lit < FRONT_RADIUS {
                            true => round_left(overlay),
                            false => overlay,
                        };
                        w.child(round_right(overlay))
                    })
                    .when(moving, |w| {
                        w.child(front_fillet(lit, 0., true, fillet, muted.opacity(0.7)))
                            .when(gallery, |w| {
                                w.child(front_fillet(
                                    lit,
                                    edge - fillet,
                                    false,
                                    fillet,
                                    muted.opacity(0.7),
                                ))
                            })
                    }),
                None => w.when(lit > 0., |w| {
                    let fill = div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h(px(edge))
                        .w(px(lit))
                        .bg(accent.opacity(0.3));
                    let fill = match lit >= edge {
                        true => round(fill),
                        false => round_left(fill)
                            .rounded_tr(px(fillet))
                            .when(gallery, |f| f.rounded_br(px(fillet))),
                    };
                    w.child(fill)
                }),
            });

        let open_view = entity.clone();
        let open = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            open_view.update(cx, |_, cx| cx.emit(AlbumsEvent::OpenLidarrAlbum(album_id)));
        };
        let id = gpui::SharedString::from(format!("incoming-{album_id}"));
        let title = item.album.title.clone();
        let artist = item.artist.clone();

        if gallery {
            return well
                .id(id)
                .cursor_pointer()
                .active(|s| s.opacity(0.8))
                .on_click(open)
                .child(
                    v_flex()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .px_2()
                        .py_1p5()
                        .bg(gpui::hsla(0., 0., 0., 0.6))
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::white())
                                .truncate()
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(match item.problem {
                                    true => cx.theme().danger,
                                    false => gpui::hsla(0., 0., 1., 0.75),
                                })
                                .truncate()
                                .child(status),
                        ),
                )
                .into_any_element();
        }

        v_flex()
            .id(id)
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
            .relative()
            .hover(|s| s.bg(cx.theme().muted))
            .active(|s| s.opacity(0.8))
            .on_click(open)
            // The card's fill in the cover's colour, behind the cover and the
            // text (its top corner is the cover's, rounded there); absolute
            // children sit inside the 1px border.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px((front - 1.).clamp(0., card_w - 2.)))
                    .rounded_lg()
                    .bg(accent.opacity(0.22)),
            )
            .child(well)
            .child(
                v_flex()
                    // Same block as an album card's, so the row height holds.
                    .h(px(TEXT_BLOCK_H))
                    .map(|t| match flush {
                        true => t
                            .px(px(crate::ui::card_inset()))
                            .pb(px(crate::ui::card_inset())),
                        false => t,
                    })
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(artist),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(status_color)
                            .truncate()
                            .child(status),
                    ),
            )
            .into_any_element()
    }

    fn render_card(
        &self,
        entity: &Entity<Self>,
        index: usize,
        album: &Album,
        tile: f32,
        focused: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let name = album.name.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        let open_view = entity.clone();
        let play_view = entity.clone();
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        let accent = if self.session.read(cx).settings.selection_glow_album_color {
            art.as_ref().and_then(|p| {
                crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), &album.id, p)
            })
        } else {
            None
        };

        // The cover takes the card's inset for itself, or sits inside it; the
        // border stays either way, since it is what separates one card from the
        // next. The card's own width is the same either way, so the grid's
        // columns do not move when the setting does.
        let flush = !self.session.read(cx).settings.classic_album_cards;
        let cover = crate::ui::card_cover_edge(tile, flush);

        let card = v_flex()
            .id(gpui::SharedString::from(format!("album-{}", album.id)))
            .group("acard")
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
            .on_click(move |_, _, cx: &mut App| {
                open_view.update(cx, |_, cx| cx.emit(AlbumsEvent::OpenAlbum(id.clone())));
            })
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
                    // Hover play button over the artwork.
                    .child(
                        div()
                            .absolute()
                            .bottom_2()
                            .right_2()
                            .opacity(0.)
                            .group_hover("acard", |s| s.opacity(1.))
                            .child(
                                Button::new(("card-play", index))
                                    .primary()
                                    .icon(app_icon(icons::PLAY))
                                    .on_click(move |_, _, cx: &mut App| {
                                        play_view.update(cx, |this, cx| {
                                            this.queue_album(play_id.clone(), QueueMode::Play, cx);
                                        });
                                        cx.stop_propagation();
                                    }),
                            ),
                    ),
            )
            .child(
                v_flex()
                    // Fixed height (fits a two-line name + artist + optional
                    // year) so cards stay uniform — required for the
                    // virtualized row list. Line heights are set explicitly:
                    // the default line box is tight enough to clip descenders
                    // (y, g, j) inside the overflow-hidden text block.
                    .h(px(TEXT_BLOCK_H))
                    // Flush, the card has no padding of its own for the text to
                    // sit in — the cover took it — so the text block carries
                    // its own, or the title runs into the card's edge and,
                    // with no border to read it against, into the next card.
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
                            // Long titles wrap onto a second line instead of
                            // being cut mid-word; anything longer is clipped.
                            .max_h(px(NAME_LINE_H * 2.))
                            .overflow_hidden()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .child(name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(artist),
                    )
                    .when(!year.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .line_height(px(META_LINE_H))
                                .text_color(cx.theme().muted_foreground)
                                .child(year),
                        )
                    }),
            )
            .context_menu(self.album_menu(entity, album));
        with_focus_cursor(format!("vi-focus-{index}"), card, focused, glow, accent, cx)
    }
}

impl Render for AlbumsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;
        self.sync_menu_playlists(cx);

        // Pick up cover-size changes: refetch art at the new resolution. The
        // fetch resolution follows the *setting*, not the tile the window ends
        // up picking inside its range, so a resize never invalidates art — and
        // it is compared at the rung art is actually stored at, so the two
        // settings that share one (Medium and Large) don't invalidate it
        // either.
        let cover = self.session.read(cx).settings.cover_size;
        let (min_tile, max_tile) = cover.range();
        let want_px = artwork::bucket(cover.art_px());
        if want_px != self.art_px {
            self.art_px = want_px;
            self.refetch_art();
        }

        // The filled primary button is the whole selection marker. An
        // underline used to ride under the strip as well, but it was placed in
        // even fractions of the row while the buttons are label-width, so it
        // never lined up with the tab it marked.
        let tabs = h_flex().gap_1().children(TABS.iter().map(|&tab| {
            Button::new(tab_label(tab))
                .ghost()
                .xsmall()
                .label(tab_label(tab))
                .when(active == tab, |b: Button| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
        }));

        // If the loaded content doesn't fill the viewport (no scrollbar yet),
        // keep fetching until it does or the list is exhausted.
        let base = self.scroll.0.borrow().base_handle.clone();
        let needs_fill = active != AlbumSort::Timeline
            && self
                .tabs
                .get(&active)
                .is_some_and(|t| !t.loading && !t.exhausted && !t.albums.is_empty())
            && base.max_offset().height <= px(0.);
        if needs_fill {
            self.load_more(active, cx);
        }

        let (album_count, fetching, cached) = self
            .tabs
            .get(&active)
            .map(|t| (t.albums.len(), t.loading, t.cached))
            .unwrap_or((0, false, false));
        // The connect itself is part of the wait: before it lands there is no
        // client to fetch with, so `loading` is false while the grid is still
        // very much not up to date.
        let connecting = self.session.read(cx).status == ConnectionStatus::Connecting;
        let loading = fetching || connecting;
        // Cards on screen are last sync's copy, not the server's answer yet.
        let showing_cache = cached && loading;
        // Exactly one progress indicator at a time: the header carries the
        // first/refresh load (it stays visible while cached cards already fill
        // the page, where a stale-but-complete grid would otherwise look
        // final), the floating one carries pagination further down the list,
        // where the header has scrolled out of reach.
        let paginating = loading && !showing_cache && album_count > 0;
        let header_loading = loading && !paginating;

        let body = if active == AlbumSort::Timeline {
            self.render_timeline(window, cx)
        } else {
            // Columns *and* the tile they're drawn at, from this frame's window
            // width: the covers grow inside the setting's range to spend what would
            // otherwise be left as gutters. Falls back to a guess on the very first
            // frame (before anything is laid out), then self-corrects.
            let gallery =
                self.session.read(cx).settings.album_card_style == AlbumCardStyle::Gallery;
            let (cols, tile) = self.live_width.grid(
                f32::from(base.bounds().size.width),
                min_tile,
                max_tile,
                window,
                FALLBACK_COLS,
                gallery,
            );
            self.sync_incoming(cx);
            let lead = self.incoming.len();
            let row_count = (album_count + lead).div_ceil(cols);
            self.ensure_art_for_viewport(row_count, cols, cx);

            let entity = cx.entity();
            uniform_list("albums-grid", row_count, move |range, _window, cx| {
                let view = entity.read(cx);
                let albums = view
                    .tabs
                    .get(&active)
                    .map(|t| t.albums.as_slice())
                    .unwrap_or_default();
                range
                    .map(|row| {
                        // Grid positions: download cards, then the albums.
                        let start = row * cols;
                        let end = ((row + 1) * cols).min(albums.len() + lead);
                        let cards: Vec<_> = (start..end)
                            .map(|pos| {
                                if let Some(item) = view.incoming.get(pos) {
                                    return view.render_incoming(&entity, item, tile, gallery, cx);
                                }
                                let card_index = pos - lead;
                                let album = &albums[card_index];
                                let focused = view.vi_cursor == Some(card_index);
                                match gallery {
                                    true => view
                                        .render_tile(&entity, card_index, album, tile, focused, cx),
                                    false => view
                                        .render_card(&entity, card_index, album, tile, focused, cx),
                                }
                            })
                            .collect();
                        // Centered so the ragged last row's leftover space splits
                        // evenly — left/right gutters stay equal at any width.
                        h_flex()
                            .w_full()
                            .gap(px(crate::ui::grid_item_gap(gallery)))
                            .justify_center()
                            .pb(px(crate::ui::grid_row_gap(gallery)))
                            .children(cards)
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            // Half of `grid_padding_x`, which is the pair; `grid_columns_padded`
            // takes the whole of it back off the element's own bounds.
            .px(px(crate::ui::grid_padding_x() / 2.))
            .track_scroll(self.scroll.clone())
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.maybe_load_more_on_scroll(cx);
            }))
            .into_any_element()
        };

        // No bottom padding: the grid runs to the window edge so rows slide
        // under the player bar instead of stopping short of it with a gap.
        v_flex()
            .id("albums-scroll")
            .size_full()
            .relative()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_2()
                    .px_4()
                    // Extra margin sets the caption apart from the sort pills.
                    .child(div().text_lg().mr_4().child("Albums"))
                    .child(tabs)
                    // Spinner sits in the header rather than over the grid so
                    // it's visible while cached cards are already filling the
                    // page — otherwise a stale-but-complete grid looks final.
                    .when(header_loading, |this| {
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
                    // Totals ride the right edge; the spinner keeps its place
                    // beside the tabs so it reads as part of the listing.
                    // Nothing to summarise until a sync has written rows —
                    // zeros next to a grid full of live cards read as a bug.
                    .when(self.stats.albums > 0, |this| {
                        this.child(
                            // Fills the rest of the line and right-aligns, rather
                            // than `ml_auto`: taffy left the auto-margin text ~2rem
                            // short of the row's padding in this wrapping row.
                            h_flex()
                                .flex_1()
                                .justify_end()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::ui::library_summary(
                                    (self.stats.albums, "album"),
                                    &self.stats,
                                )),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, note| {
                this.child(crate::ui::error_banner(
                    &note,
                    cx.listener(|view, _, _, cx| view.retry_load(cx)),
                    cx,
                ))
            })
            .child(body)
            // Pagination indicator, floated over the grid's bottom edge so it
            // doesn't shorten the scroll area. Only for further pages: on the
            // first load and while cached cards are showing, the header spinner
            // is the single signal.
            .when(paginating, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .right_0()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Loading…"),
                        ),
                )
            })
    }
}

/// One rounded corner of a download card's lit front at `x`: an `r` square
/// just left of the front, painted `color` outside a quarter circle by a ring
/// centred on the corner's inner point (clipping is rectangular, so a curve
/// has to be painted, not cut). `top` picks which way the circle opens.
fn front_fillet(x: f32, y: f32, top: bool, r: f32, color: gpui::Hsla) -> gpui::Div {
    // Wide enough to reach the square's far corner, r·√2 from the centre.
    let ring = r;
    div()
        .absolute()
        .left(px(x - r))
        .top(px(y))
        .size(px(r))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left(px(-(r + ring)))
                .top(px(if top { -ring } else { -(r + ring) }))
                .size(px(2. * (r + ring)))
                .rounded_full()
                .border(px(ring))
                .border_color(color),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str) -> Album {
        Album {
            id: id.into(),
            name: id.into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: None,
            duration: None,
            created: None,
            year: None,
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

    fn ids(state: &TabState) -> Vec<String> {
        state.albums.iter().map(|a| a.id.clone()).collect()
    }

    fn seeded(n: usize) -> TabState {
        TabState {
            albums: (0..n).map(|i| album(&format!("c{i}"))).collect(),
            cached: true,
            ..Default::default()
        }
    }

    fn added(id: &str, created: Option<&str>) -> Album {
        Album {
            created: created.map(Into::into),
            ..album(id)
        }
    }

    #[test]
    fn added_date_reads_iso_stamps() {
        assert_eq!(added_date(Some("2024-03-09T12:00:00Z")), Some((2024, 3, 9)));
        assert_eq!(added_date(Some("2024-13-01")), None);
        assert_eq!(added_date(Some("2024-03")), None);
        assert_eq!(added_date(Some("20240309")), None);
        assert_eq!(added_date(Some("")), None);
        assert_eq!(added_date(None), None);
    }

    #[test]
    fn civil_days_round_trip() {
        assert_eq!(days_from_civil((1970, 1, 1)), 0);
        assert_eq!(days_from_civil((2000, 3, 1)), 11_017);
        for days in [-800_000, -1, 0, 59, 11_016, 20_724, 400_000] {
            assert_eq!(days_from_civil(civil_from_days(days)), days);
        }
        assert_eq!(
            civil_from_days(days_from_civil((2024, 2, 29))),
            (2024, 2, 29)
        );
        // 2026-09-28 is a Monday, 1970-01-01 a Thursday.
        assert_eq!(weekday((2026, 9, 28)), 0);
        assert_eq!(weekday((1970, 1, 1)), 3);
    }

    #[test]
    fn buckets_start_their_day_week_month_or_year() {
        let d = (2026, 1, 3); // a Saturday
        assert_eq!(bucket_of(d, TimelineGrouping::Day), d);
        // Weeks start on Monday, across the year boundary.
        assert_eq!(bucket_of(d, TimelineGrouping::Week), (2025, 12, 29));
        assert_eq!(
            bucket_of((2025, 12, 29), TimelineGrouping::Week),
            (2025, 12, 29)
        );
        assert_eq!(bucket_of(d, TimelineGrouping::Month), (2026, 1, 1));
        assert_eq!(bucket_of(d, TimelineGrouping::Year), (2026, 1, 1));
    }

    #[test]
    fn bucket_labels() {
        let b = Some((2026, 9, 28));
        assert_eq!(
            bucket_label(b, TimelineGrouping::Day),
            "Monday, 28 September 2026"
        );
        assert_eq!(
            bucket_label(b, TimelineGrouping::Week),
            "Week of 28 September 2026"
        );
        assert_eq!(
            bucket_label(Some((2026, 9, 1)), TimelineGrouping::Month),
            "September 2026"
        );
        assert_eq!(
            bucket_label(Some((2026, 1, 1)), TimelineGrouping::Year),
            "2026"
        );
        assert_eq!(bucket_label(None, TimelineGrouping::Month), "Date unknown");
    }

    #[test]
    fn timeline_groups_months_and_splits_rows() {
        let albums = [
            added("a", Some("2026-09-20T00:00:00Z")),
            added("b", Some("2026-09-01T00:00:00Z")),
            added("c", Some("2026-09-01T00:00:00Z")),
            added("d", Some("2026-08-31T00:00:00Z")),
            added("e", None),
        ];
        let rows = timeline_rows(&albums, 2, TimelineGrouping::Month);
        assert_eq!(
            rows,
            [
                TimelineRow::Header {
                    bucket: Some((2026, 9, 1)),
                    count: 3
                },
                TimelineRow::Covers { start: 0, end: 2 },
                TimelineRow::Covers { start: 2, end: 3 },
                TimelineRow::Header {
                    bucket: Some((2026, 8, 1)),
                    count: 1
                },
                TimelineRow::Covers { start: 3, end: 4 },
                TimelineRow::Header {
                    bucket: None,
                    count: 1
                },
                TimelineRow::Covers { start: 4, end: 5 },
            ]
        );
        assert_eq!(timeline_row_of(&rows, 2), Some(2));
        assert_eq!(timeline_row_of(&rows, 4), Some(6));
        assert_eq!(timeline_row_of(&rows, 9), None);
        // A heading anchors on the first album under it.
        assert_eq!(timeline_album_at(&rows, 3), Some(3));
        assert_eq!(timeline_album_at(&rows, 7), None);
        assert!(timeline_rows(&[], 4, TimelineGrouping::Month).is_empty());
    }

    #[test]
    fn timeline_grouping_changes_the_headings() {
        let albums = [
            added("a", Some("2026-09-02T00:00:00Z")),
            added("b", Some("2026-09-01T00:00:00Z")),
            added("c", Some("2026-08-31T00:00:00Z")),
            added("d", Some("2026-08-30T00:00:00Z")),
        ];
        let headings = |g| {
            timeline_rows(&albums, 4, g)
                .into_iter()
                .filter_map(|r| match r {
                    TimelineRow::Header { bucket, count } => Some((bucket, count)),
                    TimelineRow::Covers { .. } => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(headings(TimelineGrouping::Day).len(), 4);
        // Mon 31 Aug – Sun 6 Sep holds three; Sun 30 Aug is the week before.
        assert_eq!(
            headings(TimelineGrouping::Week),
            [(Some((2026, 8, 31)), 3), (Some((2026, 8, 24)), 1)]
        );
        assert_eq!(headings(TimelineGrouping::Month).len(), 2);
        assert_eq!(headings(TimelineGrouping::Year), [(Some((2026, 1, 1)), 4)]);
    }

    #[test]
    fn uncached_pages_append() {
        let mut state = TabState::default();
        apply_live_page(&mut state, &[album("a"), album("b")]);
        apply_live_page(&mut state, &[album("c")]);
        assert_eq!(ids(&state), ["a", "b", "c"]);
        assert!(!state.cached);
    }

    #[test]
    fn live_pages_overwrite_cache_without_changing_row_count() {
        let mut state = seeded(6);
        apply_live_page(&mut state, &[album("l0"), album("l1")]);
        // Same length: the scroll extent must not move mid-load.
        assert_eq!(ids(&state), ["l0", "l1", "c2", "c3", "c4", "c5"]);
        apply_live_page(&mut state, &[album("l2"), album("l3")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2", "l3", "c4", "c5"]);
        assert!(state.cached);
    }

    #[test]
    fn exhausted_page_drops_the_stale_cached_tail() {
        let mut state = seeded(6);
        state.exhausted = true;
        apply_live_page(&mut state, &[album("l0"), album("l1")]);
        assert_eq!(ids(&state), ["l0", "l1"]);
        assert!(
            !state.cached,
            "cache is fully replaced once the server ends"
        );
    }

    #[test]
    fn live_list_longer_than_cache_grows_past_it() {
        let mut state = seeded(2);
        apply_live_page(&mut state, &[album("l0"), album("l1"), album("l2")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2"]);
        // Subsequent pages append normally once past the cached tail.
        apply_live_page(&mut state, &[album("l3")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2", "l3"]);
    }

    #[test]
    fn empty_final_page_truncates_to_what_the_server_sent() {
        let mut state = seeded(4);
        apply_live_page(&mut state, &[album("l0")]);
        state.exhausted = true;
        apply_live_page(&mut state, &[]);
        assert_eq!(ids(&state), ["l0"]);
    }

    #[test]
    fn cached_rows_strip_the_sync_id_namespace() {
        let row = AlbumRow {
            id: "navidrome:album:42".into(),
            source: "navidrome".into(),
            title: "Kid A".into(),
            artist: Some("Radiohead".into()),
            artist_id: Some("navidrome:artist:7".into()),
            year: Some(2000),
            cover_art: Some("al-42".into()),
            song_count: 10,
            duration: 2000.0,
            created: Some("2024-01-01T00:00:00Z".into()),
            play_count: Some(3),
            starred: None,
            library_id: Some("1".into()),
            release_types: Vec::new(),
        };
        let a = album_from_row(row);
        // Ids must match the live listing's, or the swap reloads every cover
        // and the cards navigate to nothing.
        assert_eq!(a.id, "42");
        assert_eq!(a.artist_id.as_deref(), Some("7"));
        assert_eq!(a.name, "Kid A");
    }
}
