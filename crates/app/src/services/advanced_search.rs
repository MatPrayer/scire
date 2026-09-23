//! The full-page search's query model.
//!
//! The palette (`ui::search_bar`) answers "the thing I am reaching for, now":
//! a handful of rows out of the cache, merged with whatever `search3` adds,
//! capped at a few per section. This is the other half — every row that
//! matches, narrowed by field rather than by typing more words.
//!
//! It is **cache-only**, deliberately. `search3` takes no filter beyond the
//! query text and caps its own response, so a year range or a format applied to
//! the cache rows and not to the server's would mean two different things in
//! one list. The cache holds the last sync's whole catalog plus every locally
//! scanned file, which is also the only search a local-only library ever gets.
//!
//! The SQL is built here rather than in `library_db` so the clause assembly is
//! testable without a database: `Filters::sql` is pure, and the tests below are
//! about which rows a filter is *capable* of excluding.

#[cfg(test)]
use rusqlite::Connection;
use rusqlite::ToSql;

use super::library_db::{
    AlbumRow, ArtistRow, LibraryDb, TrackRow, like_clause, like_terms, track_from_row,
};

/// Which table a search runs against. The filters that mean nothing for a kind
/// are ignored here and disabled in the UI, rather than silently matching
/// everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKind {
    /// Artists, albums and tracks at once — what the palette shows, without its
    /// caps. The default, because "search everything" is the question somebody
    /// arriving at a search page has; the single kinds are the narrowing.
    #[default]
    All,
    Albums,
    Artists,
    Tracks,
}

impl SearchKind {
    pub const ALL: [SearchKind; 4] = [
        SearchKind::All,
        SearchKind::Albums,
        SearchKind::Artists,
        SearchKind::Tracks,
    ];

    /// The kinds `All` is made of, in the order the page lists them — the
    /// palette's own order, so the two searches read the same way.
    pub const EVERY: [SearchKind; 3] =
        [SearchKind::Artists, SearchKind::Albums, SearchKind::Tracks];

    pub fn label(self) -> &'static str {
        match self {
            SearchKind::All => "Everything",
            SearchKind::Albums => "Albums",
            SearchKind::Artists => "Artists",
            SearchKind::Tracks => "Tracks",
        }
    }

    /// Whether a filter applies at all. An artist row carries a name, a source
    /// and a library and nothing else, so everything but those three is inert.
    ///
    /// Under `All` a filter one of the three cannot express does not quietly
    /// match everything: that kind is dropped from the results instead
    /// ([`Filters::kinds`]), so the filter still means one thing in one list.
    pub fn supports_year(self) -> bool {
        !matches!(self, SearchKind::Artists)
    }
    pub fn supports_genre(self) -> bool {
        !matches!(self, SearchKind::Artists)
    }
    pub fn supports_duration(self) -> bool {
        !matches!(self, SearchKind::Artists)
    }
    /// Only the album table records a star; a track's is not synced.
    pub fn supports_starred(self) -> bool {
        matches!(self, SearchKind::Albums | SearchKind::All)
    }
    /// Format and bitrate live on the track rows. For an album they are asked
    /// of its tracks, which is still an honest question — "an album I have a
    /// FLAC of".
    pub fn supports_technical(self) -> bool {
        !matches!(self, SearchKind::Artists)
    }
}

/// Where a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceFilter {
    #[default]
    All,
    Server,
    Local,
}

impl SourceFilter {
    pub const ALL: [SourceFilter; 3] =
        [SourceFilter::All, SourceFilter::Server, SourceFilter::Local];

    pub fn label(self) -> &'static str {
        match self {
            SourceFilter::All => "All sources",
            SourceFilter::Server => "Server",
            SourceFilter::Local => "Local files",
        }
    }

    fn source_value(self) -> Option<&'static str> {
        match self {
            SourceFilter::All => None,
            SourceFilter::Server => Some(super::library_db::SOURCE_NAVIDROME),
            SourceFilter::Local => Some(super::library_db::SOURCE_LOCAL),
        }
    }
}

/// How the result list is ordered.
///
/// `Relevance` is the only one not expressible in SQL — it is the palette's own
/// tiered ranking over the query text, applied to the rows once they are back —
/// so it yields no `ORDER BY` at all and the caller sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Relevance,
    Title,
    Artist,
    Year,
    Added,
    Duration,
    Plays,
}

