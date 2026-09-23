//! Scrobble decision logic — pure and unit-testable.
//!
//! Navidrome forwards scrobbles to ListenBrainz/Last.fm server-side and uses
//! the `submission=true` call to bump play counts. Standard rule: submit once
//! a track has been played to ≥50% of its length OR for ≥4 minutes.

use std::time::{Duration, SystemTime};

const SUBMIT_MIN_ELAPSED: Duration = Duration::from_secs(4 * 60);
const SUBMIT_FRACTION: f32 = 0.5;

/// Tracks scrobble progress for the currently playing song.
#[derive(Debug, Default)]
pub struct ScrobbleTracker {
    track: Option<ScrobbleTrack>,
    submitted: bool,
    /// When the current track started playing. A listen is stamped with this
    /// rather than with the moment the threshold is crossed — ListenBrainz
    /// defines `listened_at` as the start of the play, and submitting at the
    /// 50% mark puts every listen half a track late, which reorders a
    /// listening history against anything else submitting correctly.
    started: Option<SystemTime>,
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
    /// `at` is the wall clock the play began at, kept for the submission's
    /// `listened_at`; it is passed in rather than read here so the whole
    /// module stays pure.
    pub fn start(&mut self, track: ScrobbleTrack, at: SystemTime) -> ScrobbleAction {
        self.track = Some(track.clone());
        self.submitted = false;
        self.started = Some(at);
        ScrobbleAction::NowPlaying(track)
    }

    /// When the track being tracked started playing.
    pub fn started_at(&self) -> Option<SystemTime> {
        self.started
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
        self.started = None;
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

    fn at(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + secs(s)
    }

    #[test]
    fn start_emits_now_playing() {
        let mut t = ScrobbleTracker::default();
        assert_eq!(
            t.start(track("s1"), at(1_000)),
            ScrobbleAction::NowPlaying(track("s1"))
        );
    }

    #[test]
    fn submits_at_half_for_short_track() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"), at(1_000));
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
        t.start(track("s1"), at(1_000));
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
        t.start(track("s1"), at(1_000));
        assert_eq!(t.on_position(secs(100), None), ScrobbleAction::None);
        assert_eq!(
            t.on_position(secs(240), None),
            ScrobbleAction::Submit(track("s1"))
        );
    }

    #[test]
    fn new_track_resets_submission() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"), at(1_000));
        t.on_position(secs(240), None);
        t.start(track("s2"), at(1_000));
        assert_eq!(
            t.on_position(secs(240), None),
            ScrobbleAction::Submit(track("s2"))
        );
    }

    #[test]
    fn the_listen_is_stamped_at_the_start_not_the_submission() {
        let mut t = ScrobbleTracker::default();
        t.start(track("s1"), at(1_000));
        // The threshold is crossed 100s in; the stamp stays at the start.
        assert_eq!(
            t.on_position(secs(100), Some(secs(200))),
            ScrobbleAction::Submit(track("s1"))
        );
        assert_eq!(t.started_at(), Some(at(1_000)));
        // A new track takes its own start, and stopping drops it.
        t.start(track("s2"), at(1_300));
        assert_eq!(t.started_at(), Some(at(1_300)));
        t.clear();
        assert_eq!(t.started_at(), None);
    }

    #[test]
    fn no_action_before_start() {
        let mut t = ScrobbleTracker::default();
        assert_eq!(t.on_position(secs(300), None), ScrobbleAction::None);
    }
}
