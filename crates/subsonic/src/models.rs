//! Typed models for Subsonic/OpenSubsonic responses.
//!
//! Deserialization is tolerant: OpenSubsonic servers add fields freely, and
//! many fields are optional depending on server version and media state.

use serde::{Deserialize, Serialize};

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
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
    pub play_count: Option<u64>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    pub id: String,
    pub url: String,
    pub description: Option<String>,
    pub created: Option<String>,
    pub expires: Option<String>,
    pub visit_count: Option<u64>,
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
    AlphabeticalByArtist,
    Newest,
    Recent,
    Frequent,
    Random,
    Starred,
    ByYear,
    ByGenre,
}

impl AlbumListType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlphabeticalByName => "alphabeticalByName",
            Self::AlphabeticalByArtist => "alphabeticalByArtist",
            Self::Newest => "newest",
            Self::Recent => "recent",
            Self::Frequent => "frequent",
            Self::Random => "random",
            Self::Starred => "starred",
            Self::ByYear => "byYear",
            Self::ByGenre => "byGenre",
        }
    }
}
