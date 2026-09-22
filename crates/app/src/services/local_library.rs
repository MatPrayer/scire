//! Local music file scanner. Walks configured directories, reads tags via
//! lofty, populates the SQLite library DB.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use anyhow::Result;
use lofty::AudioFile;
use lofty::TaggedFileExt;
use lofty::{Accessor, ItemKey};

use crate::services::artwork;
use crate::services::library_db::{AlbumRow, LibraryDb, TrackMetadata};

pub const IDLE: u8 = 0;
pub const SCANNING: u8 = 1;
pub const DONE: u8 = 2;

/// Set while *any* scanner is walking the disk.
///
/// The 5-minute background rescan and the sidebar's manual refresh each build
/// their own `LocalScanner`, so a per-instance flag doesn't see the other one:
/// a manual refresh landing on a background tick had two walks competing for
/// the same SQLite write lock, and each one's `cleanup_stale_entries` ran
/// against a half-filled `seen` set.
static SCAN_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Local music directory scanner.
pub struct LocalScanner {
    db: Arc<LibraryDb>,
    status: Arc<AtomicU8>,
    progress: Arc<AtomicUsize>,
}

impl LocalScanner {
    pub fn new(db: Arc<LibraryDb>) -> Self {
        Self {
            db,
            status: Arc::new(AtomicU8::new(IDLE)),
            progress: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn status(&self) -> u8 {
        self.status.load(Ordering::Relaxed)
    }
    pub fn progress(&self) -> usize {
        self.progress.load(Ordering::Relaxed)
    }

    /// Whether any scanner is currently walking the disk.
    pub fn scan_in_flight() -> bool {
        SCAN_IN_FLIGHT.load(Ordering::Relaxed)
    }

    /// Scan directories, populate DB. Synchronous and long-running — callers
    /// must use `runtime::spawn_blocking_io`, never `spawn_io`.
    ///
    /// A no-op while another scan is walking the same directories.
    pub fn scan(&self, dirs: &[PathBuf]) -> Result<()> {
        if SCAN_IN_FLIGHT.swap(true, Ordering::SeqCst) {
            tracing::debug!("local scan already running; skipping");
            return Ok(());
        }
        let _guard = ScanGuard;
        self.scan_locked(dirs, true)
    }

    /// Clear generated covers, invalidate fingerprints, and scan while holding
    /// the process-wide guard for the entire maintenance job.
    pub fn rebuild_cache(&self, dirs: &[PathBuf]) -> Result<()> {
        let started = std::time::Instant::now();
        while SCAN_IN_FLIGHT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            if started.elapsed() >= std::time::Duration::from_secs(30 * 60) {
                anyhow::bail!("timed out waiting for the current local scan");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _guard = ScanGuard;
        self.db.invalidate_local_scan_fingerprints()?;
        self.scan_locked(dirs, false)?;
        prune_local_art_cache(&self.db)
    }

    fn scan_locked(&self, dirs: &[PathBuf], preserve_existing_covers: bool) -> Result<()> {
        self.status.store(SCANNING, Ordering::Relaxed);
        self.progress.store(0, Ordering::Relaxed);
        let exts = [
            "flac", "mp3", "ogg", "opus", "wav", "aiff", "aac", "m4a", "m4b",
        ];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut album_covers: std::collections::HashMap<String, String> =
            if preserve_existing_covers {
                self.db
                    .albums_by_source("local")?
                    .into_iter()
                    .filter_map(|album| album.cover_art.map(|cover| (album.id, cover)))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };
        for dir in dirs {
            if !dir.is_dir() {
                tracing::warn!("local music dir not found: {dir:?}");
                continue;
            }
            scan_dir(
                self.db.clone(),
                dir,
                &exts,
                &self.progress,
                &mut seen,
                &mut album_covers,
            )?;
        }
        cleanup_stale_entries(&self.db, &seen);
        // Update album stats from tracks (avoids per-file accounting).
        let _ = self.db.conn.lock().unwrap().execute_batch(
            "UPDATE albums SET
               song_count = (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id),
               duration   = (SELECT COALESCE(SUM(duration), 0) FROM tracks WHERE tracks.album_id = albums.id),
               created    = (
                   SELECT datetime(MIN(file_created), 'unixepoch')
                   FROM tracks
                   WHERE tracks.album_id = albums.id
               )
             WHERE source = 'local'",
        );
        self.db.bump_scan_version();
        self.status.store(DONE, Ordering::Relaxed);
        Ok(())
    }
}

/// Clears `SCAN_IN_FLIGHT` however `scan` leaves — including on the `?` that
/// an unreadable directory can raise, which would otherwise wedge the flag on
/// and silence every later scan for the rest of the session.
struct ScanGuard;

impl Drop for ScanGuard {
    fn drop(&mut self) {
        SCAN_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

fn scan_dir(
    db: Arc<LibraryDb>,
    dir: &Path,
    exts: &[&str],
    progress: &AtomicUsize,
    seen: &mut std::collections::HashSet<String>,
    album_covers: &mut std::collections::HashMap<String, String>,
) -> Result<()> {
    // ponytail: scan m3u files found directly in each root dir (not recursed).
    // Full m3u-tree scanning is O(extra IO); current approach checks root only.
    for entry in std::fs::read_dir(dir).ok().into_iter().flatten() {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let is_m3u = ext.eq_ignore_ascii_case("m3u") || ext.eq_ignore_ascii_case("m3u8");
            if is_m3u && let Err(e) = import_m3u(&db, &path, dir) {
                tracing::warn!("error importing m3u {path:?}: {e}");
            }
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("cannot read dir {dir:?}: {e}");
            return Ok(());
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with('.')
        {
            continue;
        }
        if path.is_dir() {
            scan_dir(db.clone(), &path, exts, progress, seen, album_covers)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if exts.contains(&ext.to_lowercase().as_str()) {
                let path_str = path.to_string_lossy().to_string();
                if let Err(e) = scan_file(&db, &path, album_covers) {
                    tracing::warn!("error scanning {path:?}: {e}");
                }
                seen.insert(path_str);
                progress.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn scan_file(
    db: &LibraryDb,
    path: &Path,
    album_covers: &mut std::collections::HashMap<String, String>,
) -> Result<()> {
    let path_str = path.to_string_lossy();
    let file_metadata = std::fs::metadata(path).ok();
    let modified = file_metadata
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let id = path_to_id(&canonical, "local");

    // ponytail: O(n) lookup per file; fine until ~10k files. Add index if slow.
    if let Ok(Some(existing)) = db.get_track(&id)
        && existing.file_modified.is_some()
        && existing.file_modified == modified
    {
        return Ok(());
    }

    let tagged = match lofty::read_from_path(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("lofty error {path_str}: {e}");
            return Ok(());
        }
    };
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let (title, artist, album_name, track_no, disc_number, year, genre) = if let Some(tag) = tag {
        (
            tag.title().map(|s| s.to_string()).unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            }),
            tag.artist().map(|s| s.to_string()),
            tag.album().map(|s| s.to_string()),
            tag.track(),
            tag.disk(),
            tag.year(),
            tag.genre().map(|s| s.to_string()),
        )
    } else {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();
        (name, None, None, None, None, None, None)
    };

    let properties = tagged.properties();
    let duration = properties.duration().as_secs_f64();
    let duration = if duration > 0.0 { Some(duration) } else { None };
    let suffix = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let content_type = suffix.as_deref().and_then(mime_for_suffix);
    let album_artist = tag_value(&tagged, &ItemKey::AlbumArtist);
    let metadata = TrackMetadata {
        suffix,
        content_type: content_type.map(str::to_string),
        bit_rate: properties.audio_bitrate().map(i64::from),
        sampling_rate: properties.sample_rate().map(i64::from),
        bit_depth: properties.bit_depth().map(i64::from),
        channel_count: properties.channels().map(i64::from),
        file_size: file_metadata
            .as_ref()
            .and_then(|metadata| i64::try_from(metadata.len()).ok()),
        file_created: file_metadata
            .as_ref()
            .and_then(|metadata| metadata.created().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok()),
        replay_gain_track: replaygain_value(&tagged, &ItemKey::ReplayGainTrackGain),
        replay_gain_album: replaygain_value(&tagged, &ItemKey::ReplayGainAlbumGain),
        replay_peak_track: replaygain_value(&tagged, &ItemKey::ReplayGainTrackPeak),
        replay_peak_album: replaygain_value(&tagged, &ItemKey::ReplayGainAlbumPeak),
        play_count: None,
    };

    let album_key = format!("local:album:{}", album_name.as_deref().unwrap_or("Unknown"));
    let cover = select_album_cover(
        &album_key,
        extract_cover(path, &album_name, &artist),
        album_covers,
    );
    let artist_key = format!("local:artist:{}", artist.as_deref().unwrap_or("Unknown"));

    if let Some(ref name) = artist {
        let _ = db.upsert_artist(&artist_key, "local", name, cover.as_deref(), None);
    }
    let mut album_row = AlbumRow::new(
        &album_key,
        "local",
        album_name.as_deref().unwrap_or("Unknown"),
    );
    album_row.artist = artist.clone();
    album_row.artist_id = Some(artist_key.clone());
    album_row.year = year.map(|y| y as i32);
    album_row.cover_art = cover.clone();
    let _ = db.upsert_album(&album_row);
    let _ = db.upsert_track_with_metadata(
        &id,
        "local",
        &title,
        artist.as_deref(),
        Some(&artist_key),
        album_name.as_deref(),
        Some(&album_key),
        album_artist.as_deref(),
        track_no.map(|t| t as i32),
        disc_number.map(|d| d as i32),
        year.map(|y| y as i32),
        genre.as_deref(),
        duration,
        Some(&path_str),
        cover.as_deref(),
        modified,
        &metadata,
    );
    Ok(())
}

fn tag_value(tagged: &lofty::TaggedFile, key: &ItemKey) -> Option<String> {
    tagged
        .tags()
        .iter()
        .find_map(|tag| tag.get_string(key))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn replaygain_value(tagged: &lofty::TaggedFile, key: &ItemKey) -> Option<f64> {
    tag_value(tagged, key).and_then(|value| parse_replaygain(&value))
}

fn parse_replaygain(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    let numeric = trimmed
        .strip_suffix("dB")
        .or_else(|| trimmed.strip_suffix("DB"))
        .or_else(|| trimmed.strip_suffix("db"))
        .unwrap_or(trimmed)
        .trim();
    numeric
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn mime_for_suffix(suffix: &str) -> Option<&'static str> {
    match suffix {
        "flac" => Some("audio/flac"),
        "mp3" => Some("audio/mpeg"),
        "ogg" | "oga" => Some("audio/ogg"),
        "opus" => Some("audio/opus"),
        "wav" => Some("audio/wav"),
        "aiff" | "aif" => Some("audio/aiff"),
        "aac" => Some("audio/aac"),
        "m4a" | "m4b" => Some("audio/mp4"),
        _ => None,
    }
}

/// Remove DB entries for files no longer on disk.
fn cleanup_stale_entries(db: &LibraryDb, seen: &std::collections::HashSet<String>) {
    let Ok(tracks) = db.tracks_by_source("local") else {
        return;
    };
    for t in &tracks {
        let Some(ref local_path) = t.local_path else {
            continue;
        };
        if !seen.contains(local_path) && !std::path::Path::new(local_path).exists() {
            let _ = db.delete_track(&t.id);
        }
    }
}

/// Parse an .m3u/.m3u8 file, upsert playlist + entries into DB.
fn import_m3u(db: &Arc<LibraryDb>, path: &Path, root_dir: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Playlist");
    let id = path_to_id(path, "playlist");

    db.upsert_playlist(&id, name, None)?;
    db.clear_playlist_entries(&id)?;

    let parent = path.parent().unwrap_or(Path::new("."));
    let mut order = 0i32;
    // ponytail: no EXTINF parsing — just paths. EXTINF for display would be
    // nice but requires storing extra metadata. Add when playlist views land.
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // resolve relative to m3u location, or root_dir, or absolute
        let track_path = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            let rel = parent.join(line);
            if rel.exists() {
                rel
            } else {
                root_dir.join(line)
            }
        };
        let track_path = std::fs::canonicalize(&track_path).unwrap_or(track_path);
        if !track_path.exists() {
            continue;
        }
        let track_id = path_to_id(&track_path, "local");
        if let Ok(Some(_track)) = db.get_track(&track_id) {
            db.add_playlist_entry(&id, Some("local"), Some(&track_id), order)?;
            order += 1;
        }
    }
    // update song_count
    if order > 0 {
        let conn = db.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE playlists SET song_count = ?1 WHERE id = ?2",
            rusqlite::params![order, id],
        );
    }
    Ok(())
}

/// Extract cover: try `folder.jpg` in parent dir, fallback to embedded art.
fn extract_cover(path: &Path, _album: &Option<String>, _artist: &Option<String>) -> Option<String> {
    if let Some(parent) = path.parent() {
        for candidate in &["folder.jpg", "cover.jpg", "Folder.jpg", "Cover.jpg"] {
            let cp = parent.join(candidate);
            if cp.is_file()
                && let Some(cached) = cache_cover_file(&cp)
            {
                return Some(cached);
            }
        }
        // embedded fallback
        if let Ok(tagged) = lofty::read_from_path(path)
            && let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag())
            && let Some(pic) = tag.pictures().first()
        {
            let data = pic.data();
            if !data.is_empty() {
                return cache_cover_bytes(data);
            }
        }
    }
    None
}

fn cache_cover_file(src: &Path) -> Option<String> {
    let data = std::fs::read(src).ok()?;
    cache_cover_bytes(&data)
}

fn cache_cover_bytes(data: &[u8]) -> Option<String> {
    let dir = local_art_dir()?;
    cache_cover_bytes_in(data, &dir)
}

fn cache_cover_bytes_in(data: &[u8], dir: &Path) -> Option<String> {
    let hash = cover_hash(data);
    let dest = dir.join(format!("{hash}.jpg"));
    if !dest.exists() {
        std::fs::create_dir_all(dir).ok()?;
        let squared = artwork::square_crop(data);
        std::fs::write(&dest, squared.as_deref().unwrap_or(data)).ok()?;
    }
    dest.is_file().then_some(hash)
}

fn cover_hash(data: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    format!("{:x}", h.finish())
}

fn select_album_cover(
    album_id: &str,
    found: Option<String>,
    album_covers: &mut std::collections::HashMap<String, String>,
) -> Option<String> {
    if let Some(cover) = found {
        album_covers.insert(album_id.to_string(), cover.clone());
        Some(cover)
    } else {
        album_covers.get(album_id).cloned()
    }
}

fn path_to_id(path: &Path, prefix: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{}:{:x}", prefix, h.finish())
}

pub fn local_art_path(hash: &str) -> Option<PathBuf> {
    Some(local_art_dir()?.join(format!("{hash}.jpg")))
}

/// Directory holding art extracted from local files.
pub fn local_art_dir() -> Option<PathBuf> {
    let dir = crate::config::project_dirs().ok()?;
    Some(dir.cache_dir().join("local_art"))
}

fn prune_local_art_cache(db: &LibraryDb) -> Result<()> {
    let Some(dir) = local_art_dir() else {
        return Ok(());
    };
    let mut referenced = std::collections::HashSet::new();
    for album in db.albums_by_source("local")? {
        if let Some(hash) = album.cover_art {
            referenced.insert(hash);
        }
    }
    for track in db.tracks_by_source("local")? {
        if let Some(hash) = track.cover_art {
            referenced.insert(hash);
        }
    }
    prune_local_art_dir(&dir, &referenced)
}

fn prune_local_art_dir(dir: &Path, referenced: &std::collections::HashSet<String>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        // The squaring marker and a download's temp file are the directory's
        // own bookkeeping, not covers: nothing in the DB references either, so
        // an unfiltered sweep reads both as orphans. Losing the marker re-runs
        // the whole squaring pass on the next launch for nothing.
        let bookkeeping = entry.file_name() == std::ffi::OsStr::new(artwork::SQUARED_MARKER)
            || path.extension().is_some_and(|ext| ext == "part");
        let keep = bookkeeping
            || path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|hash| referenced.contains(hash));
        if keep {
            continue;
        }
        if file_type.is_dir() && !file_type.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// `SCAN_IN_FLIGHT` is process-global, so two scanning tests running at once
    /// would have one of them skip its scan and report IDLE. They also shared
    /// one temp directory, which each `test_scanner` wipes on entry.
    static SCAN_TESTS: Mutex<()> = Mutex::new(());

    fn test_db() -> Arc<LibraryDb> {
        Arc::new(LibraryDb::open_in_memory().unwrap())
    }

    fn test_scanner() -> (LocalScanner, PathBuf, MutexGuard<'static, ()>) {
        let guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("scire-local-lib-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scanner = LocalScanner::new(test_db());
        (scanner, dir, guard)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn wav_bytes() -> Vec<u8> {
        let samples = [0_i16; 8];
        let data_len = (samples.len() * std::mem::size_of::<i16>()) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100_u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn scan_empty_dir() {
        let (scanner, dir, _guard) = test_scanner();
        scanner.scan(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(scanner.status(), DONE);
        assert_eq!(scanner.progress(), 0);
        cleanup(&dir);
    }

    #[test]
    fn scan_skips_dot_dirs() {
        let (scanner, dir, _guard) = test_scanner();
        let dotdir = dir.join(".hidden");
        std::fs::create_dir_all(&dotdir).unwrap();
        let f = dotdir.join("song.flac");
        std::fs::write(&f, b"not a real flac").unwrap();
        scanner.scan(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(scanner.status(), DONE);
        cleanup(&dir);
    }

    #[test]
    fn scan_nonexistent_dir_skips_gracefully() {
        // Holds the same lock as the other scanning tests: `scan` is a no-op
        // while another one holds `SCAN_IN_FLIGHT`, and the status would then
        // never leave IDLE.
        let _guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let scanner = LocalScanner::new(test_db());
        scanner
            .scan(&[PathBuf::from("/nonexistent_path_xyz")])
            .unwrap();
        assert_eq!(scanner.status(), DONE);
    }

    #[test]
    fn a_second_scan_is_skipped_while_one_is_in_flight() {
        let _guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        SCAN_IN_FLIGHT.store(true, Ordering::SeqCst);
        let scanner = LocalScanner::new(test_db());
        let result = scanner.scan(&[PathBuf::from("/nonexistent_path_xyz")]);
        SCAN_IN_FLIGHT.store(false, Ordering::SeqCst);
        // Skipped, not failed — the caller's refresh carries on to the import.
        assert!(result.is_ok());
        assert_eq!(scanner.status(), IDLE);
    }

    #[test]
    fn the_in_flight_flag_clears_after_a_scan() {
        let _guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let scanner = LocalScanner::new(test_db());
        scanner
            .scan(&[PathBuf::from("/nonexistent_path_xyz")])
            .unwrap();
        assert!(!LocalScanner::scan_in_flight());
    }

    #[test]
    fn path_to_id_is_deterministic() {
        let a = path_to_id(Path::new("/music/test.flac"), "local");
        let b = path_to_id(Path::new("/music/test.flac"), "local");
        assert_eq!(a, b);
    }

    #[test]
    fn path_to_id_differs_by_prefix() {
        let a = path_to_id(Path::new("/music/test.flac"), "local");
        let b = path_to_id(Path::new("/music/test.flac"), "navidrome");
        assert_ne!(a, b);
    }

    #[test]
    fn pruning_local_art_keeps_references_and_stays_inside_target() {
        let root =
            std::env::temp_dir().join(format!("scire-local-art-test-{}", std::process::id()));
        let cache = root.join("local_art");
        std::fs::create_dir_all(cache.join("nested")).unwrap();
        std::fs::write(cache.join("keep.jpg"), b"cover").unwrap();
        std::fs::write(cache.join("remove.jpg"), b"old").unwrap();
        std::fs::write(cache.join("nested/cover.jpg"), b"cover").unwrap();
        std::fs::write(cache.join(artwork::SQUARED_MARKER), b"").unwrap();
        std::fs::write(cache.join("half-written.part"), b"partial").unwrap();
        let sentinel = root.join("keep.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        let referenced = std::collections::HashSet::from(["keep".to_string()]);

        prune_local_art_dir(&cache, &referenced).unwrap();

        assert!(cache.exists());
        assert_eq!(std::fs::read(cache.join("keep.jpg")).unwrap(), b"cover");
        assert!(!cache.join("remove.jpg").exists());
        assert!(!cache.join("nested").exists());
        // The directory's own bookkeeping is not a cover to be collected.
        assert!(cache.join(artwork::SQUARED_MARKER).exists());
        assert!(cache.join("half-written.part").exists());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        cleanup(&root);
    }

    #[test]
    fn folder_cover_key_changes_with_content() {
        assert_eq!(cover_hash(b"same"), cover_hash(b"same"));
        assert_ne!(cover_hash(b"old"), cover_hash(b"new"));
    }

    #[test]
    fn cover_reference_is_returned_only_after_file_exists() {
        let root =
            std::env::temp_dir().join(format!("scire-cover-write-test-{}", std::process::id()));
        cleanup(&root);
        let hash = cache_cover_bytes_in(b"cover bytes", &root).unwrap();
        assert_eq!(
            std::fs::read(root.join(format!("{hash}.jpg"))).unwrap(),
            b"cover bytes"
        );

        let blocked = root.join("not-a-directory");
        std::fs::write(&blocked, b"file").unwrap();
        assert!(cache_cover_bytes_in(b"other", &blocked).is_none());
        cleanup(&root);
    }

    #[test]
    fn later_track_without_art_keeps_album_cover_found_this_scan() {
        let mut covers = std::collections::HashMap::new();
        assert_eq!(
            select_album_cover("album", Some("cover".into()), &mut covers),
            Some("cover".into())
        );
        assert_eq!(
            select_album_cover("album", None, &mut covers),
            Some("cover".into())
        );
    }

    #[test]
    fn invalidating_fingerprints_forces_unchanged_file_rescan() {
        let _guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("scire-rescan-test-{}", std::process::id()));
        cleanup(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Original.wav");
        std::fs::write(&path, wav_bytes()).unwrap();
        let db = Arc::new(LibraryDb::open_in_memory().unwrap());
        let scanner = LocalScanner::new(db.clone());
        scanner.scan(std::slice::from_ref(&dir)).unwrap();
        let id = path_to_id(&std::fs::canonicalize(&path).unwrap(), "local");
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE tracks SET title = 'Stale' WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();

        db.invalidate_local_scan_fingerprints().unwrap();
        scanner.scan(std::slice::from_ref(&dir)).unwrap();

        assert_eq!(db.get_track(&id).unwrap().unwrap().title, "Original");
        cleanup(&dir);
    }

    #[test]
    fn scanner_records_file_properties() {
        let _guard = SCAN_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("scire-metadata-test-{}", std::process::id()));
        cleanup(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Properties.wav");
        std::fs::write(&path, wav_bytes()).unwrap();
        let db = Arc::new(LibraryDb::open_in_memory().unwrap());
        LocalScanner::new(db.clone())
            .scan(std::slice::from_ref(&dir))
            .unwrap();
        let id = path_to_id(&std::fs::canonicalize(&path).unwrap(), "local");
        let track = db.get_track(&id).unwrap().unwrap();

        assert_eq!(track.suffix.as_deref(), Some("wav"));
        assert_eq!(track.content_type.as_deref(), Some("audio/wav"));
        assert_eq!(track.sampling_rate, Some(44_100));
        assert_eq!(track.bit_depth, Some(16));
        assert_eq!(track.channel_count, Some(1));
        assert_eq!(track.file_size, Some(wav_bytes().len() as i64));
        cleanup(&dir);
    }

    #[test]
    fn replaygain_parser_accepts_units_and_rejects_bad_values() {
        assert_eq!(parse_replaygain(" -7.20 dB "), Some(-7.2));
        assert_eq!(parse_replaygain("0.9876"), Some(0.9876));
        assert_eq!(parse_replaygain("not gain"), None);
        assert_eq!(parse_replaygain("NaN"), None);
    }
}
