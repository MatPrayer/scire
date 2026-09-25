//! Album descriptions and links from outside the server.
//!
//! `getAlbumInfo2` is whatever Navidrome's agents found, which in practice is
//! Last.fm's wiki *summary* — cut off mid-sentence at a fixed length, the
//! "Read more" link that would have explained the cut stripped with the rest
//! of the HTML — and nothing at all for a library the agents cannot match.
//! This asks the open databases instead, in the order they can be trusted:
//!
//! 1. [MusicBrainz](https://musicbrainz.org) resolves the album to a *release
//!    group* — by the release MBID the server read out of the file's tags when
//!    it has one, otherwise by a title + artist search that must match the
//!    title exactly. The release group carries the album's external links
//!    (Wikidata, Discogs, AllMusic, Bandcamp …) and an optional annotation.
//! 2. [Wikidata](https://www.wikidata.org) turns its item into the English
//!    Wikipedia article, and Wikipedia answers with the article's full intro.
//! 3. Where MusicBrainz knows nothing, a Wikipedia search is tried directly,
//!    accepted only for an article titled exactly like the album whose intro
//!    names the artist — a looser match puts some other record's history on
//!    the page, which is worse than no history at all.
//!
//! Every service here is free and keyless. MusicBrainz asks for at most one
//! request per second per client, which [`mb_get`] enforces process-wide.
//! Answers are cached on disk like the lyrics lookups: a hit for
//! [`HIT_TTL`] (the databases grow links), a miss for [`MISS_TTL`].

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config;

const MUSICBRAINZ: &str = "https://musicbrainz.org/ws/2";
const WIKIDATA: &str = "https://www.wikidata.org/w/api.php";

/// MusicBrainz rejects anonymous user agents; both it and Wikimedia ask for a
/// contact URL.
const UA: &str = concat!(
    "scire/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/MatPrayer/scire)"
);

pub(crate) const HIT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub(crate) const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// MusicBrainz's rate limit is one request per second; a hair over it, since a
/// request landing exactly on the boundary is the one that gets a 503.
const MB_SPACING: Duration = Duration::from_millis(1100);

/// How many times a throttled request is sent again, the first wait, and the
/// longest any one wait may be. See [`retry_delay`].
const RETRIES: u32 = 3;
const RETRY_BASE: Duration = Duration::from_millis(1500);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// A search hit scoring under this is MusicBrainz's own "probably not".
pub(crate) const MB_MIN_SCORE: u32 = 90;

/// What the About card gets from outside the server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlbumInfo {
    /// The Wikipedia article's intro, paragraphs separated by `\n`.
    pub wikipedia: Option<Description>,
    /// The release group's MusicBrainz annotation, markup stripped.
    pub annotation: Option<Description>,
    /// External pages, one per kind, in [`LinkKind`] order.
    pub links: Vec<Link>,
}

impl AlbumInfo {
    fn is_empty(&self) -> bool {
        self.wikipedia.is_none() && self.annotation.is_none() && self.links.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Description {
    pub text: String,
    /// Where the text came from, for attribution.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub kind: LinkKind,
    pub url: String,
}

/// The external pages worth an icon. Declaration order is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    Wikipedia,
    MusicBrainz,
    Discogs,
    AllMusic,
    Bandcamp,
    /// The artist's own site — MusicBrainz's "official homepage" relation.
    /// Never classified by host, since it is anyone's.
    Homepage,
}

impl LinkKind {
    pub fn label(self) -> &'static str {
        match self {
            LinkKind::Wikipedia => "Wikipedia",
            LinkKind::MusicBrainz => "MusicBrainz",
            LinkKind::Discogs => "Discogs",
            LinkKind::AllMusic => "AllMusic",
            LinkKind::Bandcamp => "Bandcamp",
            LinkKind::Homepage => "Official site",
        }
    }

