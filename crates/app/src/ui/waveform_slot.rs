//! Seek-bar peaks for the playing track, as held by the player bar and the
//! fullscreen player.
//!
//! Each view keeps its own slot (the disk cache makes the second one free),
//! but the bookkeeping is shared so they cannot drift apart. They did: a fetch
//! that failed while the network was down left the bar marked as "done" for
//! that song and it never asked again, while the fullscreen player, opened
//! after the network came back, fetched it fine.

use std::time::{Duration, Instant};

use crate::services::waveform::{self, Source};
use crate::state::player::PlayerState;
use crate::state::session::Session;

/// First wait before asking again after a failed fetch; doubles per failure.
const RETRY_FIRST: Duration = Duration::from_secs(5);
/// Longest wait between attempts while a song keeps failing.
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Wait before the next attempt after `failures` failed ones in a row.
fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_FIRST.saturating_mul(1 << doublings).min(RETRY_MAX)
}

#[derive(Default)]
pub struct WaveformSlot {
    /// Peaks for `song`, once its fetch landed.
    pub peaks: Option<Vec<f32>>,
    /// Song the peaks are for, or are being fetched for.
    song: Option<String>,
    in_flight: bool,
    /// Failed attempts for `song` in a row, and when the next may start.
    failures: u32,
    retry_at: Option<Instant>,
}

impl WaveformSlot {
    /// The fetch to start now, if any: the playing track changed, or its last
    /// attempt failed and the backoff has run out. Called from the views'
    /// player and session observers — the player notifies every position
    /// tick, so a retry comes due on its own while something plays.
    pub fn begin(&mut self, player: &PlayerState, session: &Session) -> Option<(String, Source)> {
        let wanted = (session.settings.waveform_seekbar && !player.is_radio())
            .then(|| player.current_song())
            .flatten();
        let Some(song) = wanted else {
            *self = Self::default();
            return None;
        };
        if self.song.as_deref() == Some(song.id.as_str()) {
            let due = self.retry_at.is_some_and(|at| Instant::now() >= at);
            if self.in_flight || self.peaks.is_some() || !due {
                return None;
            }
        } else {
            *self = Self::default();
        }
        let source = match &song.local_path {
            Some(path) => Source::Local(path.into()),
            None => {
                let opts = waveform::stream_options();
                // No client yet (still connecting): nothing is marked, so the
                // next notify asks again.
                let url = session.client.as_ref()?.stream_url(&song.id, &opts).ok()?;
                Source::Remote(url.to_string())
            }
        };
        self.song = Some(song.id.clone());
        self.in_flight = true;
        self.retry_at = None;
        Some((song.id.clone(), source))
    }

    /// Takes a fetch's result. False when it was for a song no longer wanted
    /// (nothing changed, no repaint needed).
    pub fn finish(&mut self, id: &str, result: anyhow::Result<Vec<f32>>) -> bool {
        if self.song.as_deref() != Some(id) {
            return false;
        }
        self.in_flight = false;
        match result {
            Ok(peaks) => {
                self.peaks = Some(peaks);
                self.failures = 0;
            }
            Err(e) => {
                self.failures += 1;
                let wait = retry_delay(self.failures);
                tracing::warn!("waveform peaks failed (retrying in {wait:?}): {e:#}");
                self.retry_at = Some(Instant::now() + wait);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backs_off_and_caps() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(10));
        assert_eq!(retry_delay(4), Duration::from_secs(40));
        assert_eq!(retry_delay(5), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }
}
