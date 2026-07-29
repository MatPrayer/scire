//! Waveform peaks for the seek bar: download the track, decode, bucket.
//!
//! Peaks are computed from a low-bitrate transcode (the amplitude envelope
//! survives lossy compression) so the extra download stays small, and cached
//! on disk keyed by song id so repeat plays skip the work entirely.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};

use crate::config;

/// Peak buckets rendered by the player bar's waveform.
pub const BUCKETS: usize = 480;

/// Stream options for the peak download: a low-bitrate transcode keeps the
/// extra download small, and the amplitude envelope survives it fine.
pub fn stream_options() -> subsonic::StreamOptions {
    subsonic::StreamOptions {
        format: Some("mp3".into()),
        max_bit_rate: Some(96),
    }
}

// One lock per song id, so a prewarm already downloading a track and the
// player bar asking for the same peaks don't both download and decode it —
// the second waits, then reads what the first cached.
static IN_FLIGHT: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

fn song_lock(song_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    IN_FLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(song_id.to_string())
        .or_default()
        .clone()
}

/// Drop the lock entry once nobody else holds it, so the map does not grow
/// with every song played.
fn release_lock(song_id: &str, lock: Arc<tokio::sync::Mutex<()>>) {
    let mut map = IN_FLIGHT.get_or_init(Default::default).lock().unwrap();
    // 2 = the map's copy + ours; anything more means another task is waiting.
    if Arc::strong_count(&lock) <= 2 {
        map.remove(song_id);
    }
}

fn cache_path(song_id: &str) -> Option<PathBuf> {
    Some(
        config::waveform_cache_dir()
            .ok()?
            .join(format!("{}.v3.json", config::sanitize(song_id))),
    )
}

/// Peaks already on disk for `song_id`, if any.
fn cached_peaks(path: Option<&PathBuf>) -> Option<Vec<f32>> {
    let path = path?;
    let text = fs::read_to_string(path).ok()?;
    let peaks = serde_json::from_str::<Vec<f32>>(&text).ok()?;
    (peaks.len() == BUCKETS).then_some(peaks)
}

/// Download `url` fully and reduce it to [`BUCKETS`] normalized peaks,
/// reading/writing the on-disk cache under `song_id`.
/// Must run inside the tokio runtime (`runtime::spawn_io`).
pub async fn fetch_peaks(url: String, song_id: String) -> Result<Vec<f32>> {
    let path = cache_path(&song_id);
    if let Some(peaks) = cached_peaks(path.as_ref()) {
        return Ok(peaks);
    }

    let lock = song_lock(&song_id);
    let guard = lock.clone().lock_owned().await;
    // A prewarm may have finished this track while we waited on the lock.
    if let Some(peaks) = cached_peaks(path.as_ref()) {
        drop(guard);
        release_lock(&song_id, lock);
        return Ok(peaks);
    }

    let result = compute_peaks(&url, path).await;
    drop(guard);
    release_lock(&song_id, lock);
    result
}

async fn compute_peaks(url: &str, path: Option<PathBuf>) -> Result<Vec<f32>> {
    let bytes = reqwest::get(url)
        .await
        .context("waveform download")?
        .error_for_status()
        .context("waveform download")?
        .bytes()
        .await
        .context("waveform download")?
        .to_vec();
    // Decoding a whole track is CPU-heavy; keep it off the async workers.
    let peaks =
        tokio::task::spawn_blocking(move || playback::waveform::peaks_from_bytes(bytes, BUCKETS))
            .await
            .context("waveform decode task")?
            .context("waveform decode")?;

    if let Some(path) = path
        && let Ok(json) = serde_json::to_string(&peaks)
    {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(path, json);
    }
    Ok(peaks)
}
