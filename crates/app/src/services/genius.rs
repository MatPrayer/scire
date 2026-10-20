//! Song credits, samples and descriptions from [Genius](https://genius.com).
//!
//! Tags rarely say who produced, mixed or mastered a song, or what it samples;
//! Genius usually does. The song details dialog asks here when it opens (never
//! in the background) and merges the answer into its Credits tab.
//!
//! Only the official API (`api.genius.com`), with a client access token the
//! user pastes into Settings — sent as an `Authorization` header, never in a
//! URL. Without a token nothing is asked: the website's own JSON API sits
//! behind a bot check that refuses non-browser clients. A rejected token or a
//! bot-check page is reported (and not cached), never worked around.
//!
//! Matching is strict — a confident title and artist match or nothing — since
//! another song's producers on this one are worse than no credits. Answers
//! are cached on disk like `album_info`'s: hits for 30 days, misses for 7.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::services::album_info::{cached, retry_delay, store};

const API: &str = "https://api.genius.com";

const UA: &str = concat!(
    "scire/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/MatPrayer/scire)"
);

const CACHE_VERSION: u32 = 2;

/// Gap kept between any two Genius requests from the process.
const SPACING: Duration = Duration::from_millis(300);

/// What the dialog asks about.
#[derive(Debug, Clone)]
pub struct Query {
    pub title: String,
    /// Main artists, lead first.
    pub artists: Vec<String>,
}

impl Query {
    fn cache_key(&self) -> String {
        let mut h = DefaultHasher::new();
        // Bumped when `GeniusSong` gains fields, so cached answers are asked
        // again instead of showing without them for a month.
        CACHE_VERSION.hash(&mut h);
        normalize(&self.title).hash(&mut h);
        for a in &self.artists {
            normalize(a).hash(&mut h);
        }
        format!("{:016x}", h.finish())
    }
}

/// Everything the Credits tab takes from Genius. `default` so answers cached
/// before a field existed still read (the field is simply empty).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeniusSong {
    pub url: String,
    pub title: String,
    pub artist: String,
    pub description: Option<String>,
    /// The About text was written or confirmed by the artist.
    pub description_verified: bool,
    pub album: Option<String>,
    pub album_url: Option<String>,
    /// "July 31, 2007" — as Genius words it, often more exact than the tags.
    pub release_date: Option<String>,
    pub recording_location: Option<String>,
    /// Language name ("English"), from Genius's ISO code.
    pub language: Option<String>,
    /// Genre tags, primary first ("Rap", "Italian Rap").
    pub tags: Vec<String>,
    pub pageviews: Option<u64>,
    /// On Genius's trending list right now.
    pub hot: bool,
    /// Streaming links in [`MediaLink::ORDER`].
    pub media: Vec<MediaLink>,
    /// The song has a music video on Genius (its YouTube link is the video).
    pub has_video: bool,
    /// Artists who verified lyrics or annotations on the page.
    pub verified_by: Vec<String>,
    /// `(role, people)` in Genius's order: Featuring, Producer, Writer, then
    /// every custom performance ("Mixing Engineer", "Label", "Guitar", …).
    pub credits: Vec<(String, Vec<GeniusPerson>)>,
    pub relations: Vec<Relation>,
}

/// Where else the song can be heard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaLink {
    /// "Spotify", "YouTube", "SoundCloud", "Apple Music".
    pub service: String,
    pub url: String,
}

impl MediaLink {
    pub const ORDER: [&str; 4] = ["Spotify", "Apple Music", "YouTube", "SoundCloud"];
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GeniusPerson {
    pub name: String,
    pub url: Option<String>,
}

/// "Samples", "Sampled in", "Cover of", … with the songs on the other end.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub label: String,
    pub songs: Vec<RelatedSong>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RelatedSong {
    pub title: String,
    pub artist: String,
    pub url: Option<String>,
}

/// One lookup's outcome.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub song: Option<GeniusSong>,
    /// Genius refused the token, in its own words ("invalid_token: …").
    pub token_rejected: Option<String>,
    /// A token was sent, and Genius's bot check turned the request away
    /// before it was looked at (an HTML 403, not an API answer).
    pub blocked: bool,
}

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

