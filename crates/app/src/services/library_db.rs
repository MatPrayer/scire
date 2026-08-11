//! SQLite-backed music library database. Holds both local and Navidrome
//! tracks in a single schema, enabling fast local queries and incremental
//! scanning.
// ponytail: dead_code OK — used by M4, M5, M8. Remove once those land.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_V2: &str = "
CREATE TABLE IF NOT EXISTS playlists (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL DEFAULT 'local',
    name        TEXT NOT NULL,
    description TEXT,
    duration    REAL DEFAULT 0.0,
    song_count  INTEGER DEFAULT 0,
    public      INTEGER DEFAULT 0
) STRICT;

CREATE TABLE IF NOT EXISTS playlist_entries (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_source TEXT,
    track_id    TEXT,
    entry_order INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_pe_playlist ON playlist_entries(playlist_id);
";

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL CHECK (source IN ('navidrome','local')),
    title       TEXT NOT NULL,
    artist      TEXT,
    artist_id   TEXT,
    album       TEXT,
    album_id    TEXT,
    album_artist TEXT,
    track_no    INTEGER,
    disc_number INTEGER,
    year        INTEGER,
    genre       TEXT,
    duration    REAL,
    local_path  TEXT,
    cover_art   TEXT,
    file_modified INTEGER,
    starred     INTEGER DEFAULT 0,
    user_rating INTEGER,
    play_count  INTEGER DEFAULT 0
) STRICT;

CREATE TABLE IF NOT EXISTS albums (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    title       TEXT NOT NULL,
    artist      TEXT,
    artist_id   TEXT,
    year        INTEGER,
    cover_art   TEXT,
    song_count  INTEGER DEFAULT 0,
    duration    REAL DEFAULT 0.0,
    starred     INTEGER DEFAULT 0
) STRICT;

