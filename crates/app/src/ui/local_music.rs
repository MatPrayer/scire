//! Local music album grid. Reads from SQLite DB; background scan populates
//! the DB async so results appear as they're indexed.
// ponytail: no sort/filter tabs (unlike AlbumsView). All local albums shown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, SharedString, UniformListScrollHandle,
    Window, div, img, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::assets::{app_icon, icons};
use crate::services::artwork;
use crate::services::library_db::{AlbumRow, LibraryDb, LibraryStats};
use crate::services::local_library::local_art_path;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::{CARD_PADDING, with_focus_cursor};

/// Column guess before the grid has measured itself.
const FALLBACK_COLS: usize = 5;
/// Cover rows resolved beyond each viewport edge.
const ART_LOOKAHEAD_ROWS: usize = 2;
/// Fixed card text metrics required by `uniform_list`.
const NAME_LINE_H: f32 = 20.;
const META_LINE_H: f32 = 17.;
const TEXT_BLOCK_H: f32 = NAME_LINE_H + META_LINE_H * 3.;

fn viewport_card_range(
    row_count: usize,
    cols: usize,
    album_count: usize,
    viewport: f32,
    content: f32,
    scrolled: f32,
) -> Option<(usize, usize)> {
    if row_count == 0 || cols == 0 || album_count == 0 {
        return None;
    }
    let (first_row, last_row) = if viewport > 0. && content > 0. {
        let row_h = content / row_count as f32;
        (
            (scrolled.max(0.) / row_h).floor() as usize,
            ((scrolled.max(0.) + viewport) / row_h).ceil() as usize,
        )
    } else {
        (0, ART_LOOKAHEAD_ROWS)
    };
    let start = first_row.saturating_sub(ART_LOOKAHEAD_ROWS) * cols;
    let end = ((last_row + 1 + ART_LOOKAHEAD_ROWS) * cols).min(album_count);
    (start < end).then_some((start, end))
}

fn clamped_cursor(cursor: Option<usize>, album_count: usize) -> Option<usize> {
    cursor
        .map(|index| index.min(album_count.saturating_sub(1)))
        .filter(|_| album_count > 0)
}

fn cursor_row(index: usize, cols: usize) -> usize {
    index / cols.max(1)
}

/// How a context-menu action should enqueue an album's songs.
#[derive(Clone, Copy)]
enum QueueMode {
    Play,
    Shuffle,
    PlayNext,
    Enqueue,
}

#[derive(Clone)]
pub enum LocalMusicEvent {
    OpenAlbum(String),
}

pub struct LocalMusicView {
    session: Entity<Session>,
    db: Arc<LibraryDb>,
    player: Entity<PlayerState>,
    albums: Vec<AlbumRow>,
    art_paths: HashMap<String, PathBuf>,
    /// In-flight cover jobs by album id, so a card that scrolls away can have
    /// its job cancelled and a card that comes back can ask again.
    art_tasks: HashMap<String, gpui::Task<()>>,
    art_repaint_pending: bool,
    art_px: u32,
    art_range: Option<(usize, usize)>,
    scroll: UniformListScrollHandle,
    /// Last seen scan_version — reload albums only when it changes.
    scan_version: u64,
    /// Album index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Catalog totals shown in the header; recomputed with `refresh`, so a
    /// finished scan updates them along with the grid.
    stats: LibraryStats,
    /// Tracks grid width against window width for same-frame resize reflow.
    live_width: crate::ui::LiveWidth,
    /// Per-album accent colours for `Settings::selection_glow_album_color`,
    /// decoded lazily for rendered cards only.
    glow_accents: RefCell<HashMap<String, gpui::Hsla>>,
}

impl EventEmitter<LocalMusicEvent> for LocalMusicView {}

