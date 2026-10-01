# Changelog

All notable changes to Scirè are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/).

## [1.3.2] — 2026-10-01

### Changed

- The artist page's Lidarr switch carries the count of missing releases —
  **Show missing releases (3)** — once Lidarr has answered, and the
  "Missing releases" heading and the caption under the switch are gone.
- **Open in Lidarr** moves to the artist's name in the header card, and shows
  whether or not missing releases are.

### Fixed

- The album page's About card no longer draws its More button over the last
  line of the description.
- The darkened hover over the player bar's cover follows the cover's rounded
  corners.

## [1.3.1] — 2026-10-01

### Fixed

- The player bar, fullscreen player, recent list and search show an album's
  own cover when it is sharper than the picture embedded in the playing
  track — a small thumbnail tagged into every file no longer stands in for
  the full-size `cover.jpg` beside them. Song covers cached by earlier
  versions are dropped once on launch and fetched again.

## [1.3.0] — 2026-10-01

### Added

- The artist page files releases under their own sections: **Live albums**,
  **Compilations**, **Remixes**, **DJ mixes**, **Mixtapes**, **Soundtracks**,
  **Demos** and **Other releases** (spoken word, interviews, broadcasts), next
  to Albums and Singles / EPs. The server's release types decide it, from the
  files' `RELEASETYPE` tags; an untagged record is filed as live or a DJ mix
  when its title says so — "(Live)", "Live at …", "(DJ Mix)", "(Mixed by …)".
  A section only appears when the artist has something in it. Release types
  are kept in the library cache, so sections are right from the first frame
  after the next sync.
- **Lidarr integration** (Settings → Connections → Downloads; URL and API key,
  the key kept in the system keyring). Artist pages get a *Show missing
  releases* switch that adds the releases Lidarr knows of and the library lacks
  to the page's own sections (Albums, Singles / EPs, Live, … by Lidarr's
  release types, in date order among the library's own), dimmed and
  dash-bordered, with what each is doing
  (searching, downloading, not monitored). A missing
  release opens its own page: cover, track list, monitored switch, **Search
  automatically** (Lidarr's album search) and **Interactive search** (every
  indexer's results with size, quality, seeders, age and Lidarr's rejections;
  pick one to send it to the download client). A **Lidarr** sidebar entry
  appears while the integration is on, with a badge counting the queue: its
  page shows downloads with progress and ETA (removable), the monitored albums
  Lidarr still wants, and recent activity. When Lidarr finishes importing,
  the app asks the server to scan (admins) and refreshes the library on its
  own; an album or artist page showing the import reloads. New music can be found
  and added from search: the palette (Ctrl/Cmd+K) has a **Search Lidarr**
  row under *Advanced search*, and the search page a **Lidarr** tab (Enter or
  the button asks). Artists and albums from Lidarr's metadata search open an
  add page (root folder, quality and metadata profiles, what to monitor,
  search on add) when Lidarr lacks them. A **Lidarr artist page** (from a
  hit, a Lidarr album's artist line, or after adding) shows the discography
  in sections by type with each release's state, its own monitored switch and
  search button, an *Only missing* filter, a summary of what's on disk, and
  the artist's monitored switch and *Search monitored*. Albums downloading
  show up first on the album grid's *All* and *New* tabs as cards that fill
  left to right in the cover's own colour as the download progresses (the
  cover lighting up as the fill crosses it), then wait as *Waiting for library*
  until the server lists them and the normal card takes over.

## [1.2.3] — 2026-09-29

### Fixed

- An artist's photo and the full-size image that opens when it is clicked
  no longer show two different pictures. The cached photo is rechecked
  against the server once per session when the artist page opens, and the
  full-size view follows whichever photo the page settled on.

### Changed

- The artist page shows loading placeholders like the album page: the
  name, album cards, genres, biography and photo hold their place and
  pulse while a slow server answers, and album covers pulse while they
  download.

## [1.2.2] — 2026-09-29

### Fixed

- The fullscreen player's queue panel is centred beside the card again
  instead of sitting at the top of the window when the queue is short.

## [1.2.1] — 2026-09-29

### Fixed

- An artist page no longer waits on a slow server to show anything but
  *Appears on*: the name, photo and discography paint from the last sync at
  once and the server's answer replaces them, and the bio is asked for
  alongside the discography instead of after it.

## [1.2.0] — 2026-09-27

### Added

- **Timeline** tab on the Albums page: the library laid out like a phone's
  photo gallery. Albums newest-added first, grouped under date headings with
  their counts, and a year column down the right edge that jumps straight to a
  year. Drawn as cards or gallery tiles, following the Album cards setting.
- **Settings → Browsing → Timeline groups**: head the Timeline by day, week
  (Monday to Sunday), month or year.
- **Settings → Browsing → Album cards → Gallery**: draw every album grid — the
  Albums and Local pages, the Timeline and artist discographies — as
  cover-only tiles packed tight, with title, artist and a play button showing
  on hover. Cards stay the default.
