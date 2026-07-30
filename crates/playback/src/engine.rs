//! Engine control loop: owns the rodio output and player on a blocking thread,
//! driven by commands from the `Player` handle.
//!
//! Gapless playback: one `rodio::Player` survives across tracks. The prepared
//! next track is appended into that player's queue shortly before the current
//! one ends, so rodio hands over between them sample-continuously (no new
//! player, no silence in between). The hand-over is observed through an
//! `EndSignal` wrapper rather than by polling, so the reported track switch is
//! sample-accurate too.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::source::{self, EndSignal, SourceReader};
use crate::spectrum::{SpectrumTap, Tap};
use crate::{Command, Event, PlaybackError, TrackSource};

const TICK: Duration = Duration::from_millis(500);

/// How long before the end of the current track its successor is appended.
/// Must exceed `TICK` so the tick that spots the window still lands before the
/// hand-over; kept short because an appended track can no longer be pulled back
/// out of rodio's queue.
const COMMIT_LEAD: Duration = Duration::from_secs(3);

/// A fully-opened, decoded-and-ready track, not yet handed to rodio.
struct Prepared {
    track: TrackSource,
    decoder: rodio::Decoder<SourceReader>,
    /// Length according to the decoder, used when the server gave no hint.
    decoded_duration: Option<Duration>,
    /// Set when the source turned out to be a live radio stream.
    station: Option<crate::icy::StationInfo>,
}

/// A track that has been appended to the player's queue: either playing now or
/// waiting directly behind the one that is.
struct Loaded {
    track: TrackSource,
    /// Identifies this append in `EndSignal` messages.
    serial: u64,
    duration: Option<Duration>,
    /// Set when the source turned out to be a live radio stream.
    station: Option<crate::icy::StationInfo>,
}

pub(crate) fn spawn(
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<Event>,
    tap: Arc<SpectrumTap>,
) {
    tokio::spawn(control_loop(cmd_rx, event_tx, tap));
}

