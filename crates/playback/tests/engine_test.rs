use std::time::Duration;

use playback::{Event, Player, TrackSource};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal valid WAV: 16-bit mono 8kHz, ~0.5s of silence.
fn wav_bytes() -> Vec<u8> {
    let sample_rate: u32 = 8000;
    let samples: u32 = sample_rate / 2; // 0.5s
    let data_len = samples * 2;
    let mut buf = Vec::with_capacity(44 + data_len as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.resize(44 + data_len as usize, 0);
    buf
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_wav_stream_to_completion() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(wav_bytes()),
        )
        .mount(&server)
        .await;

    let (player, mut events) = Player::new();
    player.set_volume(0.0); // silent test run
    player.play(TrackSource {
        url: format!("{}/rest/stream", server.uri()),
        duration_hint: Some(Duration::from_millis(500)),
    });

    let mut saw_playing = false;
    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event.expect("event channel closed early") {
                    Event::Playing => saw_playing = true,
                    Event::TrackEnded { .. } => break,
                    Event::Failed(msg) => {
                        // Headless environments (CI) may have no audio device;
                        // treat that as a skip rather than a failure.
                        if msg.contains("audio output unavailable") {
                            eprintln!("skipping: no audio device ({msg})");
                            return;
                        }
                        panic!("playback failed: {msg}");
                    }
                    _ => {}
                }
            }
            _ = &mut deadline => panic!("timed out waiting for TrackEnded"),
        }
    }
    assert!(saw_playing, "never saw Playing event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetched_track_auto_advances() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/wav")
                .set_body_bytes(wav_bytes()),
        )
        .mount(&server)
        .await;

    let (player, mut events) = Player::new();
    player.set_volume(0.0);
    player.play(TrackSource {
        url: format!("{}/rest/stream?id=1", server.uri()),
        duration_hint: Some(Duration::from_millis(500)),
    });
    player.prefetch_next(TrackSource {
        url: format!("{}/rest/stream?id=2", server.uri()),
        duration_hint: Some(Duration::from_millis(500)),
    });

    let mut ends = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event.expect("event channel closed early") {
                    Event::TrackEnded { auto_advanced } => {
                        ends.push(auto_advanced);
                        if ends.len() == 2 { break; }
                    }
                    Event::Failed(msg) => {
                        if msg.contains("audio output unavailable") {
                            eprintln!("skipping: no audio device ({msg})");
                            return;
                        }
                        panic!("playback failed: {msg}");
                    }
                    _ => {}
                }
            }
            _ = &mut deadline => panic!("timed out; ends so far: {ends:?}"),
        }
    }
    // First end transitions into the prefetched track; second is a plain end.
    assert_eq!(ends, vec![true, false]);
}
