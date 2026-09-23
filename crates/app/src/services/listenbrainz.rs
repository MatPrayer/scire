//! ListenBrainz submissions for tracks played directly from local files.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::Serialize;

const API: &str = "https://api.listenbrainz.org/1/submit-listens";
const UA: &str = concat!(
    "scire/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/LanaMirko04/scire)"
);

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

#[derive(Debug, Clone)]
pub struct ListenBrainzListen {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    /// Track duration in seconds.
    pub duration: Option<u32>,
}

#[derive(Serialize)]
struct Submission<'a> {
    listen_type: &'static str,
    payload: [Payload<'a>; 1],
}

#[derive(Serialize)]
struct Payload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    listened_at: Option<u64>,
    track_metadata: TrackMetadata<'a>,
}

#[derive(Serialize)]
struct TrackMetadata<'a> {
    artist_name: &'a str,
    track_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_info: Option<AdditionalInfo>,
}

#[derive(Serialize)]
struct AdditionalInfo {
    duration_ms: u64,
}

/// Submit now-playing (`submission=false`) or completed-listen metadata.
///
/// Empty artist/title metadata cannot form a valid ListenBrainz listen, so it
/// is ignored without making a request.
pub async fn submit_listen(
    token: &str,
    listen: ListenBrainzListen,
    submission: bool,
) -> Result<()> {
    let artist = listen.artist.trim();
    let title = listen.title.trim();
    let token = token.trim();
    if artist.is_empty() || title.is_empty() || token.is_empty() {
        return Ok(());
    }
    let listened_at = submission.then(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });
    let body = Submission {
        listen_type: if submission { "single" } else { "playing_now" },
        payload: [Payload {
            listened_at,
            track_metadata: TrackMetadata {
                artist_name: artist,
                track_name: title,
                release_name: listen
                    .album
                    .as_deref()
                    .filter(|album| !album.trim().is_empty()),
                additional_info: listen.duration.map(|seconds| AdditionalInfo {
                    duration_ms: u64::from(seconds) * 1_000,
                }),
            },
        }],
    };

    http()
        .post(API)
        .header("Authorization", format!("Token {token}"))
        .json(&body)
        .send()
        .await
        .context("ListenBrainz submit")?
        .error_for_status()
        .context("ListenBrainz submit")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_listen_payload_has_timestamp_and_duration() {
        let listen = ListenBrainzListen {
            artist: "Artist".into(),
            title: "Track".into(),
            album: Some("Album".into()),
            duration: Some(123),
        };
        let body = Submission {
            listen_type: "single",
            payload: [Payload {
                listened_at: Some(42),
                track_metadata: TrackMetadata {
                    artist_name: &listen.artist,
                    track_name: &listen.title,
                    release_name: listen.album.as_deref(),
                    additional_info: listen.duration.map(|seconds| AdditionalInfo {
                        duration_ms: u64::from(seconds) * 1_000,
                    }),
                },
            }],
        };
        let json = serde_json::to_value(body).unwrap();
        assert_eq!(json["listen_type"], "single");
        assert_eq!(json["payload"][0]["listened_at"], 42);
        assert_eq!(
            json["payload"][0]["track_metadata"]["additional_info"]["duration_ms"],
            123_000
        );
    }
}
