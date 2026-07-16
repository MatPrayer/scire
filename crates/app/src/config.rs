//! Settings persistence (TOML) and credential storage (OS keyring).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::state::queue::RepeatMode;

const KEYRING_SERVICE: &str = "navidrome-rusty-client";

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("com", "mirko", "navidrome-rusty-client")
        .context("cannot determine platform config directories")
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("settings.toml"))
}

pub fn artwork_cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("artwork"))
}

pub fn recent_played_path() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("recently_played.json"))
}

pub fn waveform_cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("waveform"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: Option<ServerConfig>,
    /// Volume in [0.0, 1.0].
    pub volume: f32,
    /// Legacy single-library selection; migrated into `library_ids` on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_id: Option<String>,
    /// Selected library (music folder) ids; empty = all libraries.
    pub library_ids: Vec<String>,
    /// Streaming/transcoding preferences.
    pub transcoding: Transcoding,
    /// Colour theme.
    pub theme: ThemePref,
    /// Draw the in-app title bar (gpui-component `TitleBar`). When false, use native WM chrome.
    pub client_titlebar: bool,
    /// Forward now-playing / scrobble submissions to the server.
    pub scrobble_enabled: bool,
    /// Default shuffle state for new sessions.
    pub default_shuffle: bool,
    /// Default repeat mode for new sessions.
    pub default_repeat: RepeatMode,
    /// On-disk artwork cache cap in megabytes.
    pub artwork_cache_mb: u32,
    /// Page shown right after connecting.
    pub default_page: DefaultPage,
    /// Last selected album list filter, restored across sessions.
    pub album_sort: AlbumSort,
    /// Cover-art tile size in the album grid.
    pub cover_size: CoverSize,
    /// Extra columns shown next to song titles in track lists.
    pub track_info: TrackInfo,
    /// Render the seek bar as the track's waveform (downloads each track a
    /// second time to decode it).
    pub waveform_seekbar: bool,
}

/// Cover-art tile size for the album grid. The value doubles as the pixel
/// resolution requested/decoded for grid thumbnails, so smaller tiles fetch
/// and render smaller textures — full-res art is only used on detail pages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverSize {
    Small,
    #[default]
    Medium,
    Large,
    ExtraLarge,
}

impl CoverSize {
    /// Rendered tile edge in logical pixels.
    pub fn px(self) -> f32 {
        match self {
            Self::Small => 120.,
            Self::Medium => 160.,
            Self::Large => 200.,
            Self::ExtraLarge => 260.,
        }
    }

    /// Resolution to request/decode for grid thumbnails. Bumped ~1.5× over the
    /// tile size so HiDPI screens stay crisp without decoding full art.
    pub fn art_px(self) -> u32 {
        (self.px() * 1.5) as u32
    }
}

/// Which section opens after a successful connect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultPage {
    #[default]
    Albums,
    Artists,
    Favorites,
    Recent,
    Radio,
}

/// Album grid sort/filter, mirrors the Subsonic getAlbumList2 types we expose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlbumSort {
    #[default]
    All,
    New,
    Recent,
    Frequent,
    Random,
    Starred,
}

/// Which extra fields to show next to song titles in album/playlist views.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    pub artist: bool,
    pub album: bool,
    pub year: bool,
    pub genre: bool,
    pub bitrate: bool,
    pub plays: bool,
}

impl Default for TrackInfo {
    fn default() -> Self {
        Self {
            artist: true,
            album: false,
            year: false,
            genre: false,
            bitrate: false,
            plays: false,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: None,
            volume: 1.0,
            library_id: None,
            library_ids: Vec::new(),
            transcoding: Transcoding::default(),
            theme: ThemePref::default(),
            client_titlebar: true,
            scrobble_enabled: true,
            default_shuffle: false,
            default_repeat: RepeatMode::Off,
            artwork_cache_mb: 256,
            default_page: DefaultPage::default(),
            album_sort: AlbumSort::default(),
            cover_size: CoverSize::default(),
            track_info: TrackInfo::default(),
            waveform_seekbar: false,
        }
    }
}

