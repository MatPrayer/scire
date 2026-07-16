use reqwest::Url;
use serde::Deserialize;

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
