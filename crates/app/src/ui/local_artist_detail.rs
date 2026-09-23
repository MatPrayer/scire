//! Local artist detail page backed entirely by LibraryDb.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle, SharedString,
    Window, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use crate::assets::{app_icon, icons};
use crate::services::library_db::{AlbumRow, ArtistRow, LibraryDb};
use crate::services::local_library::local_art_path;
use crate::state::player::PlayerState;
use crate::state::session::Session;
use crate::ui::{card_inset, card_padding, sync_focus_scroll, with_focus_cursor};

pub enum LocalArtistEvent {
    OpenAlbum(String),
}

pub struct LocalArtistDetailView {
    db: Arc<LibraryDb>,
    player: Entity<PlayerState>,
    session: Entity<Session>,
    artist_id: String,
    artist: Option<ArtistRow>,
    albums: Vec<AlbumRow>,
    art_paths: HashMap<String, PathBuf>,
    artist_art: Option<PathBuf>,
    scan_version: u64,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
}

impl EventEmitter<LocalArtistEvent> for LocalArtistDetailView {}

impl LocalArtistDetailView {
    pub fn new(
        db: Arc<LibraryDb>,
        player: Entity<PlayerState>,
        session: Entity<Session>,
        artist_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let scroll = ScrollHandle::new();
        let mut view = Self {
            db,
            player,
            session,
            artist_id,
            artist: None,
            albums: Vec::new(),
            art_paths: HashMap::new(),
            artist_art: None,
            scan_version: 0,
            focus_anchor: ScrollAnchor::for_handle(scroll.clone()),
            scroll,
            vi_cursor: None,
            vi_scroll_synced: None,
        };
        view.load(cx);
        view
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.artist = self
            .db
            .artist_by_id("local", &self.artist_id)
            .unwrap_or_default();
        self.albums = self
            .db
            .albums_by_artist("local", &self.artist_id)
            .unwrap_or_default();
        self.scan_version = self.db.scan_version();
        self.vi_cursor = self
            .vi_cursor
            .map(|i| i.min(self.albums.len().saturating_sub(1)))
            .filter(|_| !self.albums.is_empty());
        self.vi_scroll_synced = None;

        self.art_paths.clear();
        for album in &self.albums {
            if let Some(path) = album
                .cover_art
                .as_deref()
                .and_then(local_art_path)
                .filter(|path| path.exists())
            {
                self.art_paths.insert(album.id.clone(), path);
            }
        }
        self.artist_art = self
            .artist
            .as_ref()
            .and_then(|artist| artist.cover_art.as_deref())
            .and_then(local_art_path)
            .filter(|path| path.exists());
        cx.notify();
    }

    fn album_songs(&self, album_id: &str) -> Vec<subsonic::Song> {
        self.db
            .tracks_by_album(album_id)
            .unwrap_or_default()
            .into_iter()
            .map(|track| track.into_song())
            .collect()
    }

    fn play_album(&mut self, album_id: String, shuffle: bool, cx: &mut Context<Self>) {
        let songs = self.album_songs(&album_id);
        if songs.is_empty() {
            return;
        }
        self.player.update(cx, |player, cx| {
            if shuffle {
                player.play_queue_shuffled(songs, cx);
            } else {
                player.play_queue(songs, 0, cx);
            }
        });
    }

