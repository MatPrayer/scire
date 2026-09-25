//! Settings persistence (TOML) and credential storage (OS keyring).

use std::fs;
use std::path::{Path, PathBuf};

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

/// Write `bytes` to `path` through a temp file in the same directory and a
/// rename, so a crash or a SIGKILL partway leaves the previous file intact
/// rather than a truncated one. Every file this module and the player write is
/// state the app reloads at launch — a half-written `settings.toml` loses the
/// server, and a half-written `queue.json` loses the queue, both silently,
/// since a parse failure falls back to the defaults.
///
/// `secret` restricts the file to the owner on unix: the settings file carries
/// the plaintext password fallback and the ListenBrainz token, and the default
/// mode is world-readable.
pub fn write_atomic(path: &Path, bytes: &[u8], secret: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // The temp file has to sit beside the target: a rename across filesystems
    // fails, and the config dir and the system temp dir are routinely on
    // different ones.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt as _;
        // Before the rename, so the file is never world-readable at its real
        // name, not even for an instant.
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = secret;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
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

/// Where online lyrics lookups are parked, so the panel does not re-ask for
/// every song on every reopen.
pub fn lyrics_cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("lyrics"))
}

/// On-disk answers from `services::album_info`.
pub fn album_info_cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("album_info"))
}

/// On-disk answers from `services::artist_info`.
pub fn artist_info_cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().join("artist_info"))
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

