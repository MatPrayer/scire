//! Lidarr metadata search (shared by the palette and the search page) and
//! the add page for a hit Lidarr doesn't track yet: root folder, profiles,
//! what to monitor, search on add.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, Task,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};

use super::lidarr::{HEADER_COVER, cover_well, muted_line};
use crate::services::lidarr::{
    self, AddOptions, Album, Artist, Lidarr, Lookup, Monitor, Profile, RootFolder,
};
use crate::services::{artwork, runtime};
use crate::state::lidarr::lidarr as lidarr_state;
use crate::state::session::Session;
use crate::ui::{sync_focus_scroll, with_focus_cursor};

/// Both of Lidarr's lookups for `query`: up to `max` artists then up to `max`
/// albums, and the first failure. Runs on the IO runtime.
pub async fn lookup(client: Lidarr, query: String, max: usize) -> (Vec<LidarrHit>, Option<String>) {
    let (artists, albums) =
        tokio::join!(client.lookup_artists(&query), client.lookup_albums(&query));
    let mut hits = Vec::new();
    let mut error = None;
    match artists {
        Ok(a) => hits.extend(
            a.into_iter()
                .take(max)
                .map(|a| LidarrHit::Artist(Box::new(a))),
        ),
        Err(e) => error = Some(crate::errors::error_text(&e)),
    }
    match albums {
        Ok(a) => hits.extend(
            a.into_iter()
                .take(max)
                .map(|a| LidarrHit::Album(Box::new(a))),
        ),
        Err(e) => error = error.or(Some(crate::errors::error_text(&e))),
    }
    (hits, error.map(|e| format!("Lidarr: {e}")))
}

