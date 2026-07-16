# Scirè

A cross-platform desktop music client for [Navidrome](https://www.navidrome.org/) servers, written in Rust.  
Built on [GPUI](https://www.gpui.rs/) (Zed's UI framework) and speaks Subsonic API v1.16.1 + OpenSubsonic.

## Features

- **Connect** to any Navidrome server: token auth, password stored in the OS keyring
- **Browse** albums and artists with full track listings and album art
- **Search** globally across songs, albums, and artists
- **Queue** with shuffle, repeat (off / all / one), play-next, and reorder
- **Playlists**: create, rename, delete, add/remove tracks
- **Favorites**: star and 5-star ratings with a dedicated view
- **Multi-library** support (Navidrome ≥ 0.58) with a sidebar switcher
- **Scrobbling**: Navidrome forwards to ListenBrainz / Last.fm server-side
- **Internet radio**: list, play, add, delete stations
- **Shares**: copies the public link to clipboard
- **Transcoding**: configure format and max bitrate per session
- **Full-screen player** with album-colour gradient background
- **Theming**: light / dark / follow-system
- **OS media keys** and Now Playing (macOS) / MPRIS (Linux) via souvlaki

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Space` | Play / pause |
| `←` | Previous track |
| `→` | Next track |
| `↑` | Volume +5% |
| `↓` | Volume −5% |
| `[` | Navigate back |
| `]` | Navigate forward |
| `Esc` | Close full-screen player |

## Build & Run

Requires stable Rust (edition 2024, ≥ 1.85).

```bash
cargo run
```

### Linux dependencies

```
vulkan-loader  mesa-vulkan-drivers  libwayland  libxkbcommon
libX11  fontconfig  freetype  alsa-lib  dbus
```

macOS needs nothing extra. Builds fine with Xcode Command Line Tools only (`runtime_shaders` avoids requiring `xcrun metal`).

## Testing

```bash
cargo test --workspace          # all tests
cargo test -p subsonic          # API client only (fast, no GPUI build)
cargo test -p playback          # audio engine (self-skips without audio device)
cargo clippy --workspace --all-targets
```

Manual smoke test: `https://demo.navidrome.org` (user `demo` / password `demo`), or `docker run deluan/navidrome`.

## Architecture

Cargo workspace: three crates, strict dependency direction: UI → services → protocol.

| Crate | Role |
|-------|------|
| `crates/subsonic` | Pure async Subsonic/OpenSubsonic API client: no UI, no audio |
| `crates/playback` | Audio engine (rodio + stream-download) behind a command/event facade |
| `crates/app` | GPUI application: state entities, services, views |

See [`CLAUDE.md`](CLAUDE.md) for the detailed module map and conventions.

## License

MIT
