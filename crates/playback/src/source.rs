//! HTTP source construction: stream-download → blocking Read+Seek reader.

use stream_download::http::HttpStream;
use stream_download::http::reqwest::Client;
use stream_download::source::SourceStream;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};

use crate::PlaybackError;

/// Reader type handed to the rodio decoder.
pub(crate) type StreamReader = StreamDownload<TempStorageProvider>;

/// Open an HTTP stream for `url`, downloading in the background to a temp
/// file. The returned reader implements `Read + Seek`; seeks into
/// not-yet-downloaded regions trigger ranged re-requests.
pub(crate) async fn open(url: &str) -> Result<(StreamReader, Option<u64>), PlaybackError> {
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

    Ok((reader, content_length))
}
