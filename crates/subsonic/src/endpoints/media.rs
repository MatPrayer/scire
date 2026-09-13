use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::client::SubsonicClient;
use crate::error::Error;

/// Unsynced lyrics from getLyrics. `value` is None when the server has no
/// lyrics for the song.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LyricsWrapper {
    #[serde(default)]
    lyrics: Lyrics,
}

/// One line of a lyrics document. `start` is milliseconds from the start of
/// the song, and is absent for unsynced lyrics.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub start: Option<i64>,
    #[serde(default)]
    pub value: String,
}

/// One lyrics document for a song, from getLyricsBySongId (OpenSubsonic
/// `songLyrics`). A song can carry several — different languages, and a synced
/// and an unsynced copy of the same words.
///
/// `Serialize` as well, because the app's online-lookup fallback caches what it
/// found on disk in this same shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredLyrics {
    pub display_artist: Option<String>,
    pub display_title: Option<String>,
    pub lang: Option<String>,
    /// Milliseconds to add to every `start` before displaying.
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub synced: bool,
    #[serde(default, rename = "line")]
    pub lines: Vec<LyricLine>,
}

impl StructuredLyrics {
    /// The whole document as plain text, one line per entry.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.value.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsListWrapper {
    #[serde(default)]
    lyrics_list: LyricsList,
}

/// Navidrome answers `"lyricsList":{}` — with no `structuredLyrics` key at all
/// — for a song it has no lyrics for, so both levels default.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsList {
    #[serde(default)]
    structured_lyrics: Vec<StructuredLyrics>,
}

/// Options for building a stream URL.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Target format (e.g. "mp3", "opus", "raw"). None = server default.
    pub format: Option<String>,
    /// Max bitrate in kbps for transcoding. None = no cap.
    pub max_bit_rate: Option<u32>,
}

impl SubsonicClient {
    /// Authenticated URL for streaming a song. The playback layer fetches it.
    pub fn stream_url(&self, id: &str, opts: &StreamOptions) -> Result<Url, Error> {
        let mut params: Vec<(&str, &str)> = vec![("id", id)];
        let mbr;
        if let Some(fmt) = &opts.format {
            params.push(("format", fmt));
        }
        if let Some(rate) = opts.max_bit_rate {
            mbr = rate.to_string();
            params.push(("maxBitRate", &mbr));
        }
        self.build_url("stream", &params)
    }

    /// Authenticated URL for cover art, optionally scaled to `size` px.
    pub fn cover_art_url(&self, id: &str, size: Option<u32>) -> Result<Url, Error> {
        let mut params: Vec<(&str, &str)> = vec![("id", id)];
        let s;
        if let Some(px) = size {
            s = px.to_string();
            params.push(("size", &s));
        }
        self.build_url("getCoverArt", &params)
    }

    /// Song lyrics looked up by artist/title (classic Subsonic getLyrics).
    pub async fn get_lyrics(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
    ) -> Result<Lyrics, Error> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(a) = artist {
            params.push(("artist", a));
        }
        if let Some(t) = title {
            params.push(("title", t));
        }
        let w: LyricsWrapper = self.get("getLyrics", &params).await?;
        Ok(w.lyrics)
    }

    /// Song lyrics by id (OpenSubsonic `songLyrics` extension).
    ///
    /// This is the one worth asking. Navidrome collects lyrics at scan time
    /// from the file's tags *and* from a sidecar `.lrc`, and only exposes what
    /// it found under the song's own id; classic `getLyrics` searches the
    /// library by artist/title instead, so anything whose tags don't match the
    /// query exactly comes back empty. An empty list means "no lyrics", and a
    /// server without the extension answers error 70 (not found).
    pub async fn get_lyrics_by_song_id(&self, id: &str) -> Result<Vec<StructuredLyrics>, Error> {
        let w: LyricsListWrapper = self.get("getLyricsBySongId", &[("id", id)]).await?;
        Ok(w.lyrics_list.structured_lyrics)
    }

    /// Report playback to the server.
    ///
    /// `submission=false` = "now playing"; `submission=true` = played scrobble
    /// (drives play counts and server-side ListenBrainz/Last.fm forwarding).
    pub async fn scrobble(&self, id: &str, submission: bool) -> Result<(), Error> {
        let sub = if submission { "true" } else { "false" };
        self.get_empty("scrobble", &[("id", id), ("submission", sub)])
            .await
    }
}
