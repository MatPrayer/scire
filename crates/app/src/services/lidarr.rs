//! Lidarr v1 API client: the artist page's missing releases, their track
//! lists, automatic and interactive search, and the download queue the
//! sidebar's Lidarr page shows.
//!
//! The API key travels in the `X-Api-Key` header, never in the URL, so request
//! errors (which carry the URL) cannot leak it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const UA: &str = concat!(
    "scire/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/MatPrayer/scire)"
);

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
static SLOW_HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| client(Duration::from_secs(30)))
}

/// Interactive search: Lidarr asks every indexer before it answers and sends
/// nothing meanwhile, so the per-gap read timeout has to cover the whole
/// search. A minute is common on a few slow indexers.
fn slow_http() -> &'static reqwest::Client {
    SLOW_HTTP.get_or_init(|| client(Duration::from_secs(180)))
}

fn client(read_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(read_timeout)
        .build()
        .unwrap_or_default()
}

/// Connection details for one Lidarr instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lidarr {
    base: String,
    key: String,
}

impl Lidarr {
    /// `base` is what the user typed (`http://host:8686`, maybe with a URL
    /// base path); a missing scheme is taken as http.
    pub fn new(base: &str, key: &str) -> Option<Self> {
        let base = normalize_base(base)?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        Some(Self {
            base,
            key: key.to_string(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.base)
    }

    /// Absolute URL for a path Lidarr hands out relative to itself
    /// (`/MediaCover/…`), honouring the URL base.
    pub fn absolute(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/{}", self.base, path.trim_start_matches('/'))
        }
    }

    pub fn api_key(&self) -> &str {
        &self.key
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.get_with(http(), path, query).await
    }

    async fn get_with<T: DeserializeOwned>(
        &self,
        http: &reqwest::Client,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let response = http
            .get(self.url(path))
            .header("X-Api-Key", &self.key)
            .query(query)
            .send()
            .await
            .map_err(without_url)?;
        decode(response).await
    }

    async fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let response = http()
            .request(method, self.url(path))
            .header("X-Api-Key", &self.key)
            .json(body)
            .send()
            .await
            .map_err(without_url)?;
        decode(response).await
    }

    /// `GET /system/status` — the connection test.
    pub async fn status(&self) -> Result<SystemStatus> {
        self.get("system/status", &[]).await
    }

    /// The Lidarr artist for a MusicBrainz id, else the one whose name matches
    /// (case-insensitively, once). `None` = Lidarr doesn't track this artist.
    pub async fn find_artist(&self, mbid: Option<&str>, name: &str) -> Result<Option<Artist>> {
        if let Some(mbid) = mbid.filter(|m| !m.is_empty()) {
            let hits: Vec<Artist> = self.get("artist", &[("mbId", mbid.to_string())]).await?;
            if let Some(hit) = hits.into_iter().find(|a| a.foreign_artist_id == mbid) {
                return Ok(Some(hit));
            }
        }
        let all: Vec<Artist> = self.get("artist", &[]).await?;
        Ok(match_artist_by_name(all, name))
    }

    pub async fn albums(&self, artist_id: i64) -> Result<Vec<Album>> {
        self.get("album", &[("artistId", artist_id.to_string())])
            .await
    }

    pub async fn album(&self, album_id: i64) -> Result<Album> {
        self.get(&format!("album/{album_id}"), &[]).await
    }

    pub async fn tracks(&self, album_id: i64) -> Result<Vec<Track>> {
        let mut tracks: Vec<Track> = self
            .get("track", &[("albumId", album_id.to_string())])
            .await?;
        tracks.sort_by_key(|t| (t.medium_number, t.absolute_track_number));
        Ok(tracks)
    }