    /// Classify a MusicBrainz url-relation by host. The relation *type* is no
    /// help for half of these — a Bandcamp page is "free streaming" or
    /// "purchase for download" depending on the store, never "bandcamp".
    pub(crate) fn of(url: &str) -> Option<LinkKind> {
        let host = url
            .split("://")
            .nth(1)?
            .split('/')
            .next()?
            .to_ascii_lowercase();
        let is = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        if is("wikipedia.org") {
            Some(LinkKind::Wikipedia)
        } else if is("musicbrainz.org") {
            Some(LinkKind::MusicBrainz)
        } else if is("discogs.com") {
            Some(LinkKind::Discogs)
        } else if is("allmusic.com") {
            Some(LinkKind::AllMusic)
        } else if is("bandcamp.com") {
            Some(LinkKind::Bandcamp)
        } else {
            None
        }
    }
}

/// What an album is looked up by.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub album: String,
    pub artist: Option<String>,
    /// The *release* MBID the server read from the file's tags.
    pub mbid: Option<String>,
    /// The album's year, to tell apart two records by one artist sharing a
    /// title (a self-titled debut and a self-titled second album). Not part
    /// of the cache key: it only steers a search that the title and artist
    /// already name.
    pub year: Option<i32>,
    /// Which services may be asked (`Settings::album_info_*`).
    pub sources: Sources,
}

/// The services a lookup may contact. A disabled one is never sent a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sources {
    pub wikipedia: bool,
    pub musicbrainz: bool,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            wikipedia: true,
            musicbrainz: true,
        }
    }
}

impl Sources {
    pub fn any(self) -> bool {
        self.wikipedia || self.musicbrainz
    }

    /// Whether a link found by a lookup may be shown: a Wikipedia link is
    /// Wikipedia's, and every other kind was found through MusicBrainz's
    /// relations.
    pub fn allows(self, kind: LinkKind) -> bool {
        match kind {
            LinkKind::Wikipedia => self.wikipedia,
            _ => self.musicbrainz,
        }
    }

    /// Whether an answer looked up with `self` can stand in for one asked of
    /// `want`: it asked at least every service `want` does.
    pub fn covers(self, want: Sources) -> bool {
        (self.wikipedia || !want.wikipedia) && (self.musicbrainz || !want.musicbrainz)
    }
}

impl Query {
    fn cache_key(&self) -> String {
        let mut h = DefaultHasher::new();
        match self.mbid.as_deref().filter(|id| is_mbid(id)) {
            // An MBID names the release on its own; keying on the tags as well
            // would re-ask for the same album after a retag.
            Some(id) => id.to_ascii_lowercase().hash(&mut h),
            None => {
                normalize(&base_title(&self.album)).hash(&mut h);
                normalize(self.artist.as_deref().unwrap_or("")).hash(&mut h);
            }
        }
        // A lookup with a service switched off is a different answer, and must
        // not be served for 30 days to the page that has it back on. Both on
        // hashes nothing extra, so the cache written before the switches
        // existed stays valid.
        if self.sources != Sources::default() {
            (self.sources.wikipedia, self.sources.musicbrainz).hash(&mut h);
        }
        format!("{:016x}", h.finish())
    }
}

// One client for every request, as in `lyrics`.
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
        config::album_info_cache_dir()
            .ok()?
            .join(format!("{}.json", query.cache_key())),
    )
}

/// The outer `Option` is "nothing usable cached", the inner "cached miss".
pub(crate) fn cached<T: for<'de> Deserialize<'de>>(path: Option<&PathBuf>) -> Option<Option<T>> {
    let path = path?;
    let text = fs::read_to_string(path).ok()?;
    let info = serde_json::from_str::<Option<T>>(&text).ok()?;
    let age = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())?;
    let ttl = if info.is_some() { HIT_TTL } else { MISS_TTL };
    (age <= ttl).then_some(info)
}

pub(crate) fn store<T: Serialize>(path: Option<&PathBuf>, info: &Option<T>) {
    let Some(path) = path else { return };
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(info) {
        let _ = fs::write(path, json);
    }
}

