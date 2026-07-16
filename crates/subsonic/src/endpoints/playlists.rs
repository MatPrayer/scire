use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::{Playlist, PlaylistWithSongs};

#[derive(Debug, Deserialize)]
struct PlaylistsWrapper {
    playlists: PlaylistsInner,
}

#[derive(Debug, Deserialize)]
struct PlaylistsInner {
    #[serde(default)]
    playlist: Vec<Playlist>,
}

#[derive(Debug, Deserialize)]
struct PlaylistWrapper {
    playlist: PlaylistWithSongs,
}

impl SubsonicClient {
    /// All playlists visible to the user.
    pub async fn get_playlists(&self) -> Result<Vec<Playlist>, Error> {
        let w: PlaylistsWrapper = self.get("getPlaylists", &[]).await?;
        Ok(w.playlists.playlist)
    }

    /// One playlist with its songs.
    pub async fn get_playlist(&self, id: &str) -> Result<PlaylistWithSongs, Error> {
        let w: PlaylistWrapper = self.get("getPlaylist", &[("id", id)]).await?;
        Ok(w.playlist)
    }

    /// Create a playlist with the given songs; returns the created playlist.
    pub async fn create_playlist(
        &self,
        name: &str,
        song_ids: &[&str],
    ) -> Result<PlaylistWithSongs, Error> {
        let mut params: Vec<(&str, &str)> = vec![("name", name)];
        for id in song_ids {
            params.push(("songId", id));
        }
        // Navidrome returns the playlist in the response envelope.
        let w: PlaylistWrapper = self.get("createPlaylist", &params).await?;
        Ok(w.playlist)
    }

    /// Update a playlist: rename and/or add/remove songs.
    ///
    /// `remove_indices` are positions within the playlist (per the spec:
    /// `songIndexToRemove`), not song ids.
    pub async fn update_playlist(
        &self,
        playlist_id: &str,
        new_name: Option<&str>,
        add_song_ids: &[&str],
        remove_indices: &[u32],
    ) -> Result<(), Error> {
        let mut params: Vec<(&str, &str)> = vec![("playlistId", playlist_id)];
        if let Some(name) = new_name {
            params.push(("name", name));
        }
        for id in add_song_ids {
            params.push(("songIdToAdd", id));
        }
        let indices: Vec<String> = remove_indices.iter().map(|i| i.to_string()).collect();
        for idx in &indices {
            params.push(("songIndexToRemove", idx));
        }
        self.get_empty("updatePlaylist", &params).await
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<(), Error> {
        self.get_empty("deletePlaylist", &[("id", id)]).await
    }
}