- **Song details**: an info button on album track rows (and *Song details* in
  their right-click menu) opens every field the server has on the track —
  credits by role, composer, audio format, ReplayGain, path, dates, ISRC,
  MusicBrainz id — with a *Copy* button. Local tracks list every tag in the
  file as well.
- An artist's *Appears on* section includes covers: albums by other artists
  holding a song this artist wrote (the composer credit). The first sync after
  updating re-reads every album's tracks to pick the credits up.
- The library size sits beside the album, track and playtime totals at the
  top right of the Albums and Local Music pages.
- Long instrumental breaks in synced lyrics show three dots, the way Apple
  Music does: they light one by one as the break runs, then fade just before
  the singing comes back. Covers the intro, gaps the lyrics mark with a blank
  line, and long gaps between two sung lines. The dots
  grow and shrink smoothly about their own centres.

### Changed

- More room between the *Albums* caption and the sort pills.
- Every Settings card is laid out like Connections: each group's controls in
  one boxed list, name on the left and control on the right.

### Fixed

- The fullscreen player no longer scrolls with the mouse wheel at some window
  shapes; the page only scrolls in a window too short for its controls.
- The library totals at the top right of the Albums, Artists and Local Music
  pages sit as far from the right edge as the page title does from the left.
- Bluetooth earbuds dropping mid-track pause playback until they come back,
  instead of carrying on through the speakers after any earlier press of play.
- A device reconnecting no longer restarts music you paused yourself.
- Resuming the last session starts the track at the saved position straight
  away: no more hearing its opening seconds first, and the saved position is
  no longer lost when the app quits during that first seek. Switching output
  device or following a route change resumes the same way.

## [1.1.2] — 2026-09-27

### Added

- In the fullscreen player the artist and album names are links: a click
  closes the fullscreen view and opens that page. The visualizer's mini
  player links its artist too.

### Changed

- With a direct (bit-perfect) output the volume control is replaced by a
  *Direct · bit-perfect* indicator, instead of a greyed-out slider; its tooltip
  says to set the level on the device.

### Fixed

- Long output device names end in an ellipsis (full name in a tooltip) instead
  of running out of the player bar and the Settings dropdown; they wrap in the
  player bar's device list.
- Clicking the album or artist of a local track in the player bar opens its
  local page instead of an empty server one.
- With the lyrics panel open, the next song's lyrics start at the top instead
  of showing from their last lines.

## [1.1.1] — 2026-09-26

### Fixed

- A long artist bio no longer runs out of the header card and over the
  discography: the card grows with its contents in every window shape.
- The lyrics source badge no longer claims your library has no lyrics while
  LRCLIB's are showing and your library does have some.
- The lyrics panel keeps its size while lyrics load, with the loading
  indicator in its centre, instead of collapsing to a strip and springing back.

### Changed

- A long artist bio shows a preview on the page; *Read more* opens the whole
  text in a popup (close button, a click outside, or Escape) instead of
  stretching the header down the page.
- The audio output status in Settings is a coloured indicator: bit-perfect,
  resampled, direct unavailable, or system output, with the details beside it.
- Settings descriptions reworded without long dashes.

## [1.1.0] — 2026-09-26

### Added

- The album page's About card looks the album up on MusicBrainz and Wikipedia
  (Settings → Connections → Album descriptions, each on by default and
  switchable on its own): the full Wikipedia intro, the MusicBrainz annotation,
  and links to Wikipedia, MusicBrainz, Discogs, AllMusic and Bandcamp. When more
  than one description is on offer, pills in the card's header switch between
  them.
- The card's links are site icons with tooltips instead of text.
- The artist page's bio does the same (Settings → Connections → Artist bios,
  Wikipedia and MusicBrainz each on by default and switchable on their own):
  the full Wikipedia intro, the MusicBrainz annotation, pills to switch between
  them and Last.fm's bio, and icon links to Wikipedia, MusicBrainz, Last.fm,
  Discogs, AllMusic, Bandcamp and the artist's official site. An artist is
  found by the MusicBrainz id in your tags, else through one of their albums,
  so two artists sharing a name are not mixed up. A cut Last.fm bio is marked
  with "…" and ends in a *Read more on Last.fm* link, rather than trailing the
  link's words after the cut. Its answers have their own Clear cache button.
- Settings → Connections gathers every outside service the app relies on,
  grouped by what it is for — album descriptions, lyrics, scrobbling, artist
  bios — with each feature listing its services: whether your server reaches
  them or Scirè contacts them itself (and which host), a switch for each one the
  app decides about — Wikipedia, MusicBrainz, LRCLIB, the library's own lyrics,
  ListenBrainz for local files, and whether to show the Last.fm notes and bios
  the server forwards — and a Clear cache button where answers are kept on disk.
  The lyrics *Source* menu became two switches and *Ask LRCLIB first*. The
  switches that used to live under Album pages, Library → Lyrics and Playback →
  ListenBrainz have moved there.
- Connections also says whether your Navidrome server forwards your plays to
  ListenBrainz and Last.fm (linked, not linked, or not enabled on the server),
  read from Navidrome's own API. The check sends your password to the
  server's login, so it is skipped over plain HTTP unless the server is on
  your machine or local network.