/// Starts art downloads for `hits` into a view's key → path map (`get`
/// finds it again when an answer lands). Cached art is filled in at once.
pub fn want_hit_art<V: 'static>(
    paths: &mut HashMap<String, PathBuf>,
    client: &Lidarr,
    hits: &[LidarrHit],
    size: u32,
    get: fn(&mut V) -> &mut HashMap<String, PathBuf>,
    cx: &mut Context<V>,
) {
    for hit in hits {
        let key = hit.art_key();
        if paths.contains_key(&key) {
            continue;
        }
        if let Some(path) = artwork::cached_best(&key, size) {
            paths.insert(key, path);
            continue;
        }
        let Some(fetch) = hit.fetch_art(client, size) else {
            continue;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(path) = fetch.await {
                let _ = this.update(cx, |view, cx| {
                    get(view).insert(key, path);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// One Lidarr metadata-search hit.
#[derive(Clone, Debug, PartialEq)]
pub enum LidarrHit {
    Artist(Box<Lookup<Artist>>),
    Album(Box<Lookup<Album>>),
}

impl LidarrHit {
    pub fn title(&self) -> String {
        match self {
            LidarrHit::Artist(a) => a.item.artist_name.clone(),
            LidarrHit::Album(a) => a.item.title.clone(),
        }
    }

    /// The line under the title: what the hit is, and what tells it apart
    /// from its namesakes.
    pub fn subtitle(&self) -> String {
        let parts: Vec<String> = match self {
            LidarrHit::Artist(a) => vec![
                a.item
                    .artist_type
                    .clone()
                    .unwrap_or_else(|| "Artist".into()),
                a.item.disambiguation.clone().unwrap_or_default(),
            ],
            LidarrHit::Album(a) => vec![
                a.item.artist_name().to_string(),
                a.item.year().unwrap_or_default().to_string(),
                a.item.type_label(),
            ],
        };
        parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn in_lidarr(&self) -> bool {
        match self {
            LidarrHit::Artist(a) => a.item.in_lidarr(),
            LidarrHit::Album(a) => a.item.in_lidarr(),
        }
    }

    /// Art cache key, by MusicBrainz id so it survives being added.
    pub fn art_key(&self) -> String {
        match self {
            LidarrHit::Artist(a) => format!("lidarr-artist-{}", a.item.foreign_artist_id),
            LidarrHit::Album(a) => format!("lidarr-{}", a.item.foreign_album_id),
        }
    }

    /// Fetch for the hit's art at `size`, `None` when it has none.
    pub fn fetch_art(
        &self,
        client: &Lidarr,
        size: u32,
    ) -> Option<impl std::future::Future<Output = anyhow::Result<PathBuf>> + use<>> {
        let (url, remote) = match self {
            LidarrHit::Artist(a) => a.item.photo(),
            LidarrHit::Album(a) => a.item.cover(),
        }?;
        let (url, header) = if remote {
            (lidarr::cover_thumbnail_url(url, size), None)
        } else {
            (
                client.absolute(url),
                Some(("X-Api-Key", client.api_key().to_string())),
            )
        };
        Some(artwork::fetch_url(url, header, self.art_key(), size))
    }
}

pub enum LidarrAddEvent {
    OpenAlbum(i64),
    /// The artist was added; its Lidarr page replaces this one.
    OpenArtist(i64),
}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

struct Choices {
    roots: Vec<RootFolder>,
    quality: Vec<Profile>,
    metadata: Vec<Profile>,
}

/// A vi-walkable control of the page.
#[derive(Clone, Copy, PartialEq)]
enum ViItem {
    Root,
    Quality,
    Metadata,
    Monitor,
    Search,
    Add,
}

pub struct LidarrAddView {
    session: Entity<Session>,
    hit: LidarrHit,
    choices: Load<Choices>,
    root: Option<String>,
    quality: Option<i64>,
    metadata: Option<i64>,
    monitor: Monitor,
    search: bool,
    adding: bool,
    error: Option<String>,
    art: Option<PathBuf>,
    art_loading: bool,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
    _load: Option<Task<()>>,
    _art: Option<Task<()>>,
}

impl EventEmitter<LidarrAddEvent> for LidarrAddView {}

impl LidarrAddView {
    pub fn new(session: Entity<Session>, hit: LidarrHit, cx: &mut Context<Self>) -> Self {
        let state = lidarr_state(cx);
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            hit,
            choices: Load::Loading,
            root: None,
            quality: None,
            metadata: None,
            monitor: Monitor::All,
            search: true,
            adding: false,
            error: None,
            art: None,
            art_loading: false,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            _load: None,
            _art: None,
        };
        this.load_art(cx);
        this.load_choices(cx);
        this
    }

    fn client(&self, cx: &App) -> Option<Lidarr> {
        lidarr_state(cx).read(cx).client.clone()
    }

    fn load_art(&mut self, cx: &mut Context<Self>) {
        let size = (HEADER_COVER * 2.) as u32;
        if let Some(path) = artwork::cached_best(&self.hit.art_key(), size) {
            self.art = Some(path);
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(fetch) = self.hit.fetch_art(&client, size) else {
            return;
        };
        self.art_loading = true;
        self._art = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            let _ = this.update(cx, |this, cx| {
                this.art_loading = false;
                match result {
                    Ok(path) => this.art = Some(path),
                    Err(e) => tracing::debug!("lidarr lookup art: {e:#}"),
                }
                cx.notify();
            });
        }));
    }

    fn load_choices(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.choices = Load::Failed("Lidarr is not connected.".into());
            return;
        };
        self.choices = Load::Loading;
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let (roots, quality, metadata) = tokio::join!(
                    client.root_folders(),
                    client.quality_profiles(),
                    client.metadata_profiles()
                );
                Ok(Choices {
                    roots: roots?,
                    quality: quality?,
                    metadata: metadata?,
                })
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(choices) if choices.roots.is_empty() => {
                        this.choices = Load::Failed(
                            "Lidarr has no root folder. Add one in Lidarr's Media Management settings."
                                .into(),
                        );
                    }
                    Ok(choices) => {
                        let first = choices.roots[0].path.clone();
                        this.choices = Load::Loaded(choices);
                        this.pick_root(first);
                    }
                    Err(e) => this.choices = Load::Failed(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        }));
    }

    /// Picks a root folder and takes on the defaults Lidarr keeps for it.
    fn pick_root(&mut self, path: String) {
        let Load::Loaded(choices) = &self.choices else {
            return;
        };
        let Some(root) = choices.roots.iter().find(|r| r.path == path) else {
            return;
        };
        let known = |list: &[Profile], id: i64| list.iter().any(|p| p.id == id);
        self.quality = Some(
            if known(&choices.quality, root.default_quality_profile_id) {
                root.default_quality_profile_id
            } else {
                choices.quality.first().map(|p| p.id).unwrap_or(0)
            },
        );
        self.metadata = Some(
            if known(&choices.metadata, root.default_metadata_profile_id) {
                root.default_metadata_profile_id
            } else {
                choices.metadata.first().map(|p| p.id).unwrap_or(0)
            },
        );
        if let Some(monitor) = root
            .default_monitor_option
            .as_deref()
            .and_then(Monitor::from_api)
        {
            self.monitor = monitor;
        }
        self.root = Some(path);
    }

    fn options(&self) -> Option<AddOptions> {
        Some(AddOptions {
            root_folder: self.root.clone()?,
            quality_profile_id: self.quality?,
            metadata_profile_id: self.metadata?,
            monitor: self.monitor,
            search: self.search,
        })
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        if self.adding {
            return;
        }
        let (Some(client), Some(opts)) = (self.client(cx), self.options()) else {
            return;
        };
        self.adding = true;
        self.error = None;
        cx.notify();
        let hit = self.hit.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                match &hit {
                    LidarrHit::Artist(a) => client.add_artist(a, &opts).await.map(|a| a.id),
                    LidarrHit::Album(a) => client.add_album(a, &opts).await.map(|a| a.id),
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.adding = false;
                lidarr_state(cx).update(cx, |s, cx| s.poke(cx));
                match (result, &this.hit) {
                    (Ok(id), LidarrHit::Artist(_)) => cx.emit(LidarrAddEvent::OpenArtist(id)),
                    (Ok(id), LidarrHit::Album(_)) => cx.emit(LidarrAddEvent::OpenAlbum(id)),
                    (Err(e), _) => this.error = Some(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn is_artist(&self) -> bool {
        matches!(self.hit, LidarrHit::Artist(_))
    }

    fn vi_items(&self) -> Vec<ViItem> {
        let Load::Loaded(choices) = &self.choices else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if choices.roots.len() > 1 {
            items.push(ViItem::Root);
        }
        if choices.quality.len() > 1 {
            items.push(ViItem::Quality);
        }
        if choices.metadata.len() > 1 {
            items.push(ViItem::Metadata);
        }
        if self.is_artist() {
            items.push(ViItem::Monitor);
        }
        items.push(ViItem::Search);
        items.push(ViItem::Add);
        items
    }

    fn vi_focused(&self, item: ViItem) -> bool {
        self.vi_cursor
            .and_then(|i| self.vi_items().get(i).copied())
            .is_some_and(|f| f == item)
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = self.vi_items().len();
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

    /// Enter: a choice group moves to its next option, the switch flips,
    /// Add adds, an album opens.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.vi_cursor.and_then(|i| self.vi_items().get(i).copied()) else {
            return;
        };
        fn next<T: Clone + PartialEq>(list: &[T], cur: Option<&T>) -> Option<T> {
            let at = cur.and_then(|c| list.iter().position(|x| x == c));
            let i = at.map_or(0, |i| (i + 1) % list.len());
            list.get(i).cloned()
        }
        match item {
            ViItem::Root => {
                if let Load::Loaded(c) = &self.choices {
                    let paths: Vec<String> = c.roots.iter().map(|r| r.path.clone()).collect();
                    if let Some(path) = next(&paths, self.root.as_ref()) {
                        self.pick_root(path);
                    }
                }
            }
            ViItem::Quality => {
                if let Load::Loaded(c) = &self.choices {
                    let ids: Vec<i64> = c.quality.iter().map(|p| p.id).collect();
                    self.quality = next(&ids, self.quality.as_ref()).or(self.quality);
                }
            }
            ViItem::Metadata => {
                if let Load::Loaded(c) = &self.choices {
                    let ids: Vec<i64> = c.metadata.iter().map(|p| p.id).collect();
                    self.metadata = next(&ids, self.metadata.as_ref()).or(self.metadata);
                }
            }
            ViItem::Monitor => {
                self.monitor = next(&Monitor::ALL, Some(&self.monitor)).unwrap_or(Monitor::All);
            }
            ViItem::Search => self.search = !self.search,
            ViItem::Add => self.add(cx),
        }
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// A labelled group of buttons, the picked one filled.
    fn choice_group(
        &self,
        item: ViItem,
        label: &'static str,
        buttons: Vec<Button>,
        glow: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let id = match item {
            ViItem::Root => "vi-lidarr-add-root",
            ViItem::Quality => "vi-lidarr-add-quality",
            ViItem::Metadata => "vi-lidarr-add-metadata",
            _ => "vi-lidarr-add-monitor",
        };
        let focused = self.vi_focused(item);
        let group = v_flex()
            .id((id, 0usize))
            .gap_1p5()
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(h_flex().gap_1p5().flex_wrap().children(buttons));
        with_focus_cursor(id, group, focused, glow, None, cx)
    }

    fn render_form(&self, glow: bool, cx: &Context<Self>) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let choices = match &self.choices {
            Load::Loading => {
                return div()
                    .text_sm()
                    .text_color(muted)
                    .child("Loading Lidarr's settings…")
                    .into_any_element();
            }
            Load::Failed(why) => {
                return h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(why.clone()),
                    )
                    .child(
                        Button::new("lidarr-add-retry")
                            .ghost()
                            .small()
                            .label("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.load_choices(cx))),
                    )
                    .into_any_element();
            }
            Load::Loaded(c) => c,
        };

        let mut form = v_flex()
            .flex_none()
            .w_full()
            .max_w(px(720.))
            .gap_4()
            .p_4()
            .rounded_2xl()
            .bg(cx.theme().sidebar);

        // A group with one option has nothing to choose; its value still
        // shows, as a line, so the user knows where the files will go.
        let root_buttons: Vec<Button> = choices
            .roots
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let path = r.path.clone();
                let label = match r.free_space {
                    Some(free) => format!("{} ({} free)", r.path, lidarr::human_size(free)),
                    None => r.path.clone(),
                };
                Button::new(("lidarr-add-root", i))
                    .small()
                    .label(label)
                    .when(self.root.as_deref() == Some(r.path.as_str()), |b| {
                        b.primary()
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_root(path.clone());
                        cx.notify();
                    }))
            })
            .collect();
        form = form.child(self.choice_group(ViItem::Root, "Root folder", root_buttons, glow, cx));

        let profile_buttons = |which: ViItem, list: &[Profile], picked: Option<i64>| {
            list.iter()
                .map(|p| {
                    let id = p.id;
                    let key = if which == ViItem::Quality {
                        "lidarr-add-quality"
                    } else {
                        "lidarr-add-metadata"
                    };
                    Button::new((key, id as usize))
                        .small()
                        .label(p.name.clone())
                        .when(picked == Some(id), |b| b.primary())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if which == ViItem::Quality {
                                this.quality = Some(id);
                            } else {
                                this.metadata = Some(id);
                            }
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>()
        };
        form = form
            .child(self.choice_group(
                ViItem::Quality,
                "Quality profile",
                profile_buttons(ViItem::Quality, &choices.quality, self.quality),
                glow,
                cx,
            ))
            .child(self.choice_group(
                ViItem::Metadata,
                "Metadata profile",
                profile_buttons(ViItem::Metadata, &choices.metadata, self.metadata),
                glow,
                cx,
            ));

        if self.is_artist() {
            let monitor_buttons: Vec<Button> = Monitor::ALL
                .into_iter()
                .enumerate()
                .map(|(i, m)| {
                    Button::new(("lidarr-add-monitor", i))
                        .small()
                        .label(m.label())
                        .when(self.monitor == m, |b| b.primary())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.monitor = m;
                            cx.notify();
                        }))
                })
                .collect();
            form = form.child(self.choice_group(
                ViItem::Monitor,
                "Monitor",
                monitor_buttons,
                glow,
                cx,
            ));
        }

        let search_label = if self.is_artist() {
            "Start searching for monitored albums"
        } else {
            "Start searching for this album"
        };
        let search_focused = self.vi_focused(ViItem::Search);
        form = form.child(with_focus_cursor(
            "vi-lidarr-add-search",
            h_flex()
                .id("lidarr-add-search-row")
                .gap_2()
                .items_center()
                .when(search_focused, |s| {
                    s.anchor_scroll(Some(self.focus_anchor.clone()))
                })
                .child(
                    Switch::new("lidarr-add-search")
                        .checked(self.search)
                        .small()
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.search = *checked;
                            cx.notify();
                        })),
                )
                .child(div().text_sm().child(search_label)),
            search_focused,
            glow,
            None,
            cx,
        ));

        let add_focused = self.vi_focused(ViItem::Add);
        let add_label = if self.is_artist() {
            "Add artist"
        } else {
            "Add album"
        };
        form = form.child(
            h_flex()
                .gap_3()
                .items_center()
                .child(with_focus_cursor(
                    "vi-lidarr-add-button",
                    div()
                        .id("lidarr-add-button-row")
                        .when(add_focused, |s| {
                            s.anchor_scroll(Some(self.focus_anchor.clone()))
                        })
                        .child(
                            Button::new("lidarr-add")
                                .primary()
                                .icon(Icon::new(IconName::Plus))
                                .label(add_label)
                                .loading(self.adding)
                                .on_click(cx.listener(|this, _, _, cx| this.add(cx))),
                        ),
                    add_focused,
                    glow,
                    None,
                    cx,
                ))
                .when_some(self.error.clone(), |this, why| {
                    this.child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(why),
                    )
                }),
        );
        form.into_any_element()
    }
}