/// Look the album up, reading and writing the disk cache. `Ok(None)` is an
/// answer — nobody knows this album — and is cached as one; `Err` is a lookup
/// that failed and is not. Must run inside the tokio runtime.
pub async fn fetch(query: Query) -> Result<Option<AlbumInfo>> {
    let path = cache_path(&query);
    if let Some(hit) = cached::<AlbumInfo>(path.as_ref()) {
        return Ok(hit);
    }
    let found = lookup(&query).await?.filter(|info| !info.is_empty());
    store(path.as_ref(), &found);
    Ok(found)
}

async fn lookup(query: &Query) -> Result<Option<AlbumInfo>> {
    let mut info = AlbumInfo::default();
    let group = match release_group_id(query).await? {
        Some(id) => mb_get::<ReleaseGroup>(
            &format!("release-group/{id}"),
            &[("inc", "url-rels+annotation")],
        )
        .await?
        .map(|g| (id, g)),
        None => None,
    };

    let mut wikipedia_url = None;
    let mut wikidata = None;
    if let Some((id, group)) = &group {
        info.links.push(Link {
            kind: LinkKind::MusicBrainz,
            url: format!("https://musicbrainz.org/release-group/{id}"),
        });
        for rel in &group.relations {
            let Some(url) = rel.url.as_ref().map(|u| u.resource.clone()) else {
                continue;
            };
            if rel.kind == "wikidata" {
                wikidata = wikidata.or(wikidata_id(&url));
                continue;
            }
            match LinkKind::of(&url) {
                Some(LinkKind::Wikipedia) => wikipedia_url = wikipedia_url.or(Some(url)),
                Some(kind) if !info.links.iter().any(|l| l.kind == kind) => {
                    info.links.push(Link { kind, url });
                }
                _ => {}
            }
        }
        info.annotation = group
            .annotation
            .as_deref()
            .map(strip_mb_markup)
            .filter(|t| !t.is_empty())
            .map(|text| Description {
                text,
                url: format!("https://musicbrainz.org/release-group/{id}"),
            });
    }

    if !query.sources.wikipedia {
        info.links.sort_by_key(|l| l.kind);
        return Ok(Some(info));
    }
    // Wikidata's sitelink is the article the item is *about*; a direct
    // Wikipedia relation is older and sometimes points at another language.
    let article = match wikidata {
        Some(item) => enwiki_title(&item).await?.map(|t| ("en".to_string(), t)),
        None => None,
    }
    .or_else(|| wikipedia_url.as_deref().and_then(article_from_url));
    info.wikipedia = match article {
        Some((lang, title)) => wikipedia_intro(&lang, &title).await?,
        None => match &query.artist {
            Some(artist) if group.is_none() => search_wikipedia(&query.album, artist).await?,
            _ => None,
        },
    };
    if let Some(w) = &info.wikipedia {
        info.links.push(Link {
            kind: LinkKind::Wikipedia,
            url: w.url.clone(),
        });
    }
    info.links.sort_by_key(|l| l.kind);
    Ok(Some(info))
}

/// The release group behind the query: through the release MBID when there is
/// one, by search otherwise.
async fn release_group_id(query: &Query) -> Result<Option<String>> {
    if !query.sources.musicbrainz {
        return Ok(None);
    }
    if let Some(mbid) = query.mbid.as_deref().filter(|id| is_mbid(id)) {
        let release =
            mb_get::<Release>(&format!("release/{mbid}"), &[("inc", "release-groups")]).await?;
        if let Some(group) = release.and_then(|r| r.release_group) {
            return Ok(Some(group.id));
        }
        // A stale MBID (merged away) falls through to the search.
    }
    let Some(artist) = query.artist.as_deref().filter(|a| !a.trim().is_empty()) else {
        return Ok(None);
    };
    let title = base_title(&query.album);
    if title.is_empty() {
        return Ok(None);
    }
    let lucene = format!(
        "releasegroup:\"{}\" AND artist:\"{}\"",
        lucene_escape(&title),
        lucene_escape(artist)
    );
    let hits = mb_get::<GroupSearch>("release-group", &[("query", &lucene), ("limit", "5")])
        .await?
        .map(|s| s.release_groups)
        .unwrap_or_default();
    Ok(pick_group(&hits, &title, query.year))
}

