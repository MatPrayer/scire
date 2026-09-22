//! Navidrome → LibraryDb sync.
//!
//! Fetches all albums (paginated) and their tracks from the Subsonic API and
//! upserts them into the local SQLite DB so the app can query both local and
//! remote music from a single database.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use subsonic::SubsonicClient;

use crate::services::library_db::{AlbumFingerprint, AlbumRow, LibraryDb, TrackMetadata};

const PAGE_SIZE: u32 = 500;

/// How many `getAlbum` requests are in flight at once.
///
/// The track fetch is one request per album — over a thousand of them on a
/// mid-sized library — and issuing them serially is what made a manual refresh
/// take minutes with no sign of life. Bounded rather than unbounded so the sync
/// doesn't monopolise the server (or the two IO workers) while the user is
/// browsing.
const ALBUM_FETCH_CONCURRENCY: usize = 6;

/// Live counters a running sync publishes for the UI's progress bar.
///
/// Atomics rather than a channel: the sync runs on the IO runtime and the
/// reader is a gpui view that repaints on its own timer, so neither side needs
/// to wake the other.
#[derive(Debug, Default)]
pub struct SyncProgress {
    /// Albums to fetch tracks for. Known only after the listing pass.
    pub total: AtomicUsize,
    /// Albums whose tracks have landed.
    pub done: AtomicUsize,
}

impl SyncProgress {
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

/// How much of the server catalog a sync re-reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Fetch tracks only for albums the listing says are new or changed.
    ///
    /// This is what the Refresh button runs. The listing is three requests for
    /// a thousand albums and carries each one's `songCount`/`duration`, so a
    /// library with nothing new costs those three requests and a local compare
    /// — versus one `getAlbum` per album, which is the same work whether or not
    /// anything changed.
    #[default]
    Incremental,
    /// Drop every cached row and re-import the lot.
    ///
    /// The escape hatch for the one thing the incremental pass cannot see: an
    /// album re-tagged without its track count or total duration moving.
    Full,
}

/// Decide whether a listed album's tracks still need fetching.
///
/// `duration` is compared with a tolerance because servers round it and the
/// value round-trips through an f64 column; an exact compare re-fetched albums
/// that hadn't changed at all.
fn needs_track_fetch(album: &subsonic::Album, cached: Option<&AlbumFingerprint>) -> bool {
    let Some(cached) = cached else { return true };
    let listed_count = album.song_count.unwrap_or(0) as i64;
    if cached.track_rows != listed_count || cached.song_count != listed_count {
        return true;
    }
    let listed_duration = album.duration.unwrap_or(0) as f64;
    (cached.duration - listed_duration).abs() > 1.0
}