    /// Starts Lidarr's automatic search for the album (`AlbumSearch` command);
    /// it grabs the best approved release by itself.
    pub async fn search_album(&self, album_id: i64) -> Result<Command> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            name: &'static str,
            album_ids: [i64; 1],
        }
        self.send_json(
            reqwest::Method::POST,
            "command",
            &Body {
                name: "AlbumSearch",
                album_ids: [album_id],
            },
        )
        .await
    }

    /// Commands Lidarr is running or ran recently.
    pub async fn commands(&self) -> Result<Vec<Command>> {
        self.get("command", &[]).await
    }

    /// Interactive search: every indexer's releases for the album, with
    /// Lidarr's verdict on each. Slow — see [`slow_http`].
    pub async fn releases(&self, album_id: i64) -> Result<Vec<Release>> {
        let mut releases: Vec<Release> = self
            .get_with(slow_http(), "release", &[("albumId", album_id.to_string())])
            .await?;
        sort_releases(&mut releases);
        Ok(releases)
    }

    /// Sends one interactive-search result to the download client.
    pub async fn grab(&self, release: &Release) -> Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            guid: &'a str,
            indexer_id: i64,
        }
        let _: serde_json::Value = self
            .send_json(
                reqwest::Method::POST,
                "release",
                &Body {
                    guid: &release.guid,
                    indexer_id: release.indexer_id,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn set_monitored(&self, album_id: i64, monitored: bool) -> Result<()> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            album_ids: [i64; 1],
            monitored: bool,
        }
        let _: serde_json::Value = self
            .send_json(
                reqwest::Method::PUT,
                "album/monitor",
                &Body {
                    album_ids: [album_id],
                    monitored,
                },
            )
            .await?;
        forget_discographies();
        Ok(())
    }

    /// The download queue, newest first as Lidarr orders it.
    pub async fn queue(&self) -> Result<Vec<QueueItem>> {
        let page: QueuePage = self
            .get(
                "queue",
                &[
                    ("page", "1".into()),
                    ("pageSize", "200".into()),
                    ("includeArtist", "true".into()),
                    ("includeAlbum", "true".into()),
                ],
            )
            .await?;
        Ok(page.records)
    }

    /// Removes a queue entry, also from the download client.
    pub async fn remove_from_queue(&self, id: i64, blocklist: bool) -> Result<()> {
        let response = http()
            .delete(self.url(&format!("queue/{id}")))
            .header("X-Api-Key", &self.key)
            .query(&[
                ("removeFromClient", "true"),
                ("blocklist", if blocklist { "true" } else { "false" }),
            ])
            .send()
            .await
            .map_err(without_url)?;
        check(response).await.map(drop)
    }

    /// Albums Lidarr wants and has no files for, across every artist.
    pub async fn wanted_missing(&self) -> Result<(u64, Vec<Album>)> {
        let page: WantedPage = self
            .get(
                "wanted/missing",
                &[
                    ("page", "1".into()),
                    ("pageSize", "50".into()),
                    ("sortKey", "releaseDate".into()),
                    ("sortDirection", "descending".into()),
                    ("includeArtist", "true".into()),
                    ("monitored", "true".into()),
                ],
            )
            .await?;
        Ok((page.total_records, page.records))
    }

    /// Recent history (grabs, imports, failures), newest first.
    pub async fn history(&self) -> Result<Vec<HistoryItem>> {
        self.recent_history(30).await
    }

    /// The newest `size` history records.
    pub async fn recent_history(&self, size: u32) -> Result<Vec<HistoryItem>> {
        let page: HistoryPage = self
            .get(
                "history",
                &[
                    ("page", "1".into()),
                    ("pageSize", size.to_string()),
                    ("sortKey", "date".into()),
                    ("sortDirection", "descending".into()),
                    ("includeArtist", "true".into()),
                    ("includeAlbum", "true".into()),
                ],
            )
            .await?;
        Ok(page.records)
    }

    /// Lidarr's metadata search for artists (MusicBrainz, through Lidarr's
    /// metadata server). Hits Lidarr already tracks carry their id.
    pub async fn lookup_artists(&self, term: &str) -> Result<Vec<Lookup<Artist>>> {
        let raw: Vec<serde_json::Value> = self
            .get("artist/lookup", &[("term", term.to_string())])
            .await?;
        Ok(parse_lookups(raw))
    }

    /// Same for albums; each hit carries its artist.
    pub async fn lookup_albums(&self, term: &str) -> Result<Vec<Lookup<Album>>> {
        let raw: Vec<serde_json::Value> = self
            .get("album/lookup", &[("term", term.to_string())])
            .await?;
        Ok(parse_lookups(raw))
    }

    /// A tracked artist, raw record kept so it can be written back.
    pub async fn artist(&self, artist_id: i64) -> Result<Lookup<Artist>> {
        let raw: serde_json::Value = self.get(&format!("artist/{artist_id}"), &[]).await?;
        let item = serde_json::from_value(raw.clone()).context("unexpected artist record")?;
        Ok(Lookup { item, raw })
    }

    /// `PUT /artist/{id}` with the record as read and `monitored` changed.
    pub async fn set_artist_monitored(
        &self,
        artist: &Lookup<Artist>,
        monitored: bool,
    ) -> Result<Lookup<Artist>> {
        let mut body = artist.raw.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("monitored".into(), serde_json::json!(monitored));
        }
        let raw: serde_json::Value = self
            .send_json(
                reqwest::Method::PUT,
                &format!("artist/{}", artist.item.id),
                &body,
            )
            .await?;
        forget_discographies();
        let item = serde_json::from_value(raw.clone()).context("unexpected artist record")?;
        Ok(Lookup { item, raw })
    }

    /// Lidarr's search for every monitored album of the artist it lacks.
    pub async fn search_artist(&self, artist_id: i64) -> Result<Command> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            name: &'static str,
            artist_id: i64,
        }
        self.send_json(
            reqwest::Method::POST,
            "command",
            &Body {
                name: "ArtistSearch",
                artist_id,
            },
        )
        .await
    }

    pub async fn root_folders(&self) -> Result<Vec<RootFolder>> {
        self.get("rootfolder", &[]).await
    }

    pub async fn quality_profiles(&self) -> Result<Vec<Profile>> {
        self.get("qualityprofile", &[]).await
    }

    pub async fn metadata_profiles(&self) -> Result<Vec<Profile>> {
        self.get("metadataprofile", &[]).await
    }

    pub async fn add_artist(&self, artist: &Lookup<Artist>, opts: &AddOptions) -> Result<Artist> {
        let added = self
            .send_json(
                reqwest::Method::POST,
                "artist",
                &artist_body(&artist.raw, opts),
            )
            .await;
        forget_discographies();
        added
    }

    /// Adds the album, and its artist first when Lidarr has to.
    pub async fn add_album(&self, album: &Lookup<Album>, opts: &AddOptions) -> Result<Album> {
        let added = self
            .send_json(
                reqwest::Method::POST,
                "album",
                &album_body(&album.raw, opts),
            )
            .await;
        forget_discographies();
        added
    }

    /// The artist (see [`Lidarr::find_artist`]) and their albums, fresh from
    /// Lidarr; the answer is written to the disk cache
    /// [`cached_discography`] reads. `Ok(None)` = Lidarr doesn't track them.
    pub async fn discography(&self, mbid: Option<&str>, name: &str) -> Result<Discography> {
        let found = match self.find_artist(mbid, name).await? {
            Some(artist) => {
                let albums = self.albums(artist.id).await?;
                Some((artist, albums))
            }
            None => None,
        };
        let entry = CachedDiscography {
            found,
            mbid: mbid.filter(|m| !m.is_empty()).map(str::to_string),
            fetched_ms: now_ms(),
        };
        if let Some(path) = discography_path(&self.base, name)
            && let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
            && let Ok(json) = serde_json::to_string(&entry)
        {
            let _ = std::fs::write(path, json);
        }
        Ok(entry.found)
    }
}

fn without_url(error: reqwest::Error) -> anyhow::Error {
    let timeout = error.is_timeout();
    let connect = error.is_connect();
    let error = error.without_url();
    if timeout {
        anyhow::Error::new(error).context("Lidarr did not answer in time")
    } else if connect {
        anyhow::Error::new(error).context("Could not reach Lidarr")
    } else {
        anyhow::Error::new(error).context("Lidarr request failed")
    }
}

async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Lidarr rejected the API key");
    }
    // Lidarr explains validation failures in the body (`[{errorMessage}]` or
    // `{message}`); show that rather than a bare status.
    let body = response.text().await.unwrap_or_default();
    match error_message(&body) {
        Some(message) => bail!("Lidarr: {message}"),
        None => bail!("Lidarr answered {status}"),
    }
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let response = check(response).await?;
    let bytes = response.bytes().await.map_err(without_url)?;
    // A reverse proxy's login page answers 200 with HTML.
    serde_json::from_slice(&bytes).context("Lidarr sent an unexpected answer — check the URL")
}

