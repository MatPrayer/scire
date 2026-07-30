//! Source construction: HTTP stream-download or local file → blocking
//! Read+Seek reader for rodio.

use std::io::{Read, Seek};
use std::path::Path;
use std::sync::OnceLock;

use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use stream_download::http::HttpStream;
use stream_download::http::reqwest::Client;
use stream_download::source::SourceStream;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::sync::mpsc;

use crate::icy::{IcyStrip, StationInfo};
use crate::{Event, PlaybackError};

/// Reader type handed to the rodio decoder: an HTTP stream downloading to a
/// temp file, the same with ICY metadata blocks filtered out, or a plain local
/// file.
pub(crate) enum SourceReader {
    Http(StreamDownload<TempStorageProvider>),
    Icy(IcyStrip<StreamDownload<TempStorageProvider>>),
    Local(std::fs::File),
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            SourceReader::Http(r) => r.read(buf),
            SourceReader::Icy(r) => r.read(buf),
            SourceReader::Local(r) => r.read(buf),
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            SourceReader::Http(r) => r.seek(pos),
            // Metadata blocks make byte offsets in the stream and byte offsets
            // in the audio two different things; a live stream is not seekable
            // in the first place, so there is nothing to reconcile.
            SourceReader::Icy(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "live stream is not seekable",
            )),
            SourceReader::Local(r) => r.seek(pos),
        }
    }
}

/// Prefetch for a track: the server sends it as fast as the link allows, so a
/// generous buffer costs nothing and protects against stalls.
const PREFETCH_FILE: u64 = 256 * 1024;
/// Prefetch for a live stream. A station sends in real time, so this is a
/// *duration*, not a size: 256KB of a 128kbps stream is sixteen seconds of
/// dead air before playback may begin. 32KB is a couple of seconds.
const PREFETCH_LIVE: u64 = 32 * 1024;
/// Cap on fetching a playlist before giving up and trying the URL as a stream.
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Streams worth trying for `url`, in order. Usually just the URL itself; a
/// playlist expands to everything it lists, because stations routinely offer
/// the same programme in several formats and the first is not always one this
/// engine can decode.
pub(crate) async fn stream_candidates(url: &str) -> Vec<String> {
    // Stations are hand-entered often enough that stray whitespace turns up in
    // the URL, where it fails to parse rather than being ignored.
    let url = url.trim();
    let entries = resolve_playlist(url).await;
    if entries.is_empty() {
        vec![url.to_string()]
    } else {
        entries
    }
}

/// Open an HTTP stream for `url`, downloading in the background to a temp
/// file. The returned reader implements `Read + Seek`; seeks into
/// not-yet-downloaded regions trigger ranged re-requests. `None` for the length
/// means a live stream (no `Content-Length`).
pub(crate) async fn open(
    url: &str,
    event_tx: &mpsc::UnboundedSender<Event>,
) -> Result<(SourceReader, Option<u64>, Option<StationInfo>), PlaybackError> {
    let stream = HttpStream::new(
        icy_client().clone(),
        url.parse()
            .map_err(|e| PlaybackError(format!("bad url: {e}")))?,
    )
    .await
    .map_err(|e| PlaybackError(e.to_string()))?;

    // Reject what the decoder cannot play *before* handing it over. HE-AAC
    // ("aacp") is the one that matters: symphonia implements AAC-LC only, and
    // on an endless stream it does not fail, it simply never produces a packet
    // — so without this check the engine sits on "buffering" forever instead
    // of moving to the station's next entry.
    if let Some(ct) = stream.content_type() {
        let subtype = ct.subtype.to_ascii_lowercase();
        if matches!(subtype.as_str(), "aacp" | "x-mpegurl" | "vnd.apple.mpegurl") {
            return Err(PlaybackError(format!(
                "unsupported stream format: {}/{}",
                ct.r#type, subtype
            )));
        }
    }

    let content_length = stream.content_length();
    let settings = Settings::default().prefetch_bytes(match content_length {
        Some(_) => PREFETCH_FILE,
        None => PREFETCH_LIVE,
    });

    // ICY details, present only on live streams. `icy-metaint` is the server
    // agreeing to interleave now-playing titles; without it there is nothing to
    // strip and the plain reader is used.
    let station = station_info(&stream, content_length.is_none());
    let metaint = stream
        .header("icy-metaint")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0);

    let reader = StreamDownload::from_stream(stream, TempStorageProvider::new(), settings)
        .await
        .map_err(|e| PlaybackError(e.to_string()))?;

    let reader = match metaint {
        Some(metaint) => SourceReader::Icy(IcyStrip::new(reader, metaint, event_tx.clone())),
        None => SourceReader::Http(reader),
    };
    Ok((reader, content_length, station))
}