fn cache_path(query: &Query) -> Option<PathBuf> {
    Some(
        config::genius_cache_dir()
            .ok()?
            .join(format!("{}.json", query.cache_key())),
    )
}

/// Look the song up, through the disk cache, with the user's API token.
/// Must run inside the tokio runtime.
pub async fn fetch(query: Query, token: &str) -> Result<Answer> {
    let path = cache_path(&query);
    if let Some(hit) = cached::<GeniusSong>(path.as_ref()) {
        return Ok(Answer {
            song: hit,
            ..Answer::default()
        });
    }
    let mut answer = Answer::default();
    let song = match lookup(&query, token.trim()).await {
        Err(e) if e.downcast_ref::<AuthError>().is_some() => {
            tracing::warn!("genius: {e}");
            answer.token_rejected = e.downcast_ref::<AuthError>().map(|a| a.0.clone());
            return Ok(answer);
        }
        Err(e) if is_blocked(&e) => {
            answer.blocked = true;
            return Ok(answer);
        }
        other => other?,
    };
    store(path.as_ref(), &song);
    answer.song = song;
    Ok(answer)
}

/// Genius answered a token request with an API error, carrying its message.
#[derive(Debug)]
struct AuthError(String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Genius refused the API token ({})", self.0)
    }
}

impl std::error::Error for AuthError {}

/// The message in a Genius API error body: `meta.message`, or OAuth's
/// `error` / `error_description`. `None` when the body is not API JSON —
/// a bot-check page, say.
pub fn api_error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let message = v["meta"]["message"]
        .as_str()
        .or_else(|| v["error_description"].as_str())
        .or_else(|| v["error"].as_str())
        .unwrap_or("no reason given");
    Some(message.to_string())
}

/// Genius's bot check turned us away (an HTML 403, not an API answer).
#[derive(Debug)]
struct Blocked;

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Genius's bot check refused the request")
    }
}

impl std::error::Error for Blocked {}

fn is_blocked(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Blocked>().is_some()
}

async fn lookup(query: &Query, token: &str) -> Result<Option<GeniusSong>> {
    let Some(lead) = query.artists.first() else {
        return Ok(None);
    };
    let core = core_title(&query.title);
    let mut tried = Vec::new();
    for q in [format!("{core} {lead}"), format!("{} {lead}", query.title)] {
        if tried.contains(&q) {
            continue;
        }
        let body = get(token, "/search", &[("q", q.as_str()), ("per_page", "10")]).await?;
        tried.push(q);
        let Some(body) = body else { continue };
        let hits = parse_hits(&body);
        if let Some(id) = pick_hit(&hits, &query.title, &query.artists) {
            let song = get(token, &format!("/songs/{id}"), &[("text_format", "plain")]).await?;
            return Ok(song.as_ref().and_then(parse_song));
        }
    }
    Ok(None)
}

/// One request, spaced after the last; 404 is `None`.
async fn get(token: &str, path: &str, params: &[(&str, &str)]) -> Result<Option<Value>> {
    static LAST: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
    let mut last = LAST.lock().await;
    if let Some(at) = *last {
        let wait = SPACING.saturating_sub(at.elapsed());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
    let url = format!("{API}{path}");
    let mut attempt = 0;
    let result = loop {
        let req = http().get(&url).query(params).bearer_auth(token);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => break Err(anyhow!(e.without_url())),
        };
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            break Ok(None);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // An API error is JSON; a bot check is an HTML page. Only the
            // first says anything about the token.
            let body = resp.text().await.unwrap_or_default();
            let api_message = api_error_message(&body);
            tracing::warn!(
                "genius {path}: {status}, {}",
                api_message
                    .as_deref()
                    .unwrap_or("not an API answer (bot check?)")
            );
            break Err(match api_message {
                Some(message) => anyhow::Error::new(AuthError(message)),
                None if status == reqwest::StatusCode::FORBIDDEN => anyhow::Error::new(Blocked),
                None => anyhow!("Genius answered {status}: no reason given"),
            });
        }
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok());
        if let Some(wait) = retry_delay(status, retry_after, attempt) {
            attempt += 1;
            tokio::time::sleep(wait).await;
            continue;
        }
        if !status.is_success() {
            break Err(anyhow!("Genius answered {status}"));
        }
        break resp
            .json::<Value>()
            .await
            .map(Some)
            .map_err(|e| anyhow!(e.without_url()));
    };
    *last = Some(Instant::now());
    result
}

