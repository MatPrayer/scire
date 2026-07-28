//! Playback engine: rodio + stream-download behind a command/event facade.
//!
//! The facade hides rodio entirely so the engine could be swapped for a
//! symphonia+cpal implementation without touching consumers.
//!
//! Must be constructed inside a tokio runtime (stream-download needs the
//! reactor; the control loop is a tokio task).

mod engine;
mod source;
pub mod waveform;

use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;

/// What to play: a fully-authenticated stream URL.
#[derive(Debug, Clone)]
pub struct TrackSource {
    pub url: String,
    /// Duration hint from server metadata; used until decoding knows better.
    pub duration_hint: Option<Duration>,
}

/// Commands accepted by the engine.
#[derive(Debug)]
pub enum Command {
    Play(TrackSource),
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    /// Pre-open and pre-decode the next track for near-gapless transition.
    /// The engine auto-starts it when the current track drains.
    PrefetchNext(TrackSource),
    /// Drop any prefetched track (queue changed).
    ClearPrefetch,
    /// Switch the OS output device by its description name; None = system
    /// default. Reopens the sink and resumes the current track in place.
    SetOutputDevice(Option<String>),
}

/// Enumerate available output device names (cpal descriptions), de-duplicated.
/// Best-effort: returns an empty list when the host cannot be queried. Skips
/// the "null" driver so dummy devices don't appear in the picker.
pub fn output_devices() -> Vec<String> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    let mut names = Vec::new();
    if let Ok(devices) = rodio::cpal::default_host().output_devices() {
        for dev in devices {
            let Ok(desc) = dev.description() else {
                continue;
            };
            if desc.driver().is_some_and(|d| d == "null") {
                continue;
            }
            let name = desc.name().to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Events emitted by the engine.
#[derive(Debug, Clone)]
pub enum Event {
    /// Periodic position update (~500ms) while playing.
    Position(Duration),
    /// Total duration became known (decode or hint).
    DurationKnown(Duration),
    /// Track finished on its own (not via Stop). When `auto_advanced` the
    /// engine already started the prefetched next track — the consumer should
    /// advance its queue pointer without sending Play.
    TrackEnded { auto_advanced: bool },
    /// Source is being fetched/buffered.
    Buffering,
    /// Playback started/resumed.
    Playing,
    /// Playback paused.
    Paused,
    /// Unrecoverable failure for the current track.
    Failed(String),
    /// Audio output was opened; reports the OS output device name.
    OutputOpened { device: Option<String> },
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct PlaybackError(pub String);

/// Handle to the playback engine. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Player {
    tx: mpsc::UnboundedSender<Command>,
}

impl Player {
    /// Spawn the engine control loop; returns the handle and the event stream.
    ///
    /// Must be called from within a tokio runtime.
    pub fn new() -> (Player, mpsc::UnboundedReceiver<Event>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        engine::spawn(cmd_rx, event_tx);
        (Player { tx: cmd_tx }, event_rx)
    }

    pub fn play(&self, source: TrackSource) {
        let _ = self.tx.send(Command::Play(source));
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    pub fn resume(&self) {
        let _ = self.tx.send(Command::Resume);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn seek(&self, position: Duration) {
        let _ = self.tx.send(Command::Seek(position));
    }

    /// Volume in [0.0, 1.0] (clamped by the engine).
    pub fn set_volume(&self, volume: f32) {
        let _ = self.tx.send(Command::SetVolume(volume));
    }

    /// Prepare `source` to start seamlessly after the current track.
    pub fn prefetch_next(&self, source: TrackSource) {
        let _ = self.tx.send(Command::PrefetchNext(source));
    }

    /// Drop any prepared next track (call when the queue changes).
    pub fn clear_prefetch(&self) {
        let _ = self.tx.send(Command::ClearPrefetch);
    }

    /// Switch output device by name (None = system default).
    pub fn set_output_device(&self, name: Option<String>) {
        let _ = self.tx.send(Command::SetOutputDevice(name));
    }
}
