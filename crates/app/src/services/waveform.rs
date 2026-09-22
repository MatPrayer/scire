//! Waveform peaks for the seek bar: load the track, decode, bucket.
//!
//! Remote peaks are computed from a low-bitrate transcode (the amplitude
//! envelope survives lossy compression) so the extra download stays small.
//! Local peaks are read straight from the music file. Both are cached on disk
//! keyed by song id so repeat plays skip the work entirely.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result};

use crate::config;

/// Peak buckets rendered by the player bar's waveform.
pub const BUCKETS: usize = 480;

/// Audio source used to compute waveform peaks.
pub enum Source {
    Remote(String),
    Local(PathBuf),
}

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

/// Load `source` fully and reduce it to [`BUCKETS`] normalized peaks,
/// reading/writing the on-disk cache under `song_id`.
/// Must run inside the tokio runtime (`runtime::spawn_io`).
pub async fn fetch_peaks(source: Source, song_id: String) -> Result<Vec<f32>> {
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

    let result = match source {
        Source::Remote(url) => compute_remote_peaks(&url, path).await,
        Source::Local(file) => compute_local_peaks(file, path).await,
    };
    drop(guard);
    release_lock(&song_id, lock);
    result
}

async fn compute_remote_peaks(url: &str, path: Option<PathBuf>) -> Result<Vec<f32>> {
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

    write_cached_peaks(path, &peaks);
    Ok(peaks)
}

async fn compute_local_peaks(file: PathBuf, path: Option<PathBuf>) -> Result<Vec<f32>> {
    let peaks = tokio::task::spawn_blocking(move || {
        let bytes = fs::read(file).context("waveform file read")?;
        playback::waveform::peaks_from_bytes(bytes, BUCKETS).context("waveform decode")
    })
    .await
    .context("waveform decode task")??;

    write_cached_peaks(path, &peaks);
    Ok(peaks)
}

fn write_cached_peaks(path: Option<PathBuf>, peaks: &[f32]) {
    let Some(path) = path else { return };
    let Ok(json) = serde_json::to_string(peaks) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, json);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wav() -> Vec<u8> {
        let samples = [0_i16; 800];
        let data_len = (samples.len() * std::mem::size_of::<i16>()) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[tokio::test]
    async fn local_file_decodes_to_waveform_buckets() {
        let file = std::env::temp_dir().join(format!(
            "scire-waveform-local-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(&file, test_wav()).unwrap();

        let peaks = compute_local_peaks(file.clone(), None).await.unwrap();

        assert_eq!(peaks.len(), BUCKETS);
        let _ = fs::remove_file(file);
    }
}
