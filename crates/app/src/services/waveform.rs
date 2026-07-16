//! Waveform peaks for the seek bar: download the track, decode, bucket.
//!
//! Peaks are computed from a low-bitrate transcode (the amplitude envelope
//! survives lossy compression) so the extra download stays small, and cached
//! on disk keyed by song id so repeat plays skip the work entirely.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::config;

/// Peak buckets rendered by the player bar's waveform.
pub const BUCKETS: usize = 480;

fn cache_path(song_id: &str) -> Option<PathBuf> {
    let safe: String = song_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // v3: 480 buckets for a continuous filled waveform.
    Some(
        config::waveform_cache_dir()
            .ok()?
            .join(format!("{safe}.v3.json")),
    )
}

/// Download `url` fully and reduce it to [`BUCKETS`] normalized peaks,
/// reading/writing the on-disk cache under `song_id`.
/// Must run inside the tokio runtime (`runtime::spawn_io`).
pub async fn fetch_peaks(url: String, song_id: String) -> Result<Vec<f32>> {
    let path = cache_path(&song_id);
    if let Some(path) = &path
        && let Ok(text) = fs::read_to_string(path)
        && let Ok(peaks) = serde_json::from_str::<Vec<f32>>(&text)
        && peaks.len() == BUCKETS
    {
        return Ok(peaks);
    }

    let bytes = reqwest::get(&url)
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