impl LocalMusicView {
    pub fn new(
        session: Entity<Session>,
        db: Arc<LibraryDb>,
        player: Entity<PlayerState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let art_px = artwork::bucket(session.read(cx).settings.cover_size.art_px());
        let mut view = Self {
            session,
            db,
            player,
            albums: Vec::new(),
            art_paths: HashMap::new(),
            art_tasks: HashMap::new(),
            art_repaint_pending: false,
            art_px,
            art_range: None,
            scroll: UniformListScrollHandle::new(),
            scan_version: 0,
            vi_cursor: None,
            stats: LibraryStats::default(),
            live_width: crate::ui::LiveWidth::default(),
            glow_accents: RefCell::new(HashMap::new()),
        };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.albums = self.db.albums_by_source("local").unwrap_or_default();
        self.scan_version = self.db.scan_version();
        // Cover hashes can change after a cache rebuild. Retaining these maps
        // leaves cards pointing at deleted files and GPUI keeps the miss.
        self.art_paths.clear();
        self.art_tasks.clear();
        self.art_range = None;
        self.glow_accents.borrow_mut().clear();
        self.vi_cursor = clamped_cursor(self.vi_cursor, self.albums.len());
        // Local rows carry no library provenance — the folder list is the
        // whole local library, so there is no subset to filter to.
        self.stats = self.db.library_stats("local", &[]).unwrap_or_default();
        cx.notify();
    }

    fn grid_cols(&mut self, window: &Window, cx: &gpui::App) -> usize {
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let (min_tile, max_tile) = self.session.read(cx).settings.cover_size.range();
        self.live_width
            .grid(measured, min_tile, max_tile, window, FALLBACK_COLS)
            .0
    }

    fn fetch_art(&mut self, album: &AlbumRow, cx: &mut Context<Self>) {
        if self.art_paths.contains_key(&album.id) {
            return;
        }
        let Some(hash) = album.cover_art.clone() else {
            return;
        };
        let key = artwork::local_cover_key(&hash);
        if let Some(path) = artwork::cached(&key, self.art_px) {
            self.art_paths.insert(album.id.clone(), path);
            return;
        }
        // If the cover-size setting moved to another rung, keep the existing
        // derivative visible while the requested one is generated.
        if let Some(path) = artwork::cached_best(&key, self.art_px) {
            self.art_paths.insert(album.id.clone(), path);
        }
        let Some(source) = local_art_path(&hash).filter(|path| path.exists()) else {
            return;
        };
        // A range that grows by a row re-asks for every card it already
        // covers; without this the same cover is decoded once per pass.
        if self.art_tasks.contains_key(&album.id) {
            return;
        }
        let album_id = album.id.clone();
        let art_px = self.art_px;
        let task = cx.spawn(async move |this, cx| {
            // Formats outside the thumbnail decoder's JPEG/PNG feature set
            // keep the original path rather than losing a cover GPUI may
            // still know how to display.
            let path = artwork::thumbnail_file(&source, &key, art_px)
                .await
                .unwrap_or(source);
            let _ = this.update(cx, |view, cx| {
                // A scan may have replaced this album's cover while the
                // derivative was being built.
                let current = view
                    .albums
                    .iter()
                    .find(|album| album.id == album_id)
                    .and_then(|album| album.cover_art.as_deref());
                if current == Some(hash.as_str()) {
                    view.art_paths.insert(album_id, path);
                    view.schedule_art_repaint(cx);
                }
            });
        });
        self.art_tasks.insert(album.id.clone(), task);
    }

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

    fn refetch_art(&mut self) {
        self.art_paths.clear();
        self.art_tasks.clear();
        self.art_range = None;
    }

    fn ensure_art_for_viewport(&mut self, row_count: usize, cols: usize, cx: &mut Context<Self>) {
        let base = self.scroll.0.borrow().base_handle.clone();
        let viewport = f32::from(base.bounds().size.height);
        let content = f32::from(base.max_offset().height) + viewport;
        let scrolled = f32::from(-base.offset().y);
        let Some(range) = viewport_card_range(
            row_count,
            cols,
            self.albums.len(),
            viewport,
            content,
            scrolled,
        ) else {
            return;
        };
        if self.art_range == Some(range) {
            return;
        }
        self.art_range = Some(range);
        let albums = self.albums[range.0..range.1].to_vec();
        // Keep only the window's own jobs. Dropping a `Task` cancels it, so
        // what is thrown away here is work for cards that have scrolled out of
        // reach — and dropping the entry is also what lets the card ask again
        // if it comes back. Capping a flat list instead cancelled by *age*,
        // which during a fast scroll is whatever was queued first: the cards
        // now on screen, left blank with nothing to re-request them.
        let keep: std::collections::HashSet<&str> =
            albums.iter().map(|album| album.id.as_str()).collect();
        self.art_tasks.retain(|id, _| keep.contains(id.as_str()));
        for album in &albums {
            self.fetch_art(album, cx);
        }
    }

