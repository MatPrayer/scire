# Search upgrade: Local Music section + ListenBrainz scrobbling — keep v0.20.0

## 1. Search palette — dedicated "Local music" section

Today (`ui/search_bar.rs`): cache-first Ctrl/Cmd+K palette. One `Hits` struct with
`artists/albums/songs`; local albums & songs are interleaved into the server sections
with a "Local" badge; **local artists are filtered out** in `hits_from_cache`
(`a.source != "local"`) because there is no local artist destination. Server `search3`
merges in after the cache.

Target: the palette gains a **"Local music" section** — local artists, local songs and
local albums/EPs grouped under their own header; the server sections keep server rows
only. Local-only library keeps working and now also surfaces artists.

- [ ] `search_bar.rs` — split `Hits` by source. Add `local_artists / local_albums /
      local_songs: Vec<…>`; rename server buckets to `artists/albums/songs` (or
      `server_*`) consistently; update `is_empty`, `merge`, `rank`, `items`,
      `result_rows`, `fetch_result_art` and all tests.
- [ ] `hits_from_cache` — stop dropping local artists. Route `source == "local"` rows
      into the local buckets (artists included now); `in_selection` filter stays
      navidrome-only. `strip_ns` id handling unchanged (local ids pass through).
- [ ] `hits_from_server` — always feeds server buckets only.
- [ ] `merge` — dedupe within each bucket by id. Cross-source `adopt` (server album
      donating a cover to its local twin) is no longer needed: twins now live in
      different sections and never sit beside each other.
- [ ] `rank` — apply the same `sort_key` ranking to every bucket, server and local.
- [ ] New caps `MAX_LOCAL_*` (reuse `MAX_SONGS/ALBUMS/ARTISTS` values) for the local
      buckets.
- [ ] Render: after the three server sections, one **"Local music"** section title,
      then local artist → local album → local song rows (same `row_shell`, `local_badge`,
      click/`+` handlers as today). Skip the section when empty. Empty-query hint text
      updated ("…artists, albums and songs…" → mention your libraries).
- [ ] Keyboard walk: `PaletteItem::LocalArtist` variant; `items()` and
      `selected_child_index()` include local rows in render order (server sections
      first, then local).
- [ ] New event `SearchBarEvent::OpenLocalArtist(String)`; wire activation + click.
- [ ] `local_music.rs` — `LocalMusicView::new(…, artist_filter: Option<String>)`:
      `refresh()` filters `albums_by_source("local")` by `album.artist_id == filter`
      (scanner writes `artist_id = "local:artist:<name>"`, so a filtered grid is one
      query + an in-memory filter). Filtered header shows the artist name + a "Back to
      all local albums" clear button emitting `LocalMusicEvent::ClearFilter`. Extract a
      pure `filter_albums(albums, artist_id) -> Vec<AlbumRow>` and unit-test it.
- [ ] `root.rs` — new `Content::LocalArtist(Entity<LocalMusicView>)` built with the
      filter; add to the render match, `content_vi_move/activate/play/shuffle/tab`,
      `vi_clear_cursors` (per AGENTS.md every new Content variant goes into all of
      them). `open_local_artist(id, cx)` subscribes `LocalMusicEvent::OpenAlbum` /
      `ClearFilter` like the plain LocalMusic branch and pushes history.
- [ ] Tests (`search_bar.rs`): local artist now routed to the local bucket; server
      rows untouched; local merge/rank; local album and its server twin land in
      different sections, no cover adoption.
- [ ] README: update the Search feature bullet (local artists searchable, "Local
      music" section).

## 2. ListenBrainz scrobble for local music

Today: scrobble is Navidrome-only — `PlayerState::fire_scrobble` calls
`client.scrobble(id, submission)` when `scrobble_enabled` and a client exists; local
tracks never scrobble. Listener: `state/scrobble.rs` (`ScrobbleTracker`, 50%-or-4-min
rule, tracks only a `song_id`).

Plan: same threshold tracker, but the actions carry track metadata; local tracks are
posted straight to ListenBrainz (`POST /1/submit-listens`, token auth). Server tracks
keep going through Navidrome (which already forwards to LB — scrobbling local tracks
to LB and server tracks through Navidrome avoids double counts).

- [ ] `config.rs` — `#[derive(Debug, Clone, Serialize, Deserialize, Default)]`
      `ListenBrainzConfig { enabled: bool, token: String }` (empty token = disabled),
      `#[serde(default)]`. Add `pub listenbrainz: ListenBrainzConfig` to `Settings`,
      default in `impl Default`. Stored in `settings.toml` (precedent:
      `password_plaintext`; token is low-stakes, note it; keyring stays an option).
- [ ] New `services/listenbrainz.rs` — `pub const API: &str =
      "https://api.listenbrainz.org/1"`; shared `reqwest::Client` (10s connect /
      30s read timeouts, matching the house pattern); `payload(listen_type, track,
      listened_at)` pure builder (unit-tested: `playing_now` has no `listened_at`,
      `single` carries a unix timestamp, artist/album optional, missing fields
      omitted); `submit(token, payload)` POSTs `{"listen_type", "payload": […]}` with
      `Authorization: Token <token>`; `validate_token(token) -> Result<bool>` against
      `GET /validate-token`. Export from `services/mod.rs`.
- [ ] `state/scrobble.rs` — `ScrobbleTracker` tracks a `ScrobbleTrack { id, artist:
      Option<String>, title, album: Option<String> }` instead of a bare id;
      `ScrobbleAction::{NowPlaying, Submit}` carry it. Thresholds unchanged. Update
      unit tests.
- [ ] `state/player.rs` —
    - fields `lb_enabled: bool`, `lb_token: Option<String>` (mirror of
      `listenbrainz` config), set from `apply_playback_settings` and a new
      `set_listenbrainz(cfg, cx)`; loaded at boot where `scrobble_enabled` is applied
      (~`player.rs:1355`).
    - `fire_scrobble` branches on the current song: `song.local_path.is_some()` +
      `lb_enabled` + token → ListenBrainz `playing_now` / `single` (submission
      timestamp = now via `SystemTime`); otherwise the existing server path.
    - `start_current` / `auto_advanced` / `Event::Position` call sites already hand
      the tracker a song — pass the `ScrobbleTrack` instead of the id.
- [ ] `ui/settings.rs` —
    - new section card **"ListenBrainz"** after Playback; 11th entry of
      `COMPACT_SECTIONS` (`("ListenBrainz", …)` weight) so quick-nav + vi `[`/`]`
      cycling and the compact grid pick it up automatically.
    - `SettingsSwitch::ListenBrainz` (enable/disable, mirrors `set_scrobble`), a
      `lb_token: Entity<InputState>` text field (mirrors `dir_input`) with a
      "Check token" button calling `validate_token` and showing valid/invalid
      (best-effort, no blocking), plus a note: local tracks only — server plays go
      through Navidrome's own forwarding.
    - `is_typing` / `vi_insert` / `vi_activate` include the token field (root's
      `vi_typing` asks Settings by name — the new field must stand the shortcuts
      down like `dir_input` does).
- [ ] `crates/app/Cargo.toml` — no change (reqwest/serde_json already present).
- [ ] README — Scrobbling bullet + Settings docs: local files scrobble to
      ListenBrainz (token under Settings → ListenBrainz) while server plays keep
      going through Navidrome.
- [ ] `cargo test -p scire` (library_db/scrobble/search/settings unit tests),
      `cargo clippy --workspace --all-targets`, `cargo fmt --all`.

## 3. Version

- [ ] Keep `0.20.0` — no `Cargo.toml` bump, no README badge / Cargo.lock churn.