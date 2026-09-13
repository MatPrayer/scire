//! Lyrics for the songs the server has none of.
//!
//! Navidrome only ever publishes what it found at scan time — the file's tags
//! and a sidecar `.lrc` — so a library tagged without lyrics has none, and
//! local files have none either way (their ids are the scanner's, and no
//! server knows them). Two fallbacks live here, in the order the panel tries
//! them:
//!
//! - [`from_file`] does for a local file what the server's scan does for the
//!   rest of the library: reads its own `.lrc` sidecar and its `LYRICS` tag.
//! - [`fetch`] asks [LRCLIB](https://lrclib.net), the same community database
//!   the `syncedlyrics` tool reaches for first. Free, no key, and it answers
//!   with an LRC document when it has one, so a hit comes back *timed* rather
//!   than as a wall of text.
//!
//! Online results are cached on disk keyed by artist/title/duration. Misses are
//! cached too, or every reopen of the panel re-asks for a song nobody has — but
//! only for [`MISS_TTL`], since the database grows and today's miss is next
//! month's hit. Nothing about a local file is cached: reading it back is a file
//! read of something already on this disk.

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use lofty::{ItemKey, TaggedFileExt};
use serde::Deserialize;
use subsonic::{LyricLine, StructuredLyrics};

use crate::config;

const API: &str = "https://lrclib.net/api";

/// LRCLIB asks clients to identify themselves so it can tell traffic apart.
const UA: &str = concat!(
    "scire/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/LanaMirko04/scire)"
);

/// How long a "nobody has this song" answer stays cached.
const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A search hit this far from the song's own length (seconds) is a different
/// recording — a live cut, an edit — and its timings would not line up.
const DURATION_TOLERANCE: i64 = 2;

/// One LRCLIB track. `synced_lyrics` is an LRC document, `plain_lyrics` the
/// same words without timings; both are null for an instrumental.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Track {
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    plain_lyrics: Option<String>,
    #[serde(default)]
    synced_lyrics: Option<String>,
}

// One client rather than `reqwest::get`, which builds (and drops) a whole
// client per call, TLS handshake included.
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default()
    })
}

/// What a song is looked up by. Everything but the title is optional, since a
/// local file may be tagged with very little.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Track length in seconds.
    pub duration: Option<u32>,
}

impl Query {
    /// Cache filename for this lookup. Hashed rather than sanitized: the three
    /// fields joined run past what a filename can hold, and the sanitizer
    /// flattens every non-alphanumeric byte, so two different songs can end up
    /// at the same path.
    fn cache_key(&self) -> String {
        let mut h = DefaultHasher::new();
        self.title.to_lowercase().hash(&mut h);
        self.artist
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .hash(&mut h);
        self.album
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .hash(&mut h);
        self.duration.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

fn cache_path(query: &Query) -> Option<PathBuf> {
    Some(
        config::lyrics_cache_dir()
            .ok()?
            .join(format!("{}.json", query.cache_key())),
    )
}

/// A cached answer, if there is a usable one. The outer `Option` is "nothing
/// cached", the inner one "cached, and it was a miss".
fn cached(path: Option<&PathBuf>) -> Option<Option<StructuredLyrics>> {
    let path = path?;
    let text = fs::read_to_string(path).ok()?;
    let doc = serde_json::from_str::<Option<StructuredLyrics>>(&text).ok()?;
    if doc.is_none() {
        // A miss goes stale; a hit does not.
        let age = fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())?;
        if age > MISS_TTL {
            return None;
        }
    }
    Some(doc)
}

fn store(path: Option<&PathBuf>, doc: &Option<StructuredLyrics>) {
    let Some(path) = path else { return };
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(doc) {
        let _ = fs::write(path, json);
    }
}

/// Look `query` up online, reading and writing the on-disk cache.
///
/// `Ok(None)` means nobody has lyrics for this song — it is an answer, and is
/// cached as one. Must run inside the tokio runtime (`runtime::spawn_io`).
pub async fn fetch(query: Query) -> Result<Option<StructuredLyrics>> {
    let path = cache_path(&query);
    if let Some(hit) = cached(path.as_ref()) {
        return Ok(hit);
    }
    let found = lookup(&query).await?;
    store(path.as_ref(), &found);
    Ok(found)
}

/// The two requests, in order: the exact-match endpoint when the song carries
/// enough metadata for it, then a search.
///
/// `get` wants artist, album *and* duration and matches all three, so it is
/// both the most accurate answer and the one most libraries cannot ask for —
/// an album tagged differently from LRCLIB's copy misses on the album alone.
/// The search takes artist and title and returns candidates to choose between.
async fn lookup(query: &Query) -> Result<Option<StructuredLyrics>> {
    let exact = match (&query.artist, &query.album, query.duration) {
        (Some(artist), Some(album), Some(secs)) => {
            let params = [
                ("artist_name", artist.as_str()),
                ("track_name", query.title.as_str()),
                ("album_name", album.as_str()),
                ("duration", &secs.to_string()),
            ];
            get_json::<Track>(&format!("{API}/get"), &params).await?
        }
        _ => None,
    };
    if let Some(doc) = exact.and_then(|t| document(&t)) {
        return Ok(Some(doc));
    }

    let mut params = vec![("track_name", query.title.as_str())];
    if let Some(artist) = &query.artist {
        params.push(("artist_name", artist.as_str()));
    }
    let hits = get_json::<Vec<Track>>(&format!("{API}/search"), &params)
        .await?
        .unwrap_or_default();
    Ok(best(&hits, query.duration).and_then(document))
}

/// GET and decode, treating "no such song" as `None` rather than an error.
///
/// LRCLIB answers a miss with 404 and a JSON error body, which would otherwise
/// fail to decode and read as "the lookup broke" — a distinction the caller
/// needs, since only a real answer is worth caching as one.
async fn get_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    params: &[(&str, &str)],
) -> Result<Option<T>> {
    let resp = http().get(url).query(params).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json::<T>().await?))
}

