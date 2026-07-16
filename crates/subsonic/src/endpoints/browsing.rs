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

    /// Global search across artists, albums and songs (ID3).
    ///
    /// Navidrome implements this as simple autocomplete matching.
    pub async fn search3(
        &self,
        query: &str,
        music_folder_id: Option<&LibraryId>,
    ) -> Result<SearchResult3, Error> {
        let mut params: Vec<(&str, &str)> = vec![("query", query)];
        if let Some(id) = music_folder_id {
            params.push(("musicFolderId", id));
        }
        let w: SearchWrapper = self.get("search3", &params).await?;
        Ok(w.result)
    }
}
