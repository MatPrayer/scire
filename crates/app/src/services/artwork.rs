//! Cover-art fetching with an in-memory + size-capped disk cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use subsonic::{Song, SubsonicClient};

use crate::config;
use crate::services::runtime;

static CACHE_CAP_BYTES: AtomicU64 = AtomicU64::new(256 * 1024 * 1024);

// Process-wide in-memory index: avoids filesystem round-trips when views are
// recreated (e.g. navigating back to Albums after visiting an artist).
//
// `None` is a remembered *miss*. `cached` is called from `render`, once per
// visible card, so a grid of art that has not been downloaded yet used to
// stat the disk for every card on every frame — and a scroll or a resize is
// many frames. Recording the absence costs one entry and answers the repeats
// without a syscall; a later `fetch_as` overwrites it with the real path when
// the download lands.
static IN_MEM: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();

fn mem_cache() -> &'static Mutex<HashMap<String, Option<PathBuf>>> {
    IN_MEM.get_or_init(|| Mutex::new(HashMap::new()))
}

// Shared client so cover fetches reuse pooled keep-alive connections instead
// of doing a fresh TLS handshake per thumbnail (`reqwest::get` builds a new
// client every call).
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(reqwest::Client::new)
}

// Cap concurrent cover downloads. Fast-scrolling a large grid would otherwise
// spawn hundreds of simultaneous requests + image decodes, saturating the IO
// runtime and stalling the UI. Excess fetches park cheaply on the semaphore.
const MAX_CONCURRENT_FETCHES: usize = 8;
static FETCH_SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();

fn fetch_sem() -> &'static tokio::sync::Semaphore {
    FETCH_SEM.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_FETCHES))
}

/// Update the on-disk cache cap (megabytes, clamped to 64–1024).
pub fn set_cache_cap_mb(mb: u32) {
    let mb = mb.clamp(64, 1024);
    CACHE_CAP_BYTES.store(u64::from(mb) * 1024 * 1024, Ordering::Relaxed);
}

fn cache_cap_bytes() -> u64 {
    CACHE_CAP_BYTES.load(Ordering::Relaxed)
}

/// Cover identity for a song: the id to request from the server, and the key it
/// is cached under. Navidrome mints a distinct cover id per file (`mf-<song>`),
/// so keying the cache on it re-downloads identical album art for every track —
/// group by album instead, and fetch whichever track's id we saw first.
///
/// The key drops the server's cache-busting suffix, so art replaced on the
/// server keeps serving from cache here until the entry is evicted.
pub fn song_cover(song: &Song) -> Option<(String, String)> {
    let cover_id = song.cover_art.clone()?;
    let key = song
        .album_id
        .as_ref()
        .map_or_else(|| cover_id.clone(), |album| album_cover_key(album));
    Some((cover_id, key))
}

/// The cache key [`song_cover`] groups an album's covers under. Exposed so a
/// view holding an album id — rather than one of its songs — can address the
/// very same cache entry, which is what keeps the adaptive accent identical
/// between the player bar and the album page.
pub fn album_cover_key(album_id: &str) -> String {
    format!("album-{album_id}")
}

/// Sizes art is actually stored at, smallest first.
///
/// Callers ask for whatever their layout wants — the grid alone asks for four
/// different widths as the cover-size setting moves, and seven more are spread
/// across the other views — and every distinct number was its own download and
/// its own cache entry of the same picture. Snapping each request up to the
/// next rung collapses those into a handful: the four grid sizes become two,
/// and the thumbnail-ish askers (recent, the player bar, the artist grid)
/// share with the grid instead of each keeping a private copy. Requests are
/// rounded *up*, never down, so nothing is ever drawn from art thinner than it
/// asked for; the extra pixels cost a little bandwidth once and are scaled down
/// at draw time, which every one of these callers already relies on.
const SIZE_LADDER: [u32; 5] = [64, 256, 512, 640, 1500];

/// Snap a requested edge length up to the nearest stored size.
fn bucket(size: u32) -> u32 {
    SIZE_LADDER
        .into_iter()
        .find(|&rung| rung >= size)
        .unwrap_or(SIZE_LADDER[SIZE_LADDER.len() - 1])
}

