use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::MusicFolder;

/// Server info from `ping` (OpenSubsonic servers include version details).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    #[serde(default)]
    pub server_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MusicFoldersWrapper {
    #[serde(rename = "musicFolders")]
    music_folders: MusicFoldersInner,
}

#[derive(Debug, Deserialize)]
struct MusicFoldersInner {
    #[serde(rename = "musicFolder", default)]
    items: Vec<MusicFolder>,
}

impl SubsonicClient {
    /// Validate connectivity and credentials.
    pub async fn ping(&self) -> Result<ServerInfo, Error> {
        self.get("ping", &[]).await
    }

    /// Libraries ("music folders") the authenticated user can access.
    pub async fn get_music_folders(&self) -> Result<Vec<MusicFolder>, Error> {
        let w: MusicFoldersWrapper = self.get("getMusicFolders", &[]).await?;
        Ok(w.music_folders.items)
    }
}
