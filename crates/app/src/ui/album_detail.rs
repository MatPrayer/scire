//! Album page: header (artwork, star, rating) + track list with play,
//! queue and playlist actions.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, Window, div,
    img, linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::link::Link;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::{Album, AlbumInfo2, AlbumWithSongs, Song, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::ThemePref;
use crate::services::library_db::LibraryDb;
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::Session;
use crate::ui::{
    focus_glow, format_duration, strip_html, track_extras, truncate_at_word, with_focus_animation,
};

/// Resolution to request for the header cover.
///
/// The header draws it at 220 logical px, so 512 covers a 2× display with room
/// to spare — 600 was asking for detail no screen here can show, and it snapped
/// up to a cache rung of its own that nothing else in the app used. 512 is the
/// rung the album and artist grids already land on at the larger cover-size
/// settings, so those settings now open a detail page straight off the grid's
/// own download. The full-resolution copy behind the lightbox is unaffected.
const ART_SIZE: u32 = 512;

/// Collapsed album-notes length, matching the artist page's bio preview.
const NOTES_PREVIEW_CHARS: usize = 400;

pub enum AlbumDetailEvent {
    OpenArtist(String),
}

pub struct AlbumDetailView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    album_id: String,
    album: Option<AlbumWithSongs>,
    /// getAlbumInfo2 payload: description + external ids. Fetched once.
    info: Option<AlbumInfo2>,
    /// Album description expanded past its preview length.
    notes_expanded: bool,
    art_path: Option<PathBuf>,
    error: Option<String>,
    /// Last observed playing-song id; used to refresh play counts when a track
    /// from this album finishes (its scrobble updates the server count).
    last_playing_id: Option<String>,
    /// Full-resolution cover lightbox open.
    show_full_art: bool,
    /// High-res cover for the lightbox (fetched lazily on first open).
    full_art_path: Option<PathBuf>,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Track index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Accent extracted from *this album's* cover, for the page's own tint
    /// under `Settings::adaptive_from_page`. The app's chrome keeps the playing
    /// track's accent; only this page carries the album's.
    accent: Option<gpui::Hsla>,
    /// Cover the accent was extracted from, so a repaint doesn't re-decode it.
    accent_for: Option<PathBuf>,
}

impl EventEmitter<AlbumDetailEvent> for AlbumDetailView {}

