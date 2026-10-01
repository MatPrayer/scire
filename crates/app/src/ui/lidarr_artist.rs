//! A tracked Lidarr artist's page: photo, what Lidarr has of the
//! discography, the artist's monitored switch and search, then every release
//! in sections by type with its state, its own monitored switch and search.
//! Opened from search hits, a Lidarr album's artist line, and after adding.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, Task,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use super::lidarr::{Covers, HEADER_COVER, album_activity, cover_well, muted_line, section_title};
use super::lidarr_add::LidarrHit;
use crate::assets::{app_icon, icons};
use crate::services::lidarr::{self, Album, Artist, Lidarr, Lookup};
use crate::services::runtime;
use crate::state::lidarr::{LidarrState, lidarr as lidarr_state};
use crate::state::session::Session;
use crate::ui::{sync_focus_scroll, with_focus_cursor};

/// Cover edge on a release row; a little larger than the Lidarr page's.
const RELEASE_COVER: f32 = 48.;
/// A freshly added artist's releases arrive once Lidarr has refreshed it from
/// its metadata server; until then they are asked for again this often.
const DISCOGRAPHY_RETRY: Duration = Duration::from_secs(3);
const DISCOGRAPHY_TRIES: u32 = 10;

pub enum LidarrArtistEvent {
    OpenAlbum(i64),
}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

pub struct LidarrArtistView {
    session: Entity<Session>,
    artist_id: i64,
    artist: Option<Lookup<Artist>>,
    albums: Load<Vec<Album>>,
    /// Releases that have every file are folded away.
    only_missing: bool,
    action_error: Option<String>,
    art: Option<PathBuf>,
    art_loading: bool,
    art_asked: bool,
    covers: Covers,
    imports_seen: u64,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// 0 = search monitored, then release rows in drawn order.
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
    _load: Option<Task<()>>,
    _art: Option<Task<()>>,
}

impl EventEmitter<LidarrArtistEvent> for LidarrArtistView {}