- Settings → About links to the GitHub repository, its releases, the
  changelog and the issue tracker, and names the license.
- Settings → Audio: the output device is a dropdown, listing the system's
  devices and, on Linux, the sound cards themselves under *Direct
  (bit-perfect)*. A direct card is opened on its own, bypassing
  PipeWire/PulseAudio, at each track's own sample rate, so 16- and 24-bit files
  reach the DAC unchanged. Volume and ReplayGain are off while it is in use (set
  the level on the DAC or amp), gapless holds only between tracks of the same
  rate, and nothing else can play through that card meanwhile — if it is busy,
  the system output is used and the page says why. The player bar's device line
  shows the format the card runs at.
- ReplayGain gains a pre-amp (−6 to +6 dB) and a *Prevent clipping* switch
  (on by default, as before).

### Changed

- ReplayGain moved from Playback to the new Audio section, next to the output
  device.
- Settings that only apply once another one is on are hidden until it is,
  instead of shown greyed out — the minimal title bar, the album-colour glow,
  the album panel side, the album page tint and wash, the floating bar's
  see-through switch, the cover tint and the lyrics order switches. The
  ListenBrainz token field sits inside the ListenBrainz row and appears when
  it is switched on.
- In the wide settings grid, which has no room for the Connections page's
  explanations, they are behind an info icon beside each heading.
- Captions that only restated their setting are gone: the in-app title bar,
  Reduce motion, the local music folders and About.

### Fixed

- Text no longer wraps a closing bracket, semicolon, question mark or ellipsis
  onto a line of its own — a settings caption could end with `).` alone on its
  last line.
- Album art replaced on the server now shows up in Scirè. A library refresh
  picks up albums whose cover changed and swaps the cached pictures in place,
  and Settings → Library → Rebuild local cache rechecks every cached cover.
  Before, the old art stayed until it was evicted from the cache.
- Two albums by one artist sharing a title (a self-titled debut and a
  self-titled follow-up) no longer both get the same album's description: the
  lookup prefers the one first released in the album's year. Clear the album
  descriptions cache to re-ask for one already looked up.
- A long artist bio no longer runs out through the bottom of the header card,
  in a window wide enough to set it beside the photo or narrow enough to put it
  under.
- A saved server that does not answer at startup is retried (after 2s, 5s,
  15s, 30s, then every minute) instead of leaving the session offline until a
  restart while the cached library made everything look fine.
- The About card no longer ends mid-sentence after More. The text Navidrome
  forwards is Last.fm's summary, cut at a fixed length with its "Read more" link
  stripped; a cut-off summary is now marked with "…" and ends in a *Read more on
  Last.fm* link to the full text, and the Wikipedia intro — complete, and split
  into paragraphs — is shown in its place where there is one.

## [1.0.2] — 2026-09-24

### Fixed

- Portrait windows: the album page header (server and local) becomes one
  centred column with a bigger cover once the window is clearly taller than
  wide, instead of a stack hugging the left edge. The side-panel layout now
  takes any landscape window a little wider than square rather than only
  widescreen ones, so the header-over-tracks layout is left to near-square
  windows.
- Track titles keep a share of the row in narrow windows instead of being
  squeezed to nothing by the artist column, and the grid headers wrap their
  summary under the tabs rather than pushing it off the edge. A narrow pane
  keeps two grid columns at a slightly smaller tile.

## [1.0.1] — 2026-09-24

### Fixed

- The artist page's Albums / Singles / EPs split follows the release type the
  server publishes (OpenSubsonic `releaseTypes`, from the files' tags). Untagged
  releases fall back to the title and length, and that guess is tighter now:
  "EP" or "Single" must be the title's last word ("Deep" and "Sleep" were filed
  as EPs), and a release counts as short at six tracks or fewer in under 30
  minutes rather than four tracks or fewer at any length.
- A year below 1000 is treated as missing. A date written day-first into a
  year-first tag (`0003-09-2026`) used to show as "3" under the album's cover
  and sort as the oldest record in the library.
- The Recent page draws covers already in the cache, including art the player
  bar or the album grid stored at another size, and offline. Local tracks'
  covers are read from disk instead of being asked of the server.

## [1.0.0] — 2026-09-24

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

[Unreleased]: https://github.com/MatPrayer/scire/compare/v1.3.1...HEAD
[1.3.1]: https://github.com/MatPrayer/scire/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/MatPrayer/scire/compare/v1.2.3...v1.3.0
[1.2.3]: https://github.com/MatPrayer/scire/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/MatPrayer/scire/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/MatPrayer/scire/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/MatPrayer/scire/compare/v1.1.2...v1.2.0
[1.1.2]: https://github.com/MatPrayer/scire/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/MatPrayer/scire/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/MatPrayer/scire/compare/d6706f2...v1.1.0
[1.0.2]: https://github.com/MatPrayer/scire/compare/a7c8c36...d6706f2
[1.0.1]: https://github.com/MatPrayer/scire/compare/v1.0.0...a7c8c36
[1.0.0]: https://github.com/MatPrayer/scire/releases/tag/v1.0.0