/// Pick one of the search hits.
///
/// Hits with nothing to show are dropped first, so the ranking cannot crown a
/// candidate the panel would then find empty. What is left is ordered by what
/// matters to it: a track whose length matches the song we are playing (a
/// different recording's timings drift out of step within a verse), then one
/// with timings at all. `min_by_key` over a reversed key rather than
/// `max_by_key`, which returns the *last* of several equal maxima — ties should
/// go to LRCLIB's own relevance order, which is the order they arrived in.
fn best(hits: &[Track], duration: Option<u32>) -> Option<&Track> {
    let close = |t: &Track| match (duration, t.duration) {
        (Some(want), Some(got)) => (got as i64 - i64::from(want)).abs() <= DURATION_TOLERANCE,
        // Nothing to compare against is not a mismatch.
        _ => true,
    };
    hits.iter()
        .filter(|t| document(t).is_some())
        .min_by_key(|t| {
            std::cmp::Reverse((
                close(t),
                t.synced_lyrics
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()),
            ))
        })
}

/// Turn a track into the document the panel draws, preferring the timed copy.
fn document(track: &Track) -> Option<StructuredLyrics> {
    if track.instrumental {
        return None;
    }
    track
        .synced_lyrics
        .as_deref()
        .and_then(timed)
        .or_else(|| track.plain_lyrics.as_deref().and_then(plain))
}

/// An LRC document as timed lines. `None` when the text carries no stamps at
/// all — plain words written into a field that promised timings.
fn timed(text: &str) -> Option<StructuredLyrics> {
    let (lines, offset) = parse_lrc(text);
    (!lines.is_empty()).then_some(StructuredLyrics {
        offset,
        synced: true,
        lines,
        ..Default::default()
    })
}

/// The same words with no timings. `None` for a blob of nothing but blank
/// lines, which the panel would draw as an empty page rather than as the miss
/// it is.
fn plain(text: &str) -> Option<StructuredLyrics> {
    let lines: Vec<LyricLine> = text
        .lines()
        .map(|l| LyricLine {
            start: None,
            value: l.to_string(),
        })
        .collect();
    lines
        .iter()
        .any(|l| !l.value.trim().is_empty())
        .then_some(StructuredLyrics {
            lines,
            ..Default::default()
        })
}

/// The lyrics a local file carries itself: a sidecar `.lrc` beside it, or the
/// `LYRICS` tag inside it.
///
/// This is what a server's scan would have collected, done for the files no
/// server ever sees. It is read on demand rather than recorded by the scanner
/// so that editing an `.lrc` shows up the next time the panel is opened instead
/// of at the next scan, and so that a library's worth of lyric text stays out
/// of `music.db` — which is rewritten far more often than it is read for this.
///
/// Both sources are returned when both exist, newest convention first; the
/// caller picks between them by the same rule the server path uses, which is
/// that a timed document wins. Either one may itself be LRC — some taggers
/// write synced lyrics straight into the tag — so both go through the same
/// timed-or-plain test rather than the sidecar being assumed to be the timed
/// one.
pub fn from_file(path: &Path) -> Vec<StructuredLyrics> {
    [sidecar(path), embedded(path)]
        .into_iter()
        .flatten()
        .filter_map(|text| timed(&text).or_else(|| plain(&text)))
        .collect()
}