/// `true`, for the `#[serde(default = …)]` of a field whose own default is on
/// while the struct's is not reachable (a field skipped by the container's
/// `#[serde(default)]` only when it is itself absent).
fn default_true() -> bool {
    true
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
    /// Base size for rem-based interface text, clamped to 9–32px.
    pub font_size: UiFontSize,
    /// Coarse multiplier on the interface's pixel chrome — gutters, card
    /// padding, row and bar heights. See [`UiScale`] for why this is separate
    /// from `font_size`.
    #[serde(default)]
    pub ui_scale: UiScale,
    /// Draw the in-app title bar (gpui-component `TitleBar`). When false, use native WM chrome.
    pub client_titlebar: bool,
    /// Strip the in-app title bar down to the window controls: no app name, no
    /// separator, the app background instead of the title-bar tint, and only
    /// tall enough to hold macOS's traffic lights. Ignored when
    /// `client_titlebar` is off, since the WM draws the bar then.
    #[serde(default)]
    pub minimal_titlebar: bool,
    /// Forward now-playing / scrobble submissions to the server.
    pub scrobble_enabled: bool,
    /// Submit local-file plays directly to ListenBrainz.
    pub listenbrainz_enabled: bool,
    /// Plaintext token fallback for systems without a usable keyring.
    /// Only written when keyring storage fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listenbrainz_token_plaintext: Option<String>,
    /// Default shuffle state for new sessions.
    pub default_shuffle: bool,
    /// Default repeat mode for new sessions.
    pub default_repeat: RepeatMode,
    /// On-disk artwork cache cap in megabytes.
    pub artwork_cache_mb: u32,
    /// Pull every album and artist cover into the artwork cache in the
    /// background, instead of downloading them as the grids scroll past.
    #[serde(default)]
    pub precache_art: bool,
    /// Legacy "look missing lyrics up on LRCLIB" switch, superseded by
    /// [`lyrics_provider`](Self::lyrics_provider) and migrated into it on load.
    ///
    /// An `Option` rather than a `bool` so the migration can tell a settings
    /// file that *said* something from one that never carried the key: the
    /// container's `#[serde(default)]` fills a missing field from
    /// `Settings::default()`, which for a plain `bool` is indistinguishable
    /// from the user having chosen that value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online_lyrics: Option<bool>,
    /// Which sources the lyrics panel reads, and in what order.
    #[serde(default)]
    pub lyrics_provider: LyricsProvider,
    /// Let a *timed* document from the lower-priority source beat an untimed
    /// one from the higher.
    ///
    /// Off, the first source with any words at all wins, which is what the
    /// panel did before the setting existed. On, a library copy scraped out of
    /// a tag with no timings is passed over for an LRCLIB document that has
    /// them — the words are usually the same and only one of the two can be
    /// followed line by line. It costs one extra lookup on exactly the tracks
    /// whose library copy is untimed, and nothing at all under
    /// `LibraryOnly`/`OnlineOnly`, which have no second source to consult.
    #[serde(default = "default_true")]
    pub prefer_synced_lyrics: bool,
    /// Page shown right after connecting.
    pub default_page: DefaultPage,
    /// Last selected album list filter, restored across sessions.
    pub album_sort: AlbumSort,
    /// Cover-art tile size in the album grid.
    pub cover_size: CoverSize,
    /// Put the padding back around a grid card's cover and round all four of
    /// its corners — the card the grids drew before the cover was let fill
    /// them.
    ///
    /// Off (the default) the cover takes the card's inset for itself, edge to
    /// edge inside the border, and squares its bottom two corners so the art
    /// runs flat into the title below; the top two stay rounded with the card.
    /// The card keeps the width it always had either way — the cover grows into
    /// the chrome rather than the card shrinking — so the column count and the
    /// grid's rhythm are identical, and toggling this cannot reflow the page.
    #[serde(default)]
    pub classic_album_cards: bool,
    /// Cover size of the album cards on an artist's page. `Match` follows
    /// `cover_size`; the rest pick a size for that page alone.
    #[serde(default)]
    pub artist_album_size: ArtistAlbumSize,
    /// Extra columns shown next to song titles in track lists.
    pub track_info: TrackInfo,
    /// Render the seek bar as the track's waveform (downloads remote tracks a
    /// second time or reads local tracks to decode them).
    pub waveform_seekbar: bool,
    /// Show format/bitrate/sample-rate of the current track in the player bar.
    pub stream_info_bar: bool,
    /// Show a precise percentage readout next to the volume slider.
    pub detailed_volume: bool,
    /// Show the queue-toggle button in the bottom player bar.
    pub show_queue_button: bool,
    /// Drop the bottom player bar entirely while there is nothing to play: an
    /// empty queue, no radio and no playback. A bar with no track in it is a
    /// strip of disabled buttons, and the content above it gets its height.
    pub hide_idle_player_bar: bool,
    /// ReplayGain loudness-normalization mode.
    pub replay_gain: ReplayGainMode,
    /// Pre-amplification added to every ReplayGain adjustment, in dB. Tags
    /// target 89 dB SPL (-18 LUFS for R128), which many listeners find quiet
    /// next to unnormalized audio; a few dB back evens that out.
    pub replay_gain_preamp: f32,
    /// Cap the ReplayGain factor at `1/peak` so a boost never clips. Off lets
    /// a quiet track with a hot peak be raised all the way, clipping included.
    pub replay_gain_prevent_clipping: bool,
    /// Chosen audio output device, named as `playback::output_devices` reports
    /// it (a PulseAudio/PipeWire sink description on Linux, a cpal one
    /// elsewhere); None = OS default. A name that no longer matches any device
    /// falls back to the default rather than failing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_device: Option<String>,
    /// A card opened directly, bypassing the sound server — an ALSA id as
    /// `playback::direct_devices` reports it. Overrides `output_device` while
    /// set: bit-perfect playback at each track's own rate, volume and
    /// ReplayGain bypassed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_direct: Option<String>,
    /// What to do when the play queue reaches its end.
    pub queue_end: QueueEndBehavior,
    /// Background style of the fullscreen now-playing overlay.
    pub fullscreen_bg: FullscreenBackground,
    /// Directories to scan for local music files. Empty = local music disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_music_dirs: Vec<PathBuf>,
    /// Show the vertical volume slider in the fullscreen now-playing overlay.
    pub fullscreen_volume: bool,
    /// How far the fullscreen overlay's cover art grows on a window with room
    /// to spare. `Fixed` keeps the size a window that only just holds the
    /// overlay draws.
    #[serde(default)]
    pub fullscreen_cover: FullscreenCoverSize,
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
    /// Sidebar folded down to an icon rail; restored across sessions. A
    /// portrait window collapses it on its own whatever this says, and
    /// restores this value when it turns landscape again.
    pub sidebar_collapsed: bool,
    /// Vi-style modal keyboard navigation.
    pub vi_mode: bool,
    /// Draw the back/forward history buttons above the content area. The
    /// mouse's navigation buttons and the `[`/`]` keys work either way.
    pub show_nav_buttons: bool,
    /// Under the Adaptive theme, let an album page tint itself from its own
    /// cover. Only that page: the app's chrome — sidebar, player bar, sliders,
    /// fullscreen — always follows the playing track, since a single global
    /// theme is what colours them. Ignored by the other themes.
    pub adaptive_from_page: bool,
    /// Wash the album page's header card with that album's colour. Off by
    /// default — the accent surfaces alone are the quiet version of
    /// `adaptive_from_page`, and the gradient is the loud one.
    pub adaptive_page_gradient: bool,
    /// How an album page arranges its cover and details against its track
    /// list. `SidePanel` only applies where the window has the room for it;
    /// see `ui::album_side_panel`.
    #[serde(default)]
    pub album_layout: AlbumPageLayout,
    /// Spell the dates on an album page out in full: the time of day beside
    /// the "Added" date, and the month and day of the release where the
    /// server publishes them (OpenSubsonic `originalReleaseDate`), instead of
    /// the bare year. Off by default — the short forms are what the summary
    /// line is sized for, and a release date is a year to most people.
    #[serde(default)]
    pub detailed_album_dates: bool,
    /// Look albums up on Wikipedia (through Wikidata) for the About card
    /// (`services::album_info`). On by default: the server's own notes are
    /// Last.fm's *summary*, cut off mid-sentence, and most libraries have none
    /// at all.
    pub album_info_wikipedia: bool,
    /// Look albums up on MusicBrainz: its annotation, its links (Discogs,
    /// AllMusic, Bandcamp) and the most reliable route to the right Wikipedia
    /// article. Off, Wikipedia is found by a title search instead.
    pub album_info_musicbrainz: bool,
    /// Show the album notes the server forwards (Last.fm, via
    /// `getAlbumInfo2`). Nothing is sent either way — the server fetches them
    /// — so this only decides whether the card offers them.
    pub server_album_notes: bool,
    /// Show the artist biography the server forwards (`getArtistInfo2`).
    pub server_artist_bios: bool,
    /// Look artists up on Wikipedia for the artist page's bio
    /// (`services::artist_info`). On by default, like the album one: the
    /// server's bio is Last.fm's summary, cut off mid-sentence.
    pub artist_info_wikipedia: bool,
    /// Look artists up on MusicBrainz: its annotation, its links (official
    /// site, Discogs, AllMusic, Bandcamp) and the route to the right article.
    pub artist_info_musicbrainz: bool,
    /// How the bottom player bar sits against the rest of the UI. `Docked`
    /// (default) reserves its own row, same as every other panel. `Floating`
    /// draws it as a translucent, rounded card hovering over the content
    /// instead — the same card treatment as the fullscreen overlay's panels.
    #[serde(default)]
    pub player_bar_style: PlayerBarStyle,
    /// Wash the bottom player bar with the playing track's colour under the
    /// Adaptive theme. Off leaves it the flat panel fill every other theme
    /// draws, while the accent itself stays on buttons, sliders and the seek
    /// bar — this is the backdrop alone. No other theme tints the bar, so the
    /// switch does nothing under them.
    pub player_bar_tint: bool,
    /// Let the page show through the `Floating` player bar's card. Off
    /// (default) draws the same card fully opaque, which is the readable one
    /// over a grid of cover art; on restores the show-through that makes it
    /// read as glass over the page. The `Docked` bar is opaque either way, so
    /// this does nothing under it.
    pub player_bar_translucent: bool,
    /// Put the cover-and-details panel on the *right* and the track list on
    /// the left; the layout's default is the other way round. Only the
    /// `SidePanel` layout has two columns to swap, so this is ignored — and the
    /// switch disabled — under `Stacked`.
    #[serde(default)]
    pub album_panel_right: bool,
    /// Drop the star buttons from the album page — the one beside the title and
    /// the one on each track row. Starring stays available from a track's
    /// context menu, so this hides the buttons rather than the feature.
    #[serde(default)]
    pub hide_album_stars: bool,
    /// Disable non-essential UI animations (tab transitions, hover effects, panel slides).
    /// Useful on lower-end GPUs or for users sensitive to motion.
    #[serde(default)]
    pub reduced_motion: bool,
    /// In vi mode, the focused card's highlight also gets a muted fill, an
    /// outer glow, and a growing entry animation. Off, the cursor is just a
    /// primary border around the card.
    #[serde(default)]
    pub selection_glow_vi: bool,
    /// Same glow (fill + border, no entry animation — the mouse doesn't jump
    /// the way j/k does) on whichever card or row the pointer is over.
    #[serde(default)]
    pub selection_glow_hover: bool,
    /// Colour the glow (vi cursor and/or hover, whichever is on) from the
    /// hovered/focused album's own cover instead of the theme's primary
    /// colour. Falls back to primary wherever a card has no single album to
    /// draw a colour from.
    #[serde(default)]
    pub selection_glow_album_color: bool,
    /// Where the window was and how big it was when it was last moved or
    /// resized. Absent until the first launch that writes one, which is what
    /// keeps a fresh install on the centred default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
}