/// One search result, as much as matching needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: u64,
    pub title: String,
    pub artist: String,
}

/// Song hits from a search answer's `response.hits[]`.
pub fn parse_hits(body: &Value) -> Vec<Hit> {
    body["response"]["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["type"].as_str().is_none_or(|t| t == "song"))
        .filter_map(|h| {
            let r = &h["result"];
            Some(Hit {
                id: r["id"].as_u64()?,
                title: r["title"].as_str()?.to_string(),
                artist: r["primary_artist"]["name"]
                    .as_str()
                    .or_else(|| r["artist_names"].as_str())?
                    .to_string(),
            })
        })
        .collect()
}

/// The hit that is this song, or none. The title must match exactly once
/// normalised, or match at its core (feat. clauses dropped) with the same
/// version brackets — so a different remix of the song is never taken. The
/// artist must be one of ours.
pub fn pick_hit(hits: &[Hit], title: &str, artists: &[String]) -> Option<u64> {
    let artist_ok = |hit: &Hit| {
        let theirs = normalize(&hit.artist);
        !theirs.is_empty()
            && artists.iter().any(|a| {
                let ours = normalize(a);
                !ours.is_empty()
                    && (ours == theirs
                        || (ours.chars().count() >= 3 && theirs.contains(&ours))
                        || (theirs.chars().count() >= 3 && ours.contains(&theirs)))
            })
    };
    let full = normalize(title);
    let core = normalize(&core_title(title));
    let versions = version_tags(title);
    hits.iter()
        .filter(|h| artist_ok(h))
        .find(|h| normalize(&h.title) == full)
        .or_else(|| {
            hits.iter().filter(|h| artist_ok(h)).find(|h| {
                normalize(&core_title(&h.title)) == core && version_tags(&h.title) == versions
            })
        })
        .map(|h| h.id)
}

/// The title with guest clauses removed: "(feat. X)", "[with X]", a bare
/// "feat. X" up to the next bracket.
fn core_title(title: &str) -> String {
    let mut out = String::new();
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(close) = rest[open..].find(close_char).map(|c| open + c) else {
            break;
        };
        let inner = rest[open + 1..close].trim().to_lowercase();
        out.push_str(&rest[..open]);
        if !is_guest_clause(&inner) {
            out.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    let lower = out.to_ascii_lowercase();
    let cut = [" feat. ", " feat ", " ft. ", " featuring "]
        .iter()
        .filter_map(|m| lower.find(m))
        .min();
    if let Some(cut) = cut
        && out.is_char_boundary(cut)
    {
        out.truncate(cut);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_guest_clause(inner: &str) -> bool {
    ["feat.", "feat ", "ft.", "ft ", "featuring ", "with "]
        .iter()
        .any(|m| inner.starts_with(m))
}

/// The non-guest brackets of a title, normalised: "(Remix)", "[Live]".
fn version_tags(title: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(close) = rest[open..].find(close_char).map(|c| open + c) else {
            break;
        };
        let inner = rest[open + 1..close].trim().to_lowercase();
        if !is_guest_clause(&inner) {
            out.push(normalize(&inner));
        }
        rest = &rest[close + 1..];
    }
    out
}

/// Lowercase alphanumerics only: "Don’t Stop (Remix)" and "dont stop remix"
/// compare equal.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// A `/songs/{id}` answer (both APIs share the shape).
pub fn parse_song(body: &Value) -> Option<GeniusSong> {
    let s = &body["response"]["song"];
    let url = s["url"].as_str()?.to_string();
    let people = |v: &Value| -> Vec<GeniusPerson> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                Some(GeniusPerson {
                    name: a["name"].as_str()?.trim().to_string(),
                    url: a["url"].as_str().map(str::to_string),
                })
            })
            .filter(|p| !p.name.is_empty())
            .collect()
    };
    let mut credits: Vec<(String, Vec<GeniusPerson>)> = Vec::new();
    for (role, key) in [
        ("Featuring", "featured_artists"),
        ("Producer", "producer_artists"),
        ("Writer", "writer_artists"),
    ] {
        let list = people(&s[key]);
        if !list.is_empty() {
            credits.push((role.to_string(), list));
        }
    }
    for perf in s["custom_performances"].as_array().into_iter().flatten() {
        let Some(label) = perf["label"]
            .as_str()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        else {
            continue;
        };
        let list = people(&perf["artists"]);
        if !list.is_empty() {
            credits.push((label.to_string(), list));
        }
    }

    let mut relations: Vec<Relation> = Vec::new();
    for rel in s["song_relationships"].as_array().into_iter().flatten() {
        let kind = rel["relationship_type"]
            .as_str()
            .or_else(|| rel["type"].as_str())
            .unwrap_or_default();
        let Some(label) = relation_label(kind) else {
            continue;
        };
        let songs: Vec<RelatedSong> = rel["songs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|song| {
                Some(RelatedSong {
                    title: song["title"].as_str()?.to_string(),
                    artist: song["primary_artist"]["name"]
                        .as_str()
                        .or_else(|| song["artist_names"].as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: song["url"].as_str().map(str::to_string),
                })
            })
            .collect();
        if !songs.is_empty() {
            relations.push(Relation {
                label: label.to_string(),
                songs,
            });
        }
    }
    let translations: Vec<RelatedSong> = s["translation_songs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(RelatedSong {
                title: text(&t["title"])?,
                artist: t["language"]
                    .as_str()
                    .and_then(language_name)
                    .unwrap_or_default(),
                url: text(&t["url"]),
            })
        })
        .collect();
    if !translations.is_empty() {
        relations.push(Relation {
            label: "Translations".into(),
            songs: translations,
        });
    }
    relations.sort_by_key(|r| relation_order(&r.label));

    // Genius writes "?" for a song nobody has described.
    let description = s["description"]["plain"]
        .as_str()
        .or_else(|| s["description"].as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "?")
        .map(str::to_string);

    Some(GeniusSong {
        url,
        title: s["title"].as_str().unwrap_or_default().to_string(),
        artist: s["primary_artist"]["name"]
            .as_str()
            .or_else(|| s["artist_names"].as_str())
            .unwrap_or_default()
            .to_string(),
        description,
        description_verified: description_verified(s),
        album: s["album"]["name"].as_str().map(str::to_string),
        album_url: s["album"]["url"].as_str().map(str::to_string),
        release_date: s["release_date_for_display"]
            .as_str()
            .or_else(|| s["release_date"].as_str())
            .map(str::to_string),
        recording_location: text(&s["recording_location"]),
        language: s["language"].as_str().and_then(language_name),
        tags: tags(&s["tags"]),
        pageviews: s["stats"]["pageviews"].as_u64(),
        hot: s["stats"]["hot"].as_bool().unwrap_or(false),
        media: media_links(s),
        has_video: s["featured_video"].as_bool().unwrap_or(false),
        verified_by: verified_by(s),
        credits,
        relations,
    })
}