/// Drop the cache-busting suffix Navidrome appends to a cover id
/// (`al-<album>_<hash>`, `ar-<artist>_<hash>`, `mf-<song>_<hash>`).
///
/// The hash moves whenever the server touches the album, so keying the cache
/// on the id as given re-downloads art that has not changed — measured on a
/// real library, 42% of albums were held under two or more hashes at once.
/// [`song_cover`] already sidesteps this for songs by keying on the album;
/// normalizing here covers the album, artist and detail views, which fetch by
/// cover id directly.
///
/// The trade is the one `song_cover` already documents: art genuinely replaced
/// on the server keeps serving from cache until the entry is evicted. Only a
/// trailing `_` followed by hex is removed, so an `album-<id>` key — or any id
/// without that shape — passes through untouched.
fn stable_key(key: &str) -> &str {
    match key.rsplit_once('_') {
        Some((head, suffix))
            if !head.is_empty()
                && !suffix.is_empty()
                && suffix.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            head
        }
        _ => key,
    }
}

/// Synchronous cache lookup (in-memory index, then disk) by cache key — the
/// cover id itself, or the album-scoped key from [`song_cover`]. Never touches
/// the network. Returns the cached file path if the art was already downloaded,
/// so callers can render it on the first frame instead of waiting on a task.
///
/// The key is normalized and the size bucketed here rather than at the call
/// sites, so a lookup and the fetch that follows it cannot disagree about
/// which entry they mean.
pub fn cached(key: &str, size: u32) -> Option<PathBuf> {
    let key = stable_key(key);
    let size = bucket(size);
    let cache_key = format!("{key}-{size}");
    // A recorded entry answers both ways without touching the disk: a path
    // that is there, or a miss that was already looked for.
    if let Some(entry) = mem_cache().lock().unwrap().get(&cache_key) {
        return entry.clone();
    }
    let dir = config::artwork_cache_dir().ok()?;
    let path = dir.join(format!("{}-{size}.img", config::sanitize(key)));
    let found = path.exists().then_some(path);
    mem_cache().lock().unwrap().insert(cache_key, found.clone());
    found
}

/// The best rendition of `key` already on disk, whatever size it is.
///
/// [`cached`] answers about one exact rung, which is what the fetch path needs
/// but the wrong question for a view that just wants to show *something* now.
/// A detail page opened from the grid is the case that matters: the grid holds
/// a 256 of that same cover, the page wants a bigger one, and asking only for
/// the bigger one means an empty frame and a network round trip for art that is
/// already sitting in the cache. Painting the smaller file immediately and
/// letting [`fetch`] replace it a moment later is the whole difference between
/// a page that appears and a page that loads.
///
/// Preference order: the requested rung, then the largest smaller one (closest
/// to the requested detail), then the smallest larger one. Callers should still
/// start the fetch for the size they actually want — this only decides what to
/// draw meanwhile.
pub fn cached_best(key: &str, want: u32) -> Option<PathBuf> {
    search_order(want)
        .into_iter()
        .find_map(|rung| cached(key, rung))
}

/// Rungs to try for [`cached_best`], best first.
fn search_order(want: u32) -> Vec<u32> {
    let want = bucket(want);
    std::iter::once(want)
        // Largest smaller rung first: closest to the detail asked for.
        .chain(SIZE_LADDER.into_iter().rev().filter(|&rung| rung < want))
        // Only then oversized art, smallest first — it looks right but costs
        // the most to hold and scale.
        .chain(SIZE_LADDER.into_iter().filter(|&rung| rung > want))
        .collect()
}

/// Fetch cover art for `cover_id` at `size` px, returning a cached file path.
pub async fn fetch(client: SubsonicClient, cover_id: String, size: u32) -> Result<PathBuf> {
    let key = cover_id.clone();
    fetch_as(client, cover_id, key, size).await
}

/// Like [`fetch`], but stores the result under `key` so several cover ids that
/// resolve to the same image (all tracks of an album) share one cache entry and
/// one download. Pair it with [`song_cover`].
pub async fn fetch_as(
    client: SubsonicClient,
    cover_id: String,
    key: String,
    size: u32,
) -> Result<PathBuf> {
    // 1 + 2. In-memory / disk hit (no network).
    if let Some(path) = cached(&key, size) {
        return Ok(path);
    }

    // Same normalization `cached` just applied, so the entry written below is
    // the one the next lookup goes looking for. `size` is bucketed before the
    // request too — there is no point downloading a width we will not store.
    let key = stable_key(&key).to_string();
    let size = bucket(size);
    let cache_key = format!("{key}-{size}");
    let dir = config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{size}.img", config::sanitize(&key)));

    // 3. Network fetch.
    let path2 = path.clone();
    let cache_key2 = cache_key.clone();
    runtime::spawn_io(async move {
        // Throttle: hold a permit for the download so at most
        // MAX_CONCURRENT_FETCHES run at once. Dropped (cancelled) fetches
        // release their permit/wait immediately.
        let _permit = fetch_sem().acquire().await?;
        // A cover may have been cached by a concurrent request while we waited.
        if let Some(path) = cached(&key, size) {
            return Ok(path);
        }
        let url = client.cover_art_url(&cover_id, Some(size))?;
        let bytes = http()
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        // Square the art before it lands in the cache. Decoding a cover is
        // CPU work and the IO runtime has two workers, so it goes to the
        // blocking pool rather than parking one of them behind a JPEG.
        let bytes = tokio::task::spawn_blocking(move || {
            square_crop(&bytes).unwrap_or_else(|| bytes.to_vec())
        })
        .await?;
        std::fs::create_dir_all(&dir)?;
        // Write via temp file so partial downloads never poison the cache.
        let tmp = path2.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path2)?;
        evict_if_over_cap(&dir);
        // Replaces the remembered miss `cached` left behind for this key.
        mem_cache()
            .lock()
            .unwrap()
            .insert(cache_key2, Some(path2.clone()));
        Ok(path2)
    })
    .await
}