    fn play_all(&mut self, shuffle: bool, cx: &mut Context<Self>) {
        let songs: Vec<_> = self
            .albums
            .iter()
            .flat_map(|album| self.album_songs(&album.id))
            .collect();
        if songs.is_empty() {
            return;
        }
        self.player.update(cx, |player, cx| {
            if shuffle {
                player.play_queue_shuffled(songs, cx);
            } else {
                player.play_queue(songs, 0, cx);
            }
        });
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        if self.albums.is_empty() {
            return;
        }
        let current = self.vi_cursor.unwrap_or(0);
        self.vi_cursor = Some(if delta > 0 {
            (current + delta as usize).min(self.albums.len() - 1)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        });
        cx.notify();
    }

    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.focused_album_id() else {
            return;
        };
        cx.emit(LocalArtistEvent::OpenAlbum(id));
    }

    pub fn vi_play(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.play_album(id, false, cx);
        }
    }

    pub fn vi_shuffle(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.focused_album_id() {
            self.play_album(id, true, cx);
        }
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            self.vi_scroll_synced = None;
            cx.notify();
        }
    }

    fn focused_album_id(&self) -> Option<String> {
        self.vi_cursor
            .and_then(|index| self.albums.get(index))
            .map(|album| album.id.clone())
    }

    fn render_card(
        &self,
        album: &AlbumRow,
        index: usize,
        tile: f32,
        focused: bool,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let id = album.id.clone();
        let play_id = id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let anchor = self.focus_anchor.clone();
        let glow = self.session.read(cx).settings.selection_glow_vi;

        // Same trade as the album grid's cards: the cover takes the card's
        // chrome for itself, the card's width is unchanged.
        let flush = self.session.read(cx).settings.flush_album_covers;
        let cover = crate::ui::card_cover_edge(tile, flush);

        let card = v_flex()
            .id(SharedString::from(format!(
                "local-artist-album-{}",
                album.id
            )))
            .group("local-artist-card")
            .w(px(tile + card_padding()))
            .map(|c| match flush {
                true => c,
                false => c
                    .p(px(card_inset()))
                    .border_1()
                    .border_color(gpui::hsla(0., 0., 0.5, 0.15)),
            })
            .gap_1p5()
            .rounded_lg()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().muted))
            .active(|style| style.opacity(0.8))
            .when(focused, |style| style.anchor_scroll(Some(anchor)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(LocalArtistEvent::OpenAlbum(id.clone()));
            }))
            .child(
                div()
                    .size(px(cover))
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .relative()
                    .when_some(art, |this, path| {
                        this.child(img(path).size(px(cover)).rounded_lg())
                    })
                    .child(
                        div()
                            .absolute()
                            .bottom_2()
                            .right_2()
                            .opacity(0.)
                            .group_hover("local-artist-card", |style| style.opacity(1.))
                            .child(
                                Button::new(("local-artist-play", index))
                                    .primary()
                                    .icon(app_icon(icons::PLAY))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.play_album(play_id.clone(), false, cx);
                                        cx.stop_propagation();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    // Flush, the card kept no padding for the text to sit in.
                    .map(|t| match flush {
                        true => t.px(px(card_inset())).pb(px(card_inset())),
                        false => t,
                    })
                    .child(div().text_sm().truncate().child(album.title.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                album
                                    .year
                                    .map(|year| year.to_string())
                                    .unwrap_or_else(|| format!("{} tracks", album.song_count)),
                            ),
                    ),
            );
        with_focus_cursor(
            format!("vi-local-artist-album-{index}"),
            card,
            focused,
            glow,
            None,
            cx,
        )
    }
}

impl Render for LocalArtistDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.db.scan_version() != self.scan_version {
            self.load(cx);
        }
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );

        let name = self
            .artist
            .as_ref()
            .map(|artist| artist.name.clone())
            .unwrap_or_else(|| "Local artist".into());
        let tracks: i64 = self.albums.iter().map(|album| album.song_count).sum();
        let album_count = self.albums.len();
        let cover = self
            .session
            .read(cx)
            .settings
            .artist_album_size
            .resolve(self.session.read(cx).settings.cover_size);
        let tile = cover.wrap_tile();
        let cards: Vec<_> = self
            .albums
            .iter()
            .enumerate()
            .map(|(index, album)| {
                self.render_card(album, index, tile, self.vi_cursor == Some(index), cx)
            })
            .collect();

        v_flex()
            .id("local-artist-detail")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_5()
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(
                        div()
                            .size(px(128.))
                            .flex_none()
                            .rounded_full()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .when_some(self.artist_art.clone(), |this, path| {
                                this.child(img(path).size(px(128.)).rounded_full())
                            }),
                    )
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().text_2xl().font_semibold().child(name))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "{album_count} {} · {tracks} {}",
                                        if album_count == 1 { "album" } else { "albums" },
                                        if tracks == 1 { "track" } else { "tracks" }
                                    )),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("local-artist-play-all")
                                            .primary()
                                            .icon(app_icon(icons::PLAY))
                                            .label("Play")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.play_all(false, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("local-artist-shuffle-all")
                                            .outline()
                                            .label("Shuffle")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.play_all(true, cx)
                                            })),
                                    ),
                            ),
                    ),
            )
            .child(div().text_lg().child("Albums"))
            .child(h_flex().w_full().gap_4().flex_wrap().children(cards))
    }
}
