//! Playlists page: every playlist as a card with its cover, opened from the
//! sidebar's "Playlists" heading. A "New playlist" card leads the grid.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, Render, ScrollAnchor, ScrollHandle,
    SharedString, Window, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use subsonic::Playlist;

use crate::assets::{app_icon, icons};
use crate::services::artwork;
use crate::state::playlists::{PlaylistsState, copy_name};
use crate::state::session::Session;
use crate::ui::{
    card_inset, card_padding, format_playtime, grid_gap, grid_padding_x, sync_focus_scroll,
    with_focus_cursor,
};

/// Columns guessed for the first frame, before the grid has been measured.
const FALLBACK_COLS: usize = 4;

pub enum PlaylistsViewEvent {
    Open(String),
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistOrder {
    /// Most recently changed first.
    Recent,
    Name,
}

pub struct PlaylistsView {
    session: Entity<Session>,
    playlists: Entity<PlaylistsState>,
    order: PlaylistOrder,
    /// Cover cache key → path; a key carries the playlist's `changed` stamp.
    covers: HashMap<String, PathBuf>,
    covers_pending: HashSet<String>,
    live_width: crate::ui::LiveWidth,
    scroll: ScrollHandle,
    focus_anchor: ScrollAnchor,
    /// Card under the vi cursor; 0 is the "New playlist" card.
    vi_cursor: Option<usize>,
    vi_scroll_synced: Option<usize>,
}

impl EventEmitter<PlaylistsViewEvent> for PlaylistsView {}

impl PlaylistsView {
    pub fn new(
        session: Entity<Session>,
        playlists: Entity<PlaylistsState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&playlists, |_, _, cx| cx.notify()).detach();
        let scroll = ScrollHandle::new();
        Self {
            session,
            playlists,
            order: PlaylistOrder::Recent,
            covers: HashMap::new(),
            covers_pending: HashSet::new(),
            live_width: crate::ui::LiveWidth::default(),
            scroll: scroll.clone(),
            focus_anchor: ScrollAnchor::for_handle(scroll),
            vi_cursor: None,
            vi_scroll_synced: None,
        }
    }

    fn sorted(&self, cx: &App) -> Vec<Playlist> {
        let mut list = self.playlists.read(cx).playlists.clone();
        sort_playlists(&mut list, self.order);
        list
    }