/// The human part of a Lidarr error body, if it has one.
fn error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let pick = |v: &serde_json::Value| {
        ["errorMessage", "message"]
            .iter()
            .find_map(|k| v.get(k)?.as_str().map(str::to_string))
    };
    match &value {
        serde_json::Value::Array(items) => items.iter().find_map(pick),
        other => pick(other),
    }
    .filter(|m| !m.trim().is_empty())
}

/// `host:8686/` → `http://host:8686`; `None` for an empty field.
/// How long an artist's cached Lidarr discography is taken as current. Older
/// ones still paint, and are asked for again behind them.
pub const DISCOGRAPHY_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// A Lidarr artist and their albums; `None` = Lidarr doesn't track them.
pub type Discography = Option<(Artist, Vec<Album>)>;

/// One artist's answer from [`Lidarr::discography`], as kept on disk.
#[derive(Serialize, Deserialize)]
struct CachedDiscography {
    found: Discography,
    /// The MusicBrainz id it was asked with.
    mbid: Option<String>,
    fetched_ms: u64,
}

/// The last [`Lidarr::discography`] answer for this artist on this Lidarr, and
/// whether it is still current (younger than [`DISCOGRAPHY_TTL`]). `None` when
/// nothing usable is cached: never asked, unreadable, or asked by name and
/// now a MusicBrainz id may tell differently. Blocking (reads a file).
pub fn cached_discography(
    base: &str,
    mbid: Option<&str>,
    name: &str,
) -> Option<(Discography, bool)> {
    let path = discography_path(base, name)?;
    let entry: CachedDiscography =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if !entry.answers(mbid.filter(|m| !m.is_empty())) {
        return None;
    }
    let age = Duration::from_millis(now_ms().saturating_sub(entry.fetched_ms));
    Some((entry.found, age < DISCOGRAPHY_TTL))
}

impl CachedDiscography {
    /// Whether this answer stands for a lookup with `mbid`: a found artist
    /// must carry it, and a miss counts only if it was asked with it (a name
    /// miss may be found by id).
    fn answers(&self, mbid: Option<&str>) -> bool {
        match (&self.found, mbid) {
            (_, None) => true,
            (Some((artist, _)), Some(mbid)) => artist.foreign_artist_id == mbid,
            (None, Some(mbid)) => self.mbid.as_deref() == Some(mbid),
        }
    }
}