impl LidarrArtistView {
    /// `fresh`: just added, so an empty discography means "not yet".
    pub fn new(
        session: Entity<Session>,
        artist_id: i64,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = lidarr_state(cx);
        let imports_seen = state.read(cx).imports;
        cx.observe(&state, |this: &mut Self, state, cx| {
            let imports = state.read(cx).imports;
            if imports != this.imports_seen {
                this.imports_seen = imports;
                this.load(DISCOGRAPHY_TRIES, cx);
            }
            cx.notify();
        })
        .detach();
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            artist_id,
            artist: None,
            albums: Load::Loading,
            only_missing: false,
            action_error: None,
            art: None,
            art_loading: false,
            art_asked: false,
            covers: Covers::default(),
            imports_seen,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
            _load: None,
            _art: None,
        };
        this.load(if fresh { 0 } else { DISCOGRAPHY_TRIES }, cx);
        this
    }

    fn client(&self, cx: &App) -> Option<Lidarr> {
        lidarr_state(cx).read(cx).client.clone()
    }

    /// Reads the artist and its releases. `tries` below `DISCOGRAPHY_TRIES`
    /// asks again later when the release list comes back empty.
    fn load(&mut self, tries: u32, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.albums = Load::Failed("Lidarr is not connected.".into());
            return;
        };
        let id = self.artist_id;
        self._load = Some(cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let (artist, albums) = tokio::join!(client.artist(id), client.albums(id));
                Ok((artist?, albums?))
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((artist, albums)) => {
                        this.artist = Some(artist);
                        this.want_art(cx);
                        if albums.is_empty() && tries < DISCOGRAPHY_TRIES {
                            this.retry_later(tries + 1, cx);
                        } else {
                            this.albums = Load::Loaded(albums);
                        }
                    }
                    Err(e) => this.albums = Load::Failed(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        }));
    }

    fn retry_later(&mut self, tries: u32, cx: &mut Context<Self>) {
        self._load = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DISCOGRAPHY_RETRY).await;
            let _ = this.update(cx, |this, cx| this.load(tries, cx));
        }));
    }

    fn want_art(&mut self, cx: &mut Context<Self>) {
        if self.art_asked {
            return;
        }
        let (Some(client), Some(artist)) = (self.client(cx), self.artist.clone()) else {
            return;
        };
        self.art_asked = true;
        let hit = LidarrHit::Artist(Box::new(artist));
        let size = (HEADER_COVER * 2.) as u32;
        if let Some(path) = crate::services::artwork::cached_best(&hit.art_key(), size) {
            self.art = Some(path);
            return;
        }
        let Some(fetch) = hit.fetch_art(&client, size) else {
            return;
        };
        self.art_loading = true;
        self._art = Some(cx.spawn(async move |this, cx| {
            let result = fetch.await;
            let _ = this.update(cx, |this, cx| {
                this.art_loading = false;
                match result {
                    Ok(path) => this.art = Some(path),
                    Err(e) => tracing::debug!("lidarr artist art: {e:#}"),
                }
                cx.notify();
            });
        }));
    }

    fn albums(&self) -> &[Album] {
        match &self.albums {
            Load::Loaded(albums) => albums,
            _ => &[],
        }
    }

    /// Release indices in drawn order, for the vi walk.
    fn drawn(&self) -> Vec<usize> {
        lidarr::discography_sections(self.albums(), self.only_missing)
            .into_iter()
            .flat_map(|(_, ids)| ids)
            .collect()
    }

    fn search_artist(&mut self, cx: &mut Context<Self>) {
        let id = self.artist_id;
        let task = lidarr_state(cx).update(cx, |s, cx| s.search_artist(id, cx));
        self.report(task, cx);
    }

    fn search_album(&mut self, album_id: i64, cx: &mut Context<Self>) {
        let task = lidarr_state(cx).update(cx, |s, cx| s.search_album(album_id, cx));
        self.report(task, cx);
    }

    fn report(&mut self, task: Task<anyhow::Result<()>>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.action_error = result.err().map(|e| crate::errors::error_text(&e));
                cx.notify();
            });
        })
        .detach();
    }

    fn set_artist_monitored(&mut self, monitored: bool, cx: &mut Context<Self>) {
        let (Some(client), Some(artist)) = (self.client(cx), self.artist.clone()) else {
            return;
        };
        if let Some(a) = self.artist.as_mut() {
            a.item.monitored = monitored;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(
                    async move { client.set_artist_monitored(&artist, monitored).await },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(updated) => this.artist = Some(updated),
                    Err(e) => {
                        if let Some(a) = this.artist.as_mut() {
                            a.item.monitored = !monitored;
                        }
                        this.action_error = Some(crate::errors::error_text(&e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_album_monitored(&mut self, album_id: i64, monitored: bool, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let flip = move |this: &mut Self, value: bool| {
            if let Load::Loaded(albums) = &mut this.albums
                && let Some(album) = albums.iter_mut().find(|a| a.id == album_id)
            {
                album.monitored = value;
            }
        };
        flip(self, monitored);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(async move { client.set_monitored(album_id, monitored).await })
                    .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    flip(this, !monitored);
                    this.action_error = Some(crate::errors::error_text(&e));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = 1 + self.drawn().len();
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        cx.notify();
    }

    /// Enter: search the artist, or open the release.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        match self.vi_cursor {
            Some(0) => self.search_artist(cx),
            Some(i) => {
                let drawn = self.drawn();
                if let Some(album) = drawn.get(i - 1).and_then(|&ix| self.albums().get(ix)) {
                    cx.emit(LidarrArtistEvent::OpenAlbum(album.id));
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

    fn render_release(
        &self,
        album: &Album,
        state: &LidarrState,
        focused: bool,
        glow: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let muted = cx.theme().muted_foreground;
        let id = album.id;
        let activity = album_activity(state, id);
        let busy = activity.is_some();
        let (status, color) = match activity {
            Some((label, problem)) => (
                label,
                if problem {
                    cx.theme().warning
                } else {
                    cx.theme().primary
                },
            ),
            None if album.complete() => ("On disk".to_string(), cx.theme().success),
            None => match album.partial() {
                Some((files, tracks)) => (format!("{files}/{tracks} tracks"), cx.theme().warning),
                None if album.monitored => ("Missing".to_string(), muted),
                None => ("Not monitored".to_string(), muted),
            },
        };
        let tracks = album
            .statistics
            .as_ref()
            .map(|s| s.total_track_count.max(s.track_count))
            .filter(|n| *n > 0);
        let secondary = album.type_label();
        let secondary = secondary
            .split_once(" · ")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
        let sub = [
            album.release_day().map(str::to_string),
            (!secondary.is_empty()).then_some(secondary),
            tracks.map(|n| format!("{n} tracks")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let monitored = album.monitored;
        let row = h_flex()
            .id(("lidarr-artist-album", id as usize))
            .px_2()
            .py_1p5()
            .gap_3()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(!monitored, |s| s.opacity(0.7))
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(LidarrArtistEvent::OpenAlbum(id))))
            .child(cover_well(
                self.covers.get(id),
                RELEASE_COVER,
                self.covers.pending(id),
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(div().text_sm().truncate().child(album.title.clone()))
                            .when_some(
                                album.disambiguation.clone().filter(|d| !d.is_empty()),
                                |this, d| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("({d})")),
                                    )
                                },
                            ),
                    )
                    .child(muted_line(sub, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(110.))
                    .text_xs()
                    .text_color(color)
                    .text_right()
                    .truncate()
                    .child(status),
            )
            .child(
                div()
                    .id(("lidarr-artist-album-mon-wrap", id as usize))
                    .flex_none()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        Switch::new(("lidarr-artist-album-mon", id as usize))
                            .checked(monitored)
                            .small()
                            .tooltip(if monitored {
                                "Monitored"
                            } else {
                                "Not monitored"
                            })
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.set_album_monitored(id, *checked, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            .child(
                Button::new(("lidarr-artist-album-search", id as usize))
                    .ghost()
                    .small()
                    .icon(app_icon(icons::DOWNLOAD))
                    .tooltip("Search automatically")
                    .disabled(busy || album.complete())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.search_album(id, cx);
                        cx.stop_propagation();
                    })),
            );
        with_focus_cursor(
            format!("vi-lidarr-artist-album-{id}"),
            row,
            focused,
            glow,
            None,
            cx,
        )
    }
}

impl Render for LidarrArtistView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let muted = cx.theme().muted_foreground;
        let Some(client) = self.client(cx) else {
            return v_flex()
                .size_full()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("Lidarr is off. Turn it on under Settings → Connections."),
                )
                .into_any_element();
        };
        let albums: Vec<Album> = self.albums().to_vec();
        for album in &albums {
            self.covers.want(
                &client,
                album,
                (RELEASE_COVER * 2.) as u32,
                |v: &mut Self| &mut v.covers,
                cx,
            );
        }

        let glow = self.session.read(cx).settings.selection_glow_vi;
        let state_entity = lidarr_state(cx);
        let state = state_entity.read(cx);
        let artist = self.artist.as_ref().map(|a| a.item.clone());
        let name = artist
            .as_ref()
            .map(|a| a.artist_name.clone())
            .unwrap_or_else(|| "Lidarr artist".into());
        let subtitle = [
            artist.as_ref().and_then(|a| a.artist_type.clone()),
            artist.as_ref().and_then(|a| a.disambiguation.clone()),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let genres = artist
            .as_ref()
            .map(|a| a.genres.join(", "))
            .unwrap_or_default();
        let overview = artist
            .as_ref()
            .and_then(|a| a.overview.clone())
            .filter(|o| !o.trim().is_empty());
        let artist_monitored = artist.as_ref().is_some_and(|a| a.monitored);
        let web = artist
            .as_ref()
            .map(|a| format!("{}/artist/{}", client.base(), a.foreign_artist_id));

        // What Lidarr is doing for this artist right now.
        let ids: HashSet<i64> = albums.iter().map(|a| a.id).collect();
        let downloading = state
            .queue
            .iter()
            .filter(|q| q.album_id.is_some_and(|id| ids.contains(&id)))
            .count();
        let searching_albums = state.searching.iter().filter(|id| ids.contains(id)).count();
        let artist_searching = state.searching_artists.contains(&self.artist_id);
        let activity = [
            (downloading > 0).then(|| format!("{downloading} downloading")),
            (searching_albums > 0).then(|| format!("{searching_albums} searching")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");

        let search_focused = self.vi_cursor == Some(0);
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
                            .child(Icon::new(IconName::CircleUser).large())
                    },
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(div().text_2xl().font_medium().child(name))
                    .when(!subtitle.is_empty(), |this| {
                        this.child(div().text_sm().text_color(muted).child(subtitle))
                    })
                    .when(!genres.is_empty(), |this| {
                        this.child(muted_line(genres, cx))
                    })
                    .when(!albums.is_empty(), |this| {
                        this.child(div().text_sm().child(lidarr::discography_summary(&albums)))
                    })
                    .when(!activity.is_empty(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().primary)
                                .child(activity),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .pt_2()
                            .child(with_focus_cursor(
                                "vi-lidarr-artist-search",
                                div()
                                    .id("lidarr-artist-search-wrap")
                                    .when(search_focused, |s| {
                                        s.anchor_scroll(Some(self.focus_anchor.clone()))
                                    })
                                    .child(
                                        Button::new("lidarr-artist-search")
                                            .primary()
                                            .icon(app_icon(icons::DOWNLOAD))
                                            .label(if artist_searching {
                                                "Searching…"
                                            } else {
                                                "Search monitored"
                                            })
                                            .tooltip(
                                                "Search for every monitored release Lidarr lacks",
                                            )
                                            .loading(artist_searching)
                                            .disabled(self.artist.is_none())
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.search_artist(cx)
                                                }),
                                            ),
                                    ),
                                search_focused,
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
                                        Switch::new("lidarr-artist-monitored")
                                            .checked(artist_monitored)
                                            .small()
                                            .disabled(self.artist.is_none())
                                            .on_click(cx.listener(
                                                |this, checked: &bool, _, cx| {
                                                    this.set_artist_monitored(*checked, cx)
                                                },
                                            )),
                                    )
                                    .child(div().text_sm().child("Monitored")),
                            )
                            .child(
                                Button::new("lidarr-artist-refresh")
                                    .ghost()
                                    .small()
                                    .icon(app_icon(icons::REFRESH))
                                    .tooltip("Reload")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load(DISCOGRAPHY_TRIES, cx);
                                    })),
                            )
                            .when_some(web, |this, url| {
                                this.child(
                                    Button::new("lidarr-artist-open")
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

        let body: gpui::AnyElement = match &self.albums {
            Load::Loading => div()
                .text_sm()
                .text_color(muted)
                .child("Waiting for Lidarr to fetch the discography…")
                .into_any_element(),
            Load::Failed(why) => h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(why.clone()),
                )
                .child(
                    Button::new("lidarr-artist-retry")
                        .ghost()
                        .small()
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.load(DISCOGRAPHY_TRIES, cx))),
                )
                .into_any_element(),
            Load::Loaded(albums) if albums.is_empty() => div()
                .text_sm()
                .text_color(muted)
                .child("Lidarr lists no releases for this artist under its metadata profile.")
                .into_any_element(),
            Load::Loaded(albums) => {
                let sections = lidarr::discography_sections(albums, self.only_missing);
                let mut flat = 0usize;
                let mut column = v_flex().gap_5();
                if sections.is_empty() {
                    column = column.child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("Lidarr has every release on disk."),
                    );
                }
                for (label, ids) in sections {
                    let rows: Vec<gpui::AnyElement> = ids
                        .iter()
                        .map(|&ix| {
                            flat += 1;
                            self.render_release(
                                &albums[ix],
                                state,
                                self.vi_cursor == Some(flat),
                                glow,
                                cx,
                            )
                        })
                        .collect();
                    column = column.child(
                        v_flex()
                            .flex_none()
                            .gap_1()
                            .child(section_title(label, Some(rows.len().to_string()), cx))
                            .child(v_flex().gap_0p5().children(rows)),
                    );
                }
                column.into_any_element()
            }
        };

        let only_missing = self.only_missing;
        v_flex()
            .id("lidarr-artist-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4()
            .child(header)
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_medium().child("Discography"))
                    .child(div().flex_1())
                    .child(
                        Switch::new("lidarr-artist-only-missing")
                            .checked(only_missing)
                            .small()
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.only_missing = *checked;
                                this.vi_cursor = None;
                                cx.notify();
                            })),
                    )
                    .child(div().text_sm().text_color(muted).child("Only missing")),
            )
            .child(body)
            .when_some(overview, |this, o| {
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
