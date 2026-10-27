//! Playlist page: cover header with details, play/shuffle, edit, duplicate,
//! public switch and delete; a filterable, sortable track list whose rows
//! drag into a new order.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, Focusable as _, IntoElement, Render, ScrollAnchor,
    ScrollHandle, SharedString, Window, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::{Playlist, PlaylistWithSongs, Song, SubsonicClient};

use crate::assets::{app_icon, icons};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::playlists::{PlaylistsState, copy_name};
use crate::state::session::Session;
use crate::ui::{format_duration, format_playtime, sync_focus_scroll, with_focus_cursor};

/// Row height; also what the art window estimates visible rows from.
const ROW_H: f32 = 52.;
/// Track thumbnails: 40px drawn, fetched at the smallest rung.
const THUMB: f32 = 40.;
const THUMB_ART: u32 = 64;
/// Header cover edge.
const COVER: f32 = 184.;
const COVER_ART: u32 = 512;
/// Rows past the visible ones whose art is fetched ahead of the scroll.
const ART_LOOKAHEAD: usize = 24;
const ALBUM_W: f32 = 220.;
const DURATION_W: f32 = 52.;
/// How long the armed Delete button waits for its second press.
const DELETE_ARM: Duration = Duration::from_secs(4);

pub enum PlaylistDetailEvent {
    /// Playlist was deleted — navigate away.
    Deleted,
    OpenAlbum(String),
    OpenArtist(String),
}

/// How the track list is ordered on screen. Only `Custom` is the playlist's
/// own order; the others are a view of it until saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackSort {
    Custom,
    Title,
    Artist,
    Album,
    Duration,
}

impl TrackSort {
    const ALL: [TrackSort; 5] = [
        TrackSort::Custom,
        TrackSort::Title,
        TrackSort::Artist,
        TrackSort::Album,
        TrackSort::Duration,
    ];

    fn label(self) -> &'static str {
        match self {
            TrackSort::Custom => "Playlist order",
            TrackSort::Title => "Title",
            TrackSort::Artist => "Artist",
            TrackSort::Album => "Album",
            TrackSort::Duration => "Length",
        }
    }
}

/// A row being dragged: its position in the playlist and what the ghost says.
#[derive(Clone)]
struct DraggedTrack {
    from: usize,
    title: SharedString,
}

/// The ghost that follows the pointer during a drag.
struct DragGhost {
    title: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_md()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .text_sm()
            .child(app_icon(icons::MUSIC).small())
            .child(self.title.clone())
    }
}

pub struct PlaylistDetailView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    playlists: Entity<PlaylistsState>,
    playlist_id: String,
    playlist: Option<PlaylistWithSongs>,
    /// Bumped by every load and every local edit; an answer from an older
    /// load is dropped, so a reorder shown at once is not undone by a fetch
    /// that left before it.
    generation: u64,
    /// `PlaylistsState::revision` this page last loaded at.
    seen_revision: u64,
    editing: bool,
    name_input: Entity<InputState>,
    desc_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    sort: TrackSort,
    descending: bool,
    /// Song indices in display order (after filter and sort).
    visible: Vec<usize>,
    delete_armed: bool,
    error: Option<crate::errors::ErrorNote>,
    cover: Option<PathBuf>,
    cover_key: Option<String>,
    /// Album-scoped art key → cached thumbnail.
    thumbs: HashMap<String, PathBuf>,
    thumbs_pending: HashSet<String>,
    /// Id of the playing song — the only thing this view draws out of the
    /// player, so the only change worth repainting for.
    playing_id: Option<String>,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Position in `visible` under the vi-mode cursor (None = cursor hidden).
    vi_cursor: Option<usize>,
    /// Cursor position the scroll has caught up to, so `render` scrolls only
    /// when the cursor actually moved (`ui::sync_focus_scroll`).
    vi_scroll_synced: Option<usize>,
}

impl EventEmitter<PlaylistDetailEvent> for PlaylistDetailView {}

