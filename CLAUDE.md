# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository. It records rules, traps and where things live; the history behind each decision is in git.

## Project

**Scirè** — cross-platform (macOS + Linux) desktop music client for [Navidrome](https://www.navidrome.org/) servers and local music files, built with GPUI (Zed's UI framework) + [gpui-component](https://github.com/longbridge/gpui-component). Speaks Subsonic API v1.16.1 + OpenSubsonic; identifies as `scire` in the Subsonic `c` param. Local music: engine reads files via `SourceReader::Local(File)`; virtualized album grid with viewport-scoped, size-bucketed cover textures (DB refreshed via `scan_version`); `LocalAlbumDetailView`; incremental mtime-based scanner; background scan every 5 min. `local_music.rs` keys in-flight cover jobs by album id and prunes them to the visible window — dropping a `gpui::Task` cancels it, and a flat capped list cancelled the cards currently on screen during a fast scroll. See `README.md` for features and Linux build deps.

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

## Versioning

`[workspace.package] version` in `Cargo.toml` is the single source of truth. Mirrors that must move in the same commit: the README badge (`badge/version-<x.y.z>-6f7ce8`) and `Cargo.lock`, where **all four workspace members** (`scire`, `subsonic`, `playback`, `dmg`) carry the version — any `cargo check` rewrites them, stage the result. `packaging/macos/bundle.sh` reads the version at package time, so after a bump run `cargo build --release` first.

[SemVer 2.0.0](https://semver.org/): MAJOR incompatible API, MINOR compatible features, PATCH fixes. Major `0` = unstable API. The bump ships **in the commit that makes the change** — never a separate "bump version" commit. Refactors, tests, docs and comment-only changes need no bump. If the change is already pushed when the bump is noticed, use a follow-up `chore: bump version to x.y.z` commit, never an amend.

## Packaging & releases

The packaging scripts (`packaging/linux/make-tarball.sh`, `packaging/linux/make-deb.sh`, `packaging/macos/bundle.sh`) package `target/release` **without rebuilding** — run `cargo build --release` first. All read the version from `[workspace.package]`.

- **Linux tarball** — binary, desktop entry, SVG icon, LICENSE/CHANGELOG/README, `packaging/linux/install.sh` at top level. The install script needs only coreutils (no `librsvg`; only the scalable icon ships, unlike from-source `install-icon.sh` which renders hicolor PNGs). It rewrites `Exec=scire` to an absolute path, since `~/.local/bin` is often not on a graphical session's PATH.
- **Debian `.deb`** (`make-deb.sh`, alongside the tarball). `Depends:` are **derived**: `derive_depends` resolves the binary's **direct `NEEDED` entries** (`readelf -d`, not `ldd`'s transitive closure) to owning packages via `dpkg-query`, following each `.so` to its real path first. It **unions in `RUNTIME_DEPENDS`** — libraries gpui `dlopen`s (Vulkan via ash, Wayland, X11) that the ELF doesn't list; without them the package installs and then can't open a window. `Recommends:` `mesa-vulkan-drivers`, `pulseaudio-utils` (`pactl`, the output picker). Only valid built on the target distro (CI does this); off Debian the script warns and uses a static list. Without `dpkg-deb` it assembles with `ar` in dpkg's order: `debian-binary`, `control.tar.gz`, `data.tar.gz`. No maintainer scripts (dpkg triggers handle desktop db / icon cache). `Exec=scire` left alone (`/usr/bin` is on PATH).
- **Arch** — `packaging/aur/{PKGBUILD,.SRCINFO}` are the canonical copies of the AUR repo (`packaging/aur/README.md` has publish steps). Builds **from source at the tag**. `cargo fetch --locked` / `cargo build --frozen` are load-bearing: the workspace patches two crates via `[patch.crates-io]` and a fresh resolve would take upstream ones. `depends` include Vulkan, Wayland, X11 (dlopened).
- **macOS** — `.dmg` from `make-dmg.sh` (`cargo dmg` locally, same script on CI). Presentation (background, icon positions via Finder/Apple Events) is optional and gated by `THEMED`; `finder_available` probes with a deadline since `osascript` can hang waiting on Automation consent. CI sets `DMG_PLAIN=1`. Always built: `.app`, Applications symlink, volume icon (file + `SetFile`, not Finder). `bundle.sh` falls back to committed `scire.icns` without `rsvg-convert`. Ad-hoc signed, not notarised (first launch: right-click → Open).

`.github/workflows/ci.yml`: fmt + clippy + tests on both platforms per push/PR. `release.yml`: on `v*` tag, builds both and opens a **draft** release — publishing stays a person's decision.

## Architecture

Cargo workspace, strict dependency direction UI → services → protocol.

### `crates/subsonic` — pure async API client

reqwest + serde only; NO gpui, NO audio.

- Every request carries fresh token auth (`t = md5(password+salt)`, new salt per request, `auth.rs`). `client.rs` owns the request core, `subsonic-response` envelope unwrapping, typed error codes (40 bad credentials, 50 not authorized).
- One shared `reqwest::Client`: `pool_idle_timeout` 300s (TLS handshake is half a cold request), `connect_timeout` 10s, `read_timeout` 30s (per-gap, not total — `getArtists` returns a whole library in one body). Without timeouts a packet-dropping host blocks both IO workers for minutes.
- HTTP/1.1 only (`default-features = false` drops `http2`).
- **Errors never leak the auth token**: `http_error` calls `without_url()` before the error leaves the crate (request URLs carry `u`, `t`, `s`). `Error::user_message()` is the UI wording; `Error::is_transient()` decides whether Retry is offered (timeout, refused, 5xx, 429 — never bad credentials, bad URL, 404).
- Endpoints in `endpoints/{system,browsing,lists,media,playlists,annotation,radio,navidrome}.rs`. All catalog methods take `music_folder_id: Option<&LibraryId>`.
- `system.rs`: `start_scan()`/`get_scan_status()` → `ScanStatus { scanning, count }`. `startScan` is admin-only on Navidrome; failure is reported, not fatal.
- `media.rs` lyrics: **`getLyricsBySongId`** (OpenSubsonic `songLyrics`, → `Vec<StructuredLyrics>`, `line[].start` ms) is the one that works — Navidrome publishes tag + sidecar `.lrc` lyrics only by song id. Classic `get_lyrics(artist, title)` is fallback only for servers answering by-id with error 70. Empty `"lyricsList":{}` parses as empty list.
- `navidrome.rs`: `scrobble_forwarding()` logs into Navidrome's native API (`POST /auth/login` → JWT in `X-ND-Authorization`), reads `GET /api/{lastfm,listenbrainz}/link` → `ScrobbleLink`; 404 = `Disabled`, no `/auth/login` = `Ok(None)`. Sends the **plaintext password**, so callers gate on `is_secure_transport()` (https, loopback, private/link-local IPv4, `localhost`, `.local`).
- `stream_url()`/`cover_art_url()` build authenticated URLs without requesting.
- `Song.local_path: Option<String>` (serde-skipped when `None`) for local tracks.

### `crates/playback` — audio engine

Command/event facade (`Player` handle ↔ `Event` stream via tokio mpsc); hides rodio entirely. Must be constructed inside a tokio runtime. rodio 0.22 (`DeviceSinkBuilder`/`MixerDeviceSink`/`Player` — rodio renamed Sink→Player) + stream-download (HTTP → blocking `Read+Seek` with ranged re-requests).

- **Sources**: `TrackSource` → `SourceReader::{Http(StreamDownload), Local(File)}`; `path: Some` opens via `source::open_local()`. HTTP clients: 10s connect / 30s read timeouts (`Command::Play` awaits the open inline on the control loop). `PlaybackError::from_http` runs messages through `scrub_urls` (strips query strings = token/salt).
- **`TrackSource.live` = internet radio**; only live sources send `Icy-MetaData: 1`. On library files that header makes servers drop `Content-Length` → unseekable → m4a with trailing `moov` unreadable.
- `open`/`open_local` return `Opened` with a `Hint` (MIME or extension). symphonia 0.5.5 ignores the hint; its real use is `engine::decoder_failure`, which rewrites rodio's catch-all `UnrecognizedFormat` for MP4/M4A into the actual cause (no length vs. server ignoring ranges), logging the original.
- `Event::PrefetchFailed { id, error }` reports a failed prefetch while the current track plays.
- **Gapless**: one `rodio::Player` across tracks; `PrefetchNext` prepares the next decoder, appended only within `COMMIT_LEAD` (3s) of the end (rodio's queue has no removal). `TrackEnded { auto_advanced, started }` reports the id that actually started. Hand-over instant from the `EndSignal` wrapper (`source.rs`), not polling `empty()`.
- **Seeking runs off the control loop** (`spawn_seek`; sink is `Arc<rodio::Player>`). `try_seek` blocks until the audio thread moves the decoder, which over HTTP can mean many ranged requests (measured 5–30s). While in flight: tick reports the **destination** (not `get_pos()`), skips the route watch; still running at next tick (500ms) → `Event::Buffering`, completion → `Playing`/`Paused`. Stale results dropped via generation counter (`cancel_seek`). Device-switch/route-change reopen at zero then seek the same way.
- **Output routing**: the `MixerDeviceSink` outlives tracks, opened only when absent. Tick watches the route: `route_lost` compares `open_device` against where audio would go now (`resolved_device_name`, which reports the **fallback** when a chosen device is missing, so its return is noticed). Runs idle too (`IDLE_ROUTE_CHECK_TICKS` ~8s vs ~2s playing). `Command::Play` drops a stale output first; `Resume` forces a check. `route_action` (pure, tested): device pulled during playback → `HoldForDevice` (pause, resume when `lost_device` returns); route moved to a newly appeared device → `Follow`. `route_grace` lets an explicit Play/Resume outrank waiting.
- **Linux device layer is `pactl`, not cpal** (`pulse.rs`) — cpal/ALSA can't see PipeWire sinks (Bluetooth missing). `output_devices()`, `device_present`, default-route name from `pactl list sinks`; cpal is fallback (and all of macOS). `open_output` opens the default stream, `retarget_stream` moves it with `pactl move-sink-input`, finding our sink-input by `node.name = "alsa_playback.<exe>"`. Traps: opening registers a transient node — **read back the move**; sink id `u32::MAX` means mid-move, ask again. A router like EasyEffects undoing the move → `MoveResult::Grabbed`, log and stop. **Every `pactl` call bounded** (`TIMEOUT` 2s, `bounded_output`, stdout drained on a second thread so a full pipe can't deadlock); a miss returns `None` = "could not ask".
- **Direct output** (`direct.rs`, Linux only; macOS lists nothing): `Command::SetDirectOutput(Some(alsa_id))` opens an ALSA `hw:` card, bypassing PipeWire, at each track's own rate/channels (`plan`, pure/tested: rate > channels > format `I32 > I24 > F32 > I16`; missing rate → nearest above, labelled "resampled"). Bit-perfect for 16/24-bit because symphonia/dasp int↔f32 are power-of-two scalings, rodio's resampler passes through at equal rates, mixer is `0.0 + x` — so **volume is forced to exactly 1.0** (`applied_volume`). `direct_devices()` ids rewrite `hw:CARD=<index>` to `/proc/asound/cardN/id` names (`stable_id`; indices move with USB DACs). Engine `Output { sink, direct }`; `start_track` reopens when `DirectFormat::serves(want)` fails, `commit_next` won't append a different-shape track (gapless only across same rate — the app's non-auto-advanced `TrackEnded` starts the next). Busy card (`OpenError::Busy`): retried `DIRECT_BUSY_PATIENCE` (7s, PipeWire's suspend timeout) only right after dropping our own output, then **falls back to the shared output** with `direct_error`; that fallback sticks until the output is chosen again. Route watch skipped while a direct target is set. `Event::OutputOpened { device, direct, direct_error }`.
- **`spectrum`**: `Tap<S>` wrapper (inserted in `engine::append`) mirrors mono samples into lock-free `SpectrumTap` ring (`AtomicU32` + write counter; audio thread never locks). `spectrum::analyze`: 4096-sample window, hand-rolled radix-2 FFT, log bands. `Player::spectrum_tap()`; unchanged counter = no audio, consumers decay.
- **`waveform::peaks_from_bytes`**: offline decode → normalized RMS buckets (gamma-expanded). Pure CPU; call from a blocking context.
- `playback::MAX_VOLUME` is 4.0 (ReplayGain boosts exceed 1.0).

### `crates/app` — gpui binary (package `scire`)

#### services/

- **`runtime.rs`** — gpui↔tokio bridge: global **2-worker** tokio runtime. `spawn_io(fut)` returns a runtime-agnostic future; `enter(f)` for tokio-dependent construction. **Blocking work must use `spawn_blocking_io`** (scanner, decodes) or it pins a worker and stalls HTTP. gpui tasks have no tokio reactor: `tokio::time::sleep` panics there — use `cx.background_executor().timer()`.
- **`artwork.rs`** — cover fetch + disk cache.
  - **Navidrome quirk**: every song has its own `coverArt` id (`mf-<songId>_<hash>`). For a *song's* cover use `song_cover(&Song)` → (id to request, album-scoped key) with `fetch_as`/`cached`. Album/artist covers use plain `fetch`.
  - Art is **center-cropped square on the way in** (`square_crop`, also used by scanner and artist photos). `ObjectFit::Cover` is not a fix (paints outside bounds, rounding lands off-tile). Dimensions read from header; formats outside jpeg/png stored untouched.
  - All writes via `write_atomic` (temp name = pid + per-call counter; losing a rename to identical bytes counts as success).
  - Pre-existing art squared in place by `squarify_cached_art` at startup (one `SQUARED_MARKER` per dir; public so the local prune doesn't delete it). Never bump the cache key (would re-download everything / blank local covers).
  - `bucket` is public: views compare rungs, not widths (Medium and Large share 512). `cached_best` returns any cached rung.
  - Cache keys drop Navidrome's `_<hash>` suffix (`stable_key`), so replaced art isn't seen by key. Sync compares album cover ids before/after (`covers_to_revalidate`; Full = every album) and spawns `revalidate_album_covers`: re-downloads only rungs on disk (smallest probed first, byte-compared), rewrites in place under both `al-<id>` and `album-<id>`, rebuilds blur. gpui caches decoded images **by path**, so rewritten paths go through `take_replaced` → `RootView` poll (`REPLACED_ART_POLL`) → `ImageSource::remove_asset` + `refresh_windows`. Artist photos not rechecked.
- **`art_precache.rs`** — `Settings::precache_art` bulk warm-up: walks album/artist rows, skips cached, fetches `PRECACHE_CONCURRENCY` (2) at a time (under artwork's shared semaphore). Local rows skipped. `RootView::maybe_precache_art` runs after each sync (one pass at a time via `precaching`); the Settings switch starts one with a progress line.
- **`waveform.rs`** — seek-bar peaks from a low-bitrate transcode → 480 buckets, JSON cache keyed by song id, **`.v3` suffix — bump when the format changes**. `PlayerState::prewarm_next_waveform` (from `refresh_prefetch`, gated on `waveform_enabled`) precomputes the next track; `fetch_peaks` holds a per-song lock.
- **`library_db.rs`** — SQLite (`~/.cache/scire/music.db`): `tracks`, `albums`, `artists`, `track_artists`, `album_artists`, `config`. `Mutex<Connection>`.
  - Migrations: `_schema_version`, currently **8**. **Every step goes through `migration_step`** (batch + version row in one transaction — a half-applied batch otherwise bricks the DB with `duplicate column name`). v4 source/`album_id` indexes; v5 `tracks.artist_id` index + wipe synced tracks; v6 `track_artists` + wipe; v7 `album_artists` (no wipe, refilled by next sync); v8 local technical/file/ReplayGain metadata, local fingerprints marked stale.
  - Both `artist_id` columns hold only the server's **primary** credit; collaborations live in `track_artists`/`album_artists`. Delete paths (`delete_track`, `delete_tracks_for_album`) must clear `track_artists` too — no cascade.
  - `album_libraries`, `appears_on` (credited on a track via `track_artists`, not credited on the album via `album_artists`).
  - `album_by_id(source, id)` — source is a guard, `albums.id` alone is the PK.
  - **Write in batches** (one transaction; `upsert_catalog`).
  - `library_stats(source, library_ids)` counts from album rows' `song_count`/`duration`, not tracks (incremental sync leaves tracks partial).
- **`album_info.rs`** — album About card online sources, behind `Settings::album_info_wikipedia`/`album_info_musicbrainz` (`Query::sources`; a disabled service gets no request; cache keyed per source set, `Sources::covers` decides re-asking). MusicBrainz → release group (server MBID, else exact `base_title` match with edition noise stripped) → url-rels (link icons, classified by host) + Wikidata → English Wikipedia intro. Fallback: Wikipedia search, exact title and intro naming the artist. `pick_group` tiebreaks same-titled groups by year. `mb_get` spaces MusicBrainz 1.1s process-wide; `get_json` retries 503/429 up to 3× (`retry_delay`, pure). Disk cache: hits 30d, misses 7d; failures not cached. Exists because Navidrome's `getAlbumInfo2` notes are truncated Last.fm summaries: `looks_truncated` adds "…" and a *Read more on Last.fm* button (`read_more`).
- **`artist_info.rs`** — artist-page counterpart; shares client, spacing, retries, `cached`/`store` (own `artist_info_cache_dir`), behind `artist_info_wikipedia`/`artist_info_musicbrainz`. Resolves by `getArtistInfo2` MBID, else via one of its albums (`credited_artist`), else a unique confident name hit (`unique_artist`). `official homepage` → `LinkKind::Homepage`. Wikipedia fallback accepts bare or musical-qualifier titles; bare must pass `sounds_musical`. Album titles steer search, not in cache key.
- **`advanced_search.rs`** — full-page search model (`Filters`, `SearchKind`, `SortBy`, `Facets`) and `LibraryDb::search_advanced`/`search_facets`. See `ui/advanced_search.rs`.
- **`local_library.rs`** — sync scanner using `lofty`; walks `local_music_dirs`, extracts art (`folder.jpg`/embedded), technical/file/ReplayGain metadata, upserts into `LibraryDb`. Added date = earliest file creation time (absent if unavailable). Scoped cache rebuild: invalidate fingerprints, rescan with content-hashed art, prune only unreferenced files in `local_art/`; never touches music files. Progress atomics + process-global `SCAN_IN_FLIGHT` (a concurrent `scan` returns `Ok(())` without scanning — **tests must serialise on `SCAN_TESTS`**).
- **`navidrome_sync.rs`** — reconciles catalog into `LibraryDb` (source=`navidrome`). Phases: list albums (paginated `getAlbumList2`) → reconcile → fetch tracks, `ALBUM_FETCH_CONCURRENCY` (6) `getAlbum` via `JoinSet`.
  - `SyncMode::Incremental`: `needs_track_fetch` compares listing `songCount`/`duration` against `album_fingerprints`; `getAlbum` only for new/changed. `track_rows` is `COUNT(*)` (interrupted syncs must look incomplete); duration tolerance 1s. Album *rows* always rewritten (play_count/starred move). Vanished albums deleted, then `prune_orphan_artists`.
  - `fetch_album_tracks` clears the album's tracks before re-insert; writes each song's own `artistId` (fallback album artist); `song_credits` → `track_artists`; `album_credits` → `album_artists` and artist rows, one per credit (`album.artist` is a joined display string).
  - `SyncMode::Full` wipes first — escape hatch for re-tags that don't move count/duration.
  - `getArtists` once per music folder for artist covers only (failure logged). `upsert_catalog` writes artist `cover_art` as `COALESCE(?, existing)`.
  - Walks one music folder at a time so rows record `library_id`. Pagination stops on a short page or a head id repeating the previous page *within the same folder*.
  - `SyncProgress` atomics. Started once per session from `root.rs` behind its own `sync_started` flag, delayed 30s.
  - `run_server_scan` (`startScan` + poll `getScanStatus`, runs in tokio so `tokio::time` OK). Grace period: idle counts only after scanning was seen or grace elapsed.

**Refresh vs rescan are deliberately split.** Sidebar **Refresh library** (`RootView::refresh_library`) = local scan then incremental reconcile, in sequence; never asks the server to rescan. **Settings → Library** holds *Scan server library* and *Rebuild local cache* (art reset + forced scan + `SyncMode::Full` if connected); both report status lines; admin-only error 50 is surfaced. Progress: `RefreshStage` (root.rs, tested); `ui::poll_until_done` awaits on `cx.background_spawn` and samples atomics on a gpui timer. Only import has a denominator (filled bar); others get a bare track. On completion catalog views are dropped and redrawn only if a listing is showing. Failures surface via `RootView::refresh_error` (tooltip when folded).

#### state/

- **`session.rs`** — settings, `SubsonicClient`, connect flow, music folders + library selection. A saved server's transient ping failure (`worth_retrying`) is retried on `reconnect_delay` backoff (2/5/15/30s, then 1min); a login being typed is not; `connect_generation` cancels stale retries.
- **`player.rs`** — queue/position/volume, consumes playback events (the single audio↔UI touchpoint), scrobbling, prefetch, media keys, radio (`play_radio`, seek disabled).
  - `Event::Failed` **skips to next** while `failed_streak < MAX_FAILED_STREAK` (5); streak cleared by `Playing` and `start_current`, so the failure path re-applies streak and `last_error` after calling it.
  - Before skipping, a remote track is retried once transcoded (`retryable_transcode` → `transcode_retry`): symphonia can't decode **32-bit FLAC** (reports `IoError("end of stream")`). `unplayable_as_stored`/`stream_opts_for` (pure) pre-empt it when `bitDepth` is known. Forced `format=mp3` 320kbps; a user-set format wins except `raw`. Local 32-bit FLAC still fails.
  - **ReplayGain** scales engine volume: `replaygain_linear` (pure) = `10^((gain+baseGain+preamp)/20)`, album falling back to track, `fallbackGain` for untagged, capped at `1/peak` unless `RgTuning::prevent_clipping` is off (`Settings::replay_gain_preamp`/`replay_gain_prevent_clipping`; no gain → unity, pre-amp not applied); `effective_volume` multiplies by user volume, ceiling `MAX_VOLUME`. `Auto` = Album if the queue is one album. Recomputed on queue edits. Local tracks carry no gain (unnormalized).
  - Queue persisted to `queue.json`; position to `resume.json` only with `Settings::resume_playback`, written each whole second of `Event::Position` + on pause (no shutdown hook — crashes). Restored only if it names the restored current song, applied as a **pending seek** on that track's first `Playing`, dropped if the user skips.
  - `seek` writes the destination into `position` before the engine call. `smooth_position`/`smoothed` (pure): position + wall time since last tick, only while `playing && !buffering`, capped at `SMOOTH_MAX` (1s), clamped to duration. `position` itself never stores the extrapolation. All assignments go through `set_position`.
  - **Notifies on every event including `Position`** — any observer needs a change gate (see recent.rs).
- `queue.rs` (pure queue model, tested), `scrobble.rs` (pure threshold machine, tested), `playlists.rs` + `radio.rs` (shared CRUD), `media.rs` (souvlaki, best-effort).

Scrobbling is server-side: `/rest/scrobble` (`submission=false` on start, `true` at ≥50%/4min); Navidrome forwards.

#### ui/ — views

`root.rs` routes login ↔ main layout (sidebar | content | player bar) and hubs child events (`cx.subscribe` on `AlbumsEvent`/`ArtistsEvent`/…).

**RootView lifecycle**
- Retains `albums_view`/`artists_view`/`recent_view` across navigation; subscribed only on first construction. `invalidate_catalog_views` drops them on disconnect and library-selection change. Selection change re-navigates only if a listing is showing (`on_catalog_page`); a detail page updates in place (`reload_content_libraries` → `ArtistDetailView::reload_libraries`). On connect they're kept and given the client via `resume_catalog_views` → `client_ready`.
- `RootView::new` opens the default page itself (`open_default_page` via `cx.defer_in`) so the cache paints before connect resolves; also wired into the session observer.
- Window geometry: `track_window_geometry` from `render`, flushed after `GEOMETRY_FLUSH` (700ms) idle.
- `apply_orientation` (top of `render`): fold sidebar on transition to portrait, restore persisted value on landscape; only transitions act; zero viewport ignored.

**search_bar.rs** — `Ctrl`/`Cmd`+`K` command palette (no search page, no inline bar).
- Cache-first: `LibraryDb::search_catalog` after `CACHE_DEBOUNCE` (60ms; server 300ms); `search3` results **merged** (`Hits::merge` appends only new rows; `adopt` fills missing covers from duplicates). Failed request keeps cached rows with error under them. Works offline.
- Local rows marked (`AlbumHit::local`, "Local" badge), open via `SearchBarEvent::OpenLocalAlbum`; local artists dropped.
- DB matching: every word in any column (`like_terms`/`like_clause`, one `%word%` LIKE per word AND-ed; user wildcards escaped).
- Ranking (pure, tested): `match_rank` tiers exact → boundary prefix → prefix → all words at word starts → all words anywhere; `score` drops secondary-field matches a band; `sort_key` adds text length then text. **`rank` only on freshly built sets**, never on-screen ones (server hits ranked before merge). `row_id` keys element ids on item id + source, not index (press/release matching).
- "Advanced search" footer is the last `PaletteItem` (keyboard-reachable); `selected_child_index` returns `None` for it.
- `Reveal` lives in SearchBar but is set in `open_palette`/`dismiss` (RootView reads it first). `dismiss` does not clear the query; `open_palette` resets.

**advanced_search.rs** — full-page search (`NavSection::Search`, palette footer; query handed over via `SearchBarEvent::OpenAdvancedSearch` → `RootView::pending_search_query`).
- `SearchKind` (`[`/`]` in vi mode), **`All` default**: artists, albums, tracks in one flat `uniform_list`; rows carry a kind badge, their own `kind` and `index` within that kind's vec (never use the row index). Empty album column still drawn for alignment. Ranking per list; summary counts per kind.
- `Filters::kinds` (pure) drops kinds a filter can't express. `SearchKind::All` never reaches `Filters::sql`; `search_advanced` splits per kind.
- **Cache-only** (`search3` has no filters). `Filters::sql` tests run against scratch SQLite. Traps: `%word%` params are positional `?1..?n` and filter `?`s come **after**; `tracks` has no `library_id` (place by album); albums lack `genre`/`suffix`/`bit_rate` (use `EXISTS` on tracks); NULL `library_id` rows are **kept**. Nullable sorts: `col IS NULL, col DESC`.
- Empty query legal with filters; neither → nothing runs. `HARD_LIMIT` 5000 (header says when hit). `SortBy::Relevance` = alphabetical from SQL then `rank` via `search_bar::sort_key`, applied to the result set.
- Dropdowns from `search_facets`; unsupported filters disabled (`SearchKind::supports_*`).
- Covers viewport-driven (`ensure_art_for_viewport`/`art_range`), synchronous hit via `artwork::cached_best` (44px thumbs — never re-download another rung). `generation` drops stale answers.

**albums.rs** — infinite-scroll grid (`uniform_list` over rows), responsive covers, persisted sort.
- **Cache-first**: `seed_from_cache` paints the last sync's rows on the first frame; `apply_live_page` overwrites from the front (count and scroll hold). Header spinner "Updating from server…". All tabs except Recent and Random; seed re-sorts with `album_cmp`. Rows with NULL `library_id` skipped under a subset.
- **Covers viewport-driven** (`ensure_art_for_viewport` from `render`, `art_range` memo). `refetch_art` only clears; size changes compared at `artwork::bucket` rung; `fetch_art` paints `cached_best` meanwhile.
- Header summary (`ui::library_summary` over `LibraryStats`): count, tracks, playtime `34d 5h`; filtered by selected libraries; hidden while cache is empty.
- One shared `Rc` of playlist ids for card menus (`sync_menu_playlists`).

**artists.rs** — card grid shaped like albums.rs (round covers, cache-first, spinner). `getArtists` buckets flattened. Covers fetched from `render` via `ensure_art_for_viewport`, not on list arrival.
- **Detail view**: bio + photo from `getArtistInfo2` via shared artwork cache; photo lightbox re-fetched at `FULL_ART_SIZE` only for server covers. Bio picked like the album About card, sharing `album_detail::{AboutSource, about_sources, ext_links, link_icon, ONLINE_WAIT}`. `clean_server_bio` (pure) strips "Read more on Last.fm" and flags truncation.
- Bio column has an explicit width (`bio_column_width` over `ui::LiveWidth`); hero row chooses beside/under itself (no `flex_wrap` — taffy mismeasures).
- Sections: Albums, Singles / EPs, **Appears on** (`LibraryDb::appears_on` minus `own_album_ids`; hidden when empty; cache-only). Library selection applied client-side (`keep_selected_libraries`; unknown albums kept). `discography_ids` spans all three for the vi cursor. `sort_discography` newest-first over `Album::release_key` (`originalReleaseDate` > `releaseDate` > `year`; missing month/day = 0).
- `Settings::artist_album_size` (`ArtistAlbumSize`; `Match` → `cover_size`). Uses `CoverSize::wrap_tile` (Medium 160px) and `wrap_art_px` (2×). Size change detected in `render`, compared at bucket rung; `refetch_art` doesn't clear.

**album_detail.rs**
- Header chips (`quality_chips`: format, bitrate range, rate/depth, channels, size; plus genre, Added), ReplayGain line (`replaygain_line`), About card with More/Less and links. `album_credits` → one link per credited artist from OpenSubsonic `artists`. Pure + tested. Vanilla servers yield fewer chips. `load_info` gated on `info.is_none()` (`load` re-runs on playback enter/leave).
- **Cache-first**: `seed_from_cache` from `album_by_id`/`tracks_by_album`, starts cover download; seeds nothing if tracks never landed.
- **Skeleton placeholders** (the cache lacks per-file fields):
  - Space reserved from frame one; grey only after `PLACEHOLDER_DELAY` (220ms). `loading_*` (in flight) vs `show_*` (`placeholding` over `*_since`) are separate; `wake_at_placeholder_delay` schedules the repaint.
  - Shaped by **sample strings in the real type styles** (`skeleton_text`, `skeleton_chip`, `skeleton_line`, `skeleton_block` with `notes_sample`/`NOTES_PREVIEW_CHARS`, all over `skeleton_pulse`), painted `transparent_black` — never px sizes. Skeleton track rows copy real padding/columns and an `invisible()` copy of hover buttons.
  - **Counts must match**: five quality chips.
  - About card in the **stacked** layout goes **below** the track list (height unknowable); in the side panel it keeps its placeholder there. Placeholder lasts while either lookup is in flight (`about_loading`), grey timed from the first request (`show_about`).
  - ReplayGain slot reserved unconditionally.
  - Skeleton track count `expected_tracks` = `cached_song_count`, else `TRACK_SKELETONS` (8).
  - gpui-component `Skeleton` not used (childless div).
  - Gates: `album_pending`/`album_loaded`, `info_pending`/`info_loaded` — both needed. Header title/credits/summary placeheld only when nothing seeded.
- About card `ONLINE_WAIT` (4s) hold; late online answers only add source pills (`about_sources`: Wikipedia → server → MusicBrainz). Links as `icons::BRAND_*` via `ext_links`, server release link wins.
- Header card has an explicit width (`header_width` over `LiveWidth`) — stretch-sized height mismeasure. Info column not `flex_1` (stacked branch adds flex props).

**Album page layout** (`Settings::album_layout`, `AlbumPageLayout`, both server and local pages): side panel = tracks + tall cover/details panel, each scrolling.
- `ui::album_side_panel` (pure) returns `None` (stacked) below `SIDE_PANEL_MIN_WINDOW_W`, squarer than `SIDE_PANEL_MIN_ASPECT` (1.1, whole-window aspect), when `SIDE_PANEL_TRACKS_MIN` would be eaten, or when the cover would be smaller than the stacked one. `ui::header_stacks_for_shape` → centred column when `HEADER_STACK_ASPECT` (1.25) taller than wide, cover via `ui::centred_header_art`.
- `SIDE_PANEL_PADDING` = 64 (panel `p_4` + card `p_4`); track column top padding `SIDE_PANEL_TRACKS_TOP`.
- Content width = `scroll.bounds() + panel_w`; **panel width floored to a whole pixel** (fractional widths oscillate).
- First frame is unmeasured (`unmeasured`): `window.request_animation_frame()` (**not** `Window::refresh`, a no-op mid-draw) and paint at `opacity(0.)` (not `hidden()`), only for the side-panel setting.
- `Settings::album_panel_right` swaps columns; hidden outside side-panel.
- `Settings::detailed_album_dates`: `ui::format_added_date(created, detailed)`, `ui::format_release_date(album, detailed)` (month and day needed, else year). No tz crate: time printed as sourced, `UTC` kept where marked. Local rows only get the Added stamp.

**player_bar.rs** — waveform seek bar, pixel-accurate click seek.
- `Settings::hide_idle_player_bar` (on): `root::player_bar_idle` (pure) = nothing playing, nothing loaded (`now_playing`, radio counts), empty queue. `ui::Reveal` (`player_bar_reveal`) shrinks a clip while the bar translates down. `player_bar_primed` + `Reveal::opened` avoid a launch slide-in. Queue panel has its own close button (`QueuePanelEvent::Close`).
- `Settings::player_bar_style` (`PlayerBarStyle`): **Docked** (default, `flex_none` row) or **Floating** card (`absolute`, centred, `FLOAT_MARGIN`, `FLOAT_MAX_W` via `float_width`, `rounded_2xl`/`shadow_xl`/`occlude`). Content scrolls under it. Smaller metrics: `FLOAT_BAR_H` 84 vs `BAR_H` 124, 56px cover, 20px waveform.
- Fill via `float_fill` (pure, tested): **darken 22% (`FLOAT_DARKEN`) and alpha `FLOAT_FILL_ALPHA` 0.92** — not the fullscreen panels' 0.72 (those sit on a dimmed backdrop; no backdrop blur in gpui). Only lightness/alpha change. Translucency opt-in: `Settings::player_bar_translucent` (off → alpha 1, darkening kept). Hidden outside Floating.
- `Settings::player_bar_tint` (on): Adaptive gradient behind the bar; hidden outside Adaptive.
- `float_bottom` (pure): travel = card height + margin. Columns sized off the card width. `player_bar::side_width` shrinks flanks together below ~1060px.
- **Queue panel follows the style**: `queue_panel.rs` reads `player_bar_style`/`player_bar_translucent`, draws the same card; `root.rs` mounts it `absolute`, travelling on `right`. Bottom via `float_panel_bottom` (pure) off the bar's openness.

**recent.rs** — rows in a `uniform_list`, each explicitly `w_full()`. Observes `PlayerState`, so gates on `signature` (length + head id + client presence) and holds a pre-formatted `Row` model. Any view observing the player needs the same.

**Settings::classic_album_cards** (off): default is the flush card — cover takes the card's inset (`ui::card_cover_edge`, pure) and squares its bottom corners (`ui::cover_square_bottom`/`cover_rounding`, applied to both well and `img`). Border stays. Card width unchanged (`grid_fit` unaware; no refetch). Text block carries its own `card_inset()` when flush. Applies to album grid, local grid, artist discography; not artist avatars. Old keys `flush_album_covers`/`square_cover_bottom` ignored.

**Settings::ui_scale** (`UiScale`, 90/100/110/125%): scales the app's **px** chrome (rems already follow `font_size` via gpui-component's `Theme::font_size`). Scaled: `ui::grid_gap`/`card_inset`/`card_padding`/`grid_padding_x`, `player_bar::bar_h`/`float_bar_h`/`float_margin`/`float_max_w`/`bar_inset`/`side_width` minimums, queue rows (docked + fullscreen), sidebar widths. Process global `ui::UI_SCALE` (`AtomicU32` of f32 bits) because pure layout helpers read it. Metrics defined at 100% and read via `ui::scaled`; **tests must never set the scale** (parallel). Card border not scaled (`card_padding()` = scaled inset + fixed 1px each side). `ui::init_ui_scale` in `main.rs` before the window; `ui::set_ui_scale` refreshes windows.

**Window resizing**: shape-dependent layout uses `window.viewport_size()`, not scroll-handle `bounds()` (previous frame). `ui::LiveWidth` remembers viewport − element gap; `ui::grid_columns` reflows same frame. Used by album and artist grids.

**Sidebar** folds to a 52px rail (`SidebarModel::collapsed`, `SidebarAction::ToggleSidebar`, Ctrl/Cmd+B, `PanelLeft*` handle as a muted icon riding the first existing row when expanded). `SIDEBAR_SECTIONS` in `sidebar.rs` mirrors rail order — keep in step; `sidebar_targets` builds the walk. Folded: icons with tooltips (incl. refresh stage). Library switcher and playlists become `Popover` dropdowns (`icons::LIBRARY`, `icons::LIST_MUSIC`) reusing `sidebar::library_row`/`playlist_row`; `AfterPick` hook (library toggles and stays open; playlist navigates and closes); dismiss via the popover entity. `Settings::sidebar_collapsed` persists. Multi-library selection = sidebar checkboxes; views merge into single sorted lists.

**fullscreen_player.rs** — full-window now-playing overlay with blurred-art background. No natural reflow, so `Layout::resolve` (pure, tested) sizes everything from window w/h:
- Stacks cover above card for aspect ≤ `STACK_MAX_ASPECT` (1.4) or too narrow; a landscape stack must give the cover `CARD_MIN + ART_LEAD` or it reverts to the row. Portrait always stacks.
- `Layout::panel_beside`: panel beside the card under the cover when width holds `CARD_MIN + GAP` + panel.
- `ART_MAX`/`ART_MAX_STACKED` are minimums of growth; `roomy_art_cap`/`stacked_art_cap` scale with `Settings::fullscreen_cover` (`FullscreenCoverSize`, share + ceiling; `Fixed` = 0). Beside the card, grows only into `free - CARD_MAX`.
- Sacrifice order: volume column → cover shrinks → with a panel in a tight window the cover drops → below `ART_MIN` the cover goes.
- `CardDensity`: `Tight` (spacing) before `Compact` (drops album + stream-info lines). Card height depends on width (`INFO_ONE_LINE_MIN`), so both branches lay out twice. Stacked uses `Tight` max. `CARD_MIN` = transport width; toggle labels drop below `TOGGLE_LABEL_MIN`; rows `flex_wrap`. Stacked: cover capped `ART_MAX_STACKED`, card at `CARD_STACKED_SHARE`, re-laid with `Tight` when lead < `ART_LEAD`.
- Scroll wrapper last resort; inner column `min_h(px(window height))` — **not a percentage** (collapses, centring lost).
- `panel_drawn` (lags `panel` by the exit) feeds `Layout::resolve`. Queue ↔ Lyrics is a swap: outgoing leaves, `replay()` at 0.
- **Queue panel**: `uniform_list` of `QUEUE_ROW_H` rows, whole-row height (`queue_visible_rows`, `panel_max_h`, `QUEUE_CHROME_H`). `sync_queue_scroll` from `render` after `Layout::resolve`; `queue_followed` → only position changes scroll; scrolls to the row `visible / 2` above with `ScrollStrategy::Top` (not `Center`).
- **Lyrics panel**:
  - Styling: `FontWeight::BLACK`; `LYRIC_REM` 1.25 → `LYRIC_REM_ACTIVE` 1.5 on the active line; `line_emphasis` (pure) symmetric `LYRIC_DIM` falloff, flat for intro/untimed. No element blur in gpui — opacity carries the falloff.
  - Handover animated both ends over `LYRIC_GROW_MS` (420): incoming grows, outgoing (`lyrics_prev`) shrinks. Ids `fs-lyric-in-{ix}` / `fs-lyric-out-{ix}` (direction in the id or the clock doesn't rewind). Easing `ease_in_out`, not `ease_out_quint`. Font size interpolated directly (no transforms on divs).
  - `lyric_wrap` (pure): line laid out at width scaled by size/`LYRIC_REM_ACTIVE` so wrapping never changes mid-animation.
  - `lyric_color` (pure) mixes `foreground`→`primary` via `Hsla::blend` (exact endpoints, keeps resting alpha).
  - Wider than the queue: `PANEL_LYRICS_MIN/MAX/SHARE`.
  - Fetch (`maybe_fetch_lyrics`) on open and song change. Server: `getLyricsBySongId`, fallback `getLyrics`; **nothing asked for local tracks** — they use `lyrics::from_file` (blocking pool): sidecar `.lrc` (strict UTF-8, BOM stripped) + `LYRICS` tag, read on demand, not at scan. `best_lyrics` (pure): synced wins, all-blank dropped.
  - `active_line` (pure): last started line, `None` before first / untimed; linear scan; document `offset` **added** (`parse_lrc` flips LRC `[offset:]` sign). Click a timed line → `lyric_seek_target` (pure, inverse of `active_line`); untimed lines inert. Click notifies the player.
  - `sync_lyrics_scroll` from `render`: offset computed so the line sits `LYRICS_LEAD` (0.35) down, clamped `[-max_offset, 0]`; lines are the scroller's direct children. `lyrics_followed` gates; cleared by panel toggle/song change; empty bounds → retry. Scroll eased over `LYRIC_GROW_MS` via `lyrics_scroll_anim` toward a target recomputed from live bounds; a new line mid-travel resets it.
  - `lyrics_following` requests animation frames only for a timed document, panel open, audio moving (or scroll travelling); uses `smooth_position`.
  - **Source badge** `LyricsSource` (`Library` / `LRCLIB`, tooltip); `lyrics_source` is `Some` iff `lyrics` is. Local files: own sidecar/tag = `Library`.
  - **Badge switches source**: `lyrics_switch_target` (pure) — only under two-source providers, never to a source known empty, never to library with nothing to ask. `lyrics_library`/`lyrics_online` cache each source's answer (`Option<Option<_>>`); unasked source costs a round trip (badge shows `…`). Lookups are free fns `library_lyrics`/`online_lyrics`.
  - `services/lyrics::fetch` = [LRCLIB](https://lrclib.net): `/api/get` (artist+title+album+duration) then `/api/search`, `best` filters empty hits first, then duration within `DURATION_TOLERANCE`, then synced; `min_by_key` on reversed key (keeps LRCLIB order on ties). 404 = `Ok(None)`. `parse_lrc` (pure): `[mm:ss]`/`.xx`/`.xxx`, multi-stamp lines, keeps `[offset:]`, drops untimed lines inside synced docs. Cache in `config::lyrics_cache_dir()` under a **hash** of artist/title/album/duration; misses cached `MISS_TTL` (7d). Only requested with the panel open.
  - `Settings::lyrics_provider` (`LyricsProvider`: `LibraryFirst` default, `OnlineFirst`, `LibraryOnly`, `OnlineOnly`; switches via `from_parts`/`with_library`/`with_online`/`with_online_first`; last source can't be switched off). Legacy `online_lyrics: Option<bool>` — only `false` migrates (→ `LibraryOnly`). `maybe_fetch_lyrics` polls only enabled sources in order. `Settings::prefer_synced_lyrics` (on; hidden outside two-source providers): `takes_over`/`settled` (pure) — untimed first hit held while the other source is asked; timed hit never displaced; off → first answer settles.

**ui/visualizer.rs** — software-3D visualizer (no shaders): world → perspective → `gpui::canvas` + `paint_path`/`paint_quad`, painter-sorted (no depth buffer). `Visualizer::tick` = one FFT/frame; fullscreen player requests animation frames only while a scene is on.
- Scenes (`Scene`): `Terrain` (rows widen with depth, `terrain_row_half`), `Tunnel`, `Sphere` (Fibonacci cloud), `Scope` (only time-domain scene: `scope_trace` triggers on rising zero crossing, samples at `SCOPE_STRIDE`; `scope` deque of drifting echoes; starved tap pushes flat traces; baseline circle; `hot_runs` relative to the trace's own peak via `SCOPE_HOT_REL`), `Bloom` (screen-space mandala; `BLOOM_LAYER_SECTORS` non-dividing; odd sectors mirror; `sin(πu)` lobes; counter-rotating layers; bass hub; beat spokes under petals), `Warp` (streaks = next travel segment; `warp_speed`; `WARP_ROLL`; `WARP_NEAR_FADE`), `Orb` (icosphere, `OrbMesh::new` once, whole wireframe one path), `Retro` (PS1 look: flat shading, back-face culling, fog, `SNAP` jitter).
- `VisualizerMode` (config.rs): Off → **Auto** → scenes (`VisualizerMode::SCENES`, iterated by the mini player's popover).
- **Auto** (`OnsetSwitcher`): spectral flux from raw bands, `Onset::strength` = `0.7·bass_flux + 0.3·flux`; crossing `max(mean + 2.2σ, 2.5×mean)` over ~4s **arms**; **commits** when the transient turns over with bass held 1.18× for 3 frames. Every threshold needs a ratio floor too. 9s hold, 45s backstop. Next scene random excluding current and previous. 220ms cross-fade.
- While a scene runs the cover/info column stands down for a **floating mini player** (thumb, title, transport, seek, `Off`/`Auto` buttons + `More` scene popover). Its slider button opens the **tuning card** (`VizKnob`): sensitivity, smoothing, power, speed, Auto switch sensitivity, hold → `Settings::visualizer_tuning`, re-read each tick. Defaults reproduce the untuned look.

**ui/mod.rs** — shared helpers and theming.
- **`ui::Reveal`**: open/close clock for anything mounted behind a flag (`with_animation` can't play exits). `set(flag, reduced_motion)` from `render`; `visible`, `openness`, `settling` (→ request frame), `replay()`, `opened`. Easing applied to travel (`reveal_openness`, pure); reversals scale span by distance. Used by queue panel, fullscreen side panel + tuning card, palette, new-playlist dialog, vi help overlay, `:` bar — each fades and moves. Modals get `active`: click/key handlers only while open.
- **Adaptive theme**: `accent_from_cover_bytes` returns `None` only on decode failure (vivid pass → relaxed → neutral from mean lightness). `RootView::maybe_update_adaptive_accent` resolves local art from disk; clears its key on failure. `Theme::change` resets `primary`, so `restore_adaptive_accent` re-applies when `theme().primary` drifts; `SettingsView::set_theme` notifies the session. Derived once at construction via `cx.defer_in`. Art key is `Option<Option<String>>` (outer `None` = retry, `Some(None)` = no cover). Coverless keeps the previous accent. Global accent follows the **playing track**.
- `Settings::adaptive_from_page`: album pages tint themselves (`ui::accent_button`, `ui::page_tint`); `adaptive_page_gradient` (off, gated). Detail views hold `accent`/`accent_for`, `refresh_accent` from `load`, cover fetch and `render`. Playing-row highlight stays `theme().primary`. (Page-local because gpui-component `Slider` has no colour override.)
- Focus styling: `with_focus_cursor(id, el, focused, glow, cx)` is the only door (`focus_glow`/`with_focus_animation` private); glow only with `Settings::selection_glow` (off).
- `ui::sync_focus_scroll`, `ui::LiveWidth`, `ui::poll_until_done`, `ui::error_banner`, `ui::library_summary`.

**ui/settings.rs**
- **Audio** card: output **dropdown** (`DropdownMenu`, groups *System output* / *Direct (bit-perfect)*; `OutputChoice`; vi Enter cycles like FontSize). Lists via `spawn_blocking_io` on open + Refresh (`pactl` can take 2s, never from `render`); a saved-but-absent device shows *(not connected)*. Status line from `PlayerState` (`output_device`/`output_direct`/`direct_error`) behind a change gate. `Settings::output_direct` overrides `output_device`; the player-bar popover only offers shared devices and picking one clears direct. ReplayGain group hidden while direct is chosen (engine plays at unity; volume sliders disabled, RG badge hidden); pre-amp + *Prevent clipping* hidden while mode is Off.
- **Connections** card: one place for every outside service, **grouped by feature** (Album descriptions, Lyrics, Scrobbling, Artist bios and photos). `service_list` of `Service` rows: mark, name, `Route` icon (server vs globe + host), optional status, switch, description dimmed when unused. Switches: `server_album_notes`, `server_artist_bios` (hide only), `album_info_*`/`artist_info_*` (gate requests; each group has a `cache_row`), lyrics provider. Server-forwarded scrobble rows have no switch; status only while unanswerable (`server_offline`: Connecting…/Unreachable/No server). `ConnectionState` for the app's own ListenBrainz path (*Needs token*); `server_scrobble_status` also requires *Scrobble plays to server*. `cache_row` counts `.json` only, off the UI thread. ListenBrainz icon: `icons::HEADPHONES`. **A new outside service goes here.**
- Dependent switch → **hidden**, not greyed (`vi_switch` returns `hidden()`, no vi action; caption via `note_if`); `switch_disabled` names dependencies. Unlabelled `vi_toggle` still disables (last lyrics source).
- Compact grid hides `note()`s (`display: none`); captions are shared consts also shown via `info_hint` (`subheading_with_notes`, `section_with_notes`). Truncated `detail` lines get tooltips.
- Layout: scrolling column with quick-nav pills, or centred asymmetric grid. `compact_grid` (pure, tested) plans and decides (empty = column). `compact_columns`: ≥2 columns fit width (`COMPACT_COL_MIN` 380, ≤ `COMPACT_COL_MAX`, ≤ one per section) and tallest fits height; pick closest to square by `|ln(w/h)|`. `split_runs` keeps document order (contiguous runs, binary-searched height cap, then the split under that cap whose shortest run is tallest — no lone card left in the last column). Columns `COMPACT_COL_TARGET` 460, proportional width nudge within floor. Heights from `COMPACT_ROW_H`/`COMPACT_CARD_H` over `COMPACT_SECTIONS` weights — **hand-kept, indexed by document order, debug-asserted in `section()`**. Pills and `scroll_to_section`/`current_section` disabled in grid.
- **About** card last, under Account (Account may be absent: `present_sections` filters it, `section` asserts against `section_titles`); has a Copy button (`version_line()`) because `section_starts` needs each section to own a control.
- First frame unmeasured: `request_animation_frame()` + `opacity(0.)` body.

**errors.rs** — the one place failures become text. `error_text` prefers `subsonic::Error::user_message()` (keeping anyhow context), scrubs other text via `playback::scrub_urls`. `ErrorNote { text, retryable }` → `ui::error_banner` with Retry when retryable. `playback_error` names the track and says why; a `MAX_FAILED_STREAK` run says the queue stopped.

**config.rs** — settings TOML via `directories::ProjectDirs`; password in OS keyring (`keyring`), plaintext fallback only when keyring fails. Cache dirs, UI prefs (`default_page`, `album_sort`, `library_ids` — legacy `library_id` migrated), `local_music_dirs`. `Settings::window` (`WindowGeometry`): `main.rs::startup_bounds` restores only if `usable_on` (pure) finds `MIN_VISIBLE_W`×`MIN_VISIBLE_H` (180×80) on an attached display; empty display list keeps it. Fullscreen not persisted. Wayland: size + maximized only.

#### Vi-mode keyboard navigation

When `settings.vi_mode` is on, `root.rs` routes keys to `handle_vi_key` and legacy shortcuts (space/arrows/`[`/`]`) are dead.
- Cursor protocol per content view: `vi_cursor: Option<usize>`, `vi_move(delta, window, cx)`, `vi_activate(cx)`, `vi_clear(cx)`. Dispatch: `content_vi_move` / `content_vi_activate` / `content_vi_tab` — **add every new `Content` variant to all three plus `vi_clear_cursors`**.
- Scroll-into-view: `anchor_scroll(Some(anchor))` + `ScrollAnchor::for_handle(handle)` at call sites, triggered from **`render`, never `vi_move`** (anchors are last paint's), via `ui::sync_focus_scroll` with `vi_scroll_synced`; its `window.refresh()` is load-bearing. Albums grid exception: `UniformListScrollHandle::scroll_to_item(row, ScrollStrategy::Top)` in `vi_move` (only Top/Center/Bottom exist).
- `[`/`]` cycle album tabs (`vi_tab` → `select_tab`, persists `album_sort`); no-vi mode: nav back/forward.
- `:` commands in `execute_command` (`:newpl <name>`, `:pl add <name>`, `:pl list`, `:q`, `:help`); `set_vi_status` auto-clears after 4s.
- **Text fields win**: with `RootView::vi_typing`, `handle_vi_normal`/`handle_vi_insert` return **without `cx.stop_propagation()`** (only Escape handled). Normal mode asks each field (focus handles are unusable — buttons take focus): root inputs, `SearchBar::is_typing`, `PlayerBar::is_typing`, `content_typing` (Settings, Radio, Playlist). **Every new text field needs an `is_typing` and a line in one of those.**
- Folded sidebar: `rail_playlists_open` drives the playlists `Popover` (`.open(..)` + `.on_open_change(..)`); `sync_rail_playlists` opens it while the cursor is in the playlist range; closed on activate, unfold, or leaving vi mode.

### Async pattern (used everywhere in the app crate)

```rust
cx.spawn(async move |this, cx| {
    let data = runtime::spawn_io(async move { client.some_call().await.map_err(anyhow::Error::from) }).await;
    let _ = this.update(cx, |view, cx| { /* apply */ cx.notify(); });
}).detach();
```

Never block gpui's executor with IO; never share locks with UI state — entities are mutated only via `update`.

### Recurring gpui traps

- A measurement landing dirties nothing: request the next frame from `render` with `window.request_animation_frame()`; `Window::refresh` is a no-op mid-draw.
- Scroll-handle / child bounds are the previous paint's — compute scrolls in `render`.
- Stretch-sized (width-auto) flex children measure height at a narrower width; wrapping content inflates it — give them a px width.
- Self-measured widths must be integral or layout oscillates.
- gpui keys animation state on the whole element-id path; drive transitions from entity state, not wrapper ids.

## Pinning & toolchain notes

- **gpui `0.2.2` + gpui-component `0.5.1` are a matched crates.io pair.** Don't bump one without the other; don't switch to git deps casually. gpui types stay in `crates/app`. gpui-component is vendored under `vendor/` via `[patch.crates-io]`. **gpui is vendored too** (`vendor/gpui`, 0.2.2 minus examples + one patch): `LineWrapper::clings_to_previous` stops closing punctuation (`)` `;` `?` `…` `"` …) from being a break point, which upstream made of every non-word char — captions wrapped as `mode` / `).`. Test `test_closing_punctuation_stays_on_its_word` (run from a copy with an empty `[workspace]`; the vendored crate is not a member). Second patch: `taffy.rs` `to_grid_repeat` spells `length(0.0_f32), fr(1.0_f32)` — newer rustc's `float_literal_f32_fallback` lint (future hard error) fired on the bare literals. **Re-apply both when bumping gpui.**
- gpui `runtime_shaders` feature required (no `xcrun metal` with CLT only). Don't remove.
- `[profile.dev.package.{gpui,gpui-macros,gpui-component}] opt-level = 2` — only those.
- rodio built with `symphonia-all` (defaults miss ALAC/aiff/mkv/caf/adpcm; a missing codec reports as "The format of the data has not been recognized"). Covered by `plays_alac_m4a_stream`. Real m4a failures are usually the container over HTTP (trailing `moov` + unseekable) — `m4a_without_content_length_reports_the_container`, `library_tracks_do_not_request_icy_metadata` (raw `TcpListener` servers; wiremock always sets `Content-Length`).
- **stream-download is a local fork** (`vendor/stream-download`, 0.24.4 + one patch): `Source::notify_if_position_reached` also called from the **prefetch** write path, so a seek to a trailing index wakes the reader without the whole download (12.1s → 0.85s on a 4.5MB AAC; `DECODE_TIMEOUT` is 12s). Test: `a_trailing_index_is_read_without_the_whole_download`. **Re-derive the ~20-line patch in `src/source/mod.rs` when bumping.**
- Edition 2024; let-chains used.
- `souvlaki` needs `dbus` on Linux; best-effort, degrades to no-op.

## Testing conventions

- `subsonic`: wiremock tests in `tests/client_test.rs` (auth params, envelope parsing from realistic Navidrome JSON, error codes). **Add one for every new endpoint.**
- `playback`: `tests/engine_test.rs` streams an in-memory WAV via wiremock and asserts events; local playback via `plays_local_wav_to_completion`, `local_file_missing_errors`. `slow_range_server` (trickles body, stalls ranged requests; takes a content type) for `a_slow_seek_does_not_stop_the_engine_answering` and the trailing-index test. Audio tests **self-skip when no output opens**, checked up front via `no_audio_output()` over `playback::output_available()` — never by matching error text.
- Pure layout/logic helpers are unit-tested; never set `UI_SCALE` in tests.
- Manual smoke: demo.navidrome.org (`demo`/`demo`) or `docker run deluan/navidrome`. Navidrome folder browsing is simulated — use ID3 endpoints (`getArtists`/`getAlbum*`) only, never `getIndexes`/`getMusicDirectory`.