/// Shared client that asks every server for ICY metadata. Servers that do not
/// speak ICY ignore the header, so it costs nothing to send it always — and
/// there is no way to know a URL is a radio station before opening it.
fn icy_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut headers = stream_download::http::reqwest::header::HeaderMap::new();
        headers.insert(
            "Icy-MetaData",
            stream_download::http::reqwest::header::HeaderValue::from_static("1"),
        );
        Client::builder()
            .default_headers(headers)
            .build()
            .unwrap_or_default()
    })
}

/// Station details advertised in the response headers, or `None` when this is
/// not a station. `live` (no `Content-Length`) is required because the codec
/// label alone would otherwise describe every library track as a station.
fn station_info(stream: &HttpStream<Client>, live: bool) -> Option<StationInfo> {
    if !live {
        return None;
    }
    let header = |name: &str| {
        stream
            .header(name)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let info = StationInfo {
        name: header("icy-name"),
        genre: header("icy-genre"),
        bitrate: header("icy-br").and_then(|v| v.parse().ok()),
        format: stream
            .content_type()
            .as_ref()
            .and_then(|ct| crate::icy::format_label(&ct.subtype.to_ascii_lowercase())),
    };
    (!info.is_empty()).then_some(info)
}

/// Stream URLs listed by a `.pls` or `.m3u` playlist, in order. Empty when
/// `url` is not a playlist, or cannot be read — in which case the caller just
/// tries the original URL.
///
/// HLS (`.m3u8`) is deliberately not followed: it is a segment index, not a
/// stream, and playing it needs a client this engine does not have.
async fn resolve_playlist(url: &str) -> Vec<String> {
    let lower = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if !(lower.ends_with(".pls") || lower.ends_with(".m3u")) {
        return Vec::new();
    }
    // Bounded: a playlist that never answers must not hold up playback.
    let fetch = async { Client::new().get(url).send().await.ok()?.text().await.ok() };
    let Ok(Some(body)) = tokio::time::timeout(PLAYLIST_TIMEOUT, fetch).await else {
        return Vec::new();
    };
    body.lines()
        .map(str::trim)
        // `.pls` lists entries as `File1=<url>`; `.m3u` is bare URLs with
        // `#EXTINF` comments in between.
        .filter_map(|line| match line.split_once('=') {
            Some((key, value)) if key.to_ascii_lowercase().starts_with("file") => Some(value),
            _ if !line.starts_with('#') && !line.is_empty() => Some(line),
            _ => None,
        })
        .map(str::trim)
        .filter(|target| target.starts_with("http"))
        .map(str::to_string)
        .collect()
}

/// Open a local file, returning a `SourceReader::Local`. The byte length is
/// always known for local files.
pub(crate) async fn open_local(path: &Path) -> Result<(SourceReader, Option<u64>), PlaybackError> {
    let file =
        std::fs::File::open(path).map_err(|e| PlaybackError(format!("open local file: {e}")))?;
    let len = file.metadata().ok().map(|m| m.len());
    Ok((SourceReader::Local(file), len))
}

/// Wraps a decoder so the engine learns the exact sample at which it runs out.
///
/// Gapless playback keeps several tracks queued inside one `rodio::Player`, so
/// polling `Player::empty()` can no longer tell tracks apart: this reports the
/// hand-over the instant rodio's queue pulls the last sample of `serial`.
pub(crate) struct EndSignal<S> {
    inner: S,
    serial: u64,
    /// Taken on the first exhaustion so the signal fires exactly once.
    tx: Option<mpsc::UnboundedSender<u64>>,
}

impl<S> EndSignal<S> {
    pub(crate) fn new(inner: S, serial: u64, tx: mpsc::UnboundedSender<u64>) -> Self {
        Self {
            inner,
            serial,
            tx: Some(tx),
        }
    }
}

impl<S: Source> Iterator for EndSignal<S> {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        let sample = self.inner.next();
        if sample.is_none()
            && let Some(tx) = self.tx.take()
        {
            let _ = tx.send(self.serial);
        }
        sample
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for EndSignal<S> {
    #[inline]
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    #[inline]
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    #[inline]
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    #[inline]
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}
