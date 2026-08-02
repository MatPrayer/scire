//! Album grid with cover art, pagination, and sort/filter tabs.

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, UniformListScrollHandle, Window, div,
    img, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use subsonic::{Album, AlbumListType, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::config::AlbumSort;
use crate::services::library_db::{AlbumRow, LibraryDb};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::PlaylistsState;
use crate::state::session::{ConnectionStatus, Session};

const PAGE_SIZE: u32 = 100;
/// Load the next page when scrolled within this many pixels of the bottom.
const LOAD_AHEAD_PX: f32 = 600.;
/// How many cached placeholder cards get their cover looked up on seed.
/// The cache list can run to thousands of rows and every lookup is a stat;
/// the rest pick up art as the live pages overwrite them.
const CACHE_ART_PREFETCH: usize = 300;

/// Card text metrics. The line heights are explicit because gpui's default
/// line box for these font sizes clips descenders; the block height is fixed
/// so every card is the same size (a requirement of the virtualized rows).
const NAME_LINE_H: f32 = 20.;
const META_LINE_H: f32 = 17.;
const TEXT_BLOCK_H: f32 = NAME_LINE_H * 2. + META_LINE_H * 2.;

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
    /// `albums` still holds placeholders seeded from the last Navidrome sync;
    /// live pages overwrite them from the front instead of appending.
    cached: bool,
    /// How many entries of `albums` came from the server (only meaningful
    /// while `cached` — it's the write cursor for the overwrite).
    live_len: usize,
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

/// Render a cached DB row as an `Album` placeholder.
///
/// The sync stores ids namespaced (`navidrome:album:<id>`); strip that back off
/// so a placeholder card navigates and fetches art with the same id the live
/// listing would use — that's also what lets the cover survive the swap instead
/// of blanking and re-downloading.
fn album_from_row(row: AlbumRow) -> Album {
    let strip = |id: &str, prefix: &str| id.strip_prefix(prefix).unwrap_or(id).to_string();
    Album {
        id: strip(&row.id, "navidrome:album:"),
        name: row.title,
        artist: row.artist,
        artist_id: row
            .artist_id
            .as_deref()
            .map(|id| strip(id, "navidrome:artist:")),
        cover_art: row.cover_art,
        song_count: Some(row.song_count as u32),
        duration: Some(row.duration as u32),
        year: row.year,
        // Not stored by the sync; the placeholder never needs them (the tabs
        // that sort on these keys don't seed from cache — see `seed_from_cache`).
        created: None,
        genre: None,
        starred: None,
        user_rating: None,
        play_count: None,
    }
}

/// Merge a freshly-fetched page into a tab's display list.
///
/// Normally an append. While the tab still holds placeholders seeded from the
/// last sync, the page *overwrites* them from the front instead: the row count
/// stays put, so the scrollbar doesn't jump under the user while live pages
/// stream in. Once the server's list ends, any cached tail (albums deleted
/// server-side since the sync) is dropped.
///
/// `state.exhausted` must already be set for this page.
fn apply_live_page(state: &mut TabState, page: &[Album]) {
    if !state.cached {
        state.albums.extend_from_slice(page);
        return;
    }
    let start = state.live_len;
    let end = (start + page.len()).min(state.albums.len());
    state.albums.splice(start..end, page.iter().cloned());
    state.live_len += page.len();
    if state.exhausted {
        state.albums.truncate(state.live_len);
        state.cached = false;
    }
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
    /// Last Navidrome sync's albums, painted while the server request runs.
    library_db: Arc<LibraryDb>,
    /// Albums for each filter tab, loaded independently and lazily.
    tabs: HashMap<AlbumSort, TabState>,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight cover downloads, kept so they're cancelled when this view is
    /// dropped on navigation (instead of leaking and starving the next page).
    art_tasks: Vec<gpui::Task<()>>,
    /// A coalesced repaint is scheduled; batches a burst of cover arrivals
    /// into one re-render instead of one per completed download.
    art_repaint_pending: bool,
    active_tab: AlbumSort,
    /// Resolution thumbnails are currently fetched at; tracked so a cover-size
    /// change can drop stale art and refetch at the new resolution.
    art_px: u32,
    /// Virtualized row scroll handle: only visible rows are built/uploaded.
    scroll: UniformListScrollHandle,
    error: Option<String>,
}

impl EventEmitter<AlbumsEvent> for AlbumsView {}

impl AlbumsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        library_db: Arc<LibraryDb>,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_tab = session.read(cx).settings.album_sort;
        let art_px = session.read(cx).settings.cover_size.art_px();
        let mut this = Self {
            session,
            player,
            playlists,
            library_db,
            tabs: HashMap::new(),
            art_paths: HashMap::new(),
            art_tasks: Vec::new(),
            art_repaint_pending: false,
            active_tab,
            art_px,
            scroll: UniformListScrollHandle::new(),
            error: None,
        };
        this.seed_from_cache(active_tab, cx);
        this.load_more(active_tab, cx);
        this
    }

    /// Fill a tab with the last Navidrome sync's albums so the grid is
    /// populated on the first frame instead of after the server answers.
    ///
    /// Alphabetical only: the synced rows carry no timestamps, play counts or
    /// star state, so the other tabs' orderings can't be reproduced — and rows
    /// under the wrong heading are worse than an empty grid. `albums_by_source`
    /// already returns them in exactly this tab's order (`title COLLATE
    /// NOCASE`), which is what makes the in-place overwrite line up.
    fn seed_from_cache(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        // Gated on a *configured* server, not a connected one: this view is
        // built once before the connect completes and again after, and the
        // pre-connect pass is exactly the slow window worth filling.
        if tab != AlbumSort::All || self.session.read(cx).settings.server.is_none() {
            return;
        }
        // Only for "all libraries": the sync doesn't record which library a
        // row came from, so a filtered selection can't be honoured here.
        if !self.session.read(cx).library_ids.is_empty() {
            return;
        }
        if self.tabs.get(&tab).is_some_and(|t| !t.albums.is_empty()) {
            return;
        }
        let Ok(rows) = self.library_db.albums_by_source("navidrome") else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        let albums: Vec<Album> = rows.into_iter().map(album_from_row).collect();
        let state = self.tabs.entry(tab).or_default();
        state.cached = true;
        state.live_len = 0;
        state.albums = albums.clone();
        for album in albums.iter().take(CACHE_ART_PREFETCH) {
            self.fetch_art(album, cx);
        }
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn select_tab(&mut self, tab: AlbumSort, cx: &mut Context<Self>) {
        self.active_tab = tab;
        if self.tabs.get(&tab).is_none_or(|t| t.albums.is_empty()) {
            self.seed_from_cache(tab, cx);
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
                        apply_live_page(state, &new_albums);
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
        let base = self.scroll.0.borrow().base_handle.clone();
        let scrolled = -base.offset().y;
        let max = base.max_offset().height;
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
                    let _ = playlists.update(cx, |pl, cx| pl.add_songs(playlist_id, ids, cx));
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
        // Soft-cap the bag: oldest entries are the earliest-scrolled covers,
        // long since downloaded, so dropping their handles just frees memory.
        if self.art_tasks.len() > 256 {
            self.art_tasks.drain(0..128);
        }
        let task = cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, art_px).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_paths.insert(album_id, path);
                    view.schedule_art_repaint(cx);
                });
            }
        });
        self.art_tasks.push(task);
    }

    /// Coalesce cover-arrival repaints: a fast scroll completes many downloads
    /// in quick succession; batch them into ~one re-render per frame-ish rather
    /// than re-rendering the whole grid on every single completion.
    fn schedule_art_repaint(&mut self, cx: &mut Context<Self>) {
        if self.art_repaint_pending {
            return;
        }
        self.art_repaint_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.art_repaint_pending = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Drop cached thumbnail paths and refetch the active tab's art at the
    /// current `art_px` (called when the cover-size setting changes).
    fn refetch_art(&mut self, cx: &mut Context<Self>) {
        self.art_paths.clear();
        // Cancel in-flight downloads at the old resolution.
        self.art_tasks.clear();
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
        entity: &Entity<Self>,
        index: usize,
        album: &Album,
        tile: f32,
        cx: &App,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let name = album.name.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        // Right-click context menu data.
        let menu_id = album.id.clone();
        let menu_artist_id = album.artist_id.clone();
        let view = entity.clone();
        let open_view = entity.clone();
        let play_view = entity.clone();
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
            .on_click(move |_, _, cx: &mut App| {
                open_view.update(cx, |_, cx| cx.emit(AlbumsEvent::OpenAlbum(id.clone())));
            })
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
                                    .on_click(move |_, _, cx: &mut App| {
                                        play_view.update(cx, |this, cx| {
                                            this.queue_album(play_id.clone(), QueueMode::Play, cx);
                                        });
                                        cx.stop_propagation();
                                    }),
                            ),
                    ),
            )
            .child(
                v_flex()
                    // Fixed height (fits a two-line name + artist + optional
                    // year) so cards stay uniform — required for the
                    // virtualized row list. Line heights are set explicitly:
                    // the default line box is tight enough to clip descenders
                    // (y, g, j) inside the overflow-hidden text block.
                    .h(px(TEXT_BLOCK_H))
                    .gap_0()
                    .overflow_hidden()
                    .child(
                        div()
                            // Long titles wrap onto a second line instead of
                            // being cut mid-word; anything longer is clipped.
                            .max_h(px(NAME_LINE_H * 2.))
                            .overflow_hidden()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .child(name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(META_LINE_H))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(artist),
                    )
                    .when(!year.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .line_height(px(META_LINE_H))
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
                            return sub.item(PopupMenuItem::new("No playlists yet").disabled(true));
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
            .into_any_element()
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
        let base = self.scroll.0.borrow().base_handle.clone();
        let needs_fill = self
            .tabs
            .get(&active)
            .is_some_and(|t| !t.loading && !t.exhausted && !t.albums.is_empty())
            && base.max_offset().height <= px(0.);
        if needs_fill {
            self.load_more(active, cx);
        }

        let (album_count, fetching, cached) = self
            .tabs
            .get(&active)
            .map(|t| (t.albums.len(), t.loading, t.cached))
            .unwrap_or((0, false, false));
        // The connect itself is part of the wait: before it lands there is no
        // client to fetch with, so `loading` is false while the grid is still
        // very much not up to date.
        let connecting = self.session.read(cx).status == ConnectionStatus::Connecting;
        let loading = fetching || connecting;
        // Cards on screen are last sync's copy, not the server's answer yet.
        let showing_cache = cached && loading;

        // Columns from the measured viewport width; falls back to a guess on
        // the very first frame (before layout), then self-corrects.
        let width = f32::from(base.bounds().size.width);
        let card_w = tile + 12.;
        let gap = 16.;
        let cols = if width > 0. {
            (((width + gap) / (card_w + gap)).floor() as usize).max(1)
        } else {
            5
        };
        let row_count = album_count.div_ceil(cols);

        let entity = cx.entity();
        let grid = uniform_list("albums-grid", row_count, move |range, _window, cx| {
            let view = entity.read(cx);
            let Some(tab) = view.tabs.get(&active) else {
                return Vec::new();
            };
            range
                .map(|row| {
                    let start = row * cols;
                    let end = ((row + 1) * cols).min(tab.albums.len());
                    let cards: Vec<_> = tab.albums[start..end]
                        .iter()
                        .enumerate()
                        .map(|(j, album)| view.render_card(&entity, start + j, album, tile, cx))
                        .collect();
                    // Centered so the ragged last row's leftover space splits
                    // evenly — left/right gutters stay equal at any width.
                    h_flex()
                        .w_full()
                        .gap_4()
                        .justify_center()
                        .pb_3()
                        .children(cards)
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        })
        .flex_1()
        .px_4()
        .track_scroll(self.scroll.clone())
        .on_scroll_wheel(cx.listener(|this, _, _, cx| {
            this.maybe_load_more_on_scroll(cx);
        }));

        // No bottom padding: the grid runs to the window edge so rows slide
        // under the player bar instead of stopping short of it with a gap.
        v_flex()
            .id("albums-scroll")
            .size_full()
            .relative()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .child(div().text_lg().child("Albums"))
                    .child(tabs)
                    // Spinner sits in the header rather than over the grid so
                    // it's visible while cached cards are already filling the
                    // page — otherwise a stale-but-complete grid looks final.
                    .when(loading, |this| {
                        this.child(
                            h_flex()
                                .items_center()
                                .gap_1p5()
                                .child(Spinner::new().xsmall().color(cx.theme().muted_foreground))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if showing_cache {
                                            "Updating from server…"
                                        } else {
                                            "Loading…"
                                        }),
                                ),
                        )
                    }),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(
                    div()
                        .px_4()
                        .text_color(cx.theme().danger)
                        .text_sm()
                        .child(e),
                )
            })
            .child(grid)
            // Pagination indicator, floated over the grid's bottom edge so it
            // doesn't shorten the scroll area. Suppressed while cached cards
            // are showing — there the rows are already there and the header
            // spinner is the honest signal.
            .when(loading && !showing_cache, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .right_0()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Loading…"),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str) -> Album {
        Album {
            id: id.into(),
            name: id.into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: None,
            duration: None,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
        }
    }

    fn ids(state: &TabState) -> Vec<String> {
        state.albums.iter().map(|a| a.id.clone()).collect()
    }

    fn seeded(n: usize) -> TabState {
        TabState {
            albums: (0..n).map(|i| album(&format!("c{i}"))).collect(),
            cached: true,
            ..Default::default()
        }
    }

    #[test]
    fn uncached_pages_append() {
        let mut state = TabState::default();
        apply_live_page(&mut state, &[album("a"), album("b")]);
        apply_live_page(&mut state, &[album("c")]);
        assert_eq!(ids(&state), ["a", "b", "c"]);
        assert!(!state.cached);
    }

    #[test]
    fn live_pages_overwrite_cache_without_changing_row_count() {
        let mut state = seeded(6);
        apply_live_page(&mut state, &[album("l0"), album("l1")]);
        // Same length: the scroll extent must not move mid-load.
        assert_eq!(ids(&state), ["l0", "l1", "c2", "c3", "c4", "c5"]);
        apply_live_page(&mut state, &[album("l2"), album("l3")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2", "l3", "c4", "c5"]);
        assert!(state.cached);
    }

    #[test]
    fn exhausted_page_drops_the_stale_cached_tail() {
        let mut state = seeded(6);
        state.exhausted = true;
        apply_live_page(&mut state, &[album("l0"), album("l1")]);
        assert_eq!(ids(&state), ["l0", "l1"]);
        assert!(
            !state.cached,
            "cache is fully replaced once the server ends"
        );
    }

    #[test]
    fn live_list_longer_than_cache_grows_past_it() {
        let mut state = seeded(2);
        apply_live_page(&mut state, &[album("l0"), album("l1"), album("l2")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2"]);
        // Subsequent pages append normally once past the cached tail.
        apply_live_page(&mut state, &[album("l3")]);
        assert_eq!(ids(&state), ["l0", "l1", "l2", "l3"]);
    }

    #[test]
    fn empty_final_page_truncates_to_what_the_server_sent() {
        let mut state = seeded(4);
        apply_live_page(&mut state, &[album("l0")]);
        state.exhausted = true;
        apply_live_page(&mut state, &[]);
        assert_eq!(ids(&state), ["l0"]);
    }

    #[test]
    fn cached_rows_strip_the_sync_id_namespace() {
        let row = AlbumRow {
            id: "navidrome:album:42".into(),
            source: "navidrome".into(),
            title: "Kid A".into(),
            artist: Some("Radiohead".into()),
            artist_id: Some("navidrome:artist:7".into()),
            year: Some(2000),
            cover_art: Some("al-42".into()),
            song_count: 10,
            duration: 2000.0,
        };
        let a = album_from_row(row);
        // Ids must match the live listing's, or the swap reloads every cover
        // and the cards navigate to nothing.
        assert_eq!(a.id, "42");
        assert_eq!(a.artist_id.as_deref(), Some("7"));
        assert_eq!(a.name, "Kid A");
    }
}