/// Center-crop art that is not square, so a cover fills the square tile it is
/// drawn in instead of being letterboxed.
///
/// Cropping the *file* rather than the draw is what keeps the rounded corners
/// every view gives its art: gpui's `ObjectFit::Cover` paints the image
/// outside the element's bounds, the corner radii are applied to that
/// oversized quad, and the only clip gpui offers is a rectangular content
/// mask — so the rounding would land off-tile and be cut away. A square file
/// needs no fit at all and is square in every view that draws it.
///
/// Returns `None` when the image is already square, or cannot be read, in
/// which case the caller stores the bytes exactly as they arrived. The
/// dimensions come from the header, so the common case — square art — never
/// pays for a decode.
///
/// CPU-bound when it does crop: call it from a blocking context.
pub fn square_crop(bytes: &[u8]) -> Option<Vec<u8>> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format();
    let (width, height) = reader.into_dimensions().ok()?;
    if width == height {
        return None;
    }
    let edge = width.min(height);
    let cropped = image::load_from_memory(bytes).ok()?.crop_imm(
        (width - edge) / 2,
        (height - edge) / 2,
        edge,
        edge,
    );
    let mut out = Vec::new();
    // PNG art keeps its format (transparency, flat-colour covers); everything
    // else is re-encoded as JPEG, which is what it almost always already was.
    if format == Some(image::ImageFormat::Png) {
        cropped
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .ok()?;
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&cropped.to_rgb8())
            .ok()?;
    }
    Some(out)
}

/// Marker written into a cache directory once its art has been squared.
const SQUARED_MARKER: &str = ".squared-v1";

/// One-off pass over the art already on disk, cropping what was cached before
/// covers were squared on the way in.
///
/// Both caches are keyed by cover id and size rather than by content, so a
/// name bump is not an option: the server cache would re-download the whole
/// library's art, and the local one would simply lose its covers, since the
/// scanner re-extracts art only for files whose mtime moved. Cropping what is
/// already there costs a header read per file and nothing over the network.
///
/// Best-effort and idempotent — a file that fails to read, decode or write is
/// left as it is, and an already-square one is skipped. CPU-bound: call from a
/// blocking context.
pub fn squarify_cached_art() {
    for dir in [
        config::artwork_cache_dir().ok(),
        crate::services::local_library::local_art_dir(),
    ]
    .into_iter()
    .flatten()
    {
        squarify_dir(&dir);
    }
}

fn squarify_dir(dir: &Path) {
    let marker = dir.join(SQUARED_MARKER);
    if marker.exists() {
        return;
    }
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            // Skip the marker and the temp files a cancelled download leaves.
            if path == marker || path.extension().is_some_and(|ext| ext == "part") {
                continue;
            }
            if !entry.metadata().is_ok_and(|meta| meta.is_file()) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Some(square) = square_crop(&bytes) else {
                continue;
            };
            let tmp = path.with_extension("part");
            if std::fs::write(&tmp, &square).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&marker, b"");
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
    let mut dropped: Vec<PathBuf> = Vec::new();
    for (path, len, _) in entries {
        if total <= cache_cap_bytes() {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            dropped.push(path);
        }
    }
    // The in-memory index still points at the files just deleted, and a
    // remembered path that no longer exists renders as a broken cover rather
    // than a missing one — which is worse, since nothing goes on to re-fetch
    // it. Forget them so the next `cached` misses and the fetch runs again.
    if !dropped.is_empty() {
        let mut mem = mem_cache().lock().unwrap();
        mem.retain(|_, entry| entry.as_ref().is_none_or(|p| !dropped.contains(p)));
    }
}