async fn control_loop(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<Event>,
    tap: Arc<SpectrumTap>,
) {
    // rodio output must outlive all players; created lazily on first Play so
    // a missing audio device only fails playback, not app startup.
    let mut output: Option<rodio::MixerDeviceSink> = None;
    let mut sink: Option<rodio::Player> = None;
    let mut volume: f32 = 1.0;
    // Chosen output device name (None = OS default) and the currently-loaded
    // track, retained so a device switch can reopen and resume in place.
    let mut selected_device: Option<String> = None;
    let mut current: Option<Loaded> = None;
    // Next track already appended behind `current` (committed, cannot be
    // withdrawn) and the one prepared but still withheld.
    let mut queued: Option<Loaded> = None;
    let mut pending: Option<Prepared> = None;
    // In-flight preparation of the next track; results arrive on `prep_rx`
    // tagged with the generation that requested them, so a superseded prefetch
    // that lands late is discarded.
    let mut prefetch: Option<JoinHandle<()>> = None;
    let mut prefetch_gen: u64 = 0;
    let (prep_tx, mut prep_rx) =
        mpsc::unbounded_channel::<(u64, Result<Prepared, PlaybackError>)>();
    // Track-exhaustion signals from appended sources.
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<u64>();
    let mut serials: u64 = 0;
    let mut ticker = tokio::time::interval(TICK);
    let mut playing = false;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // all Player handles dropped
                match cmd {
                    Command::Play(track) => {
                        drop_prefetch(&mut prefetch, &mut prefetch_gen, &mut pending);
                        queued = None;
                        current = None;
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        let _ = event_tx.send(Event::Buffering);
                        let output_was_open = output.is_some();
                        match start_track(
                            &mut output,
                            &selected_device,
                            track,
                            volume,
                            &mut serials,
                            &end_tx,
                            &tap,
                            &event_tx,
                        )
                        .await
                        {
                            Ok((new_sink, loaded)) => {
                                if !output_was_open {
                                    let _ = event_tx.send(Event::OutputOpened {
                                        device: resolved_device_name(&selected_device),
                                    });
                                }
                                if let Some(d) = loaded.duration {
                                    let _ = event_tx.send(Event::DurationKnown(d));
                                }
                                if let Some(station) = loaded.station.clone() {
                                    let _ = event_tx.send(Event::StationInfo(station));
                                }
                                current = Some(loaded);
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
                        drop_prefetch(&mut prefetch, &mut prefetch_gen, &mut pending);
                        queued = None;
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
                            // An already-appended next track dies with the old
                            // player; re-prepare it so gapless survives.
                            let requeue = queued.take().map(|l| l.track);
                            if let Some(s) = sink.take() {
                                s.stop();
                            }
                            output = None;
                            if let Some(loaded) = current.take() {
                                let _ = event_tx.send(Event::Buffering);
                                match start_track(
                                    &mut output,
                                    &selected_device,
                                    loaded.track,
                                    volume,
                                    &mut serials,
                                    &end_tx,
                                    &tap,
                                    &event_tx,
                                )
                                .await
                                {
                                    Ok((new_sink, loaded)) => {
                                        if let Some(p) = pos
                                            && let Err(e) = new_sink.try_seek(p)
                                        {
                                            tracing::warn!("seek after device switch failed: {e}");
                                        }
                                        if !resume {
                                            new_sink.pause();
                                        }
                                        current = Some(loaded);
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
                            if let Some(track) = requeue {
                                start_prefetch(
                                    track,
                                    &mut prefetch,
                                    &mut prefetch_gen,
                                    &mut pending,
                                    &prep_tx,
                                    &event_tx,
                                );
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
                        if let Some(q) = &queued {
                            // Already appended into rodio's queue, which offers
                            // no way to take it back out. It plays, and the
                            // consumer resyncs from `TrackEnded::started`.
                            if q.track.url != track.url {
                                tracing::debug!("prefetch changed after commit; keeping committed track");
                            }
                        } else {
                            start_prefetch(
                                track,
                                &mut prefetch,
                                &mut prefetch_gen,
                                &mut pending,
                                &prep_tx,
                                &event_tx,
                            );
                        }
                    }
                    Command::ClearPrefetch => {
                        drop_prefetch(&mut prefetch, &mut prefetch_gen, &mut pending);
                    }
                }
            }
            Some((generation, result)) = prep_rx.recv() => {
                if generation == prefetch_gen {
                    prefetch = None;
                    match result {
                        Ok(prepared) => {
                            pending = Some(prepared);
                            commit_next(
                                &mut pending,
                                &mut queued,
                                &sink,
                                current.as_ref(),
                                &mut serials,
                                &end_tx,
                                &tap,
                            );
                        }
                        Err(e) => tracing::warn!("prefetch failed: {e}"),
                    }
                }
            }
            Some(serial) = end_rx.recv() => {
                // Ignore signals from sources of a superseded player: only the
                // track we believe is playing can end.
                if current.as_ref().is_some_and(|c| c.serial == serial) {
                    if let Some(next) = queued.take() {
                        // rodio already flowed into the appended track.
                        let started = next.track.id.clone();
                        let duration = next.duration;
                        current = Some(next);
                        playing = true;
                        let _ = event_tx.send(Event::TrackEnded { auto_advanced: true, started });
                        if let Some(d) = duration {
                            let _ = event_tx.send(Event::DurationKnown(d));
                        }
                        let _ = event_tx.send(Event::Playing);
                    } else {
                        // Nothing lined up: drop the drained player so it stops
                        // feeding keep-alive silence to the mixer.
                        if let Some(s) = sink.take() {
                            s.stop();
                        }
                        current = None;
                        playing = false;
                        let _ = event_tx.send(Event::TrackEnded {
                            auto_advanced: false,
                            started: None,
                        });
                    }
                }
            }
            _ = ticker.tick() => {
                if let Some(s) = &sink
                    && playing
                {
                    if s.empty() {
                        // Belt-and-braces: the end signal should have arrived
                        // first. Report the end so playback cannot wedge.
                        sink = None;
                        current = None;
                        queued = None;
                        playing = false;
                        let _ = event_tx.send(Event::TrackEnded {
                            auto_advanced: false,
                            started: None,
                        });
                    } else {
                        let _ = event_tx.send(Event::Position(s.get_pos()));
                        commit_next(
                            &mut pending,
                            &mut queued,
                            &sink,
                            current.as_ref(),
                            &mut serials,
                            &end_tx,
                            &tap,
                        );
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

/// Start preparing `track`, superseding any prefetch already in flight.
fn start_prefetch(
    track: TrackSource,
    prefetch: &mut Option<JoinHandle<()>>,
    generation: &mut u64,
    pending: &mut Option<Prepared>,
    prep_tx: &mpsc::UnboundedSender<(u64, Result<Prepared, PlaybackError>)>,
    event_tx: &mpsc::UnboundedSender<Event>,
) {
    drop_prefetch(prefetch, generation, pending);
    let generation = *generation;
    let tx = prep_tx.clone();
    let event_tx = event_tx.clone();
    *prefetch = Some(tokio::spawn(async move {
        let result = prepare(track, &event_tx).await;
        let _ = tx.send((generation, result));
    }));
}

/// Abandon the in-flight and the ready-but-uncommitted next track. Bumping the
/// generation makes any result still on its way irrelevant.
fn drop_prefetch(
    prefetch: &mut Option<JoinHandle<()>>,
    generation: &mut u64,
    pending: &mut Option<Prepared>,
) {
    if let Some(handle) = prefetch.take() {
        handle.abort();
    }
    *generation += 1;
    *pending = None;
}

/// Append the prepared next track into the live player once the current track
/// is within `COMMIT_LEAD` of its end, making the hand-over gapless. Nothing
/// happens while the window is still far off, since an appended track cannot be
/// withdrawn if the queue changes.
fn commit_next(
    pending: &mut Option<Prepared>,
    queued: &mut Option<Loaded>,
    sink: &Option<rodio::Player>,
    current: Option<&Loaded>,
    serials: &mut u64,
    end_tx: &mpsc::UnboundedSender<u64>,
    tap: &Arc<SpectrumTap>,
) {
    if pending.is_none() || queued.is_some() {
        return;
    }
    let Some(s) = sink else { return };
    // Without a known length there is no window to wait for: append now rather
    // than risk missing the join.
    if let Some(total) = current.and_then(|c| c.duration)
        && total.saturating_sub(s.get_pos()) > COMMIT_LEAD
    {
        return;
    }
    let prepared = pending.take().expect("checked above");
    *queued = Some(append(s, prepared, serials, end_tx, tap));
}

/// Hand a prepared track to the player's queue and describe what was appended.
fn append(
    sink: &rodio::Player,
    prepared: Prepared,
    serials: &mut u64,
    end_tx: &mpsc::UnboundedSender<u64>,
    tap: &Arc<SpectrumTap>,
) -> Loaded {
    *serials += 1;
    let serial = *serials;
    // Tap innermost: it mirrors exactly the samples this track contributes,
    // and wrapping it inside `EndSignal` keeps the end-of-track detection on
    // the outermost source where the player pulls from.
    sink.append(EndSignal::new(
        Tap::new(prepared.decoder, tap.clone()),
        serial,
        end_tx.clone(),
    ));
    Loaded {
        // Server metadata wins over the decoder's guess; the decoder covers
        // sources the server said nothing about.
        duration: prepared.track.duration_hint.or(prepared.decoded_duration),
        track: prepared.track,
        station: prepared.station,
        serial,
    }
}

/// How long a decoder may take to identify a source before it is written off.
/// Needed because an undecodable endless stream does not error — it simply
/// never yields a packet, which would otherwise wedge playback on "buffering".
const DECODE_TIMEOUT: Duration = Duration::from_secs(12);

/// Open the source (local file or HTTP) and build a decoder, ready to
/// append to a player.
///
/// A URL may expand to several candidates (a station playlist listing the same
/// programme in several formats); they are tried in order and the first that
/// decodes wins.
async fn prepare(
    track: TrackSource,
    event_tx: &mpsc::UnboundedSender<Event>,
) -> Result<Prepared, PlaybackError> {
    if let Some(path) = track.path.clone() {
        let (reader, byte_len) = source::open_local(&path).await?;
        return build(track, reader, byte_len, None).await;
    }

    let candidates = source::stream_candidates(&track.url).await;
    let mut last = None;
    for url in &candidates {
        let attempt = async {
            let (reader, byte_len, station) = source::open(url, event_tx).await?;
            build(track.clone(), reader, byte_len, station).await
        };
        match attempt.await {
            Ok(prepared) => return Ok(prepared),
            Err(e) => {
                if candidates.len() > 1 {
                    tracing::warn!("stream {url} unusable ({e}), trying next");
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| PlaybackError("no playable stream".into())))
}

/// Build a decoder over an opened source.
async fn build(
    track: TrackSource,
    reader: SourceReader,
    byte_len: Option<u64>,
    station: Option<crate::icy::StationInfo>,
) -> Result<Prepared, PlaybackError> {
    // Decoder construction reads from the (blocking) stream reader; do it off
    // the async thread. byte_len enables seeking + duration calculation.
    let decoder = tokio::time::timeout(
        DECODE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            // A live stream has no length and no seeking: declaring it seekable
            // makes the decoder probe backwards through a source that only
            // moves forward, which costs seconds of startup on a radio stream.
            let mut builder = rodio::Decoder::builder()
                .with_data(reader)
                .with_seekable(byte_len.is_some());
            if let Some(len) = byte_len {
                builder = builder.with_byte_len(len);
            }
            builder.build()
        }),
    )
    .await
    .map_err(|_| PlaybackError("timed out identifying the stream".into()))?
    .map_err(|e| PlaybackError(e.to_string()))?
    .map_err(|e| PlaybackError(e.to_string()))?;
    let decoded_duration = rodio::Source::total_duration(&decoder);
    Ok(Prepared {
        track,
        decoder,
        decoded_duration,
        station,
    })
}

/// Open the HTTP source, build a decoder, and start a new player on the
/// selected output device (opening it if not already open).
// Everything here is engine-loop state that has to be threaded through by
// reference; bundling it into a struct would only move the same list one level
// out.
#[allow(clippy::too_many_arguments)]
async fn start_track(
    output: &mut Option<rodio::MixerDeviceSink>,
    selected_device: &Option<String>,
    track: TrackSource,
    volume: f32,
    serials: &mut u64,
    end_tx: &mpsc::UnboundedSender<u64>,
    tap: &Arc<SpectrumTap>,
    event_tx: &mpsc::UnboundedSender<Event>,
) -> Result<(rodio::Player, Loaded), PlaybackError> {
    let prepared = prepare(track, event_tx).await?;

    if output.is_none() {
        *output = Some(open_output(selected_device)?);
    }
    let out = output
        .as_ref()
        .ok_or(PlaybackError("no output sink".into()))?;

    let player = rodio::Player::connect_new(out.mixer());
    player.set_volume(volume);
    let loaded = append(&player, prepared, serials, end_tx, tap);
    player.play();
    Ok((player, loaded))
}

/// Open the sink for `selected` (a cpal device description name), falling back
/// to the system default when None or when the named device is gone.
fn open_output(selected: &Option<String>) -> Result<rodio::MixerDeviceSink, PlaybackError> {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    if let Some(name) = selected
        && let Ok(devices) = rodio::cpal::default_host().output_devices()
    {
        for dev in devices {
            if dev
                .description()
                .ok()
                .map(|d| d.name().to_string())
                .as_deref()
                == Some(name.as_str())
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