/// The first hit MusicBrainz is sure of whose title is the album's. The search
/// ranks "Live in Rainbows" a close second to "In Rainbows", so the score alone
/// cannot be trusted to have found the right record. Among several with the
/// title — Crystal Castles made two albums called "Crystal Castles" — the one
/// first released in the album's year wins; MusicBrainz's order otherwise.
fn pick_group(hits: &[GroupHit], title: &str, year: Option<i32>) -> Option<String> {
    let want = normalize(title);
    let mut sure = hits
        .iter()
        .filter(|h| h.score >= MB_MIN_SCORE && normalize(&h.title) == want);
    let first = sure.clone().next()?;
    let same_year = year.and_then(|y| {
        let y = y.to_string();
        sure.find(|h| h.first_release_date.get(..4) == Some(y.as_str()))
    });
    Some(same_year.unwrap_or(first).id.clone())
}

pub(crate) async fn enwiki_title(item: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        entities: std::collections::HashMap<String, Entity>,
    }
    #[derive(Deserialize)]
    struct Entity {
        #[serde(default)]
        sitelinks: std::collections::HashMap<String, Sitelink>,
    }
    #[derive(Deserialize)]
    struct Sitelink {
        title: String,
    }
    let resp = get_json::<Resp>(
        WIKIDATA,
        &[
            ("action", "wbgetentities"),
            ("ids", item),
            ("props", "sitelinks"),
            ("sitefilter", "enwiki"),
            ("format", "json"),
        ],
    )
    .await?;
    Ok(resp
        .and_then(|mut r| r.entities.remove(item))
        .and_then(|mut e| e.sitelinks.remove("enwiki"))
        .map(|s| s.title))
}

#[derive(Deserialize)]
struct PagesResp {
    query: Option<Pages>,
}
#[derive(Deserialize)]
struct Pages {
    #[serde(default)]
    pages: Vec<Page>,
}
#[derive(Deserialize)]
struct Page {
    title: String,
    #[serde(default)]
    extract: Option<String>,
}

/// The article's intro as plain text, one paragraph per line.
pub(crate) async fn wikipedia_intro(lang: &str, title: &str) -> Result<Option<Description>> {
    if !lang.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return Ok(None);
    }
    let resp = get_json::<PagesResp>(
        &format!("https://{lang}.wikipedia.org/w/api.php"),
        &[
            ("action", "query"),
            ("prop", "extracts"),
            ("exintro", "1"),
            ("explaintext", "1"),
            ("redirects", "1"),
            ("titles", title),
            ("format", "json"),
            ("formatversion", "2"),
        ],
    )
    .await?;
    let Some(page) = resp
        .and_then(|r| r.query)
        .and_then(|q| q.pages.into_iter().next())
    else {
        return Ok(None);
    };
    Ok(page
        .extract
        .as_deref()
        .map(clean_extract)
        .filter(|t| !t.is_empty() && !is_disambiguation(t))
        .map(|text| Description {
            text,
            url: article_url(lang, &page.title),
        }))
}