/// Drops every cached discography: called after an edit made from here
/// (monitoring, adding), which the cached answers no longer show.
fn forget_discographies() {
    if let Ok(dir) = crate::config::lidarr_cache_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn discography_path(base: &str, name: &str) -> Option<PathBuf> {
    let mut h = DefaultHasher::new();
    base.hash(&mut h);
    fold(name).hash(&mut h);
    Some(
        crate::config::lidarr_cache_dir()
            .ok()?
            .join(format!("artist-{:016x}.json", h.finish())),
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn normalize_base(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }
    Some(if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    })
}

fn match_artist_by_name(all: Vec<Artist>, name: &str) -> Option<Artist> {
    let want = fold(name);
    if want.is_empty() {
        return None;
    }
    let mut hits = all.into_iter().filter(|a| fold(&a.artist_name) == want);
    let first = hits.next()?;
    // Two Lidarr artists of the same name: no way to tell which is meant.
    hits.next().is_none().then_some(first)
}

/// Lower-cased letters and digits only, so "AC/DC" meets "ACDC".
pub fn fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Approved first, then Lidarr's own quality weight, then seeders.
pub fn sort_releases(releases: &mut [Release]) {
    releases.sort_by(|a, b| {
        b.approved
            .cmp(&a.approved)
            .then(b.quality_weight.cmp(&a.quality_weight))
            .then(b.seeders.unwrap_or(0).cmp(&a.seeders.unwrap_or(0)))
            .then(b.size.cmp(&a.size))
    });
}

/// Cover Art Archive serves thumbnails as `<image>-500.jpg`; the full image
/// can be many megabytes for a 160px card.
pub fn cover_thumbnail_url(url: &str, size: u32) -> String {
    if !url.contains("coverartarchive.org") {
        return url.to_string();
    }
    let rung = if size <= 250 {
        250
    } else if size <= 500 {
        500
    } else {
        1200
    };
    match url.rsplit_once('.') {
        Some((head, ext))
            if matches!(ext, "jpg" | "jpeg" | "png")
                && !head.ends_with("-250")
                && !head.ends_with("-500")
                && !head.ends_with("-1200") =>
        {
            format!("{head}-{rung}.{ext}")
        }
        _ => url.to_string(),
    }
}

/// Lidarr's bytes as a short human size (`312 MB`).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `ageHours` as `3h` / `5d` / `2y`.
pub fn human_age(hours: f64) -> String {
    if hours < 48.0 {
        format!("{}h", hours.max(0.0).round() as u64)
    } else if hours < 24.0 * 730.0 {
        format!("{}d", (hours / 24.0).round() as u64)
    } else {
        format!("{}y", (hours / (24.0 * 365.0)).round() as u64)
    }
}

/// Lidarr albums the server's library doesn't have: no files in Lidarr and
/// not matched (MBID, else folded title) to any `owned` album.
pub fn missing_albums(albums: Vec<Album>, owned: &[(Option<String>, String)]) -> Vec<Album> {
    let mut missing: Vec<Album> = albums
        .into_iter()
        .filter(|album| {
            if album
                .statistics
                .as_ref()
                .is_some_and(|s| s.track_file_count > 0)
            {
                return false;
            }
            let title = fold(&album.title);
            !owned.iter().any(|(mbid, owned_title)| {
                mbid.as_deref()
                    .is_some_and(|m| !m.is_empty() && m == album.foreign_album_id)
                    || (!title.is_empty() && fold(owned_title) == title)
            })
        })
        .collect();
    // Newest first, like the discography; undated last.
    missing.sort_by(|a, b| {
        b.release_date
            .is_some()
            .cmp(&a.release_date.is_some())
            .then(b.release_date.cmp(&a.release_date))
    });
    missing
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatus {
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    /// 0 (or absent) on a lookup hit Lidarr does not track yet.
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub foreign_artist_id: String,
    #[serde(default)]
    pub monitored: bool,
    #[serde(default)]
    pub disambiguation: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    /// `Person`, `Group`, …
    #[serde(default)]
    pub artist_type: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

impl Artist {
    pub fn in_lidarr(&self) -> bool {
        self.id > 0
    }

    /// The artist's poster's remote URL, else Lidarr's local copy (`false`).
    pub fn photo(&self) -> Option<(&str, bool)> {
        let image = self
            .images
            .iter()
            .find(|i| i.cover_type.eq_ignore_ascii_case("poster"))
            .or_else(|| self.images.first())?;
        image_source(image)
    }
}

fn image_source(image: &Image) -> Option<(&str, bool)> {
    match image.remote_url.as_deref().filter(|u| !u.is_empty()) {
        Some(remote) => Some((remote, true)),
        None => image
            .url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(|u| (u, false)),
    }
}

/// One hit of Lidarr's metadata search, with the record exactly as Lidarr
/// sent it: adding posts that record back, filled in with where and how to
/// keep it, so fields this client doesn't model still reach Lidarr.
#[derive(Debug, Clone, PartialEq)]
pub struct Lookup<T> {
    pub item: T,
    raw: serde_json::Value,
}

/// Hits that don't parse are skipped, not the whole answer.
pub fn parse_lookups<T: DeserializeOwned>(raw: Vec<serde_json::Value>) -> Vec<Lookup<T>> {
    raw.into_iter()
        .filter_map(|raw| {
            let item = serde_json::from_value(raw.clone()).ok()?;
            Some(Lookup { item, raw })
        })
        .collect()
}

/// Primary types in the order Lidarr's artist page lists them; anything else
/// lands in "Other".
const SECTIONS: [(&str, &str); 4] = [
    ("album", "Albums"),
    ("ep", "EPs"),
    ("single", "Singles"),
    ("broadcast", "Broadcasts"),
];

/// A tracked artist's discography in sections by primary type, newest first
/// within each, as indices into `albums`. `only_missing` keeps the albums
/// that lack files. Empty sections are left out.
pub fn discography_sections(
    albums: &[Album],
    only_missing: bool,
) -> Vec<(&'static str, Vec<usize>)> {
    let mut sections: Vec<(&'static str, Vec<usize>)> = SECTIONS
        .iter()
        .map(|(_, label)| (*label, Vec::new()))
        .chain(std::iter::once(("Other", Vec::new())))
        .collect();
    for (i, album) in albums.iter().enumerate() {
        if only_missing && album.complete() {
            continue;
        }
        let kind = album.album_type.to_ascii_lowercase();
        let slot = SECTIONS
            .iter()
            .position(|(k, _)| *k == kind)
            .unwrap_or(SECTIONS.len());
        sections[slot].1.push(i);
    }
    for (_, ids) in &mut sections {
        ids.sort_by(|&a, &b| {
            let (a, b) = (albums[a].release_day(), albums[b].release_day());
            b.is_some().cmp(&a.is_some()).then(b.cmp(&a))
        });
    }
    sections.retain(|(_, ids)| !ids.is_empty());
    sections
}

/// `14 releases · 9 on disk · 5 missing · 3.2 GB`.
pub fn discography_summary(albums: &[Album]) -> String {
    let on_disk = albums.iter().filter(|a| a.complete()).count();
    let missing = albums.len() - on_disk;
    let size: u64 = albums
        .iter()
        .filter_map(|a| a.statistics.as_ref())
        .map(|s| s.size_on_disk)
        .sum();
    let releases = match albums.len() {
        1 => "1 release".to_string(),
        n => format!("{n} releases"),
    };
    let mut parts = vec![releases];
    if on_disk > 0 {
        parts.push(format!("{on_disk} on disk"));
    }
    if missing > 0 {
        parts.push(format!("{missing} missing"));
    }
    if size > 0 {
        parts.push(human_size(size));
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub default_quality_profile_id: i64,
    #[serde(default)]
    pub default_metadata_profile_id: i64,
    #[serde(default)]
    pub default_monitor_option: Option<String>,
    #[serde(default)]
    pub free_space: Option<u64>,
}

/// A quality or metadata profile.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

/// Which of a new artist's albums Lidarr monitors (`addOptions.monitor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Monitor {
    All,
    Future,
    Missing,
    Existing,
    First,
    Latest,
    None,
}

impl Monitor {
    pub const ALL: [Monitor; 7] = [
        Monitor::All,
        Monitor::Future,
        Monitor::Missing,
        Monitor::Existing,
        Monitor::First,
        Monitor::Latest,
        Monitor::None,
    ];

    pub fn api(self) -> &'static str {
        match self {
            Monitor::All => "all",
            Monitor::Future => "future",
            Monitor::Missing => "missing",
            Monitor::Existing => "existing",
            Monitor::First => "first",
            Monitor::Latest => "latest",
            Monitor::None => "none",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Monitor::All => "All albums",
            Monitor::Future => "Future albums",
            Monitor::Missing => "Missing albums",
            Monitor::Existing => "Existing albums",
            Monitor::First => "First album",
            Monitor::Latest => "Latest album",
            Monitor::None => "None",
        }
    }

    pub fn from_api(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.api() == raw)
    }
}

/// Where and how Lidarr keeps something being added.
#[derive(Debug, Clone, PartialEq)]
pub struct AddOptions {
    pub root_folder: String,
    pub quality_profile_id: i64,
    pub metadata_profile_id: i64,
    /// Artists only; an added album monitors just itself.
    pub monitor: Monitor,
    /// Start searching for what was added straight away.
    pub search: bool,
}

/// `POST /artist` body: the lookup record plus the add options.
pub fn artist_body(raw: &serde_json::Value, opts: &AddOptions) -> serde_json::Value {
    let mut body = raw.clone();
    if let Some(obj) = body.as_object_mut() {
        fill_artist(obj, opts, opts.monitor, opts.search);
    }
    body
}

/// `POST /album` body: the album monitored on its own; its artist, when
/// Lidarr has to add it too, monitors nothing else.
pub fn album_body(raw: &serde_json::Value, opts: &AddOptions) -> serde_json::Value {
    use serde_json::json;
    let mut body = raw.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("monitored".into(), json!(true));
        obj.insert(
            "addOptions".into(),
            json!({ "searchForNewAlbum": opts.search }),
        );
        let artist = obj.entry("artist").or_insert_with(|| json!({}));
        if let Some(artist) = artist.as_object_mut() {
            fill_artist(artist, opts, Monitor::None, false);
        }
    }
    body
}

fn fill_artist(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    opts: &AddOptions,
    monitor: Monitor,
    search: bool,
) {
    use serde_json::json;
    obj.insert("qualityProfileId".into(), json!(opts.quality_profile_id));
    obj.insert("metadataProfileId".into(), json!(opts.metadata_profile_id));
    obj.insert("rootFolderPath".into(), json!(opts.root_folder));
    // An album add leaves the artist monitored (Lidarr skips the albums of
    // an unmonitored artist) but watching for nothing new.
    obj.insert("monitored".into(), json!(true));
    obj.insert(
        "monitorNewItems".into(),
        json!(if monitor == Monitor::None {
            "none"
        } else {
            "all"
        }),
    );
    obj.entry("tags").or_insert_with(|| json!([]));
    obj.insert(
        "addOptions".into(),
        json!({ "monitor": monitor.api(), "searchForMissingAlbums": search }),
    );
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    /// 0 (or absent) on a lookup hit Lidarr does not track yet.
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub disambiguation: Option<String>,
    #[serde(default)]
    pub foreign_album_id: String,
    #[serde(default)]
    pub artist_id: i64,
    #[serde(default)]
    pub album_type: String,
    #[serde(default)]
    pub secondary_types: Vec<serde_json::Value>,
    /// ISO date-time; only the date part is meaningful.
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub monitored: bool,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Milliseconds.
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub statistics: Option<AlbumStatistics>,
    #[serde(default)]
    pub artist: Option<Artist>,
}

impl Album {
    /// `2019-05-03T00:00:00Z` → `2019-05-03`.
    pub fn release_day(&self) -> Option<&str> {
        let date = self.release_date.as_deref()?;
        let day = date.split('T').next().unwrap_or(date);
        // Lidarr writes 0001-01-01 for "unknown".
        (!day.starts_with("0001")).then_some(day)
    }

    pub fn year(&self) -> Option<&str> {
        self.release_day().and_then(|d| d.get(..4))
    }

    /// `(year, month, day)` shaped like subsonic's `Album::release_key`
    /// (missing month/day = 0), so the two discographies sort together.
    pub fn release_key(&self) -> Option<(i32, u32, u32)> {
        let mut parts = self.release_day()?.split('-');
        let year = parts.next()?.parse().ok()?;
        let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((year, next(), next()))
    }

    /// Album type plus any secondary types (`Album · Live`).
    pub fn type_label(&self) -> String {
        self.release_types().join(" · ")
    }

    /// Primary type then secondary types (`Album`, `Live`, …), MusicBrainz's
    /// names as Lidarr sends them — the same vocabulary as OpenSubsonic's
    /// `releaseTypes`.
    pub fn release_types(&self) -> Vec<String> {
        let mut parts = vec![self.album_type.clone()];
        parts.extend(self.secondary_types.iter().filter_map(|t| match t {
            serde_json::Value::String(s) => Some(s.clone()),
            other => other.get("name")?.as_str().map(str::to_string),
        }));
        parts.retain(|p| !p.is_empty());
        parts
    }

    /// The cover's remote URL (Cover Art Archive), else Lidarr's local copy.
    pub fn cover(&self) -> Option<(&str, bool)> {
        let cover = self
            .images
            .iter()
            .find(|i| i.cover_type.eq_ignore_ascii_case("cover"))
            .or_else(|| self.images.first())?;
        image_source(cover)
    }

    pub fn in_lidarr(&self) -> bool {
        self.id > 0
    }

    /// Files, out of tracks.
    fn file_counts(&self) -> (u32, u32) {
        let s = self.statistics.clone().unwrap_or_default();
        (s.track_file_count, s.track_count)
    }

    /// Every track has a file.
    pub fn complete(&self) -> bool {
        let (files, tracks) = self.file_counts();
        tracks > 0 && files >= tracks
    }

    /// Some tracks have files, not all.
    pub fn partial(&self) -> Option<(u32, u32)> {
        let (files, tracks) = self.file_counts();
        (files > 0 && files < tracks).then_some((files, tracks))
    }

    pub fn artist_name(&self) -> &str {
        self.artist
            .as_ref()
            .map(|a| a.artist_name.as_str())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AlbumStatistics {
    #[serde(default)]
    pub track_file_count: u32,
    #[serde(default)]
    pub track_count: u32,
    #[serde(default)]
    pub total_track_count: u32,
    #[serde(default)]
    pub size_on_disk: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    #[serde(default)]
    pub cover_type: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: i64,
    #[serde(default)]
    pub title: String,
    /// Lidarr's own label ("A1" for vinyl sides), shown as-is.
    #[serde(default)]
    pub track_number: String,
    #[serde(default)]
    pub absolute_track_number: u32,
    #[serde(default)]
    pub medium_number: u32,
    /// Milliseconds.
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub has_file: bool,
    #[serde(default)]
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    /// `queued`, `started`, `completed`, `failed`, `aborted`, …
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub body: serde_json::Value,
    #[serde(default)]
    pub message: Option<String>,
}

impl Command {
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "started")
    }

    /// The artist an `ArtistSearch` command covers.
    pub fn artist_id(&self) -> Option<i64> {
        (self.name == "ArtistSearch")
            .then(|| self.body.get("artistId")?.as_i64())
            .flatten()
    }

    /// Albums an `AlbumSearch` command covers.
    pub fn album_ids(&self) -> Vec<i64> {
        if self.name != "AlbumSearch" {
            return Vec::new();
        }
        self.body
            .get("albumIds")
            .and_then(|v| v.as_array())
            .map(|ids| ids.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    #[serde(default)]
    pub guid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub indexer: String,
    #[serde(default)]
    pub indexer_id: i64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub seeders: Option<u32>,
    #[serde(default)]
    pub leechers: Option<u32>,
    /// `torrent` / `usenet`.
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub quality: Option<QualityModel>,
    #[serde(default)]
    pub quality_weight: i64,
    #[serde(default)]
    pub age_hours: f64,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub rejections: Vec<String>,
}

impl Release {
    pub fn quality_name(&self) -> &str {
        self.quality
            .as_ref()
            .map(|q| q.quality.name.as_str())
            .unwrap_or("Unknown")
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QualityModel {
    #[serde(default)]
    pub quality: Quality,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueuePage {
    #[serde(default)]
    records: Vec<QueueItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WantedPage {
    #[serde(default)]
    total_records: u64,
    #[serde(default)]
    records: Vec<Album>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPage {
    #[serde(default)]
    records: Vec<HistoryItem>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: i64,
    #[serde(default)]
    pub album_id: Option<i64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub sizeleft: f64,
    /// `hh:mm:ss` or `d.hh:mm:ss`.
    #[serde(default)]
    pub timeleft: Option<String>,
    /// `downloading`, `paused`, `queued`, `completed`, `delay`, `warning`, `failed`, …
    #[serde(default)]
    pub status: String,
    /// `ok` / `warning` / `error`.
    #[serde(default)]
    pub tracked_download_status: Option<String>,
    /// `downloading`, `importPending`, `importing`, `imported`, `failedPending`, `failed`.
    #[serde(default)]
    pub tracked_download_state: Option<String>,
    #[serde(default)]
    pub status_messages: Vec<StatusMessage>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub download_client: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub quality: Option<QualityModel>,
    #[serde(default)]
    pub artist: Option<Artist>,
    #[serde(default)]
    pub album: Option<Album>,
}

impl QueueItem {
    /// 0.0–1.0 downloaded.
    pub fn progress(&self) -> f32 {
        if self.size <= 0.0 {
            return 0.0;
        }
        ((self.size - self.sizeleft) / self.size).clamp(0.0, 1.0) as f32
    }

    /// What the row should say it is doing.
    pub fn state_label(&self) -> &'static str {
        match self.tracked_download_state.as_deref() {
            Some("importPending") => "Waiting to import",
            Some("importing") => "Importing",
            Some("imported") => "Imported",
            Some("failedPending") | Some("failed") => "Failed",
            _ => match self.status.as_str() {
                "downloading" => "Downloading",
                "paused" => "Paused",
                "queued" => "Queued",
                "completed" => "Downloaded",
                "delay" => "Delayed",
                "warning" => "Warning",
                "failed" => "Failed",
                "downloadClientUnavailable" => "Client unavailable",
                _ => "Pending",
            },
        }
    }

    pub fn has_problem(&self) -> bool {
        matches!(
            self.tracked_download_status.as_deref(),
            Some("warning") | Some("error")
        ) || matches!(self.status.as_str(), "warning" | "failed")
    }

    /// Every status message line, for the row's tooltip.
    pub fn messages(&self) -> Vec<String> {
        let mut out: Vec<String> = self.error_message.iter().cloned().collect();
        for group in &self.status_messages {
            out.extend(group.messages.iter().cloned());
            if group.messages.is_empty() && !group.title.is_empty() {
                out.push(group.title.clone());
            }
        }
        out
    }

    /// `01:02:03` → `1h 2m`; `1.02:00:00` → `1d 2h`.
    pub fn eta(&self) -> Option<String> {
        format_timeleft(self.timeleft.as_deref()?)
    }
}

pub fn format_timeleft(raw: &str) -> Option<String> {
    let (days, rest) = match raw.split_once('.') {
        Some((d, rest)) if !d.contains(':') => (d.parse::<u64>().ok()?, rest),
        _ => (0, raw),
    };
    let mut parts = rest.split(':').map(|p| p.split('.').next().unwrap_or(p));
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let s: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some(if days > 0 {
        format!("{days}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    })
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatusMessage {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: i64,
    /// `grabbed`, `trackFileImported`, `downloadFailed`, `albumImportIncomplete`, …
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub source_title: String,
    #[serde(default)]
    pub artist: Option<Artist>,
    #[serde(default)]
    pub album: Option<Album>,
}

impl HistoryItem {
    pub fn event_label(&self) -> &str {
        match self.event_type.as_str() {
            "grabbed" => "Grabbed",
            "trackFileImported" => "Imported",
            "downloadImported" => "Imported",
            "downloadFailed" => "Download failed",
            "albumImportIncomplete" => "Import incomplete",
            "trackFileDeleted" => "Deleted",
            "trackFileRenamed" => "Renamed",
            "trackFileRetagged" => "Retagged",
            "downloadIgnored" => "Ignored",
            other => other,
        }
    }

    /// Files landed in the music folder: a download's import, or a track
    /// imported by hand.
    pub fn is_import(&self) -> bool {
        matches!(
            self.event_type.as_str(),
            "downloadImported" | "trackFileImported"
        )
    }

    pub fn is_failure(&self) -> bool {
        matches!(
            self.event_type.as_str(),
            "downloadFailed" | "albumImportIncomplete"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_discography_answers_only_what_it_was_asked() {
        let artist = Artist {
            id: 3,
            foreign_artist_id: "mb-a".into(),
            ..Default::default()
        };
        let found = CachedDiscography {
            found: Some((artist, vec![album(1, "One", "", 0, None)])),
            mbid: None,
            fetched_ms: 0,
        };
        assert!(found.answers(None));
        assert!(found.answers(Some("mb-a")));
        assert!(!found.answers(Some("mb-b")));

        let missed_by_name = CachedDiscography {
            found: None,
            mbid: None,
            fetched_ms: 0,
        };
        assert!(missed_by_name.answers(None));
        assert!(!missed_by_name.answers(Some("mb-a")));
        let missed_by_id = CachedDiscography {
            mbid: Some("mb-a".into()),
            ..missed_by_name
        };
        assert!(missed_by_id.answers(Some("mb-a")));
    }

    #[test]
    fn release_key_reads_the_date_and_skips_unknown() {
        assert_eq!(
            album(1, "a", "m", 0, Some("2019-05-03T00:00:00Z")).release_key(),
            Some((2019, 5, 3))
        );
        assert_eq!(
            album(1, "a", "m", 0, Some("0001-01-01T00:00:00Z")).release_key(),
            None
        );
        assert_eq!(album(1, "a", "m", 0, None).release_key(), None);
    }

    #[test]
    fn cached_discography_round_trips() {
        let entry = CachedDiscography {
            found: Some((
                Artist {
                    id: 3,
                    artist_name: "A".into(),
                    ..Default::default()
                },
                vec![album(1, "One", "mb-1", 4, Some("2019-05-03T00:00:00Z"))],
            )),
            mbid: Some("mb-a".into()),
            fetched_ms: 42,
        };
        let back: CachedDiscography =
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert_eq!(back.found, entry.found);
        assert_eq!(back.mbid, entry.mbid);
        assert_eq!(back.fetched_ms, 42);
    }

    fn album(id: i64, title: &str, mbid: &str, files: u32, date: Option<&str>) -> Album {
        Album {
            id,
            title: title.into(),
            foreign_album_id: mbid.into(),
            release_date: date.map(str::to_string),
            statistics: Some(AlbumStatistics {
                track_file_count: files,
                track_count: 10,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn base_gets_a_scheme_and_loses_trailing_slashes() {
        assert_eq!(normalize_base(" host:8686/ ").unwrap(), "http://host:8686");
        assert_eq!(
            normalize_base("https://x.example/lidarr/").unwrap(),
            "https://x.example/lidarr"
        );
        assert_eq!(normalize_base("  "), None);
        assert!(Lidarr::new("host", " ").is_none());
    }

    #[test]
    fn absolute_honours_url_base() {
        let l = Lidarr::new("http://h/lidarr", "k").unwrap();
        assert_eq!(
            l.absolute("/MediaCover/1.jpg"),
            "http://h/lidarr/MediaCover/1.jpg"
        );
        assert_eq!(l.absolute("https://caa/x.jpg"), "https://caa/x.jpg");
    }

    #[test]
    fn missing_skips_owned_and_downloaded_albums() {
        let albums = vec![
            album(1, "Owned By Id", "mb-1", 0, Some("2001-01-01T00:00:00Z")),
            album(
                2,
                "Owned: By Title!",
                "mb-2",
                0,
                Some("2002-01-01T00:00:00Z"),
            ),
            album(3, "Has Files", "mb-3", 4, Some("2003-01-01T00:00:00Z")),
            album(4, "Old", "mb-4", 0, Some("1990-01-01T00:00:00Z")),
            album(5, "Undated", "mb-5", 0, None),
            album(6, "New", "mb-6", 0, Some("2020-01-01T00:00:00Z")),
        ];
        let owned = vec![
            (Some("mb-1".to_string()), "Something Else".to_string()),
            (None, "owned by title".to_string()),
        ];
        let ids: Vec<i64> = missing_albums(albums, &owned)
            .iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![6, 4, 5]);
    }

    #[test]
    fn name_match_refuses_ambiguity() {
        let a = |id, name: &str| Artist {
            id,
            artist_name: name.into(),
            monitored: true,
            ..Default::default()
        };
        assert_eq!(
            match_artist_by_name(vec![a(1, "AC/DC"), a(2, "Other")], "acdc").map(|a| a.id),
            Some(1)
        );
        assert!(match_artist_by_name(vec![a(1, "Nirvana"), a(2, "Nirvana")], "Nirvana").is_none());
        assert!(match_artist_by_name(vec![a(1, "X")], "").is_none());
    }

    #[test]
    fn releases_sort_approved_then_quality_then_seeders() {
        let r = |guid: &str, approved, weight, seeders| Release {
            guid: guid.into(),
            title: String::new(),
            indexer: String::new(),
            indexer_id: 0,
            size: 0,
            seeders: Some(seeders),
            leechers: None,
            protocol: String::new(),
            quality: None,
            quality_weight: weight,
            age_hours: 0.0,
            approved,
            rejections: vec![],
        };
        let mut v = vec![
            r("a", false, 9, 99),
            r("b", true, 1, 5),
            r("c", true, 5, 1),
            r("d", true, 5, 9),
        ];
        sort_releases(&mut v);
        let order: Vec<&str> = v.iter().map(|r| r.guid.as_str()).collect();
        assert_eq!(order, vec!["d", "c", "b", "a"]);
    }

    #[test]
    fn caa_urls_get_thumbnails() {
        assert_eq!(
            cover_thumbnail_url("https://coverartarchive.org/release/x/123.jpg", 300),
            "https://coverartarchive.org/release/x/123-500.jpg"
        );
        assert_eq!(
            cover_thumbnail_url("https://coverartarchive.org/release/x/123-250.jpg", 300),
            "https://coverartarchive.org/release/x/123-250.jpg"
        );
        assert_eq!(
            cover_thumbnail_url("https://e/x.jpg", 100),
            "https://e/x.jpg"
        );
    }

    #[test]
    fn timeleft_formats() {
        assert_eq!(format_timeleft("00:00:42").as_deref(), Some("42s"));
        assert_eq!(format_timeleft("00:12:00").as_deref(), Some("12m"));
        assert_eq!(format_timeleft("01:02:03").as_deref(), Some("1h 2m"));
        assert_eq!(format_timeleft("2.03:00:00").as_deref(), Some("2d 3h"));
        assert_eq!(format_timeleft("00:00:05.1234567").as_deref(), Some("5s"));
        assert_eq!(format_timeleft("junk"), None);
    }

    #[test]
    fn sizes_and_ages_read_short() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(312_000_000), "312 MB");
        assert_eq!(human_size(1_500_000_000), "1.5 GB");
        assert_eq!(human_age(5.4), "5h");
        assert_eq!(human_age(72.0), "3d");
        assert_eq!(human_age(24.0 * 365.0 * 3.0), "3y");
    }

    #[test]
    fn error_body_is_read() {
        assert_eq!(
            error_message(r#"[{"propertyName":"x","errorMessage":"Bad thing"}]"#).as_deref(),
            Some("Bad thing")
        );
        assert_eq!(
            error_message(r#"{"message":"Nope"}"#).as_deref(),
            Some("Nope")
        );
        assert_eq!(error_message("<html>"), None);
    }

    #[test]
    fn album_parses_lidarr_json() {
        let json = r#"{"id":7,"title":"T","foreignAlbumId":"mb","artistId":2,
            "albumType":"Album","secondaryTypes":["Live"],
            "releaseDate":"2019-05-03T00:00:00Z","monitored":true,
            "images":[{"coverType":"cover","url":"/MediaCover/Albums/7/cover.jpg",
                "remoteUrl":"https://coverartarchive.org/release/r/1.jpg"}],
            "statistics":{"trackFileCount":0,"trackCount":9,"totalTrackCount":9,
                "sizeOnDisk":0,"percentOfTracks":0}}"#;
        let a: Album = serde_json::from_str(json).unwrap();
        assert_eq!(a.release_day(), Some("2019-05-03"));
        assert_eq!(a.year(), Some("2019"));
        assert_eq!(a.type_label(), "Album · Live");
        assert_eq!(
            a.cover(),
            Some(("https://coverartarchive.org/release/r/1.jpg", true))
        );
        let unknown = Album {
            release_date: Some("0001-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert_eq!(unknown.release_day(), None);
    }

    #[test]
    fn queue_item_progress_and_label() {
        let json = r#"{"id":1,"title":"x","size":100.0,"sizeleft":25.0,
            "timeleft":"00:05:00","status":"downloading",
            "trackedDownloadStatus":"ok","trackedDownloadState":"downloading"}"#;
        let q: QueueItem = serde_json::from_str(json).unwrap();
        assert_eq!(q.progress(), 0.75);
        assert_eq!(q.state_label(), "Downloading");
        assert_eq!(q.eta().as_deref(), Some("5m"));
        assert!(!q.has_problem());
    }

    fn opts() -> AddOptions {
        AddOptions {
            root_folder: "/music".into(),
            quality_profile_id: 2,
            metadata_profile_id: 3,
            monitor: Monitor::Latest,
            search: true,
        }
    }

    #[test]
    fn lookups_keep_the_raw_record_and_skip_odd_hits() {
        let raw: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"artistName":"New","foreignArtistId":"mb-n","extra":{"kept":1},
                 "images":[{"coverType":"fanart","remoteUrl":"https://f/1.jpg"},
                           {"coverType":"poster","remoteUrl":"https://p/1.jpg"}]},
                {"id":9,"artistName":"Known","foreignArtistId":"mb-k"},
                "not an artist"]"#,
        )
        .unwrap();
        let hits: Vec<Lookup<Artist>> = parse_lookups(raw);
        assert_eq!(hits.len(), 2);
        assert!(!hits[0].item.in_lidarr());
        assert!(hits[1].item.in_lidarr());
        assert_eq!(hits[0].item.photo(), Some(("https://p/1.jpg", true)));
        assert_eq!(hits[0].raw["extra"]["kept"], 1);
    }

    #[test]
    fn artist_body_fills_in_where_and_how() {
        let raw = serde_json::json!({"artistName":"New","foreignArtistId":"mb-n","tags":[4]});
        let body = artist_body(&raw, &opts());
        assert_eq!(body["foreignArtistId"], "mb-n");
        assert_eq!(body["rootFolderPath"], "/music");
        assert_eq!(body["qualityProfileId"], 2);
        assert_eq!(body["metadataProfileId"], 3);
        assert_eq!(body["monitored"], true);
        assert_eq!(body["monitorNewItems"], "all");
        assert_eq!(body["tags"], serde_json::json!([4]));
        assert_eq!(body["addOptions"]["monitor"], "latest");
        assert_eq!(body["addOptions"]["searchForMissingAlbums"], true);
    }

    #[test]
    fn album_body_monitors_only_the_album() {
        let raw = serde_json::json!({"title":"T","foreignAlbumId":"mb-a",
            "artist":{"artistName":"A","foreignArtistId":"mb-r"}});
        let body = album_body(&raw, &opts());
        assert_eq!(body["monitored"], true);
        assert_eq!(body["addOptions"]["searchForNewAlbum"], true);
        let artist = &body["artist"];
        assert_eq!(artist["foreignArtistId"], "mb-r");
        assert_eq!(artist["rootFolderPath"], "/music");
        assert_eq!(artist["monitorNewItems"], "none");
        assert_eq!(artist["addOptions"]["monitor"], "none");
        assert_eq!(artist["addOptions"]["searchForMissingAlbums"], false);
        assert_eq!(artist["tags"], serde_json::json!([]));
    }

    #[test]
    fn discography_files_by_type_newest_first() {
        let typed = |id, kind: &str, files, date| Album {
            album_type: kind.into(),
            ..album(id, "", "", files, date)
        };
        let albums = vec![
            typed(1, "Album", 0, Some("2001-01-01T00:00:00Z")),
            typed(2, "Single", 10, Some("2002-01-01T00:00:00Z")),
            typed(3, "Album", 10, Some("2005-01-01T00:00:00Z")),
            typed(4, "Mixtape", 0, None),
            typed(5, "Album", 0, None),
            typed(6, "EP", 3, Some("2003-01-01T00:00:00Z")),
        ];
        let all = discography_sections(&albums, false);
        assert_eq!(
            all,
            vec![
                ("Albums", vec![2, 0, 4]),
                ("EPs", vec![5]),
                ("Singles", vec![1]),
                ("Other", vec![3]),
            ]
        );
        let missing = discography_sections(&albums, true);
        assert_eq!(
            missing,
            vec![("Albums", vec![0, 4]), ("EPs", vec![5]), ("Other", vec![3])]
        );
        assert!(albums[5].partial().is_some());
        assert_eq!(
            discography_summary(&albums),
            "6 releases · 2 on disk · 4 missing"
        );
    }

    #[test]
    fn monitor_options_round_trip() {
        for m in Monitor::ALL {
            assert_eq!(Monitor::from_api(m.api()), Some(m));
        }
        assert_eq!(Monitor::from_api("bogus"), None);
    }

    #[test]
    fn search_command_lists_its_albums() {
        let json = r#"{"id":3,"name":"AlbumSearch","status":"started","body":{"albumIds":[4,5]}}"#;
        let c: Command = serde_json::from_str(json).unwrap();
        assert!(c.is_active());
        assert_eq!(c.album_ids(), vec![4, 5]);
        assert_eq!(c.artist_id(), None);
        let json = r#"{"id":4,"name":"ArtistSearch","status":"queued","body":{"artistId":7}}"#;
        let c: Command = serde_json::from_str(json).unwrap();
        assert_eq!(c.artist_id(), Some(7));
        assert!(c.album_ids().is_empty());
    }
}
