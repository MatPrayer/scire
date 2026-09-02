use std::io::Write;
use std::path::PathBuf;
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
        path: None,
        id: None,
        live: false,
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

/// ALAC lives in an m4a container and is outside rodio's default codec set —
/// a Navidrome library streaming raw Apple Lossless must still decode. The
/// fixture also has its `moov` atom at the end, so this exercises the decoder
/// seeking backwards through the HTTP source.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_alac_m4a_stream() {
    let server = MockServer::start().await;
    Mock::given(path("/rest/stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/mp4")
                .set_body_bytes(include_bytes!("fixtures/alac.m4a").to_vec()),
        )
        .mount(&server)
        .await;

    let (player, mut events) = Player::new();
    player.set_volume(0.0);
    player.play(TrackSource {
        url: format!("{}/rest/stream", server.uri()),
        duration_hint: Some(Duration::from_millis(300)),
        path: None,
        id: None,
        live: false,
    });

    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event.expect("event channel closed early") {
                    Event::TrackEnded { .. } => break,
                    Event::Failed(msg) => {
                        if msg.contains("audio output unavailable") {
                            eprintln!("skipping: no audio device ({msg})");
                            return;
                        }
                        panic!("ALAC playback failed: {msg}");
                    }
                    _ => {}
                }
            }
            _ = &mut deadline => panic!("timed out waiting for TrackEnded"),
        }
    }
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
        path: None,
        id: None,
        live: false,
    });
    player.prefetch_next(TrackSource {
        url: format!("{}/rest/stream?id=2", server.uri()),
        duration_hint: Some(Duration::from_millis(500)),
        path: None,
        id: Some("2".into()),
        live: false,
    });

    let mut ends = Vec::new();
    let mut end_times = Vec::new();
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event.expect("event channel closed early") {
                    Event::TrackEnded { auto_advanced, started } => {
                        ends.push((auto_advanced, started));
                        end_times.push(std::time::Instant::now());
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
    // First end transitions into the prefetched track (reporting which one);
    // second is a plain end.
    assert_eq!(ends, vec![(true, Some("2".to_string())), (false, None)]);
    // Coarse gapless guard: the second track is 500ms of audio, so the two ends
    // must be about that far apart. Re-opening a player between them (the
    // non-gapless path) adds the poll interval plus source setup on top.
    let between = end_times[1] - end_times[0];
    assert!(
        between < Duration::from_millis(800),
        "transition was not gapless: {between:?} between track ends"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plays_local_wav_to_completion() {
    // Write a temporary WAV file.
    let dir = std::env::temp_dir().join("scire-test-local");
    let _ = std::fs::create_dir_all(&dir);
    let wav_path = dir.join("test.wav");
    let mut f = std::fs::File::create(&wav_path).unwrap();
    f.write_all(&wav_bytes()).unwrap();
    drop(f);

    let (player, mut events) = Player::new();
    player.set_volume(0.0); // silent test run
    player.play(TrackSource {
        url: String::new(), // unused when path is set
        duration_hint: Some(Duration::from_millis(500)),
        path: Some(wav_path.clone()),
        id: None,
        live: false,
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
                        if msg.contains("audio output unavailable") {
                            eprintln!("skipping: no audio device ({msg})");
                            let _ = std::fs::remove_file(&wav_path);
                            return;
                        }
                        panic!("local playback failed: {msg}");
                    }
                    _ => {}
                }
            }
            _ = &mut deadline => panic!("timed out waiting for TrackEnded"),
        }
    }
    assert!(saw_playing, "never saw Playing event");

    let _ = std::fs::remove_file(&wav_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_file_missing_errors() {
    let (player, mut events) = Player::new();
    player.set_volume(0.0);
    player.play(TrackSource {
        url: String::new(),
        duration_hint: None,
        path: Some(PathBuf::from("/nonexistent/test.wav")),
        id: None,
        live: false,
    });

    let deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            event = events.recv() => {
                match event.expect("event channel closed early") {
                    Event::Failed(msg) => {
                        assert!(msg.contains("local"), "unexpected error: {msg}");
                        return;
                    }
                    Event::Playing => panic!("should not play a non-existent file"),
                    _ => {}
                }
            }
            _ = &mut deadline => panic!("timed out waiting for Failed event"),
        }
    }
}

/// A one-shot HTTP server that answers every request with `body`, optionally
/// without a `Content-Length` (chunked), and reports the request headers it
/// saw. wiremock always sets a length, and the length is exactly what decides
/// whether an m4a can be read.
fn raw_server(
    body: Vec<u8>,
    content_type: &'static str,
    chunked: bool,
) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, BufReader};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line.trim_end().is_empty() {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            let _ = tx.send(headers);

            let mut head = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n");
            if chunked {
                head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            } else {
                head.push_str(&format!(
                    "Accept-Ranges: bytes\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                ));
            }
            if stream.write_all(head.as_bytes()).is_err() {
                continue;
            }
            if chunked {
                for chunk in body.chunks(16 * 1024) {
                    if stream
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .is_err()
                        || stream.write_all(chunk).is_err()
                        || stream.write_all(b"\r\n").is_err()
                    {
                        break;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n");
            } else {
                let _ = stream.write_all(&body);
            }
        }
    });
    (format!("http://{addr}/rest/stream"), rx)
}

/// An m4a keeps its index (`moov`) after the audio unless it was written with
/// faststart, so reading one needs seeking, which needs a length. Served
/// without one the decode cannot work — and the message has to say why, since
/// rodio reports every unsupported container the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_without_content_length_reports_the_container() {
    let (url, _headers) = raw_server(
        include_bytes!("fixtures/alac.m4a").to_vec(),
        "audio/mp4",
        true,
    );

    let (player, mut events) = Player::new();
    player.set_volume(0.0);
    player.play(TrackSource {
        url,
        duration_hint: Some(Duration::from_millis(300)),
        path: None,
        id: None,
        live: false,
    });

    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = events.recv() => match event.expect("event channel closed early") {
                Event::Failed(msg) => {
                    assert!(
                        msg.contains("m4a") && msg.contains("no length"),
                        "unhelpful failure message: {msg}"
                    );
                    break;
                }
                Event::Playing => panic!("expected the decode to fail"),
                _ => {}
            },
            _ = &mut deadline => panic!("timed out waiting for Failed"),
        }
    }
}

/// `Icy-MetaData: 1` asks the server to interleave now-playing titles into the
/// audio. A server that honours it on a library file answers without a length,
/// which is exactly what breaks the previous test — so library tracks must not
/// send the header at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_tracks_do_not_request_icy_metadata() {
    let (url, headers) = raw_server(wav_bytes(), "audio/wav", false);

    let (player, mut events) = Player::new();
    player.set_volume(0.0);
    player.play(TrackSource {
        url,
        duration_hint: Some(Duration::from_millis(500)),
        path: None,
        id: None,
        live: false,
    });

    let seen = headers
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("no request reached the server");
    assert!(
        !seen.contains("icy-metadata"),
        "library track asked for ICY metadata:\n{seen}"
    );
    drop(events.recv());
    player.stop();
}
