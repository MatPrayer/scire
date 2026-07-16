# AGENTS.md

This repo is Scirè, a Rust desktop client for Navidrome. Start with [README.md](README.md) for features and platform notes, then [CLAUDE.md](CLAUDE.md) for crate layout, testing conventions, and toolchain pinning.

## Working conventions
- Keep edits small and targeted. Prefer minimal changes that fit the existing module boundaries.
- Keep UI code in [crates/app](crates/app), protocol/client logic in [crates/subsonic](crates/subsonic), and audio-engine work in [crates/playback](crates/playback).
- Preserve the dependency direction UI → services → protocol. Keep GPUI types confined to [crates/app](crates/app), and avoid blocking the GPUI executor with IO.
- This workspace uses Rust edition 2024; keep changes compatible with let-chains and the current toolchain.

## Commands
- Build/run: `cargo run`
- Test: `cargo test --workspace`, `cargo test -p subsonic`, `cargo test -p playback`
- Lint/format: `cargo clippy --workspace --all-targets`, `cargo fmt --all`

## Repo-specific notes
- `gpui` `0.2.2` + `gpui-component` `0.5.1` are a matched pair; do not bump them independently.
- GPUI builds on macOS may need `runtime_shaders` when only the Command Line Tools are installed and `xcrun metal` is unavailable.
- `souvlaki` media-key/MPRIS support needs `dbus` on Linux; initialization is best-effort and must not block startup.

## Testing expectations
- New Subsonic endpoints should include WireMock coverage in [crates/subsonic/tests/client_test.rs](crates/subsonic/tests/client_test.rs).
- Playback changes should be covered by [crates/playback/tests/engine_test.rs](crates/playback/tests/engine_test.rs) when feasible.