/// The window's last position, size and maximized state, in logical pixels.
///
/// Stored as bare `f32`s rather than a gpui `Bounds`: this module is the one
/// part of the app crate with no gpui in it, and the restore rule below is the
/// sort of thing that has to be unit-testable without opening a window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Maximized windows are restored maximized; the rest of the rect is the
    /// size the window goes back to when it is un-maximized, which is what
    /// gpui's `WindowBounds::Maximized` carries too.
    #[serde(default)]
    pub maximized: bool,
}

/// A window narrower or shorter than this is not restored: it is the smallest
/// thing the layout is meant for, and a rect saved from a broken frame (or
/// hand-edited) should not open a sliver.
const MIN_WINDOW_SIDE: f32 = 320.;

/// How much of the window has to land on a display for the rect to be usable.
/// The failure this exists for is a monitor that is no longer attached: the
/// saved rect then names coordinates that are on no screen at all, and the
/// window opens invisible, which is strictly worse than ignoring the setting.
/// The numbers are "enough of the title bar to grab", not a share of the area —
/// a large window hanging mostly off the edge is still usable if its top-left
/// corner is reachable, and a small one fully on screen must not fail a
/// percentage test.
const MIN_VISIBLE_W: f32 = 180.;
const MIN_VISIBLE_H: f32 = 80.;

