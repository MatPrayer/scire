<div align="center">

# Scirè

**A fast, native desktop music client for [Navidrome](https://www.navidrome.org/) — and for the music already on your disk.**

[![Version](https://img.shields.io/badge/version-0.3.0-6f7ce8?style=flat-square)](Cargo.toml)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-b7410e?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-4c8bf5?style=flat-square)](#build--run)
[![Subsonic](https://img.shields.io/badge/Subsonic-v1.16.1%20%2B%20OpenSubsonic-3fb950?style=flat-square)](http://www.subsonic.org/pages/api.jsp)
[![License](https://img.shields.io/badge/license-MIT-lightgrey?style=flat-square)](#license)

Built with [GPUI](https://www.gpui.rs/) — Zed's UI framework — and [gpui-component](https://github.com/longbridge/gpui-component).<br>
GPU-rendered UI, gapless audio, a real-time 3D visualizer, and no Electron in sight.

</div>

---

## Highlights

|  |  |
|---|---|
| **Gapless playback** | One audio sink across tracks, prefetched hand-over — no gap, no click |
| **Two libraries, one app** | Navidrome over Subsonic, plus local files indexed into SQLite |
| **Real-time 3D visualizer** | Eight software-rendered scenes that switch on the beat |
| **Waveform seek bar** | Per-track amplitude envelope, precomputed for the next track |
| **Fully themable** | Light / Dark / system / custom JSON, with pywal16 support |

## Quick start

```bash
git clone https://github.com/LanaMirko04/scire.git
cd scire
cargo run
```

Log in with your Navidrome URL, username and password — or point **Settings → Local Music** at a folder and skip the server entirely.

<sub>Requires stable Rust ≥ 1.85. See [Linux dependencies](#linux-dependencies) before building on Linux.</sub>

---

## Table of Contents

- [Features](#features)
- [Keyboard Shortcuts](#keyboard-shortcuts)
- [Audio Visualizer](#audio-visualizer)
- [Local Music](#local-music)
- [Internet Radio](#internet-radio)
- [Architecture](#architecture)
- [Repository Structure](#repository-structure)
- [Theming](#theming)
- [Build & Run](#build--run)
- [Configuration](#configuration)
- [Testing](#testing)
- [License](#license)

---

## Features

| Area | Capabilities |
|------|-------------|
| **Playback** | HTTP streaming and direct local-file playback · gapless transitions · seek, pause, resume · ReplayGain (track / album / auto) · output device selection (macOS + PulseAudio/PipeWire) |
| **Formats** | Everything Symphonia decodes — FLAC, MP3, AAC/M4A, ALAC, Vorbis, WAV, AIFF and more (rodio is built with `symphonia-all`) |
| **Browse** | Album grid with infinite scroll and sort (name / new / recent / frequent / random / starred) · artist index with bios and images · album and artist detail pages |
| **Search** | Inline search bar (`/`) and a centered command palette (`Ctrl`/`Cmd`+`K`) with arrow-key navigation — songs, albums, artists via `search3` |
| **Queue** | Shuffle · repeat (off / all / one) · reorder · play-next · clear · configurable end-of-queue behaviour · persisted across restarts · optional resume of the current track's position (**Settings → Playback → Resume where you left off**) |
| **Playlists** | Create, rename, delete, add/remove tracks · local `.m3u`/`.m3u8` files imported as playlists · `:newpl` / `:pl add` from the vi-mode command bar |
| **Favorites** | Star and 1–5 star ratings · dedicated starred view |
| **Multi-library** | Sidebar checkbox selector · all selected libraries merged into one sorted view |
| **Scrobbling** | Calls `/rest/scrobble` at ≥ 50% or 4 min · Navidrome forwards to ListenBrainz / Last.fm |
| **Internet radio** | List, play, add, delete stations · live ICY now-playing title, station name, codec and bitrate |
| **Transcoding** | Per-session format (mp3 / ogg / raw) and max bitrate |
| **Theming** | Light / Dark / Follow-system / custom JSON · pywal16 template · cover-reactive accent colour · CJK font support |
| **Fullscreen player** | Album art, track info, waveform seek bar, lyrics panel, queue panel, five background styles, optional volume slider |
| **3D visualizer** | Eight software-rendered scenes plus a music-timed Auto mode, with a floating mini player — [details below](#audio-visualizer) |
| **Waveform seek bar** | Per-track amplitude envelope (480 buckets, cached to disk) · click to seek · the next track's peaks are computed while the current one plays |
| **OS media keys** | Media keys + Now Playing via `souvlaki` (macOS media center, Linux MPRIS) |
| **Artwork cache** | LRU-evicted disk cache (configurable cap) · HiDPI-aware resolution bump · album-scoped keys, so Navidrome's per-song cover ids don't re-download identical art |
| **Navigation** | Mouse back/forward buttons · bracket keys · configurable default page · vi-mode navigation (below) |
| **Local music** | Directory scanner (`lofty` tags + `folder.jpg` / embedded art) into a SQLite library · incremental mtime-based rescan · periodic background scan · album grid with cover art (cached, no re-query per frame) · album detail view with track listing, play/shuffle/queue per track · cover art in player bar + fullscreen player · engine reads local files via `SourceReader` |

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| <kbd>Space</kbd> | Play / pause |
| <kbd>←</kbd> | Previous track (restarts the current one if > 3 s in) |
| <kbd>→</kbd> | Next track |
| <kbd>↑</kbd> / <kbd>↓</kbd> | Volume ± 5% |
| <kbd>[</kbd> / mouse back | Navigate back |
| <kbd>]</kbd> / mouse forward | Navigate forward |
| <kbd>/</kbd> | Focus the inline search bar |
| <kbd>Ctrl</kbd>+<kbd>K</kbd> / <kbd>Cmd</kbd>+<kbd>K</kbd> | Open the command palette |
| <kbd>Esc</kbd> | Close fullscreen player · dismiss search · cancel |

### Vi-mode navigation

Enable **Vi-mode** in Settings to replace the shortcuts above with vim-style navigation. With it ON, the legacy shortcuts are disabled and the keys below take over; <kbd>i</kbd> returns to insert mode (keys pass through to text inputs) and <kbd>Esc</kbd> back to normal mode. The focused item is highlighted with a glow + border; <kbd>?</kbd> shows the full keymap in-app.

| Key | Action |
|-----|--------|
| <kbd>j</kbd> / <kbd>k</kbd> | Move cursor down / up (sidebar, content list; player-bar volume) |
| <kbd>Enter</kbd> | Open / play the focused item |
| <kbd>h</kbd> / <kbd>l</kbd> | Navigate back / forward in history |
| <kbd>[</kbd> / <kbd>]</kbd> | Cycle album filter tabs (All / New / Recent / Frequent / Random / Starred) |
| <kbd>←</kbd> / <kbd>→</kbd> | Previous / next track |
| <kbd>Space</kbd> | Play / pause |
| <kbd>Ctrl</kbd>+<kbd>h</kbd> / <kbd>j</kbd> / <kbd>k</kbd> / <kbd>l</kbd> | Focus sidebar / player bar / content |
| <kbd>i</kbd> | Insert mode |
| <kbd>:</kbd> | Command mode — `:q` quit · `:help` · `:newpl <name>` · `:pl add <name>` · `:pl list` |
| <kbd>/</kbd> | Search |
| <kbd>?</kbd> | Toggle the in-app vi-mode help |

---

## Audio Visualizer

The fullscreen player can draw a 3D scene driven by the live audio. GPUI exposes no shaders, so scenes are projected in software — world space → perspective divide → `gpui::canvas` — and painted back-to-front. Samples are mirrored off the audio thread through a lock-free ring, then reduced to log-spaced bands by a hand-rolled 4096-point FFT: one per frame, no FFT dependency.

| Scene | Look |
|-------|------|
| **Terrain** | Scrolling spectrum landscape — frequency across, time into the distance |
| **Tunnel** | Flight through rings whose radius follows the spectrum |
| **Sphere** | Rotating Fibonacci point cloud displaced along its normals |
| **Orb** | Wireframe icosphere — bass inflates it, treble roughens its surface |
| **Retro** | PlayStation-era low-poly solids tumbling at the camera: flat shading, depth fog, quantised vertices |
| **Scope** | Polar oscilloscope — the triggered waveform itself wrapped around a ring, with the previous frames trailing behind it |
| **Bloom** | Kaleidoscope mandala — the spectrum folded into mirrored petals across counter-rotating layers |
| **Warp** | Starfield drawn as streaks: speed reads as length, so a drop stretches the field into hyperspace |

The mode cycles **Off → Auto → Terrain → Tunnel → Sphere → Retro → Orb → Scope → Bloom → Warp** and is persisted.

In **Auto**, the scene changes with the music. A bass-weighted spectral-flux detector *arms* on a transient and *commits* only once the low end holds — so the cut lands on the drop rather than on the run-up into it. Scenes are picked at random excluding the last two, held for at least 9 s, and forced after 45 s so ambient tracks still move. Changes cross-fade over 220 ms.

While a scene runs, the overlay's cover-and-info column stands down and a **floating mini player** — cover thumb, title/artist, transport, seek bar, scene picker — sits over the scene instead. With the visualizer off, nothing is rendered and no animation frames are requested.

## Local Music

Add directories under **Settings → Local Music**. A scanner walks each root, reads tags with `lofty`, pulls cover art from `folder.jpg` or the embedded picture, and upserts tracks, albums and artists into a SQLite database (`music.db`). Rescans are incremental — unchanged files are skipped by mtime — and a background scan re-runs every 5 minutes. `.m3u`/`.m3u8` files sitting directly in a root are imported as playlists, with entries resolved relative to the playlist, relative to the root, or as absolute paths.

Local tracks play straight off disk — no HTTP round-trip — through the same engine as streamed ones. The **Local** sidebar entry shows an album grid backed by the database, so albums appear as they are indexed.

Navidrome libraries can also be mirrored into that same database (`services/navidrome_sync.rs`), giving one local index across both sources.

## Internet Radio

Stations are read from and written to the server's internet-radio endpoints. Playback requests ICY metadata (`Icy-MetaData: 1`); the reader strips the interleaved metadata blocks before the decoder ever sees them and reports each new `StreamTitle` on the event channel — so the player bar and the fullscreen overlay show the live track title next to the station name, codec and bitrate. Long titles scroll. Seeking is disabled for live streams.

---

## Architecture

A Cargo workspace of three crates with a strict, one-way dependency direction:

```
  app        GPUI binary — views, entities, IO services
    ↓
  playback   audio engine — rodio + stream-download, behind a command/event facade
    ↓
  subsonic   pure async API client — reqwest + serde, nothing else
```

**`crates/subsonic`** — Pure async Subsonic/OpenSubsonic client. `reqwest` + `serde` only: no GPUI, no audio. Every request carries fresh token auth (`t = md5(password + salt)`, new salt per request). Endpoint methods are grouped by domain, and all catalog calls take an optional music-folder id. `stream_url()` / `cover_art_url()` build authenticated URLs without issuing a request.

**`crates/playback`** — The audio engine, hidden behind a command/event facade (`Player` ↔ `Event` over a tokio `mpsc`); rodio never leaks past it, so the engine stays swappable. One rodio player lives across tracks, and the prefetched next decoder is appended shortly before the current track ends — the hand-over instant comes from a source wrapper, not from polling. Also exports the visualizer sample tap + FFT and offline waveform peak extraction.

**`crates/app`** — The GPUI binary. `services/` for IO (tokio bridge, artwork cache, waveform fetch, SQLite library, scanners), `state/` for GPUI entities (session, player, queue, playlists, radio), `ui/` for views. IO never blocks the GPUI executor, and UI state is mutated only through entity `update`.

All three milestones are implemented — M1 (connect / browse / play), M2 (search / queue / playlists / star), M3 (multi-library, scrobbling, radio, transcoding, theming, media keys, artwork cache) — plus the waveform seek bar, fullscreen player, lyrics, artist bios, persisted queue, gapless playback, the 3D visualizer and local-file support.

See [`CLAUDE.md`](CLAUDE.md) for module-level conventions and the async pattern.

## Repository Structure

<details>
<summary><b>Full tree</b></summary>

```
scire/
├── crates/
│   ├── subsonic/          # Subsonic/OpenSubsonic API client
│   │   ├── src/
│   │   │   ├── client.rs            # HTTP core, auth, envelope unwrapping
│   │   │   ├── endpoints/           # One file per endpoint group
│   │   │   │   ├── system.rs        # ping, getMusicFolders
│   │   │   │   ├── browsing.rs      # getArtists, getArtist, getAlbum, getArtistInfo2
│   │   │   │   ├── lists.rs         # getAlbumList2, getStarred2
│   │   │   │   ├── media.rs         # search3, getLyrics, scrobble, setRating
│   │   │   │   ├── playlists.rs     # CRUD
│   │   │   │   ├── annotation.rs    # star, unstar
│   │   │   │   └── radio.rs         # internet radio CRUD
│   │   │   ├── auth.rs              # md5 token auth
│   │   │   ├── error.rs             # typed error codes
│   │   │   └── models.rs            # deserializable response types
│   │   └── tests/                   # wiremock integration tests
│   │
│   ├── playback/          # Audio engine (rodio + stream-download)
│   │   ├── src/
│   │   │   ├── lib.rs               # Player handle, Event enum
│   │   │   ├── engine.rs            # rodio sink, prefetch, gapless hand-over
│   │   │   ├── source.rs            # HTTP/local source, EndSignal + Tap wrappers
│   │   │   ├── icy.rs               # ICY headers + metadata-block stripping
│   │   │   ├── spectrum.rs          # lock-free sample tap + FFT bands
│   │   │   └── waveform.rs          # offline RMS peak extraction
│   │   └── tests/                   # engine integration tests
│   │
│   └── app/               # GPUI application binary
│       └── src/
│           ├── main.rs              # Entry point, window setup
│           ├── config.rs            # Settings TOML, keyring, cache dirs
│           ├── assets.rs            # Embedded icons and assets
│           ├── services/            # IO-bound services
│           │   ├── runtime.rs       # gpui↔tokio bridge
│           │   ├── artwork.rs       # Cover art fetch + disk cache
│           │   ├── waveform.rs      # Peak download + cache
│           │   ├── library_db.rs    # SQLite library database
│           │   ├── local_library.rs # Local file + m3u scanner
│           │   └── navidrome_sync.rs # Navidrome → DB sync
│           ├── state/               # GPUI entities
│           │   ├── session.rs       # Connection + settings
│           │   ├── player.rs        # Queue, transport, scrobble, radio, media keys
│           │   ├── queue.rs         # Pure queue model (unit-tested)
│           │   ├── scrobble.rs      # Scrobble threshold engine (unit-tested)
│           │   ├── playlists.rs     # Playlist CRUD state
│           │   └── radio.rs         # Radio station state
│           └── ui/                  # Views (one module per screen)
│               ├── root.rs          # Main layout + navigation hub
│               ├── login.rs
│               ├── player_bar.rs    # Bottom transport bar
│               ├── fullscreen_player.rs
│               ├── visualizer.rs    # Software-rendered 3D scenes
│               ├── sidebar.rs
│               ├── albums.rs
│               ├── album_detail.rs
│               ├── artists.rs
│               ├── favorites.rs
│               ├── local_music.rs # Local music album grid
│               ├── local_album_detail.rs # Local album detail view
│               ├── recent.rs
│               ├── search_bar.rs    # Inline search + command palette
│               ├── settings.rs
│               ├── queue_panel.rs
│               ├── playlist_detail.rs
│               ├── radio.rs
│               └── mod.rs           # Shared utilities
│
├── themes/                # Example theme JSON + pywal16 template
├── vendor/                # Local gpui-component fork (see the Cargo.toml patch)
├── Cargo.toml             # Workspace root
├── CLAUDE.md              # Contributor/AI guide (detailed module map)
└── README.md              # This file
```

</details>

---

## Theming

### Built-in modes

| Mode | Description |
|------|-------------|
| **Light** | Light background, dark text — the default |
| **Dark** | Dark background, light text |
| **Follow system** | Matches the OS appearance (auto-switches on the macOS light/dark toggle) |
| **Custom** | Loaded from `theme.json` — see below |

Set the mode under **Settings → Appearance → Theme**. Accent colours adapt to the current album cover on top of whichever mode is active.

### Custom theme JSON

Drop a `theme.json` into the config directory:

```bash
~/.config/scire/theme.json                          # Linux
~/Library/Application Support/scire/theme.json      # macOS
```

The file holds an array of theme definitions; the first entry is used:

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

<details>
<summary><b>All supported colour fields</b> (each optional — missing fields fall back to the default theme)</summary>

`background` · `foreground` · `border` · `muted` · `muted_foreground` · `primary` · `primary_foreground` · `secondary` · `secondary_foreground` · `accent` · `accent_foreground` · `sidebar` · `sidebar_foreground` · `success` · `success_foreground` · `warning` · `warning_foreground` · `danger` · `danger_foreground` · `selection` · `scrollbar_thumb` · `scrollbar_thumb_hover`

</details>

Then select **Custom** under **Settings → Appearance → Theme**.

#### Example themes

| File | Description |
|------|-------------|
| [`themes/tokyo-night.json`](themes/tokyo-night.json) | Dark, blue-based |
| [`themes/catppuccin-mocha.json`](themes/catppuccin-mocha.json) | Dark, warm-based |

```bash
cp themes/tokyo-night.json ~/.config/scire/theme.json
```

#### pywal16 integration

[pywal16](https://github.com/eylles/pywal16) generates a palette from your wallpaper; [`themes/pywal16.json`](themes/pywal16.json) turns that palette into a Scirè theme.

```bash
# Install the template so pywal processes it on every run
cp themes/pywal16.json ~/.config/wal/templates/scire-theme.json

# Generate the palette (also runs automatically on wallpaper change)
wal -i /path/to/wallpaper

# Put the generated theme in place
cp ~/.cache/wal/scire-theme.json ~/.config/scire/theme.json
```

Select **Custom** in the appearance settings, and re-copy whenever the wallpaper changes.

### CJK font support

Japanese, Chinese and Korean names render automatically when a CJK font is installed. Scirè prefers `Noto Sans CJK JP` / `SC` / `KR`, falling back to the system `sans-serif`.

```bash
apt install fonts-noto-cjk                   # Debian/Ubuntu
dnf install google-noto-sans-cjk-fonts       # Fedora
pacman -S noto-fonts-cjk                     # Arch
```

macOS ships Hiragino and Yu — nothing to install.

### Fullscreen background

Set under **Settings → Appearance → Fullscreen background**:

| Style | Description |
|-------|-------------|
| **Solid** | Flat background colour |
| **Gradient** | Two-tone gradient from the album art's dominant colours |
| **Vibrant** | Saturated gradient from the album art |
| **Blurred art** | Album cover blurred and scaled to fill the window |
| **Animated** | Album art with a slow zoom |

---

## Build & Run

Stable Rust ≥ 1.85 (edition 2024).

```bash
cargo run                     # build + launch
cargo build --release         # optimized build (fat LTO, single codegen unit)
```

### Linux dependencies

```
vulkan-loader  mesa-vulkan-drivers  libwayland  libxkbcommon
libX11  fontconfig  freetype  alsa-lib  dbus
```

`dbus` is only needed for media keys / MPRIS — without it that layer degrades to a no-op rather than failing.

**macOS** needs nothing extra, and builds with Xcode Command Line Tools alone (GPUI's `runtime_shaders` feature avoids requiring `xcrun metal`).

### Vendored dependency

`vendor/gpui-component` is a local fork of gpui-component 0.5.1, wired in via `[patch.crates-io]`, carrying a one-line change: the popover shadow is removed so context menus don't cast a halo over a bright album grid.

> [!IMPORTANT]
> gpui `0.2.2` and gpui-component `0.5.1` are a matched pair — don't bump one without the other.

## Configuration

Settings live in the platform config directory, caches in the platform cache directory (`directories::ProjectDirs`, app name `scire`).

| Data | Location | Format |
|------|----------|--------|
| App prefs (volume, theme, library selection, …) | `$CONFIG_DIR/settings.toml` | TOML |
| Password | OS keyring (`keyring` crate) | — |
| Password fallback | inside `settings.toml` | plaintext, only if the keyring fails |
| Custom theme | `$CONFIG_DIR/theme.json` | JSON |
| Artwork cache | `$CACHE_DIR/artwork/` | JPEG / PNG |
| Waveform peaks | `$CACHE_DIR/waveform/` | JSON |
| Queue snapshot | `$CACHE_DIR/queue.json` | JSON |
| Playback position | `$CACHE_DIR/resume.json` | JSON, only while "Resume where you left off" is on |
| Recently played | `$CACHE_DIR/recently_played.json` | JSON |
| Local / synced music library | `$CACHE_DIR/music.db` | SQLite |

| Platform | `$CONFIG_DIR` | `$CACHE_DIR` |
|----------|---------------|--------------|
| Linux | `~/.config/scire/` | `~/.cache/scire/` |
| macOS | `~/Library/Application Support/scire/` | `~/Library/Caches/scire/` |

The Settings page is grouped into **Window**, **Appearance**, **Playback**, **Browsing**, **Streaming**, **Local Music**, **Storage** and **Account**.

## Testing

```bash
cargo test --workspace                 # everything
cargo test -p subsonic                 # API client — fast, no GPUI build
cargo test -p playback                 # audio engine — self-skips without an audio device
cargo clippy --workspace --all-targets
cargo fmt --all
```

- **`subsonic`** — wiremock integration tests asserting auth params, envelope parsing against realistic Navidrome JSON, and error-code mapping. Add one for every new endpoint.
- **`playback`** — streams a generated in-memory WAV through the full engine and asserts the event sequence, plus local-file playback and ALAC decoding.
- **`queue` / `scrobble`** — pure models, unit-tested.

Manual smoke test: [demo.navidrome.org](https://demo.navidrome.org) (`demo` / `demo`), or `docker run deluan/navidrome`.

---

## License

MIT.
