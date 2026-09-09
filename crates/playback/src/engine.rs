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

#[cfg(target_os = "linux")]
use crate::pulse;
use crate::source::{self, EndSignal, Hint, Opened, SourceReader};
use crate::spectrum::{SpectrumTap, Tap};
use crate::{Command, Event, PlaybackError, TrackSource};

const TICK: Duration = Duration::from_millis(500);

/// How long before the end of the current track its successor is appended.
/// Must exceed `TICK` so the tick that spots the window still lands before the
/// hand-over; kept short because an appended track can no longer be pulled back
/// out of rodio's queue.
const COMMIT_LEAD: Duration = Duration::from_secs(3);

/// Ticks between output-route checks (~2s at `TICK`).
const ROUTE_CHECK_TICKS: u8 = 4;

/// Ticks between route checks while nothing is playing (~8s at `TICK`). The
/// output sink outlives the track playing through it, so the route has to be
/// watched while idle too — earbuds connected between tracks would otherwise go
/// unnoticed for the rest of the session, since nothing reopens an output that
/// was never dropped. Checking costs a `pactl` subprocess on Linux, hence the
/// slower cadence; a Play or Resume forces a check anyway.
const IDLE_ROUTE_CHECK_TICKS: u8 = 16;

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
    // Resolved name of the device `output` was actually opened on, and the
    // device playback was auto-paused for when it disappeared. macOS hands a
    // Bluetooth disconnect (earbuds off, AirPods out of the ear) to us as a
    // dead stream on a device that is no longer the default: rodio's player
    // then drains, which the tick below used to read as "track finished" and
    // the queue walked on through the laptop speakers. Watching the route
    // instead lets the engine pause, move the output, and resume only when the
    // device that vanished comes back.
    let mut open_device: Option<String> = None;
    let mut lost_device: Option<String> = None;
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
        mpsc::unbounded_channel::<(u64, Option<String>, Result<Prepared, PlaybackError>)>();
    // Track-exhaustion signals from appended sources.
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<u64>();
    let mut serials: u64 = 0;
    let mut ticker = tokio::time::interval(TICK);
    // Ticks since the output route was last checked. Asking the OS costs a
    // device enumeration (a `pactl` subprocess on Linux), so it is throttled to
    // ~2s while playing (or while waiting for a device to come back), ~8s while
    // idle, and skipped entirely until an output has been opened.
    let mut route_ticks: u8 = 0;
    // Set by Play/Resume, cleared by the next route check: the user asked for
    // audio *now*, so a device found missing in that window must not put
    // playback on hold waiting for it to come back.
    let mut route_grace = false;
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
                        // The route may have moved while we sat idle: an output
                        // is only ever opened when there is none, so a stale one
                        // would keep this track on the old device until the tick
                        // below tore it down mid-playback. Drop it here instead
                        // and let `start_track` open on the current route.
                        if output.is_some() && route_lost(&selected_device, &open_device) {
                            output = None;
                        }
                        route_ticks = 0;
                        route_grace = true;
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
                                    open_device = resolved_device_name(&selected_device);
                                    let _ = event_tx.send(Event::OutputOpened {
                                        device: open_device.clone(),
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
                            // Resuming onto a route that moved while paused
                            // would play the first seconds on the old device;
                            // make the next tick reconcile it instead of
                            // waiting out the idle interval.
                            route_ticks = u8::MAX;
                            route_grace = true;
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
                            lost_device = None;
                            open_device = resolved_device_name(&selected_device);
                            let _ = event_tx.send(Event::OutputOpened {
                                device: open_device.clone(),
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
            Some((generation, id, result)) = prep_rx.recv() => {
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
                        // The next track is already known to be unplayable.
                        // Reporting it now lets the consumer say so while the
                        // current track is still running, instead of the queue
                        // stopping dead on a hand-over that never comes.
                        Err(e) => {
                            tracing::warn!("prefetch failed: {e}");
                            let _ = event_tx.send(Event::PrefetchFailed {
                                id,
                                error: e.to_string(),
                            });
                        }
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
                // Route watch first: a dead output makes rodio's player look
                // drained, and the `empty()` branch below would report that as
                // a finished track and advance the queue onto the speakers.
                route_ticks = route_ticks.saturating_add(1);
                let due = if playing || lost_device.is_some() {
                    ROUTE_CHECK_TICKS
                } else {
                    IDLE_ROUTE_CHECK_TICKS
                };
                let check_route = output.is_some() && route_ticks >= due;
                if check_route {
                    route_ticks = 0;
                }
                if check_route && route_lost(&selected_device, &open_device) {
                    // A device pulled out from under playback is not the same
                    // event as a new one taking the route over, and only the
                    // first should stop the music.
                    let vanished = open_device.as_deref().is_none_or(|name| !device_present(name));
                    let action = route_action(playing, vanished, route_grace);
                    route_grace = false;
                    if action == RouteAction::HoldForDevice {
                        lost_device = open_device.clone();
                    }
                    let resume = action == RouteAction::Follow;
                    let pos = sink.as_ref().map(|s| s.get_pos());
                    // The appended next track dies with the old player.
                    let requeue = queued.take().map(|l| l.track);
                    if let Some(s) = sink.take() {
                        s.stop();
                    }
                    output = None;
                    playing = false;
                    if let Some(loaded) = current.take() {
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
                                    tracing::warn!("seek after route change failed: {e}");
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
                                let _ = event_tx.send(Event::Failed(e.to_string()));
                            }
                        }
                    }
                    open_device = resolved_device_name(&selected_device);
                    let _ = event_tx.send(Event::OutputOpened {
                        device: open_device.clone(),
                    });
                    // The device that was pulled out is back: pick playback up
                    // where it stopped. Anything else stays paused — the user
                    // asked for audio in the earbuds, not in the room.
                    if lost_device.is_some() && lost_device == open_device {
                        lost_device = None;
                        if let Some(s) = &sink {
                            s.play();
                            playing = true;
                            let _ = event_tx.send(Event::Playing);
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
                } else if let Some(s) = &sink
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

/// Has the output route moved out from under an open sink?
///
/// The test is the same in every case: where would audio go if we opened now,
/// and is that still where this sink was opened? With no device chosen that
/// catches the default moving (a Bluetooth headset connecting, or being pulled
/// out and dropping the route back to the speakers); with one chosen it catches
/// both it going away — `open_output` falls back to the default — and it coming
/// back, which is when we should leave the fallback and go claim it.
fn route_lost(selected: &Option<String>, open: &Option<String>) -> bool {
    // Never seen the device we opened on, or cannot tell where audio would go
    // now: nothing to compare against, and a name query that keeps failing must
    // not restart playback every tick.
    if open.is_none() {
        return false;
    }
    let Some(resolved) = resolved_device_name(selected) else {
        return false;
    };
    Some(resolved.as_str()) != open.as_deref()
}

/// What a detected route change means for playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteAction {
    /// Move the output and keep playing: the route moved to another device
    /// (earbuds connected) rather than the one in use going away.
    Follow,
    /// The device playing was pulled out. Pause, and resume only when it comes
    /// back — the user asked for audio in the earbuds, not in the room.
    HoldForDevice,
    /// Nothing was playing: move the output and stay as we were.
    Moved,
}

/// Decide what to do about a route change. `user_started` marks a Play or
/// Resume since the last check: an explicit request for audio outranks waiting
/// for a device, so a device found missing in that window is followed rather
/// than held for.
fn route_action(playing: bool, vanished: bool, user_started: bool) -> RouteAction {
    match (playing, vanished && !user_started) {
        (false, _) => RouteAction::Moved,
        (true, true) => RouteAction::HoldForDevice,
        (true, false) => RouteAction::Follow,
    }
}

/// Is the device this name refers to still connected? Names come from
/// `output_devices`, so they are PulseAudio/PipeWire sink descriptions on Linux
/// and cpal descriptions elsewhere.
fn device_present(name: &str) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(sinks) = pulse::descriptions() {
        return sinks.iter().any(|d| d == name);
    }
    cpal_device_present(name)
}

/// Is a device with this cpal description name currently connected?
fn cpal_device_present(name: &str) -> bool {
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    let Ok(devices) = rodio::cpal::default_host().output_devices() else {
        // Can't enumerate: assume it is there rather than tearing the output
        // down on a transient host error.
        return true;
    };
    devices
        .into_iter()
        .any(|d| d.description().ok().is_some_and(|desc| desc.name() == name))
}

/// Name of the OS default output device (what `open_default_sink` uses).
/// cpal's ALSA host only reports a generic "default", so on Linux ask
/// PulseAudio/PipeWire for the real sink description first.
fn default_output_device_name() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(desc) = pulse::default_sink_description() {
        return Some(desc);
    }
    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    rodio::cpal::default_host()
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
}

/// Start preparing `track`, superseding any prefetch already in flight.
fn start_prefetch(
    track: TrackSource,
    prefetch: &mut Option<JoinHandle<()>>,
    generation: &mut u64,
    pending: &mut Option<Prepared>,
    prep_tx: &mpsc::UnboundedSender<(u64, Option<String>, Result<Prepared, PlaybackError>)>,
    event_tx: &mpsc::UnboundedSender<Event>,
) {
    drop_prefetch(prefetch, generation, pending);
    let generation = *generation;
    let tx = prep_tx.clone();
    let event_tx = event_tx.clone();
    let id = track.id.clone();
    *prefetch = Some(tokio::spawn(async move {
        let result = prepare(track, &event_tx).await;
        let _ = tx.send((generation, id, result));
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
        let opened = source::open_local(&path).await?;
        return build(track, opened).await;
    }

    let live = track.live;
    let candidates = source::stream_candidates(&track.url).await;
    let mut last = None;
    for url in &candidates {
        let attempt = async {
            let opened = source::open(url, live, event_tx).await?;
            build(track.clone(), opened).await
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
async fn build(track: TrackSource, opened: Opened) -> Result<Prepared, PlaybackError> {
    let Opened {
        reader,
        byte_len,
        station,
        hint,
    } = opened;
    // Kept for the failure message: what the source claimed to be decides how a
    // "format not recognized" should be read.
    let described = hint.clone();
    let seekable = byte_len.is_some();

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
                .with_seekable(seekable);
            if let Some(len) = byte_len {
                builder = builder.with_byte_len(len);
            }
            // Tell the decoder what the source claims to be. symphonia 0.5.5
            // takes the hint as `_hint` and identifies the format from marker
            // bytes regardless, so this buys nothing today — it is passed
            // because this is the one place that knows, and the hint is what
            // the failure message below is built from.
            match &hint {
                Some(Hint::MimeType(mime)) => builder = builder.with_mime_type(mime),
                Some(Hint::Extension(ext)) => builder = builder.with_hint(ext),
                None => {}
            }
            builder.build()
        }),
    )
    .await
    .map_err(|_| PlaybackError("timed out identifying the stream".into()))?
    .map_err(|e| PlaybackError(e.to_string()))?
    .map_err(|e| decoder_failure(e, described.as_ref(), seekable))?;
    let decoded_duration = rodio::Source::total_duration(&decoder);
    Ok(Prepared {
        track,
        decoder,
        decoded_duration,
        station,
    })
}

/// Turn a rodio decoder failure into something that names a cause.
///
/// rodio collapses every `Unsupported` symphonia raises — missing `moov` atom,
/// more than one sample entry, an ALAC magic cookie it does not know — into a
/// single `UnrecognizedFormat` whose text is "the format of the data has not
/// been recognized". For an MP4/M4A that is almost always one specific thing:
/// the file's index sits *after* the audio, and the decoder could not seek back
/// to it. That happens when the response carried no `Content-Length`, or when
/// the server answered a ranged request with the whole file.
fn decoder_failure(
    err: rodio::decoder::DecoderError,
    hint: Option<&Hint>,
    seekable: bool,
) -> PlaybackError {
    tracing::warn!(?hint, seekable, "decoder rejected the source: {err}");
    use rodio::decoder::DecoderError;
    let message = match &err {
        DecoderError::UnrecognizedFormat if is_mp4(hint) && !seekable => {
            "unreadable m4a: the server sent no length, so the file's index \
             (stored after the audio) could not be reached"
                .to_string()
        }
        DecoderError::UnrecognizedFormat if is_mp4(hint) => {
            "unreadable m4a: the file's index could not be read — the server \
             may not honour ranged requests"
                .to_string()
        }
        DecoderError::IoError(inner) => format!("stream read failed: {inner}"),
        _ => err.to_string(),
    };
    PlaybackError(message)
}

/// Whether the source claims to be an ISO base-media file (MP4/M4A), the
/// container ALAC and AAC arrive in.
fn is_mp4(hint: Option<&Hint>) -> bool {
    match hint {
        Some(Hint::MimeType(mime)) => matches!(
            mime.as_str(),
            "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "video/mp4" | "audio/mp4a-latm"
        ),
        Some(Hint::Extension(ext)) => {
            matches!(ext.as_str(), "m4a" | "m4b" | "m4p" | "mp4" | "mov")
        }
        None => false,
    }
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

/// Open the sink for `selected` (a name as `output_devices` reports it), falling
/// back to the system default when None or when the named device is gone.
fn open_output(selected: &Option<String>) -> Result<rodio::MixerDeviceSink, PlaybackError> {
    // On Linux the names are PulseAudio/PipeWire sink descriptions, which cpal
    // cannot open by name — it only sees ALSA devices, and a Bluetooth sink is
    // not one. Open the default stream and move it to the chosen sink instead,
    // the same thing a desktop volume applet does.
    #[cfg(target_os = "linux")]
    if pulse::descriptions().is_some() {
        let sink = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| PlaybackError(e.to_string()))?;
        // A name saved before this machine's sinks changed (or by an older
        // build, which stored cpal names) matches nothing; the default is where
        // that should land, without spending the retry window looking for a
        // sink that is not there.
        if let Some(name) = selected.as_deref().filter(|name| device_present(name)) {
            retarget_stream(name);
        }
        return Ok(sink);
    }

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

/// Move this process's playback stream onto `name`, retrying briefly.
///
/// The sound server registers the stream a moment after the device is opened,
/// and there is nothing to move until it has; a move that lands late plays the
/// first fraction of a second on the wrong device. Blocking is deliberate —
/// this runs inside `open_output`, which is already a blocking device open, and
/// only on a first play, a device switch or a route change.
#[cfg(target_os = "linux")]
fn retarget_stream(name: &str) {
    use crate::pulse::MoveResult;
    const ATTEMPTS: u32 = 10;
    const WAIT: Duration = Duration::from_millis(30);
    for _ in 0..ATTEMPTS {
        match pulse::move_output_to(name) {
            MoveResult::Landed => return,
            // Something else on this desktop owns the routing. Retrying would
            // only fight it, and it would win.
            MoveResult::Grabbed(actual) => {
                tracing::warn!(
                    "asked for output on {name}, but another program routes our audio to {actual}"
                );
                return;
            }
            MoveResult::NoStream => std::thread::sleep(WAIT),
        }
    }
    tracing::warn!("could not move the output to {name}; playing on the default device");
}

/// Where audio would actually go if the output were opened right now: the
/// chosen device, or the system default when nothing is chosen — or when the
/// chosen device is gone, since `open_output` falls back to the default. That
/// fallback has to be reported honestly: naming the chosen device anyway would
/// hide it, and would leave the route watch with nothing to notice when the
/// device came back.
fn resolved_device_name(selected: &Option<String>) -> Option<String> {
    match selected {
        Some(name) if device_present(name) => Some(name.clone()),
        _ => default_output_device_name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::decoder::DecoderError;

    #[test]
    fn mp4_containers_are_recognized_from_either_hint() {
        assert!(is_mp4(Some(&Hint::MimeType("audio/mp4".into()))));
        assert!(is_mp4(Some(&Hint::MimeType("audio/x-m4a".into()))));
        assert!(is_mp4(Some(&Hint::Extension("m4a".into()))));
        assert!(!is_mp4(Some(&Hint::MimeType("audio/flac".into()))));
        assert!(!is_mp4(Some(&Hint::Extension("flac".into()))));
        assert!(!is_mp4(None));
    }

    #[test]
    fn unseekable_m4a_failure_names_the_missing_length() {
        let hint = Hint::MimeType("audio/mp4".into());
        let err = decoder_failure(DecoderError::UnrecognizedFormat, Some(&hint), false);
        assert!(err.0.contains("no length"), "{}", err.0);
    }

    #[test]
    fn seekable_m4a_failure_points_at_ranged_requests() {
        let hint = Hint::Extension("m4a".into());
        let err = decoder_failure(DecoderError::UnrecognizedFormat, Some(&hint), true);
        assert!(err.0.contains("ranged requests"), "{}", err.0);
    }

    #[test]
    fn non_mp4_failure_keeps_the_decoder_wording() {
        let hint = Hint::MimeType("audio/flac".into());
        let err = decoder_failure(DecoderError::UnrecognizedFormat, Some(&hint), true);
        assert_eq!(err.0, DecoderError::UnrecognizedFormat.to_string());
    }

    #[test]
    fn earbuds_taking_the_route_over_keep_playing() {
        // Default moved to a device that just appeared; the one we were on is
        // still there, so follow it rather than stopping the music.
        assert_eq!(route_action(true, false, false), RouteAction::Follow);
    }

    #[test]
    fn a_device_pulled_out_mid_track_holds_playback() {
        assert_eq!(route_action(true, true, false), RouteAction::HoldForDevice);
    }

    #[test]
    fn a_route_change_while_paused_only_moves_the_output() {
        assert_eq!(route_action(false, true, false), RouteAction::Moved);
        assert_eq!(route_action(false, false, false), RouteAction::Moved);
    }

    #[test]
    fn pressing_play_outranks_waiting_for_a_missing_device() {
        // Unplugged while paused, then Play: the user wants audio now, on
        // whatever is connected.
        assert_eq!(route_action(true, true, true), RouteAction::Follow);
    }

    #[test]
    fn io_failure_carries_the_underlying_reason() {
        // rodio's Display for IoError drops the payload; the reason is the
        // whole value of the message.
        let err = decoder_failure(DecoderError::IoError("connection reset".into()), None, true);
        assert!(err.0.contains("connection reset"), "{}", err.0);
    }
}
