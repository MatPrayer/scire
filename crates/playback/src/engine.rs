//! Engine control loop: owns the rodio output and sink on a blocking thread,
//! driven by commands from the `Player` handle.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::source::{self, StreamReader};
use crate::{Command, Event, PlaybackError, TrackSource};

const TICK: Duration = Duration::from_millis(500);

/// A fully-opened, decoded-and-ready next track.
struct Prepared {
    track: TrackSource,
    decoder: rodio::Decoder<StreamReader>,
}

pub(crate) fn spawn(
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(control_loop(cmd_rx, event_tx));
}

async fn control_loop(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    // rodio output must outlive all players; created lazily on first Play so
    // a missing audio device only fails playback, not app startup.
    let mut output: Option<rodio::MixerDeviceSink> = None;
    let mut sink: Option<rodio::Player> = None;
    let mut volume: f32 = 1.0;
    // Chosen output device name (None = OS default) and the currently-loaded
    // track, retained so a device switch can reopen and resume in place.
    let mut selected_device: Option<String> = None;
    let mut current: Option<TrackSource> = None;
    // In-flight preparation of the next track for gapless transition.
    let mut prefetch: Option<JoinHandle<Result<Prepared, PlaybackError>>> = None;
    let mut ticker = tokio::time::interval(TICK);
    let mut playing = false;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // all Player handles dropped
                match cmd {
                    Command::Play(track) => {
                        abort_prefetch(&mut prefetch);
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        let _ = event_tx.send(Event::Buffering);
                        let output_was_open = output.is_some();
                        match start_track(&mut output, &selected_device, &track, volume).await {
                            Ok(new_sink) => {
                                if !output_was_open {
                                    let _ = event_tx.send(Event::OutputOpened {
                                        device: resolved_device_name(&selected_device),
                                    });
                                }
                                if let Some(d) = track.duration_hint {
                                    let _ = event_tx.send(Event::DurationKnown(d));
                                }
                                current = Some(track);
                                sink = Some(new_sink);
                                playing = true;
                                let _ = event_tx.send(Event::Playing);
                            }
                            Err(e) => {
                                playing = false;
                                let _ = event_tx.send(Event::Failed(e.to_string()));
                            }
                        }
                    }
                    Command::Pause => {
                        if let Some(s) = &sink {
                            s.pause();
                            playing = false;
                            let _ = event_tx.send(Event::Paused);
                        }
                    }
                    Command::Resume => {
                        if let Some(s) = &sink {
                            s.play();
                            playing = true;
                            let _ = event_tx.send(Event::Playing);
                        }
                    }
                    Command::Stop => {
                        abort_prefetch(&mut prefetch);
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        current = None;
                        playing = false;
                    }
                    Command::SetOutputDevice(name) => {
                        if name != selected_device {
                            selected_device = name;
                            let _ = event_tx.send(Event::OutputOpened {
                                device: resolved_device_name(&selected_device),
                            });
                            // Reopen on the new device, resuming the current
                            // track at its position (paused stays paused).
                            let resume = playing;
                            let pos = sink.as_ref().map(|s| s.get_pos());
                            abort_prefetch(&mut prefetch);
                            if let Some(s) = sink.take() {
                                s.stop();
                            }
                            output = None;
                            if let Some(track) = current.clone() {
                                let _ = event_tx.send(Event::Buffering);
                                match start_track(&mut output, &selected_device, &track, volume)
                                    .await
                                {
                                    Ok(new_sink) => {
                                        if let Some(p) = pos
                                            && let Err(e) = new_sink.try_seek(p)
                                        {
                                            tracing::warn!("seek after device switch failed: {e}");
                                        }
                                        if !resume {
                                            new_sink.pause();
                                        }
                                        sink = Some(new_sink);
                                        playing = resume;
                                        let _ = event_tx.send(if resume {
                                            Event::Playing
                                        } else {
                                            Event::Paused
                                        });
                                    }
                                    Err(e) => {
                                        playing = false;
                                        let _ = event_tx.send(Event::Failed(e.to_string()));
                                    }
                                }
                            }
                        }
                    }
                    Command::Seek(pos) => {
                        if let Some(s) = &sink {
                            if let Err(e) = s.try_seek(pos) {
                                tracing::warn!("seek failed: {e}");
                            } else {
                                let _ = event_tx.send(Event::Position(pos));
                            }
                        }
                    }
                    Command::SetVolume(v) => {
                        volume = v.clamp(0.0, 1.0);
                        if let Some(s) = &sink {
                            s.set_volume(volume);
                        }
                    }
                    Command::PrefetchNext(track) => {
                        abort_prefetch(&mut prefetch);
                        prefetch = Some(tokio::spawn(prepare(track)));
                    }
                    Command::ClearPrefetch => {
                        abort_prefetch(&mut prefetch);
                    }
                }
            }
            _ = ticker.tick() => {
                if let Some(s) = &sink
                    && playing
                {
                    if s.empty() {
                        // Track drained. Start the prefetched next track if
                        // it is ready; otherwise report a plain end and let
                        // the consumer drive the next Play.
                        sink = None;
                        let auto = try_start_prefetched(
                            &mut prefetch,
                            &mut sink,
                            &output,
                            volume,
                            &event_tx,
                            &mut current,
                        );
                        playing = auto;
                        let _ = event_tx.send(Event::TrackEnded { auto_advanced: auto });
                        if auto {
                            let _ = event_tx.send(Event::Playing);
                        }
                    } else {
                        let _ = event_tx.send(Event::Position(s.get_pos()));
                    }
                }
            }
        }
    }
}