/// Sync all Navidrome data into the local database.
///
/// 1. Fetch all albums (paginated `getAlbumList2` → `alphabeticalByName`).
/// 2. Reconcile against the cache: upsert every album row, drop the ones the
///    server no longer lists, and collect the albums whose tracks are missing
///    or stale.
/// 3. Fetch tracks via `getAlbum` for that subset only (`SyncMode::Full`
///    wipes the cache first and so treats every album as stale).
/// 4. Record last-sync timestamp.
///
/// `music_folder_id` restricts the sync to one library. When it is `None` the
/// libraries are enumerated and walked one at a time instead of syncing them in
/// bulk: the rows then carry which library they came from, which is what lets
/// the album/artist grids paint the cache for a *subset* of libraries rather
/// than only for "all".
pub async fn sync_navidrome(
    db: Arc<LibraryDb>,
    client: &SubsonicClient,
    music_folder_id: Option<&str>,
    progress: Arc<SyncProgress>,
    mode: SyncMode,
) -> Result<()> {
    tracing::info!("navidrome sync start ({mode:?})");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    progress.total.store(0, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    // One pass per library, so every row records its provenance. A server that
    // doesn't answer getMusicFolders still syncs — just without library ids,
    // which only costs the subset-filtered cache seed.
    let folders: Vec<Option<String>> = match music_folder_id {
        Some(id) => vec![Some(id.to_string())],
        None => match client.get_music_folders().await {
            Ok(folders) if !folders.is_empty() => folders.iter().map(|f| Some(f.id())).collect(),
            _ => vec![None],
        },
    };

    // Phase 1 — list every album, before touching the DB. The listing is the
    // part that can fail on a flaky link, and truncating first meant a failure
    // there left the user with an empty cached catalog until the next full
    // sync succeeded.
    //
    // An album reachable from two libraries would otherwise be re-fetched
    // (one `getAlbum` each) and land twice.
    let mut seen: HashSet<String> = HashSet::new();
    let mut listed: Vec<(subsonic::Album, Option<String>)> = Vec::new();

    // Artist covers, once per music folder. The album listing never carries an
    // artist's own cover id, so without this every cached artist row is written
    // with a NULL cover and the seeded grids draw a placeholder icon. Cosmetic:
    // a failure here is logged and the sync carries on with what it has.
    let mut artist_covers: HashMap<String, String> = HashMap::new();
    for folder_id in &folders {
        match client.get_artists(folder_id.as_ref()).await {
            Ok(indexes) => {
                for index in indexes {
                    for artist in index.artist {
                        if let Some(cover) = artist.cover_art {
                            artist_covers.insert(format!("navidrome:artist:{}", artist.id), cover);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("navidrome sync: artist covers unavailable: {e}");
            }
        }
    }

    for folder_id in &folders {
        let mut offset = 0u32;
        let mut previous_page_head: Option<String> = None;
        loop {
            let albums = client
                .get_album_list2(
                    subsonic::AlbumListType::AlphabeticalByName,
                    PAGE_SIZE,
                    offset,
                    folder_id.as_ref(),
                )
                .await?;
            let page_len = albums.len();
            if page_len == 0 {
                break;
            }
            // Safety net for a server that ignores `offset`: it answers a full
            // page forever and the loop never ends. Compared against the last
            // page of *this* folder rather than against everything seen so far,
            // because two libraries sharing albums legitimately produce a page
            // with nothing new in it.
            let head = albums[0].id.clone();
            if previous_page_head.as_ref() == Some(&head) {
                tracing::warn!("album listing repeated itself at offset {offset}; stopping");
                break;
            }
            previous_page_head = Some(head);

            for album in albums {
                if seen.insert(album.id.clone()) {
                    listed.push((album, folder_id.clone()));
                }
            }
            // A short page is the last one — asking for the empty page after it
            // costs a round-trip per library for nothing.
            if page_len < PAGE_SIZE as usize {
                break;
            }
            offset += PAGE_SIZE;
        }
    }

    tracing::info!("navidrome sync: {} albums listed", listed.len());

    // Phase 2 — reconcile against the cache.
    //
    // A full sync wipes first and so finds no fingerprints, which makes every
    // album stale; an incremental one keeps the rows and only re-fetches what
    // the listing says has moved.
    if mode == SyncMode::Full {
        let count = remove_navidrome(&db)?;
        tracing::info!("removed {count} stale navidrome tracks/albums/artists");
    }
    let cached = db.album_fingerprints("navidrome").unwrap_or_default();

    let mut stale: Vec<(subsonic::Album, Option<String>)> = Vec::new();
    let mut album_rows: Vec<AlbumRow> = Vec::with_capacity(listed.len());
    let mut artist_rows: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();
    let mut album_credit_rows: Vec<(String, Vec<String>)> = Vec::with_capacity(listed.len());
    let mut artists_seen: HashSet<String> = HashSet::new();

    for (album, folder_id) in listed {
        let album_id = format!("navidrome:album:{}", album.id);
        let artist_id = album
            .artist_id
            .as_ref()
            .map(|id| format!("navidrome:artist:{id}"));

        // Every artist the album is credited to, each under its *own* name.
        // `album.artist` is the server's display string — two artists come back
        // as "A • B" with the id of A alone, so taking the pair at face value
        // filed A's name as "A • B" and left B out of the artist table.
        let credits = album_credits(&album);
        for (aid, name) in &credits {
            if artists_seen.insert(aid.clone()) {
                artist_rows.push((
                    aid.clone(),
                    name.clone(),
                    folder_id.clone(),
                    artist_covers.get(aid).cloned(),
                ));
            }
        }
        album_credit_rows.push((
            album_id.clone(),
            credits.iter().map(|(id, _)| id.clone()).collect(),
        ));

        // The album row is rewritten either way: `play_count` and `starred` are
        // the New/Frequent/Starred tabs' sort keys and move without the track
        // list changing at all, so skipping the upsert for an unchanged album
        // would leave those tabs sorting on stale numbers.
        album_rows.push(AlbumRow {
            id: album_id.clone(),
            source: "navidrome".into(),
            title: album.name.clone(),
            artist: album.artist.clone(),
            artist_id,
            year: album.year,
            cover_art: album.cover_art.clone(),
            song_count: album.song_count.unwrap_or(0) as i64,
            duration: album.duration.unwrap_or(0) as f64,
            created: album.created.clone(),
            play_count: album.play_count.map(|c| c as i64),
            starred: album.starred.clone(),
            library_id: folder_id.clone(),
        });

        if needs_track_fetch(&album, cached.get(&album_id)) {
            stale.push((album, folder_id));
        }
    }

    // One transaction for the lot. Row-at-a-time autocommits dominated the
    // whole incremental sync — a thousand commits against a library where
    // nothing had changed.
    db.upsert_catalog("navidrome", &album_rows, &artist_rows, &album_credit_rows)?;

    // Albums the server no longer lists. `seen` holds bare server ids; the
    // cache keys are namespaced, so compare on the namespaced form.
    //
    // Only safe when this pass walked every library: restricted to one music
    // folder, `seen` holds that folder's albums alone and everything cached
    // from the other libraries would look deleted.
    if music_folder_id.is_none() {
        let mut removed = 0usize;
        for cached_id in cached.keys() {
            let bare = cached_id
                .strip_prefix("navidrome:album:")
                .unwrap_or(cached_id);
            if !seen.contains(bare) {
                let _ = db.delete_album_with_tracks(cached_id);
                removed += 1;
            }
        }
        if removed > 0 {
            let _ = db.prune_orphan_artists("navidrome");
            tracing::info!("removed {removed} albums no longer on the server");
        }
    }

    progress.total.store(stale.len(), Ordering::Relaxed);
    tracing::info!("navidrome sync: {} albums need tracks", stale.len());

    // Phase 3 — one `getAlbum` per stale album, `ALBUM_FETCH_CONCURRENCY` at a
    // time.
    let mut pending = stale.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        while tasks.len() < ALBUM_FETCH_CONCURRENCY
            && let Some((album, _)) = pending.next()
        {
            let client = client.clone();
            let db = db.clone();
            tasks.spawn(async move { fetch_album_tracks(&db, &client, &album, now).await });
        }
        if tasks.join_next().await.is_none() {
            break;
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }

    // Record sync timestamp
    db.upsert_config("navidrome_last_sync", &now.to_string())?;

    let c = db.track_count_by_source("navidrome")?;
    tracing::info!("navidrome sync done: {c} tracks");
    Ok(())
}

/// Fetch one album's songs and upsert them. Errors are logged, not propagated:
/// a single unreadable album must not abandon the rest of the sync.
async fn fetch_album_tracks(
    db: &LibraryDb,
    client: &SubsonicClient,
    album: &subsonic::Album,
    now: i64,
) {
    let album_with = match client.get_album(&album.id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("getAlbum {} failed: {e}", album.id);
            return;
        }
    };
    let album_id = format!("navidrome:album:{}", album.id);
    // Clear the album's tracks first. Upserts alone would leave behind rows for
    // songs deleted from the album since the last sync — under a full sync the
    // wipe covered that, but an incremental one re-fetches in place.
    let _ = db.delete_tracks_for_album(&album_id);
    let album_artist_id = album
        .artist_id
        .as_ref()
        .map(|id| format!("navidrome:artist:{id}"));
    for song in &album_with.song {
        let track_id = format!("navidrome:track:{}", song.id);
        // The song's *own* artist, not the album's: writing the album artist
        // onto every track made a featured artist indistinguishable from the
        // credited one, which is what the artist page's "Appears on" section
        // reads. Songs without one fall back to the album artist so the column
        // is still populated for servers that omit it.
        let artist_id = song
            .artist_id
            .as_ref()
            .map(|id| format!("navidrome:artist:{id}"))
            .or_else(|| album_artist_id.clone());
        let metadata = TrackMetadata {
            suffix: song.suffix.clone(),
            content_type: song.content_type.clone(),
            bit_rate: song.bit_rate.map(i64::from),
            sampling_rate: song.sampling_rate.map(i64::from),
            bit_depth: song.bit_depth.map(i64::from),
            channel_count: song.channel_count.map(i64::from),
            file_size: song.size.and_then(|value| i64::try_from(value).ok()),
            replay_gain_track: song
                .replay_gain
                .as_ref()
                .and_then(|gain| gain.track_gain)
                .map(f64::from),
            replay_gain_album: song
                .replay_gain
                .as_ref()
                .and_then(|gain| gain.album_gain)
                .map(f64::from),
            replay_peak_track: song
                .replay_gain
                .as_ref()
                .and_then(|gain| gain.track_peak)
                .map(f64::from),
            replay_peak_album: song
                .replay_gain
                .as_ref()
                .and_then(|gain| gain.album_peak)
                .map(f64::from),
            play_count: song.play_count.and_then(|value| i64::try_from(value).ok()),
            ..TrackMetadata::default()
        };
        let _ = db.upsert_track_with_metadata(
            &track_id,
            "navidrome",
            &song.title,
            song.artist.as_deref(),
            artist_id.as_deref(),
            song.album.as_deref(),
            Some(&album_id),
            album.artist.as_deref(),
            song.track.map(|t| t as i32),
            song.disc_number.map(|d| d as i32),
            song.year,
            song.genre.as_deref(),
            song.duration.map(|d| d as f64),
            None, // local_path
            song.cover_art.as_deref(),
            Some(now),
            &metadata,
        );
        let _ = db.set_track_artists(&track_id, &song_credits(song, artist_id.as_deref()));
    }
}

/// Every artist an album is credited to, as `(namespaced id, name)`.
///
/// OpenSubsonic's `artists` array is the only place the second album artist
/// appears: `artist` is a display string joining every name and `artistId` is
/// the first credit alone, so a record by two artists is one artist's own album
/// and looks like a guest spot to the other. A vanilla server sends no array,
/// and then the single pair is all there is.
fn album_credits(album: &subsonic::Album) -> Vec<(String, String)> {
    if !album.artists.is_empty() {
        return album
            .artists
            .iter()
            .map(|a| (format!("navidrome:artist:{}", a.id), a.name.clone()))
            .collect();
    }
    match (&album.artist_id, &album.artist) {
        (Some(id), Some(name)) => vec![(format!("navidrome:artist:{id}"), name.clone())],
        _ => Vec::new(),
    }
}

/// Every artist id credited on a song, namespaced for the cache.
///
/// OpenSubsonic's `artists` array is the only place a collaboration is spelled
/// out — `artistId` carries whichever participant the server leads with, so a
/// remix credited to "irossa • Amore Audio" looks like an irossa track and the
/// guest cannot be found at all. A vanilla server sends no array, and then the
/// single id (already falling back to the album artist) is the whole truth.
fn song_credits(song: &subsonic::Song, primary: Option<&str>) -> Vec<String> {
    if !song.artists.is_empty() {
        return song
            .artists
            .iter()
            .map(|a| format!("navidrome:artist:{}", a.id))
            .collect();
    }
    primary.map(str::to_string).into_iter().collect()
}

/// How often the server scan is polled while it runs.
const SCAN_POLL: Duration = Duration::from_secs(1);
/// Give up watching the server's scan after this. It keeps running server-side;
/// only our watching of it stops.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Navidrome answers `scanning: false` for a moment after accepting a
/// `startScan`, before its scanner thread picks the job up. Only trust an idle
/// reading straight away once the scan has been seen running; otherwise wait
/// this long before believing it.
const SCAN_GRACE: Duration = Duration::from_secs(5);

/// Ask the server to rescan its media directories and watch until it finishes.
///
/// Separate from `sync_navidrome`, and deliberately not part of the Refresh
/// button: a server-side rescan walks the whole music tree and is the slow,
/// occasional operation, whereas refreshing the local cache against what the
/// server already knows is the fast, frequent one.
///
/// `files` is updated with the running file count for the UI. Navidrome
/// restricts `startScan` to admins and answers error 50 for everyone else, so
/// an `Err` here is a normal outcome to report, not a bug.
pub async fn run_server_scan(client: &SubsonicClient, files: Arc<AtomicU64>) -> Result<u64> {
    let started = client.start_scan().await?;
    files.store(started.count.unwrap_or(0), Ordering::Relaxed);
    let mut observed_running = started.scanning;

    let deadline = Instant::now() + SCAN_TIMEOUT;
    let grace_ends = Instant::now() + SCAN_GRACE;
    while Instant::now() < deadline {
        // Inside the tokio runtime, so `tokio::time` is available here — unlike
        // in the gpui tasks that call this.
        tokio::time::sleep(SCAN_POLL).await;
        let status = client.get_scan_status().await?;
        let count = status.count.unwrap_or(0);
        files.store(count, Ordering::Relaxed);
        if status.scanning {
            observed_running = true;
        } else if observed_running || Instant::now() > grace_ends {
            tracing::info!("server scan finished: {count} files");
            return Ok(count);
        }
    }
    Err(anyhow!(
        "server scan still running after {} minutes",
        SCAN_TIMEOUT.as_secs() / 60
    ))
}

/// Remove all navidrome-sourced tracks, albums, and artists from the DB.
fn remove_navidrome(db: &LibraryDb) -> Result<i64> {
    let count = db.track_count_by_source("navidrome")?;
    db.execute("DELETE FROM tracks WHERE source = 'navidrome'")?;
    db.execute("DELETE FROM albums WHERE source = 'navidrome'")?;
    db.execute("DELETE FROM artists WHERE source = 'navidrome'")?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Arc<LibraryDb> {
        Arc::new(LibraryDb::open_in_memory().unwrap())
    }

    fn song(json: serde_json::Value) -> subsonic::Song {
        let mut base = serde_json::json!({ "id": "s1", "title": "Song" });
        let map = base.as_object_mut().unwrap();
        for (k, v) in json.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
        serde_json::from_value(base).unwrap()
    }

    /// `artist` is a display string joining both names and `artistId` names
    /// only the first, so taking the pair at face value filed the first artist
    /// under "A • B" and never recorded the second at all.
    #[test]
    fn an_album_by_two_artists_credits_each_under_its_own_name() {
        let mut album = listed_album(1, 100);
        album.artist = Some("Nico Arezzo • Amore Audio".into());
        album.artist_id = Some("na".into());
        album.artists = vec![
            subsonic::ArtistRef {
                id: "na".into(),
                name: "Nico Arezzo".into(),
            },
            subsonic::ArtistRef {
                id: "aa".into(),
                name: "Amore Audio".into(),
            },
        ];
        assert_eq!(
            album_credits(&album),
            vec![
                ("navidrome:artist:na".to_string(), "Nico Arezzo".to_string()),
                ("navidrome:artist:aa".to_string(), "Amore Audio".to_string()),
            ]
        );
    }

    #[test]
    fn a_server_without_the_album_array_credits_its_one_artist() {
        let album = listed_album(1, 100);
        assert_eq!(
            album_credits(&album),
            vec![("navidrome:artist:ar1".to_string(), "Artist".to_string())]
        );
    }

    #[test]
    fn every_credited_artist_is_recorded_not_only_the_primary() {
        let s = song(serde_json::json!({
            "artistId": "ir",
            "artists": [{ "id": "ir", "name": "irossa" }, { "id": "aa", "name": "Amore Audio" }],
        }));
        assert_eq!(
            song_credits(&s, Some("navidrome:artist:ir")),
            vec!["navidrome:artist:ir", "navidrome:artist:aa"]
        );
    }

    #[test]
    fn a_server_without_the_array_credits_the_one_artist() {
        let s = song(serde_json::json!({ "artistId": "ir" }));
        assert_eq!(
            song_credits(&s, Some("navidrome:artist:ir")),
            vec!["navidrome:artist:ir"]
        );
    }

    #[test]
    fn a_song_with_no_artist_at_all_is_credited_to_nobody() {
        let s = song(serde_json::json!({}));
        assert!(song_credits(&s, None).is_empty());
    }

    fn listed_album(song_count: u32, duration: u32) -> subsonic::Album {
        subsonic::Album {
            id: "a1".into(),
            name: "Album".into(),
            artist: Some("Artist".into()),
            artist_id: Some("ar1".into()),
            cover_art: None,
            song_count: Some(song_count),
            duration: Some(duration),
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
            artists: Vec::new(),
            original_release_date: None,
            release_date: None,
        }
    }

    fn fingerprint(song_count: i64, duration: f64, track_rows: i64) -> AlbumFingerprint {
        AlbumFingerprint {
            song_count,
            duration,
            track_rows,
        }
    }

    #[test]
    fn an_album_not_in_the_cache_needs_its_tracks() {
        assert!(needs_track_fetch(&listed_album(10, 2400), None));
    }

    #[test]
    fn an_unchanged_album_is_left_alone() {
        // The whole point: a refresh over a settled library issues no getAlbum
        // calls at all.
        let cached = fingerprint(10, 2400.0, 10);
        assert!(!needs_track_fetch(&listed_album(10, 2400), Some(&cached)));
    }

    #[test]
    fn a_changed_track_count_or_duration_forces_a_refetch() {
        let cached = fingerprint(10, 2400.0, 10);
        assert!(needs_track_fetch(&listed_album(11, 2400), Some(&cached)));
        assert!(needs_track_fetch(&listed_album(10, 2600), Some(&cached)));
    }

    #[test]
    fn rounding_in_the_reported_duration_is_not_a_change() {
        // Servers round album durations and the value round-trips through an
        // f64 column; an exact compare re-fetched albums that hadn't moved.
        let cached = fingerprint(10, 2400.4, 10);
        assert!(!needs_track_fetch(&listed_album(10, 2400), Some(&cached)));
    }

    #[test]
    fn an_album_row_whose_tracks_never_landed_is_refetched() {
        // A sync interrupted between phase 2 and phase 3 leaves album rows with
        // no tracks. `song_count` alone would call that complete.
        let cached = fingerprint(10, 2400.0, 0);
        assert!(needs_track_fetch(&listed_album(10, 2400), Some(&cached)));
    }

    #[test]
    fn fingerprints_count_the_tracks_actually_present() {
        let db = test_db();
        db.upsert_album(&AlbumRow {
            id: "navidrome:album:a1".into(),
            source: "navidrome".into(),
            title: "Album".into(),
            artist: None,
            artist_id: None,
            year: None,
            cover_art: None,
            song_count: 2,
            duration: 300.0,
            created: None,
            play_count: None,
            starred: None,
            library_id: None,
        })
        .unwrap();
        db.upsert_track(
            "navidrome:track:t1",
            "navidrome",
            "Song",
            None,
            None,
            None,
            Some("navidrome:album:a1"),
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

        let fps = db.album_fingerprints("navidrome").unwrap();
        let fp = fps.get("navidrome:album:a1").unwrap();
        assert_eq!(fp.song_count, 2);
        assert_eq!(fp.track_rows, 1, "only one track was actually inserted");
        assert!(needs_track_fetch(&listed_album(2, 300), Some(fp)));
    }

    #[test]
    fn deleting_an_album_takes_its_tracks_with_it() {
        let db = test_db();
        db.upsert_album(&AlbumRow::new("navidrome:album:a1", "navidrome", "Album"))
            .unwrap();
        db.upsert_track(
            "navidrome:track:t1",
            "navidrome",
            "Song",
            None,
            None,
            None,
            Some("navidrome:album:a1"),
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
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 1);

        db.delete_album_with_tracks("navidrome:album:a1").unwrap();
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 0);
        assert!(db.album_fingerprints("navidrome").unwrap().is_empty());
    }

    #[test]
    fn pruning_drops_only_artists_no_album_points_at() {
        let db = test_db();
        db.upsert_artist("navidrome:artist:keep", "navidrome", "Keep", None, None)
            .unwrap();
        db.upsert_artist("navidrome:artist:drop", "navidrome", "Drop", None, None)
            .unwrap();
        let mut album = AlbumRow::new("navidrome:album:a1", "navidrome", "Album");
        album.artist_id = Some("navidrome:artist:keep".into());
        db.upsert_album(&album).unwrap();

        assert_eq!(db.prune_orphan_artists("navidrome").unwrap(), 1);
        let names: Vec<_> = db
            .artists_by_source("navidrome")
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, vec!["Keep".to_string()]);
    }

    #[test]
    fn remove_navidrome_clears_tracks() {
        let db = test_db();
        db.upsert_track(
            "n1",
            "navidrome",
            "Song",
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
            None,
        )
        .unwrap();
        db.upsert_track(
            "l1", "local", "Local", None, None, None, None, None, None, None, None, None, None,
            None, None, None,
        )
        .unwrap();
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 1);
        assert_eq!(db.track_count_by_source("local").unwrap(), 1);
        remove_navidrome(&db).unwrap();
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 0);
        assert_eq!(db.track_count_by_source("local").unwrap(), 1);
    }

    #[test]
    fn remove_navidrome_idempotent() {
        let db = test_db();
        remove_navidrome(&db).unwrap();
        remove_navidrome(&db).unwrap(); // no error on empty
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 0);
    }
}
