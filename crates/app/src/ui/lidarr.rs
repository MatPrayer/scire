//! Lidarr views: the sidebar's Lidarr page (queue, wanted, history), a Lidarr
//! album's page (tracks, automatic and interactive search), and the artist
//! page's missing-releases section.
//!
//! Everything here reads the shared [`LidarrState`] for the connection, the
//! queue and the searches in flight; what only one page needs (an artist's
//! albums, interactive results) is fetched by that page.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle,
    SharedString, Task, Window, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::progress::Progress;
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use crate::assets::{app_icon, icons};
use crate::services::lidarr::{self, Album, HistoryItem, Lidarr, QueueItem, Release, Track};
use crate::services::{artwork, runtime};
use crate::state::lidarr::{Connection, LidarrState, lidarr as lidarr_state};
use crate::state::session::Session;
use crate::ui::{sync_focus_scroll, with_focus_cursor};

/// Cover edge on the Lidarr page's rows.
pub(super) const ROW_COVER: f32 = 40.;
/// Cover edge on a Lidarr album's page.
pub(super) const HEADER_COVER: f32 = 200.;

// ---------------------------------------------------------------------------
// Covers

fn cover_key(album: &Album) -> String {
    if album.foreign_album_id.is_empty() {
        format!("lidarr-{}", album.id)
    } else {
        format!("lidarr-{}", album.foreign_album_id)
    }
}

/// Lidarr album covers, by Lidarr album id. One attempt per album per view:
/// a release with no art on Cover Art Archive stays a plain well.
#[derive(Default)]
pub(super) struct Covers {
    paths: HashMap<i64, PathBuf>,
    tasks: HashMap<i64, Task<()>>,
    tried: HashSet<i64>,
}

impl Covers {
    pub(super) fn get(&self, album_id: i64) -> Option<PathBuf> {
        self.paths.get(&album_id).cloned()
    }

    pub(super) fn pending(&self, album_id: i64) -> bool {
        self.tasks.contains_key(&album_id)
    }

    /// Starts the download for `album` unless it is cached or was tried.
    /// `get` finds this `Covers` in the view again when the answer lands.
    pub(super) fn want<V: 'static>(
        &mut self,
        client: &Lidarr,
        album: &Album,
        size: u32,
        get: fn(&mut V) -> &mut Covers,
        cx: &mut Context<V>,
    ) {
        let id = album.id;
        if !self.tried.insert(id) {
            return;
        }
        let key = cover_key(album);
        if let Some(path) = artwork::cached_best(&key, size) {
            self.paths.insert(id, path);
            return;
        }
        let Some((url, remote)) = album.cover() else {
            return;
        };
        let (url, header) = if remote {
            (lidarr::cover_thumbnail_url(url, size), None)
        } else {
            (
                client.absolute(url),
                Some(("X-Api-Key", client.api_key().to_string())),
            )
        };
        let fetch = artwork::fetch_url(url, header, key, size);
        let task = cx.spawn(async move |this, cx| {
            let result = fetch.await;
            let _ = this.update(cx, |view, cx| {
                let covers = get(view);
                covers.tasks.remove(&id);
                match result {
                    Ok(path) => {
                        covers.paths.insert(id, path);
                    }
                    Err(e) => tracing::debug!("lidarr cover {id}: {e:#}"),
                }
                cx.notify();
            });
        });
        self.tasks.insert(id, task);
    }
}

pub(super) fn cover_well(path: Option<PathBuf>, edge: f32, loading: bool, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .size(px(edge))
        .rounded_md()
        .overflow_hidden()
        .bg(cx.theme().muted)
        .when_some(path, |this, path| {
            this.child(img(path).size(px(edge)).rounded_md())
        })
        .when(loading, |this| {
            this.child(crate::ui::skeleton_fill(
                "lidarr-cover-sk",
                div().rounded_md(),
                cx,
            ))
        })
}

pub(super) fn muted_line(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .truncate()
        .child(text.into())
}

/// `2026-09-30T12:34:56Z` → `2026-09-30 12:34` (as sent, no time zone maths,
/// like the rest of the app's dates).
fn short_stamp(raw: &str) -> String {
    let (date, time) = raw.split_once('T').unwrap_or((raw, ""));
    let hm = time.get(..5).unwrap_or("");
    if hm.is_empty() {
        date.to_string()
    } else {
        format!("{date} {hm}")
    }
}

/// What a missing or wanted album is doing right now, if anything.
pub(super) fn album_activity(state: &LidarrState, album_id: i64) -> Option<(String, bool)> {
    if let Some(q) = state.queued_album(album_id) {
        let pct = (q.progress() * 100.).round() as u32;
        let label = match q.state_label() {
            "Downloading" => format!("Downloading {pct}%"),
            other => other.to_string(),
        };
        return Some((label, q.has_problem()));
    }
    state
        .searching
        .contains(&album_id)
        .then(|| ("Searching…".to_string(), false))
}

// ---------------------------------------------------------------------------
// Artist page: missing releases

pub enum MissingEvent {
    OpenAlbum(i64),
}

enum MissingLoad {
    Idle,
    Loading,
    NotInLidarr,
    Failed(String),
    Loaded {
        artist: lidarr::Artist,
        albums: Vec<Album>,
    },
}

fn missing_load(found: lidarr::Discography) -> MissingLoad {
    match found {
        Some((artist, albums)) => MissingLoad::Loaded { artist, albums },
        None => MissingLoad::NotInLidarr,
    }
}

/// The artist page's "Missing releases" section: Lidarr albums for this
/// artist that the library does not have. A child view of the artist page,
/// which hands it the artist and the albums it already shows via
/// [`MissingReleases::set_context`] from its own `render`.
pub struct MissingReleases {
    name: String,
    mbid: Option<String>,
    /// (MusicBrainz id, title) of every album the artist page shows.
    owned: Vec<(Option<String>, String)>,
    width: Option<f32>,
    /// What the current answer was asked for: (name, mbid, Lidarr base).
    loaded_for: Option<(String, Option<String>, String)>,
    load: MissingLoad,
    /// The next lookup skips the disk cache: an import or a retry asked.
    refresh: bool,
    imports_seen: u64,
    covers: Covers,
    task: Option<Task<()>>,
    action_error: Option<String>,
}

