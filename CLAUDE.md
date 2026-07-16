# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Cross-platform (macOS + Linux) desktop music client for [Navidrome](https://www.navidrome.org/) servers, built with GPUI (Zed's UI framework) + [gpui-component](https://github.com/longbridge/gpui-component). Speaks Subsonic API v1.16.1 + OpenSubsonic. All three milestones are implemented: M1 (connect/browse/play), M2 (search/queue/playlists/star), M3 (multi-library switcher, scrobbling, internet radio, shares, transcoding settings, theming, OS media keys via souvlaki, artwork cache eviction). See `README.md` for the user-facing feature list and Linux build deps.

## Commands

```bash
cargo run                     # build + launch the app (crates/app binary)
cargo test --workspace        # all tests
cargo test -p subsonic        # API client tests only (fast, no gpui build)
cargo test -p playback        # playback engine test (plays a WAV via local mock HTTP)
cargo test <name>             # tests matching <name>
cargo clippy --workspace --all-targets
cargo fmt --all
```

## Architecture

Cargo workspace, three crates with a strict dependency direction — UI → services → protocol:

- **`crates/subsonic`** — pure async Subsonic/OpenSubsonic API client. reqwest + serde only; NO gpui, NO audio. Every request carries fresh token auth (`t = md5(password+salt)`, new salt per request, built in `auth.rs`). `client.rs` owns the request core + `subsonic-response` envelope unwrapping + typed error codes (40 = bad credentials). Endpoint methods live in `endpoints/{system,browsing,lists,media,playlists,annotation,radio,sharing}.rs`. All catalog methods accept `music_folder_id: Option<&LibraryId>` (multi-library, wired to UI in M3). `stream_url()`/`cover_art_url()` build authenticated URLs without issuing requests — playback/artwork layers fetch them.
- **`crates/playback`** — audio engine behind a command/event facade (`Player` handle ↔ `Event` stream via tokio mpsc). Internals: rodio 0.22 (`DeviceSinkBuilder`/`MixerDeviceSink`/`Player` — note rodio renamed Sink→Player in 0.22) + stream-download (HTTP → blocking `Read+Seek` with ranged re-requests for seek). The facade hides rodio completely so the engine can be swapped. Must be constructed inside a tokio runtime context.
- **`crates/app`** — the gpui binary (package name `scire`). Layers:
  - `services/runtime.rs` — the gpui↔tokio bridge: global 2-worker tokio runtime; `spawn_io(fut)` spawns there and returns a runtime-agnostic future awaited from gpui tasks; `enter(f)` for constructing tokio-dependent types (the playback engine).
  - `state/` — gpui Entities and pure logic: `session.rs` (settings, SubsonicClient, connect flow, music-folder list + library selection), `player.rs` (owns the queue/position/volume, consumes playback events — the single audio↔UI touchpoint — and drives scrobbling, prefetch, media keys, radio), `queue.rs` (pure queue model: shuffle/repeat/reorder, unit-tested), `scrobble.rs` (pure threshold state machine, unit-tested), `playlists.rs` + `radio.rs` (shared CRUD state), `media.rs` (souvlaki OS media controls, best-effort). `player.rs` distinguishes library playback (queue + Subsonic stream URLs) from live radio (`play_radio`, external URL, seek disabled).
  - `ui/` — views; `root.rs` routes login ↔ main layout (sidebar | content | player bar) and hubs navigation events from child views (`cx.subscribe` on `AlbumsEvent`/`ArtistsEvent`/…).
  - `config.rs` — settings TOML via `directories::ProjectDirs`; password in OS keyring (`keyring` crate), plaintext-in-settings fallback only when keyring fails.

### Async pattern (used everywhere in the app crate)

```rust
cx.spawn(async move |this, cx| {
    let data = runtime::spawn_io(async move { client.some_call().await.map_err(anyhow::Error::from) }).await;
    let _ = this.update(cx, |view, cx| { /* apply */ cx.notify(); });
}).detach();
```

Never block gpui's executor with IO; never share locks with UI state — entities are mutated only via `update`.

## Pinning & toolchain notes

- **gpui `0.2.2` + gpui-component `0.5.1` are a matched crates.io pair** (gpui-component depends on `gpui ^0.2.2`). Do NOT bump one without the other; do NOT switch to git deps casually — gpui-component `main` tracks Zed `main` and churns. gpui types stay confined to `crates/app`.
- gpui is built with the `runtime_shaders` feature: required on machines with Command Line Tools only (no `xcrun metal`). Don't remove it.
- `[profile.dev.package."*"] opt-level = 2` keeps the dev binary usable; gpui debug builds are slow otherwise.
- Edition 2024 across the workspace; let-chains are used (`if let … && …`).
- `souvlaki` (media keys / MPRIS) needs `dbus` on Linux; init is best-effort and `MediaKeys` degrades to a no-op if the backend is unavailable, so it never blocks startup.

## Testing conventions

- `subsonic`: wiremock integration tests in `tests/client_test.rs` — token/auth param assertions, envelope parsing from realistic Navidrome JSON, error-code mapping. Add a wiremock test for every new endpoint.
- `playback`: `tests/engine_test.rs` streams a generated in-memory WAV from a wiremock server through the full engine and asserts the event sequence; it self-skips when no audio device exists (CI).
- Manual smoke: demo.navidrome.org (user `demo`/`demo`) or `docker run deluan/navidrome`. Navidrome quirk: folder browsing is simulated — only use ID3 endpoints (`getArtists`/`getAlbum*`), never `getIndexes`/`getMusicDirectory`.
- Scrobbling is server-side: client only calls `/rest/scrobble` (`submission=false` on start, `true` at ≥50%/4min); Navidrome forwards to ListenBrainz/Last.fm itself.
