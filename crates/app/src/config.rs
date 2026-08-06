//! Settings persistence (TOML) and credential storage (OS keyring).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::state::queue::RepeatMode;

const KEYRING_SERVICE: &str = "scire";

pub(crate) fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "scire").context("cannot determine platform config directories")
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

pub fn queue_path() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("queue.json"))
}

/// Where the current track's playback position is parked between runs (see
/// `Settings::resume_playback`). Separate from the queue file because it is
/// rewritten every few seconds while the queue only changes on edits.
pub fn resume_path() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("resume.json"))
}

#[allow(dead_code)]
pub fn library_db_path() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("music.db"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: Option<ServerConfig>,
    /// Volume in [0.0, 1.0].
    pub volume: f32,
    /// Legacy single-library selection; migrated into `library_ids` on load.
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
    /// Show format/bitrate/sample-rate of the current track in the player bar.
    pub stream_info_bar: bool,
    /// Show a precise percentage readout next to the volume slider.
    pub detailed_volume: bool,
    /// Show the queue-toggle button in the bottom player bar.
    pub show_queue_button: bool,
    /// ReplayGain loudness-normalization mode.
    pub replay_gain: ReplayGainMode,
    /// Chosen audio output device (cpal description name); None = OS default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    /// What to do when the play queue reaches its end.
    pub queue_end: QueueEndBehavior,
    /// Background style of the fullscreen now-playing overlay.
    pub fullscreen_bg: FullscreenBackground,
    /// Directories to scan for local music files. Empty = local music disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_music_dirs: Vec<PathBuf>,
    /// Show the vertical volume slider in the fullscreen now-playing overlay.
    pub fullscreen_volume: bool,
    /// Scene drawn by the fullscreen visualizer; Off hides it. Persisted so the
    /// overlay comes back the way it was left.
    pub visualizer: VisualizerMode,
    /// Per-scene sensitivity/intensity knobs for the visualizer.
    pub visualizer_tuning: VisualizerSettings,
    /// Remember where the current track was when the app closed and pick it up
    /// there on the next launch. The queue is always restored; this adds the
    /// position within its current track.
    pub resume_playback: bool,
    /// Sidebar library switcher folded away; restored across sessions.
    pub sidebar_libraries_collapsed: bool,
    /// Sidebar playlist list folded away; restored across sessions.
    pub sidebar_playlists_collapsed: bool,
}

/// ReplayGain normalization source. Track uses per-track gain; Album keeps
/// relative loudness within an album (falls back to track gain when absent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
    /// Album gain when the queue is a single album, track gain otherwise.
    Auto,
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

/// Background style for the fullscreen now-playing overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullscreenBackground {
    /// Solid theme background.
    Solid,
    /// Dark two-tone gradient from the album palette.
    #[default]
    Gradient,
    /// Brighter, more saturated album-palette gradient.
    Vibrant,
    /// The cover art blown up (soft/blurred) behind a dark scrim.
    BlurredArt,
    /// Slowly rotating album-palette gradient.
    Animated,
}

/// Scene drawn by the fullscreen 3D audio visualizer. The fullscreen player's
/// button cycles through these in declaration order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualizerMode {
    #[default]
    Off,
    /// Alternate between the scenes by itself, switching on the music (see
    /// `ui::visualizer::OnsetSwitcher`).
    Auto,
    /// Scrolling spectrum landscape: frequency across, time into the distance.
    Terrain,
    /// Flight through rings whose radius is modulated by the spectrum.
    Tunnel,
    /// Rotating point cloud displaced along its normals by the spectrum.
    Sphere,
    /// Randomly generated wireframe shapes flying at the camera over a
    /// reactive background — the 2000s media-player look.
    Retro,
    /// Wireframe icosphere: bass inflates it, treble roughens its surface.
    Orb,
    /// Polar oscilloscope: the waveform itself wrapped around a ring, with the
    /// previous frames trailing behind it.
    Scope,
    /// Kaleidoscope mandala: the spectrum mirrored into rotating petals.
    Bloom,
    /// Starfield streaking past the camera, accelerating with the track.
    Warp,
}

impl VisualizerMode {
    /// Next mode in the cycle, wrapping back to `Off`. `Auto` comes first so
    /// the music-driven mode — the point of the feature — is one click away,
    /// with the pinned single scenes after it.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Auto,
            Self::Auto => Self::Terrain,
            Self::Terrain => Self::Tunnel,
            Self::Tunnel => Self::Sphere,
            Self::Sphere => Self::Retro,
            Self::Retro => Self::Orb,
            Self::Orb => Self::Scope,
            Self::Scope => Self::Bloom,
            Self::Bloom => Self::Warp,
            Self::Warp => Self::Off,
        }
    }

    /// Button label: the scene's name while it is running, otherwise the
    /// feature's name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Visualizer",
            Self::Auto => "Auto",
            Self::Terrain => "Terrain",
            Self::Tunnel => "Tunnel",
            Self::Sphere => "Sphere",
            Self::Retro => "Retro",
            Self::Orb => "Orb",
            Self::Scope => "Scope",
            Self::Bloom => "Bloom",
            Self::Warp => "Warp",
        }
    }

    pub fn is_on(self) -> bool {
        self != Self::Off
    }

    /// Label inside the scene menu, where "Off" is one option among many and
    /// the button-face wording ("Visualizer") would make no sense.
    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            other => other.label(),
        }
    }

    /// The scenes, i.e. everything except the two behaviours (`Off`, `Auto`).
    pub const SCENES: [VisualizerMode; 8] = [
        VisualizerMode::Terrain,
        VisualizerMode::Tunnel,
        VisualizerMode::Sphere,
        VisualizerMode::Retro,
        VisualizerMode::Orb,
        VisualizerMode::Scope,
        VisualizerMode::Bloom,
        VisualizerMode::Warp,
    ];
}