impl SortBy {
    pub const ALL: [SortBy; 7] = [
        SortBy::Relevance,
        SortBy::Title,
        SortBy::Artist,
        SortBy::Year,
        SortBy::Added,
        SortBy::Duration,
        SortBy::Plays,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Relevance => "Relevance",
            SortBy::Title => "Title",
            SortBy::Artist => "Artist",
            SortBy::Year => "Year",
            SortBy::Added => "Recently added",
            SortBy::Duration => "Length",
            SortBy::Plays => "Most played",
        }
    }

    /// Whether this key exists on the kind's table. An artist row has a name
    /// and nothing to sort by beyond it.
    ///
    /// A sort under `All` is applied to each of the three lists separately, and
    /// a list whose table does not carry the key falls back to the title there
    /// — so every key is offered, rather than the union collapsing to the two
    /// an artist row can be ordered by.
    pub fn applies_to(self, kind: SearchKind) -> bool {
        match self {
            SortBy::Relevance | SortBy::Title => true,
            _ if kind == SearchKind::All => true,
            SortBy::Artist => kind != SearchKind::Artists,
            SortBy::Year | SortBy::Duration => kind != SearchKind::Artists,
            SortBy::Added => kind == SearchKind::Albums,
            SortBy::Plays => kind != SearchKind::Artists,
        }
    }

    /// The column, or `None` for relevance (ranked in Rust).
    ///
    /// Every nullable key sorts its NULLs last whichever direction is asked
    /// for: SQLite orders NULL below every value, so a descending sort by year
    /// otherwise opens with every album whose year is unknown — the rows that
    /// answer the question least.
    fn order_by(self, kind: SearchKind, desc: bool) -> Option<String> {
        let dir = if desc { "DESC" } else { "ASC" };
        let nullable = |col: &str| Some(format!("{col} IS NULL, {col} {dir}"));
        match (self, kind) {
            (SortBy::Relevance, _) => None,
            (SortBy::Title, SearchKind::Artists) => Some(format!("name COLLATE NOCASE {dir}")),
            (SortBy::Title, _) => Some(format!("title COLLATE NOCASE {dir}")),
            (SortBy::Artist, _) => nullable("artist COLLATE NOCASE"),
            (SortBy::Year, _) => nullable("year"),
            (SortBy::Added, _) => nullable("created"),
            (SortBy::Duration, _) => nullable("duration"),
            (SortBy::Plays, _) => nullable("play_count"),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

/// Everything the page narrows by, beside the query text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filters {
    pub kind: SearchKind,
    pub genre: Option<String>,
    pub year_min: Option<i32>,
    pub year_max: Option<i32>,
    /// Seconds.
    pub duration_min: Option<f64>,
    pub duration_max: Option<f64>,
    pub source: SourceFilter,
    pub starred_only: bool,
    /// File extension, as the scanner and the server spell it (`flac`, `mp3`).
    pub format: Option<String>,
    pub bitrate_min: Option<i64>,
    /// Selected music folders. Empty means every library.
    pub library_ids: Vec<String>,
    pub sort: SortBy,
    pub descending: bool,
}

impl Filters {
    /// Whether anything at all is narrowing the result set — what the UI's
    /// "Clear filters" button is enabled by, and what decides whether an empty
    /// query may list the whole library.
    pub fn is_empty(&self) -> bool {
        self.genre.is_none()
            && self.year_min.is_none()
            && self.year_max.is_none()
            && self.duration_min.is_none()
            && self.duration_max.is_none()
            && self.source == SourceFilter::All
            && !self.starred_only
            && self.format.is_none()
            && self.bitrate_min.is_none()
    }

    /// How many filters are active, for the button's badge.
    pub fn active_count(&self) -> usize {
        usize::from(self.genre.is_some())
            + usize::from(self.year_min.is_some() || self.year_max.is_some())
            + usize::from(self.duration_min.is_some() || self.duration_max.is_some())
            + usize::from(self.source != SourceFilter::All)
            + usize::from(self.starred_only)
            + usize::from(self.format.is_some())
            + usize::from(self.bitrate_min.is_some())
    }

    /// Reset the narrowing without touching the kind or the sort — those are
    /// what the user is looking *at*, not what they are excluding.
    pub fn clear(&mut self) {
        let (kind, sort, desc, libs) = (
            self.kind,
            self.sort,
            self.descending,
            std::mem::take(&mut self.library_ids),
        );
        *self = Filters {
            kind,
            sort,
            descending: desc,
            library_ids: libs,
            ..Filters::default()
        };
    }

    /// The kinds one run of this query actually searches.
    ///
    /// A single kind is itself. `All` is the three of them, minus any whose
    /// table cannot express a filter that is set: a genre, a year, a length or
    /// a format excludes artists, and a star excludes everything but albums.
    /// Dropping the kind is the honest reading — the alternative is a filter
    /// that narrows two of the three lists and silently matches all of the
    /// third, i.e. one filter meaning two things in one list, which is the same
    /// trap the cache-only decision exists to avoid.
    pub fn kinds(&self) -> Vec<SearchKind> {
        if self.kind != SearchKind::All {
            return vec![self.kind];
        }
        let narrowed = self.genre.is_some()
            || self.year_min.is_some()
            || self.year_max.is_some()
            || self.duration_min.is_some()
            || self.duration_max.is_some()
            || self.format.is_some()
            || self.bitrate_min.is_some();
        SearchKind::EVERY
            .into_iter()
            .filter(|k| !(self.starred_only && *k != SearchKind::Albums))
            .filter(|k| !(narrowed && *k == SearchKind::Artists))
            .collect()
    }

    /// The `WHERE` conditions and their bound values, in order.
    ///
    /// The query text's own `%word%` patterns are bound first (positionally, as
    /// `like_clause` numbers them from 1), so every condition built here appends
    /// after them and the clause is assembled with `?` placeholders that
    /// rusqlite numbers in sequence.
    fn conditions(&self) -> (Vec<String>, Vec<Box<dyn ToSql>>) {
        let mut sql: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let kind = self.kind;

        if let Some(src) = self.source.source_value() {
            sql.push("source = ?".into());
            args.push(Box::new(src.to_string()));
        }

        if !self.library_ids.is_empty() {
            // A row synced before the `library_id` column existed has NULL
            // there; hiding it would empty the page for a cache that predates
            // the column, so an unknown library is kept rather than excluded.
            //
            // The track table carries no library of its own — provenance is
            // recorded one level up, on the album the sync walked — so a track
            // is placed by its album, and one whose album row is missing
            // entirely is kept for the same reason a NULL is.
            let marks = vec!["?"; self.library_ids.len()].join(", ");
            let test = format!("lib.library_id IS NULL OR lib.library_id IN ({marks})");
            sql.push(match kind {
                SearchKind::Tracks => format!(
                    "NOT EXISTS (SELECT 1 FROM albums lib WHERE lib.id = tracks.album_id
                                 AND NOT ({test}))"
                ),
                _ => format!("({})", test.replace("lib.", "")),
            });
            for id in &self.library_ids {
                args.push(Box::new(id.clone()));
            }
        }

        if kind == SearchKind::Artists {
            // Nothing below exists on the artist table.
            return (sql, args);
        }

        if let Some(genre) = self.genre.as_ref().filter(|g| !g.is_empty()) {
            match kind {
                SearchKind::Tracks => {
                    sql.push("genre = ?".into());
                    args.push(Box::new(genre.clone()));
                }
                // Albums carry no genre of their own; it is the tags on their
                // files, which is also how the album grid's chip reads it.
                _ => {
                    sql.push(
                        "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.genre = ?)"
                            .into(),
                    );
                    args.push(Box::new(genre.clone()));
                }
            }
        }

        if let Some(y) = self.year_min {
            sql.push("year >= ?".into());
            args.push(Box::new(y));
        }
        if let Some(y) = self.year_max {
            sql.push("year <= ?".into());
            args.push(Box::new(y));
        }
        if let Some(d) = self.duration_min {
            sql.push("duration >= ?".into());
            args.push(Box::new(d));
        }
        if let Some(d) = self.duration_max {
            sql.push("duration <= ?".into());
            args.push(Box::new(d));
        }

        if self.starred_only && kind == SearchKind::Albums {
            sql.push("starred_at IS NOT NULL".into());
        }

        if let Some(fmt) = self.format.as_ref().filter(|f| !f.is_empty()) {
            match kind {
                SearchKind::Tracks => {
                    sql.push("suffix = ?".into());
                    args.push(Box::new(fmt.clone()));
                }
                _ => {
                    sql.push(
                        "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.suffix = ?)"
                            .into(),
                    );
                    args.push(Box::new(fmt.clone()));
                }
            }
        }

        if let Some(br) = self.bitrate_min {
            match kind {
                SearchKind::Tracks => {
                    sql.push("bit_rate >= ?".into());
                    args.push(Box::new(br));
                }
                _ => {
                    sql.push(
                        "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.bit_rate >= ?)"
                            .into(),
                    );
                    args.push(Box::new(br));
                }
            }
        }

        (sql, args)
    }

    /// The whole statement for a query, plus its bound values.
    ///
    /// An **empty query is legal here** where it is not in the palette: with a
    /// filter set, "every FLAC album from 1998" is the question, and there are
    /// no words in it. With neither text nor filters the caller does not run
    /// this at all — listing the library is what the album grid is for.
    fn sql(&self, query: &str) -> (String, Vec<Box<dyn ToSql>>) {
        let terms = like_terms(query);
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let mut wheres: Vec<String> = Vec::new();

        if !terms.is_empty() {
            // `All` never reaches here: `search_advanced` splits it into its
            // kinds first, and each runs its own statement. It shares the album
            // arms so the match stays total without a panic.
            let columns: &[&str] = match self.kind {
                SearchKind::Albums | SearchKind::All => &["title", "artist"],
                SearchKind::Artists => &["name"],
                SearchKind::Tracks => &["title", "artist", "album"],
            };
            wheres.push(like_clause(columns, terms.len()));
            for t in terms {
                args.push(Box::new(t));
            }
        }

        let (conds, cond_args) = self.conditions();
        wheres.extend(conds);
        args.extend(cond_args);

        let (table, select) = match self.kind {
            SearchKind::Albums | SearchKind::All => (
                "albums",
                "id, source, title, artist, artist_id, year, cover_art, song_count, duration, \
                 created, play_count, starred_at, library_id",
            ),
            SearchKind::Artists => ("artists", "id, source, name, cover_art, library_id"),
            SearchKind::Tracks => (
                "tracks",
                "id, source, title, artist, album, duration, local_path, cover_art, \
                 track_no, file_modified, album_id, disc_number, year, genre, \
                 suffix, content_type, bit_rate, sampling_rate, bit_depth, channel_count, \
                 file_size, file_created, replay_gain_track, replay_gain_album, \
                 replay_peak_track, replay_peak_album, play_count",
            ),
        };

        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wheres.join(" AND "))
        };
        // A sort the kind does not carry falls back to the title, which every
        // table has — a dropdown left on "Most played" while the kind is
        // switched to Artists must not produce invalid SQL.
        let sort = if self.sort.applies_to(self.kind) {
            self.sort
        } else {
            SortBy::Title
        };
        let order = sort
            .order_by(self.kind, self.descending)
            // Relevance is ranked in Rust; the cheap alphabetical order it
            // starts from keeps the result stable for queries that tie.
            .unwrap_or_else(|| match self.kind {
                SearchKind::Artists => "name COLLATE NOCASE ASC".into(),
                _ => "title COLLATE NOCASE ASC".into(),
            });

        (
            format!("SELECT {select} FROM {table}{where_sql} ORDER BY {order} LIMIT {HARD_LIMIT}"),
            args,
        )
    }
}