/// A `.lrc` beside the track, read strictly as UTF-8: a file in some legacy
/// codepage would come back as mojibake with its timings intact, and a panel
/// full of replacement characters is a worse answer than falling through to
/// LRCLIB's own copy. A leading BOM is stripped, or the first line's stamp is
/// not at the start of the line and the whole document parses as untimed.
fn sidecar(path: &Path) -> Option<String> {
    ["lrc", "LRC"]
        .into_iter()
        .map(|ext| path.with_extension(ext))
        .find_map(|p| fs::read(&p).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| text.trim_start_matches('\u{feff}').to_string())
}

/// The file's own lyrics tag (`USLT` in ID3, `©lyr` in MP4, `LYRICS` in Vorbis
/// comments — `ItemKey::Lyrics` is all three). *Every* tag in the file is
/// searched rather than only the primary one: a file carrying both ID3v2 and
/// APE has the words in whichever one its tagger wrote, which is not
/// necessarily the one lofty ranks first.
fn embedded(path: &Path) -> Option<String> {
    let tagged = lofty::read_from_path(path).ok()?;
    tagged
        .tags()
        .iter()
        .find_map(|tag| tag.get_string(&ItemKey::Lyrics))
        .map(str::to_string)
}

/// Parse an LRC document into timed lines, plus the document's own offset in
/// milliseconds.
///
/// Handles `[mm:ss]`, `[mm:ss.xx]` and `[mm:ss.xxx]`, and several stamps on one
/// line — a repeated chorus is written once with a stamp per repeat, and each
/// one is its own line here. The `[ar:…]`/`[ti:…]`/`[by:…]` header every LRC
/// file carries is dropped, since it is metadata rather than words; `[offset:]`
/// is the one header that means something and comes back separately — **with
/// its sign flipped**, since LRC's `+` means "show the lyrics this much
/// earlier" while OpenSubsonic's `offset` — the field this ends up in, and the
/// one the panel adds to every line — means the opposite. One convention has
/// to win or the two sources drift apart; this is the documented one.
///
/// Lines with no stamp at all are dropped rather than kept untimed: in a synced
/// document they are the header's leftovers, and a line the panel cannot place
/// is worse than a line it does not have.
fn parse_lrc(text: &str) -> (Vec<LyricLine>, i64) {
    let mut lines: Vec<LyricLine> = Vec::new();
    let mut offset = 0_i64;

    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut stamps: Vec<i64> = Vec::new();
        // Peel the leading `[...]` groups off; the first thing that is not one
        // begins the words.
        while let Some(body) = rest
            .strip_prefix('[')
            .and_then(|r| r.find(']').map(|i| &r[..i]))
        {
            rest = &rest[body.len() + 2..];
            match parse_stamp(body) {
                Some(ms) => stamps.push(ms),
                None => {
                    if let Some(v) = body.strip_prefix("offset:") {
                        // `+` in an LRC file pulls the words forward, which is
                        // a negative offset in the sense the panel applies.
                        // `i64`'s own parser takes the leading sign either way.
                        offset = -v.trim().parse().unwrap_or(0);
                    }
                }
            }
        }
        let value = rest.trim().to_string();
        for start in stamps {
            lines.push(LyricLine {
                start: Some(start),
                value: value.clone(),
            });
        }
    }

    lines.sort_by_key(|l| l.start);
    (lines, offset)
}

