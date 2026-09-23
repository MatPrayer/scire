//! Scrobble decision logic — pure and unit-testable.
//!
//! Navidrome forwards scrobbles to ListenBrainz/Last.fm server-side and uses
//! the `submission=true` call to bump play counts. Standard rule: submit once
//! a track has been played to ≥50% of its length OR for ≥4 minutes.

use std::time::Duration;

const SUBMIT_MIN_ELAPSED: Duration = Duration::from_secs(4 * 60);
const SUBMIT_FRACTION: f32 = 0.5;

/// Tracks scrobble progress for the currently playing song.
#[derive(Debug, Default)]
pub struct ScrobbleTracker {
    track: Option<ScrobbleTrack>,
    submitted: bool,
}

/// Metadata carried from queue playback to whichever scrobble backend owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrobbleTrack {
    pub id: String,
    pub local: bool,
    pub artist: Option<String>,
    pub title: String,
    pub album: Option<String>,
    /// Track duration in seconds.
    pub duration: Option<u32>,
}

/// What the caller should do after feeding a position update.
#[derive(Debug, PartialEq, Eq)]
pub enum ScrobbleAction {
    None,
    /// Announce currently playing metadata.
    NowPlaying(ScrobbleTrack),
    /// Submit a completed listen.
    Submit(ScrobbleTrack),
}

impl ScrobbleTracker {
    /// Begin tracking a new track. Returns the now-playing action to fire.
    pub fn start(&mut self, track: ScrobbleTrack) -> ScrobbleAction {
        self.track = Some(track.clone());
        self.submitted = false;
        ScrobbleAction::NowPlaying(track)
    }

    /// Feed the latest playback position; returns a submit action once the
    /// threshold is crossed (only once per track).
    pub fn on_position(
        &mut self,
        position: Duration,
        duration: Option<Duration>,
    ) -> ScrobbleAction {
        if self.submitted {
            return ScrobbleAction::None;
        }
        let Some(track) = self.track.clone() else {
            return ScrobbleAction::None;
        };
        let by_fraction = duration
            .map(|d| {
                d.as_secs_f32() > 0.0 && position.as_secs_f32() / d.as_secs_f32() >= SUBMIT_FRACTION
            })
            .unwrap_or(false);
        let by_time = position >= SUBMIT_MIN_ELAPSED;
        if by_fraction || by_time {
            self.submitted = true;
            ScrobbleAction::Submit(track)
        } else {
            ScrobbleAction::None
        }
    }

    /// Clear on stop / empty queue.
    pub fn clear(&mut self) {
        self.track = None;
        self.submitted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> ScrobbleTrack {
        ScrobbleTrack {
            id: id.into(),
            local: false,
            artist: Some("Artist".into()),
            title: "Title".into(),
            album: Some("Album".into()),
            duration: Some(200),
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn start_emits_now_playing() {
        let mut t = ScrobbleTracker::default();
        assert_eq!(
            t.start(track("s1")),
            ScrobbleAction::NowPlaying(track("s1"))
        );
    }

    #[test]
    fn submits_at_half_for_short_track() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"));
        let dur = Some(secs(200)); // 50% = 100s
        assert_eq!(t.on_position(secs(99), dur), ScrobbleAction::None);
        assert_eq!(
            t.on_position(secs(100), dur),
            ScrobbleAction::Submit(track("s1"))
        );
        // No double-submit.
        assert_eq!(t.on_position(secs(150), dur), ScrobbleAction::None);
    }

    #[test]
    fn submits_at_four_minutes_for_long_track() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"));
        let dur = Some(secs(3600)); // 50% would be 30min; 4min rule wins
        assert_eq!(t.on_position(secs(239), dur), ScrobbleAction::None);
        assert_eq!(
            t.on_position(secs(240), dur),
            ScrobbleAction::Submit(track("s1"))
        );
    }

    #[test]
    fn unknown_duration_still_submits_by_time() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"));
        assert_eq!(t.on_position(secs(100), None), ScrobbleAction::None);
        assert_eq!(
            t.on_position(secs(240), None),
            ScrobbleAction::Submit(track("s1"))
        );
    }

    #[test]
    fn new_track_resets_submission() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"));
        t.on_position(secs(240), None);
        t.start(track("s2"));
        assert_eq!(
            t.on_position(secs(240), None),
            ScrobbleAction::Submit(track("s2"))
        );
    }

    #[test]
    fn no_action_before_start() {
        let mut t = ScrobbleTracker::default();
        assert_eq!(t.on_position(secs(300), None), ScrobbleAction::None);
    }
}