impl WindowGeometry {
    /// Whether this rect may be reopened against the displays currently
    /// attached, each given as `(x, y, width, height)` in the same logical
    /// pixel space.
    ///
    /// No display list at all (a platform that reports none) counts as "cannot
    /// tell", and the rect is kept: refusing to restore is the fallback for
    /// knowing the rect is off-screen, not for not knowing.
    pub fn usable_on(&self, displays: &[(f32, f32, f32, f32)]) -> bool {
        if !self.width.is_finite()
            || !self.height.is_finite()
            || !self.x.is_finite()
            || !self.y.is_finite()
            || self.width < MIN_WINDOW_SIDE
            || self.height < MIN_WINDOW_SIDE
        {
            return false;
        }
        if displays.is_empty() {
            return true;
        }
        displays.iter().any(|&(dx, dy, dw, dh)| {
            let w = (self.x + self.width).min(dx + dw) - self.x.max(dx);
            let h = (self.y + self.height).min(dy + dh) - self.y.max(dy);
            w >= MIN_VISIBLE_W.min(self.width) && h >= MIN_VISIBLE_H.min(self.height)
        })
    }
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

/// Cover-art tile size for the album grid — a *range*, not one width.
///
/// A fixed tile size leaves the grid's leftover width as gutters: the columns
/// are whole cards, so a 2560px-wide window keeps up to a card's worth of empty
/// space split either side, which is why a 16:9 monitor looked padded where a
/// laptop's 16:10 pane happened to divide evenly. The grid instead takes as
/// many columns as fit at the range's *minimum* and grows the tile up to the
/// maximum to spend what's left over, so the setting picks how big covers are
/// roughly and the window decides exactly.
///
/// The maximum doubles as the pixel resolution requested/decoded for grid
/// thumbnails, so smaller tiles still fetch smaller textures — and the fetch
/// resolution depends only on the setting, so a resize never refetches art.
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
    /// Smallest and largest rendered tile edge, in logical pixels.
    ///
    /// The spread is wide enough to absorb one column's worth of leftover at
    /// the column counts these sizes actually produce (a grid `n` columns wide
    /// needs `max >= min * (n + 1) / n` to swallow the gutters), and the ranges
    /// don't overlap so the four settings stay visibly different.
    pub fn range(self) -> (f32, f32) {
        match self {
            Self::Small => (110., 148.),
            Self::Medium => (150., 198.),
            Self::Large => (200., 262.),
            Self::ExtraLarge => (264., 350.),
        }
    }

    /// Largest tile the grid will grow to before it leaves gutters again.
    pub fn max_px(self) -> f32 {
        self.range().1
    }

    /// Resolution to request/decode for grid thumbnails. Bumped ~1.5× over the
    /// *largest* tile the setting can draw so HiDPI screens stay crisp without
    /// decoding full art — and so a resize, which moves the tile inside the
    /// range, never invalidates already-cached covers.
    pub fn art_px(self) -> u32 {
        (self.max_px() * 1.5) as u32
    }

    /// Tile edge for a *wrapped* grid of cards rather than the album grid's
    /// column-fitting one — the artist page's discography rows, which wrap at
    /// whatever width they have and so have no leftover to spend.
    ///
    /// A single number per size, not a range: with nothing to absorb there is
    /// nothing to grow into. `Medium` is the 160px the artist page drew before
    /// the size was settable, so the default look is unchanged.
    pub fn wrap_tile(self) -> f32 {
        match self {
            Self::Small => 112.,
            Self::Medium => 160.,
            Self::Large => 212.,
            Self::ExtraLarge => 280.,
        }
    }

    /// Resolution to request for [`wrap_tile`](Self::wrap_tile) cards. 2× the
    /// tile, which keeps `Medium` on the 512 rung the artist page already used
    /// and so costs the default no refetch.
    pub fn wrap_art_px(self) -> u32 {
        (self.wrap_tile() * 2.) as u32
    }
}

/// Cover size for the album cards on an artist's page.
///
/// Separate from [`CoverSize`] because the two grids are not the same shape —
/// the album grid fills its width by growing the tile, the artist page's
/// discography wraps at a fixed one — and a user who wants a dense browsing
/// grid may still want a readable discography. `Match` is the default and is
/// what the page did before the setting existed: whatever the album grid is set
/// to, so one knob still moves both for anyone who only wants one knob.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtistAlbumSize {
    /// Follow `Settings::cover_size`.
    #[default]
    Match,
    Small,
    Medium,
    Large,
    ExtraLarge,
}

