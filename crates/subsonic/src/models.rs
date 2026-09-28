//! Typed models for Subsonic/OpenSubsonic responses.
//!
//! Deserialization is tolerant: OpenSubsonic servers add fields freely, and
//! many fields are optional depending on server version and media state.

use serde::{Deserialize, Serialize};

/// A `year` field, with a year no release can have read as missing.
///
/// Servers publish whatever the tag held, and a date written day-first into a
/// year-first field (`©day = "0003-09-2026"`) comes back as year 3, which the
/// views would print under the cover and the sort would file as the oldest
/// record in the library. Nothing recorded predates 1000, so anything below it
/// is a broken tag rather than a date.
fn plausible_year<'de, D>(de: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i32>::deserialize(de)?.filter(|y| *y >= 1000))
}

/// Identifier of a music library ("music folder" in Subsonic terms).
pub type LibraryId = String;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicFolder {
    pub id: serde_json::Value, // servers return int or string; normalized via `id()`
    pub name: Option<String>,
}

impl MusicFolder {
    pub fn id(&self) -> LibraryId {
        match &self.id {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub cover_art: Option<String>,
    pub album_count: Option<u32>,
    pub artist_image_url: Option<String>,
    #[serde(default, alias = "bio")]
    pub biography: Option<String>,
    pub starred: Option<String>,
}

/// Index bucket from getArtists (grouped by initial).
#[derive(Debug, Clone, Deserialize)]
pub struct ArtistIndex {
    pub name: String,
    #[serde(default)]
    pub artist: Vec<Artist>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub cover_art: Option<String>,
    pub song_count: Option<u32>,
    pub duration: Option<u32>,
    pub created: Option<String>,
    #[serde(default, deserialize_with = "plausible_year")]
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
    pub play_count: Option<u64>,
    /// OpenSubsonic per-artist credits (id + name). Empty on vanilla servers,
    /// which carry only the single `artist`/`artist_id` pair — an album credited
    /// to several artists then collapses to whichever one the server picked.
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    /// OpenSubsonic release dates. `year` is only a year, so two records from
    /// the same one can only be ordered alphabetically; these carry the month
    /// and day where the tags have them. `original_release_date` is the first
    /// release of the work and `release_date` this edition's, which is why the
    /// former leads when both are present — a remaster reissued this year
    /// belongs beside the record it is a remaster of.
    #[serde(default)]
    pub original_release_date: Option<ItemDate>,
    #[serde(default)]
    pub release_date: Option<ItemDate>,
    /// OpenSubsonic release types, MusicBrainz vocabulary: the primary type
    /// (`Album`, `EP`, `Single`, …) followed by any secondary ones
    /// (`Compilation`, `Live`, …). Navidrome fills it from the files'
    /// `RELEASETYPE` tags; empty on vanilla servers and untagged libraries.
    #[serde(default)]
    pub release_types: Vec<String>,
}

/// An OpenSubsonic `ItemDate`: a partial date, any component of which may be
/// missing (a server publishing only a year sends `{"year": 2020}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDate {
    #[serde(default, deserialize_with = "plausible_year")]
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
}

impl Album {
    /// Sort key for release order: year, then month, then day.
    ///
    /// Missing components sort *before* a present one within the same year, so
    /// an album dated only `2020` comes after one dated `2020-06-01` under a
    /// newest-first sort — the precise date is the one that earns its place.
    /// An album with no date at all yields `None`, which the caller places
    /// last.
    pub fn release_key(&self) -> Option<(i32, u32, u32)> {
        let date = self
            .original_release_date
            .as_ref()
            .filter(|d| d.year.is_some())
            .or_else(|| self.release_date.as_ref().filter(|d| d.year.is_some()));
        match date {
            Some(d) => Some((d.year?, d.month.unwrap_or(0), d.day.unwrap_or(0))),
            None => self.year.map(|y| (y, 0, 0)),
        }
    }
}

/// Album detail: header + track list (getAlbum).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumWithSongs {
    #[serde(flatten)]
    pub album: Album,
    #[serde(default)]
    pub song: Vec<Song>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub id: String,
    pub title: String,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub artist: Option<String>,
    pub artist_id: Option<String>,
    pub track: Option<u32>,
    pub disc_number: Option<u32>,
    #[serde(default, deserialize_with = "plausible_year")]
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub cover_art: Option<String>,
    pub duration: Option<u32>,
    pub bit_rate: Option<u32>,
    /// OpenSubsonic extension fields; absent on vanilla Subsonic servers.
    pub sampling_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channel_count: Option<u32>,
    pub content_type: Option<String>,
    pub suffix: Option<String>,
    pub size: Option<u64>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
    pub play_count: Option<u64>,
    /// OpenSubsonic loudness-normalization metadata; absent on vanilla servers.
    pub replay_gain: Option<ReplayGain>,
    /// OpenSubsonic per-artist credits (id + name). Empty on vanilla servers;
    /// falls back to the single `artist`/`artist_id` pair.
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    /// Absolute path to a local file. `None` for Subsonic tracks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// Everything else a server says about the track, for the details dialog
    /// and composer credits. Flattened: these are ordinary `Child` fields.
    #[serde(flatten)]
    pub details: SongDetails,
}

/// The rest of a Subsonic `Child`: fields nothing plays or sorts by, kept so
/// the song details dialog can show them.
///
/// Every field is [`lenient`]: these are the extensions servers disagree on
/// most (`isrc` as a string or an array, `bpm` as a string), and one odd
/// value must not fail the whole `getAlbum` it arrived in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SongDetails {
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub created: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub played: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub comment: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub bpm: Option<u32>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub music_brainz_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub isrc: Vec<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub sort_name: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub media_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub explicit_status: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_artist: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_album_artist: Option<String>,
    /// OpenSubsonic joined composer credit ("A, B & C").
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_composer: Option<String>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub album_artists: Vec<ArtistRef>,
    /// OpenSubsonic per-role credits: composer, lyricist, producer, …
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub contributors: Vec<Contributor>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub genres: Vec<ItemGenre>,
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub moods: Vec<String>,
}

