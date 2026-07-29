//! Source construction: HTTP stream-download or local file → blocking
//! Read+Seek reader for rodio.

use std::io::{Read, Seek};
use std::path::Path;

use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use stream_download::http::HttpStream;
use stream_download::http::reqwest::Client;
use stream_download::source::SourceStream;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::sync::mpsc;

use crate::PlaybackError;

/// Reader type handed to the rodio decoder. Either an HTTP stream
/// downloading to a temp file, or a plain local file.
pub(crate) enum SourceReader {
    Http(StreamDownload<TempStorageProvider>),
    Local(std::fs::File),
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            SourceReader::Http(r) => r.read(buf),
            SourceReader::Local(r) => r.read(buf),
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            SourceReader::Http(r) => r.seek(pos),
            SourceReader::Local(r) => r.seek(pos),
        }
    }
}

/// Open an HTTP stream for `url`, downloading in the background to a temp
/// file. The returned reader implements `Read + Seek`; seeks into
/// not-yet-downloaded regions trigger ranged re-requests.
pub(crate) async fn open(url: &str) -> Result<(SourceReader, Option<u64>), PlaybackError> {
    let stream = HttpStream::<Client>::create(
        url.parse()
            .map_err(|e| PlaybackError(format!("bad url: {e}")))?,
    )
    .await
    .map_err(|e| PlaybackError(e.to_string()))?;

    let content_length = stream.content_length();

    let reader =
        StreamDownload::from_stream(stream, TempStorageProvider::new(), Settings::default())
            .await
            .map_err(|e| PlaybackError(e.to_string()))?;

    Ok((SourceReader::Http(reader), content_length))
}

/// Open a local file, returning a `SourceReader::Local`. The byte length is
/// always known for local files.
pub(crate) async fn open_local(path: &Path) -> Result<(SourceReader, Option<u64>), PlaybackError> {
    let file = std::fs::File::open(path)
        .map_err(|e| PlaybackError(format!("open local file: {e}")))?;
    let len = file
        .metadata()
        .ok()
        .map(|m| m.len());
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