/// A ceiling on what one query may return.
///
/// The page is virtualized, so the cost of a large result is the `Vec` and the
/// ranking pass rather than the drawing — but an empty query under a single
/// loose filter is the whole library, and materialising 100k track rows (27
/// columns each) to draw twenty of them is not worth being literal about.
pub const HARD_LIMIT: usize = 5_000;

/// What one advanced query found. Exactly one of the three is populated — the
/// page searches one kind at a time, which is what lets every filter mean
/// something.
#[derive(Debug, Clone, Default)]
pub struct AdvancedResults {
    pub albums: Vec<AlbumRow>,
    pub artists: Vec<ArtistRow>,
    pub tracks: Vec<TrackRow>,
    /// Whether `HARD_LIMIT` cut the result short, so the page can say so
    /// instead of quietly showing a round number.
    pub truncated: bool,
}

impl AdvancedResults {
    /// `is_empty` is the page's own emptiness test; `len` is what the header
    /// counts. Clippy wants them as a pair, and they are one.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.albums.len() + self.artists.len() + self.tracks.len()
    }
    /// Whether the query found nothing — distinct from the page's own idea of
    /// empty, which also covers a query that has not run yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The values the filter dropdowns offer, read off the cache so a library with
/// no FLAC in it is never offered FLAC.
#[derive(Debug, Clone, Default)]
pub struct Facets {
    pub genres: Vec<String>,
    pub formats: Vec<String>,
    pub year_min: Option<i32>,
    pub year_max: Option<i32>,
}