/// Name of the OS default output device (what `open_default_sink` uses).
/// cpal's ALSA host only reports a generic "default", so on Linux ask
/// PulseAudio/PipeWire for the real sink description first.
fn default_output_device_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(desc) = pulse_default_sink_description() {
        return Some(desc);
    }
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
}

/// Human-readable description of the default PulseAudio/PipeWire sink, via
/// `pactl` (best-effort; None when unavailable).
#[cfg(target_os = "linux")]
fn pulse_default_sink_description() -> Option<String> {
    use std::process::Command;
    let out = Command::new("pactl")
        .env("LC_ALL", "C") // keep field labels unlocalized
        .arg("get-default-sink")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sink = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sink.is_empty() {
        return None;
    }
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["list", "sinks"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut in_target = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Name:") {
            in_target = name.trim() == sink;
        } else if in_target && let Some(desc) = line.strip_prefix("Description:") {
            return Some(desc.trim().to_string());
        }
    }
    None
}

fn abort_prefetch(prefetch: &mut Option<JoinHandle<Result<Prepared, PlaybackError>>>) {
    if let Some(handle) = prefetch.take() {
        handle.abort();
    }
}

/// If a prefetched track finished preparing, start it on a fresh player and
/// return true. A prefetch still in flight is aborted (the consumer will send
/// a regular Play, which re-opens the source anyway).
fn try_start_prefetched(
    prefetch: &mut Option<JoinHandle<Result<Prepared, PlaybackError>>>,
    sink: &mut Option<rodio::Player>,
    output: &Option<rodio::MixerDeviceSink>,
    volume: f32,
    event_tx: &mpsc::UnboundedSender<Event>,
    current: &mut Option<TrackSource>,
) -> bool {
    let Some(handle) = prefetch.take() else {
        return false;
    };
    if !handle.is_finished() {
        handle.abort();
        return false;
    }
    // is_finished: now_or_never-style await cannot block.
    let prepared = match futures_now(handle) {
        Some(Ok(p)) => p,
        _ => return false,
    };
    let Some(out) = output else { return false };

    let track = prepared.track.clone();
    let player = rodio::Player::connect_new(out.mixer());
    player.set_volume(volume);
    player.append(prepared.decoder);
    player.play();
    *sink = Some(player);
    if let Some(d) = track.duration_hint {
        let _ = event_tx.send(Event::DurationKnown(d));
    }
    *current = Some(track);
    true
}

/// Resolve a finished JoinHandle without awaiting (caller checked
/// `is_finished`). Returns None on join error (panic/abort).
fn futures_now(
    handle: JoinHandle<Result<Prepared, PlaybackError>>,
) -> Option<Result<Prepared, PlaybackError>> {
    use std::future::Future as _;
    use std::task::{Context, Poll};
    let mut handle = handle;
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    match std::pin::Pin::new(&mut handle).poll(&mut cx) {
        Poll::Ready(Ok(result)) => Some(result),
        _ => None,
    }
}

/// Open the HTTP source and build a decoder, ready to append to a player.
async fn prepare(track: TrackSource) -> Result<Prepared, PlaybackError> {
    let (reader, byte_len) = source::open(&track.url).await?;
    // Decoder construction reads from the (blocking) stream reader; do it off
    // the async thread. byte_len enables seeking + duration calculation.
    let decoder = tokio::task::spawn_blocking(move || {
        let mut builder = rodio::Decoder::builder()
            .with_data(reader)
            .with_seekable(true);
        if let Some(len) = byte_len {
            builder = builder.with_byte_len(len);
        }
        builder.build()
    })
    .await
    .map_err(|e| PlaybackError(e.to_string()))?
    .map_err(|e| PlaybackError(e.to_string()))?;
    Ok(Prepared { track, decoder })
}

/// Open the HTTP source, build a decoder, and start a new player on the
/// selected output device (opening it if not already open).
async fn start_track(
    output: &mut Option<rodio::MixerDeviceSink>,
    selected_device: &Option<String>,
    track: &TrackSource,
    volume: f32,
) -> Result<rodio::Player, PlaybackError> {
    let prepared = prepare(track.clone()).await?;

    if output.is_none() {
        *output = Some(open_output(selected_device)?);
    }
    let out = output.as_ref().ok_or(PlaybackError("no output sink".into()))?;

    let player = rodio::Player::connect_new(out.mixer());
    player.set_volume(volume);
    player.append(prepared.decoder);
    player.play();
    Ok(player)
}

/// Open the sink for `selected` (a cpal device description name), falling back
/// to the system default when None or when the named device is gone.
fn open_output(selected: &Option<String>) -> Result<rodio::MixerDeviceSink, PlaybackError> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    if let Some(name) = selected
        && let Ok(devices) = rodio::cpal::default_host().output_devices()
    {
        for dev in devices {
            if dev.description().ok().map(|d| d.name().to_string()).as_deref() == Some(name.as_str())
            {
                return rodio::DeviceSinkBuilder::from_device(dev)
                    .and_then(|b| b.open_stream())
                    .map_err(|e| PlaybackError(e.to_string()));
            }
        }
    }
    rodio::DeviceSinkBuilder::open_default_sink().map_err(|e| PlaybackError(e.to_string()))
}

/// Display name for the selected device: the chosen name, or the resolved
/// default device description when None.
fn resolved_device_name(selected: &Option<String>) -> Option<String> {
    match selected {
        Some(name) => Some(name.clone()),
        None => default_output_device_name(),
    }
}