impl SongDetails {
    /// Composer credits, with the server's artist id where it has one.
    ///
    /// `contributors` wins (it carries ids); a server without it only has
    /// `displayComposer`, split on the separators tags use between names.
    pub fn composers(&self) -> Vec<(Option<String>, String)> {
        let credited: Vec<_> = self
            .contributors
            .iter()
            .filter(|c| c.role.eq_ignore_ascii_case("composer"))
            .map(|c| {
                let id = Some(c.artist.id.clone()).filter(|id| !id.is_empty());
                (id, c.artist.name.clone())
            })
            .collect();
        if !credited.is_empty() {
            return credited;
        }
        self.display_composer
            .as_deref()
            .map(split_credit)
            .unwrap_or_default()
            .into_iter()
            .map(|name| (None, name))
            .collect()
    }
}

/// Split a joined credit ("A, B & C", "A; B", "A / B") into names.
pub fn split_credit(joined: &str) -> Vec<String> {
    joined
        .split([',', ';', '&', '/'])
        .flat_map(|part| part.split(" and "))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// One OpenSubsonic `contributors` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributor {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_role: Option<String>,
    pub artist: ArtistRef,
}

/// One OpenSubsonic `genres` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemGenre {
    pub name: String,
}

/// A field that reads as its default when the server sent something else
/// (a string where a number belongs, an object where a string does).
fn lenient<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(de)?;
    if let serde_json::Value::String(s) = &value
        && let Ok(n) = s.trim().parse::<u64>()
        && let Ok(parsed) = T::deserialize(serde_json::Value::from(n))
    {
        return Ok(parsed);
    }
    Ok(T::deserialize(value).unwrap_or_default())
}

/// A list that some servers send as a single value; unreadable entries are
/// dropped rather than failing the list.
fn one_or_many<'de, D, T>(de: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|item| T::deserialize(item).ok())
            .collect(),
        serde_json::Value::Null => Vec::new(),
        single => T::deserialize(single).into_iter().collect(),
    })
}

/// A single artist credit as returned in OpenSubsonic `artists` arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
}

/// OpenSubsonic ReplayGain block: gains are in dB, peaks are linear
/// (1.0 = full scale). Any field may be missing depending on server + tags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub album_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_peak: Option<f32>,
    pub base_gain: Option<f32>,
    pub fallback_gain: Option<f32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistWithAlbums {
    #[serde(flatten)]
    pub artist: Artist,
    #[serde(default)]
    pub album: Vec<Album>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: Option<String>,
    pub owner: Option<String>,
    pub public: Option<bool>,
    pub song_count: Option<u32>,
    pub duration: Option<u32>,
    pub created: Option<String>,
    pub changed: Option<String>,
    pub cover_art: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistWithSongs {
    #[serde(flatten)]
    pub playlist: Playlist,
    #[serde(default, rename = "entry")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadioStation {
    pub id: String,
    pub name: String,
    pub stream_url: String,
    pub home_page_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult3 {
    #[serde(default)]
    pub artist: Vec<Artist>,
    #[serde(default)]
    pub album: Vec<Album>,
    #[serde(default)]
    pub song: Vec<Song>,
}

/// Sort order for getAlbumList2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumListType {
    AlphabeticalByName,
    Newest,
    Recent,
    Frequent,
    Random,
    Starred,
}

impl AlbumListType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlphabeticalByName => "alphabeticalByName",
            Self::Newest => "newest",
            Self::Recent => "recent",
            Self::Frequent => "frequent",
            Self::Random => "random",
            Self::Starred => "starred",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_deserializes_without_local_path() {
        let json = r#"{"id":"1","title":"Test","artist":"Artist"}"#;
        let song: Song = serde_json::from_str(json).unwrap();
        assert_eq!(song.id, "1");
        assert_eq!(song.title, "Test");
        assert_eq!(song.local_path, None);
    }

    #[test]
    fn song_serialization_omits_local_path_when_none() {
        let json = r#"{"id":"1","title":"Test","artist":"Artist"}"#;
        let song: Song = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&song).unwrap();
        assert!(!serialized.contains("local_path"));
    }

    #[test]
    fn song_round_trips_local_path() {
        let mut song: Song = serde_json::from_str(r#"{"id":"1","title":"Test"}"#).unwrap();
        song.local_path = Some("/music/test.flac".into());
        let serialized = serde_json::to_string(&song).unwrap();
        let deserialized: Song = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.local_path.as_deref(), Some("/music/test.flac"));
    }

    /// Navidrome's answer for an m4a tagged `©day = "0003-09-2026"`: the
    /// year field and the release date both carry the broken year.
    #[test]
    fn an_implausible_year_reads_as_missing() {
        let json = r#"{"id":"a","name":"advice","year":3,
            "releaseDate":{"year":3,"month":9,"day":20}}"#;
        let album: Album = serde_json::from_str(json).unwrap();
        assert_eq!(album.year, None);
        assert_eq!(album.release_key(), None);

        let song: Song = serde_json::from_str(r#"{"id":"1","title":"t","year":27}"#).unwrap();
        assert_eq!(song.year, None);

        let dated: Album = serde_json::from_str(r#"{"id":"b","name":"b","year":2021}"#).unwrap();
        assert_eq!(dated.year, Some(2021));
        let missing: Album = serde_json::from_str(r#"{"id":"c","name":"c"}"#).unwrap();
        assert_eq!(missing.year, None);
    }
}
