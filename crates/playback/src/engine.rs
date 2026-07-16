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
                        match start_track(&mut output, &track, volume).await {
                            Ok(new_sink) => {
                                if let Some(d) = track.duration_hint {
                                    let _ = event_tx.send(Event::DurationKnown(d));
                                }
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
                        playing = false;
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

    let player = rodio::Player::connect_new(out.mixer());
    player.set_volume(volume);
    player.append(prepared.decoder);
    player.play();
    *sink = Some(player);
    if let Some(d) = prepared.track.duration_hint {
        let _ = event_tx.send(Event::DurationKnown(d));
    }
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
    .map_err(|e| PlaybackError::Decode(e.to_string()))?
    .map_err(|e| PlaybackError::Decode(e.to_string()))?;
    Ok(Prepared { track, decoder })
}

/// Open the HTTP source, build a decoder, and start a new player.
async fn start_track(
    output: &mut Option<rodio::MixerDeviceSink>,
    track: &TrackSource,
    volume: f32,
) -> Result<rodio::Player, PlaybackError> {
    let prepared = prepare(track.clone()).await?;

    if output.is_none() {
        let device_sink = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| PlaybackError::Output(e.to_string()))?;
        *output = Some(device_sink);
    }
    let out = output.as_ref().expect("output just initialized");

    let player = rodio::Player::connect_new(out.mixer());
    player.set_volume(volume);
    player.append(prepared.decoder);
    player.play();
    Ok(player)
}
