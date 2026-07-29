# Scirè

A cross-platform desktop music client for [Navidrome](https://www.navidrome.org/) servers **and local music files**.

Built with [GPUI](https://www.gpui.rs/) (Zed's UI framework) + [gpui-component](https://github.com/longbridge/gpui-component).  
Speaks Subsonic API v1.16.1 + OpenSubsonic. Runs on macOS and Linux.

## Table of Contents

- [Features](#features)
- [Keyboard Shortcuts](#keyboard-shortcuts)
- [Repository Structure](#repository-structure)
- [Architecture](#architecture)
- [Theming](#theming)
  - [Custom theme JSON](#custom-theme-json)
  - [pywal16 integration](#pywal16-integration)
  - [CJK font support](#cjk-font-support)
  - [Fullscreen background](#fullscreen-background)
- [Build & Run](#build--run)
- [Configuration](#configuration)
- [Testing](#testing)
- [License](#license)

## Features

| Area | Capabilities |
|------|-------------|
| **Playback** | Stream FLAC/MP3/OGG via HTTP ; gapless track transitions ; seek, pause, resume ; ReplayGain (track/album/auto) ; output device selection (macOS + PulseAudio/PipeWire) |
| **Browse** | Album grid with infinite scroll and sort (name/new/recent/frequent/random/starred) ; artist index with bios and images |
| **Search** | Global search bar (`/` shortcut) — songs, albums, artists via `search3` |
| **Queue** | Shuffle, repeat (off / all / one), drag-free reorder, play-next, clear ; persisted across restarts |
| **Playlists** | Create, rename, delete, add/remove tracks |
| **Favorites** | Star and 1–5 star ratings ; dedicated starred view |
| **Multi-library** | Sidebar checkbox selector ; all libraries merged into one sorted view |
| **Scrobbling** | Calls `/rest/scrobble` at ≥50%/4min ; Navidrome forwards to ListenBrainz / Last.fm |
| **Internet radio** | List, play, add, delete stations |
| **Transcoding** | Per-session format (mp3/ogg/raw) and max bitrate |
| **Theming** | Light / Dark / Follow-system ; custom theme JSON ; CJK font support |
| **Fullscreen player** | Album art, track info, waveform seek bar, lyrics panel, queue panel, background gradient from album colours |
| **Waveform seek bar** | Per-track amplitude envelope (480 buckets, cached to disk) — click to seek ; next track's peaks computed while the current one plays |
| **OS media keys** | Media keys + Now Playing via `souvlaki` (macOS media center, Linux MPRIS) |
| **Artwork cache** | LRU-evicted disk cache (configurable capacity) ; HiDPI-aware resolution bump |
| **Navigation** | Mouse back/forward buttons ; bracket keys ; configurable default page |
| **Local music (early WIP)** | Directory scanner (`lofty` + `folder.jpg`) populates `LibraryDb` with tracks, albums, artists ; album grid UI with cover art ; incremental mtime-based scanner ; periodic background scan ; engine reads local files via `SourceReader` |

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Space` | Play / pause |
| `←` | Previous track (restart if >3s in) |
| `→` | Next track |
| `↑` | Volume +5% |
| `↓` | Volume −5% |
| `[` / mouse back | Navigate back |
| `]` / mouse forward | Navigate forward |
| `Esc` | Close fullscreen player / dismiss search / cancel |
| `/` | Focus search bar |

## Repository Structure

```
scire/
├── crates/
│   ├── subsonic/          # Subsonic/OpenSubsonic API client
│   │   ├── src/
│   │   │   ├── client.rs          # HTTP core, auth, error unwrapping
│   │   │   ├── endpoints/         # One file per endpoint group
│   │   │   │   ├── system.rs      # ping, getMusicFolders
│   │   │   │   ├── browsing.rs    # getArtists, getArtist, getAlbum, getArtistInfo2
│   │   │   │   ├── lists.rs       # getAlbumList2, getStarred2
│   │   │   │   ├── media.rs       # search3, getLyrics, scrobble, setRating
│   │   │   │   ├── playlists.rs   # CRUD
│   │   │   │   ├── annotation.rs  # star, unstar
│   │   │   │   └── radio.rs       # internet radio CRUD
│   │   │   ├── auth.rs            # md5 token auth
│   │   │   ├── error.rs           # typed error codes
│   │   │   └── models.rs          # deserializable response types
│   │   └── tests/                 # wiremock integration tests
│   │
│   ├── playback/          # Audio engine (rodio + stream-download)
│   │   ├── src/
│   │   │   ├── lib.rs             # Player handle, Event enum
│   │   │   ├── engine.rs          # rodio DeviceSinkBuilder, prefetch, auto-advance
│   │   │   └── waveform.rs        # offline RMS peak extraction
│   │   └── tests/                 # engine integration tests
│   │
│   └── app/               # GPUI application binary
│       └── src/
│           ├── main.rs            # Entry point, window setup
│           ├── config.rs          # Settings TOML, keyring, cache dirs
│           ├── assets.rs          # Embedded icons and assets
│           ├── services/          # IO-bound services
│           │   ├── runtime.rs     # gpui↔tokio bridge
│           │   ├── artwork.rs     # Cover art fetch + disk cache
│           │   ├── waveform.rs    # Peak download + cache
│           │   ├── library_db.rs  # SQLite database for local library
│           │   ├── local_library.rs # Local file scanner + m3u parser
│           │   └── navidrome_sync.rs # Navidrome→DB sync
│           ├── state/             # GPUI entities
│           │   ├── session.rs     # Connection + settings
│           │   ├── player.rs      # Queue, transport, scrobble, media keys
│           │   ├── queue.rs       # Pure queue model (unit-tested)
│           │   ├── scrobble.rs    # Scrobble threshold engine
│           │   ├── playlists.rs   # Playlist CRUD state
│           │   └── radio.rs       # Radio station state
│           └── ui/                # Views (one module per screen)
│               ├── root.rs        # Main layout + navigation hub
│               ├── login.rs
│               ├── player_bar.rs  # Bottom transport bar
│               ├── fullscreen_player.rs
│               ├── sidebar.rs
│               ├── albums.rs
│               ├── album_detail.rs
│               ├── artists.rs
│               ├── favorites.rs
│               ├── local_music.rs # Local music browser
│               ├── recent.rs
│               ├── search_bar.rs
│               ├── settings.rs
│               ├── queue_panel.rs
│               ├── playlist_detail.rs
│               ├── radio.rs
│               └── mod.rs         # Shared utilities
│
├── Cargo.toml              # Workspace root
├── CLAUDE.md               # AI assistant guide (detailed module map)
└── README.md               # This file
```

## Architecture

Cargo workspace with three crates. Strict dependency direction:

```
  app (GPUI binary)
    ↓
  playback (audio engine)
    ↓
  subsonic (API client)
```

- **`crates/subsonic`** — Pure async Subsonic/OpenSubsonic API client. `reqwest` + `serde` only. Every request carries fresh token auth. Endpoint methods are grouped by domain. `stream_url()`/`cover_art_url()` build authenticated URLs without issuing requests.
- **`crates/playback`** — Audio engine behind a command/event facade (`Player` ↔ `Event` via tokio `mpsc`). Uses `rodio` for audio output, `stream-download` for HTTP streaming with seek support. Keeps one rodio player across tracks and appends the prefetched next track shortly before the current one ends, so transitions are gapless. Exposes waveform utilities for offline peak extraction.
- **`crates/app`** — The GPUI binary. Layers: `services/` for IO (tokio bridge, artwork cache, waveform fetch), `state/` for gpui Entities (session, player, queue, playlists, radio), `ui/` for views.

All three milestones implemented: M1 (connect/browse/play), M2 (search/queue/playlists/star), M3 (multi-library, scrobbling, radio, transcoding, theming, media keys, artwork cache) — plus waveform seek bar, fullscreen player, lyrics, artist bios, persisted queue, configurable start page.

See [`CLAUDE.md`](CLAUDE.md) for the detailed module-level conventions and async pattern.

## Theming

Scirè supports three theme modes and a custom theme import.

### Built-in modes

| Mode | Description |
|------|-------------|
| **Light** | Light background, dark text — default |
| **Dark** | Dark background, light text |
| **Follow system** | Matches the OS appearance (auto-switches on macOS light/dark toggle) |

Set the mode in **Settings > Appearance > Theme**.

### Custom theme JSON

Place a `theme.json` file in the config directory to load a custom theme:

```bash
# Linux:
~/.config/scire/theme.json

# macOS:
~/Library/Application Support/scire/theme.json
```

The file is a JSON array of theme definitions. The first entry is used:

```json
{
  "themes": [
    {
      "name": "My Theme",
      "mode": "dark",
      "background": "#1a1b26",
      "foreground": "#c0caf5",
      "primary": "#7aa2f7",
      "border": "#1a1b26",
      "muted": "#1a1b26",
      "selection": "#33467c",
      "sidebar": "#16161e"
    }
  ]
}
```

Supported color fields (all optional; missing fields use the default theme value):

`background`, `foreground`, `border`, `muted`, `muted_foreground`, `primary`, `primary_foreground`, `secondary`, `secondary_foreground`, `accent`, `accent_foreground`, `sidebar`, `sidebar_foreground`, `success`, `success_foreground`, `warning`, `warning_foreground`, `danger`, `danger_foreground`, `selection`, `scrollbar_thumb`, `scrollbar_thumb_hover`

To apply the custom theme, select **Custom** in **Settings > Appearance > Theme**.

#### Example themes

Pre-built theme files are in [`themes/`](themes/):

| File | Description |
|------|-------------|
| [`themes/tokyo-night.json`](themes/tokyo-night.json) | Dark blue-based theme |
| [`themes/catppuccin-mocha.json`](themes/catppuccin-mocha.json) | Dark warm-based theme |

Copy one to your config dir to try it:

```bash
cp themes/tokyo-night.json ~/.config/scire/theme.json
```

#### pywal16 integration

[pywal16](https://github.com/eylles/pywal16) generates a colour palette from your wallpaper. Use the template at [`themes/pywal16.json`](themes/pywal16.json) to produce a Scirè theme:

```bash
# Install so pywal processes it on every run
cp themes/pywal16.json ~/.config/wal/templates/scire-theme.json

# Run pywal (auto-runs on wallpaper change too)
wal -i /path/to/wallpaper

# Copy generated theme into place
cp ~/.cache/wal/scire-theme.json ~/.config/scire/theme.json
```

Then select **Custom** in Scirè's appearance settings. Re-run `wal` and copy whenever wallpaper changes.

### CJK font support

Artist and song names in Japanese, Chinese, or Korean render automatically when a CJK font is installed on your system. The app prefers `Noto Sans CJK JP` / `Noto Sans CJK SC` / `Noto Sans CJK KR` and falls back to the system `sans-serif`.

On Linux, install a CJK font package:

```bash
# Debian/Ubuntu
apt install fonts-noto-cjk

# Fedora
dnf install google-noto-sans-cjk-fonts

# Arch
pacman -S noto-fonts-cjk
```

macOS includes Hiragino and Yu fonts — no extra install needed.

### Fullscreen background

The fullscreen player supports five background styles, set in **Settings > Appearance > Fullscreen background**:

| Style | Description |
|-------|-------------|
| **Solid** | Solid background colour |
| **Gradient** | Two-tone gradient from the album art's dominant colours |
| **Vibrant** | Saturated gradient from the album art |
| **Blurred art** | Album cover blurred and scaled to fill the window |
| **Animated** | Album art with a slow zoom animation |

## Build & Run

Requires stable Rust ≥ 1.85 (edition 2024).

```bash
cargo run
```

### Linux dependencies

```
vulkan-loader  mesa-vulkan-drivers  libwayland  libxkbcommon
libX11  fontconfig  freetype  alsa-lib  dbus
```

macOS needs nothing extra. Builds with Xcode Command Line Tools only (gpui's `runtime_shaders` feature avoids requiring `xcrun metal`).

## Configuration

Settings are stored in the platform config directory (`directories::ProjectDirs`):

| Setting | File | Format |
|---------|------|--------|
| App prefs (volume, theme, queue, etc.) | `$CONFIG_DIR/settings.toml` | TOML |
| Password | OS keyring (`keyring` crate) | — |
| Password fallback | Inside `settings.toml` | plaintext (only when keyring fails) |
| Custom theme | `$CONFIG_DIR/theme.json` | JSON |
| Artwork cache | `$CONFIG_DIR/cache/art/` | JPEG/PNG |
| Waveform peaks | `$CONFIG_DIR/cache/waveforms/` | JSON |
| Queue snapshot | `$CONFIG_DIR/queue.json` | JSON |

## Testing

```bash
cargo test --workspace                # all tests
cargo test -p subsonic                # API client (fast, no GPUI build)
cargo test -p playback                # audio engine (self-skips without device)
cargo clippy --workspace --all-targets
```

Manual smoke test: `https://demo.navidrome.org` (user `demo` / password `demo`), or `docker run deluan/navidrome`.

## License

MIT