impl LibraryDb {
    /// Run one advanced query.
    ///
    /// Blocking (SQLite), like every other method here — call it from
    /// `runtime::spawn_blocking_io` when the result set may be large.
    pub fn search_advanced(
        &self,
        query: &str,
        filters: &Filters,
    ) -> Result<AdvancedResults, rusqlite::Error> {
        let kinds = filters.kinds();
        // `All` is three statements against three tables, not one — SQLite can
        // union them, but the columns do not line up and each list wants its
        // own `ORDER BY` and its own share of the cap.
        if kinds.len() != 1 || kinds[0] != filters.kind {
            let mut out = AdvancedResults::default();
            for kind in kinds {
                let one = self.search_one(
                    query,
                    &Filters {
                        kind,
                        ..filters.clone()
                    },
                )?;
                out.albums.extend(one.albums);
                out.artists.extend(one.artists);
                out.tracks.extend(one.tracks);
                out.truncated |= one.truncated;
            }
            return Ok(out);
        }
        self.search_one(query, filters)
    }

    /// One statement against one table. `filters.kind` is never `All` here.
    fn search_one(
        &self,
        query: &str,
        filters: &Filters,
    ) -> Result<AdvancedResults, rusqlite::Error> {
        let (sql, args) = filters.sql(query);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(args.iter().map(|b| b.as_ref()));

        let mut out = AdvancedResults::default();
        match filters.kind {
            SearchKind::Albums | SearchKind::All => {
                out.albums = stmt
                    .query_map(params, super::library_db::album_from_row)?
                    .collect::<Result<_, _>>()?;
            }
            SearchKind::Artists => {
                out.artists = stmt
                    .query_map(params, super::library_db::artist_from_row)?
                    .collect::<Result<_, _>>()?;
            }
            SearchKind::Tracks => {
                out.tracks = stmt
                    .query_map(params, track_from_row)?
                    .collect::<Result<_, _>>()?;
            }
        }
        out.truncated = out.len() >= HARD_LIMIT;
        Ok(out)
    }

