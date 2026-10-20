//! App-local SVG assets plus delegation to gpui-component's bundled icons.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/assets"]
#[include = "icons/**/*.svg"]
struct AppAssets;

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(file) = AppAssets::get(path) {
            return Ok(Some(file.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = AppAssets::iter()
            .filter_map(|p| p.starts_with(path).then(|| p.into()))
            .collect();
        names.extend(gpui_component_assets::Assets.list(path)?);
        names.sort();
        names.dedup();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_assets_include_icons() {
        let listed: Vec<_> = AppAssets::iter().collect();
        assert!(
            AppAssets::get("icons/play.svg").is_some(),
            "icons/play.svg not embedded; embedded files: {listed:?}"
        );
    }
}

/// Bundled Noto Sans fonts for consistent rendering across platforms.
pub const NOTO_SANS: &[u8] = include_bytes!("../fonts/NotoSans-Regular.ttf");
pub const NOTO_SANS_JP: &[u8] = include_bytes!("../fonts/NotoSansJP-Regular.ttf");

/// Build an [`gpui_component::Icon`] for an app-bundled icon path.
pub fn app_icon(path: &'static str) -> gpui_component::Icon {
    gpui_component::Icon::default().path(path)
}

/// Icon paths for use with [`app_icon`]. Kept together so views don't
/// hardcode asset strings.
pub mod icons {
    pub const PLAY: &str = "icons/play.svg";
    pub const PAUSE: &str = "icons/pause.svg";
    pub const SKIP_BACK: &str = "icons/skip-back.svg";
    pub const SKIP_FORWARD: &str = "icons/skip-forward.svg";
    pub const SHUFFLE: &str = "icons/shuffle.svg";
    pub const REPEAT: &str = "icons/repeat.svg";
    pub const REPEAT_1: &str = "icons/repeat-one.svg";
    pub const LIST_PLUS: &str = "icons/list-plus.svg";
    /// Playlist list, for the collapsed sidebar rail's playlists menu.
    pub const LIST_MUSIC: &str = "icons/list-music.svg";
    /// Shelved books, for the collapsed sidebar rail's libraries menu.
    pub const LIBRARY: &str = "icons/library.svg";
    pub const VOLUME_LOW: &str = "icons/volume-1.svg";
    pub const VOLUME_HIGH: &str = "icons/volume-2.svg";
    pub const MUSIC: &str = "icons/music.svg";
    pub const RADIO: &str = "icons/radio.svg";
    pub const STAR_FILLED: &str = "icons/star-filled.svg";
    pub const REFRESH: &str = "icons/refresh-cw.svg";
    /// Outline star from the gpui-component bundle (same artwork, no fill).
    pub const STAR_OUTLINE: &str = "icons/star.svg";
    pub const SERVER: &str = "icons/server.svg";
    /// Stands in for ListenBrainz, which Simple Icons has no mark for.
    pub const HEADPHONES: &str = "icons/headphones.svg";
    pub const AUDIO_LINES: &str = "icons/audio-lines.svg";
    /// Lidarr: the settings row, the sidebar entry, download buttons.
    pub const DOWNLOAD: &str = "icons/download.svg";
    // Song details dialog (lucide): file-tag source badge, trending, language,
    // artist-verified About, recording place.
    pub const TAG: &str = "icons/tag.svg";
    pub const FLAME: &str = "icons/flame.svg";
    pub const LANGUAGES: &str = "icons/languages.svg";
    pub const BADGE_CHECK: &str = "icons/badge-check.svg";
    pub const MIC_VOCAL: &str = "icons/mic-vocal.svg";
    // Site marks for the album page's link row, from Simple Icons (CC0).
    pub const BRAND_WIKIPEDIA: &str = "icons/brand-wikipedia.svg";
    pub const BRAND_MUSICBRAINZ: &str = "icons/brand-musicbrainz.svg";
    pub const BRAND_LASTFM: &str = "icons/brand-lastdotfm.svg";
    pub const BRAND_DISCOGS: &str = "icons/brand-discogs.svg";
    pub const BRAND_BANDCAMP: &str = "icons/brand-bandcamp.svg";
    pub const BRAND_GENIUS: &str = "icons/brand-genius.svg";
    // Streaming marks for the song details dialog's listen links.
    pub const BRAND_SPOTIFY: &str = "icons/brand-spotify.svg";
    pub const BRAND_YOUTUBE: &str = "icons/brand-youtube.svg";
    pub const BRAND_SOUNDCLOUD: &str = "icons/brand-soundcloud.svg";
    pub const BRAND_APPLE_MUSIC: &str = "icons/brand-applemusic.svg";
}
