//! Local music file scanner. Walks configured directories, reads tags via
//! lofty, populates the SQLite library DB.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use anyhow::Result;
use lofty::Accessor;
use lofty::AudioFile;
use lofty::TaggedFileExt;

use crate::services::library_db::LibraryDb;

pub const IDLE: u8 = 0;
pub const SCANNING: u8 = 1;
pub const DONE: u8 = 2;

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

    /// Scan directories, populate DB. Synchronous and long-running — callers
    /// must use `runtime::spawn_blocking_io`, never `spawn_io`.
    pub fn scan(&self, dirs: &[PathBuf]) -> Result<()> {
        self.status.store(SCANNING, Ordering::Relaxed);
        self.progress.store(0, Ordering::Relaxed);
        let exts = [
            "flac", "mp3", "ogg", "opus", "wav", "aiff", "aac", "m4a", "m4b",
        ];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for dir in dirs {
            if !dir.is_dir() {
                tracing::warn!("local music dir not found: {dir:?}");
                continue;
            }
            scan_dir(self.db.clone(), dir, &exts, &self.progress, &mut seen)?;
        }
        cleanup_stale_entries(&self.db, &seen);
        // Update album stats from tracks (avoids per-file accounting).
        let _ = self.db.conn.lock().unwrap().execute_batch(
            "UPDATE albums SET
               song_count = (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id),
               duration   = (SELECT COALESCE(SUM(duration), 0) FROM tracks WHERE tracks.album_id = albums.id)
             WHERE source = 'local'",
        );
        self.db.bump_scan_version();
        self.status.store(DONE, Ordering::Relaxed);
        Ok(())
    }
}

fn scan_dir(
    db: Arc<LibraryDb>,
    dir: &Path,
    exts: &[&str],
    progress: &AtomicUsize,
    seen: &mut std::collections::HashSet<String>,
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
            scan_dir(db.clone(), &path, exts, progress, seen)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if exts.contains(&ext.to_lowercase().as_str()) {
                let path_str = path.to_string_lossy().to_string();
                if let Err(e) = scan_file(&db, &path) {
                    tracing::warn!("error scanning {path:?}: {e}");
                }
                seen.insert(path_str);
                progress.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn scan_file(db: &LibraryDb, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy();
    let modified = std::fs::metadata(path)
        .ok()
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

    let duration = tagged.properties().duration().as_secs_f64();
    let duration = if duration > 0.0 { Some(duration) } else { None };

    let cover = extract_cover(path, &album_name, &artist);
    let album_key = format!("local:album:{}", album_name.as_deref().unwrap_or("Unknown"));
    let artist_key = format!("local:artist:{}", artist.as_deref().unwrap_or("Unknown"));

    if let Some(ref name) = artist {
        let _ = db.upsert_artist(&artist_key, "local", name, cover.as_deref());
    }
    let _ = db.upsert_album(
        &album_key,
        "local",
        album_name.as_deref().unwrap_or("Unknown"),
        artist.as_deref(),
        Some(&artist_key),
        year.map(|y| y as i32),
        cover.as_deref(),
        0,
        0.0,
    );
    let _ = db.upsert_track(
        &id,
        "local",
        &title,
        artist.as_deref(),
        Some(&artist_key),
        album_name.as_deref(),
        Some(&album_key),
        None, // album_artist — lofty 0.18 Accessor lacks this
        track_no.map(|t| t as i32),
        disc_number.map(|d| d as i32),
        year.map(|y| y as i32),
        genre.as_deref(),
        duration,
        Some(&path_str),
        cover.as_deref(),
        modified,
    );
    Ok(())
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
    let hash = simple_hash(src)?;
    let dest = local_art_path(&hash)?;
    if !dest.exists() {
        let _ = std::fs::create_dir_all(dest.parent()?);
        let _ = std::fs::copy(src, &dest);
    }
    Some(hash)
}

fn cache_cover_bytes(data: &[u8]) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    let hash = format!("{:x}", h.finish());
    let dest = local_art_path(&hash)?;
    if !dest.exists() {
        let _ = std::fs::create_dir_all(dest.parent()?);
        let _ = std::fs::write(&dest, data);
    }
    Some(hash)
}

fn path_to_id(path: &Path, prefix: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{}:{:x}", prefix, h.finish())
}

fn simple_hash(path: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    Some(format!("{:x}", h.finish()))
}

pub fn local_art_path(hash: &str) -> Option<PathBuf> {
    let dir = crate::config::project_dirs().ok()?;
    let cache = dir.cache_dir().join("local_art");
    Some(cache.join(format!("{hash}.jpg")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_db() -> Arc<LibraryDb> {
        Arc::new(LibraryDb::open_in_memory().unwrap())
    }

    fn test_scanner() -> (LocalScanner, PathBuf) {
        let dir = std::env::temp_dir().join(format!("scire-local-lib-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scanner = LocalScanner::new(test_db());
        (scanner, dir)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_empty_dir() {
        let (scanner, dir) = test_scanner();
        scanner.scan(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(scanner.status(), DONE);
        assert_eq!(scanner.progress(), 0);
        cleanup(&dir);
    }

    #[test]
    fn scan_skips_dot_dirs() {
        let (scanner, dir) = test_scanner();
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
        let scanner = LocalScanner::new(test_db());
        scanner
            .scan(&[PathBuf::from("/nonexistent_path_xyz")])
            .unwrap();
        assert_eq!(scanner.status(), DONE);
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
}