/// No MusicBrainz match: ask Wikipedia by name, and believe it only for an
/// article titled like the album whose intro names the artist.
async fn search_wikipedia(album: &str, artist: &str) -> Result<Option<Description>> {
    #[derive(Deserialize)]
    struct Resp {
        query: Option<Search>,
    }
    #[derive(Deserialize)]
    struct Search {
        #[serde(default)]
        search: Vec<Hit>,
    }
    #[derive(Deserialize)]
    struct Hit {
        title: String,
    }
    let title = base_title(album);
    if title.is_empty() {
        return Ok(None);
    }
    let resp = get_json::<Resp>(
        "https://en.wikipedia.org/w/api.php",
        &[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", &format!("{title} {artist} album")),
            ("srlimit", "5"),
            ("format", "json"),
            ("formatversion", "2"),
        ],
    )
    .await?;
    let hits = resp
        .and_then(|r| r.query)
        .map(|q| q.search)
        .unwrap_or_default();
    let Some(hit) = hits
        .into_iter()
        .find(|h| article_matches(&h.title, &title, artist))
    else {
        return Ok(None);
    };
    let intro = wikipedia_intro("en", &hit.title).await?;
    Ok(intro.filter(|d| normalize(&d.text).contains(&normalize(artist))))
}

/// Whether a Wikipedia article title is the album's own: the album title, bare
/// or disambiguated the way Wikipedia does it — "(album)", "(Radiohead album)".
fn article_matches(article: &str, album: &str, artist: &str) -> bool {
    let (base, qualifier) = match article.rsplit_once(" (") {
        Some((base, rest)) => (base, Some(rest.trim_end_matches(')'))),
        None => (article, None),
    };
    if normalize(base) != normalize(album) {
        return false;
    }
    match qualifier {
        None => true,
        Some(q) => {
            let q = normalize(q);
            q.ends_with("album") || q.ends_with("ep") || q.contains(&normalize(artist))
        }
    }
}

/// GET and decode; 404 is `None`, not an error.
pub(crate) async fn get_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    params: &[(&str, &str)],
) -> Result<Option<T>> {
    let mut attempt = 0;
    loop {
        let resp = http().get(url).query(params).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok());
        if let Some(wait) = retry_delay(resp.status(), retry_after, attempt) {
            tracing::debug!("{url}: {} — retrying in {wait:?}", resp.status());
            attempt += 1;
            tokio::time::sleep(wait).await;
            continue;
        }
        return Ok(Some(resp.error_for_status()?.json::<T>().await?));
    }
}

/// Whether a refused request is worth sending again, and after how long.
///
/// MusicBrainz answers 503 when its servers are busy as a whole, not only when
/// this client goes over its rate: the spacing in [`mb_get`] holds and the
/// request still bounces now and then. Failures are not cached, so without a
/// retry the page came up with no description and a second visit found one.
/// Wikimedia's throttle is a 429. `Retry-After` is honoured in its seconds
/// form, capped so a page is not held for a minute.
fn retry_delay(
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
    attempt: u32,
) -> Option<Duration> {
    let throttled = status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    if !throttled || attempt >= RETRIES {
        return None;
    }
    let backoff = RETRY_BASE * 2u32.pow(attempt);
    let asked = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    Some(asked.unwrap_or(backoff).min(RETRY_MAX))
}

/// A MusicBrainz request, spaced at least [`MB_SPACING`] after the last one
/// from anywhere in the process — an album page opened while another's
/// lookup is still running must not double the rate.
pub(crate) async fn mb_get<T: for<'de> Deserialize<'de>>(
    path: &str,
    params: &[(&str, &str)],
) -> Result<Option<T>> {
    static LAST: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);
    let mut last = LAST.lock().await;
    if let Some(at) = *last {
        let wait = MB_SPACING.saturating_sub(at.elapsed());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
    let mut all: Vec<(&str, &str)> = params.to_vec();
    all.push(("fmt", "json"));
    let result = get_json(&format!("{MUSICBRAINZ}/{path}"), &all).await;
    *last = Some(Instant::now());
    result
}