impl ArtistAlbumSize {
    /// The size actually drawn, `grid` being the album grid's own setting.
    pub fn resolve(self, grid: CoverSize) -> CoverSize {
        match self {
            Self::Match => grid,
            Self::Small => CoverSize::Small,
            Self::Medium => CoverSize::Medium,
            Self::Large => CoverSize::Large,
            Self::ExtraLarge => CoverSize::ExtraLarge,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Match => "Match album grid",
            Self::Small => "Small",
            Self::Medium => "Medium",
            Self::Large => "Large",
            Self::ExtraLarge => "Extra large",
        }
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

/// How big the fullscreen overlay's cover art is allowed to get on a window
/// with room to spare.
///
/// A window that only just holds the overlay draws the same cover whatever this
/// says — the setting only governs the room *above* that, which is why `Fixed`
/// is a size rather than an on/off flag: it pins the cover to what a small
/// window draws instead of letting it grow into a big one.
///
/// The pair per size is a share of the room and a ceiling, and both matter:
/// the ceiling alone leaves every size looking identical on a 1080p screen,
/// where the share is what the cover actually hits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullscreenCoverSize {
    /// Never grows: the cover a window that only just fits the overlay draws.
    Fixed,
    Medium,
    #[default]
    Large,
    Huge,
}

impl FullscreenCoverSize {
    /// Share of the window's height the cover may take beside the info card,
    /// and the most it is drawn at. `Fixed` returns a share of zero, which the
    /// layout's own clamp lifts back to the no-room-to-spare cover.
    pub fn beside_card(self) -> (f32, f32) {
        match self {
            Self::Fixed => (0., 0.),
            Self::Medium => (0.52, 620.),
            Self::Large => (0.62, 780.),
            Self::Huge => (0.72, 980.),
        }
    }

    /// The same, stacked: there the cover has the whole width to itself, so the
    /// share is of the content width and both numbers are larger.
    pub fn stacked(self) -> (f32, f32) {
        match self {
            Self::Fixed => (0., 0.),
            Self::Medium => (0.70, 720.),
            Self::Large => (0.82, 880.),
            Self::Huge => (0.92, 1100.),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fixed => "Fixed",
            Self::Medium => "Medium",
            Self::Large => "Large",
            Self::Huge => "Huge",
        }
    }
}

/// How an album page arranges the cover and its details against the track
/// list.
///
/// `SidePanel` is a preference, not a promise: the page only takes that shape
/// on a window wide enough to hold a full-width track list *and* a panel worth
/// drawing a cover in (`ui::album_side_panel` decides), and falls back to
/// `Stacked` everywhere else — a narrow or portrait window would otherwise get
/// a squeezed track list beside a thumbnail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlbumPageLayout {
    /// Cover and details across the top, track list underneath.
    #[default]
    Stacked,
    /// Track list down the left, cover and details in a tall panel on the
    /// right.
    SidePanel,
}

impl AlbumPageLayout {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stacked => "Stacked",
            Self::SidePanel => "Side panel",
        }
    }

    /// Whether the page should *try* for the side panel; the window still has
    /// the final say.
    pub fn wants_side_panel(self) -> bool {
        self == Self::SidePanel
    }
}

/// Coarse multiplier on the interface's own pixel chrome.
///
/// Deliberately *not* the same knob as `font_size`. gpui-component sets the
/// window's rem size from `Theme::font_size` (`root.rs`), so the font setting
/// already scales every `rems()`-based length — all the `text_*` sizes and the
/// widget metrics derived from them. What it cannot touch is the pixel chrome
/// this app lays out itself: grid gutters, card padding, player-bar and row
/// heights, the sidebar's width. Those are what this scales, and it is why the
/// two settings are both worth having — type can be made bigger without the
/// layout loosening, and the layout can be loosened without the type growing.
///
/// Coarse on purpose: these are structural numbers that other numbers are
/// derived from (column counts, panel minimums, what fits in a window), and a
/// continuous slider over them invites widths that only *nearly* work. Four
/// rungs are enough to matter and few enough to reason about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiScale {
    /// 90% — tighter gutters and shorter rows, more content per screen.
    Snug,
    #[default]
    Normal,
    /// 110%.
    Roomy,
    /// 125%.
    Large,
}

impl UiScale {
    pub fn factor(self) -> f32 {
        match self {
            Self::Snug => 0.9,
            Self::Normal => 1.0,
            Self::Roomy => 1.1,
            Self::Large => 1.25,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Snug => "90%",
            Self::Normal => "100%",
            Self::Roomy => "110%",
            Self::Large => "125%",
        }
    }

    pub const ALL: [Self; 4] = [Self::Snug, Self::Normal, Self::Roomy, Self::Large];
}

/// Which sources the lyrics panel reads, and in what order.
///
/// "Library" is the file's own words — the server's copy for a streamed track,
/// the sidecar `.lrc` or `LYRICS` tag read off disk for a local one. "Online"
/// is a lookup on [LRCLIB](https://lrclib.net), which sends the track's artist,
/// title, album and length to lrclib.net and so is the one with a privacy cost;
/// it is also the one that routinely comes back *timed* where a tag-scraped
/// copy is not.
///
/// The order matters beyond which answer wins: a source that is never reached
/// is never asked, so `LibraryOnly` makes no network request at all and
/// `OnlineOnly` skips reading the file. `Settings::prefer_synced_lyrics` is
/// what lets the second source overrule the first, and only on timings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsProvider {
    /// Only the file's own words. Never contacts LRCLIB.
    LibraryOnly,
    /// Only LRCLIB. Ignores whatever the file or the server carries.
    OnlineOnly,
    /// The file's own words, falling back to LRCLIB when there are none.
    #[default]
    LibraryFirst,
    /// LRCLIB, falling back to the file's own words when it has none.
    OnlineFirst,
}

