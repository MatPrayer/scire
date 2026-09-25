//! Artist bios and links from outside the server — the artist page's
//! counterpart of [`album_info`](super::album_info), sharing its HTTP client,
//! MusicBrainz spacing, retry and cache helpers.
//!
//! `getArtistInfo2`'s biography is Last.fm's summary, cut at a fixed length
//! like the album notes. This asks the open databases instead:
//!
//! 1. [MusicBrainz](https://musicbrainz.org) resolves the artist — by the MBID
//!    the server read out of the tags when it has one; otherwise through one of
//!    the artist's own albums (a release-group search by title and artist,
//!    whose credit names the artist's id), which tells two bands called
//!    "Nirvana" apart where a name search cannot; and only as a last resort by
//!    a name search, believed only when exactly one artist carries the name.
//!    The artist carries its links (Wikidata, official site, Discogs, AllMusic,
//!    Bandcamp) and an optional annotation.
//! 2. Wikidata turns it into the English Wikipedia article, whose intro is the
//!    description.
//! 3. With no MusicBrainz match, a Wikipedia search by name, accepted only for
//!    an article titled like the artist — bare or "(band)", "(musician)" … —
//!    whose intro reads like it is about a musician.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::album_info::{
    self, Description, Link, LinkKind, MB_MIN_SCORE, Relation, Sources, article_from_url,
    base_title, clean_extract, enwiki_title, get_json, is_disambiguation, is_mbid, lucene_escape,
    mb_get, normalize, strip_mb_markup, wikidata_id, wikipedia_intro,
};
use crate::config;

/// How many of the artist's albums are tried to pin the artist down before
/// falling back to a name search. Each is a MusicBrainz request, i.e. a second.
const ALBUM_PROBES: usize = 2;

/// What the artist page gets from outside the server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtistInfo {
    /// The Wikipedia article's intro, paragraphs separated by `\n`.
    pub wikipedia: Option<Description>,
    /// The artist's MusicBrainz annotation, markup stripped.
    pub annotation: Option<Description>,
    /// External pages, one per kind, in [`LinkKind`] order.
    pub links: Vec<Link>,
}

impl ArtistInfo {
    fn is_empty(&self) -> bool {
        self.wikipedia.is_none() && self.annotation.is_none() && self.links.is_empty()
    }
}

/// What an artist is looked up by.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub name: String,
    /// The *artist* MBID the server read from the tags (`getArtistInfo2`).
    pub mbid: Option<String>,
    /// A few of the artist's album titles, to find the right artist of a
    /// shared name through its records.
    pub albums: Vec<String>,
    /// Which services may be asked (`Settings::artist_info_*`).
    pub sources: Sources,
}

