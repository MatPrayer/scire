# AGENTS.md

Scirè — Rust GPUI desktop client for Navidrome.

## First read

- [README.md](README.md) — features, build deps, keyboard shortcuts, theme/pywal workflow
- [CLAUDE.md](CLAUDE.md) — crate map, async pattern, testing conventions, toolchain pinning

## Commands

```bash
cargo run                     # build + launch
cargo test -p subsonic        # API client tests (fast, no gpui build)
cargo test -p playback        # audio engine tests (may skip without audio device)
cargo clippy --workspace --all-targets
cargo fmt --all
```

## Critical quirks (will cause bugs if missed)

- **Local library** — album grid caches DB reads via `scan_version` counter (no re-query per frame). **LocalAlbumDetailView** shows track listing with play/shuffle/queue per track (no star/rating — not in local DB schema). Periodic scan is naive (no file-watch), cover cache has no eviction, scanner is single-threaded.
- **`TrackSource.path` for local files**: when `path` is `Some`, the engine calls `source::open_local()` which reads the file directly — the `url` field is ignored for IO (still used for display). Always set `path: None` for Subsonic HTTP streams.
- **`SourceReader` enum**: `crates/playback/src/source.rs` defines `SourceReader::Http(StreamDownload)` and `SourceReader::Local(File)`. Both implement `Read + Seek` so `rodio::Decoder` accepts either. Do NOT add a third variant without updating both match arms in `source.rs`.
- **`local_music_dirs` milestone tracking**: local music support is implemented incrementally across M1–M9. Check which milestone the current task belongs to before editing shared code paths (`TrackSource`, `PlayerState::stream_url()`, config/settings).
- **`LocalScanner` runs synchronously**: call via `spawn_io` to avoid blocking gpui. Progress via `AtomicU8` (IDLE/SCANNING/DONE) and `AtomicUsize` (files scanned).
- **`lofty` 0.18 trait imports**: `AudioFile` and `TaggedFileExt` must be in scope for `read_from_path`→`properties()` and `primary_tag()`/`first_tag()`. `album_artist` is NOT on `Accessor` in 0.18.
- **Cover cache**: `~/.cache/scire/local_art/<hash>.jpg`. Folder art (`folder.jpg`) preferred over embedded.

- **Config path**: `~/.config/scire/` (Linux) / `~/Library/Application Support/scire/` (macOS). Was migrated from `com.mirko.navidrome-rusty-client`.
- **Theme file**: singular `theme.json`, NOT `themes.json`.
- **Switching away from Custom theme**: `Theme::apply_config()` overwrites `dark_theme`/`light_theme` stored configs. When user switches Custom → Dark/Light/System, those stored configs must be reset to `ThemeRegistry::default_*_theme()` before `Theme::change()` — otherwise custom colors persist. See `apply_theme()` in `ui/mod.rs`.
- **UI separators**: hardcoded `hsla(0., 0., 0.5, 0.15)` — do NOT use `cx.theme().border` for dividers, row borders, card borders, sidebar borders, or the player-bar top border. Custom theme `border` may have zero alpha.
- **`PlaybackError`**: single struct `PlaybackError(String)` — NOT a multi-variant enum.
- **`engine.rs` double `map_err` on `prepare()`**: both needed — outer handles `JoinError`, inner handles `DecoderError`. Do NOT collapse into one.

- **`library_db` thread safety**: `LibraryDb` wraps `rusqlite::Connection` in `Mutex`. All queries lock internally. Do NOT hold the lock across await points — keep DB access synchronous within `spawn_io` blocks.
- **`sync_navidrome` truncate-and-resync**: deletes all navidrome rows before re-import. OK until 50k+ tracks.

## Module boundaries to preserve

- `crates/subsonic` — pure async API client. No gpui, no audio. `reqwest` + `serde` only.
- `crates/playback` — audio engine behind `Player`/`Event` mpsc facade. No gpui types.
- `crates/app` — gpui binary. `services/` for IO (tokio bridge), `state/` for gpui Entities, `ui/` for views.
- `gpui` `0.2.2` + `gpui-component` `0.5.1` are a matched pair; do not bump independently.