impl LyricsProvider {
    /// The provider for a set of per-source switches: which sources are on,
    /// and — only meaningful with both — whether LRCLIB is asked first.
    /// `None` with both off, since the panel has no "no lyrics" provider.
    pub fn from_parts(library: bool, online: bool, online_first: bool) -> Option<Self> {
        match (library, online) {
            (true, true) if online_first => Some(Self::OnlineFirst),
            (true, true) => Some(Self::LibraryFirst),
            (true, false) => Some(Self::LibraryOnly),
            (false, true) => Some(Self::OnlineOnly),
            (false, false) => None,
        }
    }

    /// This provider with the library switched on or off.
    pub fn with_library(self, on: bool) -> Option<Self> {
        Self::from_parts(on, self.uses_online(), self == Self::OnlineFirst)
    }

    /// This provider with LRCLIB switched on or off.
    pub fn with_online(self, on: bool) -> Option<Self> {
        Self::from_parts(self.uses_library(), on, self == Self::OnlineFirst)
    }

    /// This provider asking LRCLIB first or not; a single-source provider has
    /// no order to change and is returned as it is.
    pub fn with_online_first(self, first: bool) -> Self {
        Self::from_parts(self.uses_library(), self.uses_online(), first).unwrap_or(self)
    }

    /// Whether the file's / server's own lyrics are consulted at all.
    pub fn uses_library(self) -> bool {
        !matches!(self, Self::OnlineOnly)
    }

    /// Whether LRCLIB is consulted at all. This is the switch that decides
    /// whether the app ever talks to lrclib.net.
    pub fn uses_online(self) -> bool {
        !matches!(self, Self::LibraryOnly)
    }

    /// Whether the library is asked first. Meaningless for the two single-
    /// source providers, which never reach a second one.
    pub fn library_first(self) -> bool {
        matches!(self, Self::LibraryOnly | Self::LibraryFirst)
    }

    /// Whether both sources are on offer, i.e. whether "prefer synced" has a
    /// second source to promote.
    pub fn has_fallback(self) -> bool {
        self.uses_library() && self.uses_online()
    }
}

/// How the bottom player bar sits against the rest of the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerBarStyle {
    /// Reserves its own row at the bottom, same as every other panel.
    #[default]
    Docked,
    /// A translucent, rounded card floating over the content with a margin on
    /// every side, matching the fullscreen overlay's panel look.
    Floating,
}

impl PlayerBarStyle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Docked => "Docked",
            Self::Floating => "Floating",
        }
    }
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
            font_size: UiFontSize::default(),
            ui_scale: UiScale::default(),
            client_titlebar: true,
            minimal_titlebar: false,
            scrobble_enabled: true,
            listenbrainz_enabled: false,
            listenbrainz_token_plaintext: None,
            default_shuffle: false,
            default_repeat: RepeatMode::Off,
            artwork_cache_mb: 256,
            precache_art: false,
            online_lyrics: None,
            lyrics_provider: LyricsProvider::default(),
            prefer_synced_lyrics: true,
            default_page: DefaultPage::default(),
            album_sort: AlbumSort::default(),
            cover_size: CoverSize::default(),
            classic_album_cards: false,
            artist_album_size: ArtistAlbumSize::default(),
            track_info: TrackInfo {
                artist: true,
                ..Default::default()
            },
            waveform_seekbar: false,
            stream_info_bar: false,
            detailed_volume: false,
            show_queue_button: true,
            hide_idle_player_bar: true,
            replay_gain: ReplayGainMode::Off,
            replay_gain_preamp: 0.0,
            replay_gain_prevent_clipping: true,
            output_device: None,
            output_direct: None,
            queue_end: QueueEndBehavior::Keep,
            fullscreen_bg: FullscreenBackground::Gradient,
            local_music_dirs: Vec::new(),
            fullscreen_volume: false,
            fullscreen_cover: FullscreenCoverSize::default(),
            visualizer: VisualizerMode::Off,
            visualizer_tuning: VisualizerSettings::default(),
            resume_playback: false,
            sidebar_libraries_collapsed: false,
            sidebar_playlists_collapsed: false,
            sidebar_collapsed: false,
            vi_mode: false,
            show_nav_buttons: true,
            adaptive_from_page: false,
            adaptive_page_gradient: false,
            album_layout: AlbumPageLayout::default(),
            detailed_album_dates: false,
            album_info_wikipedia: true,
            album_info_musicbrainz: true,
            server_album_notes: true,
            server_artist_bios: true,
            artist_info_wikipedia: true,
            artist_info_musicbrainz: true,
            player_bar_style: PlayerBarStyle::default(),
            player_bar_tint: true,
            player_bar_translucent: false,
            album_panel_right: false,
            hide_album_stars: false,
            reduced_motion: false,
            selection_glow_vi: false,
            selection_glow_hover: false,
            selection_glow_album_color: false,
            window: None,
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

