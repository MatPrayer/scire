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

- **Config path**: `~/.config/scire/` (Linux) / `~/Library/Application Support/scire/` (macOS). Was migrated from `com.mirko.navidrome-rusty-client`.
- **Theme file**: singular `theme.json`, NOT `themes.json`.
- **Switching away from Custom theme**: `Theme::apply_config()` overwrites `dark_theme`/`light_theme` stored configs. When user switches Custom → Dark/Light/System, those stored configs must be reset to `ThemeRegistry::default_*_theme()` before `Theme::change()` — otherwise custom colors persist. See `apply_theme()` in `ui/mod.rs`.
- **UI separators**: hardcoded `hsla(0., 0., 0.5, 0.15)` — do NOT use `cx.theme().border` for dividers, row borders, card borders, sidebar borders, or the player-bar top border. Custom theme `border` may have zero alpha.
- **`PlaybackError`**: single struct `PlaybackError(String)` — NOT a multi-variant enum.
- **`engine.rs` double `map_err` on `prepare()`**: both needed — outer handles `JoinError`, inner handles `DecoderError`. Do NOT collapse into one.

## Module boundaries to preserve

- `crates/subsonic` — pure async API client. No gpui, no audio. `reqwest` + `serde` only.
- `crates/playback` — audio engine behind `Player`/`Event` mpsc facade. No gpui types.
- `crates/app` — gpui binary. `services/` for IO (tokio bridge), `state/` for gpui Entities, `ui/` for views.
- `gpui` `0.2.2` + `gpui-component` `0.5.1` are a matched pair; do not bump independently.
