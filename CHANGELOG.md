# Changelog

All notable changes to Scirè are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/).

## [1.0.0] — 2026-09-23

First stable release. Everything below landed across the 0.x series; this entry
is the feature set 1.0 ships with rather than a diff against a previous tag.

### Playback

- Gapless playback: one audio sink across tracks, prefetched hand-over, no gap
  and no click.
- Streams from Navidrome over Subsonic v1.16.1 + OpenSubsonic, and plays local
  files straight off disk.
- Everything Symphonia decodes: FLAC, MP3, AAC/M4A, ALAC, Vorbis, WAV, AIFF.
  A 32-bit FLAC the decoder cannot read is retried transcoded.
- ReplayGain (track / album / auto) applied by scaling the engine volume, held
  under the track's peak so a boost cannot clip.
- Output device picker — PulseAudio/PipeWire sinks on Linux, Bluetooth
  included — and playback follows the route when a device is connected or
  pulled out.
- Queue with shuffle, repeat, reorder, play-next and clear, persisted across
  restarts, with optional resume of the current track's position.
- Internet radio with live ICY now-playing titles.
- Per-session transcoding format and bitrate.
- OS media keys and Now Playing via `souvlaki` (macOS media center, Linux
  MPRIS).
- Scrobbling through Navidrome, and direct ListenBrainz submission for local
  files.
- Every `pactl` call the Linux device layer makes is bounded: a sound server
  that stops answering can no longer wedge the audio engine's control loop.

### Library

- SQLite library cache: the album grid, artist grid, album pages and search all
  paint from the last sync before the server answers, and work with no server
  at all.
- Incremental Navidrome sync — a settled library costs three listing requests
  and a local compare.
- Local music scanner with mtime-based incremental rescans, embedded and
  folder cover art, `.m3u`/`.m3u8` playlist import and a 5-minute background
  pass.
- Multi-library support with a sidebar selector, merged into one sorted view.
- Command palette (`Ctrl`/`Cmd`+`K`) matching words in any order, plus a
  full-page advanced search with genre, year, length, source, star, format and
  bitrate filters.
- Playlists, favourites and 1–5 star ratings.

### Interface

- GPU-rendered GPUI interface with no Electron.
- Real-time 3D visualizer: eight software-rendered scenes, music-timed Auto
  switching, a floating mini player and a live tuning card.
- Waveform seek bar with per-track amplitude envelopes, cached to disk and
  pre-warmed for the next track.
- Fullscreen player with lyrics and queue panels, five background styles and
  four cover sizes.
- Lyrics from tags, sidecar `.lrc` files or LRCLIB, followed and scrolled line
  by line, clickable to seek.
- Themes: Light / Dark / system / custom JSON, a pywal16 template, and a
  cover-reactive accent colour.
- Separate font-size and UI-scale controls, album page layouts, a foldable
  sidebar and a self-laying settings page.
- Optional vi-mode keyboard navigation with `:` commands and in-app help.
- **Settings → About** names the running build — version and platform — with a
  Copy button that puts the same line on the clipboard for a bug report.
- The window reopens where it was left, at the size and maximized state it had
  — unless the display it was on is no longer attached, in which case it opens
  centred rather than off-screen.

### Security

- Credentials live in the OS keyring, with a plaintext fallback only where the
  keyring is unavailable — and the password is no longer written to disk before
  the server has accepted it.
- `settings.toml` is written owner-only on unix, and every persisted file
  (settings, queue, resume position) is written atomically.
- Auth tokens and salts are stripped from every error message before it can
  reach the interface or the log.

### Packaging

- Prebuilt artifacts on the release page: a Linux `x86_64` tarball with an
  `install.sh` that needs nothing but coreutils, a `.deb` for Debian and
  Ubuntu, and a macOS `arm64` disk image to drag into Applications (ad-hoc
  signed, not notarised).
- Arch Linux: a `scire` package in the AUR, built from source at the tag.
- Tagging `v*` builds both on GitHub Actions and drafts the release.

[1.0.0]: https://github.com/MatPrayer/scire/releases/tag/v1.0.0
