use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::{Album, Artist, LibraryId, Song};

/// Which kind of item an annotation applies to. Subsonic uses different
/// query params for songs (`id`), albums (`albumId`) and artists (`artistId`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationTarget {
    Song,
    Album,
    Artist,
}

impl AnnotationTarget {
    fn param(self) -> &'static str {
        match self {
            Self::Song => "id",
            Self::Album => "albumId",
            Self::Artist => "artistId",
        }
    }
}

/// Starred items from getStarred2 (ID3 variants).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Starred {
    #[serde(default)]
    pub artist: Vec<Artist>,
    #[serde(default)]
    pub album: Vec<Album>,
    #[serde(default)]
    pub song: Vec<Song>,
}

#[derive(Debug, Deserialize)]
struct StarredWrapper {
    #[serde(rename = "starred2")]
    starred: Starred,
}

impl SubsonicClient {
    pub async fn star(&self, target: AnnotationTarget, id: &str) -> Result<(), Error> {
        self.get_empty("star", &[(target.param(), id)]).await
    }

    pub async fn unstar(&self, target: AnnotationTarget, id: &str) -> Result<(), Error> {
        self.get_empty("unstar", &[(target.param(), id)]).await
    }

    /// Rate a song/album/artist 1-5; 0 removes the rating.
    /// setRating always uses `id` regardless of item kind.
    pub async fn set_rating(&self, id: &str, rating: u8) -> Result<(), Error> {
        let r = rating.min(5).to_string();
        self.get_empty("setRating", &[("id", id), ("rating", &r)])
            .await
    }

    /// Starred songs/albums/artists (ID3).
    pub async fn get_starred2(
        &self,
        music_folder_id: Option<&LibraryId>,
    ) -> Result<Starred, Error> {
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(id) = music_folder_id {
            params.push(("musicFolderId", id));
        }
        let w: StarredWrapper = self.get("getStarred2", &params).await?;
        Ok(w.starred)
    }
}