#[derive(Deserialize)]
struct Release {
    #[serde(rename = "release-group")]
    release_group: Option<GroupRef>,
}
#[derive(Deserialize)]
struct GroupRef {
    id: String,
}
#[derive(Deserialize)]
struct GroupSearch {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<GroupHit>,
}
#[derive(Debug, Deserialize)]
struct GroupHit {
    id: String,
    #[serde(default)]
    score: u32,
    #[serde(default)]
    title: String,
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`; empty when unknown.
    #[serde(rename = "first-release-date", default)]
    first_release_date: String,
}
#[derive(Deserialize)]
struct ReleaseGroup {
    #[serde(default)]
    annotation: Option<String>,
    #[serde(default)]
    relations: Vec<Relation>,
}
#[derive(Deserialize)]
pub(crate) struct Relation {
    #[serde(rename = "type", default)]
    pub(crate) kind: String,
    pub(crate) url: Option<RelUrl>,
}
#[derive(Deserialize)]
pub(crate) struct RelUrl {
    pub(crate) resource: String,
}

pub(crate) fn is_mbid(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// `Q223295` out of `https://www.wikidata.org/wiki/Q223295`.
pub(crate) fn wikidata_id(url: &str) -> Option<String> {
    let id = url.trim_end_matches('/').rsplit('/').next()?;
    (id.len() > 1 && id.starts_with('Q') && id[1..].chars().all(|c| c.is_ascii_digit()))
        .then(|| id.to_string())
}

/// `("en", "In Rainbows")` out of `https://en.wikipedia.org/wiki/In_Rainbows`.
pub(crate) fn article_from_url(url: &str) -> Option<(String, String)> {
    let rest = url.split("://").nth(1)?;
    let (host, path) = rest.split_once('/')?;
    let lang = host
        .strip_suffix(".wikipedia.org")?
        .trim_start_matches("www.");
    let lang = lang.split('.').next()?;
    let title = path.strip_prefix("wiki/")?;
    let title = percent_decode(title.split(['#', '?']).next()?).replace('_', " ");
    (!lang.is_empty() && !title.is_empty()).then(|| (lang.to_string(), title))
}

pub(crate) fn article_url(lang: &str, title: &str) -> String {
    let mut out = format!("https://{lang}.wikipedia.org/wiki/");
    for b in title.replace(' ', "_").bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'(' | b')' | b',' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn lucene_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The album title without the edition noise a library tags onto it —
/// "(Deluxe Edition)", "[Remastered 2011]", "CD 2" — which no database files
/// the album under.
pub fn base_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0usize;
    for ch in title.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // "… CD 2", "… Disc 1": a disc of a set tagged as its own album.
    let lower = out.to_lowercase();
    for marker in [" cd ", " disc ", " disk "] {
        if let Some(at) = lower.rfind(marker)
            && lower[at + marker.len()..]
                .chars()
                .all(|c| c.is_ascii_digit())
        {
            out.truncate(at);
            break;
        }
    }
    // iTunes-style release-type suffixes: "Perfect self - Single".
    for suffix in [" - single", " - ep"] {
        let at = out.len().saturating_sub(suffix.len());
        if out.is_char_boundary(at) && out[at..].eq_ignore_ascii_case(suffix) {
            out.truncate(at);
        }
    }
    out.trim_end_matches([' ', '-', ':']).to_string()
}

/// Case-, accent-blind-enough and punctuation-blind comparison key.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
        } else if ch == '&' {
            out.push_str("and");
        }
    }
    out
}

