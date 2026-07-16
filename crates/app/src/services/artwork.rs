//! Cover-art fetching with an in-memory + size-capped disk cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use subsonic::SubsonicClient;

use crate::config;
use crate::services::runtime;

static CACHE_CAP_BYTES: AtomicU64 = AtomicU64::new(256 * 1024 * 1024);

// Process-wide in-memory index: avoids filesystem round-trips when views are
// recreated (e.g. navigating back to Albums after visiting an artist).
static IN_MEM: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

fn mem_cache() -> &'static Mutex<HashMap<String, PathBuf>> {
    IN_MEM.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Update the on-disk cache cap (megabytes, clamped to 64–1024).
pub fn set_cache_cap_mb(mb: u32) {
    let mb = mb.clamp(64, 1024);
    CACHE_CAP_BYTES.store(u64::from(mb) * 1024 * 1024, Ordering::Relaxed);
}

fn cache_cap_bytes() -> u64 {
    CACHE_CAP_BYTES.load(Ordering::Relaxed)
}

/// Fetch cover art for `cover_id` at `size` px, returning a cached file path.
pub async fn fetch(client: SubsonicClient, cover_id: String, size: u32) -> Result<PathBuf> {
    let cache_key = format!("{cover_id}-{size}");

    // 1. In-memory hit (no FS access).
    if let Some(path) = mem_cache().lock().unwrap().get(&cache_key).cloned() {
        return Ok(path);
    }

    let dir = config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{size}.img", sanitize(&cover_id)));

    // 2. Disk hit: populate in-memory cache and return.
    if path.exists() {
        mem_cache().lock().unwrap().insert(cache_key, path.clone());
        return Ok(path);
    }

    // 3. Network fetch.
    let path2 = path.clone();
    let cache_key2 = cache_key.clone();
    runtime::spawn_io(async move {
        let url = client.cover_art_url(&cover_id, Some(size))?;
        let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
        std::fs::create_dir_all(&dir)?;
        // Write via temp file so partial downloads never poison the cache.
        let tmp = path2.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path2)?;
        evict_if_over_cap(&dir);
        mem_cache()
            .lock()
            .unwrap()
            .insert(cache_key2, path2.clone());
        Ok(path2)
    })
    .await
}

/// If the cache exceeds the cap, delete oldest files (by modified time) until
/// back under it. Best-effort — IO errors are ignored.
fn evict_if_over_cap(dir: &Path) {
    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total += meta.len();
        entries.push((entry.path(), meta.len(), modified));
    }
    if total <= cache_cap_bytes() {
        return;
    }
    // Oldest first.
    entries.sort_by_key(|(_, _, t)| *t);
    for (path, len, _) in entries {
        if total <= cache_cap_bytes() {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