impl Query {
    fn cache_key(&self) -> String {
        let mut h = DefaultHasher::new();
        match self.mbid.as_deref().filter(|id| is_mbid(id)) {
            Some(id) => id.to_ascii_lowercase().hash(&mut h),
            // The albums only steer the search; keying on them would re-ask
            // for the same artist every time the library gains a record.
            None => normalize(&self.name).hash(&mut h),
        }
        (self.sources.wikipedia, self.sources.musicbrainz).hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

fn cache_path(query: &Query) -> Option<PathBuf> {
    Some(
        config::artist_info_cache_dir()
            .ok()?
            .join(format!("{}.json", query.cache_key())),
    )
}

/// Look the artist up, reading and writing the disk cache. `Ok(None)` is an
/// answer and is cached; `Err` is not. Must run inside the tokio runtime.
pub async fn fetch(query: Query) -> Result<Option<ArtistInfo>> {
    let path = cache_path(&query);
    if let Some(hit) = album_info::cached::<ArtistInfo>(path.as_ref()) {
        return Ok(hit);
    }
    let found = lookup(&query).await?.filter(|info| !info.is_empty());
    album_info::store(path.as_ref(), &found);
    Ok(found)
}

async fn lookup(query: &Query) -> Result<Option<ArtistInfo>> {
    let mut info = ArtistInfo::default();
    let artist = match artist_id(query).await? {
        Some(id) => mb_get::<MbArtist>(&format!("artist/{id}"), &[("inc", "url-rels+annotation")])
            .await?
            .map(|a| (id, a)),
        None => None,
    };

    let mut wikipedia_url = None;
    let mut wikidata = None;
    if let Some((id, artist)) = &artist {
        let page = format!("https://musicbrainz.org/artist/{id}");
        info.links.push(Link {
            kind: LinkKind::MusicBrainz,
            url: page.clone(),
        });
        for rel in &artist.relations {
            let Some(url) = rel.url.as_ref().map(|u| u.resource.clone()) else {
                continue;
            };
            match rel_kind(rel, &url) {
                RelKind::Wikidata => wikidata = wikidata.or(wikidata_id(&url)),
                RelKind::Link(LinkKind::Wikipedia) => wikipedia_url = wikipedia_url.or(Some(url)),
                RelKind::Link(kind) if !info.links.iter().any(|l| l.kind == kind) => {
                    info.links.push(Link { kind, url });
                }
                _ => {}
            }
        }
        info.annotation = artist
            .annotation
            .as_deref()
            .map(strip_mb_markup)
            .filter(|t| !t.is_empty())
            .map(|text| Description { text, url: page });
    }

    if query.sources.wikipedia {
        let article = match wikidata {
            Some(item) => enwiki_title(&item).await?.map(|t| ("en".to_string(), t)),
            None => None,
        }
        .or_else(|| wikipedia_url.as_deref().and_then(article_from_url));
        info.wikipedia = match article {
            Some((lang, title)) => wikipedia_intro(&lang, &title).await?,
            None if artist.is_none() => search_wikipedia(&query.name).await?,
            None => None,
        };
        if let Some(w) = &info.wikipedia {
            info.links.push(Link {
                kind: LinkKind::Wikipedia,
                url: w.url.clone(),
            });
        }
    }
    info.links.sort_by_key(|l| l.kind);
    Ok(Some(info))
}

enum RelKind {
    Wikidata,
    Link(LinkKind),
    Other,
}

/// An artist's relations are typed where the site is the artist's own
/// ("official homepage") and otherwise classified by host like an album's.
fn rel_kind(rel: &Relation, url: &str) -> RelKind {
    match rel.kind.as_str() {
        "wikidata" => RelKind::Wikidata,
        "official homepage" => RelKind::Link(LinkKind::Homepage),
        _ => LinkKind::of(url).map_or(RelKind::Other, RelKind::Link),
    }
}

/// The artist behind the query: the MBID, else through one of its albums,
/// else an unambiguous name.
async fn artist_id(query: &Query) -> Result<Option<String>> {
    if !query.sources.musicbrainz {
        return Ok(None);
    }
    if let Some(mbid) = query.mbid.as_deref().filter(|id| is_mbid(id)) {
        return Ok(Some(mbid.to_ascii_lowercase()));
    }
    let name = query.name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    for album in query.albums.iter().take(ALBUM_PROBES) {
        let title = base_title(album);
        if title.is_empty() {
            continue;
        }
        let lucene = format!(
            "releasegroup:\"{}\" AND artist:\"{}\"",
            lucene_escape(&title),
            lucene_escape(name)
        );
        let hits = mb_get::<GroupSearch>("release-group", &[("query", &lucene), ("limit", "5")])
            .await?
            .map(|s| s.release_groups)
            .unwrap_or_default();
        if let Some(id) = credited_artist(&hits, &title, name) {
            return Ok(Some(id));
        }
    }
    let lucene = format!("artist:\"{}\"", lucene_escape(name));
    let hits = mb_get::<ArtistSearch>("artist", &[("query", &lucene), ("limit", "5")])
        .await?
        .map(|s| s.artists)
        .unwrap_or_default();
    Ok(unique_artist(&hits, name))
}

/// The artist credited on a sure hit for one of its albums.
fn credited_artist(hits: &[GroupHit], title: &str, name: &str) -> Option<String> {
    let (title, name) = (normalize(title), normalize(name));
    hits.iter()
        .filter(|h| h.score >= MB_MIN_SCORE && normalize(&h.title) == title)
        .flat_map(|h| h.artist_credit.iter())
        .find(|c| normalize(&c.artist.name) == name || normalize(&c.name) == name)
        .map(|c| c.artist.id.clone())
}

/// A name search is only an answer when exactly one confident hit carries the
/// name: two of them is two different artists, and picking one puts the
/// wrong band's history on the page.
fn unique_artist(hits: &[ArtistHit], name: &str) -> Option<String> {
    let name = normalize(name);
    let mut matches = hits
        .iter()
        .filter(|h| h.score >= MB_MIN_SCORE && normalize(&h.name) == name);
    let first = matches.next()?;
    matches.next().is_none().then(|| first.id.clone())
}

/// No MusicBrainz match: ask Wikipedia by name, and believe it only for an
/// article titled like the artist whose intro is about music.
async fn search_wikipedia(name: &str) -> Result<Option<Description>> {
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
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let resp = get_json::<Resp>(
        "https://en.wikipedia.org/w/api.php",
        &[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", &format!("{name} music")),
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
    let Some(hit) = hits.into_iter().find(|h| article_matches(&h.title, name)) else {
        return Ok(None);
    };
    let intro = wikipedia_intro("en", &hit.title).await?;
    Ok(intro.filter(|d| {
        let text = clean_extract(&d.text);
        !is_disambiguation(&text) && sounds_musical(&text)
    }))
}

/// Whether a Wikipedia article title is the artist's own: the name, bare or
/// disambiguated the way Wikipedia does it for musicians.
fn article_matches(article: &str, name: &str) -> bool {
    const QUALIFIERS: &[&str] = &[
        "band",
        "musician",
        "singer",
        "rapper",
        "group",
        "duo",
        "dj",
        "producer",
        "composer",
        "songwriter",
        "artist",
        "musicalgroup",
        "entertainer",
    ];
    let (base, qualifier) = match article.rsplit_once(" (") {
        Some((base, rest)) => (base, Some(rest.trim_end_matches(')'))),
        None => (article, None),
    };
    if normalize(base) != normalize(name) {
        return false;
    }
    match qualifier {
        None => true,
        Some(q) => {
            let q = normalize(q);
            QUALIFIERS.iter().any(|w| q.ends_with(w))
        }
    }
}

/// A bare title is anybody's — "Air" is a band and a gas — so the intro has to
/// say it is about music before it goes on an artist page.
fn sounds_musical(text: &str) -> bool {
    const WORDS: &[&str] = &[
        "band",
        "musician",
        "singer",
        "rapper",
        "songwriter",
        "composer",
        "producer",
        "dj",
        "duo",
        "album",
        "record label",
        "musical",
    ];
    let head: String = text.chars().take(400).collect::<String>().to_lowercase();
    head.split(|c: char| !c.is_alphanumeric() && c != ' ')
        .any(|chunk| WORDS.iter().any(|w| contains_word(chunk, w)))
}

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric)
            && !after.is_some_and(|c| c.is_alphanumeric() && c != 's')
    })
}

