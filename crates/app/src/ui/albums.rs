//! Album grid with cover art, pagination, and sort/filter tabs.

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, ScrollHandle, Window, div, img, prelude::*,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use subsonic::{Album, AlbumListType, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::AlbumSort;
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::Session;

const PAGE_SIZE: u32 = 100;
/// Load the next page when scrolled within this many pixels of the bottom.
const LOAD_AHEAD_PX: f32 = 600.;

/// All selectable filters, in display order.
const TABS: &[AlbumSort] = &[
    AlbumSort::All,
    AlbumSort::New,
    AlbumSort::Recent,
    AlbumSort::Frequent,
    AlbumSort::Random,
    AlbumSort::Starred,
];

fn tab_label(sort: AlbumSort) -> &'static str {
    match sort {
        AlbumSort::All => "All",
        AlbumSort::New => "New",
        AlbumSort::Recent => "Recent",
        AlbumSort::Frequent => "Frequent",
        AlbumSort::Random => "Random",
        AlbumSort::Starred => "Starred",
    }
}

fn tab_list_type(sort: AlbumSort) -> AlbumListType {
    match sort {
        AlbumSort::All => AlbumListType::AlphabeticalByName,
        AlbumSort::New => AlbumListType::Newest,
        AlbumSort::Recent => AlbumListType::Recent,
        AlbumSort::Frequent => AlbumListType::Frequent,
        AlbumSort::Random => AlbumListType::Random,
        AlbumSort::Starred => AlbumListType::Starred,
    }
}

#[derive(Default)]
struct TabState {
    /// Emitted albums, in final display order.
    albums: Vec<Album>,
    /// Per-library fetched-but-not-yet-emitted albums (server order).
    /// Held back until a globally-ordered merge can emit them safely.
    buffers: Vec<VecDeque<Album>>,
    /// How many albums each library has contributed to `albums` (fair
    /// interleave tie-break when the sort key can't decide).
    lib_emitted: Vec<usize>,
    loading: bool,
    exhausted: bool,
    /// Pages fetched so far; each page requests PAGE_SIZE per selected
    /// library, so per-library offsets stay aligned across the merge.
    page: u32,
    /// Per-library exhaustion, indexed like the selection at fetch time.
    lib_exhausted: Vec<bool>,
}

/// Display order between two albums for a tab (mirrors the server's order
/// for the corresponding getAlbumList2 type, so the per-library sorted
/// streams can be merge-sorted client-side).
fn album_cmp(tab: AlbumSort, a: &Album, b: &Album) -> Ordering {
    match tab {
        AlbumSort::All => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        AlbumSort::New => b.created.cmp(&a.created),
        AlbumSort::Frequent => b.play_count.cmp(&a.play_count),
        AlbumSort::Starred => b.starred.cmp(&a.starred),
        // No client-visible key; keep each library's order and let the
        // fair-interleave tie-break weave the streams together.
        AlbumSort::Recent | AlbumSort::Random => Ordering::Equal,
    }
}