impl AlbumDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        db: Arc<LibraryDb>,
        album_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        // Highlight the playing track as it changes; refresh play counts when
        // playback moves off a track belonging to this album.
        cx.observe(&player.clone(), |this: &mut Self, player, cx| {
            let cur = player.read(cx).current_song().map(|s| s.id.clone());
            if cur != this.last_playing_id {
                let prev = this.last_playing_id.take();
                this.last_playing_id = cur.clone();
                let in_album = |id: &Option<String>| {
                    id.as_ref().is_some_and(|id| {
                        this.album
                            .as_ref()
                            .is_some_and(|a| a.song.iter().any(|s| &s.id == id))
                    })
                };
                if in_album(&prev) || in_album(&cur) {
                    this.load(cx);
                }
            }
            cx.notify();
        })
        .detach();
        let last_playing_id = player.read(cx).current_song().map(|s| s.id.clone());
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            player,
            playlists,
            album_id,
            album: None,
            info: None,
            notes_expanded: false,
            art_path: None,
            error: None,
            last_playing_id,
            show_full_art: false,
            full_art_path: None,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            accent: None,
            accent_for: None,
        };
        this.seed_from_cache(&db, cx);
        this.load(cx);
        this
    }

    /// Paint the header and track list from the last sync's rows before the
    /// server answers.
    ///
    /// `getAlbum` is a full round trip, and the cover cannot even start
    /// downloading until it lands — the cover id lives in that response. On a
    /// remote server that is most of a second of empty page. The cache holds
    /// everything the page needs except the per-file quality fields, so it is
    /// drawn immediately and `load` overwrites it in place.
    fn seed_from_cache(&mut self, db: &LibraryDb, cx: &mut Context<Self>) {
        let Ok(Some(row)) = db.album_by_id("navidrome", &self.album_id) else {
            return;
        };
        let songs: Vec<Song> = db
            .tracks_by_album(&self.album_id)
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.into_song())
            .collect();
        // An album row whose tracks never landed (a sync interrupted between
        // phases) would seed an empty track list, which reads as a broken
        // page rather than a loading one.
        if songs.is_empty() {
            return;
        }
        let cover = row.cover_art.clone();
        self.album = Some(AlbumWithSongs {
            album: Album {
                id: row.id,
                name: row.title,
                artist: row.artist,
                artist_id: row.artist_id,
                cover_art: row.cover_art,
                song_count: Some(songs.len() as u32),
                duration: Some(row.duration as u32),
                created: row.created,
                year: row.year,
                genre: None,
                starred: row.starred,
                user_rating: None,
                play_count: row.play_count.map(|c| c as u64),
            },
            song: songs,
        });
        if let Some(cover) = cover {
            // Straight off disk when the grid already downloaded this cover,
            // so the header art is there on the first frame — at whatever size
            // was cached, since the grid's rung depends on the cover-size
            // setting and only matches this one at the larger settings. The
            // `fetch_art` below replaces it with the requested size when that
            // lands; drawing the grid's thumbnail scaled up in the meantime is
            // what stops the header opening empty.
            self.art_path = artwork::cached_best(&cover, ART_SIZE);
            self.refresh_accent(cx);
            self.fetch_art(cover, cx);
        }
    }

    /// Open the full-resolution cover lightbox, fetching a large version once.
    fn open_full_art(&mut self, cx: &mut Context<Self>) {
        self.show_full_art = true;
        if self.full_art_path.is_none()
            && let Some(client) = self.client(cx)
            && let Some(cover) = self.album.as_ref().and_then(|a| a.album.cover_art.clone())
        {
            cx.spawn(async move |this, cx| {
                if let Ok(path) = artwork::fetch(client, cover, 1500).await {
                    let _ = this.update(cx, |view, cx| {
                        view.full_art_path = Some(path);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_album(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(album) => {
                        // The seed already started this download when the
                        // cover id matches, which it does unless the album's
                        // art changed on the server since the last sync.
                        let seeded_cover =
                            view.album.as_ref().and_then(|a| a.album.cover_art.clone());
                        if let Some(cover) = album.album.cover_art.clone()
                            && Some(&cover) != seeded_cover.as_ref()
                        {
                            view.fetch_art(cover, cx);
                        }
                        view.album = Some(album);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        self.load_info(cx);
    }

    /// Album description + external ids. `load` re-runs whenever playback moves
    /// in or out of this album (to refresh play counts), so this is gated on the
    /// info being absent — the notes never change under us.
    fn load_info(&mut self, cx: &mut Context<Self>) {
        if self.info.is_some() {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album_info2(&id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            // A server without the metadata agent answers with an empty element
            // rather than an error; either way there is simply nothing to show.
            if let Ok(info) = result {
                let _ = this.update(cx, |view, cx| {
                    view.info = Some(info);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn fetch_art(&self, cover_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_path = Some(path);
                    view.refresh_accent(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The accent this page paints itself with, or `None` when it should use
    /// the theme's (every theme but Adaptive, the setting off, or the cover not
    /// decoded yet).
    fn page_accent(&self, cx: &Context<Self>) -> Option<gpui::Hsla> {
        let settings = &self.session.read(cx).settings;
        if settings.theme != ThemePref::Adaptive || !settings.adaptive_from_page {
            return None;
        }
        self.accent
    }

    /// The album's colour for the header wash, or `None` when the page carries
    /// no accent or the gradient is switched off (it is by default — the
    /// accented controls are the quiet half of the feature).
    fn header_tint(&self, cx: &Context<Self>) -> Option<gpui::Hsla> {
        self.page_accent(cx)
            .filter(|_| self.session.read(cx).settings.adaptive_page_gradient)
    }

    /// Extract the page's accent from the cover now on disk. Keyed on the path,
    /// so the repeated `load`s this page does (playback entering or leaving the
    /// album) don't re-decode the same image.
    fn refresh_accent(&mut self, cx: &mut Context<Self>) {
        let settings = &self.session.read(cx).settings;
        if settings.theme != ThemePref::Adaptive || !settings.adaptive_from_page {
            return;
        }
        let Some(path) = self.art_path.clone() else {
            return;
        };
        if self.accent_for.as_ref() == Some(&path) {
            return;
        }
        self.accent_for = Some(path.clone());
        cx.spawn(async move |this, cx| {
            let accent = runtime::spawn_blocking_io(move || {
                let bytes = std::fs::read(&path)?;
                crate::ui::accent_from_cover_bytes(&bytes)
                    .ok_or_else(|| anyhow::anyhow!("cover art did not decode"))
            })
            .await;
            let _ = this.update(cx, |view, cx| match accent {
                Ok(accent) => {
                    view.accent = Some(accent);
                    cx.notify();
                }
                // Undecodable cover: forget the key so a later repaint retries
                // rather than pinning the page to no accent at all.
                Err(_) => view.accent_for = None,
            });
        })
        .detach();
    }

    fn play_from(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(album) = &self.album else { return };
        let songs = album.song.clone();
        self.player.update(cx, |player, cx| {
            player.play_queue(songs, index, cx);
        });
    }

    fn play_shuffled(&mut self, cx: &mut Context<Self>) {
        let Some(album) = &self.album else { return };
        let songs = album.song.clone();
        self.player.update(cx, |player, cx| {
            player.play_queue_shuffled(songs, cx);
        });
    }

    /// Toggle star on the album (optimistic local update).
    fn toggle_album_star(&mut self, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = album.album.id.clone();
        let starred = album.album.starred.is_some();
        album.album.starred = if starred { None } else { Some(String::new()) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if starred {
                    client.unstar("albumId", &id).await
                } else {
                    client.star("albumId", &id).await
                }
                .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Toggle star on one track (optimistic local update).
    fn toggle_song_star(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(song) = album.song.get_mut(index) else {
            return;
        };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = song.id.clone();
        let starred = song.starred.is_some();
        song.starred = if starred { None } else { Some(String::new()) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if starred {
                    client.unstar("id", &id).await
                } else {
                    client.star("id", &id).await
                }
                .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Rate the album 1-5; clicking the current rating clears it.
    fn rate_album(&mut self, rating: u8, cx: &mut Context<Self>) {
        let Some(album) = &mut self.album else { return };
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let id = album.album.id.clone();
        let new = if album.album.user_rating == Some(rating) {
            0
        } else {
            rating
        };
        album.album.user_rating = if new == 0 { None } else { Some(new) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .set_rating(&id, new)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            if let Err(e) = result {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(format!("{e:#}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Per-track "add to playlist" popover.
    fn playlist_popover(&self, index: usize, song: &Song, _cx: &Context<Self>) -> impl IntoElement {
        let playlists = self.playlists.clone();
        let song_id = song.id.clone();
        Popover::new(("addpl", index))
            .trigger(
                Button::new(("addpl-btn", index))
                    .ghost()
                    .xsmall()
                    .icon(app_icon(icons::LIST_PLUS)),
            )
            .content(move |state, _window, cx| {
                let entries: Vec<(String, String)> = playlists
                    .read(cx)
                    .playlists
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect();
                let playlists = playlists.clone();
                let song_id = song_id.clone();
                let mut menu = v_flex().gap_0p5().min_w(px(180.));
                if entries.is_empty() {
                    menu = menu.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No playlists yet"),
                    );
                }
                for (i, (pl_id, pl_name)) in entries.into_iter().enumerate() {
                    let playlists = playlists.clone();
                    let song_id = song_id.clone();
                    menu = menu.child(
                        div()
                            .id(("pl-opt", i))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |state, _, window, cx| {
                                playlists.update(cx, |p, cx| {
                                    p.add_song(pl_id.clone(), song_id.clone(), cx);
                                });
                                state.dismiss(window, cx);
                            }))
                            .child(pl_name),
                    );
                }
                let _ = state;
                menu
            })
    }
}

/// Technical summary of the album's files, as short chip strings: formats,
/// bitrate, sample rate / bit depth, channels, total size. Everything here is
/// OpenSubsonic-only except the bitrate, so vanilla servers yield fewer chips
/// and no empty placeholders.
fn quality_chips(songs: &[Song]) -> Vec<String> {
    let mut chips = Vec::new();

    let mut formats: Vec<String> = Vec::new();
    for song in songs {
        // `suffix` is the file extension; fall back to the MIME subtype, which
        // is what servers that omit it still give us ("audio/flac" → FLAC).
        let raw = song.suffix.as_deref().or_else(|| {
            song.content_type
                .as_deref()
                .and_then(|c| c.rsplit('/').next())
        });
        if let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) {
            let fmt = raw.to_uppercase();
            if !formats.contains(&fmt) {
                formats.push(fmt);
            }
        }
    }
    if !formats.is_empty() {
        chips.push(formats.join(" / "));
    }

    // Ranges, not averages: a mixed-source album should say so.
    let bitrates: Vec<u32> = songs
        .iter()
        .filter_map(|s| s.bit_rate)
        .filter(|&b| b > 0)
        .collect();
    if let (Some(&lo), Some(&hi)) = (bitrates.iter().min(), bitrates.iter().max()) {
        chips.push(if lo == hi {
            format!("{lo} kbps")
        } else {
            format!("{lo}–{hi} kbps")
        });
    }

    let rate = songs
        .iter()
        .filter_map(|s| s.sampling_rate)
        .filter(|&r| r > 0)
        .max();
    let depth = songs
        .iter()
        .filter_map(|s| s.bit_depth)
        .filter(|&d| d > 0)
        .max();
    match (rate, depth) {
        (Some(r), Some(d)) => chips.push(format!("{} · {d} bit", fmt_khz(r))),
        (Some(r), None) => chips.push(fmt_khz(r)),
        (None, Some(d)) => chips.push(format!("{d} bit")),
        (None, None) => {}
    }

    if let Some(ch) = songs
        .iter()
        .filter_map(|s| s.channel_count)
        .filter(|&c| c > 0)
        .max()
    {
        chips.push(match ch {
            1 => "Mono".to_string(),
            2 => "Stereo".to_string(),
            n => format!("{n} ch"),
        });
    }

    let total: u64 = songs.iter().filter_map(|s| s.size).sum();
    if total > 0 {
        chips.push(fmt_bytes(total));
    }
    chips
}

/// ReplayGain summary line, or `None` when no track carries the tags. Album
/// gain is one value for the whole album, so the first track that has it wins;
/// track gains are shown as the range they span.
fn replaygain_line(songs: &[Song]) -> Option<String> {
    let gains: Vec<&subsonic::ReplayGain> = songs
        .iter()
        .filter_map(|s| s.replay_gain.as_ref())
        .collect();
    if gains.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(album_gain) = gains.iter().find_map(|g| g.album_gain) {
        parts.push(format!("album {album_gain:+.2} dB"));
    }
    if let Some(peak) = gains
        .iter()
        .filter_map(|g| g.album_peak)
        .fold(None, |acc: Option<f32>, p| {
            Some(acc.map_or(p, |a| a.max(p)))
        })
    {
        parts.push(format!("peak {peak:.2}"));
    }
    let track_gains: Vec<f32> = gains.iter().filter_map(|g| g.track_gain).collect();
    if let (Some(lo), Some(hi)) = (
        track_gains
            .iter()
            .copied()
            .fold(None, |a: Option<f32>, g| Some(a.map_or(g, |a| a.min(g)))),
        track_gains
            .iter()
            .copied()
            .fold(None, |a: Option<f32>, g| Some(a.map_or(g, |a| a.max(g)))),
    ) {
        parts.push(if (hi - lo).abs() < 0.005 {
            format!("tracks {lo:+.2} dB")
        } else {
            format!("tracks {lo:+.2} … {hi:+.2} dB")
        });
    }
    (!parts.is_empty()).then(|| format!("ReplayGain: {}", parts.join(" · ")))
}

/// `44100` → `44.1 kHz`, dropping a trailing `.0`.
fn fmt_khz(hz: u32) -> String {
    let khz = hz as f32 / 1000.0;
    if (khz - khz.round()).abs() < 0.05 {
        format!("{} kHz", khz.round() as u32)
    } else {
        format!("{khz:.1} kHz")
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

/// Server timestamps are ISO-8601 (`2019-03-08T21:12:44Z`); only the date is
/// worth showing, and anything unexpected is passed through untouched.
fn fmt_added(created: &str) -> String {
    created.split('T').next().unwrap_or(created).to_string()
}

impl Render for AlbumDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let playing_id = self.player.read(cx).current_song().map(|s| s.id.clone());
        // This album's own colour, when the page is set to carry one. The
        // playing-track highlight below deliberately keeps the theme's accent:
        // it marks playback, which is what the rest of the app is coloured by.
        // Extraction is kicked off from here as well as from the cover fetch,
        // so turning the setting on tints the page already open instead of
        // waiting for the next visit; it no-ops once the cover has been read.
        self.refresh_accent(cx);
        let page_accent = self.page_accent(cx);
        let header_tint = self.header_tint(cx);

        let (album_starred, album_rating) = self
            .album
            .as_ref()
            .map(|a| (a.album.starred.is_some(), a.album.user_rating.unwrap_or(0)))
            .unwrap_or((false, 0));

        let header = {
            let (name, artist, artist_id, meta) = match &self.album {
                Some(a) => {
                    let songs = a.album.song_count.unwrap_or(a.song.len() as u32);
                    let dur = a
                        .album
                        .duration
                        .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                        .unwrap_or_default();
                    let year = a.album.year.map(|y| format!("{y} · ")).unwrap_or_default();
                    (
                        a.album.name.clone(),
                        a.album.artist.clone().unwrap_or_default(),
                        a.album.artist_id.clone(),
                        format!("{year}{songs} tracks · {dur}"),
                    )
                }
                None => ("…".into(), String::new(), None, String::new()),
            };
            let has_songs = self.album.as_ref().is_some_and(|a| !a.song.is_empty());

            // Chips: genre / added date first (album-level), then the file
            // facts derived from the tracks.
            let mut chips: Vec<String> = Vec::new();
            if let Some(a) = &self.album {
                if let Some(genre) = a
                    .album
                    .genre
                    .as_deref()
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                {
                    chips.push(genre.to_string());
                }
                let discs = a
                    .song
                    .iter()
                    .filter_map(|s| s.disc_number)
                    .max()
                    .unwrap_or(0);
                if discs > 1 {
                    chips.push(format!("{discs} discs"));
                }
                chips.extend(quality_chips(&a.song));
                if let Some(created) = a.album.created.as_deref().filter(|c| !c.is_empty()) {
                    chips.push(format!("Added {}", fmt_added(created)));
                }
            }
            let chip_row = h_flex().gap_1p5().flex_wrap().children(
                chips
                    .into_iter()
                    .map(|text| {
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(cx.theme().muted)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(text)
                    })
                    .collect::<Vec<_>>(),
            );
            let replaygain = self.album.as_ref().and_then(|a| replaygain_line(&a.song));

            let rating_stars = h_flex().gap_0p5().children((1..=5u8).map(|r| {
                div()
                    .id(("rate", r as usize))
                    .cursor_pointer()
                    .text_color(if r <= album_rating {
                        cx.theme().accent
                    } else {
                        cx.theme().muted_foreground
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.rate_album(r, cx)))
                    .child(app_icon(if r <= album_rating {
                        icons::STAR_FILLED
                    } else {
                        icons::STAR_OUTLINE
                    }))
            }));

            h_flex()
                .gap_4()
                .items_start()
                .flex_wrap()
                .child(
                    div()
                        .id("album-cover")
                        .size(px(220.))
                        .rounded_2xl()
                        .bg(cx.theme().muted)
                        .overflow_hidden()
                        .when_some(self.art_path.clone(), |this, path| {
                            // Click to view the cover at full resolution.
                            this.cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.open_full_art(cx)))
                                .child(img(path).size(px(220.)).rounded_2xl())
                        }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w(px(260.))
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    // flex_1 + min_w_0 so a long title wraps
                                    // inside the header instead of pushing the
                                    // star button off the row.
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_2xl()
                                        .font_medium()
                                        .child(name),
                                )
                                .child(
                                    Button::new("album-star")
                                        .ghost()
                                        .xsmall()
                                        .icon(app_icon(if album_starred {
                                            icons::STAR_FILLED
                                        } else {
                                            icons::STAR_OUTLINE
                                        }))
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.toggle_album_star(cx)
                                            }),
                                        ),
                                ),
                        )
                        .child(match artist_id {
                            // Artist name links to the artist page.
                            Some(id) => div()
                                .id("album-artist")
                                .cursor_pointer()
                                .hover(|s| s.text_color(cx.theme().accent))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(AlbumDetailEvent::OpenArtist(id.clone()));
                                }))
                                .child(artist)
                                .into_any_element(),
                            None => div().child(artist).into_any_element(),
                        })
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(meta),
                        )
                        .child(chip_row)
                        .when_some(replaygain, |this, line| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(line),
                            )
                        })
                        .child(rating_stars)
                        .child(
                            h_flex()
                                .gap_2()
                                .mt_1()
                                .child({
                                    let play = Button::new("album-play")
                                        .icon(app_icon(icons::PLAY))
                                        .label("Play")
                                        .disabled(!has_songs)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.play_from(0, cx)),
                                        );
                                    match page_accent {
                                        Some(a) => play.custom(crate::ui::accent_button(a, cx)),
                                        None => play.primary(),
                                    }
                                })
                                .child(
                                    Button::new("album-shuffle")
                                        .ghost()
                                        .icon(app_icon(icons::SHUFFLE))
                                        .label("Shuffle")
                                        .disabled(!has_songs)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.play_shuffled(cx)),
                                        ),
                                ),
                        ),
                )
        };

        let info_prefs = self.session.read(cx).settings.track_info.clone();

        let rows: Vec<_> = self
            .album
            .clone()
            .iter()
            .flat_map(|a| a.song.iter())
            .enumerate()
            .map(|(i, song)| {
                let is_playing = playing_id.as_deref() == Some(song.id.as_str());
                let starred = song.starred.is_some();
                let track_no = song.track.map(|t| t.to_string()).unwrap_or_default();
                let extras = track_extras(song, &info_prefs, false);
                let dur = song
                    .duration
                    .map(|s| format_duration(std::time::Duration::from_secs(s as u64)))
                    .unwrap_or_default();
                let plays = song
                    .play_count
                    .filter(|&p| p > 0)
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let song_next = song.clone();
                let song_enq = song.clone();
                // Right-click context menu data.
                let menu_song = song.clone();
                let menu_artist_id = song.artist_id.clone();
                let menu_view = cx.entity();
                let menu_song_id = song.id.clone();
                let menu_playlists = self.playlists.clone();
                let menu_pl_list: Vec<(String, String)> = self
                    .playlists
                    .read(cx)
                    .playlists
                    .iter()
                    .map(|p| (p.id.clone(), p.name.clone()))
                    .collect();
                let row = h_flex()
                    .id(("track", i))
                    .group("trow")
                    .px_2()
                    .py_1()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .when(is_playing, |s| {
                        // `primary` is the vivid theme colour; `accent` is a
                        // background tint with poor text contrast.
                        s.bg(cx.theme().muted)
                            .border_l_2()
                            .border_color(cx.theme().primary)
                            .text_color(cx.theme().primary)
                    })
                    .when(self.vi_cursor == Some(i), |s| {
                        s.bg(cx.theme().muted)
                            .border_1()
                            .border_color(cx.theme().primary)
                            .shadow(focus_glow(cx))
                            .anchor_scroll(Some(self.focus_anchor.clone()))
                    })
                    .on_click(cx.listener(move |view, _, _, cx| view.play_from(i, cx)))
                    .child(
                        div()
                            .w(px(28.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(track_no),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(song.title.clone()),
                    )
                    .when(!extras.is_empty(), |this| {
                        this.child(
                            div()
                                .max_w(px(320.))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(extras),
                        )
                    })
                    // Hover actions: play-next, enqueue, star, add-to-playlist.
                    .child(
                        h_flex()
                            .gap_0p5()
                            .opacity(0.25)
                            .group_hover("trow", |s| s.opacity(1.))
                            .child(
                                Button::new(("t-next", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(app_icon(icons::SKIP_FORWARD))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.play_next(vec![song_next.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                Button::new(("t-enq", i))
                                    .ghost()
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.enqueue(vec![song_enq.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(
                                Button::new(("t-star", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(app_icon(if starred {
                                        icons::STAR_FILLED
                                    } else {
                                        icons::STAR_OUTLINE
                                    }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_song_star(i, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                            .child(self.playlist_popover(i, song, cx)),
                    )
                    // Play count and duration: fixed right-aligned columns,
                    // same text size, extra margin between them.
                    .child(
                        h_flex()
                            .w(px(40.))
                            .flex_none()
                            .justify_end()
                            .mr_3()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(plays),
                    )
                    .child(
                        h_flex()
                            .w(px(44.))
                            .flex_none()
                            .justify_end()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(dur),
                    )
                    .context_menu(move |menu, window, cx| {
                        let play_view = menu_view.clone();
                        let next_view = menu_view.clone();
                        let next_song = menu_song.clone();
                        let enq_view = menu_view.clone();
                        let enq_song = menu_song.clone();
                        let star_view = menu_view.clone();
                        // Clone per open: the outer builder is called repeatedly.
                        let pl_list = menu_pl_list.clone();
                        let playlists = menu_playlists.clone();
                        let song_id = menu_song_id.clone();
                        let mut menu = menu
                            .item(PopupMenuItem::new("Play").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    play_view.update(cx, |v, cx| v.play_from(i, cx));
                                },
                            ))
                            .item(PopupMenuItem::new("Play next").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let song = next_song.clone();
                                    next_view.update(cx, |v, cx| {
                                        v.player.update(cx, |p, cx| p.play_next(vec![song], cx))
                                    });
                                },
                            ))
                            .item(PopupMenuItem::new("Add to queue").on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let song = enq_song.clone();
                                    enq_view.update(cx, |v, cx| {
                                        v.player.update(cx, |p, cx| p.enqueue(vec![song], cx))
                                    });
                                },
                            ))
                            .submenu("Save to playlist", window, cx, move |sub, _w, _c| {
                                if pl_list.is_empty() {
                                    return sub.item(
                                        PopupMenuItem::new("No playlists yet").disabled(true),
                                    );
                                }
                                let mut sub = sub;
                                for (pid, pname) in &pl_list {
                                    let playlists = playlists.clone();
                                    let pid = pid.clone();
                                    let song_id = song_id.clone();
                                    sub = sub.item(PopupMenuItem::new(pname.clone()).on_click(
                                        move |_, _, cx: &mut gpui::App| {
                                            playlists.update(cx, |pl, cx| {
                                                pl.add_song(pid.clone(), song_id.clone(), cx)
                                            });
                                        },
                                    ));
                                }
                                sub
                            })
                            .item(
                                PopupMenuItem::new(if starred { "Unstar" } else { "Star" })
                                    .on_click(move |_, _, cx: &mut gpui::App| {
                                        star_view.update(cx, |v, cx| v.toggle_song_star(i, cx));
                                    }),
                            );
                        if let Some(aid) = menu_artist_id.clone() {
                            let artist_view = menu_view.clone();
                            menu = menu.item(PopupMenuItem::separator()).item(
                                PopupMenuItem::new("Go to artist").on_click(
                                    move |_, _, cx: &mut gpui::App| {
                                        artist_view.update(cx, |_, cx| {
                                            cx.emit(AlbumDetailEvent::OpenArtist(aid.clone()))
                                        });
                                    },
                                ),
                            );
                        }
                        menu
                    });
                if self.vi_cursor == Some(i) {
                    with_focus_animation(format!("vi-focus-{i}"), row, cx).into_any_element()
                } else {
                    row.into_any_element()
                }
            })
            .collect();

        // Description + external links (getAlbumInfo2). Collapsed by truncating
        // the string: gpui's line_clamp can't do it (see the artist bio).
        let notes = self
            .info
            .as_ref()
            .and_then(|i| i.notes.as_deref())
            .map(strip_html)
            .filter(|n| !n.is_empty());
        let notes_long = notes
            .as_ref()
            .is_some_and(|n| n.chars().count() > NOTES_PREVIEW_CHARS);
        let notes_text = notes.map(|n| {
            if self.notes_expanded || !notes_long {
                n
            } else {
                truncate_at_word(&n, NOTES_PREVIEW_CHARS)
            }
        });
        let musicbrainz_url = self
            .info
            .as_ref()
            .and_then(|i| i.music_brainz_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://musicbrainz.org/release/{id}"));
        let lastfm_url = self
            .info
            .as_ref()
            .and_then(|i| i.last_fm_url.as_deref())
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        let has_links = musicbrainz_url.is_some() || lastfm_url.is_some();
        let notes_expanded = self.notes_expanded;
        let about = (notes_text.is_some() || has_links).then(|| {
            v_flex()
                .rounded_2xl()
                .p_4()
                .gap_2()
                .bg(cx.theme().sidebar)
                // Header only when there is prose under it: servers without a
                // metadata agent return links alone, and an "About" heading
                // over a bare Last.fm link reads like something failed to load.
                .when_some(notes_text, |this, text| {
                    this.child(div().text_sm().font_medium().child("About"))
                        .child(div().text_sm().child(text))
                })
                .when(notes_long, |this| {
                    this.child(
                        h_flex().child(
                            Button::new("notes-toggle")
                                .ghost()
                                .xsmall()
                                .label(if notes_expanded { "Less" } else { "More" })
                                .icon(Icon::new(if notes_expanded {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                }))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notes_expanded = !this.notes_expanded;
                                    cx.notify();
                                })),
                        ),
                    )
                })
                .when(has_links, |this| {
                    this.child(
                        h_flex()
                            .gap_3()
                            .text_sm()
                            .when_some(musicbrainz_url, |this, url| {
                                this.child(Link::new("al-mb-link").href(url).child("MusicBrainz"))
                            })
                            .when_some(lastfm_url, |this, url| {
                                this.child(Link::new("al-lastfm-link").href(url).child("Last.fm"))
                            }),
                    )
                })
        });

        let scroll = v_flex()
            .id("album-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            // Header card matches the artist page framing.
            .child(
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    // The album's colour washes across the header card and
                    // fades back into the normal surface, so the page reads as
                    // this album's without the track list losing contrast.
                    .map(|this| match header_tint {
                        Some(accent) => this.bg(linear_gradient(
                            160.,
                            linear_color_stop(crate::ui::page_tint(accent), 0.),
                            linear_color_stop(cx.theme().sidebar, 0.85),
                        )),
                        None => this.bg(cx.theme().sidebar),
                    })
                    .child(header),
            )
            .children(about)
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(v_flex().gap_0p5().children(rows));

        div()
            .relative()
            .size_full()
            .child(scroll)
            // Full-resolution cover lightbox; click anywhere to dismiss.
            .when(self.show_full_art, |this| {
                this.child(
                    div()
                        .id("album-lightbox")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_8()
                        .occlude()
                        .cursor_pointer()
                        .bg(gpui::hsla(0., 0., 0., 0.88))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_full_art = false;
                            cx.notify();
                        }))
                        .when_some(
                            self.full_art_path.clone().or_else(|| self.art_path.clone()),
                            |this, path| {
                                this.child(img(path).max_w(px(820.)).max_h(px(820.)).rounded_lg())
                            },
                        ),
                )
            })
    }
}

impl AlbumDetailView {
    /// Move the vi-mode cursor by `delta` tracks, clamping to the album's
    /// track list and scrolling the focused row into view.
    pub fn vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(count) = self.album.as_ref().map(|a| a.song.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        self.focus_anchor.scroll_to(window, cx);
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Play the track under the vi-mode cursor.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.vi_cursor {
            self.play_from(i, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_bytes, fmt_khz, quality_chips, replaygain_line};
    use subsonic::Song;

    /// Songs carry ~20 fields; building them from JSON keeps the cases readable
    /// and exercises the same deserialization the client uses.
    fn song(json: &str) -> Song {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn quality_chips_summarize_uniform_album() {
        let songs = vec![
            song(
                r#"{"id":"1","title":"a","suffix":"flac","bitRate":1004,"samplingRate":44100,
                    "bitDepth":16,"channelCount":2,"size":31457280}"#,
            ),
            song(
                r#"{"id":"2","title":"b","suffix":"flac","bitRate":1004,"samplingRate":44100,
                    "bitDepth":16,"channelCount":2,"size":31457280}"#,
            ),
        ];
        assert_eq!(
            quality_chips(&songs),
            vec![
                "FLAC".to_string(),
                "1004 kbps".into(),
                "44.1 kHz · 16 bit".into(),
                "Stereo".into(),
                "60 MB".into(),
            ]
        );
    }

    #[test]
    fn quality_chips_show_ranges_for_mixed_sources() {
        let songs = vec![
            song(r#"{"id":"1","title":"a","suffix":"flac","bitRate":1004,"samplingRate":96000}"#),
            song(r#"{"id":"2","title":"b","contentType":"audio/mpeg","bitRate":320}"#),
        ];
        let chips = quality_chips(&songs);
        assert_eq!(chips[0], "FLAC / MPEG");
        assert_eq!(chips[1], "320–1004 kbps");
        assert_eq!(chips[2], "96 kHz");
    }

    #[test]
    fn quality_chips_empty_without_opensubsonic_fields() {
        let songs = vec![song(r#"{"id":"1","title":"a"}"#)];
        assert!(quality_chips(&songs).is_empty());
    }

    #[test]
    fn replaygain_line_reports_album_gain_peak_and_track_range() {
        let songs = vec![
            song(
                r#"{"id":"1","title":"a","replayGain":{"albumGain":-8.3,"albumPeak":0.98,
                    "trackGain":-9.1}}"#,
            ),
            song(
                r#"{"id":"2","title":"b","replayGain":{"albumGain":-8.3,"albumPeak":0.99,
                    "trackGain":-7.2}}"#,
            ),
        ];
        assert_eq!(
            replaygain_line(&songs).unwrap(),
            "ReplayGain: album -8.30 dB · peak 0.99 · tracks -9.10 … -7.20 dB"
        );
    }

    #[test]
    fn replaygain_line_absent_without_tags() {
        assert!(replaygain_line(&[song(r#"{"id":"1","title":"a"}"#)]).is_none());
    }

    #[test]
    fn khz_and_bytes_formatting() {
        assert_eq!(fmt_khz(44100), "44.1 kHz");
        assert_eq!(fmt_khz(48000), "48 kHz");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.00 GB");
    }
}