CREATE TABLE IF NOT EXISTS artists (
    id          TEXT PRIMARY KEY,
    source      TEXT NOT NULL,
    name        TEXT NOT NULL,
    cover_art   TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS _schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

INSERT INTO _schema_version (version) VALUES (1);
";

/// Catalog columns the album/artist grids need to paint a *sorted, filtered*
/// cache before the server answers.
///
/// `created`/`play_count`/`starred_at` are the sort keys of the New, Frequent
/// and Starred tabs — without them a cached row can only be placed under
/// "All", which is why the seed used to be alphabetical-only. `library_id`
/// records which music folder a row was synced from, so a user browsing a
/// subset of their libraries isn't shown rows from the others.
///
/// `starred_at` rather than the existing `starred` column: that one is INTEGER
/// and the API's value is an ISO timestamp, which a STRICT table rejects.
const SCHEMA_V3: &str = "
ALTER TABLE albums ADD COLUMN created TEXT;
ALTER TABLE albums ADD COLUMN play_count INTEGER;
ALTER TABLE albums ADD COLUMN starred_at TEXT;
ALTER TABLE albums ADD COLUMN library_id TEXT;
ALTER TABLE artists ADD COLUMN library_id TEXT;
";

/// Indexes for the two columns everything filters on.
///
/// `tracks.album_id` is the one that mattered: `album_fingerprints` counts an
/// album's tracks with a correlated subquery, and unindexed that was a full
/// scan of the track table *per album* — 1126 albums over 12k tracks cost ~2.3s,
/// which was most of an incremental sync that issues no requests at all.
/// `tracks_by_album` (album detail view) reads the same index.
const SCHEMA_V4: &str = "
CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_source ON tracks(source);
CREATE INDEX IF NOT EXISTS idx_albums_source ON albums(source);
CREATE INDEX IF NOT EXISTS idx_artists_source ON artists(source);
";

// ---------------------------------------------------------------------------
// LibraryDb
// ---------------------------------------------------------------------------

/// Thread-safe handle to the music library SQLite database.
pub struct LibraryDb {
    pub(crate) conn: Mutex<Connection>,
    /// Monotonic counter bumped after every local scan completes.
    /// Views cheaply detect whether data changed without re-querying.
    scan_version: AtomicU64,
}

impl LibraryDb {
    /// Open (or create) the database at `path` and run pending migrations.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Mutex::new(conn),
            scan_version: AtomicU64::new(0),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
            scan_version: AtomicU64::new(0),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Monotonic counter bumped after every local scan completes.
    pub fn scan_version(&self) -> u64 {
        self.scan_version.load(Ordering::Relaxed)
    }

    /// Bump the scan version, signalling to views that data may have changed.
    pub fn bump_scan_version(&self) {
        self.scan_version.fetch_add(1, Ordering::Relaxed);
    }

    // ------------------------------------------------------------------
    // Migrations
    // ------------------------------------------------------------------

    fn run_migrations(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            conn.execute_batch(SCHEMA_V2)?;
            conn.execute("INSERT INTO _schema_version (version) VALUES (2)", [])?;
        }
        if version < 3 {
            conn.execute_batch(SCHEMA_V3)?;
            conn.execute("INSERT INTO _schema_version (version) VALUES (3)", [])?;
        }
        if version < 4 {
            conn.execute_batch(SCHEMA_V4)?;
            conn.execute("INSERT INTO _schema_version (version) VALUES (4)", [])?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Track queries
    // ------------------------------------------------------------------

    /// Insert or replace a track row.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_track(
        &self,
        id: &str,
        source: &str,
        title: &str,
        artist: Option<&str>,
        artist_id: Option<&str>,
        album: Option<&str>,
        album_id: Option<&str>,
        album_artist: Option<&str>,
        track_no: Option<i32>,
        disc_number: Option<i32>,
        year: Option<i32>,
        genre: Option<&str>,
        duration: Option<f64>,
        local_path: Option<&str>,
        cover_art: Option<&str>,
        file_modified: Option<i64>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tracks
             (id, source, title, artist, artist_id, album, album_id, album_artist,
              track_no, disc_number, year, genre, duration, local_path, cover_art, file_modified)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            rusqlite::params![
                id,
                source,
                title,
                artist,
                artist_id,
                album,
                album_id,
                album_artist,
                track_no,
                disc_number,
                year,
                genre,
                duration,
                local_path,
                cover_art,
                file_modified
            ],
        )?;
        Ok(())
    }

    /// Fetch a single track by id.
    pub fn get_track(&self, id: &str) -> Result<Option<TrackRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, title, artist, album, duration, local_path, cover_art, track_no, file_modified
             FROM tracks WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id], |row| {
            Ok(TrackRow {
                id: row.get(0)?,
                source: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                duration: row.get(5)?,
                local_path: row.get(6)?,
                cover_art: row.get(7)?,
                track_no: row.get(8)?,
                file_modified: row.get(9)?,
            })
        })?;
        match rows.next() {
            Some(Ok(track)) => Ok(Some(track)),
            _ => Ok(None),
        }
    }

    /// Search tracks by title, artist, or album (LIKE %query%).
    pub fn search_tracks(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TrackRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{query}%");
        let mut stmt = conn.prepare(
            "SELECT id, source, title, artist, album, duration, local_path, cover_art, track_no, file_modified
             FROM tracks
             WHERE title LIKE ?1 OR artist LIKE ?1 OR album LIKE ?1
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![pattern, limit as i64], |row| {
            Ok(TrackRow {
                id: row.get(0)?,
                source: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                duration: row.get(5)?,
                local_path: row.get(6)?,
                cover_art: row.get(7)?,
                track_no: row.get(8)?,
                file_modified: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// List all tracks for a given album.
    pub fn tracks_by_album(&self, album_id: &str) -> Result<Vec<TrackRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, title, artist, album, duration, local_path, cover_art, track_no, file_modified
             FROM tracks WHERE album_id = ?1
             ORDER BY disc_number, track_no",
        )?;
        let rows = stmt.query_map(rusqlite::params![album_id], |row| {
            Ok(TrackRow {
                id: row.get(0)?,
                source: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                duration: row.get(5)?,
                local_path: row.get(6)?,
                cover_art: row.get(7)?,
                track_no: row.get(8)?,
                file_modified: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Number of tracks matching a source.
    pub fn track_count_by_source(&self, source: &str) -> Result<i64, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM tracks WHERE source = ?1",
            rusqlite::params![source],
            |row| row.get(0),
        )
    }

    /// List all tracks for a given source.
    pub fn tracks_by_source(&self, source: &str) -> Result<Vec<TrackRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, title, artist, album, duration, local_path, cover_art, track_no, file_modified
             FROM tracks WHERE source = ?1
             ORDER BY album, track_no",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok(TrackRow {
                id: row.get(0)?,
                source: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                duration: row.get(5)?,
                local_path: row.get(6)?,
                cover_art: row.get(7)?,
                track_no: row.get(8)?,
                file_modified: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Delete a track by id.
    pub fn delete_track(&self, id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tracks WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Album queries
    // ------------------------------------------------------------------

    /// Insert or replace an album row.
    pub fn upsert_album(&self, album: &AlbumRow) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO albums
             (id, source, title, artist, artist_id, year, cover_art, song_count, duration,
              created, play_count, starred_at, library_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                album.id,
                album.source,
                album.title,
                album.artist,
                album.artist_id,
                album.year,
                album.cover_art,
                album.song_count,
                album.duration,
                album.created,
                album.play_count,
                album.starred,
                album.library_id,
            ],
        )?;
        Ok(())
    }

    /// Upsert many albums and their artists in one transaction.
    ///
    /// SQLite autocommits every statement, so a thousand single upserts is a
    /// thousand commits — that alone was ~3s of an otherwise ~1s incremental
    /// sync. `artists` is `(id, name, library_id)`, deduplicated by the caller.
    pub fn upsert_catalog(
        &self,
        source: &str,
        albums: &[AlbumRow],
        artists: &[(String, String, Option<String>)],
    ) -> Result<(), rusqlite::Error> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO artists (id, source, name, cover_art, library_id)
                 VALUES (?1,?2,?3,NULL,?4)",
            )?;
            for (id, name, library_id) in artists {
                stmt.execute(rusqlite::params![id, source, name, library_id])?;
            }
        }
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO albums
                 (id, source, title, artist, artist_id, year, cover_art, song_count, duration,
                  created, play_count, starred_at, library_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            )?;
            for album in albums {
                stmt.execute(rusqlite::params![
                    album.id,
                    album.source,
                    album.title,
                    album.artist,
                    album.artist_id,
                    album.year,
                    album.cover_art,
                    album.song_count,
                    album.duration,
                    album.created,
                    album.play_count,
                    album.starred,
                    album.library_id,
                ])?;
            }
        }
        tx.commit()
    }

    /// List all albums for a given source, alphabetically. Tabs that sort on
    /// another key re-sort in memory — the whole point of carrying `created`,
    /// `play_count` and `starred` on the row.
    pub fn albums_by_source(&self, source: &str) -> Result<Vec<AlbumRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, title, artist, artist_id, year, cover_art, song_count, duration,
                    created, play_count, starred_at, library_id
             FROM albums WHERE source = ?1
             ORDER BY title COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok(AlbumRow {
                id: row.get(0)?,
                source: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                artist_id: row.get(4)?,
                year: row.get(5)?,
                cover_art: row.get(6)?,
                song_count: row.get(7)?,
                duration: row.get(8)?,
                created: row.get(9)?,
                play_count: row.get(10)?,
                starred: row.get(11)?,
                library_id: row.get(12)?,
            })
        })?;
        rows.collect()
    }

    /// What the cache already holds for every album of `source`, keyed by id.
    ///
    /// This is what makes an incremental sync possible: the listing endpoint
    /// hands back each album's `songCount`/`duration` for free, so comparing
    /// them against these tells us which albums need their tracks re-fetched
    /// without issuing a single extra request. `track_rows` is counted rather
    /// than trusted from `song_count` because a sync interrupted partway
    /// through leaves album rows whose tracks never landed, and those must be
    /// picked up again rather than looking complete.
    pub fn album_fingerprints(
        &self,
        source: &str,
    ) -> Result<HashMap<String, AlbumFingerprint>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT a.id, a.song_count, a.duration,
                    (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id)
             FROM albums a WHERE a.source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                AlbumFingerprint {
                    song_count: row.get(1)?,
                    duration: row.get(2)?,
                    track_rows: row.get(3)?,
                },
            ))
        })?;
        rows.collect()
    }

    /// Drop one album's tracks, leaving the album row in place.
    pub fn delete_tracks_for_album(&self, album_id: &str) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM tracks WHERE album_id = ?1",
            rusqlite::params![album_id],
        )
    }

    /// Drop one album and the tracks hanging off it.
    pub fn delete_album_with_tracks(&self, album_id: &str) -> Result<(), rusqlite::Error> {
        self.delete_tracks_for_album(album_id)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM albums WHERE id = ?1",
            rusqlite::params![album_id],
        )?;
        Ok(())
    }

    /// Remove artists of `source` that no album points at any more.
    ///
    /// An incremental sync deletes albums one at a time and can't tell whether
    /// the artist behind one still has other records; this sweeps up afterwards.
    pub fn prune_orphan_artists(&self, source: &str) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM artists WHERE source = ?1 AND id NOT IN
                 (SELECT artist_id FROM albums WHERE artist_id IS NOT NULL)",
            rusqlite::params![source],
        )
    }

    // ------------------------------------------------------------------
    // Artist queries
    // ------------------------------------------------------------------

    /// Insert or replace an artist row.
    pub fn upsert_artist(
        &self,
        id: &str,
        source: &str,
        name: &str,
        cover_art: Option<&str>,
        library_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO artists (id, source, name, cover_art, library_id)
             VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![id, source, name, cover_art, library_id],
        )?;
        Ok(())
    }

    /// List all artists for a given source.
    pub fn artists_by_source(&self, source: &str) -> Result<Vec<ArtistRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, source, name, cover_art, library_id
             FROM artists WHERE source = ?1
             ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok(ArtistRow {
                id: row.get(0)?,
                source: row.get(1)?,
                name: row.get(2)?,
                cover_art: row.get(3)?,
                library_id: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Album count per artist id, for a given source.
    pub fn album_counts_by_artist(
        &self,
        source: &str,
    ) -> Result<HashMap<String, i64>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT artist_id, COUNT(*)
             FROM albums WHERE source = ?1 AND artist_id IS NOT NULL
             GROUP BY artist_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect()
    }

    // ------------------------------------------------------------------
    // Config queries
    // ------------------------------------------------------------------

    /// Execute an arbitrary SQL statement (DELETE, etc.).
    pub fn execute(&self, sql: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql)?;
        Ok(())
    }

    /// Insert or replace a config key/value pair.
    pub fn upsert_config(&self, key: &str, value: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Playlist queries
    // ------------------------------------------------------------------

    pub fn upsert_playlist(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO playlists (id, source, name, description) VALUES (?1, 'local', ?2, ?3)",
            rusqlite::params![id, name, description],
        )?;
        Ok(())
    }

    pub fn clear_playlist_entries(&self, playlist_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM playlist_entries WHERE playlist_id = ?1",
            rusqlite::params![playlist_id],
        )?;
        Ok(())
    }

    pub fn add_playlist_entry(
        &self,
        playlist_id: &str,
        track_source: Option<&str>,
        track_id: Option<&str>,
        entry_order: i32,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO playlist_entries (playlist_id, track_source, track_id, entry_order) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![playlist_id, track_source, track_id, entry_order],
        )?;
        Ok(())
    }

    pub fn all_playlists(&self) -> Result<Vec<PlaylistRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, duration, song_count FROM playlists ORDER BY name COLLATE NOCASE"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PlaylistRow {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                duration: row.get(3)?,
                song_count: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn playlist_entries(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<PlaylistEntryRow>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, playlist_id, track_source, track_id, entry_order FROM playlist_entries WHERE playlist_id = ?1 ORDER BY entry_order"
        )?;
        let rows = stmt.query_map(rusqlite::params![playlist_id], |row| {
            Ok(PlaylistEntryRow {
                id: row.get(0)?,
                playlist_id: row.get(1)?,
                track_source: row.get(2)?,
                track_id: row.get(3)?,
                entry_order: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn remove_playlist(&self, playlist_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM playlist_entries WHERE playlist_id = ?1",
            rusqlite::params![playlist_id],
        )?;
        conn.execute(
            "DELETE FROM playlists WHERE id = ?1",
            rusqlite::params![playlist_id],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TrackRow {
    pub id: String,
    pub source: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub local_path: Option<String>,
    pub cover_art: Option<String>,
    pub track_no: Option<i32>,
    pub file_modified: Option<i64>,
}

impl TrackRow {
    /// Convert to a `subsonic::Song` for the playback queue.
    pub fn into_song(self) -> subsonic::Song {
        subsonic::Song {
            id: self.id,
            title: self.title,
            album: self.album,
            album_id: None,
            artist: self.artist,
            artist_id: None,
            track: self.track_no.map(|t| t as u32),
            disc_number: None,
            year: None,
            genre: None,
            cover_art: self.cover_art,
            duration: self.duration.map(|d| d as u32),
            bit_rate: None,
            sampling_rate: None,
            bit_depth: None,
            channel_count: None,
            content_type: None,
            suffix: None,
            size: None,
            starred: None,
            user_rating: None,
            play_count: None,
            replay_gain: None,
            artists: Vec::new(),
            local_path: self.local_path,
        }
    }
}

/// What a sync compares a listed album against to decide whether its tracks
/// still need fetching. See `LibraryDb::album_fingerprints`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlbumFingerprint {
    pub song_count: i64,
    pub duration: f64,
    /// Track rows actually present, which is not always `song_count`.
    pub track_rows: i64,
}

#[derive(Debug, Clone)]
pub struct AlbumRow {
    pub id: String,
    pub source: String,
    pub title: String,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub year: Option<i32>,
    pub cover_art: Option<String>,
    pub song_count: i64,
    pub duration: f64,
    /// Sort keys mirrored from the API so a cached row can be placed under the
    /// New / Frequent / Starred tabs, not just the alphabetical one.
    pub created: Option<String>,
    pub play_count: Option<i64>,
    pub starred: Option<String>,
    /// Music folder this row was synced from; `None` when the server exposes
    /// only one library (or the folder list was unavailable).
    pub library_id: Option<String>,
}

impl AlbumRow {
    /// A row with only the columns every caller sets; the catalog sort keys
    /// default to absent, which is what a local-file album has.
    pub fn new(id: &str, source: &str, title: &str) -> Self {
        Self {
            id: id.to_string(),
            source: source.to_string(),
            title: title.to_string(),
            artist: None,
            artist_id: None,
            year: None,
            cover_art: None,
            song_count: 0,
            duration: 0.0,
            created: None,
            play_count: None,
            starred: None,
            library_id: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtistRow {
    pub id: String,
    pub source: String,
    pub name: String,
    pub cover_art: Option<String>,
    /// Music folder this row was synced from; see [`AlbumRow::library_id`].
    pub library_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlaylistRow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub duration: f64,
    pub song_count: i64,
}

#[derive(Debug, Clone)]
pub struct PlaylistEntryRow {
    pub id: i64,
    pub playlist_id: String,
    pub track_source: Option<String>,
    pub track_id: Option<String>,
    pub entry_order: i32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> LibraryDb {
        LibraryDb::open_in_memory().unwrap()
    }

    /// Helper: insert a minimal track with just id, source, title.
    fn insert_minimal(db: &LibraryDb, id: &str, source: &str, title: &str) {
        db.upsert_track(
            id, source, title, None, None, None, None, None, None, None, None, None, None, None,
            None, None,
        )
        .unwrap();
    }

    #[test]
    fn schema_version_is_4() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let version: i32 = conn
            .query_row("SELECT MAX(version) FROM _schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 4);
    }

    #[test]
    fn the_album_id_index_exists() {
        // `album_fingerprints` counts tracks per album; without this index that
        // is a full scan of the track table per album.
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_tracks_album'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn tables_exist() {
        let db = test_db();
        let conn = db.conn.lock().unwrap();
        for table in &[
            "tracks",
            "albums",
            "artists",
            "playlists",
            "playlist_entries",
        ] {
            let count: i32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {table} should exist");
        }
    }

    #[test]
    fn upsert_and_get_track() {
        let db = test_db();
        db.upsert_track(
            "local:abc123",
            "local",
            "Test Song",
            Some("Test Artist"),
            None,
            Some("Test Album"),
            Some("album:1"),
            None,
            Some(1),
            None,
            Some(2024),
            Some("Rock"),
            Some(180.0),
            Some("/music/test.flac"),
            None,
            Some(1_000_000),
        )
        .unwrap();

        let t = db.get_track("local:abc123").unwrap().unwrap();
        assert_eq!(t.title, "Test Song");
        assert_eq!(t.artist.as_deref(), Some("Test Artist"));
        assert_eq!(t.local_path.as_deref(), Some("/music/test.flac"));
    }

    #[test]
    fn upsert_replaces_existing() {
        let db = test_db();
        insert_minimal(&db, "t1", "local", "Original");
        insert_minimal(&db, "t1", "local", "Replaced");
        let t = db.get_track("t1").unwrap().unwrap();
        assert_eq!(t.title, "Replaced");
    }

    #[test]
    fn search_tracks_finds_by_title() {
        let db = test_db();
        insert_minimal(&db, "t1", "local", "Hello World");
        insert_minimal(&db, "t2", "local", "Goodbye");
        let results = db.search_tracks("Hello", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "t1");
    }

    #[test]
    fn search_tracks_finds_by_artist() {
        let db = test_db();
        db.upsert_track(
            "t1",
            "local",
            "Song",
            Some("The Beatles"),
            None,
            None,
            None,
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
        let results = db.search_tracks("Beatles", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "t1");
    }

    #[test]
    fn tracks_by_album_id() {
        let db = test_db();
        db.upsert_track(
            "t1",
            "local",
            "A",
            None,
            None,
            None,
            Some("alb1"),
            None,
            Some(1),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.upsert_track(
            "t2",
            "local",
            "B",
            None,
            None,
            None,
            Some("alb1"),
            None,
            Some(2),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.upsert_track(
            "t3",
            "local",
            "C",
            None,
            None,
            None,
            Some("alb2"),
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
        let results = db.tracks_by_album("alb1").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn track_count_by_source() {
        let db = test_db();
        insert_minimal(&db, "t1", "local", "A");
        insert_minimal(&db, "t2", "local", "B");
        insert_minimal(&db, "t3", "navidrome", "C");
        assert_eq!(db.track_count_by_source("local").unwrap(), 2);
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 1);
    }

    #[test]
    fn upsert_album_and_list() {
        let db = test_db();
        let mut a1 = AlbumRow::new("alb1", "local", "Test Album");
        a1.artist = Some("Artist".into());
        a1.year = Some(2024);
        a1.song_count = 12;
        a1.duration = 3600.0;
        db.upsert_album(&a1).unwrap();
        let mut a2 = AlbumRow::new("alb2", "local", "Another Album");
        a2.song_count = 8;
        a2.duration = 2400.0;
        db.upsert_album(&a2).unwrap();
        let albums = db.albums_by_source("local").unwrap();
        assert_eq!(albums.len(), 2);
        // Ordered by title COLLATE NOCASE
        assert_eq!(albums[0].title, "Another Album");
        assert_eq!(albums[1].title, "Test Album");
    }

    #[test]
    fn album_catalog_columns_round_trip() {
        // The grids seed their New/Frequent/Starred tabs and honour a library
        // subset entirely from these four columns.
        let db = test_db();
        let mut row = AlbumRow::new("alb1", "navidrome", "Kid A");
        row.created = Some("2000-10-02T00:00:00Z".into());
        row.play_count = Some(41);
        row.starred = Some("2024-05-01T12:00:00Z".into());
        row.library_id = Some("2".into());
        db.upsert_album(&row).unwrap();

        let back = db.albums_by_source("navidrome").unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].created.as_deref(), Some("2000-10-02T00:00:00Z"));
        assert_eq!(back[0].play_count, Some(41));
        assert_eq!(back[0].starred.as_deref(), Some("2024-05-01T12:00:00Z"));
        assert_eq!(back[0].library_id.as_deref(), Some("2"));
    }

    #[test]
    fn artist_records_its_library() {
        let db = test_db();
        db.upsert_artist("art1", "navidrome", "Radiohead", None, Some("2"))
            .unwrap();
        let back = db.artists_by_source("navidrome").unwrap();
        assert_eq!(back[0].library_id.as_deref(), Some("2"));
    }

    #[test]
    fn upsert_artist_and_list() {
        let db = test_db();
        db.upsert_artist("art1", "local", "Artist A", None, None)
            .unwrap();
        db.upsert_artist("art2", "local", "Artist B", None, None)
            .unwrap();
        let artists = db.artists_by_source("local").unwrap();
        assert_eq!(artists.len(), 2);
    }

    #[test]
    fn get_track_nonexistent() {
        let db = test_db();
        assert!(db.get_track("nonexistent").unwrap().is_none());
    }

    #[test]
    fn search_tracks_empty_result() {
        let db = test_db();
        let results = db.search_tracks("zzz_not_found", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn multiple_sources_coexist() {
        let db = test_db();
        insert_minimal(&db, "n1", "navidrome", "Navidrome Song");
        insert_minimal(&db, "l1", "local", "Local Song");
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 1);
        assert_eq!(db.track_count_by_source("local").unwrap(), 1);
    }

    #[test]
    fn idempotent_reopen() {
        // Test that opening an existing DB doesn't error or change schema.
        let dir = std::env::temp_dir().join("scire-test-libdb");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.db");
        let _ = std::fs::remove_file(&path);

        // First open: creates schema
        let db1 = LibraryDb::open(&path).unwrap();
        assert!(db1.track_count_by_source("local").is_ok());

        // Drop and reopen
        drop(db1);
        let db2 = LibraryDb::open(&path).unwrap();
        assert!(db2.track_count_by_source("local").is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_and_list_playlist() {
        let db = test_db();
        db.upsert_playlist("pl1", "My Favorites", Some("My favorite songs"))
            .unwrap();
        db.add_playlist_entry("pl1", Some("local"), Some("track1"), 0)
            .unwrap();
        db.add_playlist_entry("pl1", None, None, 1).unwrap();
        let entries = db.playlist_entries("pl1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry_order, 0);
        let lists = db.all_playlists().unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "My Favorites");
        db.remove_playlist("pl1").unwrap();
        assert!(db.all_playlists().unwrap().is_empty());
    }

    #[test]
    fn playlist_clear_entries() {
        let db = test_db();
        db.upsert_playlist("pl2", "Empty", None).unwrap();
        db.add_playlist_entry("pl2", Some("local"), Some("t1"), 0)
            .unwrap();
        db.clear_playlist_entries("pl2").unwrap();
        assert!(db.playlist_entries("pl2").unwrap().is_empty());
    }
}
