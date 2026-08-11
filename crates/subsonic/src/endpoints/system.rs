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

/// State of the server's own media scan (`startScan` / `getScanStatus`).
///
/// `count` is the number of media files scanned so far; servers report it only
/// while scanning, and some omit it entirely.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    #[serde(default)]
    pub scanning: bool,
    #[serde(default)]
    pub count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ScanStatusWrapper {
    #[serde(rename = "scanStatus")]
    scan_status: ScanStatus,
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

    /// Ask the server to rescan its media library, returning the scan state.
    ///
    /// Navidrome restricts this to admin users and answers `50` (not
    /// authorized) otherwise, so callers must treat a failure as "the server
    /// won't rescan for us" rather than as a fatal error.
    pub async fn start_scan(&self) -> Result<ScanStatus, Error> {
        let w: ScanStatusWrapper = self.get("startScan", &[]).await?;
        Ok(w.scan_status)
    }

    /// Current state of the server's media scan.
    pub async fn get_scan_status(&self) -> Result<ScanStatus, Error> {
        let w: ScanStatusWrapper = self.get("getScanStatus", &[]).await?;
        Ok(w.scan_status)
    }
}