/// Streaming preferences applied when building stream URLs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Transcoding {
    /// Target format (e.g. "mp3", "opus"). None/empty = server raw stream.
    pub format: Option<String>,
    /// Max bitrate in kbps. None/0 = no cap.
    pub max_bit_rate: Option<u32>,
}

impl Transcoding {
    pub fn to_stream_options(&self) -> subsonic::StreamOptions {
        subsonic::StreamOptions {
            format: self.format.clone().filter(|f| !f.is_empty()),
            max_bit_rate: self.max_bit_rate.filter(|&r| r > 0),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
    #[serde(rename = "custom")]
    Custom,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportedThemeDefinition {
    pub name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub foreground: Option<String>,
    #[serde(default)]
    pub border: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub muted_foreground: Option<String>,
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub primary_foreground: Option<String>,
    #[serde(default)]
    pub secondary: Option<String>,
    #[serde(default)]
    pub secondary_foreground: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub accent_foreground: Option<String>,
    #[serde(default)]
    pub sidebar: Option<String>,
    #[serde(default)]
    pub sidebar_foreground: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    #[serde(default)]
    pub success_foreground: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub warning_foreground: Option<String>,
    #[serde(default)]
    pub danger: Option<String>,
    #[serde(default)]
    pub danger_foreground: Option<String>,
    #[serde(default)]
    pub selection: Option<String>,
    #[serde(default)]
    pub scrollbar_thumb: Option<String>,
    #[serde(default)]
    pub scrollbar_thumb_hover: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportedThemesFile {
    #[serde(default)]
    pub themes: Vec<ImportedThemeDefinition>,
}

impl ImportedThemesFile {
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, anyhow::Error> {
        let text = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    /// Plaintext password fallback for systems without a usable keyring.
    /// Only written when the keyring store fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_plaintext: Option<String>,
}

impl Settings {
    pub fn load() -> Result<Self> {
        let path = settings_path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                let mut settings: Self =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                // Migrate the pre-multi-select single library selection.
                if settings.library_ids.is_empty()
                    && let Some(id) = settings.library_id.take()
                {
                    settings.library_ids = vec![id];
                }
                settings.library_id = None;
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                volume: 1.0,
                ..Default::default()
            }),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Keyring account name: `user@host` so multiple servers can coexist later.
fn keyring_account(url: &str, username: &str) -> String {
    let host = url::host(url).unwrap_or_else(|| url.to_string());
    format!("{username}@{host}")
}

mod url {
    /// Tiny host extractor to avoid pulling a full URL crate here.
    pub fn host(url: &str) -> Option<String> {
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let host = rest.split(['/', '?']).next()?;
        if host.is_empty() {
            None
        } else {
            Some(host.to_string())
        }
    }
}

pub fn store_password(server_url: &str, username: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(server_url, username))?;
    entry.set_password(password)?;
    Ok(())
}

pub fn load_password(server_url: &str, username: &str) -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(server_url, username))?;
    Ok(entry.get_password()?)
}

pub fn delete_password(server_url: &str, username: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(server_url, username))
    {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::ImportedThemesFile;

    #[test]
    fn imported_theme_json_deserializes_named_theme() {
        let data = r###"{"themes":[{"name":"My Theme","mode":"dark","background":"#000000","foreground":"#ffffff"}] }"###;
        let themes = serde_json::from_str::<ImportedThemesFile>(data).unwrap();
        assert_eq!(themes.themes.len(), 1);
        let theme = &themes.themes[0];
        assert_eq!(theme.name, "My Theme");
        assert_eq!(theme.mode, "dark");
        assert_eq!(theme.background.as_deref(), Some("#000000"));
        assert_eq!(theme.foreground.as_deref(), Some("#ffffff"));
    }
}