    /// The distinct genres, formats and year bounds present in the cache.
    ///
    /// One pass over the track table plus one over the albums; it is read once
    /// when the page opens and again after a scan, not per keystroke.
    pub fn search_facets(&self) -> Result<Facets, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut facets = Facets::default();

        let mut stmt = conn.prepare(
            "SELECT DISTINCT genre FROM tracks
             WHERE genre IS NOT NULL AND genre <> ''
             ORDER BY genre COLLATE NOCASE",
        )?;
        facets.genres = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;

        let mut stmt = conn.prepare(
            "SELECT DISTINCT LOWER(suffix) FROM tracks
             WHERE suffix IS NOT NULL AND suffix <> ''
             ORDER BY 1",
        )?;
        facets.formats = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;

        // Albums rather than tracks: the year the page filters albums by is the
        // album's own, and a stray mistagged track should not stretch the range
        // the slider offers.
        let (lo, hi) = conn.query_row(
            "SELECT MIN(year), MAX(year) FROM albums WHERE year IS NOT NULL AND year > 0",
            [],
            |r| Ok((r.get::<_, Option<i32>>(0)?, r.get::<_, Option<i32>>(1)?)),
        )?;
        facets.year_min = lo;
        facets.year_max = hi;
        Ok(facets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE albums (id TEXT PRIMARY KEY, source TEXT, title TEXT, artist TEXT,
                artist_id TEXT, year INTEGER, cover_art TEXT, song_count INTEGER,
                duration REAL, created TEXT, play_count INTEGER, starred_at TEXT,
                library_id TEXT);
             CREATE TABLE artists (id TEXT PRIMARY KEY, source TEXT, name TEXT, cover_art TEXT,
                library_id TEXT);
             CREATE TABLE tracks (id TEXT PRIMARY KEY, source TEXT, title TEXT, artist TEXT,
                album TEXT, duration REAL, local_path TEXT, cover_art TEXT, track_no INTEGER,
                file_modified INTEGER, album_id TEXT, disc_number INTEGER, year INTEGER,
                genre TEXT, suffix TEXT, content_type TEXT, bit_rate INTEGER,
                sampling_rate INTEGER, bit_depth INTEGER, channel_count INTEGER,
                file_size INTEGER, file_created INTEGER, replay_gain_track REAL,
                replay_gain_album REAL, replay_peak_track REAL, replay_peak_album REAL,
                play_count INTEGER);",
        )
        .unwrap();
        conn
    }

    /// Run a filter set against a scratch database and return the ids it keeps.
    /// The point of the tests is which rows a clause can exclude, so they go
    /// through real SQLite rather than asserting on the string.
    fn ids(conn: &Connection, query: &str, f: &Filters) -> Vec<String> {
        let (sql, args) = f.sql(query);
        let mut stmt = conn.prepare(&sql).expect(&sql);
        let params = rusqlite::params_from_iter(args.iter().map(|b| b.as_ref()));
        stmt.query_map(params, |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn every_kind_and_sort_combination_is_valid_sql() {
        let conn = db();
        for kind in SearchKind::ALL {
            for sort in SortBy::ALL {
                for descending in [false, true] {
                    let f = Filters {
                        kind,
                        sort,
                        descending,
                        genre: Some("Rock".into()),
                        year_min: Some(1990),
                        year_max: Some(2005),
                        duration_min: Some(60.0),
                        duration_max: Some(600.0),
                        source: SourceFilter::Local,
                        starred_only: true,
                        format: Some("flac".into()),
                        bitrate_min: Some(320),
                        library_ids: vec!["1".into(), "2".into()],
                    };
                    // A sort a kind does not carry must fall back rather than
                    // produce `ORDER BY play_count` over the artist table.
                    ids(&conn, "anything", &f);
                }
            }
        }
    }

    #[test]
    fn an_album_genre_is_asked_of_its_tracks() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, source, title, artist) VALUES ('a1','navidrome','Kid A','Radiohead');
             INSERT INTO albums (id, source, title, artist) VALUES ('a2','navidrome','Blue','Joni');
             INSERT INTO tracks (id, album_id, genre) VALUES ('t1','a1','Electronic');
             INSERT INTO tracks (id, album_id, genre) VALUES ('t2','a2','Folk');",
        )
        .unwrap();
        let f = Filters {
            genre: Some("Electronic".into()),
            ..Default::default()
        };
        assert_eq!(ids(&conn, "", &f), vec!["a1".to_string()]);
    }

    #[test]
    fn an_empty_query_is_filter_only_rather_than_empty() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, source, title, year) VALUES ('a1','local','One',1998);
             INSERT INTO albums (id, source, title, year) VALUES ('a2','local','Two',2012);",
        )
        .unwrap();
        let f = Filters {
            year_max: Some(2000),
            ..Default::default()
        };
        assert_eq!(ids(&conn, "", &f), vec!["a1".to_string()]);
    }

    #[test]
    fn a_library_subset_keeps_rows_that_predate_the_column() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, title, library_id) VALUES ('a1','One','1');
             INSERT INTO albums (id, title, library_id) VALUES ('a2','Two','2');
             INSERT INTO albums (id, title, library_id) VALUES ('a3','Three',NULL);",
        )
        .unwrap();
        let f = Filters {
            library_ids: vec!["1".into()],
            ..Default::default()
        };
        let got = ids(&conn, "", &f);
        assert!(got.contains(&"a1".to_string()));
        assert!(got.contains(&"a3".to_string()), "a NULL library is kept");
        assert!(!got.contains(&"a2".to_string()));
    }

    #[test]
    fn a_track_is_placed_in_a_library_by_its_album() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, title, library_id) VALUES ('a1','One','1');
             INSERT INTO albums (id, title, library_id) VALUES ('a2','Two','2');
             INSERT INTO tracks (id, title, album_id) VALUES ('t1','Song','a1');
             INSERT INTO tracks (id, title, album_id) VALUES ('t2','Song','a2');
             INSERT INTO tracks (id, title, album_id) VALUES ('t3','Song','gone');",
        )
        .unwrap();
        let f = Filters {
            kind: SearchKind::Tracks,
            library_ids: vec!["1".into()],
            ..Default::default()
        };
        let got = ids(&conn, "", &f);
        assert!(got.contains(&"t1".to_string()));
        assert!(!got.contains(&"t2".to_string()));
        assert!(
            got.contains(&"t3".to_string()),
            "a track with no album row is kept, like a NULL library"
        );
    }

    #[test]
    fn a_descending_sort_puts_unknown_values_last() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, title, year) VALUES ('a1','One',1998);
             INSERT INTO albums (id, title, year) VALUES ('a2','Two',NULL);
             INSERT INTO albums (id, title, year) VALUES ('a3','Three',2012);",
        )
        .unwrap();
        let f = Filters {
            sort: SortBy::Year,
            descending: true,
            ..Default::default()
        };
        assert_eq!(ids(&conn, "", &f), vec!["a3", "a1", "a2"]);
    }

    #[test]
    fn the_query_text_and_the_filters_bind_in_order() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, source, title, artist, year) VALUES ('a1','local','Kid A','Radiohead',2000);
             INSERT INTO albums (id, source, title, artist, year) VALUES ('a2','navidrome','Kid A','Radiohead',2000);",
        )
        .unwrap();
        let f = Filters {
            source: SourceFilter::Local,
            year_min: Some(1999),
            ..Default::default()
        };
        // Two query words, then a source, then a year: the `?1`/`?2` the LIKE
        // chain numbers itself must not collide with the appended `?`s.
        assert_eq!(ids(&conn, "kid radiohead", &f), vec!["a1".to_string()]);
    }

    #[test]
    fn everything_drops_the_kinds_a_filter_cannot_narrow() {
        // The alternative is a genre that excludes albums and tracks while
        // every artist in the library comes back beside them — one filter
        // meaning two things in one list.
        let plain = Filters::default();
        assert_eq!(plain.kinds(), SearchKind::EVERY.to_vec());

        let by_genre = Filters {
            genre: Some("Jazz".into()),
            ..Default::default()
        };
        assert_eq!(
            by_genre.kinds(),
            vec![SearchKind::Albums, SearchKind::Tracks]
        );

        // Only the album table records a star.
        let starred = Filters {
            starred_only: true,
            ..Default::default()
        };
        assert_eq!(starred.kinds(), vec![SearchKind::Albums]);

        // A single kind is itself, filters or no filters.
        let one = Filters {
            kind: SearchKind::Tracks,
            starred_only: true,
            ..Default::default()
        };
        assert_eq!(one.kinds(), vec![SearchKind::Tracks]);
    }

    #[test]
    fn everything_returns_all_three_tables_in_one_result() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO albums (id, title, artist) VALUES ('a1','Blue Train','Coltrane');
             INSERT INTO artists (id, name) VALUES ('r1','Coltrane');
             INSERT INTO tracks (id, title, artist) VALUES ('t1','Blue Train','Coltrane');",
        )
        .unwrap();
        // `search_advanced` needs a `LibraryDb`, which the scratch connection is
        // not — so run what it runs: one statement per kind the filters name.
        let f = Filters::default();
        let mut got: Vec<String> = Vec::new();
        for kind in f.kinds() {
            let one = Filters { kind, ..f.clone() };
            got.extend(ids(&conn, "blue", &one));
            got.extend(ids(&conn, "coltrane", &one));
        }
        assert!(got.contains(&"a1".to_string()));
        assert!(got.contains(&"r1".to_string()));
        assert!(got.contains(&"t1".to_string()));
    }

    #[test]
    fn clearing_keeps_what_the_user_is_looking_at() {
        let mut f = Filters {
            kind: SearchKind::Tracks,
            sort: SortBy::Plays,
            descending: true,
            library_ids: vec!["1".into()],
            starred_only: true,
            format: Some("flac".into()),
            ..Default::default()
        };
        f.clear();
        assert_eq!(f.kind, SearchKind::Tracks);
        assert_eq!(f.sort, SortBy::Plays);
        assert!(f.descending);
        assert_eq!(f.library_ids, vec!["1".to_string()]);
        assert!(f.is_empty());
        assert_eq!(f.active_count(), 0);
    }
}
