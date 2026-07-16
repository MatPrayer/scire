use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::{AlbumWithSongs, ArtistIndex, ArtistWithAlbums, LibraryId, SearchResult3};

#[derive(Debug, Deserialize)]
struct ArtistsWrapper {
    artists: ArtistsInner,
}

#[derive(Debug, Deserialize)]
struct ArtistsInner {
    #[serde(default)]
    index: Vec<ArtistIndex>,
}

#[derive(Debug, Deserialize)]
struct ArtistWrapper {
    artist: ArtistWithAlbums,
}

#[derive(Debug, Deserialize)]
struct AlbumWrapper {
    album: AlbumWithSongs,
}

#[derive(Debug, Deserialize)]
struct SearchWrapper {
    #[serde(rename = "searchResult3")]
    result: SearchResult3,
}

/// Artist metadata from getArtistInfo2 (ID3). Navidrome fills biography and
/// image URLs from its configured agents (Last.fm, Spotify, local files).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistInfo2 {
    /// May contain HTML (e.g. a trailing `<a>Read more…</a>` from Last.fm).
    pub biography: Option<String>,
    pub music_brainz_id: Option<String>,
    pub last_fm_url: Option<String>,
    pub small_image_url: Option<String>,
    pub medium_image_url: Option<String>,
    pub large_image_url: Option<String>,
}

impl ArtistInfo2 {
    /// Best available image URL, largest first; empty strings are skipped.
    pub fn image_url(&self) -> Option<&str> {
        [
            &self.large_image_url,
            &self.medium_image_url,
            &self.small_image_url,
        ]
        .into_iter()
        .filter_map(|u| u.as_deref())
        .map(str::trim)
        .find(|u| !u.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct ArtistInfo2Wrapper {
    #[serde(rename = "artistInfo2", default)]
    info: ArtistInfo2,
}

impl SubsonicClient {
    /// All artists (ID3), grouped by index letter.
    pub async fn get_artists(
        &self,
        music_folder_id: Option<&LibraryId>,
    ) -> Result<Vec<ArtistIndex>, Error> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(id) = music_folder_id {
            params.push(("musicFolderId", id));
        }
        let w: ArtistsWrapper = self.get("getArtists", &params).await?;
        Ok(w.artists.index)
    }

    /// One artist with their albums (ID3).
    pub async fn get_artist(&self, id: &str) -> Result<ArtistWithAlbums, Error> {
        let w: ArtistWrapper = self.get("getArtist", &[("id", id)]).await?;
        Ok(w.artist)
    }

    /// One album with its tracks (ID3).
    pub async fn get_album(&self, id: &str) -> Result<AlbumWithSongs, Error> {
        let w: AlbumWrapper = self.get("getAlbum", &[("id", id)]).await?;
        Ok(w.album)
    }

    /// Artist biography and image URLs (ID3, OpenSubsonic getArtistInfo2).
    pub async fn get_artist_info2(&self, id: &str) -> Result<ArtistInfo2, Error> {
        let w: ArtistInfo2Wrapper = self.get("getArtistInfo2", &[("id", id)]).await?;
        Ok(w.info)
    }

    /// Global search across artists, albums and songs (ID3).
    ///
    /// Navidrome implements this as simple autocomplete matching.
    pub async fn search3(
        &self,
        query: &str,
        music_folder_id: Option<&LibraryId>,
    ) -> Result<SearchResult3, Error> {
        // Explicit counts: don't rely on server defaults for any category.
        let mut params: Vec<(&str, &str)> = vec![
            ("query", query),
            ("artistCount", "12"),
            ("albumCount", "12"),
            ("songCount", "25"),
        ];
        if let Some(id) = music_folder_id {
            params.push(("musicFolderId", id));
        }
        let w: SearchWrapper = self.get("search3", &params).await?;
        Ok(w.result)
    }
}
