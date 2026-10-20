//! What a genre is, for the search page's genre heading.
//!
//! Rate Your Music has the best genre descriptions, but no public API — its
//! promised one has never shipped, and scraping it is against its terms and
//! behind a bot wall. MusicBrainz has genres but no descriptions. So this asks
//! English Wikipedia, which has an article for nearly every genre a tag will
//! name, through [`album_info`](super::album_info)'s client, retries and cache
//! helpers.
//!
//! A tag is a bare word ("Rock", "Electronic", "Folk") and the bare title is
//! often a disambiguation page or the wrong subject, so titles are tried
//! musical-first: "Rock music", then "Rock (music)", then "Rock". Each answer
//! is believed only when its opening reads like it is about music.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use anyhow::Result;

use super::album_info::{self, Description, normalize, wikipedia_intro};
use crate::config;

/// Titles to try for `genre`, most specific first.
fn candidates(genre: &str) -> Vec<String> {
    let genre = genre.trim();
    if genre.is_empty() {
        return Vec::new();
    }
    let lower = genre.to_lowercase();
    let mut out = Vec::new();
    // "Electronic music" is already the article; "Electronic music music" is
    // not a page anyone wrote.
    if !lower.ends_with("music") {
        out.push(format!("{genre} music"));
        out.push(format!("{genre} (music)"));
    }
    out.push(genre.to_string());
    out
}

/// Whether an article's opening is about a kind of music: the first sentence
/// names a genre, a style, or music at all. "Dance is an art form …" is not;
/// "Dance music is music composed …" is.
fn about_music(text: &str) -> bool {
    let first = text
        .split_inclusive(". ")
        .next()
        .unwrap_or(text)
        .to_lowercase();
    ["genre", "music", "style", "songs"]
        .iter()
        .any(|w| first.contains(w))
}

fn cache_path(genre: &str) -> Option<PathBuf> {
    let key = normalize(genre);
    if key.is_empty() {
        return None;
    }
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    Some(
        config::genre_info_cache_dir()
            .ok()?
            .join(format!("{:016x}.json", h.finish())),
    )
}

/// Look the genre up, reading and writing the disk cache. `Ok(None)` is an
/// answer — no article reads like this genre's — and is cached as one; `Err`
/// is a failed lookup and is not. Must run inside the tokio runtime.
pub async fn fetch(genre: String) -> Result<Option<Description>> {
    let path = cache_path(&genre);
    if let Some(hit) = album_info::cached(path.as_ref()) {
        return Ok(hit);
    }
    let mut found = None;
    for title in candidates(&genre) {
        if let Some(intro) = wikipedia_intro("en", &title).await?
            && about_music(&intro.text)
        {
            found = Some(intro);
            break;
        }
    }
    album_info::store(path.as_ref(), &found);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_tag_is_tried_as_music_first() {
        assert_eq!(
            candidates("Rock"),
            vec!["Rock music", "Rock (music)", "Rock"]
        );
    }

    #[test]
    fn a_tag_already_naming_music_is_tried_as_is() {
        assert_eq!(candidates("Electronic Music"), vec!["Electronic Music"]);
        assert!(candidates("  ").is_empty());
    }

    #[test]
    fn only_an_opening_about_music_is_believed() {
        assert!(about_music(
            "Shoegaze is a subgenre of indie and alternative rock. It is noisy."
        ));
        assert!(about_music(
            "Dance music is music composed specifically to facilitate dancing."
        ));
        assert!(!about_music(
            "Dance is an art form, consisting of sequences of body movements. \
             It is often performed to music."
        ));
        assert!(!about_music("Rock is a naturally occurring solid."));
    }
}