pub const UI_FONT_SIZE_MIN: u8 = 9;
pub const UI_FONT_SIZE_MAX: u8 = 32;
pub const UI_FONT_SIZE_DEFAULT: u8 = 16;

/// Base size for rem-based interface text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UiFontSize(u8);

impl UiFontSize {
    pub fn new(px: u8) -> Self {
        Self(px.clamp(UI_FONT_SIZE_MIN, UI_FONT_SIZE_MAX))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    pub fn px(self) -> f32 {
        f32::from(self.0)
    }
}

impl Default for UiFontSize {
    fn default() -> Self {
        Self(UI_FONT_SIZE_DEFAULT)
    }
}

impl<'de> Deserialize<'de> for UiFontSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Pixels(u64),
            Legacy(String),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Pixels(px) => Ok(Self::new(px.min(u64::from(u8::MAX)) as u8)),
            Repr::Legacy(value) => match value.as_str() {
                "small" => Ok(Self::new(14)),
                "default" => Ok(Self::default()),
                "large" => Ok(Self::new(18)),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown font size {value:?}"
                ))),
            },
        }
    }
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
                // Migrate the pre-provider "fetch missing lyrics online"
                // switch. Only `false` carries information: it is the one
                // choice the four providers cannot all express, and it maps
                // onto exactly one of them. `true` was the default and means
                // the user never touched it, so it is left to whatever
                // `lyrics_provider` says — which for a file written before the
                // key existed is `LibraryFirst`, the same behaviour.
                if settings.online_lyrics == Some(false) {
                    settings.lyrics_provider = LyricsProvider::LibraryOnly;
                }
                settings.online_lyrics = None;
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
        write_atomic(&path, toml::to_string_pretty(self)?.as_bytes(), true)
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

pub fn store_lb_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, "listenbrainz")?;
    entry.set_password(token)?;
    Ok(())
}

pub fn load_lb_token(settings: &Settings) -> Result<String> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, "listenbrainz")
        && let Ok(token) = entry.get_password()
    {
        return Ok(token);
    }
    settings
        .listenbrainz_token_plaintext
        .clone()
        .context("ListenBrainz token not found")
}