/// A trimmed, non-empty string.
fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Genre tags, primary ones first, then Genius's order.
fn tags(v: &Value) -> Vec<String> {
    let mut list: Vec<(bool, String)> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["primary"].as_bool().unwrap_or(false), text(&t["name"])?)))
        .collect();
    list.sort_by_key(|(primary, _)| !primary);
    let mut out: Vec<String> = Vec::new();
    for (_, name) in list {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

/// `media[]` (Spotify, YouTube, SoundCloud) plus an Apple Music link built from
/// `apple_music_id`, one per service, in [`MediaLink::ORDER`].
fn media_links(s: &Value) -> Vec<MediaLink> {
    let mut out: Vec<MediaLink> = Vec::new();
    for m in s["media"].as_array().into_iter().flatten() {
        let Some(url) =
            text(&m["url"]).filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        else {
            continue;
        };
        let service = match m["provider"].as_str().unwrap_or_default() {
            "spotify" => "Spotify",
            "youtube" => "YouTube",
            "soundcloud" => "SoundCloud",
            "apple_music" => "Apple Music",
            _ => continue,
        };
        if !out.iter().any(|l| l.service == service) {
            out.push(MediaLink {
                service: service.into(),
                url,
            });
        }
    }
    let apple_id = s["apple_music_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| s["apple_music_id"].as_u64().map(|n| n.to_string()))
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()));
    if let Some(id) = apple_id
        && !out.iter().any(|l| l.service == "Apple Music")
    {
        out.push(MediaLink {
            service: "Apple Music".into(),
            url: format!("https://music.apple.com/song/{id}"),
        });
    }
    out.sort_by_key(|l| {
        MediaLink::ORDER
            .iter()
            .position(|s| *s == l.service)
            .unwrap_or(MediaLink::ORDER.len())
    });
    out
}

