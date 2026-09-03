# Contributing to Scirè

Thanks for stopping by. Scirè is a small, friendly project — it grew mostly as a playground for testing agentic AI coding tools, so the codebase is a mix of hand-written and AI-written code. That's fine. Good ideas, clear bug reports, and well-scoped pull requests are all welcome.

## Project notes

- A Cargo workspace of three crates with a strict, one-way dependency direction:
  `app` (GPUI binary) → `playback` (audio engine) → `subsonic` (pure async API client).
- [`CLAUDE.md`](CLAUDE.md) has the detailed module map, the async pattern, and the toolchain pinning. It's written as a guide for AI assistants, but it's the best map of the codebase there is — read it before touching shared code paths.
- **Module boundaries matter** (see `CLAUDE.md`): `subsonic` must stay free of GPUI and audio types, `playback` free of GPUI types. Don't leak one layer into another.

## Environment

- Stable Rust ≥ 1.85 (edition 2024). On Linux, install the [dependencies](https://github.com/LanaMirko04/scire#linux-dependencies) first.

```bash
git clone https://github.com/LanaMirko04/scire.git
cd scire
cargo run
```

Log in with a Navidrome URL, or point **Settings → Local Music** at a folder to run it serverless.

## Development workflow

Fork, create a branch, and open a pull request against `main`.

```bash
cargo run                     # build + launch
cargo test -p subsonic        # API client tests — fast, no GPUI build
cargo test -p playback        # audio engine tests (may skip without an audio device)
cargo test --workspace        # everything
cargo clippy --workspace --all-targets
cargo fmt --all
```

- Keep PRs **small and focused**. A one-line behaviour change with a clear message beats a sprawling refactor.
- Add a **test with every new endpoint or non-trivial bugfix** — see the testing conventions in `CLAUDE.md`.
- Run `cargo clippy` and `cargo fmt --all` before pushing.

## Conventions worth knowing (from `AGENTS.md`)

- The `gpui` `0.2.2` / `gpui-component` `0.5.1` pair is matched — don't bump one without the other.
- UI separators use a hardcoded `hsla(...)`, not `cx.theme().border` (custom themes may have a zero-alpha border).
- In vi-mode, new content views must implement `vi_cursor` + `vi_move`/`vi_activate`/`vi_clear`, and be wired into `content_vi_*` in `root.rs`.
- Don't hold the SQLite lock across an await — keep DB access synchronous inside `spawn_io` blocks.
- `TrackSource.path` is `Some` for local files and `None` for Subsonic HTTP streams — get this wrong and playback breaks.

## Bug reports & ideas

Open an issue describing the expected vs. actual behaviour, your platform, and steps to reproduce (a few lines is enough). Feature ideas are welcome too — no need to be formal.

Everything here is MIT licensed. Have fun — and if the agentic AI part interests you, there's no better project to tinker with.
