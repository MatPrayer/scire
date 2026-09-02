//! Local music album grid. Reads from SQLite DB; background scan populates
//! the DB async so results appear as they're indexed.
// ponytail: no sort/filter tabs (unlike AlbumsView). All local albums shown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, SharedString,
    Window, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::assets::{app_icon, icons};
use crate::services::library_db::{AlbumRow, LibraryDb};
use crate::services::local_library::local_art_path;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::{focus_glow, with_focus_animation};

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
    db: Arc<LibraryDb>,
    player: Entity<PlayerState>,
    session: Entity<Session>,
    albums: Vec<AlbumRow>,
    art_paths: HashMap<String, PathBuf>,
    scroll: ScrollHandle,
    /// Last seen scan_version — reload albums only when it changes.
    scan_version: u64,
    /// Album index under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Scrolls the focused card into view (works for wrapped grids, where
    /// the cards are nested inside a flex-wrap container).
    focus_anchor: ScrollAnchor,
}

impl EventEmitter<LocalMusicEvent> for LocalMusicView {}

impl LocalMusicView {
    pub fn new(
        db: Arc<LibraryDb>,
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroll = ScrollHandle::new();
        let mut view = Self {
            db,
            player,
            session,
            albums: Vec::new(),
            art_paths: HashMap::new(),
            scroll: scroll.clone(),
            scan_version: 0,
            vi_cursor: None,
            focus_anchor: ScrollAnchor::for_handle(scroll),
        };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.albums = self.db.albums_by_source("local").unwrap_or_default();
        self.scan_version = self.db.scan_version();
        // Pre-populate art paths for covers already cached on disk.
        for a in &self.albums {
            if !self.art_paths.contains_key(&a.id)
                && let Some(ref hash) = a.cover_art
                && let Some(path) = local_art_path(hash)
                && path.exists()
            {
                self.art_paths.insert(a.id.clone(), path);
            }
        }
        cx.notify();
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

    /// Move the vi-mode cursor by `delta` cards, clamping and scrolling the
    /// focused card into view (via a ScrollAnchor, since the cards are nested
    /// in a flex-wrap container and have no stable scroll child index).
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
        self.focus_anchor.scroll_to(window, cx);
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

    fn render_card(
        &self,
        index: usize,
        album: &AlbumRow,
        tile: f32,
        focused: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = id.clone();
        let hover_glow = self.session.read(cx).settings.hover_glow;
        let art = self.art_paths.get(&id).cloned();
        let name = album.title.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        let sc = album.song_count;
        let view = cx.entity();

        let card = v_flex()
            .id(SharedString::from(format!("local-album-{}", album.id)))
            .group("lcard")
            .w(px(tile + 12.))
            .p_1p5()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    s.shadow(focus_glow(cx))
                } else {
                    s
                }
            })
            .when(focused, |s| {
                s.border_color(cx.theme().primary)
                    .shadow(focus_glow(cx))
                    .anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .on_click(cx.listener({
                let click_id = id.clone();
                move |_this, _, _, cx| {
                    cx.emit(LocalMusicEvent::OpenAlbum(click_id.clone()));
                }
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
                    .when(sc > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{sc} tracks")),
                        )
                    })
                    .when(!year.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
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
        if focused {
            with_focus_animation(format!("vi-focus-{index}"), card, cx).into_any_element()
        } else {
            card.into_any_element()
        }
    }
}

impl Render for LocalMusicView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Refresh only when scan completed (scan_version bumped).
        let cur_ver = self.db.scan_version();
        if cur_ver != self.scan_version {
            self.refresh(cx);
        }

        let tile = 160_f32; // ponytail: fixed tile size, make configurable later

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

        let cards: Vec<_> = self
            .albums
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let focused = self.vi_cursor == Some(i);
                self.render_card(i, a, tile, focused, cx)
            })
            .collect();

        v_flex()
            .id("local-music-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .child(div().child(format!("Local Music  ({})", self.albums.len()))),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_center()
                    .gap_4()
                    .children(cards),
            )
            .into_any_element()
    }
}