/// Tuning knobs for the fullscreen visualizer. All are multipliers around 1.0
/// (or 0..1 mixes) so the defaults reproduce the untuned look exactly, and a
/// value out of the UI's range still behaves sanely.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualizerSettings {
    /// Gain on the analysed band levels before they are drawn. Quiet or
    /// heavily compressed masters need more than 1.0 to move the geometry.
    pub sensitivity: f32,
    /// 0 = snap to the spectrum (twitchy), 1 = long attack and release
    /// (floaty). 0.5 is the hand-tuned original.
    pub smoothing: f32,
    /// How far the audio deforms each scene: terrain height, tunnel radius,
    /// sphere/orb inflation, retro shape size.
    pub intensity: f32,
    /// Rotation, drift and scroll speed multiplier.
    pub motion: f32,
    /// Auto mode's eagerness to switch scenes: >1 lowers the onset threshold,
    /// <1 raises it so only the biggest drops count.
    pub switch_sensitivity: f32,
    /// Seconds Auto refuses to switch after a switch.
    pub switch_hold: f32,
}

impl Default for VisualizerSettings {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            smoothing: 0.5,
            intensity: 1.0,
            motion: 1.0,
            switch_sensitivity: 1.0,
            switch_hold: 9.0,
        }
    }
}

/// What happens when the play queue reaches its end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueEndBehavior {
    /// Stop but keep the queue and the last track in the player bar.
    #[default]
    Keep,
    /// Clear the queue and reset the player bar to empty.
    Clear,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    pub artist: bool,
    pub album: bool,
    pub year: bool,
    pub genre: bool,
    pub bitrate: bool,
    pub plays: bool,
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
            track_info: TrackInfo {
                artist: true,
                ..Default::default()
            },
            waveform_seekbar: false,
            stream_info_bar: false,
            detailed_volume: false,
            show_queue_button: true,
            replay_gain: ReplayGainMode::Off,
            output_device: None,
            queue_end: QueueEndBehavior::Keep,
            fullscreen_bg: FullscreenBackground::Gradient,
            local_music_dirs: Vec::new(),
            fullscreen_volume: false,
            visualizer: VisualizerMode::Off,
            visualizer_tuning: VisualizerSettings::default(),
            resume_playback: false,
            sidebar_libraries_collapsed: false,
            sidebar_playlists_collapsed: false,
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
    /// Dark base whose accent surfaces (buttons, sliders, progress bar) recolour
    /// from the current album cover. See `ui::apply_adaptive_accent`.
    #[serde(rename = "adaptive")]
    Adaptive,
    Custom,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportedThemeDefinition {
    pub name: String,
    #[serde(default)]
    pub mode: String,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub border: Option<String>,
    pub muted: Option<String>,
    pub muted_foreground: Option<String>,
    pub primary: Option<String>,
    pub primary_foreground: Option<String>,
    pub secondary: Option<String>,
    pub secondary_foreground: Option<String>,
    pub accent: Option<String>,
    pub accent_foreground: Option<String>,
    pub sidebar: Option<String>,
    pub sidebar_foreground: Option<String>,
    pub success: Option<String>,
    pub success_foreground: Option<String>,
    pub warning: Option<String>,
    pub warning_foreground: Option<String>,
    pub danger: Option<String>,
    pub danger_foreground: Option<String>,
    pub selection: Option<String>,
    pub scrollbar_thumb: Option<String>,
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
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| url.to_string());
    format!("{username}@{host}")
}

/// Sanitize a string for use in filenames: keep only alphanumeric, `-`, `_`.
pub fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
    use super::{ImportedThemesFile, Settings};

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

    #[test]
    fn settings_default_local_music_dirs_empty() {
        let s = Settings::default();
        assert!(s.local_music_dirs.is_empty());
    }

    #[test]
    fn settings_toml_round_trip_local_music_dirs() {
        let toml_input = r#"local_music_dirs = ["/music/flac", "/music/mp3"]
volume = 0.8
"#;
        let s: Settings = toml::from_str(toml_input).unwrap();
        assert_eq!(s.local_music_dirs.len(), 2);
        assert_eq!(s.local_music_dirs[0].to_string_lossy(), "/music/flac");
        assert_eq!(s.local_music_dirs[1].to_string_lossy(), "/music/mp3");
        assert!((s.volume - 0.8).abs() < f32::EPSILON);

        let output = toml::to_string_pretty(&s).unwrap();
        let restored: Settings = toml::from_str(&output).unwrap();
        assert_eq!(restored.local_music_dirs, s.local_music_dirs);
    }

    #[test]
    fn settings_backward_compat_no_local_music_dirs() {
        let toml_input = r#"volume = 0.5
"#;
        let s: Settings = toml::from_str(toml_input).unwrap();
        assert!(s.local_music_dirs.is_empty());
        assert!((s.volume - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn settings_to_skips_empty_local_music_dirs() {
        let s = Settings::default();
        let output = toml::to_string_pretty(&s).unwrap();
        assert!(!output.contains("local_music_dirs"));
    }
}