#[derive(Deserialize)]
struct MbArtist {
    #[serde(default)]
    annotation: Option<String>,
    #[serde(default)]
    relations: Vec<Relation>,
}
#[derive(Deserialize)]
struct GroupSearch {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<GroupHit>,
}
#[derive(Debug, Deserialize)]
struct GroupHit {
    #[serde(default)]
    score: u32,
    #[serde(default)]
    title: String,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<Credit>,
}
#[derive(Debug, Deserialize)]
struct Credit {
    #[serde(default)]
    name: String,
    artist: CreditArtist,
}
#[derive(Debug, Deserialize)]
struct CreditArtist {
    id: String,
    #[serde(default)]
    name: String,
}
#[derive(Deserialize)]
struct ArtistSearch {
    #[serde(default)]
    artists: Vec<ArtistHit>,
}
#[derive(Debug, Deserialize)]
struct ArtistHit {
    id: String,
    #[serde(default)]
    score: u32,
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credit(id: &str, name: &str) -> Credit {
        Credit {
            name: name.into(),
            artist: CreditArtist {
                id: id.into(),
                name: name.into(),
            },
        }
    }

    #[test]
    fn an_album_names_the_artist_among_its_credits() {
        let hits = [
            GroupHit {
                score: 100,
                title: "Live Nevermind".into(),
                artist_credit: vec![credit("wrong", "Nirvana")],
            },
            GroupHit {
                score: 98,
                title: "Nevermind".into(),
                artist_credit: vec![credit("guest", "Someone"), credit("right", "Nirvana")],
            },
        ];
        assert_eq!(
            credited_artist(&hits, "Nevermind", "Nirvana").as_deref(),
            Some("right")
        );
        assert_eq!(credited_artist(&hits, "Bleach", "Nirvana"), None);
    }

