//! Playback engine: rodio + stream-download behind a command/event facade.
//!
//! The facade hides rodio entirely so the engine could be swapped for a
//! symphonia+cpal implementation without touching consumers.
//!
//! Must be constructed inside a tokio runtime (stream-download needs the
//! reactor; the control loop is a tokio task).

mod direct;
mod engine;
pub mod icy;
#[cfg(target_os = "linux")]
mod pulse;
mod source;
pub mod spectrum;
pub mod waveform;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;

/// Ceiling on the volume the engine accepts. Above 1.0 because the caller
/// folds ReplayGain into it and a positive track gain asks for amplification;
/// capped so a bogus tag cannot ask for 40 dB of it.
pub const MAX_VOLUME: f32 = 4.0;

/// What to play: a fully-authenticated stream URL, or a local file path.
/// When `path` is `Some`, the engine reads from the local file instead of
/// fetching the URL (the URL is still set for display/metadata purposes).
#[derive(Debug, Clone)]
pub struct TrackSource {
    pub url: String,
    /// Duration hint from server metadata; used until decoding knows better.
    pub duration_hint: Option<Duration>,
    /// Local file path. `Some` → use local file IO instead of HTTP stream.
    pub path: Option<PathBuf>,
    /// Consumer-side identity (song id), echoed back in `TrackEnded::started`
    /// so a gapless hand-over can be matched to the right queue entry.
    pub id: Option<String>,
    /// This is a live stream (internet radio), not a library file. Only these
    /// ask the server for ICY metadata: the header is a request to interleave
    /// title blocks into the audio, and a server that honours it on a library
    /// file answers without a `Content-Length`, which costs the decoder the
    /// ability to seek — fatal for an m4a whose index sits at the end.
    pub live: bool,
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
    /// Pre-open and pre-decode the next track for a gapless transition. The
    /// engine appends it to the live player shortly before the current track
    /// ends, so playback flows into it without a break.
    PrefetchNext(TrackSource),
    /// Drop any prefetched track (queue changed). Ignored once the track has
    /// been appended — rodio's queue cannot give it back.
    ClearPrefetch,
    /// Switch the OS output device by its description name; None = system
    /// default. Reopens the sink and resumes the current track in place.
    SetOutputDevice(Option<String>),
    /// Open a card directly, bypassing the sound server (see
    /// [`direct_devices`]); None = back to the shared output. Overrides
    /// `SetOutputDevice` while set. Volume is fixed at unity while a card is
    /// open this way, and the output reopens at each track's sample rate.
    SetDirectOutput(Option<String>),
}

pub use direct::DirectDevice;

/// The cards that can be opened directly — ALSA `hw:` devices on Linux,
/// nothing elsewhere. Opens nothing, but asks ALSA; call off the UI thread.
pub fn direct_devices() -> Vec<DirectDevice> {
    direct::devices()
}

/// Enumerate the output devices a user could pick, de-duplicated. Best-effort:
/// returns an empty list when nothing can be queried.
///
/// On Linux these are PulseAudio/PipeWire sink descriptions — the same outputs
/// the rest of the desktop offers, Bluetooth included. cpal's ALSA hints are
/// the fallback for a machine without `pactl`, and they are a poor list: sound
/// servers, resampler plugins and capture-only devices all appear in it, and no
/// PipeWire sink does. Elsewhere (macOS) cpal is the only list and a good one.
pub fn output_devices() -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Some(sinks) = pulse::descriptions() {
        return sinks;
    }
    cpal_output_devices()
}