#[cfg(test)]
mod tests {
    use super::{SIZE_LADDER, bucket, search_order, square_crop, stable_key};

    fn encode(width: u32, height: u32) -> Vec<u8> {
        let img =
            image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 256) as u8, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn dimensions(bytes: &[u8]) -> (u32, u32) {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    }

    #[test]
    fn art_is_cropped_to_a_square_before_it_is_cached() {
        // Wide and tall art both come back at their short edge, so the square
        // tile every view draws it in is filled rather than letterboxed.
        let wide = square_crop(&encode(40, 20)).expect("wide art cropped");
        assert_eq!(dimensions(&wide), (20, 20));
        let tall = square_crop(&encode(20, 50)).expect("tall art cropped");
        assert_eq!(dimensions(&tall), (20, 20));
        // Already square: the caller stores the original bytes untouched, so
        // nothing is re-encoded for the covers that are the common case.
        assert!(square_crop(&encode(300, 300)).is_none());
        // Undecodable input is left alone rather than dropped.
        assert!(square_crop(b"not an image").is_none());
    }

    #[test]
    fn a_view_falls_back_to_the_nearest_cached_size() {
        // The album page wants 512 and the grid cached 256: 256 is tried right
        // after the requested size, so the header paints from the grid's own
        // download instead of waiting on the network.
        assert_eq!(search_order(512), vec![512, 256, 64, 640, 1500]);
        // Requests are bucketed first, so the old 600 and the new 512 agree.
        assert_eq!(search_order(600), search_order(640));
        // Smaller before larger throughout: closest detail wins, and oversized
        // art is the last resort rather than the first.
        assert_eq!(search_order(1500), vec![1500, 640, 512, 256, 64]);
        assert_eq!(search_order(64), vec![64, 256, 512, 640, 1500]);
        // Every rung is offered exactly once, whatever was asked for.
        for want in [1, 64, 256, 300, 512, 640, 4000] {
            let order = search_order(want);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                SIZE_LADDER.len(),
                "want={want} dropped a rung"
            );
        }
    }

    #[test]
    fn requests_snap_up_to_a_stored_size() {
        // The four grid widths (cover size × 1.5) collapse onto two rungs, so
        // changing the setting between Small and Medium re-uses what is
        // already on disk instead of re-downloading the library.
        assert_eq!(bucket(180), 256); // Small
        assert_eq!(bucket(240), 256); // Medium
        assert_eq!(bucket(300), 512); // Large
        assert_eq!(bucket(390), 512); // ExtraLarge
        // The other views land on the same rungs rather than each keeping a
        // private copy of the same picture.
        assert_eq!(bucket(200), 256); // recent
        assert_eq!(bucket(320), 512); // artist grid
        assert_eq!(bucket(32), 64); // fullscreen background
        assert_eq!(bucket(64), 64); // search
        // Never down: a rung is met exactly or exceeded.
        for size in [1, 63, 64, 65, 255, 256, 511, 640, 1499, 1500] {
            assert!(bucket(size) >= size, "{size} snapped below itself");
        }
        // Anything past the top rung is capped there — that is full art.
        assert_eq!(bucket(4000), SIZE_LADDER[SIZE_LADDER.len() - 1]);
    }

    #[test]
    fn cover_keys_drop_the_servers_cache_busting_suffix() {
        // The same album under two different hashes is one cache entry.
        assert_eq!(
            stable_key("al-76iTU12jdqoi5pFm0EldqG_69d2a3dc"),
            "al-76iTU12jdqoi5pFm0EldqG"
        );
        assert_eq!(
            stable_key("al-76iTU12jdqoi5pFm0EldqG_6a85e84b"),
            "al-76iTU12jdqoi5pFm0EldqG"
        );
        assert_eq!(
            stable_key("ar-3H5XrY0l644Oeq39sbM9Wd_1f2e"),
            "ar-3H5XrY0l644Oeq39sbM9Wd"
        );
        // Keys without that shape are left exactly as they are — the
        // album-scoped key from `song_cover` above all.
        assert_eq!(
            stable_key("album-76iTU12jdqoi5pFm0EldqG"),
            "album-76iTU12jdqoi5pFm0EldqG"
        );
        assert_eq!(stable_key("al-plain"), "al-plain");
        // A non-hex suffix is part of the id, not a cache-buster.
        assert_eq!(stable_key("al-abc_zzzz"), "al-abc_zzzz");
        // Degenerate halves are not suffixes either.
        assert_eq!(stable_key("_abc"), "_abc");
        assert_eq!(stable_key("al-abc_"), "al-abc_");
    }
}