/// `mm:ss`, `mm:ss.xx` or `mm:ss.xxx` as milliseconds. Anything else — a
/// metadata tag — is None.
fn parse_stamp(body: &str) -> Option<i64> {
    let (mins, rest) = body.split_once(':')?;
    let mins: i64 = mins.trim().parse().ok()?;
    let (secs, frac) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    let secs: i64 = secs.trim().parse().ok()?;
    let millis = match frac {
        // Two digits are hundredths, three are milliseconds.
        Some(f) => {
            let digits: String = f.chars().take_while(char::is_ascii_digit).collect();
            let n: i64 = digits.parse().ok()?;
            match digits.len() {
                0 => return None,
                1 => n * 100,
                2 => n * 10,
                _ => n,
            }
        }
        None => 0,
    };
    Some(mins * 60_000 + secs * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(synced: Option<&str>, plain: Option<&str>, duration: Option<f64>) -> Track {
        Track {
            duration,
            instrumental: false,
            plain_lyrics: plain.map(str::to_string),
            synced_lyrics: synced.map(str::to_string),
        }
    }

    #[test]
    fn stamps_become_milliseconds() {
        assert_eq!(parse_stamp("00:00.00"), Some(0));
        assert_eq!(parse_stamp("01:02.34"), Some(62_340));
        assert_eq!(parse_stamp("01:02.345"), Some(62_345));
        assert_eq!(parse_stamp("02:00"), Some(120_000));
    }

    #[test]
    fn metadata_tags_are_not_stamps() {
        assert_eq!(parse_stamp("ar:Someone"), None);
        assert_eq!(parse_stamp("offset:+500"), None);
        assert_eq!(parse_stamp("length"), None);
    }

    #[test]
    fn lrc_header_is_dropped_and_offset_kept() {
        let (lines, offset) = parse_lrc(
            "[ar:Someone]\n[ti:A Song]\n[offset:-250]\n[00:12.00]first\n[00:15.50]second\n",
        );
        // Flipped into the sense the panel applies: LRC's `-` delays the words.
        assert_eq!(offset, 250);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].value, "first");
        assert_eq!(lines[0].start, Some(12_000));
        assert_eq!(lines[1].start, Some(15_500));
    }

    #[test]
    fn a_positive_lrc_offset_comes_back_negative() {
        let (_, offset) = parse_lrc("[offset:+500]\n[00:01.00]a\n");
        assert_eq!(offset, -500);
    }

    #[test]
    fn repeated_stamps_become_one_line_each() {
        let (lines, _) = parse_lrc("[00:10.00][01:20.00]chorus\n");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.value == "chorus"));
        assert_eq!(lines[0].start, Some(10_000));
        assert_eq!(lines[1].start, Some(80_000));
    }

    /// The disk cache is the only thing that serializes `StructuredLyrics`, so
    /// nothing else would notice the two halves drifting apart.
    #[test]
    fn a_cached_document_comes_back_the_same() {
        let doc = document(&track(Some("[00:01.50]a\n[00:09.00]b"), None, None));
        let json = serde_json::to_string(&doc).unwrap();
        let back: Option<StructuredLyrics> = serde_json::from_str(&json).unwrap();
        let back = back.unwrap();
        assert!(back.synced);
        assert_eq!(back.lines.len(), 2);
        assert_eq!(back.lines[0].start, Some(1_500));
        assert_eq!(back.lines[1].value, "b");
        // And a miss round-trips as the miss it is, not as an empty document.
        let miss: Option<StructuredLyrics> = serde_json::from_str("null").unwrap();
        assert!(miss.is_none());
    }

    #[test]
    fn lines_come_back_in_time_order() {
        let (lines, _) = parse_lrc("[01:20.00]late\n[00:10.00]early\n");
        assert_eq!(lines[0].value, "early");
        assert_eq!(lines[1].value, "late");
    }

    #[test]
    fn untimed_lines_are_dropped() {
        let (lines, _) = parse_lrc("stray text\n[00:01.00]timed\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].value, "timed");
    }

    #[test]
    fn an_empty_timed_line_is_kept_as_a_gap() {
        let (lines, _) = parse_lrc("[00:01.00]a\n[00:20.00]\n[00:30.00]b\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].value, "");
    }

    #[test]
    fn synced_wins_over_plain_in_one_track() {
        let doc = document(&track(Some("[00:01.00]hi"), Some("hi"), None)).unwrap();
        assert!(doc.synced);
        assert_eq!(doc.lines[0].start, Some(1_000));
    }

    #[test]
    fn plain_is_used_when_there_are_no_timings() {
        let doc = document(&track(None, Some("one\ntwo"), None)).unwrap();
        assert!(!doc.synced);
        assert_eq!(doc.lines.len(), 2);
        assert!(doc.lines.iter().all(|l| l.start.is_none()));
    }

    /// A directory of this test's own, plus a stand-in "track" to hang a
    /// sidecar off. The bytes are not audio, so `embedded` fails its read and
    /// the sidecar is the only source — which is the half being tested.
    fn local_track(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scire-lyrics-{}-{name}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let track = dir.join("song.flac");
        fs::write(&track, b"not audio").unwrap();
        track
    }

    #[test]
    fn a_sidecar_lrc_is_read_as_a_timed_document() {
        let track = local_track("sidecar");
        fs::write(
            track.with_extension("lrc"),
            "[00:01.00]one\n[00:05.50]two\n",
        )
        .unwrap();
        let docs = from_file(&track);
        assert_eq!(docs.len(), 1);
        assert!(docs[0].synced);
        assert_eq!(docs[0].lines[1].start, Some(5_500));
    }

    /// Plenty of sidecars are just the words. They are still the file's own
    /// lyrics, and the panel draws an untimed document fine.
    #[test]
    fn a_sidecar_without_stamps_is_read_untimed() {
        let track = local_track("untimed");
        fs::write(track.with_extension("lrc"), "one\ntwo\n").unwrap();
        let docs = from_file(&track);
        assert_eq!(docs.len(), 1);
        assert!(!docs[0].synced);
        assert_eq!(docs[0].lines.len(), 2);
    }

    /// A Windows editor's BOM sits in front of the first `[`, which would make
    /// the whole document parse as untimed.
    #[test]
    fn a_leading_bom_does_not_cost_the_first_stamp() {
        let track = local_track("bom");
        fs::write(track.with_extension("lrc"), "\u{feff}[00:01.00]one\n").unwrap();
        let docs = from_file(&track);
        assert!(docs[0].synced);
        assert_eq!(docs[0].lines[0].start, Some(1_000));
    }

    /// Mojibake with working timings is a worse answer than none: leaving it
    /// out is what lets the online lookup have a go at the song.
    #[test]
    fn a_sidecar_that_is_not_utf8_is_skipped() {
        let track = local_track("cp1251");
        fs::write(track.with_extension("lrc"), [0x5b, 0x30, 0x30, 0xff, 0xfe]).unwrap();
        assert!(from_file(&track).is_empty());
    }

    #[test]
    fn a_file_with_no_lyrics_anywhere_yields_nothing() {
        assert!(from_file(&local_track("bare")).is_empty());
    }

    #[test]
    fn an_instrumental_has_no_document() {
        let mut t = track(None, None, None);
        t.instrumental = true;
        assert!(document(&t).is_none());
        // And a track with nothing in it at all is no different.
        assert!(document(&track(Some("   "), Some("  \n "), None)).is_none());
    }

    #[test]
    fn a_matching_length_beats_a_timed_mismatch() {
        let hits = vec![
            track(Some("[00:01.00]wrong take"), None, Some(400.)),
            track(None, Some("right take"), Some(181.)),
        ];
        let picked = best(&hits, Some(180)).unwrap();
        assert_eq!(picked.plain_lyrics.as_deref(), Some("right take"));
    }

    #[test]
    fn timings_win_among_equally_close_hits() {
        let hits = vec![
            track(None, Some("plain"), Some(180.)),
            track(Some("[00:01.00]synced"), None, Some(180.)),
        ];
        assert!(best(&hits, Some(180)).unwrap().synced_lyrics.is_some());
    }

    #[test]
    fn empty_and_instrumental_results_are_not_picked() {
        let mut instrumental = track(None, None, Some(180.));
        instrumental.instrumental = true;
        let hits = vec![instrumental, track(None, Some(""), Some(180.))];
        assert!(best(&hits, Some(180)).is_none());
        assert!(best(&[], Some(180)).is_none());
    }

    #[test]
    fn an_unknown_length_does_not_rule_hits_out() {
        let hits = vec![track(None, Some("words"), Some(999.))];
        assert!(best(&hits, Some(180)).is_some());
        assert!(best(&hits, None).is_some());
    }

    #[test]
    fn cache_keys_separate_songs_and_ignore_case() {
        let q = |title: &str, artist: &str| Query {
            title: title.into(),
            artist: Some(artist.into()),
            album: None,
            duration: Some(180),
        };
        assert_eq!(q("Song", "Band").cache_key(), q("song", "band").cache_key());
        assert_ne!(
            q("Song", "Band").cache_key(),
            q("Song", "Other").cache_key()
        );
        assert_ne!(
            q("Song", "Band").cache_key(),
            q("Other", "Band").cache_key()
        );
    }
}