    fn ensure_covers(&mut self, list: &[Playlist], cx: &mut Context<Self>) {
        let client = self.session.read(cx).client.clone();
        for pl in list {
            let Some(cover_id) = pl.cover_art.clone() else {
                continue;
            };
            let key = cover_key(pl);
            if self.covers.contains_key(&key) || self.covers_pending.contains(&key) {
                continue;
            }
            if let Some(path) = artwork::cached(&key, 256) {
                self.covers.insert(key, path);
                continue;
            }
            let Some(client) = client.clone() else {
                continue;
            };
            self.covers_pending.insert(key.clone());
            cx.spawn(async move |this, cx| {
                let fetched = artwork::fetch_as(client, cover_id, key.clone(), 256).await;
                let _ = this.update(cx, |view, cx| {
                    view.covers_pending.remove(&key);
                    if let Ok(path) = fetched {
                        view.covers.insert(key, path);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn grid(&mut self, window: &Window, cx: &App) -> (usize, f32) {
        let measured = f32::from(self.scroll.bounds().size.width);
        let (min_tile, max_tile) = self.session.read(cx).settings.cover_size.range();
        self.live_width
            .grid(measured, min_tile, max_tile, window, FALLBACK_COLS, false)
    }

    fn card_shell(
        &self,
        id: SharedString,
        tile: f32,
        focused: bool,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        v_flex()
            .id(id)
            .w(px(tile + card_padding()))
            .flex_none()
            .p(px(card_inset()))
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .when(focused, |s| {
                s.anchor_scroll(Some(self.focus_anchor.clone()))
            })
    }

    fn render_new_card(
        &self,
        tile: f32,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let card = self
            .card_shell("pl-card-new".into(), tile, focused, cx)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PlaylistsViewEvent::New)))
            .child(
                h_flex()
                    .size(px(tile))
                    .rounded_md()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_dashed()
                    .border_color(cx.theme().border)
                    .child(
                        Icon::new(IconName::Plus)
                            .size(px(40.))
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                v_flex()
                    .child(div().text_sm().font_semibold().child("New playlist"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Name it, then add songs"),
                    ),
            );
        with_focus_cursor("vi-focus-pl-new", card, focused, glow, None, cx)
    }

    fn render_card(
        &self,
        pl: &Playlist,
        tile: f32,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let state = self.playlists.read(cx);
        let mine = state.editable(pl, cx);
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let art = self.covers.get(&cover_key(pl)).cloned();
        let id = pl.id.clone();
        let subtitle = card_subtitle(pl, !mine);
        let public = pl.public.unwrap_or(false);

        let cover = div()
            .size(px(tile))
            .rounded_md()
            .overflow_hidden()
            .bg(cx.theme().muted)
            .map(|this| match art {
                Some(path) => this.child(img(path).size(px(tile)).rounded_md()),
                None => this.child(
                    h_flex().size_full().items_center().justify_center().child(
                        app_icon(icons::LIST_MUSIC)
                            .size(px(tile * 0.3))
                            .text_color(cx.theme().muted_foreground),
                    ),
                ),
            });

        let open_id = id.clone();
        let card = self
            .card_shell(format!("pl-card-{id}").into(), tile, focused, cx)
            .on_click(
                cx.listener(move |_, _, _, cx| cx.emit(PlaylistsViewEvent::Open(open_id.clone()))),
            )
            .child(cover)
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_semibold()
                                    .child(pl.name.clone()),
                            )
                            .when(public, |this| {
                                this.child(
                                    Icon::new(IconName::Globe)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                )
                            })
                            .when(!mine, |this| {
                                this.child(
                                    Icon::new(IconName::User)
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(subtitle),
                    ),
            );

        let view = cx.entity();
        let playlists = self.playlists.clone();
        let menu_pl = pl.clone();
        let card = card.context_menu(move |menu, _, cx| {
            let open = (view.clone(), menu_pl.id.clone());
            let dup = (playlists.clone(), menu_pl.clone());
            let mut menu = menu
                .item(
                    PopupMenuItem::new("Open").on_click(move |_, _, cx: &mut App| {
                        let (view, id) = open.clone();
                        view.update(cx, |_, cx| cx.emit(PlaylistsViewEvent::Open(id)));
                    }),
                )
                .item(
                    PopupMenuItem::new("Duplicate").on_click(move |_, _, cx: &mut App| {
                        let (playlists, pl) = dup.clone();
                        playlists.update(cx, |state, cx| {
                            let taken: Vec<&str> =
                                state.playlists.iter().map(|p| p.name.as_str()).collect();
                            let name = copy_name(&pl.name, &taken);
                            state.duplicate(pl.id.clone(), name, cx);
                        });
                    }),
                );
            if playlists.read(cx).editable(&menu_pl, cx) {
                let toggle = (playlists.clone(), menu_pl.clone());
                let public = menu_pl.public.unwrap_or(false);
                menu = menu.item(
                    PopupMenuItem::new(if public {
                        "Make private"
                    } else {
                        "Make public"
                    })
                    .on_click(move |_, _, cx: &mut App| {
                        let (playlists, pl) = toggle.clone();
                        playlists
                            .update(cx, |state, cx| state.set_public(pl.id.clone(), !public, cx));
                    }),
                );
            }
            menu
        });
        with_focus_cursor(format!("vi-focus-pl-{id}"), card, focused, glow, None, cx)
    }

    fn order_button(
        &self,
        order: PlaylistOrder,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        let button = Button::new(SharedString::from(format!("pl-order-{label}")))
            .label(label)
            .small()
            .h(px(28.))
            .px_3()
            .text_size(px(13.))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.order != order {
                    this.order = order;
                    this.vi_cursor = None;
                }
                cx.notify();
            }));
        if self.order == order {
            button.primary()
        } else {
            button.ghost()
        }
    }
}

impl Render for PlaylistsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        sync_focus_scroll(
            &self.focus_anchor,
            self.vi_cursor,
            &mut self.vi_scroll_synced,
            window,
            cx,
        );
        let list = self.sorted(cx);
        self.ensure_covers(&list, cx);
        let (cols, tile) = self.grid(window, cx);
        let cols = cols.max(1);

        let total = list.len();
        let summary = match total {
            0 => "No playlists yet".to_string(),
            1 => "1 playlist".to_string(),
            n => format!("{n} playlists"),
        };

        let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(total + 1);
        cards.push(self.render_new_card(tile, self.vi_cursor == Some(0), cx));
        for (i, pl) in list.iter().enumerate() {
            cards.push(self.render_card(pl, tile, self.vi_cursor == Some(i + 1), cx));
        }
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut cards = cards.into_iter().peekable();
        while cards.peek().is_some() {
            rows.push(
                h_flex()
                    .gap(px(grid_gap()))
                    .items_start()
                    .children(cards.by_ref().take(cols))
                    .into_any_element(),
            );
        }

        let toolbar = h_flex()
            .px(px(grid_padding_x() / 2.))
            .pt_6()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .child(div().text_2xl().font_bold().child("Playlists"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .p_0p5()
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .child(self.order_button(PlaylistOrder::Recent, "Recent", cx))
                    .child(self.order_button(PlaylistOrder::Name, "A–Z", cx)),
            );

        v_flex()
            .id("playlists-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .gap_4()
            .child(toolbar)
            .child(
                v_flex()
                    .px(px(grid_padding_x() / 2.))
                    .pb_6()
                    .gap_3()
                    .children(rows),
            )
    }
}

impl PlaylistsView {
    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        let count = self.playlists.read(cx).playlists.len() + 1;
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

    pub fn vi_activate(&mut self, cx: &mut Context<Self>) {
        match self.vi_cursor {
            Some(0) => cx.emit(PlaylistsViewEvent::New),
            Some(i) => {
                if let Some(pl) = self.sorted(cx).get(i - 1) {
                    cx.emit(PlaylistsViewEvent::Open(pl.id.clone()));
                }
            }
            None => {}
        }
    }

    /// `[`/`]`: switch the order.
    pub fn vi_tab(&mut self, _delta: isize, cx: &mut Context<Self>) {
        self.order = match self.order {
            PlaylistOrder::Recent => PlaylistOrder::Name,
            PlaylistOrder::Name => PlaylistOrder::Recent,
        };
        // The same slot now holds another playlist.
        self.vi_cursor = None;
        cx.notify();
    }
}

/// Cache key for a playlist's cover. Navidrome draws it from the songs, so
/// it changes with them: the `changed` stamp makes an edit a new picture.
pub fn cover_key(pl: &Playlist) -> String {
    format!(
        "playlist-{}@{}",
        pl.id,
        pl.changed.as_deref().unwrap_or_default()
    )
}

fn card_subtitle(pl: &Playlist, show_owner: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if show_owner && let Some(owner) = pl.owner.as_deref() {
        parts.push(owner.to_string());
    }
    parts.push(match pl.song_count.unwrap_or(0) {
        1 => "1 track".to_string(),
        n => format!("{n} tracks"),
    });
    if let Some(secs) = pl.duration.filter(|&s| s > 0) {
        parts.push(format_playtime(f64::from(secs)));
    }
    parts.join(" · ")
}

/// `Recent`: newest `changed` first (ISO stamps sort as text), unknown last.
/// `Name`: case-folded. Ties fall back to the name, then the id.
pub fn sort_playlists(list: &mut [Playlist], order: PlaylistOrder) {
    let name = |p: &Playlist| p.name.to_lowercase();
    match order {
        PlaylistOrder::Recent => list.sort_by(|a, b| {
            b.changed
                .cmp(&a.changed)
                .then_with(|| name(a).cmp(&name(b)))
                .then_with(|| a.id.cmp(&b.id))
        }),
        PlaylistOrder::Name => {
            list.sort_by(|a, b| name(a).cmp(&name(b)).then_with(|| a.id.cmp(&b.id)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(id: &str, name: &str, changed: Option<&str>) -> Playlist {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "changed": changed,
        }))
        .unwrap()
    }

    #[test]
    fn recent_puts_the_newest_change_first_and_unknown_last() {
        let mut list = vec![
            pl("a", "Old", Some("2025-01-01T00:00:00Z")),
            pl("b", "Never", None),
            pl("c", "New", Some("2026-10-01T00:00:00Z")),
        ];
        sort_playlists(&mut list, PlaylistOrder::Recent);
        let ids: Vec<_> = list.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
    }

    #[test]
    fn name_order_ignores_case() {
        let mut list = vec![
            pl("a", "zebra", None),
            pl("b", "Apple", None),
            pl("c", "mango", None),
        ];
        sort_playlists(&mut list, PlaylistOrder::Name);
        let names: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "mango", "zebra"]);
    }

    #[test]
    fn cover_key_moves_with_the_playlist() {
        let a = pl("p", "Mix", Some("2026-01-01T00:00:00Z"));
        let b = pl("p", "Mix", Some("2026-02-01T00:00:00Z"));
        assert_ne!(cover_key(&a), cover_key(&b));
    }

    #[test]
    fn subtitle_names_another_users_playlist() {
        let p: Playlist = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "Mix", "owner": "ann", "songCount": 2, "duration": 0
        }))
        .unwrap();
        assert_eq!(card_subtitle(&p, true), "ann · 2 tracks");
        assert_eq!(card_subtitle(&p, false), "2 tracks");
    }
}