impl PlaylistDetailView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        playlists: Entity<PlaylistsState>,
        playlist_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Playlist name"));
        let desc_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Add a description (optional)"));
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter this playlist"));
        for input in [&name_input, &desc_input] {
            cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_details(cx);
                }
            })
            .detach();
        }
        cx.subscribe(
            &filter_input,
            |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh_visible(cx);
                    this.vi_cursor = None;
                    cx.notify();
                }
            },
        )
        .detach();

        // Re-ask the server when an edit lands (here or from a menu elsewhere);
        // a plain list reload only repaints the header from the listing.
        cx.observe(&playlists, |this: &mut Self, playlists, cx| {
            let revision = playlists.read(cx).revision;
            if revision != this.seen_revision {
                this.seen_revision = revision;
                this.load(cx);
            }
            this.want_cover(cx);
            cx.notify();
        })
        .detach();
        // Re-render when the playing song changes so the highlight stays
        // fresh — and only then. PlayerState notifies on every event, position
        // ticks included, and rebuilding a playlist's worth of rows several
        // times a second is the whole cost of leaving this page open.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let id = player.read(cx).current_song().map(|s| s.id.clone());
            if id != this.playing_id {
                this.playing_id = id;
                cx.notify();
            }
        })
        .detach();

        let seen_revision = playlists.read(cx).revision;
        let scroll = ScrollHandle::new();
        let mut this = Self {
            session,
            player,
            playlists,
            playlist_id,
            playlist: None,
            generation: 0,
            seen_revision,
            editing: false,
            name_input,
            desc_input,
            filter_input,
            sort: TrackSort::Custom,
            descending: false,
            visible: Vec::new(),
            delete_armed: false,
            error: None,
            cover: None,
            cover_key: None,
            thumbs: HashMap::new(),
            thumbs_pending: HashSet::new(),
            playing_id: None,
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
        };
        this.want_cover(cx);
        this.load(cx);
        this
    }

    fn client(&self, cx: &App) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// The playlist's own record: the loaded one, else the sidebar listing's
    /// copy, so the header paints before `getPlaylist` answers.
    fn meta(&self, cx: &App) -> Option<Playlist> {
        self.playlist
            .as_ref()
            .map(|p| p.playlist.clone())
            .or_else(|| self.playlists.read(cx).get(&self.playlist_id).cloned())
    }

    fn songs(&self) -> &[Song] {
        self.playlist.as_ref().map_or(&[], |p| p.songs.as_slice())
    }

    fn editable(&self, cx: &App) -> bool {
        self.meta(cx)
            .is_some_and(|m| self.playlists.read(cx).editable(&m, cx))
    }

    /// Drag and the move items only make sense on the playlist's own order,
    /// unfiltered — a dropped row has to land between its real neighbours.
    fn can_reorder(&self, cx: &App) -> bool {
        self.sort == TrackSort::Custom && self.filter_text(cx).is_empty() && self.editable(cx)
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter_input.read(cx).value().trim().to_string()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let id = self.playlist_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_playlist(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                match result {
                    Ok(pl) => {
                        view.error = None;
                        view.playlist = Some(pl);
                        view.refresh_visible(cx);
                        view.want_cover(cx);
                    }
                    Err(e) => view.error = Some(crate::errors::ErrorNote::new(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_visible(&mut self, cx: &App) {
        let filter = self.filter_text(cx);
        self.visible = visible_order(self.songs(), &filter, self.sort, self.descending);
        if let Some(c) = self.vi_cursor {
            self.vi_cursor = (!self.visible.is_empty()).then(|| c.min(self.visible.len() - 1));
        }
    }

    /// Fetch the header cover. Navidrome draws a playlist's cover from its
    /// songs, so the key carries `changed`: an edit gives a new picture.
    fn want_cover(&mut self, cx: &mut Context<Self>) {
        let Some(meta) = self.meta(cx) else { return };
        let Some(cover_id) = meta.cover_art.clone() else {
            return;
        };
        let key = crate::ui::playlists::cover_key(&meta);
        if self.cover_key.as_ref() == Some(&key) {
            return;
        }
        self.cover_key = Some(key.clone());
        if let Some(path) = artwork::cached(&key, COVER_ART) {
            self.cover = Some(path);
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let fetched = artwork::fetch_as(client, cover_id, key.clone(), COVER_ART).await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(path) = fetched
                    && view.cover_key.as_ref() == Some(&key)
                {
                    view.cover = Some(path);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Thumbnails for the rows on screen and `ART_LOOKAHEAD` past them —
    /// a long playlist asks for its art as it is scrolled to, not all at once.
    fn ensure_thumbs(&mut self, window: &Window, cx: &mut Context<Self>) {
        let scrolled = -f32::from(self.scroll.offset().y);
        let height = f32::from(window.viewport_size().height);
        let upto = ((scrolled + height) / ROW_H).ceil() as usize + ART_LOOKAHEAD;
        let mut wanted: Vec<(String, String)> = Vec::new();
        for &ix in self.visible.iter().take(upto) {
            let Some(song) = self.songs().get(ix) else {
                continue;
            };
            let Some((cover_id, key)) = artwork::song_cover(song) else {
                continue;
            };
            if self.thumbs.contains_key(&key) || self.thumbs_pending.contains(&key) {
                continue;
            }
            // Any rung will do at 40px: the grid's or player bar's copy of
            // the same album looks identical here.
            if let Some(path) = artwork::cached_best(&key, THUMB_ART) {
                self.thumbs.insert(key, path);
                continue;
            }
            self.thumbs_pending.insert(key.clone());
            wanted.push((cover_id, key));
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        for (cover_id, key) in wanted {
            let client = client.clone();
            cx.spawn(async move |this, cx| {
                let fetched = artwork::fetch_as(client, cover_id, key.clone(), THUMB_ART).await;
                let _ = this.update(cx, |view, cx| {
                    view.thumbs_pending.remove(&key);
                    if let Ok(path) = fetched {
                        view.thumbs.insert(key, path);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.meta(cx) else { return };
        self.editing = true;
        self.name_input.update(cx, |input, cx| {
            input.set_value(meta.name.clone(), window, cx)
        });
        self.desc_input.update(cx, |input, cx| {
            input.set_value(meta.comment.clone().unwrap_or_default(), window, cx)
        });
        self.name_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn save_details(&mut self, cx: &mut Context<Self>) {
        if !self.editing {
            return;
        }
        let Some(meta) = self.meta(cx) else { return };
        let name = self.name_input.read(cx).value().trim().to_string();
        let comment = self.desc_input.read(cx).value().trim().to_string();
        self.editing = false;
        let id = self.playlist_id.clone();
        if !name.is_empty() && name != meta.name {
            if let Some(pl) = self.playlist.as_mut() {
                pl.playlist.name = name.clone();
            }
            self.playlists
                .update(cx, |state, cx| state.rename(id.clone(), name, cx));
        }
        if comment != meta.comment.unwrap_or_default().trim() {
            if let Some(pl) = self.playlist.as_mut() {
                pl.playlist.comment = (!comment.is_empty()).then(|| comment.clone());
            }
            self.playlists
                .update(cx, |state, cx| state.set_comment(id, comment, cx));
        }
        cx.notify();
    }

    fn set_sort(&mut self, sort: TrackSort, cx: &mut Context<Self>) {
        // A second press on the active column flips its direction.
        if self.sort == sort && sort != TrackSort::Custom {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = false;
        }
        self.refresh_visible(cx);
        cx.notify();
    }

    /// Store the sorted view as the playlist's own order.
    fn save_sorted_order(&mut self, cx: &mut Context<Self>) {
        let all = visible_order(self.songs(), "", self.sort, self.descending);
        self.apply_order(all, cx);
        self.sort = TrackSort::Custom;
        self.descending = false;
        self.refresh_visible(cx);
        cx.notify();
    }

    /// Show `order` (a permutation of song indices) at once, then save it.
    fn apply_order(&mut self, order: Vec<usize>, cx: &mut Context<Self>) {
        let Some(pl) = self.playlist.as_mut() else {
            return;
        };
        let songs: Vec<Song> = order.iter().map(|&i| pl.songs[i].clone()).collect();
        let ids = songs.iter().map(|s| s.id.clone()).collect();
        pl.songs = songs;
        // An answer already in flight carries the old order.
        self.generation += 1;
        self.playlists.update(cx, |state, cx| {
            state.reorder(self.playlist_id.clone(), ids, cx)
        });
    }

    fn move_song(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let len = self.songs().len();
        if from == to || from >= len || to >= len {
            return;
        }
        self.apply_order(moved(len, from, to), cx);
        self.refresh_visible(cx);
        if self.vi_cursor.is_some() {
            self.vi_cursor = Some(to);
        }
        cx.notify();
    }

    fn remove_song(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(pl) = self.playlist.as_mut() else {
            return;
        };
        if ix >= pl.songs.len() {
            return;
        }
        pl.songs.remove(ix);
        self.generation += 1;
        self.refresh_visible(cx);
        self.playlists.update(cx, |state, cx| {
            state.remove_songs(self.playlist_id.clone(), vec![ix as u32], cx)
        });
        cx.notify();
    }

    fn duplicate(&mut self, cx: &mut Context<Self>) {
        let Some(meta) = self.meta(cx) else { return };
        let name = {
            let state = self.playlists.read(cx);
            let taken: Vec<&str> = state.playlists.iter().map(|p| p.name.as_str()).collect();
            copy_name(&meta.name, &taken)
        };
        self.playlists.update(cx, |state, cx| {
            state.duplicate(self.playlist_id.clone(), name, cx)
        });
    }

    pub fn set_public(&mut self, public: bool, cx: &mut Context<Self>) {
        if let Some(pl) = self.playlist.as_mut() {
            pl.playlist.public = Some(public);
        }
        self.playlists.update(cx, |state, cx| {
            state.set_public(self.playlist_id.clone(), public, cx)
        });
        cx.notify();
    }

    /// First press arms, second deletes; the arm lapses after `DELETE_ARM`.
    fn press_delete(&mut self, cx: &mut Context<Self>) {
        if self.delete_armed {
            self.delete_armed = false;
            self.playlists.update(cx, |state, cx| {
                state.delete(self.playlist_id.clone(), cx);
            });
            cx.emit(PlaylistDetailEvent::Deleted);
            return;
        }
        self.delete_armed = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DELETE_ARM).await;
            let _ = this.update(cx, |view, cx| {
                view.delete_armed = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Play the shown list (filtered and sorted as on screen) from `pos`.
    fn play_from(&mut self, pos: usize, cx: &mut Context<Self>) {
        let songs: Vec<Song> = self
            .visible
            .iter()
            .filter_map(|&i| self.songs().get(i).cloned())
            .collect();
        if pos >= songs.len() {
            return;
        }
        self.player.update(cx, |p, cx| p.play_queue(songs, pos, cx));
    }

    fn play_shuffled(&mut self, cx: &mut Context<Self>) {
        let songs: Vec<Song> = self
            .visible
            .iter()
            .filter_map(|&i| self.songs().get(i).cloned())
            .collect();
        if songs.is_empty() {
            return;
        }
        self.player
            .update(cx, |p, cx| p.play_queue_shuffled(songs, cx));
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let meta = self.meta(cx);
        let editable = self.editable(cx);
        let name = meta
            .as_ref()
            .map_or_else(|| "…".to_string(), |m| m.name.clone());
        let comment = meta
            .as_ref()
            .and_then(|m| m.comment.clone())
            .filter(|c| !c.trim().is_empty());
        let public = meta.as_ref().and_then(|m| m.public).unwrap_or(false);
        let detailed = self.session.read(cx).settings.detailed_album_dates;
        let summary = header_summary(
            meta.as_ref(),
            self.playlist.as_ref().map(|p| p.songs.as_slice()),
            !editable,
            detailed,
        );
        let count = self.songs().len();

        let cover = div()
            .size(px(COVER))
            .flex_none()
            .rounded_lg()
            .overflow_hidden()
            .bg(cx.theme().muted)
            .shadow_md()
            .map(|this| match self.cover.clone() {
                Some(path) => this.child(img(path).size(px(COVER)).rounded_lg()),
                None => this.child(
                    h_flex().size_full().items_center().justify_center().child(
                        app_icon(icons::LIST_MUSIC)
                            .size(px(56.))
                            .text_color(cx.theme().muted_foreground),
                    ),
                ),
            });

        let kind_line = h_flex()
            .gap_1p5()
            .items_center()
            .text_xs()
            .font_semibold()
            .text_color(cx.theme().muted_foreground)
            .child(if public {
                "PUBLIC PLAYLIST"
            } else {
                "PLAYLIST"
            })
            .when(!editable, |this| {
                this.child(Icon::new(IconName::User).xsmall())
            });

        let title_block = if self.editing {
            v_flex()
                .gap_2()
                .w(px(420.))
                .max_w_full()
                .child(Input::new(&self.name_input))
                .child(Input::new(&self.desc_input))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("pl-save")
                                .primary()
                                .small()
                                .label("Save")
                                .on_click(cx.listener(|this, _, _, cx| this.save_details(cx))),
                        )
                        .child(
                            Button::new("pl-cancel")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.editing = false;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element()
        } else {
            v_flex()
                .gap_1()
                .min_w_0()
                .child(div().text_3xl().font_bold().line_clamp(2).child(name))
                .when_some(comment, |this, comment| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .line_clamp(3)
                            .child(comment),
                    )
                })
                .into_any_element()
        };

        let actions = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                Button::new("pl-play")
                    .primary()
                    .icon(app_icon(icons::PLAY))
                    .label("Play")
                    .disabled(count == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.play_from(0, cx))),
            )
            .child(
                Button::new("pl-shuffle")
                    .outline()
                    .icon(app_icon(icons::SHUFFLE))
                    .label("Shuffle")
                    .disabled(count == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.play_shuffled(cx))),
            )
            .when(editable && !self.editing, |this| {
                this.child(
                    Button::new("pl-edit")
                        .ghost()
                        .icon(app_icon(icons::PENCIL))
                        .label("Edit details")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_editing(window, cx)),
                        ),
                )
            })
            .child(
                Button::new("pl-duplicate")
                    .ghost()
                    .icon(Icon::new(IconName::Copy))
                    .label("Duplicate")
                    .disabled(self.playlist.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.duplicate(cx))),
            )
            .when(editable, |this| {
                this.child(
                    Switch::new("pl-public")
                        .checked(public)
                        .label("Public")
                        .small()
                        .tooltip("Visible to every user on the server")
                        .on_click(
                            cx.listener(|this, &checked, _, cx| this.set_public(checked, cx)),
                        ),
                )
                .child(
                    Button::new("pl-delete")
                        .danger()
                        .when(!self.delete_armed, |b| b.ghost())
                        .icon(Icon::new(IconName::Delete))
                        .label(if self.delete_armed {
                            "Click again to delete"
                        } else {
                            "Delete"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.press_delete(cx))),
                )
            });

        h_flex().gap_6().items_end().child(cover).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_3()
                .child(kind_line)
                .child(title_block)
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(summary),
                )
                .child(actions),
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editable = self.editable(cx);
        let view = cx.entity();
        let sorts = h_flex()
            .gap_0p5()
            .p_0p5()
            .rounded_lg()
            .bg(cx.theme().muted)
            .children(TrackSort::ALL.map(|sort| {
                let view = view.clone();
                let active = sort == self.sort;
                let label = if active && sort != TrackSort::Custom {
                    format!(
                        "{} {}",
                        sort.label(),
                        if self.descending { "↓" } else { "↑" }
                    )
                } else {
                    sort.label().to_string()
                };
                let button = Button::new(SharedString::from(format!("pl-sort-{}", sort.label())))
                    .label(label)
                    .small()
                    .h(px(28.))
                    .px_3()
                    .text_size(px(13.))
                    .on_click(move |_, _, cx: &mut App| {
                        view.update(cx, |this, cx| this.set_sort(sort, cx));
                    });
                if active {
                    button.primary()
                } else {
                    button.ghost()
                }
            }));
        let hint: Option<&str> = if !editable || self.sort != TrackSort::Custom {
            None
        } else if !self.filter_text(cx).is_empty() {
            Some("Clear the filter to reorder")
        } else if self.songs().len() > 1 {
            Some("Drag tracks to reorder")
        } else {
            None
        };
        h_flex()
            .gap_3()
            .items_center()
            .flex_wrap()
            .child(
                div().w(px(260.)).child(
                    Input::new(&self.filter_input)
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        ),
                ),
            )
            .child(sorts)
            .when(editable && self.sort != TrackSort::Custom, |this| {
                this.child(
                    Button::new("pl-save-order")
                        .outline()
                        .small()
                        .label("Save as playlist order")
                        .on_click(cx.listener(|this, _, _, cx| this.save_sorted_order(cx))),
                )
            })
            .when_some(hint, |this, hint| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                )
            })
    }

    fn render_row(&self, pos: usize, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let song = &self.songs()[ix];
        let is_playing = self.playing_id.as_deref() == Some(song.id.as_str());
        let focused = self.vi_cursor == Some(pos);
        let reorder = self.can_reorder(cx);
        let editable = self.editable(cx);
        let settings = &self.session.read(cx).settings;
        let glow = settings.selection_glow_vi;
        let hover_glow = settings.selection_glow_hover;
        let len = self.songs().len();

        let thumb = artwork::song_cover(song).and_then(|(_, key)| self.thumbs.get(&key).cloned());
        let credits = song.credits();
        let artist = song.artist.clone().unwrap_or_default();
        let guests =
            subsonic::credits::guest_summary(&credits, &format!("{} {artist}", song.title));
        let dur = song
            .duration
            .map(|s| format_duration(Duration::from_secs(s as u64)))
            .unwrap_or_default();
        let title: SharedString = song.title.clone().into();
        let group: SharedString = format!("pl-row-{pos}").into();

        let album_link = song.album_id.clone().map(|id| {
            cx.listener(move |_, _: &gpui::ClickEvent, _, cx| {
                cx.emit(PlaylistDetailEvent::OpenAlbum(id.clone()));
                cx.stop_propagation();
            })
        });
        let artist_link = song.artist_id.clone().map(|id| {
            cx.listener(move |_, _: &gpui::ClickEvent, _, cx| {
                cx.emit(PlaylistDetailEvent::OpenArtist(id.clone()));
                cx.stop_propagation();
            })
        });

        let row = h_flex()
            .id(("pl-track", pos))
            .group(group.clone())
            .w_full()
            .h(px(ROW_H))
            .px_2()
            .gap_3()
            .items_center()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| {
                let s = s.bg(cx.theme().muted);
                if hover_glow {
                    crate::ui::hover_glow_style(s, None, cx)
                } else {
                    s
                }
            })
            .when(is_playing, |s| {
                s.bg(cx.theme().muted).text_color(cx.theme().primary)
            })
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
            .on_click(cx.listener(move |view, _, _, cx| view.play_from(pos, cx)))
            .when(reorder, |row| {
                row.on_drag(
                    DraggedTrack {
                        from: ix,
                        title: title.clone(),
                    },
                    |dragged, _, _, cx| {
                        cx.new(|_| DragGhost {
                            title: dragged.title.clone(),
                        })
                    },
                )
                .drag_over::<DraggedTrack>(move |style, dragged, _, cx| {
                    // The line shows where the row lands: above this one when
                    // it comes from below, under it when it comes from above.
                    let style = style.bg(cx.theme().muted);
                    if dragged.from > ix {
                        style.border_t_2().border_color(cx.theme().primary)
                    } else if dragged.from < ix {
                        style.border_b_2().border_color(cx.theme().primary)
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(
                    move |view, dragged: &DraggedTrack, _, cx| {
                        view.move_song(dragged.from, ix, cx);
                    },
                ))
            })
            // Number, or the play glyph for the playing row.
            .child(
                h_flex()
                    .w(px(28.))
                    .flex_none()
                    .justify_end()
                    .text_sm()
                    .text_color(if is_playing {
                        cx.theme().primary
                    } else {
                        cx.theme().muted_foreground
                    })
                    .map(|this| {
                        if is_playing {
                            this.child(app_icon(icons::AUDIO_LINES).small())
                        } else if reorder {
                            this.child(
                                div()
                                    .group_hover(group.clone(), |s| s.invisible())
                                    .child(format!("{}", pos + 1)),
                            )
                        } else {
                            this.child(format!("{}", pos + 1))
                        }
                    }),
            )
            .child(
                div()
                    .size(px(THUMB))
                    .flex_none()
                    .rounded_md()
                    .overflow_hidden()
                    .bg(cx.theme().muted)
                    .when_some(thumb, |this, path| {
                        this.child(img(path).size(px(THUMB)).rounded_md())
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(div().min_w_0().truncate().child(title.clone()))
                            .when_some(guests, |this, guests| {
                                this.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(guests),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(("pl-artist", pos))
                            .text_xs()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .when_some(artist_link, |this, link| {
                                this.hover(|s| s.underline()).on_click(link)
                            })
                            .child(artist),
                    ),
            )
            .child(
                div()
                    .id(("pl-album", pos))
                    .w(px(ALBUM_W))
                    .min_w_0()
                    .text_sm()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .when_some(album_link, |this, link| {
                        this.hover(|s| s.underline()).on_click(link)
                    })
                    .child(song.album.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .w(px(DURATION_W))
                    .flex_none()
                    .text_right()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(dur),
            )
            .child(div().w(px(28.)).flex_none().when(editable, |this| {
                this.child(
                    div()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .child(
                            Button::new(("pl-rm", pos))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Close))
                                .tooltip("Remove from playlist")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.remove_song(ix, cx);
                                })),
                        ),
                )
            }));

        let view = cx.entity();
        let menu_song = song.clone();
        let menu_artists = crate::ui::credit_links(&credits);
        let menu_album = song.album_id.clone();
        let row =
            row.context_menu(move |menu, window, cx| {
                let play = view.clone();
                let next = (view.clone(), menu_song.clone());
                let enqueue = (view.clone(), menu_song.clone());
                let details = menu_song.clone();
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("Play").on_click(move |_, _, cx: &mut App| {
                            play.update(cx, |v, cx| v.play_from(pos, cx));
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Play next").on_click(move |_, _, cx: &mut App| {
                            let (view, song) = next.clone();
                            view.update(cx, |v, cx| {
                                v.player.update(cx, |p, cx| p.play_next(vec![song], cx))
                            });
                        }),
                    )
                    .item(
                        PopupMenuItem::new("Add to queue").on_click(move |_, _, cx: &mut App| {
                            let (view, song) = enqueue.clone();
                            view.update(cx, |v, cx| {
                                v.player.update(cx, |p, cx| p.enqueue(vec![song], cx))
                            });
                        }),
                    )
                    .item(PopupMenuItem::new("Song details").on_click(
                        move |_, _, cx: &mut App| crate::ui::song_info::show(details.clone(), cx),
                    ));
                if let Some(album) = menu_album.clone() {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::separator()).item(
                        PopupMenuItem::new("Go to album").on_click(move |_, _, cx: &mut App| {
                            view.update(cx, |_, cx| {
                                cx.emit(PlaylistDetailEvent::OpenAlbum(album.clone()))
                            });
                        }),
                    );
                }
                match menu_artists.as_slice() {
                    [] => {}
                    [(_, id)] => {
                        let view = view.clone();
                        let id = id.clone();
                        menu = menu.item(PopupMenuItem::new("Go to artist").on_click(
                            move |_, _, cx: &mut App| {
                                view.update(cx, |_, cx| {
                                    cx.emit(PlaylistDetailEvent::OpenArtist(id.clone()))
                                });
                            },
                        ));
                    }
                    artists => {
                        let artists = artists.to_vec();
                        let view = view.clone();
                        menu = menu.submenu("Go to artist", window, cx, move |sub, _, _| {
                            let mut sub = sub;
                            for (name, id) in &artists {
                                let view = view.clone();
                                let id = id.clone();
                                sub = sub.item(PopupMenuItem::new(name.clone()).on_click(
                                    move |_, _, cx: &mut App| {
                                        view.update(cx, |_, cx| {
                                            cx.emit(PlaylistDetailEvent::OpenArtist(id.clone()))
                                        });
                                    },
                                ));
                            }
                            sub
                        });
                    }
                }
                if reorder {
                    let up = view.clone();
                    let down = view.clone();
                    let top = view.clone();
                    let bottom = view.clone();
                    menu = menu
                        .item(PopupMenuItem::separator())
                        .item(PopupMenuItem::new("Move up").disabled(ix == 0).on_click(
                            move |_, _, cx: &mut App| {
                                up.update(cx, |v, cx| v.move_song(ix, ix.saturating_sub(1), cx));
                            },
                        ))
                        .item(
                            PopupMenuItem::new("Move down")
                                .disabled(ix + 1 >= len)
                                .on_click(move |_, _, cx: &mut App| {
                                    down.update(cx, |v, cx| v.move_song(ix, ix + 1, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new("Move to top")
                                .disabled(ix == 0)
                                .on_click(move |_, _, cx: &mut App| {
                                    top.update(cx, |v, cx| v.move_song(ix, 0, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new("Move to bottom")
                                .disabled(ix + 1 >= len)
                                .on_click(move |_, _, cx: &mut App| {
                                    bottom.update(cx, |v, cx| v.move_song(ix, len - 1, cx));
                                }),
                        );
                }
                if editable {
                    let remove = view.clone();
                    menu = menu.item(PopupMenuItem::separator()).item(
                        PopupMenuItem::new("Remove from playlist").on_click(
                            move |_, _, cx: &mut App| {
                                remove.update(cx, |v, cx| v.remove_song(ix, cx));
                            },
                        ),
                    );
                }
                menu
            });
        with_focus_cursor(format!("vi-focus-{pos}"), row, focused, glow, None, cx)
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, body) = if self.playlist.is_none() {
            ("Loading…", "")
        } else if self.songs().is_empty() {
            (
                "This playlist is empty",
                "Add songs from an album's track menu or an album card's menu.",
            )
        } else {
            ("No tracks match", "Try another filter.")
        };
        v_flex()
            .py_12()
            .gap_1()
            .items_center()
            .child(
                app_icon(icons::LIST_MUSIC)
                    .size(px(36.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().font_semibold().child(title))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(body),
            )
    }
}

impl Render for PlaylistDetailView {
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
        self.ensure_thumbs(window, cx);

        let rows: Vec<_> = self
            .visible
            .clone()
            .into_iter()
            .enumerate()
            .map(|(pos, ix)| self.render_row(pos, ix, cx))
            .collect();
        let error = self.error.clone().or_else(|| {
            self.playlists
                .read(cx)
                .error
                .clone()
                .map(|text| crate::errors::ErrorNote {
                    text,
                    retryable: false,
                })
        });

        v_flex()
            .id("playlist-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_6()
            .gap_5()
            .child(self.render_header(cx))
            .when_some(error, |this, note| {
                this.child(crate::ui::error_banner(
                    &note,
                    cx.listener(|view, _, _, cx| {
                        view.error = None;
                        view.playlists.update(cx, |state, cx| {
                            state.error = None;
                            state.reload(cx);
                        });
                        view.load(cx);
                    }),
                    cx,
                ))
            })
            .child(self.render_toolbar(cx))
            .map(|this| {
                if rows.is_empty() {
                    this.child(self.render_empty(cx))
                } else {
                    this.child(v_flex().gap_0p5().children(rows))
                }
            })
    }
}

impl PlaylistDetailView {
    /// Move the vi-mode cursor by `delta` rows, clamping to the shown list.
    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = self.visible.len();
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

    /// True while one of the page's fields has the keyboard — playlist names
    /// have spaces in them more often than not.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        [&self.name_input, &self.desc_input, &self.filter_input]
            .into_iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// `i`: into the filter field.
    pub fn vi_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Play the track under the vi-mode cursor.
    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        if let Some(pos) = self.vi_cursor {
            self.play_from(pos, cx);
        }
    }

    pub fn vi_play(&mut self, cx: &mut Context<Self>) {
        self.play_from(0, cx);
    }

    pub fn vi_shuffle(&mut self, cx: &mut Context<Self>) {
        self.play_shuffled(cx);
    }

    /// `:pl rename <name>` on this page.
    pub fn rename(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(pl) = self.playlist.as_mut() {
            pl.playlist.name = name.clone();
        }
        self.playlists.update(cx, |state, cx| {
            state.rename(self.playlist_id.clone(), name, cx)
        });
        cx.notify();
    }

    /// `:pl dup` on this page.
    pub fn vi_duplicate(&mut self, cx: &mut Context<Self>) {
        self.duplicate(cx);
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }
}

/// The header's one-line summary: owner (for someone else's playlist),
/// track count, total time, last change.
fn header_summary(
    meta: Option<&Playlist>,
    songs: Option<&[Song]>,
    show_owner: bool,
    detailed: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if show_owner && let Some(owner) = meta.and_then(|m| m.owner.as_deref()) {
        parts.push(format!("by {owner}"));
    }
    // The loaded songs are the truth once they land (an edit just made
    // shows at once); before that, the listing's counts.
    let (count, secs) = match songs {
        Some(songs) => (songs.len() as u64, total_secs(songs)),
        None => match meta {
            Some(m) => (
                u64::from(m.song_count.unwrap_or(0)),
                u64::from(m.duration.unwrap_or(0)),
            ),
            None => return String::new(),
        },
    };
    parts.push(match count {
        1 => "1 track".to_string(),
        n => format!("{n} tracks"),
    });
    if secs > 0 {
        parts.push(format_playtime(secs as f64));
    }
    if let Some(changed) = meta.and_then(|m| m.changed.as_deref()) {
        parts.push(format!(
            "updated {}",
            crate::ui::format_added_date(changed, detailed)
        ));
    }
    parts.join(" · ")
}

fn total_secs(songs: &[Song]) -> u64 {
    songs
        .iter()
        .map(|s| u64::from(s.duration.unwrap_or(0)))
        .sum()
}

/// Song indices to show, in display order: those matching every word of
/// `filter` (title, artist or album, case-folded), ordered by `sort`. Ties —
/// and `Custom` — keep the playlist's own order.
pub fn visible_order(
    songs: &[Song],
    filter: &str,
    sort: TrackSort,
    descending: bool,
) -> Vec<usize> {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    let mut order: Vec<usize> = songs
        .iter()
        .enumerate()
        .filter(|(_, song)| {
            if words.is_empty() {
                return true;
            }
            let hay = format!(
                "{} {} {}",
                song.title,
                song.artist.as_deref().unwrap_or_default(),
                song.album.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .map(|(i, _)| i)
        .collect();
    let text = |s: Option<&str>| s.unwrap_or_default().to_lowercase();
    let key_cmp = |a: &Song, b: &Song| match sort {
        TrackSort::Custom => std::cmp::Ordering::Equal,
        TrackSort::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        TrackSort::Artist => text(a.artist.as_deref())
            .cmp(&text(b.artist.as_deref()))
            .then_with(|| text(a.album.as_deref()).cmp(&text(b.album.as_deref())))
            .then_with(|| a.disc_number.cmp(&b.disc_number))
            .then_with(|| a.track.cmp(&b.track)),
        TrackSort::Album => text(a.album.as_deref())
            .cmp(&text(b.album.as_deref()))
            .then_with(|| a.disc_number.cmp(&b.disc_number))
            .then_with(|| a.track.cmp(&b.track)),
        TrackSort::Duration => a.duration.cmp(&b.duration),
    };
    order.sort_by(|&a, &b| {
        let ord = key_cmp(&songs[a], &songs[b]);
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(a.cmp(&b))
    });
    order
}

/// The playlist's order after the song at `from` is moved to `to`: indices
/// of the current list, the moved one now at position `to`.
pub fn moved(len: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    if from < len && to < len {
        let item = order.remove(from);
        order.insert(to, item);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str, artist: &str, album: &str, duration: u32) -> Song {
        let json = serde_json::json!({
            "id": title, "title": title, "artist": artist, "album": album,
            "duration": duration,
        });
        serde_json::from_value(json).unwrap()
    }

    fn songs() -> Vec<Song> {
        vec![
            song("Breathe", "Pink Floyd", "The Dark Side of the Moon", 163),
            song("Airbag", "Radiohead", "OK Computer", 284),
            song("Time", "Pink Floyd", "The Dark Side of the Moon", 413),
            song("creep", "Radiohead", "Pablo Honey", 238),
        ]
    }

    #[test]
    fn custom_order_is_the_playlists_own() {
        assert_eq!(
            visible_order(&songs(), "", TrackSort::Custom, false),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn filter_needs_every_word_somewhere_in_the_row() {
        assert_eq!(
            visible_order(&songs(), "floyd moon", TrackSort::Custom, false),
            vec![0, 2]
        );
        assert_eq!(
            visible_order(&songs(), "RADIOHEAD pablo", TrackSort::Custom, false),
            vec![3]
        );
        assert!(visible_order(&songs(), "beatles", TrackSort::Custom, false).is_empty());
    }

    #[test]
    fn title_sort_ignores_case() {
        assert_eq!(
            visible_order(&songs(), "", TrackSort::Title, false),
            vec![1, 0, 3, 2]
        );
    }

    #[test]
    fn descending_flips_the_key_but_keeps_ties_in_playlist_order() {
        // Artist ties (Pink Floyd ×2, Radiohead ×2) break on album, then
        // playlist order.
        assert_eq!(
            visible_order(&songs(), "", TrackSort::Artist, false),
            vec![0, 2, 1, 3]
        );
        assert_eq!(
            visible_order(&songs(), "", TrackSort::Artist, true),
            vec![3, 1, 0, 2]
        );
    }

    #[test]
    fn length_sort_orders_by_duration() {
        assert_eq!(
            visible_order(&songs(), "", TrackSort::Duration, false),
            vec![0, 3, 1, 2]
        );
    }

    #[test]
    fn moving_down_lands_the_row_at_the_target() {
        assert_eq!(moved(4, 0, 2), vec![1, 2, 0, 3]);
    }

    #[test]
    fn moving_up_lands_the_row_at_the_target() {
        assert_eq!(moved(4, 3, 1), vec![0, 3, 1, 2]);
    }

    #[test]
    fn moving_out_of_range_changes_nothing() {
        assert_eq!(moved(3, 0, 5), vec![0, 1, 2]);
    }

    #[test]
    fn summary_counts_the_loaded_songs() {
        let list = songs();
        let s = header_summary(None, Some(&list), false, false);
        assert!(s.starts_with("4 tracks · "), "{s}");
    }

    #[test]
    fn summary_names_the_owner_of_someone_elses_playlist() {
        let meta: Playlist = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "Mix", "owner": "ann", "songCount": 1, "duration": 0
        }))
        .unwrap();
        assert_eq!(
            header_summary(Some(&meta), None, true, false),
            "by ann · 1 track"
        );
    }
}