pub fn delete_lb_token() {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, "listenbrainz") {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtistAlbumSize, CoverSize, ImportedThemesFile, LyricsProvider, Settings,
        UI_FONT_SIZE_DEFAULT, UI_FONT_SIZE_MAX, UI_FONT_SIZE_MIN, UiFontSize, WindowGeometry,
        write_atomic,
    };

    #[test]
    fn lyrics_switches_map_onto_providers() {
        use LyricsProvider::*;
        assert_eq!(LibraryFirst.with_online(false), Some(LibraryOnly));
        assert_eq!(LibraryOnly.with_online(true), Some(LibraryFirst));
        assert_eq!(LibraryFirst.with_library(false), Some(OnlineOnly));
        assert_eq!(OnlineOnly.with_library(true), Some(LibraryFirst));
        // The last source on cannot be switched off.
        assert_eq!(OnlineFirst.with_online(false), Some(LibraryOnly));
        assert_eq!(LibraryOnly.with_library(false), None);
        assert_eq!(OnlineOnly.with_online(false), None);
        assert_eq!(LibraryFirst.with_online_first(true), OnlineFirst);
        assert_eq!(OnlineFirst.with_online_first(false), LibraryFirst);
        assert_eq!(LibraryOnly.with_online_first(true), LibraryOnly);
    }

    fn geometry(x: f32, y: f32, width: f32, height: f32) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            width,
            height,
            maximized: false,
        }
    }

    /// 1440p on the left, 1080p to its right — the layout in this project's own
    /// CLAUDE.md, and the one that makes "the second monitor is gone" concrete.
    const DISPLAYS: [(f32, f32, f32, f32); 2] = [(0., 0., 2560., 1440.), (2560., 0., 1920., 1080.)];

    #[test]
    fn a_window_on_an_attached_display_is_restored() {
        assert!(geometry(100., 80., 1100., 720.).usable_on(&DISPLAYS));
        // Entirely on the second monitor.
        assert!(geometry(3000., 100., 1100., 720.).usable_on(&DISPLAYS));
        // Straddling the two, which is a perfectly ordinary place to leave it.
        assert!(geometry(2200., 100., 1100., 720.).usable_on(&DISPLAYS));
    }

    #[test]
    fn a_window_on_a_display_that_is_gone_is_not_restored() {
        // Saved on the 1080p monitor, reopened with only the 1440p one
        // attached: the rect names coordinates on no screen, so restoring it
        // would open the window where it cannot be seen or grabbed.
        let saved = geometry(3000., 100., 1100., 720.);
        assert!(!saved.usable_on(&DISPLAYS[..1]));
    }

    #[test]
    fn a_window_hanging_off_an_edge_keeps_enough_to_grab() {
        // Mostly off the right edge, but the corner is still reachable.
        assert!(geometry(2300., 100., 1100., 720.).usable_on(&DISPLAYS[..1]));
        // All but a sliver past it.
        assert!(!geometry(2540., 100., 1100., 720.).usable_on(&DISPLAYS[..1]));
        // Dragged up under a top bar until only a strip is left.
        assert!(!geometry(100., -700., 1100., 720.).usable_on(&DISPLAYS[..1]));
    }

    #[test]
    fn a_nonsense_rect_is_ignored() {
        assert!(!geometry(0., 0., 40., 30.).usable_on(&DISPLAYS));
        assert!(!geometry(f32::NAN, 0., 1100., 720.).usable_on(&DISPLAYS));
        assert!(!geometry(0., 0., f32::INFINITY, 720.).usable_on(&DISPLAYS));
    }

    #[test]
    fn with_no_displays_reported_the_saved_rect_is_kept() {
        // "Cannot tell" is not "off-screen": the check exists to catch a
        // monitor that is gone, and a platform that reports no displays at all
        // has told us nothing about where the window would land.
        assert!(geometry(3000., 100., 1100., 720.).usable_on(&[]));
    }

    /// The settings file carries the plaintext password fallback and the
    /// ListenBrainz token, and the default mode is world-readable.
    #[cfg(unix)]
    #[test]
    fn a_secret_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("scire-perm-{}", std::process::id()));
        let path = dir.join("settings.toml");
        write_atomic(&path, b"server = {}", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "settings must not be world-readable");

        // And an ordinary file is left at whatever the umask says.
        let plain = dir.join("queue.json");
        write_atomic(&plain, b"{}", false).unwrap();
        assert_ne!(
            std::fs::metadata(&plain).unwrap().permissions().mode() & 0o077,
            0o000,
            "only the secret files are tightened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rename over the old file, so an interrupted write cannot leave a
    /// truncated one behind — and nothing is left in the directory either way.
    #[test]
    fn an_atomic_write_replaces_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("scire-atomic-{}", std::process::id()));
        let path = dir.join("settings.toml");
        write_atomic(&path, b"first", false).unwrap();
        write_atomic(&path, b"second", false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "temp file not cleaned up: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct FontSizeFixture {
        value: UiFontSize,
    }

    #[test]
    fn the_artist_page_cover_size_follows_the_grid_only_when_it_is_match() {
        for grid in [
            CoverSize::Small,
            CoverSize::Medium,
            CoverSize::Large,
            CoverSize::ExtraLarge,
        ] {
            assert_eq!(ArtistAlbumSize::Match.resolve(grid), grid);
            assert_eq!(ArtistAlbumSize::Small.resolve(grid), CoverSize::Small);
            assert_eq!(
                ArtistAlbumSize::ExtraLarge.resolve(grid),
                CoverSize::ExtraLarge
            );
        }
        // The default is the old behaviour: one knob still moves both.
        assert_eq!(ArtistAlbumSize::default(), ArtistAlbumSize::Match);
    }

    #[test]
    fn wrapped_tiles_are_ordered_and_medium_is_the_size_the_page_always_drew() {
        let sizes = [
            CoverSize::Small,
            CoverSize::Medium,
            CoverSize::Large,
            CoverSize::ExtraLarge,
        ];
        for pair in sizes.windows(2) {
            assert!(pair[0].wrap_tile() < pair[1].wrap_tile());
            assert!(pair[0].wrap_art_px() < pair[1].wrap_art_px());
        }
        assert_eq!(CoverSize::Medium.wrap_tile(), 160.);
        assert_eq!(CoverSize::Medium.wrap_art_px(), 320);
    }

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
        assert_eq!(s.font_size, UiFontSize::default());
        assert!((s.volume - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn ui_font_size_clamps_to_supported_range() {
        assert_eq!(UiFontSize::new(0).value(), UI_FONT_SIZE_MIN);
        assert_eq!(UiFontSize::default().value(), UI_FONT_SIZE_DEFAULT);
        assert_eq!(UiFontSize::new(u8::MAX).value(), UI_FONT_SIZE_MAX);
    }

    #[test]
    fn ui_font_size_reads_legacy_presets_and_writes_pixels() {
        let small: UiFontSize = toml::from_str("value = \"small\"")
            .map(|value: FontSizeFixture| value.value)
            .unwrap();
        assert_eq!(small.value(), 14);

        let output = toml::to_string(&FontSizeFixture {
            value: UiFontSize::new(23),
        })
        .unwrap();
        assert_eq!(output, "value = 23\n");
    }

    #[test]
    fn settings_to_skips_empty_local_music_dirs() {
        let s = Settings::default();
        let output = toml::to_string_pretty(&s).unwrap();
        assert!(!output.contains("local_music_dirs"));
    }
}