impl EventEmitter<MissingEvent> for MissingReleases {}

impl MissingReleases {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = lidarr_state(cx);
        let imports_seen = state.read(cx).imports;
        cx.observe(&state, |this: &mut Self, state, cx| {
            // An import may have filled one of ours in: ask again.
            let imports = state.read(cx).imports;
            if imports != this.imports_seen {
                this.imports_seen = imports;
                this.loaded_for = None;
                this.refresh = true;
            }
            cx.notify();
        })
        .detach();
        Self {
            name: String::new(),
            mbid: None,
            owned: Vec::new(),
            width: None,
            loaded_for: None,
            load: MissingLoad::Idle,
            refresh: false,
            imports_seen,
            covers: Covers::default(),
            task: None,
            action_error: None,
        }
    }

    /// From the artist page's `render`: who this is, what it already shows,
    /// and how big to draw. Starts a lookup when the artist (or a newly
    /// arrived MusicBrainz id) calls for one; never notifies.
    pub fn set_context(
        &mut self,
        name: &str,
        mbid: Option<&str>,
        owned: Vec<(Option<String>, String)>,
        tile: f32,
        width: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        self.name = name.to_string();
        self.mbid = mbid.filter(|m| !m.is_empty()).map(str::to_string);
        self.owned = owned;
        self.width = width;
        self.ensure_loaded(cx);
        // The cards are drawn by the artist page, so their covers are asked
        // for here rather than from this view's `render`.
        if let Some(client) = lidarr_state(cx).read(cx).client.clone() {
            let art_px = (tile * 2.) as u32;
            for album in self.missing() {
                self.covers
                    .want(&client, &album, art_px, |v: &mut Self| &mut v.covers, cx);
            }
        }
    }

    /// Lidarr's albums for this artist that the page does not show, newest
    /// first (the order the page's sections use). Empty until loaded.
    pub fn missing(&self) -> Vec<Album> {
        let MissingLoad::Loaded { albums, .. } = &self.load else {
            return Vec::new();
        };
        let mut missing = lidarr::missing_albums(albums.clone(), &self.owned);
        missing.sort_by(|a, b| b.release_day().cmp(&a.release_day()));
        missing
    }

    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        let Some(client) = lidarr_state(cx).read(cx).client.clone() else {
            return;
        };
        if self.name.is_empty() {
            return;
        }
        let want = (
            self.name.clone(),
            self.mbid.clone(),
            client.base().to_string(),
        );
        if let Some(had) = &self.loaded_for {
            if *had == want {
                return;
            }
            // Only the MusicBrainz id arrived (the artist info answers after
            // the artist): a name match to the same artist stands.
            let same_artist = had.0 == want.0
                && had.2 == want.2
                && match &self.load {
                    MissingLoad::Loaded { artist, .. } => {
                        Some(artist.foreign_artist_id.as_str()) == want.1.as_deref()
                    }
                    MissingLoad::Loading => true,
                    _ => false,
                };
            if same_artist {
                self.loaded_for = Some(want);
                return;
            }
        }
        self.loaded_for = Some(want);
        if !matches!(self.load, MissingLoad::Loaded { .. }) {
            self.load = MissingLoad::Loading;
        }
        let (name, mbid) = (self.name.clone(), self.mbid.clone());
        let use_cache = !std::mem::take(&mut self.refresh);
        self.task = Some(cx.spawn(async move |this, cx| {
            // Lidarr's answer from an earlier visit paints at once; asked
            // again only once it is older than `DISCOGRAPHY_TTL`.
            let mut painted = false;
            if use_cache {
                let (base, mbid, name) = (client.base().to_string(), mbid.clone(), name.clone());
                let cached = runtime::spawn_blocking_io(move || {
                    Ok(lidarr::cached_discography(&base, mbid.as_deref(), &name))
                })
                .await
                .ok()
                .flatten();
                if let Some((found, fresh)) = cached {
                    painted = true;
                    let _ = this.update(cx, |this, cx| {
                        this.load = missing_load(found);
                        cx.notify();
                    });
                    if fresh {
                        return;
                    }
                }
            }
            let result =
                runtime::spawn_io(async move { client.discography(mbid.as_deref(), &name).await })
                    .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(found) => this.load = missing_load(found),
                    // A stale answer on screen beats an error in its place.
                    Err(e) if painted => tracing::warn!("lidarr: refreshing discography: {e:#}"),
                    Err(e) => this.load = MissingLoad::Failed(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        }));
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.loaded_for = None;
        self.refresh = true;
        self.load = MissingLoad::Loading;
        self.ensure_loaded(cx);
        cx.notify();
    }

    fn search(&mut self, album_id: i64, cx: &mut Context<Self>) {
        let task = lidarr_state(cx).update(cx, |s, cx| s.search_album(album_id, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.action_error = result.err().map(|e| crate::errors::error_text(&e));
                cx.notify();
            });
        })
        .detach();
    }

    /// One missing release, drawn by the artist page among its own cards in
    /// the section the release's Lidarr types file it under: the same card or
    /// gallery tile as the albums beside it, cover dimmed, with what Lidarr is
    /// doing about it in place of the year.
    pub fn card(
        entity: &Entity<Self>,
        album: &Album,
        tile: f32,
        gallery: bool,
        flush: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let this = entity.read(cx);
        let state = lidarr_state(cx).read(cx);
        let id = album.id;
        let art = this.covers.get(id);
        let loading = art.is_none() && this.covers.pending(id);
        let year = album.year().unwrap_or_default().to_string();
        let searching = state.searching.contains(&id) || state.queued_album(id).is_some();
        let (status, color) = match album_activity(state, id) {
            Some((label, true)) => (label, cx.theme().warning),
            Some((label, false)) => (label, cx.theme().primary),
            None if !album.monitored => ("Not monitored".into(), cx.theme().muted_foreground),
            None => ("Not in library".into(), cx.theme().muted_foreground),
        };
        let subtitle: SharedString = if year.is_empty() {
            status.into()
        } else {
            format!("{year} · {status}").into()
        };
        let open = {
            let entity = entity.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                entity.update(cx, |_, cx| cx.emit(MissingEvent::OpenAlbum(id)));
            }
        };
        let search = (!searching).then(|| {
            let entity = entity.clone();
            Button::new(("lidarr-missing-search", id as u64))
                .primary()
                .map(|b| if gallery { b.xsmall() } else { b.small() })
                .icon(app_icon(icons::DOWNLOAD))
                .tooltip("Search automatically")
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.search(id, cx));
                    cx.stop_propagation();
                })
        });
        // Dimmed: these are records the library does not hold, and should not
        // read as playable ones.
        const DIM: f32 = 0.5;
        if gallery {
            return crate::ui::gallery_tile(
                ("lidarr-missing", id as u64),
                tile,
                art,
                album.title.clone(),
                subtitle,
                search.map(IntoElement::into_any_element),
                loading,
                cx,
            )
            .opacity(DIM)
            .hover(|s| s.opacity(0.85))
            .on_click(open)
            .into_any_element();
        }
        let cover = crate::ui::card_cover_edge(tile, flush);
        v_flex()
            .id(("lidarr-missing", id as u64))
            .group("lidarr-missing-card")
            .flex_none()
            .w(px(tile + crate::ui::card_padding()))
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .border_dashed()
            .when(!flush, |c| c.p(px(crate::ui::card_inset())))
            .gap_1p5()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .active(|s| s.opacity(0.8))
            .on_click(open)
            .child(
                div()
                    .relative()
                    .size(px(cover))
                    .child(
                        crate::ui::cover_rounding(
                            div()
                                .size(px(cover))
                                .overflow_hidden()
                                .bg(cx.theme().muted)
                                .when_some(art, |this, path| {
                                    this.child(crate::ui::cover_rounding(
                                        img(path).size(px(cover)),
                                        flush,
                                    ))
                                })
                                .when(loading, |this| {
                                    this.child(crate::ui::skeleton_fill(
                                        "lidarr-cover-sk",
                                        crate::ui::cover_rounding(div(), flush),
                                        cx,
                                    ))
                                }),
                            flush,
                        )
                        .opacity(DIM)
                        .group_hover("lidarr-missing-card", |s| s.opacity(0.85)),
                    )
                    .when_some(search, |this, button| {
                        this.child(
                            div()
                                .absolute()
                                .bottom_2()
                                .right_2()
                                .opacity(0.)
                                .group_hover("lidarr-missing-card", |s| s.opacity(1.))
                                .child(button),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_0()
                    .when(flush, |t| {
                        t.px(px(crate::ui::card_inset()))
                            .pb(px(crate::ui::card_inset()))
                    })
                    .child(
                        div()
                            .text_sm()
                            .line_height(px(20.))
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(album.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(17.))
                            .text_color(color)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .into_any_element()
    }
}

impl Render for MissingReleases {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state_entity = lidarr_state(cx);
        let Some(client) = state_entity.read(cx).client.clone() else {
            return div().into_any_element();
        };
        let missing = self.missing();
        let muted = cx.theme().muted_foreground;
        let body: gpui::AnyElement = match &self.load {
            MissingLoad::Idle | MissingLoad::Loading => div()
                .text_sm()
                .text_color(muted)
                .child("Asking Lidarr…")
                .into_any_element(),
            MissingLoad::NotInLidarr => {
                let url = client.base().to_string();
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("Lidarr doesn't track {}.", self.name)),
                    )
                    .child(
                        Button::new("lidarr-open-add")
                            .ghost()
                            .small()
                            .label("Add it in Lidarr")
                            .on_click(move |_, _, cx| cx.open_url(&format!("{url}/add/new"))),
                    )
                    .into_any_element()
            }
            MissingLoad::Failed(why) => h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(why.clone()),
                )
                .child(
                    Button::new("lidarr-missing-retry")
                        .ghost()
                        .small()
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                )
                .into_any_element(),
            MissingLoad::Loaded { .. } if missing.is_empty() => div()
                .text_sm()
                .text_color(muted)
                .child("Nothing missing — the library has every release Lidarr lists.")
                .into_any_element(),
            MissingLoad::Loaded { .. } => div()
                .text_sm()
                .text_color(muted)
                .child("Shown dimmed among the releases above, each in its own section.")
                .into_any_element(),
        };
        let count = (!missing.is_empty()).then(|| missing.len().to_string());
        let artist_link = match &self.load {
            MissingLoad::Loaded { artist, .. } if !artist.foreign_artist_id.is_empty() => Some(
                format!("{}/artist/{}", client.base(), artist.foreign_artist_id),
            ),
            _ => None,
        };
        v_flex()
            .when_some(self.width, |this, w| this.w(px(w)))
            .flex_none()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_medium().child("Missing releases"))
                    .when_some(count, |this, n| {
                        this.child(div().text_xs().text_color(muted).child(n))
                    })
                    .child(div().flex_1())
                    .when_some(artist_link, |this, url| {
                        this.child(
                            Button::new("lidarr-artist-open")
                                .ghost()
                                .xsmall()
                                .label("Open in Lidarr")
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .when_some(self.action_error.clone(), |this, why| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(why))
            })
            .child(body)
            .into_any_element()
    }
}