/// Output device names as cpal describes them, skipping the "null" driver so
/// dummy devices don't appear in the picker.
fn cpal_output_devices() -> Vec<String> {
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

/// Whether audio can be played at all: can an output stream be opened right
/// now? Opens the default sink and drops it again, which is the only honest
/// answer — a headless machine still enumerates ALSA devices (`output_devices`
/// is non-empty on a CI runner with no sound card), and opening is where that
/// turns into a failure.
///
/// This exists for the engine tests, which stream real audio and must skip
/// rather than fail where there is nothing to play it on. Matching the
/// engine's error text instead is matching whatever rodio's backend happened
/// to say — on a Linux runner, "Failed to get the config for the given
/// device", which names neither audio nor a device being absent.
pub fn output_available() -> bool {
    rodio::DeviceSinkBuilder::open_default_sink().is_ok()
}

/// Events emitted by the engine.
#[derive(Debug, Clone)]
pub enum Event {
    /// Periodic position update (~500ms) while playing.
    Position(Duration),
    /// Total duration became known (decode or hint).
    DurationKnown(Duration),
    /// Track finished on its own (not via Stop). When `auto_advanced` the
    /// engine already flowed gaplessly into the prefetched track — the consumer
    /// should advance its queue pointer without sending Play, and `started`
    /// carries that track's `TrackSource::id` (it was committed seconds
    /// earlier, so a queue edit since then may make it differ from what the
    /// consumer expects next).
    TrackEnded {
        auto_advanced: bool,
        started: Option<String>,
    },
    /// Source is being fetched/buffered.
    Buffering,
    /// Playback started/resumed.
    Playing,
    /// Playback paused.
    Paused,
    /// Unrecoverable failure for the current track.
    Failed(String),
    /// The track prepared to play *after* the current one could not be opened.
    /// Playback of the current track is unaffected; the gapless hand-over will
    /// not happen and starting that track will fail the same way.
    PrefetchFailed { id: Option<String>, error: String },
    /// Audio output was opened; reports the OS output device name. `direct`
    /// is the format a directly-opened card runs at (`44.1 kHz · 32-bit`);
    /// `direct_error` says why a direct card was asked for and not opened.
    OutputOpened {
        device: Option<String>,
        direct: Option<String>,
        direct_error: Option<String>,
    },
    /// The source that just started is a live stream, and this is what it says
    /// about itself (ICY response headers).
    StationInfo(icy::StationInfo),
    /// Now-playing title advertised by a live stream. Arrives whenever the
    /// station announces a new one, so it can change many times within a
    /// single "track" as far as the rest of the app is concerned.
    StreamTitle(Option<String>),
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct PlaybackError(pub String);

impl PlaybackError {
    /// Build an error out of a message that may quote a stream URL.
    ///
    /// A Subsonic stream URL carries `u`, `t` (the auth token) and `s` (the
    /// salt) as query params, and the HTTP layers below this one print the URL
    /// they failed on — which the app then puts on the player bar. The host is
    /// the only part worth showing and the query is the part that must not be.
    pub(crate) fn from_http(e: impl std::fmt::Display) -> Self {
        Self(scrub_urls(&e.to_string()))
    }
}

/// Cut the query string off every URL in `msg`, leaving scheme, host and path.
pub fn scrub_urls(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(start) = rest.find("http") {
        if !rest[start..].starts_with("http://") && !rest[start..].starts_with("https://") {
            // "http" inside an ordinary word; copy it and carry on past it.
            out.push_str(&rest[..start + 4]);
            rest = &rest[start + 4..];
            continue;
        }
        out.push_str(&rest[..start]);
        let url = &rest[start..];
        // A URL in prose ends at whitespace or at the bracket/quote it was put
        // in; the query begins at the first '?' inside that.
        let end = url
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\''))
            .unwrap_or(url.len());
        let (url, after) = url.split_at(end);
        match url.split_once('?') {
            Some((base, _)) => {
                out.push_str(base);
                out.push_str("?…");
            }
            None => out.push_str(url),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Handle to the playback engine. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Player {
    tx: mpsc::UnboundedSender<Command>,
    tap: Arc<spectrum::SpectrumTap>,
}

impl Player {
    /// Spawn the engine control loop; returns the handle and the event stream.
    ///
    /// Must be called from within a tokio runtime.
    pub fn new() -> (Player, mpsc::UnboundedReceiver<Event>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let tap = spectrum::SpectrumTap::new();
        engine::spawn(cmd_rx, event_tx, tap.clone());
        (Player { tx: cmd_tx, tap }, event_rx)
    }

    /// Live window onto the samples reaching the output device, for
    /// visualizers. Reading it never blocks the audio thread; see
    /// [`spectrum::SpectrumTap`].
    pub fn spectrum_tap(&self) -> Arc<spectrum::SpectrumTap> {
        self.tap.clone()
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

    /// Linear volume multiplier in [0.0, `MAX_VOLUME`] (clamped by the engine).
    /// Not capped at 1.0: ReplayGain is applied by scaling this, and a quiet
    /// master's positive gain needs more than unity.
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

    /// Open a card directly by its [`DirectDevice::id`] (None = shared output).
    pub fn set_direct_output(&self, id: Option<String>) {
        let _ = self.tx.send(Command::SetDirectOutput(id));
    }
}

#[cfg(test)]
mod scrub_tests {
    use super::scrub_urls;

    #[test]
    fn a_stream_urls_auth_params_are_cut_off() {
        let msg = "error sending request for url (https://music.example.com/rest/stream?id=42&u=me&t=deadbeef&s=abc)";
        let out = scrub_urls(msg);
        assert!(!out.contains("t=deadbeef"), "{out}");
        assert!(!out.contains("s=abc"), "{out}");
        assert!(
            out.contains("https://music.example.com/rest/stream?…"),
            "{out}"
        );
        assert!(out.ends_with(')'), "{out}");
    }

    #[test]
    fn a_message_without_a_url_is_unchanged() {
        let msg = "the format of the data has not been recognized";
        assert_eq!(scrub_urls(msg), msg);
    }

    #[test]
    fn the_word_http_in_prose_is_not_mistaken_for_a_url() {
        let msg = "http chunked transfer ended early";
        assert_eq!(scrub_urls(msg), msg);
    }

    #[test]
    fn a_url_with_no_query_keeps_its_path() {
        let msg = "failed: https://radio.example.com/stream.mp3 unreachable";
        assert_eq!(scrub_urls(msg), msg);
    }
}