impl Render for LidarrAddView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let Some(client) = self.client(cx) else {
            return v_flex()
                .size_full()
                .p_4()
                .gap_3()
                .child(div().text_lg().child(self.hit.title()))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Lidarr is off. Turn it on under Settings → Connections."),
                )
                .into_any_element();
        };

        let glow = self.session.read(cx).settings.selection_glow_vi;
        let muted = cx.theme().muted_foreground;
        let tracked = self.hit.in_lidarr();
        let (genres, overview, web) = match &self.hit {
            LidarrHit::Artist(a) => (
                a.item.genres.clone(),
                a.item.overview.clone(),
                format!("{}/artist/{}", client.base(), a.item.foreign_artist_id),
            ),
            LidarrHit::Album(a) => (
                a.item.genres.clone(),
                a.item.overview.clone(),
                format!("{}/album/{}", client.base(), a.item.foreign_album_id),
            ),
        };
        let fallback = match &self.hit {
            LidarrHit::Artist(_) => IconName::CircleUser,
            LidarrHit::Album(_) => IconName::LayoutDashboard,
        };

        let header = h_flex()
            .flex_none()
            .w_full()
            .gap_4()
            .items_start()
            .p_4()
            .rounded_2xl()
            .bg(cx.theme().sidebar)
            .child(
                cover_well(self.art.clone(), HEADER_COVER, self.art_loading, cx).when(
                    self.art.is_none() && !self.art_loading,
                    |this| {
                        this.flex()
                            .items_center()
                            .justify_center()
                            .text_color(muted)
                            .child(Icon::new(fallback).large())
                    },
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(div().text_2xl().font_medium().child(self.hit.title()))
                    .child(div().text_sm().text_color(muted).child(self.hit.subtitle()))
                    .when(!genres.is_empty(), |this| {
                        this.child(muted_line(genres.join(", "), cx))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .pt_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if tracked { cx.theme().success } else { muted })
                                    .child(if tracked {
                                        "In Lidarr"
                                    } else {
                                        "Not in Lidarr"
                                    }),
                            )
                            .when(tracked, |this| {
                                this.child(
                                    Button::new("lidarr-add-open-web")
                                        .ghost()
                                        .small()
                                        .label("Open in Lidarr")
                                        .on_click(move |_, _, cx| cx.open_url(&web)),
                                )
                            }),
                    ),
            );

        v_flex()
            .id("lidarr-add-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            .child(header)
            .when(!tracked, |this| this.child(self.render_form(glow, cx)))
            .when_some(overview.filter(|o| !o.trim().is_empty()), |this, o| {
                this.child(
                    v_flex()
                        .flex_none()
                        .gap_2()
                        .child(div().text_sm().font_medium().child("About"))
                        .child(div().text_sm().text_color(muted).child(o)),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist_hit(json: &str) -> LidarrHit {
        let raw: Vec<serde_json::Value> = vec![serde_json::from_str(json).unwrap()];
        LidarrHit::Artist(Box::new(lidarr::parse_lookups(raw).remove(0)))
    }

    fn album_hit(json: &str) -> LidarrHit {
        let raw: Vec<serde_json::Value> = vec![serde_json::from_str(json).unwrap()];
        LidarrHit::Album(Box::new(lidarr::parse_lookups(raw).remove(0)))
    }

    #[test]
    fn subtitles_say_what_the_hit_is() {
        let a = artist_hit(
            r#"{"artistName":"Low","artistType":"Group","disambiguation":"US slowcore"}"#,
        );
        assert_eq!(a.subtitle(), "Group · US slowcore");
        assert!(!a.in_lidarr());
        let plain = artist_hit(r#"{"artistName":"X"}"#);
        assert_eq!(plain.subtitle(), "Artist");
        let b = album_hit(
            r#"{"id":4,"title":"Things We Lost","albumType":"Album",
                "releaseDate":"2001-01-01T00:00:00Z","artist":{"artistName":"Low"}}"#,
        );
        assert_eq!(b.subtitle(), "Low · 2001 · Album");
        assert!(b.in_lidarr());
    }
}