/// The artist page's switch row for the section above.
pub fn missing_toggle(
    on: bool,
    on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            app_icon(icons::DOWNLOAD)
                .small()
                .text_color(cx.theme().muted_foreground),
        )
        .child(div().text_sm().child("Show missing releases"))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("from Lidarr"),
        )
        .child(
            Switch::new("lidarr-show-missing")
                .checked(on)
                .small()
                .on_click(on_toggle),
        )
}

// ---------------------------------------------------------------------------
// A Lidarr album's page

enum Releases {
    NotAsked,
    Loading(Instant),
    Loaded(Vec<Release>),
    Failed(String),
}

#[derive(Clone, PartialEq)]
enum Grab {
    Sending,
    Sent,
    Failed(String),
}

pub enum LidarrAlbumEvent {
    OpenArtist(i64),
}

impl EventEmitter<LidarrAlbumEvent> for LidarrAlbumView {}

pub struct LidarrAlbumView {
    session: Entity<Session>,
    album_id: i64,
    album: Option<Album>,
    tracks: Option<Vec<Track>>,
    error: Option<String>,
    covers: Covers,
    releases: Releases,
    grabs: HashMap<String, Grab>,
    action_error: Option<String>,
    /// Rejected releases are folded away until asked for, like Lidarr's own
    /// table sorts them last.
    show_rejected: bool,
    imports_seen: u64,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// 0 = automatic search, 1 = interactive search, then release rows.
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
    _load: Option<Task<()>>,
    _search: Option<Task<()>>,
}

