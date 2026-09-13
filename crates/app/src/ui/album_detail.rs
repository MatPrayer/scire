//! Album page: header (artwork, star, rating) + track list with play,
//! queue and playlist actions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnimationExt as _, AnyElement, App, Context, Entity, EventEmitter, IntoElement, Render,
    ScrollAnchor, ScrollHandle, Window, div, img, linear_color_stop, linear_gradient, prelude::*,
    px, relative,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::link::Link;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::{AlbumInfo2, AlbumWithSongs, Song, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::ThemePref;
use crate::services::library_db::LibraryDb;
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::Session;
use crate::ui::albums::album_from_row;
use crate::ui::{
    format_duration, strip_html, sync_focus_scroll, track_extras, truncate_at_word,
    with_focus_cursor,
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

/// Read a cached cover file and reduce it to a page accent.
///
/// Shared by the two passes that produce one — the canonical fetch and the
/// provisional cached rendition — so they cannot differ in anything but which
/// file they are handed.
fn accent_from_file(path: &Path) -> anyhow::Result<gpui::Hsla> {
    let bytes = std::fs::read(path)?;
    crate::ui::accent_from_cover_bytes(&bytes)
        .ok_or_else(|| anyhow::anyhow!("cover art did not decode"))
}

/// Namespaces `navidrome_sync` stores its rows under. This view is opened with
/// the bare id the live listing uses, so both directions have to be crossed:
/// the lookup is qualified up, and everything handed back out is stripped down.
const ALBUM_NS: &str = "navidrome:album:";
const TRACK_NS: &str = "navidrome:track:";

/// The last sync's stand-in for `album_id`: its album row plus its tracks, or
/// `None` when the cache cannot answer for it.
///
/// Split out of `seed_from_cache` so the id crossing above is testable without
/// a window — it silently missed on every album once the sync started
/// namespacing, which left the page seeding nothing at all.
fn cached_album(db: &LibraryDb, album_id: &str) -> Option<AlbumWithSongs> {
    let key = format!("{ALBUM_NS}{album_id}");
    let row = db.album_by_id("navidrome", &key).ok().flatten()?;
    let songs: Vec<Song> = db
        .tracks_by_album(&key)
        .unwrap_or_default()
        .into_iter()
        .map(|track| {
            let mut song = track.into_song();
            // These ids leave the page: playback and scrobbling send them back
            // to the server, and the playing-row highlight compares them
            // against the player's own.
            if let Some(bare) = song.id.strip_prefix(TRACK_NS) {
                song.id = bare.to_string();
            }
            // Not stored per track, but `artwork::song_cover` groups an
            // album's covers under it — without it every seeded row would ask
            // for its own copy of the same art.
            song.album_id = Some(album_id.to_string());
            song
        })
        .collect();
    // An album row whose tracks never landed (a sync interrupted between
    // phases) would seed an empty track list, which reads as a broken page
    // rather than a loading one.
    if songs.is_empty() {
        return None;
    }
    Some(AlbumWithSongs {
        album: album_from_row(row),
        song: songs,
    })
}

/// How many tracks the last sync recorded for this album, for the case
/// `cached_album` refuses: an album row whose tracks never landed still knows
/// its own `song_count`, and a skeleton list of the right length is the
/// difference between a track list that fills in and one that grows or
/// collapses under the pointer.
fn cached_song_count(db: &LibraryDb, album_id: &str) -> Option<usize> {
    let row = db
        .album_by_id("navidrome", &format!("{ALBUM_NS}{album_id}"))
        .ok()
        .flatten()?;
    (row.song_count > 0).then_some(row.song_count as usize)
}

/// How long a request runs before its placeholders become *visible*.
///
/// The space is reserved from the first frame either way — that is the whole
/// point of the placeholders, and a gate on the space itself only moved the
/// shift it exists to prevent back to wherever the gate opened. What the delay
/// gates is the grey: the cache seeds the page instantly and a warm server
/// answers in well under a tenth of a second, so bars *painted* the moment a
/// request is issued are on screen for two frames and gone, which reads as a
/// flash. Below the delay the placeholder is an empty hole of exactly the right
/// size, so the words land in silence; past it the request is slow enough that
/// the page should say so.
const PLACEHOLDER_DELAY: Duration = Duration::from_millis(220);

/// True once a request has been in flight long enough to be worth showing.
fn placeholding(since: Option<Instant>) -> bool {
    since.is_some_and(|t| t.elapsed() >= PLACEHOLDER_DELAY)
}

/// The pulse every placeholder here shares, over gpui-component's `Skeleton`
/// colour. Not `Skeleton` itself: that is a bare `div` with no children, so a
/// placeholder built from it can only be given a size in px, which is the one
/// thing that must not be guessed.
///
/// `show` decides whether anything is painted, never whether anything is laid
/// out: an unshown placeholder keeps its element, its sample text and so its
/// exact size, and only loses the fill and the animation.
fn skeleton_pulse(
    id: impl Into<gpui::ElementId>,
    show: bool,
    el: gpui::Div,
    cx: &App,
) -> AnyElement {
    // Whatever text is inside is there for its metrics alone.
    let el = el.rounded_md().text_color(gpui::transparent_black());
    if !show {
        return el.into_any_element();
    }
    el.bg(cx.theme().skeleton)
        .with_animation(
            id,
            gpui::Animation::new(Duration::from_secs(2))
                .repeat()
                .with_easing(gpui::bounce(gpui::ease_in_out)),
            |this, delta| this.opacity(1. - delta * 0.5),
        )
        .into_any_element()
}

/// A placeholder standing in for a line of text the server has not answered for
/// yet, shaped by a *sample string* laid out in the surrounding type styles.
///
/// The field it replaces is text, so the only height guaranteed to match the
/// line that lands is the one the text engine produces for the same styles —
/// hard-coded px were a few off in every place they were used, and the page
/// still stepped when the words arrived.
fn skeleton_text(id: impl Into<gpui::ElementId>, show: bool, sample: &str, cx: &App) -> AnyElement {
    // Wrapped in a row because a column stretches its children: on its own the
    // bar would be the width of the page rather than of its own sample.
    h_flex()
        .child(skeleton_pulse(
            id,
            show,
            div().child(sample.to_string()),
            cx,
        ))
        .into_any_element()
}

/// Same, at an explicit width: prose fills the column it sits in and so has no
/// sample to take a width from. The non-breaking space is what gives it the
/// line's height.
fn skeleton_line(
    id: impl Into<gpui::ElementId>,
    show: bool,
    w: gpui::DefiniteLength,
    cx: &App,
) -> AnyElement {
    skeleton_pulse(id, show, div().w(w).child("\u{a0}"), cx)
}

/// A placeholder chip: the real chip's padding and type, so the row it is in is
/// exactly as tall as the one that replaces it.
fn skeleton_chip(id: impl Into<gpui::ElementId>, show: bool, sample: &str, cx: &App) -> AnyElement {
    skeleton_pulse(
        id,
        show,
        div().px_2().py_0p5().text_xs().child(sample.to_string()),
        cx,
    )
}

/// Samples for the quality chips, the header's three lines and the ReplayGain
/// line — each the shape of what actually lands there, so the placeholders are
/// the width of a plausible answer rather than of a round number.
/// One per chip `quality_chips` can add — format, bitrate, rate/depth, channels
/// and total size, in that order. The count is what matters more than the
/// widths: the row wraps, so reserving four where five land pushes the header a
/// line taller when they arrive, and in the stacked layout the header sits on
/// top of the track list, which is why the whole page was seen to step. The
/// genre and "Added" chips are not here — the cache carries both, so a seeded
/// page already draws them.
const CHIP_SAMPLES: [&str; 5] = [
    "FLAC",
    "1004 kbps",
    "44.1 kHz · 16 bit",
    "Stereo",
    "612.4 MB",
];
const TITLE_SAMPLE: &str = "The Dark Side of the Moon";
const CREDITS_SAMPLE: &str = "Pink Floyd";
const META_SAMPLE: &str = "1973 · 10 tracks · 42:59";
const REPLAYGAIN_SAMPLE: &str = "ReplayGain −7.2 dB album · −7.4 dB track";
const TRACK_SKELETONS: usize = 8;

/// The About card's prose placeholder: `NOTES_PREVIEW_CHARS` of sample text,
/// laid out in the card's own width and type styles.
///
/// A fixed number of bars cannot work here. The card is as wide as whatever
/// column it lands in, so how many lines 400 characters wrap to is a property
/// of the window — three bars stood in for six lines on a narrow one, and the
/// card grew by half its height when the notes arrived. Wrapping the same
/// character count the collapsed view shows gives the text engine the same job
/// it is about to do for real.
fn notes_sample() -> String {
    const WORDS: &str = "the album was recorded over several sessions and mixed \
                         the following spring by a band that had been touring it \
                         for the better part of a year ";
    WORDS.chars().cycle().take(NOTES_PREVIEW_CHARS).collect()
}

/// A placeholder for a block of prose: fills its column and takes its height
/// from the wrapped sample, where `skeleton_line` is one line at a given width.
fn skeleton_block(
    id: impl Into<gpui::ElementId>,
    show: bool,
    sample: &str,
    cx: &App,
) -> AnyElement {
    skeleton_pulse(id, show, div().w_full().child(sample.to_string()), cx)
}

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
    /// The side panel's own scroll, in the layout that has one: it holds the
    /// cover, the details and the About card, which on a tall album are more
    /// than a panel's height, and it must scroll without taking the track list
    /// with it.
    panel_scroll: ScrollHandle,
    /// Width the side panel was drawn at last frame, or `0.` in the stacked
    /// layout.
    ///
    /// `scroll` tracks the *track list*, which in the side-panel layout is only
    /// part of the content area — measuring it alone would feed `live_width` a
    /// width that the panel's own presence had already taken a bite out of, and
    /// the panel would shrink itself every frame. The panel is a fixed-width
    /// sibling with no gap between them, so their sum is the content width
    /// exactly, in either layout.
    panel_w: f32,
    /// Window width the page lays out at, bridged from the last frame's
    /// measurement. The header card is sized from it rather than left to
    /// stretch — see `content_width`.
    live_width: crate::ui::LiveWidth,
    focus_anchor: ScrollAnchor,
    /// Track index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Cursor position the scroll has caught up to, so `render` scrolls only
    /// when the cursor actually moved (`ui::sync_focus_scroll`).
    vi_scroll_synced: Option<usize>,
    /// Accent extracted from *this album's* cover, for the page's own tint
    /// under `Settings::adaptive_from_page`. The app's chrome keeps the playing
    /// track's accent; only this page carries the album's.
    accent: Option<gpui::Hsla>,
    /// Cover id the accent was extracted from, so a repaint doesn't re-fetch
    /// and re-decode it.
    accent_for: Option<String>,
    /// Same, for the provisional accent below. Its own guard because it runs
    /// on a different condition than the canonical pass — cached art rather
    /// than a client — and a shared flag would let a repaint respawn one of
    /// them every frame.
    accent_seed_for: Option<String>,
    /// A `getAlbum` is in flight, and `getAlbum` has never answered for this
    /// page. Both halves are needed: the request is re-issued whenever playback
    /// enters or leaves the album, and swapping the chips a user is reading for
    /// placeholders is worse than the shift the placeholders exist to prevent.
    album_pending: bool,
    album_loaded: bool,
    /// When the request in flight was issued, for `PLACEHOLDER_DELAY`.
    album_since: Option<Instant>,
    /// Same, for `getAlbumInfo2` behind the About card.
    info_pending: bool,
    info_loaded: bool,
    info_since: Option<Instant>,
    /// The cache's track count for an album it could not seed rows for, so the
    /// skeleton list is as long as the real one.
    expected_tracks: Option<usize>,
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
            panel_scroll: ScrollHandle::new(),
            panel_w: 0.,
            live_width: crate::ui::LiveWidth::default(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            accent: None,
            accent_for: None,
            accent_seed_for: None,
            album_pending: false,
            album_loaded: false,
            album_since: None,
            info_pending: false,
            info_loaded: false,
            info_since: None,
            expected_tracks: None,
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
        let Some(seed) = cached_album(db, &self.album_id) else {
            // Nothing to paint, but the count is worth having anyway — it is
            // the skeleton track list's length.
            self.expected_tracks = cached_song_count(db, &self.album_id);
            return;
        };
        let cover = seed.album.cover_art.clone();
        self.album = Some(seed);
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

    /// Repaint once `PLACEHOLDER_DELAY` has elapsed.
    ///
    /// The placeholders appear on a *deadline* rather than on an event, and a
    /// clock running out dirties nothing — without this the page would only
    /// start holding its shape at whatever unrelated repaint happened next,
    /// which for an album page nobody is touching is the response itself.
    fn wake_at_placeholder_delay(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PLACEHOLDER_DELAY).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id.clone();
        // Set here rather than at the top of the function: with no client there
        // is nothing in flight, and a page that will never be filled in must not
        // draw placeholders forever.
        self.album_pending = true;
        self.album_since = Some(Instant::now());
        Self::wake_at_placeholder_delay(cx);
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_album(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.album_pending = false;
                view.album_loaded = true;
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
        self.info_pending = true;
        self.info_since = Some(Instant::now());
        Self::wake_at_placeholder_delay(cx);
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album_info2(&id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.info_pending = false;
                // Marked loaded either way: a server without the metadata agent
                // answers with an empty element rather than an error, and in
                // both cases there is nothing more coming to hold a placeholder
                // open for.
                view.info_loaded = true;
                if let Ok(info) = result {
                    view.info = Some(info);
                }
                cx.notify();
            });
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

    /// Extract the page's accent from this album's cover. Keyed on the cover
    /// id, so the repeated `load`s this page does (playback entering or leaving
    /// the album) don't re-decode the same image.
    ///
    /// Deliberately *not* taken from `art_path`: that is the header's cover, at
    /// whichever rung was already cached, and the accent is a
    /// saturation-weighted mean over the pixels — a different resize of the
    /// same sleeve tips a two-hue cover onto the other hue, so the page and the
    /// player bar showed the same album in two colours. Fetching
    /// `ACCENT_ART_SIZE` under the album-scoped cache key instead means both
    /// read the exact same file, and for the playing album it is already there.
    fn refresh_accent(&mut self, cx: &mut Context<Self>) {
        let settings = &self.session.read(cx).settings;
        if settings.theme != ThemePref::Adaptive || !settings.adaptive_from_page {
            return;
        }
        let Some(cover_id) = self.album.as_ref().and_then(|a| a.album.cover_art.clone()) else {
            return;
        };
        self.seed_accent(&cover_id, cx);
        if self.accent_for.as_ref() == Some(&cover_id) {
            return;
        }
        // Pre-connect: leave the key unset so the next repaint retries once
        // there is a client to fetch with.
        let Some(client) = self.client(cx) else {
            return;
        };
        self.accent_for = Some(cover_id.clone());
        let key = artwork::album_cover_key(&self.album_id);
        cx.spawn(async move |this, cx| {
            let accent = runtime::spawn_io(async move {
                let path =
                    artwork::fetch_as(client, cover_id, key, crate::ui::ACCENT_ART_SIZE).await?;
                accent_from_file(&path)
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

    /// Tint the page from whatever rendition of this cover is already on disk,
    /// while `refresh_accent` above resolves the canonical one.
    ///
    /// The canonical pass wants one exact file so the page and the player bar
    /// cannot disagree, and for an album that has never been played that file
    /// is not cached — so it downloads it, and the tint arrives a round trip
    /// after the header, the track list and the cover art, all of which the
    /// cache already painted. The grid's own thumbnail of the same sleeve is
    /// right there. A different resize can tip a two-hue cover onto the other
    /// hue, which is exactly why this is not the authority: it is a stand-in
    /// the canonical accent overwrites when it lands, the same bargain the
    /// header art makes with `cached_best`.
    fn seed_accent(&mut self, cover_id: &str, cx: &mut Context<Self>) {
        if self.accent_seed_for.as_deref() == Some(cover_id) {
            return;
        }
        // Album-scoped key first — those are the rungs the canonical pass
        // itself reads, so they are the closest stand-ins — then the cover id,
        // where the grid's thumbnail and this page's header art live.
        let key = artwork::album_cover_key(&self.album_id);
        let Some(path) = artwork::cached_best(&key, crate::ui::ACCENT_ART_SIZE)
            .or_else(|| artwork::cached_best(cover_id, crate::ui::ACCENT_ART_SIZE))
        else {
            return;
        };
        self.accent_seed_for = Some(cover_id.to_string());
        cx.spawn(async move |this, cx| {
            // Decoding and walking every pixel of what may be a 1500px cover is
            // pure CPU that never yields: the blocking pool, not one of the two
            // IO workers everything else is queued behind.
            let Ok(accent) = runtime::spawn_blocking_io(move || accent_from_file(&path)).await
            else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                // A canonical accent that landed first outranks this one.
                if view.accent.is_none() {
                    view.accent = Some(accent);
                    cx.notify();
                }
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

/// Every artist the album is credited to, as `(name, id)` pairs.
///
/// OpenSubsonic's `artists` array is the only place a collaboration is spelled
/// out — `artist`/`artistId` collapse it to the one artist the server picked as
/// primary, so linking the whole display string sent *every* name on the line
/// to that first artist's page. Vanilla servers send no array and fall back to
/// the single pair, which renders exactly as it did before.
fn album_credits(album: &subsonic::Album) -> Vec<(String, Option<String>)> {
    if !album.artists.is_empty() {
        return album
            .artists
            .iter()
            .map(|a| (a.name.clone(), Some(a.id.clone())))
            .collect();
    }
    match album
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => vec![(name.to_string(), album.artist_id.clone())],
        None => Vec::new(),
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

/// Horizontal padding the scrolling column lays its cards out within (`p_4`
/// either side).
const PAGE_PADDING_X: f32 = 32.;

/// Cover edge in the stacked header. The side panel draws a much larger one —
/// `ui::album_side_panel` sizes it from the panel it fits in.
const HEADER_ART: f32 = 220.;

impl AlbumDetailView {
    /// Width the page has to lay out in, or `0.` on the first frame — before
    /// anything has been measured — where the header card falls back to
    /// stretching.
    ///
    /// The card has to carry an explicit width because its height is measured
    /// before the stretch that gives it one: the chip row wraps, so the height
    /// taffy arrives at is the header's height at whatever narrower width that
    /// pass used, and a window with vertical room to spare keeps it. The result
    /// is a header card several hundred pixels taller than its contents, with
    /// the album's colour washing down through the gap — visible on any page
    /// short enough not to fill the window (an EP, say), and absent on the same
    /// page in a window too short to leave slack, since there the card is
    /// shrunk back to its real height.
    ///
    /// Width comes from the viewport rather than the scroll handle's own bounds
    /// for the reason `ui::LiveWidth` exists: the bounds are the previous
    /// frame's, so a resize would leave the card a frame behind the drag.
    fn content_width(&mut self, window: &Window) -> f32 {
        // Plus the panel: `scroll` measures the track list, which is the whole
        // content area only in the stacked layout (see `panel_w`).
        let measured = f32::from(self.scroll.bounds().size.width) + self.panel_w;
        self.live_width.resolve(measured, window)
    }
}

impl Render for AlbumDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scroll-into-view runs here, not in `vi_move`: the anchor's origin
        // is only as fresh as the last paint, and from a key handler that is
        // the row the cursor just LEFT — going up, the focused row landed one
        // row above the viewport and the highlight vanished.
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let content_w = self.content_width(window);
        let wants_panel = self
            .session
            .read(cx)
            .settings
            .album_layout
            .wants_side_panel();
        // Nothing has been measured on the view's first frame, so `content_w`
        // is 0 and `album_side_panel` can only answer "stacked" — and the
        // measurement landing dirties nothing, so that answer stays up until
        // something else repaints the page. In practice that is `load`
        // returning: the stacked layout flashes for a whole network round trip
        // before the panel appears. Ask for the frame that carries the
        // measurement and paint this one invisible — laid out (that is what it
        // is for) but never seen. Same trade the settings page makes, and
        // `request_animation_frame` for the same reason: `Window::refresh` is a
        // no-op while the window is drawing, which is exactly when a view
        // renders.
        let unmeasured = wants_panel && content_w <= 0.;
        if unmeasured {
            window.request_animation_frame();
        }
        // The side panel, when the setting asks for it *and* the window can
        // hold it; everything else falls back to the stacked page. Recorded on
        // the view because next frame's content width is measured through it.
        let viewport = window.viewport_size();
        let panel = wants_panel
            .then(|| {
                crate::ui::album_side_panel(
                    content_w,
                    f32::from(viewport.width),
                    f32::from(viewport.height),
                )
            })
            .flatten();
        self.panel_w = panel.map_or(0., |p| p.width);
        let panel_right = self.session.read(cx).settings.album_panel_right;
        // Card width, in the layout that knows one: the panel's inner width, or
        // the page's own. Zero means nothing has been measured yet, where the
        // card stretches as it always did.
        let header_w = match panel {
            Some(p) => p.width - PAGE_PADDING_X,
            None => (content_w - PAGE_PADDING_X).max(0.),
        };
        let art_px = panel.map_or(HEADER_ART, |p| p.art);
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

        // Everything this page gets from the server arrives after the frame it
        // is opened on — and for an album the cache has never seen, that is the
        // whole page. Where a field is still on its way it is drawn as a
        // placeholder of the size it will be, so the page fills in rather than
        // growing a piece at a time under the pointer.
        // Two separate questions: whether the space is held (from the first
        // frame, or the shift is merely postponed to whenever the gate opens)
        // and whether the grey is painted in it (only once the request is slow
        // enough to be worth remarking on).
        let loading_album = self.album_pending && !self.album_loaded;
        let loading_info = self.info_pending && !self.info_loaded;
        let show_album = loading_album && placeholding(self.album_since);
        let show_info = loading_info && placeholding(self.info_since);

        let (album_starred, album_rating) = self
            .album
            .as_ref()
            .map(|a| (a.album.starred.is_some(), a.album.user_rating.unwrap_or(0)))
            .unwrap_or((false, 0));

        let header = {
            let (name, credits, meta) = match &self.album {
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
                        album_credits(&a.album),
                        format!("{year}{songs} tracks · {dur}"),
                    )
                }
                None => ("…".into(), Vec::new(), String::new()),
            };
            let has_songs = self.album.as_ref().is_some_and(|a| !a.song.is_empty());
            // The cache seed answers for the title, the credits and the summary
            // line, so these only stand in for an album the last sync has never
            // seen — and with no request in flight (no client at all) the page
            // shows what it has rather than waiting on nothing.
            let header_pending = self.album.is_none() && loading_album;

            // Chips: genre / added date first (album-level), then the file
            // facts derived from the tracks.
            let mut chips: Vec<String> = Vec::new();
            let mut has_quality = false;
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
                let quality = quality_chips(&a.song);
                has_quality = !quality.is_empty();
                chips.extend(quality);
                if let Some(created) = a.album.created.as_deref().filter(|c| !c.is_empty()) {
                    chips.push(format!("Added {}", fmt_added(created)));
                }
            }
            // The quality chips are the clearest case of the whole problem: the
            // cache stores no per-file fields at all (`Track::into_song` leaves
            // every one of them `None`), so a seeded page has the album's genre
            // and nothing else until `getAlbum` lands, and the row then grows
            // by four chips under a header that has already been read.
            let chips_pending = loading_album && !has_quality;
            let chip_row = h_flex()
                .gap_1p5()
                .flex_wrap()
                .when(chips_pending, |this| {
                    this.children(
                        CHIP_SAMPLES
                            .iter()
                            .enumerate()
                            .map(|(i, s)| skeleton_chip(("chip-sk", i), show_album, s, cx)),
                    )
                })
                .children(
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

            let cover = div()
                .id("album-cover")
                .flex_none()
                .size(px(art_px))
                .rounded_2xl()
                .bg(cx.theme().muted)
                .overflow_hidden()
                .when_some(self.art_path.clone(), |this, path| {
                    // Click to view the cover at full resolution.
                    this.cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.open_full_art(cx)))
                        .child(img(path).size(px(art_px)).rounded_2xl())
                });

            let info = v_flex()
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
                                .map(|this| match header_pending {
                                    true => this.child(skeleton_text(
                                        "al-title-sk",
                                        show_album,
                                        TITLE_SAMPLE,
                                        cx,
                                    )),
                                    false => this.child(name),
                                }),
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
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_album_star(cx))),
                        ),
                )
                // One link per credited artist: a collaboration lists every
                // artist, and each opens its own page. Vanilla servers send a
                // single credit and this renders exactly as it used to.
                .child(
                    h_flex()
                        .flex_wrap()
                        .items_center()
                        .when(header_pending, |this| {
                            this.child(skeleton_text(
                                "al-credits-sk",
                                show_album,
                                CREDITS_SAMPLE,
                                cx,
                            ))
                        })
                        .children(credits.into_iter().enumerate().flat_map(
                            |(i, (artist, artist_id))| {
                                let sep = (i > 0).then(|| {
                                    div()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(", ")
                                        .into_any_element()
                                });
                                let link = match artist_id {
                                    Some(id) => div()
                                        .id(("album-artist", i))
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(cx.theme().accent))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(AlbumDetailEvent::OpenArtist(id.clone()));
                                        }))
                                        .child(artist)
                                        .into_any_element(),
                                    None => div().child(artist).into_any_element(),
                                };
                                sep.into_iter().chain(std::iter::once(link))
                            },
                        )),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .map(|this| match header_pending {
                            true => {
                                this.child(skeleton_text("al-meta-sk", show_album, META_SAMPLE, cx))
                            }
                            false => this.child(meta),
                        }),
                )
                .child(chip_row)
                .map(|this| match &replaygain {
                    Some(line) => this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(line.clone()),
                    ),
                    // Reserved whether or not this album turns out to carry gain
                    // tags. They live on the songs, which the cache does not
                    // keep, so the line is `getAlbum`-only — and a line that
                    // *disappears* when the response says "no tags" moves the
                    // page exactly as much as one that appears. Past the delay
                    // it is grey; loaded, it is an empty line of the same
                    // metrics, which costs an album without gain tags one faint
                    // blank row and costs every album the step.
                    None => this.child(div().text_xs().child(skeleton_text(
                        "al-rg-sk",
                        show_album,
                        REPLAYGAIN_SAMPLE,
                        cx,
                    ))),
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
                                .on_click(cx.listener(|this, _, _, cx| this.play_from(0, cx)));
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
                                .on_click(cx.listener(|this, _, _, cx| this.play_shuffled(cx))),
                        ),
                );

            match panel.is_some() {
                // In the panel the cover leads and the details read down under
                // it: there is no width to put them side by side in, and the
                // cover is the reason the layout was chosen.
                true => v_flex().gap_4().child(cover).child(info).into_any_element(),
                // Centred, not top-aligned: the info column's height depends on
                // how many chips and lines this album has, so a fixed-height
                // cover pinned to the top leaves the card visibly lopsided —
                // most of all against the header's colour wash.
                false => h_flex()
                    .gap_4()
                    .items_center()
                    .flex_wrap()
                    .child(cover)
                    // Grow and shrink, but *not* `flex_1`: that sets the flex
                    // basis to 0%, and the column's height is then measured at
                    // its min-content width, where the chip row stacks one chip
                    // per line. The card takes that height (see
                    // `content_width`), so in a narrow window it kept a gap even
                    // with the card's own width pinned. An auto basis measures
                    // at the content's natural width instead, and the shrink
                    // brings it back to the room the cover leaves. In the panel
                    // the column has no row to share, so none of it applies.
                    .child(info.flex_grow().flex_shrink().min_w(px(260.)))
                    .into_any_element(),
            }
        };

        let info_prefs = self.session.read(cx).settings.track_info.clone();
        let glow = self.session.read(cx).settings.selection_glow;

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
                        s.anchor_scroll(Some(self.focus_anchor.clone()))
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
                with_focus_cursor(
                    format!("vi-focus-{i}"),
                    row,
                    self.vi_cursor == Some(i),
                    glow,
                    cx,
                )
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
                .into_any_element()
        });
        // `getAlbumInfo2` is a second round trip, and the card it fills cannot
        // be placeheld accurately even in principle: the prose is an unknown
        // number of words wrapped in an unknown column, the More button exists
        // only when it runs past `NOTES_PREVIEW_CHARS`, the links exist only
        // when the server has them, and a server with no metadata agent answers
        // with nothing at all and the card never appears. Every one of those is
        // a height nothing can predict — so in the **stacked** layout the card
        // is drawn *below* the track list instead (see `scroll`), where its
        // arrival has nothing above it to push and no placeholder is needed.
        // The panel layout keeps it in the panel, where it has always been and
        // where it can only push itself, and there the placeholder is still
        // worth holding.
        let about = about.or_else(|| {
            (loading_info && panel.is_some()).then(|| {
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_2()
                    .bg(cx.theme().sidebar)
                    .child(skeleton_text("al-about-head-sk", show_info, "About", cx))
                    .child(
                        // The notes are text_sm prose, so the block is laid out
                        // in the same styles and at the same character count
                        // the collapsed paragraph will be.
                        div().text_sm().child(skeleton_block(
                            "al-about-sk",
                            show_info,
                            &notes_sample(),
                            cx,
                        )),
                    )
                    // The two rows under the prose are the rest of the card's
                    // height, and leaving them out meant the card still grew by
                    // a button and a line of links when the request landed —
                    // which in the stacked layout is the track list stepping
                    // down. Held as invisible copies of the real controls,
                    // since it is their own metrics that are wanted.
                    .child(
                        h_flex().invisible().child(
                            Button::new("notes-toggle-sk")
                                .ghost()
                                .xsmall()
                                .label("More")
                                .icon(Icon::new(IconName::ChevronDown)),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .text_sm()
                            .child(skeleton_text("al-mb-link-sk", show_info, "MusicBrainz", cx))
                            .child(skeleton_text("al-lastfm-link-sk", show_info, "Last.fm", cx)),
                    )
                    .into_any_element()
            })
        });

        // Header card matches the artist page framing.
        let header_card = v_flex()
            .when(header_w > 0., |this| this.w(px(header_w)))
            .flex_none()
            .rounded_2xl()
            .p_4()
            .gap_4()
            // The album's colour washes across the header card and fades back
            // into the normal surface, so the page reads as this album's
            // without the track list losing contrast.
            .map(|this| match header_tint {
                Some(accent) => this.bg(linear_gradient(
                    160.,
                    linear_color_stop(crate::ui::page_tint(accent), 0.),
                    linear_color_stop(cx.theme().sidebar, 0.85),
                )),
                None => this.bg(cx.theme().sidebar),
            })
            .child(header);

        let error_line = self
            .error
            .clone()
            .map(|e| div().text_color(cx.theme().danger).text_sm().child(e));
        // Rows of the same height and columns as the real ones, for the album
        // the cache could not seed: an empty page that sprouts a track list
        // reads as a failure until it does.
        let track_list = match rows.is_empty() && loading_album {
            true => v_flex().gap_0p5().children(
                (0..self.expected_tracks.unwrap_or(TRACK_SKELETONS)).map(|i| {
                    // The real row's padding, gaps and columns, down to an
                    // invisible copy of its hover buttons: those are what set its
                    // height, and a row guessed at in px was a pixel or two off
                    // every one of them — eight rows of which is a visible jump.
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_3()
                        .items_center()
                        .child(div().w(px(28.)).text_sm().child(skeleton_text(
                            ("t-no-sk", i),
                            show_album,
                            "1",
                            cx,
                        )))
                        .child(div().flex_1().min_w_0().child(skeleton_line(
                            ("t-title-sk", i),
                            show_album,
                            // Varied so the column reads as titles rather than
                            // as a block; deterministic so it does not
                            // reshuffle on every repaint.
                            relative(0.35 + ((i * 37) % 40) as f32 / 100.),
                            cx,
                        )))
                        .child(
                            div().invisible().child(
                                Button::new(("t-sk-h", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(app_icon(icons::STAR_OUTLINE)),
                            ),
                        )
                        .child(
                            h_flex()
                                .w(px(44.))
                                .flex_none()
                                .justify_end()
                                .text_sm()
                                .child(skeleton_text(("t-dur-sk", i), show_album, "3:41", cx)),
                        )
                }),
            ),
            false => v_flex().gap_0p5().children(rows),
        };

        let scroll = match panel {
            // Track list on one side, cover and details in their own column on
            // the other. Two scrolling columns, not one: a long album scrolled
            // under a panel that stays put is the reason for the layout.
            Some(panel) => {
                let tracks = v_flex()
                    .id("album-detail-scroll")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px_4()
                    .pb_4()
                    // Not `p_4`: the panel's cover sits a header card's padding
                    // inside the panel's own, so a track list padded the same
                    // starts above the cover beside it (`SIDE_PANEL_TRACKS_TOP`).
                    .pt(px(crate::ui::SIDE_PANEL_TRACKS_TOP))
                    .gap_4()
                    .children(error_line)
                    .child(track_list);
                let side = v_flex()
                    .id("album-side-panel")
                    .w(px(panel.width))
                    .flex_none()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.panel_scroll)
                    .p_4()
                    .gap_4()
                    .child(header_card)
                    .children(about);
                let row = h_flex().size_full().items_start();
                // Which column leads is `Settings::album_panel_right`, off by
                // default; nothing else about either one changes with it.
                match panel_right {
                    true => row.child(tracks).child(side),
                    false => row.child(side).child(tracks),
                }
                .into_any_element()
            }
            None => v_flex()
                .id("album-detail-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .p_4()
                .gap_4()
                .child(header_card)
                .children(error_line)
                .child(track_list)
                // Last, not under the header: this card's height is unknowable
                // until `getAlbumInfo2` lands, and anything of unknowable height
                // above the track list is the track list stepping down. At the
                // end of the column it grows into empty page.
                .children(about)
                .into_any_element(),
        };

        div()
            .relative()
            .size_full()
            // Opacity, not `hidden()`: the latter is `display: none`, which
            // skips the very layout pass this frame exists to produce.
            .when(unmeasured, |this| this.opacity(0.))
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
    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
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
    use super::{
        album_credits, album_from_row, cached_album, fmt_bytes, fmt_khz, quality_chips,
        replaygain_line,
    };
    use crate::services::library_db::{AlbumRow, LibraryDb};
    use subsonic::{ArtistRef, Song};

    fn album(artist: Option<&str>, artists: Vec<(&str, &str)>) -> subsonic::Album {
        let mut album = album_from_row(AlbumRow::new("navidrome:album:a1", "navidrome", "Album"));
        album.artist = artist.map(str::to_string);
        album.artist_id = artist.map(|_| "ar-1".to_string());
        album.artists = artists
            .into_iter()
            .map(|(id, name)| ArtistRef {
                id: id.into(),
                name: name.into(),
            })
            .collect();
        album
    }

    /// The whole point: a collaboration gets one link per artist. Linking the
    /// display string instead sent every name on the line to `artistId`, i.e.
    /// to whichever artist the server happened to list first.
    #[test]
    fn each_credited_artist_gets_its_own_id() {
        let credits = album_credits(&album(
            Some("Jay-Z"),
            vec![("ar-1", "Jay-Z"), ("ar-2", "Ye")],
        ));
        assert_eq!(
            credits,
            [
                ("Jay-Z".to_string(), Some("ar-1".to_string())),
                ("Ye".to_string(), Some("ar-2".to_string())),
            ]
        );
    }

    #[test]
    fn a_server_without_the_array_still_links_its_one_artist() {
        let credits = album_credits(&album(Some("The Beatles"), vec![]));
        assert_eq!(
            credits,
            [("The Beatles".to_string(), Some("ar-1".to_string()))]
        );
    }

    /// No artist at all renders nothing, rather than an empty link sitting in
    /// the header waiting to be clicked.
    #[test]
    fn an_uncredited_album_yields_no_links() {
        assert!(album_credits(&album(None, vec![])).is_empty());
        assert!(album_credits(&album(Some("  "), vec![])).is_empty());
    }

    /// The sync writes `navidrome:album:<id>` / `navidrome:track:<id>`, the
    /// grid navigates with the bare id — so the seed has to qualify its lookup
    /// and unqualify what it hands back. Missing the first half made every
    /// lookup miss, and the page that was supposed to paint from cache waited
    /// on `getAlbum` and a fresh cover download instead.
    #[test]
    fn the_seed_crosses_the_syncs_id_namespace() {
        let db = LibraryDb::open_in_memory().unwrap();
        let mut row = AlbumRow::new("navidrome:album:a1", "navidrome", "Album");
        row.cover_art = Some("al-a1_deadbeef".into());
        db.upsert_album(&row).unwrap();
        db.upsert_track(
            "navidrome:track:t1",
            "navidrome",
            "Track",
            None,
            None,
            Some("Album"),
            Some("navidrome:album:a1"),
            None,
            Some(1),
            None,
            None,
            None,
            None,
            None,
            Some("mf-t1_deadbeef"),
            None,
        )
        .unwrap();

        let seeded = cached_album(&db, "a1").expect("bare id finds the namespaced row");
        assert_eq!(seeded.album.id, "a1");
        assert_eq!(seeded.album.cover_art.as_deref(), Some("al-a1_deadbeef"));
        assert_eq!(seeded.song.len(), 1);
        assert_eq!(seeded.song[0].id, "t1");
        // Grouped under the album so all its rows share one cover download.
        assert_eq!(seeded.song[0].album_id.as_deref(), Some("a1"));
    }

    /// A row whose tracks never landed seeds nothing: an empty track list under
    /// a filled-in header reads as a broken page, not a loading one.
    #[test]
    fn an_album_without_cached_tracks_does_not_seed() {
        let db = LibraryDb::open_in_memory().unwrap();
        db.upsert_album(&AlbumRow::new("navidrome:album:a1", "navidrome", "Album"))
            .unwrap();
        assert!(cached_album(&db, "a1").is_none());
    }

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