    #[test]
    fn a_name_search_answers_only_when_unambiguous() {
        let hit = |id: &str, score, name: &str| ArtistHit {
            id: id.into(),
            score,
            name: name.into(),
        };
        assert_eq!(
            unique_artist(
                &[hit("a", 100, "Radiohead"), hit("b", 60, "Radiohead")],
                "radiohead"
            )
            .as_deref(),
            Some("a")
        );
        assert_eq!(
            unique_artist(
                &[hit("a", 100, "Nirvana"), hit("b", 100, "Nirvana")],
                "Nirvana"
            ),
            None
        );
        assert_eq!(
            unique_artist(&[hit("a", 100, "Nirvana UK")], "Nirvana"),
            None
        );
    }

    #[test]
    fn wikipedia_titles_are_matched_with_a_musical_qualifier() {
        assert!(article_matches("Radiohead", "Radiohead"));
        assert!(article_matches("Muse (band)", "Muse"));
        assert!(article_matches("Air (French band)", "Air"));
        assert!(article_matches("Drake (musician)", "Drake"));
        assert!(!article_matches("Air (film)", "Air"));
        assert!(!article_matches("Muse", "Muse (band)"));
    }

    #[test]
    fn a_bare_article_must_be_about_music() {
        assert!(sounds_musical(
            "Radiohead are an English rock band formed in 1985."
        ));
        assert!(sounds_musical(
            "Aubrey Graham is a Canadian rapper and singer."
        ));
        assert!(!sounds_musical(
            "Air is the mixture of gases in Earth's atmosphere."
        ));
        // "banded" is not "band".
        assert!(!sounds_musical("A banded ironstone is a sedimentary rock."));
    }

    #[test]
    fn the_cache_key_ignores_the_albums() {
        let q = |albums: Vec<String>| Query {
            name: "Radiohead".into(),
            mbid: None,
            albums,
            sources: Sources::default(),
        };
        assert_eq!(q(vec![]).cache_key(), q(vec!["Kid A".into()]).cache_key());
        let off = Query {
            sources: Sources {
                wikipedia: false,
                musicbrainz: true,
            },
            ..q(vec![])
        };
        assert_ne!(q(vec![]).cache_key(), off.cache_key());
    }

    #[test]
    fn official_sites_are_typed_not_hosted() {
        let rel = |kind: &str| Relation {
            kind: kind.into(),
            url: None,
        };
        assert!(matches!(
            rel_kind(&rel("official homepage"), "https://radiohead.com"),
            RelKind::Link(LinkKind::Homepage)
        ));
        assert!(matches!(
            rel_kind(&rel("discogs"), "https://www.discogs.com/artist/3840"),
            RelKind::Link(LinkKind::Discogs)
        ));
        assert!(matches!(
            rel_kind(&rel("social network"), "https://twitter.com/x"),
            RelKind::Other
        ));
    }
}