impl LidarrAlbumView {
    pub fn new(session: Entity<Session>, album_id: i64, cx: &mut Context<Self>) -> Self {
        let state = lidarr_state(cx);
        let imports_seen = state.read(cx).imports;
        cx.observe(&state, |this: &mut Self, state, cx| {
            let imports = state.read(cx).imports;
            if imports != this.imports_seen {
                this.imports_seen = imports;
                this.load(cx);
            }
            cx.notify();
        })
        .detach();
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            album_id,
            album: None,
            tracks: None,
            error: None,
            covers: Covers::default(),
            releases: Releases::NotAsked,
            grabs: HashMap::new(),
            action_error: None,
            show_rejected: false,
            imports_seen,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            _load: None,
            _search: None,
        };
        this.load(cx);
        this
    }

    fn client(&self, cx: &App) -> Option<Lidarr> {
        lidarr_state(cx).read(cx).client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.error = Some("Lidarr is not connected.".into());
            return;
        };
        let id = self.album_id;
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let (album, tracks) = tokio::join!(client.album(id), client.tracks(id));
                Ok((album?, tracks?))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((album, tracks)) => {
                        this.album = Some(album);
                        this.tracks = Some(tracks);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        }));
    }

    fn search_automatic(&mut self, cx: &mut Context<Self>) {
        let id = self.album_id;
        let task = lidarr_state(cx).update(cx, |s, cx| s.search_album(id, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.action_error = result.err().map(|e| crate::errors::error_text(&e));
                cx.notify();
            });
        })
        .detach();
    }

    fn search_interactive(&mut self, cx: &mut Context<Self>) {
        if matches!(self.releases, Releases::Loading(_)) {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id;
        self.releases = Releases::Loading(Instant::now());
        self.grabs.clear();
        cx.notify();
        self._search = Some(cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move { client.releases(id).await }).await;
            let _ = this.update(cx, |this, cx| {
                this.releases = match result {
                    Ok(releases) => Releases::Loaded(releases),
                    Err(e) => Releases::Failed(crate::errors::error_text(&e)),
                };
                cx.notify();
            });
        }));
        // Ticks the "asking for Ns" line while the indexers think.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let Ok(waiting) = this.update(cx, |this, cx| {
                    cx.notify();
                    matches!(this.releases, Releases::Loading(_))
                }) else {
                    return;
                };
                if !waiting {
                    return;
                }
            }
        })
        .detach();
    }

    fn grab(&mut self, release: Release, cx: &mut Context<Self>) {
        if matches!(
            self.grabs.get(&release.guid),
            Some(Grab::Sending | Grab::Sent)
        ) {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let guid = release.guid.clone();
        self.grabs.insert(guid.clone(), Grab::Sending);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move { client.grab(&release).await }).await;
            let _ = this.update(cx, |this, cx| {
                this.grabs.insert(
                    guid,
                    match result {
                        Ok(()) => Grab::Sent,
                        Err(e) => Grab::Failed(crate::errors::error_text(&e)),
                    },
                );
                lidarr_state(cx).update(cx, |s, cx| s.poke(cx));
                cx.notify();
            });
        })
        .detach();
    }

    fn set_monitored(&mut self, monitored: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.album_id;
        if let Some(album) = self.album.as_mut() {
            album.monitored = monitored;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(async move { client.set_monitored(id, monitored).await }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    if let Some(album) = this.album.as_mut() {
                        album.monitored = !monitored;
                    }
                    this.action_error = Some(crate::errors::error_text(&e));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Releases in table order, honouring the rejected fold.
    fn visible_releases(&self) -> Vec<Release> {
        match &self.releases {
            Releases::Loaded(all) => all
                .iter()
                .filter(|r| self.show_rejected || r.approved)
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = 2 + self.visible_releases().len();
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        cx.notify();
    }

    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        match self.vi_cursor {
            Some(0) => self.search_automatic(cx),
            Some(1) => self.search_interactive(cx),
            Some(i) => {
                if let Some(release) = self.visible_releases().get(i - 2).cloned() {
                    self.grab(release, cx);
                }
            }
            None => {}
        }
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    fn render_track(&self, track: &Track, cx: &Context<Self>) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .px_2()
            .py_1()
            .gap_3()
            .items_center()
            .rounded_md()
            .text_sm()
            .child(
                div()
                    .w(px(32.))
                    .flex_none()
                    .text_right()
                    .text_color(muted)
                    .child(if track.track_number.is_empty() {
                        track.absolute_track_number.to_string()
                    } else {
                        track.track_number.clone()
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(track.title.clone()),
            )
            .when(track.explicit, |this| {
                this.child(div().text_xs().text_color(muted).child("E"))
            })
            .when(track.has_file, |this| {
                this.child(
                    Icon::new(IconName::Check)
                        .xsmall()
                        .text_color(cx.theme().success),
                )
            })
            .child(
                div()
                    .w(px(48.))
                    .flex_none()
                    .text_right()
                    .text_color(muted)
                    .child(crate::ui::format_duration(Duration::from_millis(
                        track.duration,
                    ))),
            )
            .into_any_element()
    }

    fn render_release(
        &self,
        index: usize,
        release: &Release,
        focused: bool,
        glow: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let grab = self.grabs.get(&release.guid).cloned();
        let peers = match (release.protocol.as_str(), release.seeders) {
            ("torrent", Some(s)) => format!("{s} seeders"),
            ("usenet", _) => "usenet".to_string(),
            _ => release.protocol.clone(),
        };
        let rejections = release.rejections.join("\n");
        let release_for_grab = release.clone();
        let button: gpui::AnyElement = match &grab {
            Some(Grab::Sending) => Button::new(("lidarr-grab", index))
                .ghost()
                .small()
                .loading(true)
                .into_any_element(),
            Some(Grab::Sent) => Button::new(("lidarr-grab", index))
                .ghost()
                .small()
                .icon(Icon::new(IconName::Check))
                .tooltip("Sent to the download client")
                .into_any_element(),
            _ => Button::new(("lidarr-grab", index))
                .when(release.approved, |b| b.primary())
                .when(!release.approved, |b| b.outline())
                .small()
                .icon(app_icon(icons::DOWNLOAD))
                .tooltip(if release.approved {
                    "Download"
                } else {
                    "Download anyway"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.grab(release_for_grab.clone(), cx);
                }))
                .into_any_element(),
        };
        let failed = match grab {
            Some(Grab::Failed(why)) => Some(why),
            _ => None,
        };
        let title = SharedString::from(release.title.clone());
        let row = v_flex()
            .id(("lidarr-release", index))
            .px_2()
            .py_1p5()
            .gap_0p5()
            .rounded_md()
            .hover(|s| s.bg(cx.theme().muted))
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .w(px(92.))
                            .flex_none()
                            .text_xs()
                            .font_medium()
                            .truncate()
                            .child(release.quality_name().to_string()),
                    )
                    .child(
                        div()
                            .id(("lidarr-release-title", index))
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .when(!release.approved, |s| s.text_color(muted))
                            .child(title.clone())
                            .tooltip(move |window, cx| {
                                Tooltip::new(title.clone()).build(window, cx)
                            }),
                    )
                    .when(!release.approved, |this| {
                        let rejections = SharedString::from(rejections.clone());
                        this.child(
                            div()
                                .id(("lidarr-release-rejected", index))
                                .flex_none()
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .xsmall()
                                        .text_color(cx.theme().warning),
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::new(rejections.clone()).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        div()
                            .w(px(72.))
                            .flex_none()
                            .text_xs()
                            .text_right()
                            .text_color(muted)
                            .child(lidarr::human_size(release.size)),
                    )
                    .child(button),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pl(px(92. + 12.))
                    .text_xs()
                    .text_color(muted)
                    .child(release.indexer.clone())
                    .child("·")
                    .child(peers)
                    .child("·")
                    .child(lidarr::human_age(release.age_hours)),
            )
            .when_some(failed, |this, why| {
                this.child(
                    div()
                        .pl(px(92. + 12.))
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(why),
                )
            });
        with_focus_cursor(
            format!("vi-lidarr-release-{index}"),
            row,
            focused,
            glow,
            None,
            cx,
        )
    }
}

impl Render for LidarrAlbumView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let client = self.client(cx);
        if let (Some(client), Some(album)) = (client.as_ref(), self.album.clone()) {
            self.covers.want(
                client,
                &album,
                (HEADER_COVER * 2.) as u32,
                |v: &mut Self| &mut v.covers,
                cx,
            );
        }
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let muted = cx.theme().muted_foreground;
        let state = lidarr_state(cx);
        let activity = album_activity(state.read(cx), self.album_id);
        let busy = state.read(cx).searching.contains(&self.album_id);

        let Some(album) = self.album.clone() else {
            return v_flex()
                .size_full()
                .p_4()
                .gap_3()
                .child(div().text_lg().child("Lidarr album"))
                .child(match &self.error {
                    Some(why) => h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(why.clone()),
                        )
                        .child(
                            Button::new("lidarr-album-retry")
                                .ghost()
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                        )
                        .into_any_element(),
                    None => div()
                        .text_sm()
                        .text_color(muted)
                        .child("Loading from Lidarr…")
                        .into_any_element(),
                })
                .into_any_element();
        };

        let artist_name = album
            .artist
            .as_ref()
            .map(|a| a.artist_name.clone())
            .unwrap_or_default();
        let tracks = self.tracks.clone().unwrap_or_default();
        let total_ms: u64 = tracks.iter().map(|t| t.duration).sum();
        let have = tracks.iter().filter(|t| t.has_file).count();
        let meta = [
            Some(album.type_label()),
            album.release_day().map(str::to_string),
            (!tracks.is_empty()).then(|| format!("{} tracks", tracks.len())),
            (total_ms > 0).then(|| crate::ui::format_duration(Duration::from_millis(total_ms))),
            (have > 0).then(|| format!("{have} on disk")),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let lidarr_url = client
            .as_ref()
            .map(|c| format!("{}/album/{}", c.base(), album.foreign_album_id));
        let art = self.covers.get(album.id);
        let art_loading = art.is_none() && self.covers.pending(album.id);

        let auto = Button::new("lidarr-auto-search")
            .primary()
            .icon(app_icon(icons::DOWNLOAD))
            .label(if busy {
                "Searching…"
            } else {
                "Search automatically"
            })
            .loading(busy)
            .on_click(cx.listener(|this, _, _, cx| this.search_automatic(cx)));
        let interactive_busy = matches!(self.releases, Releases::Loading(_));
        let interactive = Button::new("lidarr-interactive-search")
            .outline()
            .icon(Icon::new(IconName::Search))
            .label("Interactive search")
            .loading(interactive_busy)
            .on_click(cx.listener(|this, _, _, cx| this.search_interactive(cx)));
        let monitored = album.monitored;

        let header = h_flex()
            .flex_none()
            .w_full()
            .gap_4()
            .items_start()
            .p_4()
            .rounded_2xl()
            .bg(cx.theme().sidebar)
            .child(cover_well(art, HEADER_COVER, art_loading, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(div().text_2xl().font_medium().child(album.title.clone()))
                    .when_some(
                        album.disambiguation.clone().filter(|d| !d.is_empty()),
                        |this, d| this.child(muted_line(d, cx)),
                    )
                    .child({
                        let artist_id = album.artist_id;
                        div()
                            .id("lidarr-album-artist")
                            .text_sm()
                            .when(artist_id > 0, |this| {
                                this.cursor_pointer().hover(|s| s.underline()).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        cx.emit(LidarrAlbumEvent::OpenArtist(artist_id))
                                    }),
                                )
                            })
                            .child(artist_name)
                    })
                    .child(div().text_sm().text_color(muted).child(meta))
                    .when(!album.genres.is_empty(), |this| {
                        this.child(muted_line(album.genres.join(", "), cx))
                    })
                    .when_some(activity.clone(), |this, (label, problem)| {
                        let progress = state
                            .read(cx)
                            .queued_album(album.id)
                            .map(QueueItem::progress);
                        this.child(
                            v_flex()
                                .gap_1()
                                .max_w(px(360.))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(if problem {
                                            cx.theme().warning
                                        } else {
                                            cx.theme().primary
                                        })
                                        .child(label),
                                )
                                .when_some(progress, |this, p| {
                                    this.child(Progress::new().value(p * 100.))
                                }),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .pt_2()
                            .child(with_focus_cursor(
                                "vi-lidarr-auto",
                                div().child(auto),
                                self.vi_cursor == Some(0),
                                glow,
                                None,
                                cx,
                            ))
                            .child(with_focus_cursor(
                                "vi-lidarr-interactive",
                                div().child(interactive),
                                self.vi_cursor == Some(1),
                                glow,
                                None,
                                cx,
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .pl_2()
                                    .child(
                                        Switch::new("lidarr-monitored")
                                            .checked(monitored)
                                            .small()
                                            .on_click(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    this.set_monitored(*checked, cx)
                                                },
                                            )),
                                    )
                                    .child(div().text_sm().child("Monitored")),
                            )
                            .when_some(lidarr_url, |this, url| {
                                this.child(
                                    Button::new("lidarr-album-open")
                                        .ghost()
                                        .small()
                                        .label("Open in Lidarr")
                                        .on_click(move |_, _, cx| cx.open_url(&url)),
                                )
                            }),
                    )
                    .when_some(self.action_error.clone(), |this, why| {
                        this.child(div().text_xs().text_color(cx.theme().danger).child(why))
                    }),
            );

        // Interactive results.
        let releases_section: Option<gpui::AnyElement> = match &self.releases {
            Releases::NotAsked => None,
            Releases::Loading(since) => Some(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!(
                        "Asking the indexers… {}s (this can take a minute)",
                        since.elapsed().as_secs()
                    ))
                    .into_any_element(),
            ),
            Releases::Failed(why) => Some(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(why.clone()),
                    )
                    .child(
                        Button::new("lidarr-releases-retry")
                            .ghost()
                            .small()
                            .label("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.search_interactive(cx))),
                    )
                    .into_any_element(),
            ),
            Releases::Loaded(all) => {
                let rejected = all.iter().filter(|r| !r.approved).count();
                let shown = self.visible_releases();
                let rows: Vec<gpui::AnyElement> = shown
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        self.render_release(i, r, self.vi_cursor == Some(i + 2), glow, cx)
                    })
                    .collect();
                let show_rejected = self.show_rejected;
                Some(
                    v_flex()
                        .gap_0p5()
                        .when(all.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child("No indexer had anything for this album."),
                            )
                        })
                        .when(!all.is_empty() && rows.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child("Lidarr rejected every result."),
                            )
                        })
                        .children(rows)
                        .when(rejected > 0, |this| {
                            this.child(
                                div().pt_1().child(
                                    Button::new("lidarr-show-rejected")
                                        .ghost()
                                        .xsmall()
                                        .label(if show_rejected {
                                            "Hide rejected".to_string()
                                        } else {
                                            format!("Show {rejected} rejected")
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_rejected = !this.show_rejected;
                                            this.vi_cursor = None;
                                            cx.notify();
                                        })),
                                ),
                            )
                        })
                        .into_any_element(),
                )
            }
        };

        v_flex()
            .id("lidarr-album-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            .child(header)
            .when_some(releases_section, |this, section| {
                this.child(
                    v_flex()
                        .flex_none()
                        .gap_2()
                        .child(div().text_sm().font_medium().child("Releases"))
                        .child(section),
                )
            })
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(div().text_sm().font_medium().child("Tracks"))
                    .when(self.tracks.is_none(), |this| {
                        this.child(div().text_sm().text_color(muted).child("Loading…"))
                    })
                    .child(
                        v_flex()
                            .gap_0()
                            .children(tracks.iter().map(|t| self.render_track(t, cx))),
                    ),
            )
            .when_some(
                album.overview.clone().filter(|o| !o.trim().is_empty()),
                |this, o| {
                    this.child(
                        v_flex()
                            .flex_none()
                            .gap_2()
                            .child(div().text_sm().font_medium().child("About"))
                            .child(div().text_sm().text_color(muted).child(o)),
                    )
                },
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// The sidebar's Lidarr page

pub enum LidarrEvent {
    OpenAlbum(i64),
}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

pub struct LidarrView {
    session: Entity<Session>,
    wanted: Load<(u64, Vec<Album>)>,
    history: Load<Vec<HistoryItem>>,
    imports_seen: u64,
    covers: Covers,
    /// Queue entry whose remove button was pressed once: the second press
    /// removes (and cancels the download in the client).
    confirm_remove: Option<i64>,
    action_error: Option<String>,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Queue rows, then wanted rows.
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
    _load: Option<Task<()>>,
}

impl EventEmitter<LidarrEvent> for LidarrView {}

impl LidarrView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let state = lidarr_state(cx);
        let imports_seen = state.read(cx).imports;
        cx.observe(&state, |this: &mut Self, state, cx| {
            let imports = state.read(cx).imports;
            if imports != this.imports_seen {
                this.imports_seen = imports;
                this.load(cx);
            }
            cx.notify();
        })
        .detach();
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            wanted: Load::Loading,
            history: Load::Loading,
            imports_seen,
            covers: Covers::default(),
            confirm_remove: None,
            action_error: None,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            _load: None,
        };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = lidarr_state(cx).read(cx).client.clone() else {
            return;
        };
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let (wanted, history) = tokio::join!(client.wanted_missing(), client.history());
                Ok((wanted, history))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((wanted, history)) => {
                        this.wanted = match wanted {
                            Ok(w) => Load::Loaded(w),
                            Err(e) => Load::Failed(crate::errors::error_text(&e)),
                        };
                        this.history = match history {
                            Ok(h) => Load::Loaded(h),
                            Err(e) => Load::Failed(crate::errors::error_text(&e)),
                        };
                    }
                    Err(e) => {
                        let why = crate::errors::error_text(&e);
                        this.wanted = Load::Failed(why.clone());
                        this.history = Load::Failed(why);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        lidarr_state(cx).update(cx, |s, cx| s.poke(cx));
        self.load(cx);
    }

    fn wanted_albums(&self) -> &[Album] {
        match &self.wanted {
            Load::Loaded((_, albums)) => albums,
            _ => &[],
        }
    }

    fn search(&mut self, album_id: i64, cx: &mut Context<Self>) {
        let task = lidarr_state(cx).update(cx, |s, cx| s.search_album(album_id, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.action_error = result.err().map(|e| crate::errors::error_text(&e));
                cx.notify();
            });
        })
        .detach();
    }

    fn remove(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.confirm_remove != Some(id) {
            self.confirm_remove = Some(id);
            cx.notify();
            return;
        }
        self.confirm_remove = None;
        let task = lidarr_state(cx).update(cx, |s, cx| s.remove_from_queue(id, false, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.action_error = result.err().map(|e| crate::errors::error_text(&e));
                cx.notify();
            });
        })
        .detach();
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = lidarr_state(cx).read(cx).queue.len() + self.wanted_albums().len();
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

    /// Enter: a queue row opens its album; a wanted row too.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.vi_cursor else {
            return;
        };
        let queue_album = {
            let state = lidarr_state(cx);
            let state = state.read(cx);
            state.queue.get(i).map(|q| q.album_id)
        };
        let album_id = match queue_album {
            Some(id) => id,
            None => {
                let queued = lidarr_state(cx).read(cx).queue.len();
                self.wanted_albums().get(i - queued).map(|a| a.id)
            }
        };
        if let Some(id) = album_id {
            cx.emit(LidarrEvent::OpenAlbum(id));
        }
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    fn render_queue_row(
        &self,
        index: usize,
        item: &QueueItem,
        focused: bool,
        glow: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let id = item.id;
        let album_id = item.album_id;
        let artist = item
            .artist
            .as_ref()
            .map(|a| a.artist_name.clone())
            .unwrap_or_default();
        let album_title = item
            .album
            .as_ref()
            .map(|a| a.title.clone())
            .unwrap_or_else(|| item.title.clone());
        let art = album_id.and_then(|a| self.covers.get(a));
        let problem = item.has_problem();
        let messages = item.messages();
        let state_color = if problem {
            cx.theme().warning
        } else {
            cx.theme().primary
        };
        let detail = [
            Some(item.state_label().to_string()),
            (item.size > 0.).then(|| {
                format!(
                    "{} of {}",
                    lidarr::human_size((item.size - item.sizeleft).max(0.) as u64),
                    lidarr::human_size(item.size as u64)
                )
            }),
            item.eta().filter(|_| item.status == "downloading"),
            item.quality.as_ref().map(|q| q.quality.name.clone()),
            item.download_client.clone(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let confirming = self.confirm_remove == Some(id);
        let tip = SharedString::from(if messages.is_empty() {
            item.title.clone()
        } else {
            format!("{}\n\n{}", item.title, messages.join("\n"))
        });
        let row = h_flex()
            .id(("lidarr-queue", index))
            .px_2()
            .py_1p5()
            .gap_3()
            .items_center()
            .rounded_md()
            .hover(|s| s.bg(cx.theme().muted))
            .when(album_id.is_some(), |s| s.cursor_pointer())
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(id) = album_id {
                    cx.emit(LidarrEvent::OpenAlbum(id));
                }
            }))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .child(cover_well(art, ROW_COVER, false, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().truncate().child(album_title))
                            .child(div().text_xs().text_color(muted).truncate().child(artist)),
                    )
                    .child(Progress::new().value(item.progress() * 100.).h(px(4.)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(state_color)
                            .truncate()
                            .child(detail),
                    ),
            )
            .when(problem, |this| {
                this.child(
                    Icon::new(IconName::TriangleAlert)
                        .small()
                        .text_color(cx.theme().warning),
                )
            })
            .child(
                Button::new(("lidarr-queue-remove", index))
                    .when(confirming, |b| b.danger().label("Remove"))
                    .when(!confirming, |b| b.ghost().icon(Icon::new(IconName::Close)))
                    .small()
                    .tooltip(if confirming {
                        "Remove from the queue and the download client"
                    } else {
                        "Remove"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove(id, cx);
                        cx.stop_propagation();
                    })),
            );
        with_focus_cursor(
            format!("vi-lidarr-queue-{index}"),
            row,
            focused,
            glow,
            None,
            cx,
        )
    }

    fn render_wanted_row(
        &self,
        flat: usize,
        album: &Album,
        state: &LidarrState,
        focused: bool,
        glow: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let id = album.id;
        let artist = album
            .artist
            .as_ref()
            .map(|a| a.artist_name.clone())
            .unwrap_or_default();
        let activity = album_activity(state, id);
        let busy = activity.is_some();
        let sub = [
            Some(artist),
            album.release_day().map(str::to_string),
            Some(album.type_label()),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let row = h_flex()
            .id(("lidarr-wanted", flat))
            .px_2()
            .py_1p5()
            .gap_3()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(LidarrEvent::OpenAlbum(id))))
            .child(cover_well(
                self.covers.get(id),
                ROW_COVER,
                self.covers.pending(id),
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().truncate().child(album.title.clone()))
                    .child(div().text_xs().text_color(muted).truncate().child(sub)),
            )
            .when_some(activity, |this, (label, problem)| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(if problem {
                            cx.theme().warning
                        } else {
                            cx.theme().primary
                        })
                        .child(label),
                )
            })
            .child(
                Button::new(("lidarr-wanted-search", flat))
                    .ghost()
                    .small()
                    .icon(app_icon(icons::DOWNLOAD))
                    .tooltip("Search automatically")
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.search(id, cx);
                        cx.stop_propagation();
                    })),
            );
        with_focus_cursor(
            format!("vi-lidarr-wanted-{flat}"),
            row,
            focused,
            glow,
            None,
            cx,
        )
    }
}