/// Whether the description annotation carries an artist's verification.
fn description_verified(s: &Value) -> bool {
    s["description_annotation"]["annotations"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|a| a["verified"].as_bool() == Some(true) || a["verified_by"].is_object())
}

/// Artists who verified something on the song's page, in Genius's order.
fn verified_by(s: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let contributors = s["verified_contributors"].as_array().into_iter().flatten();
    let names = contributors
        .filter_map(|c| text(&c["artist"]["name"]).or_else(|| text(&c["user"]["name"])))
        .chain(
            s["verified_annotations_by"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|u| text(&u["name"])),
        );
    for name in names {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

/// A language name for Genius's ISO 639-1 code; other codes as given.
fn language_name(code: &str) -> Option<String> {
    let code = code.trim();
    if code.is_empty() {
        return None;
    }
    let name = match code.to_ascii_lowercase().as_str() {
        "en" => "English",
        "it" => "Italian",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "ru" => "Russian",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "pl" => "Polish",
        "tr" => "Turkish",
        "ar" => "Arabic",
        "hi" => "Hindi",
        "fi" => "Finnish",
        "no" => "Norwegian",
        "da" => "Danish",
        "el" => "Greek",
        "he" => "Hebrew",
        "cs" => "Czech",
        "ro" => "Romanian",
        "hu" => "Hungarian",
        "uk" => "Ukrainian",
        "id" => "Indonesian",
        "tl" => "Tagalog",
        "sr" => "Serbian",
        "hr" => "Croatian",
        "instrumental" => "Instrumental",
        _ => return Some(code.to_string()),
    };
    Some(name.to_string())
}

const RELATIONS: [(&str, &str); 11] = [
    ("samples", "Samples"),
    ("sampled_in", "Sampled in"),
    ("interpolates", "Interpolates"),
    ("interpolated_by", "Interpolated by"),
    ("cover_of", "Cover of"),
    ("covered_by", "Covered by"),
    ("remix_of", "Remix of"),
    ("remixed_by", "Remixed by"),
    ("live_version_of", "Live version of"),
    ("performed_live_as", "Performed live as"),
    // Not a `song_relationships` kind: filled from `translation_songs`.
    ("translation_songs", "Translations"),
];

/// The heading for a Genius `relationship_type`; unknown kinds ("is the
/// original of", …) are left out. Translations come from their own field.
pub fn relation_label(kind: &str) -> Option<&'static str> {
    RELATIONS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l)
}

