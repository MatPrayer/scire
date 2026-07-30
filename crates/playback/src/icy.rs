//! ICY (SHOUTcast/Icecast) metadata.
//!
//! A live stream carries two kinds of information a file does not: station
//! details in the response headers, and the currently-playing track, which the
//! server interleaves into the audio itself. Asking for the latter (the
//! `Icy-MetaData: 1` request header) makes the server insert a length-prefixed
//! metadata block every `icy-metaint` bytes — bytes the decoder must never see,
//! so they are stripped here on the way out of the reader.

use std::io::{self, Read};

use tokio::sync::mpsc;

use crate::Event;

/// Station details from the ICY response headers, known as soon as the stream
/// opens. Everything is optional: stations advertise what they feel like.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StationInfo {
    /// `icy-name` — the station's own name, usually better than the one in the
    /// user's bookmark.
    pub name: Option<String>,
    pub genre: Option<String>,
    /// `icy-br`, in kbps.
    pub bitrate: Option<u32>,
    /// Codec inferred from the content type ("MP3", "AAC", …).
    pub format: Option<String>,
}

impl StationInfo {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Reader wrapper that removes the interleaved metadata blocks and reports
/// every new title on the event channel.
///
/// Reads are clamped to the bytes remaining before the next block, so the
/// decoder sees an unbroken audio stream and the block boundary is never
/// straddled.
pub(crate) struct IcyStrip<R> {
    inner: R,
    metaint: usize,
    /// Audio bytes left before the next metadata block.
    until_meta: usize,
    tx: mpsc::UnboundedSender<Event>,
    last: Option<String>,
}

impl<R: Read> IcyStrip<R> {
    pub(crate) fn new(inner: R, metaint: usize, tx: mpsc::UnboundedSender<Event>) -> Self {
        Self {
            inner,
            metaint,
            until_meta: metaint,
            tx,
            last: None,
        }
    }

    /// Read and discard one metadata block, publishing its title if it changed.
    fn consume_metadata(&mut self) -> io::Result<()> {
        // Length is given in 16-byte units; zero (the common case, since the
        // title only changes every few minutes) means no block at all.
        let mut len = [0u8; 1];
        self.inner.read_exact(&mut len)?;
        let size = len[0] as usize * 16;
        if size > 0 {
            let mut block = vec![0u8; size];
            self.inner.read_exact(&mut block)?;
            let title = parse_title(&block);
            if title != self.last {
                self.last = title.clone();
                let _ = self.tx.send(Event::StreamTitle(title));
            }
        }
        self.until_meta = self.metaint;
        Ok(())
    }
}

impl<R: Read> Read for IcyStrip<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.until_meta == 0 {
            self.consume_metadata()?;
        }
        let cap = buf.len().min(self.until_meta);
        let read = self.inner.read(&mut buf[..cap])?;
        self.until_meta -= read;
        Ok(read)
    }
}

/// Extract `StreamTitle` from a metadata block. The block is
/// `StreamTitle='…';StreamUrl='…';` padded with NULs, in no declared encoding —
/// UTF-8 in practice, so anything else is replaced rather than rejected.
fn parse_title(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let text = text.trim_end_matches('\0');
    let start = text.find("StreamTitle=")? + "StreamTitle=".len();
    let rest = text[start..].strip_prefix('\'')?;
    // Titles do contain apostrophes, so the terminator is `';`, with a bare
    // closing quote as the fallback for stations that omit the semicolon.
    let end = rest
        .find("';")
        .or_else(|| rest.rfind('\''))
        .unwrap_or(rest.len());
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// Codec name for an ICY content type, for display next to the bitrate.
pub(crate) fn format_label(subtype: &str) -> Option<String> {
    Some(
        match subtype {
            "mpeg" | "mp3" => "MP3",
            "aac" | "aacp" => "AAC",
            "ogg" | "vorbis" => "OGG",
            "opus" => "Opus",
            "flac" | "x-flac" => "FLAC",
            _ => return None,
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> Vec<u8> {
        let mut b = text.as_bytes().to_vec();
        b.resize(b.len().div_ceil(16) * 16, 0);
        b
    }

    #[test]
    fn parses_stream_title() {
        assert_eq!(
            parse_title(&block("StreamTitle='Aphex Twin - Xtal';StreamUrl='';")),
            Some("Aphex Twin - Xtal".into())
        );
    }

    #[test]
    fn title_with_apostrophe_survives() {
        assert_eq!(
            parse_title(&block(
                "StreamTitle='Guns N' Roses - Don't Cry';StreamUrl='';"
            )),
            Some("Guns N' Roses - Don't Cry".into())
        );
    }

    #[test]
    fn empty_or_absent_title_is_none() {
        assert_eq!(parse_title(&block("StreamTitle='';")), None);
        assert_eq!(parse_title(&block("StreamUrl='http://x';")), None);
    }

    /// The decoder must receive the audio bytes and nothing else, across
    /// several blocks and with reads that do not line up with `metaint`.
    #[test]
    fn strips_blocks_and_reports_titles() {
        let metaint = 32usize;
        let mut raw = Vec::new();
        let mut expected = Vec::new();
        for chunk in 0..3u8 {
            let audio = vec![chunk + 1; metaint];
            expected.extend_from_slice(&audio);
            raw.extend_from_slice(&audio);
            let meta = block(&format!("StreamTitle='track {chunk}';"));
            raw.push((meta.len() / 16) as u8);
            raw.extend_from_slice(&meta);
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reader = IcyStrip::new(io::Cursor::new(raw), metaint, tx);

        let mut out = Vec::new();
        let mut buf = [0u8; 7]; // deliberately not a divisor of metaint
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, expected);

        let mut titles = Vec::new();
        while let Ok(Event::StreamTitle(t)) = rx.try_recv() {
            titles.push(t);
        }
        assert_eq!(
            titles,
            vec![
                Some("track 0".into()),
                Some("track 1".into()),
                Some("track 2".into())
            ]
        );
    }

    /// A station that sends no metadata for a while emits zero-length blocks;
    /// those must not be mistaken for a cleared title.
    #[test]
    fn zero_length_blocks_are_transparent() {
        const METAINT: usize = 16;
        let mut raw = vec![9u8; METAINT];
        raw.push(0);
        raw.extend_from_slice(&[9u8; METAINT]);
        raw.push(0);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reader = IcyStrip::new(io::Cursor::new(raw), METAINT, tx);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).ok();
        assert_eq!(out, vec![9u8; METAINT * 2]);
        assert!(rx.try_recv().is_err());
    }
}