pub(super) fn section_title(title: &str, count: Option<String>, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(div().text_sm().font_medium().child(title.to_string()))
        .when_some(count, |this, n| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(n),
            )
        })
}

impl Render for LidarrView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let state_entity = lidarr_state(cx);
        let Some(client) = state_entity.read(cx).client.clone() else {
            return v_flex()
                .size_full()
                .p_4()
                .gap_3()
                .child(div().text_lg().child("Lidarr"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Lidarr is off. Turn it on under Settings → Connections."),
                )
                .into_any_element();
        };
        // Covers for every row on the page (a few dozen at most).
        let queue_albums: Vec<Album> = state_entity
            .read(cx)
            .queue
            .iter()
            .filter_map(|q| q.album.clone())
            .collect();
        let wanted_albums: Vec<Album> = self.wanted_albums().to_vec();
        for album in queue_albums.iter().chain(&wanted_albums) {
            self.covers.want(
                &client,
                album,
                (ROW_COVER * 2.) as u32,
                |v: &mut Self| &mut v.covers,
                cx,
            );
        }

        let glow = self.session.read(cx).settings.selection_glow_vi;
        let muted = cx.theme().muted_foreground;
        let state = state_entity.read(cx);
        let (status, status_color) = match &state.connection {
            Connection::Online { version } if version.is_empty() => {
                ("Connected".to_string(), cx.theme().success)
            }
            Connection::Online { version } => {
                (format!("Connected · Lidarr {version}"), cx.theme().success)
            }
            Connection::Checking | Connection::Off => {
                ("Connecting…".to_string(), cx.theme().warning)
            }
            Connection::Failed(why) => (why.clone(), cx.theme().danger),
        };
        let base = client.base().to_string();

        let queue_rows: Vec<gpui::AnyElement> = state
            .queue
            .iter()
            .enumerate()
            .map(|(i, q)| self.render_queue_row(i, q, self.vi_cursor == Some(i), glow, cx))
            .collect();
        let queued = queue_rows.len();
        let queue_body: gpui::AnyElement = if !state.queue_loaded {
            div()
                .text_sm()
                .text_color(muted)
                .child("Loading…")
                .into_any_element()
        } else if queue_rows.is_empty() {
            div()
                .text_sm()
                .text_color(muted)
                .child("Nothing downloading.")
                .into_any_element()
        } else {
            v_flex().gap_0p5().children(queue_rows).into_any_element()
        };

        let (wanted_total, wanted_body): (Option<u64>, gpui::AnyElement) = match &self.wanted {
            Load::Loading => (
                None,
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Loading…")
                    .into_any_element(),
            ),
            Load::Failed(why) => (
                None,
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(why.clone())
                    .into_any_element(),
            ),
            Load::Loaded((total, albums)) if albums.is_empty() => (
                Some(*total),
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Lidarr isn't missing anything it monitors.")
                    .into_any_element(),
            ),
            Load::Loaded((total, albums)) => (
                Some(*total),
                v_flex()
                    .gap_0p5()
                    .children(albums.iter().enumerate().map(|(i, album)| {
                        let flat = queued + i;
                        self.render_wanted_row(
                            flat,
                            album,
                            state,
                            self.vi_cursor == Some(flat),
                            glow,
                            cx,
                        )
                    }))
                    .into_any_element(),
            ),
        };

        let history_body: gpui::AnyElement = match &self.history {
            Load::Loading => div()
                .text_sm()
                .text_color(muted)
                .child("Loading…")
                .into_any_element(),
            Load::Failed(why) => div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(why.clone())
                .into_any_element(),
            Load::Loaded(items) if items.is_empty() => div()
                .text_sm()
                .text_color(muted)
                .child("No activity yet.")
                .into_any_element(),
            Load::Loaded(items) => v_flex()
                .gap_0p5()
                .children(items.iter().map(|h| {
                    let what = [
                        h.artist.as_ref().map(|a| a.artist_name.clone()),
                        h.album.as_ref().map(|a| a.title.clone()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" — ");
                    let what = if what.is_empty() {
                        h.source_title.clone()
                    } else {
                        what
                    };
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_3()
                        .items_center()
                        .text_sm()
                        .child(
                            div()
                                .w(px(128.))
                                .flex_none()
                                .text_xs()
                                .text_color(if h.is_failure() {
                                    cx.theme().danger
                                } else {
                                    muted
                                })
                                .child(h.event_label().to_string()),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(what))
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(short_stamp(&h.date)),
                        )
                }))
                .into_any_element(),
        };

        v_flex()
            .id("lidarr-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_5()
            .child(
                h_flex()
                    .flex_none()
                    .gap_3()
                    .items_center()
                    .child(app_icon(icons::DOWNLOAD).text_color(cx.theme().foreground))
                    .child(div().text_lg().child("Lidarr"))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(7.))
                                    .rounded_full()
                                    .bg(status_color),
                            )
                            .child(div().text_xs().text_color(muted).truncate().child(status)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("lidarr-refresh")
                            .ghost()
                            .small()
                            .icon(app_icon(icons::REFRESH))
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("lidarr-open")
                            .ghost()
                            .small()
                            .label("Open Lidarr")
                            .on_click(move |_, _, cx| cx.open_url(&base)),
                    ),
            )
            .when_some(self.action_error.clone(), |this, why| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(why))
            })
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(section_title(
                        "Queue",
                        (queued > 0).then(|| queued.to_string()),
                        cx,
                    ))
                    .child(queue_body),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(section_title(
                        "Wanted",
                        wanted_total.filter(|n| *n > 0).map(|n| n.to_string()),
                        cx,
                    ))
                    .child(wanted_body),
            )
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(section_title("Recent activity", None, cx))
                    .child(history_body),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_drop_seconds_and_zone() {
        assert_eq!(short_stamp("2026-09-30T12:34:56Z"), "2026-09-30 12:34");
        assert_eq!(short_stamp("2026-09-30"), "2026-09-30");
    }
}