/// Pop every album that can already be emitted in globally-correct order.
/// An album is only safe to emit while all non-exhausted libraries still
/// have buffered items — otherwise an unfetched item could sort earlier.
fn merge_ready(state: &mut TabState, tab: AlbumSort) -> Vec<Album> {
    let mut out = Vec::new();
    loop {
        let blocked = state
            .buffers
            .iter()
            .enumerate()
            .any(|(i, b)| b.is_empty() && !state.lib_exhausted[i]);
        if blocked {
            break;
        }
        let mut best: Option<usize> = None;
        for (i, buf) in state.buffers.iter().enumerate() {
            let Some(head) = buf.front() else { continue };
            let better = match best {
                None => true,
                Some(j) => match album_cmp(tab, head, state.buffers[j].front().unwrap()) {
                    Ordering::Less => true,
                    Ordering::Greater => false,
                    Ordering::Equal => state.lib_emitted[i] < state.lib_emitted[j],
                },
            };
            if better {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        state.lib_emitted[i] += 1;
        out.push(state.buffers[i].pop_front().unwrap());
    }
    out
}

pub enum AlbumsEvent {
    OpenAlbum(String),
    OpenArtist(String),
}

/// How a context-menu action should enqueue an album's songs.
#[derive(Clone, Copy)]
enum QueueMode {
    Play,
    Shuffle,
    PlayNext,
    Enqueue,
}

pub struct AlbumsView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    /// Albums for each filter tab, loaded independently and lazily.
    tabs: HashMap<AlbumSort, TabState>,
    art_paths: HashMap<String, PathBuf>,
    active_tab: AlbumSort,
    /// Resolution thumbnails are currently fetched at; tracked so a cover-size
    /// change can drop stale art and refetch at the new resolution.
    art_px: u32,
    scroll: ScrollHandle,
    error: Option<String>,
}

impl EventEmitter<AlbumsEvent> for AlbumsView {}

impl AlbumsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_tab = session.read(cx).settings.album_sort;
        let art_px = session.read(cx).settings.cover_size.art_px();
        let mut this = Self {
            session,
            player,
            playlists,
            tabs: HashMap::new(),
            art_paths: HashMap::new(),
            active_tab,
            art_px,
            scroll: ScrollHandle::new(),
            error: None,
        };
        this.load_more(active_tab, cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn select_tab(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        self.active_tab = tab;
        if self.tabs.get(&tab).is_none_or(|t| t.albums.is_empty()) {
            self.load_more(tab, cx);
        }
        self.session.update(cx, |session, _| {
            session.settings.album_sort = tab;
            session.persist_settings();
        });
        cx.notify();
    }

    fn load_more(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let libraries = self.session.read(cx).library_query_ids();
        let state = self.tabs.entry(tab).or_default();
        if state.loading || state.exhausted {
            return;
        }
        if state.lib_exhausted.len() != libraries.len() {
            state.lib_exhausted = vec![false; libraries.len()];
            state.buffers = vec![VecDeque::new(); libraries.len()];
            state.lib_emitted = vec![0; libraries.len()];
        }
        let offset = state.page * PAGE_SIZE;
        let pending: Vec<(usize, Option<String>)> = libraries
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !state.lib_exhausted[*i])
            .collect();
        state.loading = true;
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            // One page per selected library at the same offset, merged in
            // selection order (the API takes one musicFolderId per request).
            let result = runtime::spawn_io(async move {
                let mut batches = Vec::with_capacity(pending.len());
                for (lib_index, lib) in pending {
                    let batch = client
                        .get_album_list2(tab_list_type(tab), PAGE_SIZE, offset, lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    batches.push((lib_index, batch));
                }
                Ok::<_, anyhow::Error>(batches)
            })
            .await;

            let _ = this.update(cx, |view, cx| {
                let mut new_albums = Vec::new();
                let state = view.tabs.entry(tab).or_default();
                state.loading = false;
                match result {
                    Ok(batches) => {
                        state.page += 1;
                        for (lib_index, batch) in batches {
                            if batch.len() < PAGE_SIZE as usize
                                && let Some(flag) = state.lib_exhausted.get_mut(lib_index)
                            {
                                *flag = true;
                            }
                            if tab == AlbumSort::Random {
                                // No order to preserve — shuffle the combined
                                // page below instead of buffering.
                                new_albums.extend(batch);
                            } else {
                                state.buffers[lib_index].extend(batch);
                            }
                        }
                        if tab == AlbumSort::Random {
                            use rand::seq::SliceRandom;
                            new_albums.shuffle(&mut rand::rng());
                        } else {
                            new_albums = merge_ready(state, tab);
                        }
                        state.exhausted = state.lib_exhausted.iter().all(|&e| e)
                            && state.buffers.iter().all(|b| b.is_empty());
                        state.albums.extend(new_albums.iter().cloned());
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                for album in &new_albums {
                    view.fetch_art(album, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load the next page when the grid is scrolled near its bottom.
    fn maybe_load_more_on_scroll(&mut self, cx: &mut Context<Self>) {
        let scrolled = -self.scroll.offset().y;
        let max = self.scroll.max_offset().height;
        if max - scrolled < px(LOAD_AHEAD_PX) {
            self.load_more(self.active_tab, cx);
        }
    }

    /// Fetch the album's songs and act on them (play / shuffle / queue).
    fn queue_album(&mut self, album_id: String, mode: QueueMode, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let player = self.player.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album(&album_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            match result {
                Ok(album) => {
                    let _ = player.update(cx, |p, cx| match mode {
                        QueueMode::Play => p.play_queue(album.song, 0, cx),
                        QueueMode::Shuffle => p.play_queue_shuffled(album.song, cx),
                        QueueMode::PlayNext => p.play_next(album.song, cx),
                        QueueMode::Enqueue => p.enqueue(album.song, cx),
                    });
                }
                Err(e) => {
                    let _ = this.update(cx, |view, cx| {
                        view.error = Some(format!("{e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Fetch the album's songs and append them all to a playlist.
    fn add_album_to_playlist(
        &mut self,
        album_id: String,
        playlist_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let playlists = self.playlists.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album(&album_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            match result {
                Ok(album) => {
                    let ids: Vec<String> = album.song.iter().map(|s| s.id.clone()).collect();
                    let _ = playlists
                        .update(cx, |pl, cx| pl.add_songs(playlist_id, ids, cx));
                }
                Err(e) => {
                    let _ = this.update(cx, |view, cx| {
                        view.error = Some(format!("{e:#}"));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn fetch_art(&mut self, album: &Album, cx: &mut Context<Self>) {
        if self.art_paths.contains_key(&album.id) {
            return;
        }
        let Some(cover_id) = album.cover_art.clone() else {
            return;
        };
        // Synchronous cache hit: show it immediately, no async round-trip.
        // This is what makes covers appear instantly on app restart instead
        // of blanking then popping in one task at a time.
        if let Some(path) = artwork::cached(&cover_id, self.art_px) {
            self.art_paths.insert(album.id.clone(), path);
            return;
        }
        // Miss: download in the background.
        let Some(client) = self.client(cx) else {
            return;
        };
        let album_id = album.id.clone();
        let art_px = self.art_px;
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, art_px).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_paths.insert(album_id, path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Drop cached thumbnail paths and refetch the active tab's art at the
    /// current `art_px` (called when the cover-size setting changes).
    fn refetch_art(&mut self, cx: &mut Context<Self>) {
        self.art_paths.clear();
        let albums: Vec<Album> = self
            .tabs
            .get(&self.active_tab)
            .map(|t| t.albums.clone())
            .unwrap_or_default();
        for album in &albums {
            self.fetch_art(album, cx);
        }
    }

    fn render_card(
        &self,
        index: usize,
        album: &Album,
        tile: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = album.id.clone();
        let play_id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let name = album.name.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        // Right-click context menu data.
        let menu_id = album.id.clone();
        let menu_artist_id = album.artist_id.clone();
        let view = cx.entity();
        let menu_pl_list: Vec<(String, String)> = self
            .playlists
            .read(cx)
            .playlists
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();

        v_flex()
            .id(gpui::SharedString::from(format!("album-{}", album.id)))
            .group("acard")
            .w(px(tile + 12.))
            .p_1p5()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(AlbumsEvent::OpenAlbum(id.clone()));
            }))
            .child(
                div()
                    .size(px(tile))
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .shadow_sm()
                    .relative()
                    .when_some(art, |this, path| {
                        this.child(img(path).size(px(tile)).rounded_lg())
                    })
                    // Hover play button over the artwork.
                    .child(
                        div()
                            .absolute()
                            .bottom_2()
                            .right_2()
                            .opacity(0.)
                            .group_hover("acard", |s| s.opacity(1.))
                            .child(
                                Button::new(("card-play", index))
                                    .primary()
                                    .icon(app_icon(icons::PLAY))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.queue_album(play_id.clone(), QueueMode::Play, cx);
                                        cx.stop_propagation();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_0()
                    .child(div().text_sm().truncate().child(name))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(artist),
                    )
                    .when(!year.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(year),
                        )
                    }),
            )
            .context_menu(move |menu, window, cx| {
                let act = |mode: QueueMode| {
                    let view = view.clone();
                    let id = menu_id.clone();
                    move |_: &_, _: &mut Window, cx: &mut gpui::App| {
                        view.update(cx, |v, cx| v.queue_album(id.clone(), mode, cx));
                    }
                };
                let pl_list = menu_pl_list.clone();
                let pl_view = view.clone();
                let pl_album = menu_id.clone();
                let mut menu = menu
                    .item(PopupMenuItem::new("Play").on_click(act(QueueMode::Play)))
                    .item(PopupMenuItem::new("Shuffle").on_click(act(QueueMode::Shuffle)))
                    .item(PopupMenuItem::new("Play next").on_click(act(QueueMode::PlayNext)))
                    .item(PopupMenuItem::new("Add to queue").on_click(act(QueueMode::Enqueue)))
                    .submenu("Save to playlist", window, cx, move |sub, _w, _c| {
                        if pl_list.is_empty() {
                            return sub
                                .item(PopupMenuItem::new("No playlists yet").disabled(true));
                        }
                        let mut sub = sub;
                        for (pid, pname) in &pl_list {
                            let view = pl_view.clone();
                            let pid = pid.clone();
                            let album = pl_album.clone();
                            sub = sub.item(PopupMenuItem::new(pname.clone()).on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    view.update(cx, |v, cx| {
                                        v.add_album_to_playlist(album.clone(), pid.clone(), cx)
                                    });
                                },
                            ));
                        }
                        sub
                    });
                if let Some(aid) = menu_artist_id.clone() {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::separator()).item(
                        PopupMenuItem::new("Go to artist").on_click(
                            move |_, _, cx: &mut gpui::App| {
                                view.update(cx, |_, cx| {
                                    cx.emit(AlbumsEvent::OpenArtist(aid.clone()))
                                });
                            },
                        ),
                    );
                }
                menu
            })
    }
}

impl Render for AlbumsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_tab;

        // Pick up cover-size changes: refetch art at the new resolution.
        let cover = self.session.read(cx).settings.cover_size;
        let tile = cover.px();
        if cover.art_px() != self.art_px {
            self.art_px = cover.art_px();
            self.refetch_art(cx);
        }

        let tabs = h_flex().gap_1().children(TABS.iter().map(|&tab| {
            Button::new(tab_label(tab))
                .ghost()
                .xsmall()
                .label(tab_label(tab))
                .when(active == tab, |b: Button| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
        }));

        // If the loaded content doesn't fill the viewport (no scrollbar yet),
        // keep fetching until it does or the list is exhausted.
        let needs_fill = self
            .tabs
            .get(&active)
            .is_some_and(|t| !t.loading && !t.exhausted && !t.albums.is_empty())
            && self.scroll.max_offset().height <= px(0.);
        if needs_fill {
            self.load_more(active, cx);
        }

        let (albums, loading) = self
            .tabs
            .get(&active)
            .map(|t| (&t.albums[..], t.loading))
            .unwrap_or((&[], false));

        let cards: Vec<_> = albums
            .iter()
            .enumerate()
            .map(|(i, album)| self.render_card(i, album, tile, cx).into_any_element())
            .collect();

        v_flex()
            .id("albums-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.maybe_load_more_on_scroll(cx);
            }))
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .child(div().text_lg().child("Albums"))
                    .child(tabs),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            // Centered so the leftover space of the ragged last row is split
            // evenly — left and right gutters stay equal at any window width.
            .child(h_flex().flex_wrap().justify_center().gap_4().children(cards))
            // Loading indicator while the next page streams in.
            .when(loading, |this| {
                this.child(
                    h_flex().justify_center().py_2().child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Loading…"),
                    ),
                )
            })
    }
}