/// Wikipedia's plain-text extract keeps the blank lines between sections and
/// the odd empty paragraph; the card wants one paragraph per line.
pub(crate) fn clean_extract(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn is_disambiguation(text: &str) -> bool {
    let head: String = text.chars().take(200).collect();
    head.contains("may refer to") || head.contains("may also refer to")
}

/// MusicBrainz annotations are written in a small wiki syntax: `[url|label]`
/// links, `'''bold'''`/`''italic''`, `== headings ==`.
pub(crate) fn strip_mb_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(']') {
            Some(close) => {
                let inner = &after[..close];
                out.push_str(inner.rsplit('|').next().unwrap_or(inner));
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    let out = out.replace("'''", "").replace("''", "");
    out.lines()
        .map(|l| l.trim().trim_matches('=').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a description stops mid-sentence. Last.fm's summaries are cut at a
/// fixed length and Navidrome drops the "Read more" link that followed, so the
/// text ends on whatever word the cut landed on — "…over more".
pub fn looks_truncated(text: &str) -> bool {
    let end = text.trim_end();
    !end.is_empty() && !end.ends_with(['.', '!', '?', '…', '"', '\'', '”', '’', ')', ']', ':', ';'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_throttled_request_is_retried_a_few_times_then_given_up() {
        use reqwest::StatusCode as S;
        let busy = S::SERVICE_UNAVAILABLE;
        assert_eq!(retry_delay(busy, None, 0), Some(RETRY_BASE));
        assert_eq!(retry_delay(busy, None, 1), Some(RETRY_BASE * 2));
        assert_eq!(retry_delay(busy, None, RETRIES), None);
        assert_eq!(
            retry_delay(S::TOO_MANY_REQUESTS, Some("2"), 0),
            Some(Duration::from_secs(2))
        );
        // A server asking for a minute is not waited out.
        assert_eq!(retry_delay(busy, Some("60"), 0), Some(RETRY_MAX));
        // The date form of Retry-After falls back to the backoff.
        assert_eq!(
            retry_delay(busy, Some("Wed, 21 Oct 2026 07:28:00 GMT"), 0),
            Some(RETRY_BASE)
        );
        // Everything else fails the same way however often it is sent.
        assert_eq!(retry_delay(S::INTERNAL_SERVER_ERROR, None, 0), None);
        assert_eq!(retry_delay(S::BAD_REQUEST, None, 0), None);
    }

    #[test]
    fn edition_noise_is_dropped_from_the_title() {
        assert_eq!(base_title("In Rainbows"), "In Rainbows");
        assert_eq!(base_title("Abbey Road (Remastered 2009)"), "Abbey Road");
        assert_eq!(
            base_title("In Rainbows [Limited Edition Discbox] CD 2"),
            "In Rainbows"
        );
        assert_eq!(base_title("Mellon Collie - Disc 1"), "Mellon Collie");
        assert_eq!(base_title("Big Calm (flac)"), "Big Calm");
        assert_eq!(base_title("Perfect self - Single"), "Perfect self");
        // A number that is the title is kept.
        assert_eq!(base_title("21"), "21");
        assert_eq!(base_title("Room on Fire"), "Room on Fire");
    }

    #[test]
    fn a_search_hit_must_be_the_album_itself() {
        let hit = |id: &str, score, title: &str| GroupHit {
            id: id.into(),
            score,
            title: title.into(),
            first_release_date: String::new(),
        };
        let hits = [
            hit("live", 100, "Live in Rainbows"),
            hit("real", 98, "In Rainbows"),
        ];
        assert_eq!(
            pick_group(&hits, "In Rainbows", None).as_deref(),
            Some("real")
        );
        // Right title, but MusicBrainz itself is unsure.
        assert_eq!(
            pick_group(&[hit("x", 60, "In Rainbows")], "In Rainbows", None),
            None
        );
        assert_eq!(
            pick_group(
                &[hit("x", 95, "Hail to the Thief")],
                "Hail To The Thief",
                None
            )
            .as_deref(),
            Some("x")
        );
    }

    #[test]
    fn two_albums_of_one_title_are_told_apart_by_year() {
        let hit = |id: &str, date: &str| GroupHit {
            id: id.into(),
            score: 100,
            title: "Crystal Castles".into(),
            first_release_date: date.into(),
        };
        let hits = [hit("second", "2010-04-23"), hit("debut", "2008-03-18")];
        assert_eq!(
            pick_group(&hits, "Crystal Castles", Some(2008)).as_deref(),
            Some("debut")
        );
        assert_eq!(
            pick_group(&hits, "Crystal Castles", Some(2010)).as_deref(),
            Some("second")
        );
        // No year, or none matching: MusicBrainz's own order.
        assert_eq!(
            pick_group(&hits, "Crystal Castles", None).as_deref(),
            Some("second")
        );
        assert_eq!(
            pick_group(&hits, "Crystal Castles", Some(1999)).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn wikipedia_titles_are_matched_with_their_disambiguation() {
        assert!(article_matches("In Rainbows", "In Rainbows", "Radiohead"));
        assert!(article_matches(
            "Discovery (Daft Punk album)",
            "Discovery",
            "Daft Punk"
        ));
        assert!(article_matches("Homework (album)", "Homework", "Daft Punk"));
        assert!(!article_matches(
            "Discovery (film)",
            "Discovery",
            "Daft Punk"
        ));
        assert!(!article_matches(
            "Discovery Channel",
            "Discovery",
            "Daft Punk"
        ));
    }

    #[test]
    fn links_are_classified_by_host() {
        assert_eq!(
            LinkKind::of("https://www.discogs.com/master/21520"),
            Some(LinkKind::Discogs)
        );
        assert_eq!(
            LinkKind::of("https://radiohead.bandcamp.com/album/x"),
            Some(LinkKind::Bandcamp)
        );
        assert_eq!(
            LinkKind::of("https://en.wikipedia.org/wiki/X"),
            Some(LinkKind::Wikipedia)
        );
        assert_eq!(LinkKind::of("https://notdiscogs.com/x"), None);
        assert_eq!(LinkKind::of("https://genius.com/albums/x"), None);
    }

    #[test]
    fn ids_and_articles_come_out_of_urls() {
        assert_eq!(
            wikidata_id("https://www.wikidata.org/wiki/Q223295").as_deref(),
            Some("Q223295")
        );
        assert_eq!(wikidata_id("https://www.wikidata.org/wiki/Special:X"), None);
        assert_eq!(
            article_from_url("https://en.wikipedia.org/wiki/OK_Computer"),
            Some(("en".into(), "OK Computer".into()))
        );
        assert_eq!(
            article_from_url("https://fr.wikipedia.org/wiki/Mot%C3%B6rhead#x"),
            Some(("fr".into(), "Motörhead".into()))
        );
        assert_eq!(
            article_url("en", "Kid A (album)"),
            "https://en.wikipedia.org/wiki/Kid_A_(album)"
        );
    }

    #[test]
    fn annotation_markup_is_stripped() {
        assert_eq!(
            strip_mb_markup("== Notes ==\n'''Recorded''' at [http://x.org|Abbey Road].\n\n"),
            "Notes\nRecorded at Abbey Road."
        );
    }

    #[test]
    fn a_cut_off_summary_is_recognised() {
        assert!(looks_truncated("recorded in Oxfordshire over more"));
        assert!(!looks_truncated("It won a Grammy."));
        assert!(!looks_truncated("They called it “Nude”"));
        assert!(!looks_truncated(""));
    }

    #[test]
    fn a_lookup_with_a_service_off_is_cached_apart() {
        let q = |wikipedia, musicbrainz| Query {
            album: "In Rainbows".into(),
            artist: Some("Radiohead".into()),
            mbid: None,
            year: None,
            sources: Sources {
                wikipedia,
                musicbrainz,
            },
        };
        let full = q(true, true).cache_key();
        // Both on keys exactly as before the switches existed.
        assert_eq!(
            full,
            Query {
                sources: Sources::default(),
                ..q(true, true)
            }
            .cache_key()
        );
        assert_ne!(full, q(true, false).cache_key());
        assert_ne!(full, q(false, true).cache_key());
        assert_ne!(q(true, false).cache_key(), q(false, true).cache_key());
    }

    #[test]
    fn an_answer_covers_only_what_it_asked() {
        let s = |wikipedia, musicbrainz| Sources {
            wikipedia,
            musicbrainz,
        };
        assert!(s(true, true).covers(s(true, false)));
        assert!(s(true, false).covers(s(true, false)));
        assert!(!s(true, false).covers(s(true, true)));
        assert!(!s(false, true).covers(s(true, false)));
    }
}
