//! Source construction: HTTP stream-download or local file → blocking
//! Read+Seek reader for rodio.

use std::io::{Read, Seek};
use std::path::Path;

use stream_download::http::HttpStream;
use stream_download::http::reqwest::Client;
use stream_download::source::SourceStream;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};

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