fn relation_order(label: &str) -> usize {
    RELATIONS
        .iter()
        .position(|(_, l)| *l == label)
        .unwrap_or(RELATIONS.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hit(id: u64, title: &str, artist: &str) -> serde_json::Value {
        json!({"type": "song", "result": {
            "id": id, "title": title, "full_title": format!("{title} by {artist}"),
            "primary_artist": {"id": 1, "name": artist}, "url": "https://genius.com/x"
        }})
    }

    #[test]
    fn hits_parse_from_a_search_answer() {
        let official = json!({"meta": {"status": 200}, "response": {"hits": [
            hit(1, "Hurt", "Nine Inch Nails"),
            {"type": "artist", "result": {"id": 9, "name": "NIN"}}
        ]}});
        assert_eq!(
            parse_hits(&official),
            [Hit {
                id: 1,
                title: "Hurt".into(),
                artist: "Nine Inch Nails".into()
            }]
        );
        assert!(parse_hits(&json!({})).is_empty());
    }

    fn hits(list: &[(u64, &str, &str)]) -> Vec<Hit> {
        list.iter()
            .map(|(id, t, a)| Hit {
                id: *id,
                title: (*t).into(),
                artist: (*a).into(),
            })
            .collect()
    }

    #[test]
    fn picks_only_a_confident_match() {
        let h = hits(&[
            (1, "Hurt", "Johnny Cash"),
            (2, "Hurt", "Nine Inch Nails"),
            (3, "Hurt (Quiet)", "Nine Inch Nails"),
        ]);
        assert_eq!(pick_hit(&h, "Hurt", &["Nine Inch Nails".into()]), Some(2));
        assert_eq!(pick_hit(&h, "Hurt", &["Christina Aguilera".into()]), None);
        // A guest clause on our side only is not a different song.
        let h = hits(&[(4, "CH4NGE", "Giga")]);
        assert_eq!(
            pick_hit(&h, "CH4NGE (feat. 可不)", &["Giga".into()]),
            Some(4)
        );
        // A different remix is.
        let h = hits(&[(5, "Cinema (Skrillex Remix)", "Benny Benassi")]);
        assert_eq!(
            pick_hit(&h, "Cinema (Virtual Riot remix)", &["Benny Benassi".into()]),
            None
        );
        let h = hits(&[(6, "Cinema (Virtual Riot Remix)", "Benny Benassi")]);
        assert_eq!(
            pick_hit(&h, "Cinema (Virtual Riot remix)", &["Benny Benassi".into()]),
            Some(6)
        );
        // Punctuation and case do not matter; a collaboration's display name
        // containing ours does.
        let h = hits(&[(7, "Don’t Stop", "Justice & RIMON")]);
        assert_eq!(pick_hit(&h, "Don't stop", &["Justice".into()]), Some(7));
    }

    #[test]
    fn core_title_drops_guest_clauses_only() {
        assert_eq!(core_title("CH4NGE (feat. 可不)"), "CH4NGE");
        assert_eq!(core_title("Song [with X] (Remix)"), "Song (Remix)");
        assert_eq!(core_title("Sheath-Winged feat. renko"), "Sheath-Winged");
    }

    #[test]
    fn song_parses_credits_relations_and_description() {
        let body = json!({"response": {"song": {
            "id": 1, "url": "https://genius.com/kanye-west-stronger-lyrics",
            "title": "Stronger", "primary_artist": {"name": "Kanye West"},
            "description": {"plain": "The lead single…"},
            "album": {"name": "Graduation"},
            "release_date_for_display": "July 31, 2007",
            "featured_artists": [],
            "producer_artists": [{"name": "Kanye West", "url": "u1"}, {"name": "Mike Dean", "url": "u2"}],
            "writer_artists": [{"name": "Thomas Bangalter"}],
            "custom_performances": [
                {"label": "Mixing Engineer", "artists": [{"name": "Andrew Dawson"}]},
                {"label": "Label", "artists": []}
            ],
            "song_relationships": [
                {"relationship_type": "translations", "songs": [{"title": "Stronger (Tradução)"}]},
                {"relationship_type": "sampled_in", "songs": [{"title": "X", "primary_artist": {"name": "Y"}}]},
                {"relationship_type": "samples", "songs": [
                    {"title": "Harder, Better, Faster, Stronger", "primary_artist": {"name": "Daft Punk"}, "url": "u3"}
                ]},
                {"relationship_type": "cover_of", "songs": []}
            ]
        }}});
        let song = parse_song(&body).unwrap();
        assert_eq!(song.description.as_deref(), Some("The lead single…"));
        let roles: Vec<_> = song.credits.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(roles, ["Producer", "Writer", "Mixing Engineer"]);
        assert_eq!(song.credits[0].1[1].name, "Mike Dean");
        let labels: Vec<_> = song.relations.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Samples", "Sampled in"]);
        assert_eq!(song.relations[0].songs[0].artist, "Daft Punk");
    }

    #[test]
    fn song_parses_facts_links_and_translations() {
        let body = json!({"response": {"song": {
            "url": "u", "title": "Stronger",
            "release_date": "2007-07-31",
            "recording_location": " Sony Music Studios, New York ",
            "language": "en",
            "tags": [
                {"name": "Pop", "primary": false},
                {"name": "Rap", "primary": true},
                {"name": "rap", "primary": false}
            ],
            "stats": {"pageviews": 1234567, "hot": true},
            "featured_video": true,
            "apple_music_id": "1442951245",
            "media": [
                {"provider": "youtube", "type": "video", "url": "https://www.youtube.com/watch?v=x"},
                {"provider": "spotify", "type": "audio", "url": "https://open.spotify.com/track/y"},
                {"provider": "napster", "url": "https://napster.com/z"},
                {"provider": "soundcloud", "url": "javascript:alert(1)"}
            ],
            "album": {"name": "Graduation", "url": "https://genius.com/albums/kanye-west/graduation"},
            "description_annotation": {"annotations": [{"verified": true}]},
            "verified_contributors": [{"artist": {"name": "Kanye West"}}],
            "verified_annotations_by": [{"name": "Kanye West"}, {"name": "Mike Dean"}],
            "translation_songs": [
                {"title": "Kanye West - Stronger (Tradução)", "language": "pt", "url": "t1"}
            ]
        }}});
        let song = parse_song(&body).unwrap();
        assert_eq!(song.release_date.as_deref(), Some("2007-07-31"));
        assert_eq!(
            song.recording_location.as_deref(),
            Some("Sony Music Studios, New York")
        );
        assert_eq!(song.language.as_deref(), Some("English"));
        assert_eq!(song.tags, ["Rap", "Pop"]);
        assert_eq!(song.pageviews, Some(1234567));
        assert!(song.hot && song.has_video && song.description_verified);
        let services: Vec<_> = song.media.iter().map(|m| m.service.as_str()).collect();
        assert_eq!(services, ["Spotify", "Apple Music", "YouTube"]);
        assert_eq!(song.media[1].url, "https://music.apple.com/song/1442951245");
        assert_eq!(song.verified_by, ["Kanye West", "Mike Dean"]);
        assert_eq!(
            song.album_url.as_deref(),
            Some("https://genius.com/albums/kanye-west/graduation")
        );
        let tr = song
            .relations
            .iter()
            .find(|r| r.label == "Translations")
            .unwrap();
        assert_eq!(tr.songs[0].artist, "Portuguese");
    }

    #[test]
    fn answers_cached_before_new_fields_still_read() {
        let old = r#"{"url":"u","title":"T","artist":"A","description":null,"album":null,"release_date":null,"credits":[],"relations":[]}"#;
        let song: GeniusSong = serde_json::from_str(old).unwrap();
        assert_eq!(song.url, "u");
        assert!(song.media.is_empty());
    }

    #[test]
    fn a_question_mark_is_no_description() {
        let body = json!({"response": {"song": {"url": "u", "description": {"plain": "?"}}}});
        assert_eq!(parse_song(&body).unwrap().description, None);
        assert!(parse_song(&json!({"response": {}})).is_none());
    }

    #[test]
    fn api_errors_are_told_from_bot_checks() {
        assert_eq!(
            api_error_message(
                r#"{"meta":{"status":401,"message":"This call requires an access_token."}}"#
            )
            .as_deref(),
            Some("This call requires an access_token.")
        );
        assert_eq!(
            api_error_message(r#"{"error":"invalid_token","error_description":"The access token provided is expired, revoked, malformed or invalid"}"#)
                .as_deref(),
            Some("The access token provided is expired, revoked, malformed or invalid")
        );
        assert_eq!(api_error_message("<!DOCTYPE html><html>…"), None);
    }

    #[test]
    fn relation_labels() {
        assert_eq!(relation_label("samples"), Some("Samples"));
        assert_eq!(relation_label("translations"), None);
        assert_eq!(relation_order("Translations"), RELATIONS.len() - 1);
    }
}