    fn queue_album(&mut self, album_id: String, mode: QueueMode, cx: &mut Context<Self>) {
        let Ok(tracks) = self.db.tracks_by_album(&album_id) else {
            return;
        };
        let songs: Vec<_> = tracks.into_iter().map(|t| t.into_song()).collect();
        if songs.is_empty() {
            return;
        }
        self.player.update(cx, |p, cx| match mode {
            QueueMode::Play => p.play_queue(songs, 0, cx),
            QueueMode::Shuffle => p.play_queue_shuffled(songs, cx),
            QueueMode::PlayNext => p.play_next(songs, cx),
            QueueMode::Enqueue => p.enqueue(songs, cx),
        });
    }

    /// Move the vi-mode cursor by `delta` cards and scroll its virtual row in.
    pub fn vi_move(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.albums.is_empty() {
            return;
        }
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(self.albums.len() - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        let cols = self.grid_cols(window, cx).max(1);
        self.scroll
            .scroll_to_item(cursor_row(next, cols), gpui::ScrollStrategy::Top);
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
    }

    /// Open the album under the vi-mode cursor.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .vi_cursor
            .and_then(|c| self.albums.get(c))
            .map(|a| a.id.clone())
        else {
            return;
        };
        cx.emit(LocalMusicEvent::OpenAlbum(id));
    }

    pub fn vi_play(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.queue_album(id, QueueMode::Play, cx);
        }
    }

    pub fn vi_shuffle(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.queue_album(id, QueueMode::Shuffle, cx);
        }
    }

    fn focused_album_id(&self) -> Option<String> {
        self.vi_cursor
            .and_then(|c| self.albums.get(c))
            .map(|a| a.id.clone())
    }

    fn render_card(
        &self,
        entity: &Entity<Self>,
        index: usize,
        album: &AlbumRow,
        tile: f32,
        focused: bool,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = id.clone();
        let art = self.art_paths.get(&id).cloned();
        let name = album.title.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        let sc = album.song_count;
        let view = entity.clone();
        let open_view = entity.clone();
        let play_view = entity.clone();
        let open_id = id.clone();
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        let accent = if self.session.read(cx).settings.selection_glow_album_color {
            art.as_ref().and_then(|path| {
                crate::ui::album_glow_accent(&mut self.glow_accents.borrow_mut(), &album.id, path)
            })
        } else {
            None
        };

        let card = v_flex()
            .id(SharedString::from(format!("local-album-{}", album.id)))
            .group("lcard")
            .w(px(tile + CARD_PADDING))
            .p_1p5()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, accent, cx)
                } else {
                    s
                }
            })
            .active(|s| s.opacity(0.8))
            .on_click(move |_, _, cx: &mut gpui::App| {
                open_view.update(cx, |_, cx| {
                    cx.emit(LocalMusicEvent::OpenAlbum(open_id.clone()))
                });
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
                    .child(
                        div()
                            .absolute()
                            .bottom_2()
                            .right_2()
                            .opacity(0.)
                            .group_hover("lcard", |s| s.opacity(1.))
                            .child(
                                Button::new(("lcard-play", index))
                                    .primary()
                                    .icon(app_icon(icons::PLAY))
                                    .on_click(move |_, _, cx: &mut gpui::App| {
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
                    .h(px(TEXT_BLOCK_H))
                    .gap_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .line_height(px(NAME_LINE_H))
                            .truncate()
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
                    .when(sc > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .line_height(px(META_LINE_H))
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{sc} tracks")),
                        )
                    })
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
            .context_menu(move |menu, _window, _cx| {
                let act = |mode: QueueMode| {
                    let view = view.clone();
                    let aid = id.clone();
                    move |_: &_, _: &mut Window, cx: &mut gpui::App| {
                        view.update(cx, |v, cx| v.queue_album(aid.clone(), mode, cx));
                    }
                };
                menu.item(PopupMenuItem::new("Play").on_click(act(QueueMode::Play)))
                    .item(PopupMenuItem::new("Shuffle").on_click(act(QueueMode::Shuffle)))
                    .item(PopupMenuItem::new("Play next").on_click(act(QueueMode::PlayNext)))
                    .item(PopupMenuItem::new("Add to queue").on_click(act(QueueMode::Enqueue)))
            });
        with_focus_cursor(format!("vi-focus-{index}"), card, focused, glow, accent, cx)
    }
}

impl Render for LocalMusicView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cover = self.session.read(cx).settings.cover_size;
        let want_px = artwork::bucket(cover.art_px());
        if want_px != self.art_px {
            self.art_px = want_px;
            self.refetch_art();
        }
        // Refresh only when scan completed (scan_version bumped).
        let cur_ver = self.db.scan_version();
        if cur_ver != self.scan_version {
            self.refresh(cx);
        }

        if self.albums.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .text_sm()
                        .child("No local music found"),
                )
                .into_any_element();
        }

        let (min_tile, max_tile) = cover.range();
        let measured = f32::from(self.scroll.0.borrow().base_handle.bounds().size.width);
        let (cols, tile) =
            self.live_width
                .grid(measured, min_tile, max_tile, window, FALLBACK_COLS);
        let row_count = self.albums.len().div_ceil(cols);
        self.ensure_art_for_viewport(row_count, cols, cx);

        let entity = cx.entity();
        let grid = uniform_list("local-music-grid", row_count, move |range, _window, cx| {
            let view = entity.read(cx);
            range
                .map(|row| {
                    let start = row * cols;
                    let end = ((row + 1) * cols).min(view.albums.len());
                    let cards = view.albums[start..end]
                        .iter()
                        .enumerate()
                        .map(|(offset, album)| {
                            let index = start + offset;
                            view.render_card(
                                &entity,
                                index,
                                album,
                                tile,
                                view.vi_cursor == Some(index),
                                cx,
                            )
                        })
                        .collect::<Vec<_>>();
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
        .track_scroll(self.scroll.clone());

        v_flex()
            .id("local-music-scroll")
            .size_full()
            .pt_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .px_4()
                    .child(div().child("Local Music"))
                    .child(div().flex_1())
                    // Nothing to summarise until a sync has written rows —
                    // zeros next to a grid full of live cards read as a bug.
                    .when(self.stats.albums > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::ui::library_summary(
                                    (self.stats.albums, "album"),
                                    &self.stats,
                                )),
                        )
                    }),
            )
            .child(grid)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_range_includes_lookahead_and_clamps() {
        assert_eq!(
            viewport_card_range(20, 5, 100, 300., 2000., 700.),
            Some((25, 65))
        );
        assert_eq!(
            viewport_card_range(20, 5, 92, 300., 2000., 1800.),
            Some((80, 92))
        );
    }

    #[test]
    fn viewport_range_guesses_first_rows_before_layout() {
        assert_eq!(viewport_card_range(20, 5, 100, 0., 0., 0.), Some((0, 25)));
    }

    #[test]
    fn cursor_clamps_after_refresh() {
        assert_eq!(clamped_cursor(Some(9), 4), Some(3));
        assert_eq!(clamped_cursor(Some(2), 4), Some(2));
        assert_eq!(clamped_cursor(Some(2), 0), None);
        assert_eq!(clamped_cursor(None, 4), None);
        assert_eq!(cursor_row(11, 5), 2);
        assert_eq!(cursor_row(11, 0), 11);
    }
}
