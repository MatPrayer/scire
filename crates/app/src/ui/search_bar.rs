//! Global search bar (top right of every page): debounced search3 with a
//! dropdown of song / album / artist results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, KeyDownEvent, Render, ScrollHandle, Window,
    deferred, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use subsonic::{SearchResult3, Song};

use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::session::Session;

const DEBOUNCE: Duration = Duration::from_millis(300);
/// Thumbnail resolution for dropdown rows.
const ART_SIZE: u32 = 64;
/// Rows shown per section — the dropdown is a quick jump, not a browser.
const MAX_SONGS: usize = 8;
const MAX_ALBUMS: usize = 6;
const MAX_ARTISTS: usize = 5;

pub enum SearchBarEvent {
    OpenAlbum(String),
    OpenArtist(String),
}

/// A keyboard-selectable result, in the same order rows are rendered
/// (artists, then albums, then songs). Index into this list == `selected`.
enum PaletteItem {
    Artist(String),
    Album(String),
    Song(Box<Song>),
}

pub struct SearchBar {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    input: Entity<InputState>,
    results: Option<SearchResult3>,
    /// Dropdown visibility; results stay cached while hidden so reopening
    /// the same query is instant.
    open: bool,
    /// Centered command-palette mode (Ctrl/Cmd+K) vs. the inline top-right bar.
    palette: bool,
    /// Highlighted row for arrow-key navigation (palette mode).
    selected: usize,
    /// Scroll handle for the palette results, so arrow keys can scroll the
    /// highlighted row into view.
    results_scroll: ScrollHandle,
    searching: bool,
    error: Option<String>,
    /// Album-scoped art key (or plain cover id for albums/artists) → path.
    art_paths: HashMap<String, PathBuf>,
    generation: u64,
}

impl EventEmitter<SearchBarEvent> for SearchBar {}

impl SearchBar {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // No `clean_on_escape`: Escape must reach our own handlers to close the
        // palette / dropdown, not be swallowed to clear the field.
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search…"));

        cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_query_changed(cx);
            }
        })
        .detach();

        Self {
            session,
            player,
            input,
            results: None,
            open: false,
            palette: false,
            selected: 0,
            results_scroll: ScrollHandle::new(),
            searching: false,
            error: None,
            art_paths: HashMap::new(),
            generation: 0,
        }
    }

    /// Focus the input (wired to the `/` shortcut in the root view).
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn is_palette(&self) -> bool {
        self.palette
    }

    /// Open the centered command palette (Ctrl/Cmd+K) with a fresh query.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = true;
        self.selected = 0;
        self.results = None;
        self.open = false;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.focus(window, cx);
        cx.notify();
    }

    /// Close the dropdown/palette and clear the query (root's Escape handler).
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.palette = false;
        self.selected = 0;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.results = None;
        cx.notify();
    }

    /// Flat list of selectable rows, in render order. `selected` indexes this.
    fn items(&self) -> Vec<PaletteItem> {
        let mut v = Vec::new();
        if let Some(r) = &self.results {
            for a in r.artist.iter().take(MAX_ARTISTS) {
                v.push(PaletteItem::Artist(a.id.clone()));
            }
            for a in r.album.iter().take(MAX_ALBUMS) {
                v.push(PaletteItem::Album(a.id.clone()));
            }
            for s in r.song.iter().take(MAX_SONGS) {
                v.push(PaletteItem::Song(Box::new(s.clone())));
            }
        }
        v
    }

    /// Move the highlight by `delta`, wrapping at the ends.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.items().len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        if let Some(child) = self.selected_child_index() {
            self.results_scroll.scroll_to_item(child);
        }
        cx.notify();
    }

    /// Index of the selected row among the scroll container's children (which
    /// interleave section titles with rows), so we can scroll it into view.
    fn selected_child_index(&self) -> Option<usize> {
        let r = self.results.as_ref()?;
        let counts = [
            r.artist.len().min(MAX_ARTISTS),
            r.album.len().min(MAX_ALBUMS),
            r.song.len().min(MAX_SONGS),
        ];
        let mut child = 0;
        let mut item = 0;
        for n in counts {
            if n == 0 {
                continue;
            }
            child += 1; // section title
            for _ in 0..n {
                if item == self.selected {
                    return Some(child);
                }
                child += 1;
                item += 1;
            }
        }
        None
    }

    /// Activate the highlighted row (Enter in palette mode).
    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.items();
        let Some(item) = items.into_iter().nth(self.selected) else {
            return;
        };
        match item {
            PaletteItem::Artist(id) => cx.emit(SearchBarEvent::OpenArtist(id)),
            PaletteItem::Album(id) => cx.emit(SearchBarEvent::OpenAlbum(id)),
            PaletteItem::Song(song) => {
                self.player
                    .update(cx, |p, cx| p.play_queue(vec![*song], 0, cx));
            }
        }
        self.dismiss(window, cx);
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.selected = 0;
        let generation = self.generation;
        let empty = self.input.read(cx).value().trim().is_empty();
        if empty {
            self.open = false;
            self.results = None;
            self.searching = false;
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |bar, cx| {
                if bar.generation == generation {
                    bar.run_search(cx);
                }
            });
        })
        .detach();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let libraries = self.session.read(cx).library_query_ids();
        self.searching = true;
        self.open = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                // Search each selected library and merge, deduping items
                // that appear in more than one.
                let mut merged: Option<SearchResult3> = None;
                let mut seen = std::collections::HashSet::new();
                for lib in &libraries {
                    let mut r = client
                        .search3(&query, lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    r.artist.retain(|a| seen.insert(format!("ar:{}", a.id)));
                    r.album.retain(|a| seen.insert(format!("al:{}", a.id)));
                    r.song.retain(|s| seen.insert(format!("s:{}", s.id)));
                    match &mut merged {
                        Some(m) => {
                            m.artist.extend(r.artist);
                            m.album.extend(r.album);
                            m.song.extend(r.song);
                        }
                        None => merged = Some(r),
                    }
                }
                Ok::<_, anyhow::Error>(merged.unwrap_or_default())
            })
            .await;
            let _ = this.update(cx, |bar, cx| {
                bar.searching = false;
                match result {
                    Ok(r) => {
                        bar.fetch_result_art(&r, cx);
                        bar.results = Some(r);
                        bar.selected = 0;
                        bar.error = None;
                    }
                    Err(e) => bar.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Resolve thumbnails for the rows we will actually show.
    fn fetch_result_art(&mut self, results: &SearchResult3, cx: &mut Context<Self>) {
        // (cover id, cache key) — song covers are keyed per album so several
        // hits off one record share a single download.
        let cover_ids: Vec<(String, String)> = results
            .song
            .iter()
            .take(MAX_SONGS)
            .filter_map(artwork::song_cover)
            .chain(
                results
                    .album
                    .iter()
                    .take(MAX_ALBUMS)
                    .filter_map(|a| a.cover_art.clone().map(|id| (id.clone(), id))),
            )
            .chain(
                results
                    .artist
                    .iter()
                    .take(MAX_ARTISTS)
                    .filter_map(|a| a.cover_art.clone().map(|id| (id.clone(), id))),
            )
            .collect();
        for (cover_id, key) in cover_ids {
            if self.art_paths.contains_key(&key) {
                continue;
            }
            // Synchronous cache hit: no task, renders with the results.
            if let Some(path) = artwork::cached(&key, ART_SIZE) {
                self.art_paths.insert(key, path);
                continue;
            }
            let Some(client) = self.session.read(cx).client.clone() else {
                return;
            };
            cx.spawn(async move |this, cx| {
                if let Ok(path) =
                    runtime::spawn_io(artwork::fetch_as(client, cover_id, key.clone(), ART_SIZE))
                        .await
                {
                    let _ = this.update(cx, |bar, cx| {
                        bar.art_paths.insert(key, path);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    fn thumb(
        &self,
        art_key: Option<String>,
        fallback: IconName,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let path = art_key.and_then(|key| self.art_paths.get(&key).cloned());
        div()
            .size(px(32.))
            .flex_none()
            .rounded_sm()
            .bg(cx.theme().muted)
            .overflow_hidden()
            .map(|this| match path {
                Some(path) => this.child(img(path).size(px(32.)).rounded_sm()),
                None => this
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(fallback).small()),
            })
            .into_any_element()
    }

    fn section_title(label: &'static str, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .px_2()
            .pt_2()
            .pb_1()
            .text_xs()
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element()
    }

    /// Selected-row highlight (palette arrow-key navigation). Mirrors the
    /// album track-list convention: muted fill + a `primary` left border.
    fn row_selected(&self, idx: usize) -> bool {
        self.palette && idx == self.selected
    }

    /// Build the result rows shared by the inline dropdown and the palette.
    /// The flat selectable index is threaded so the highlighted row matches
    /// `selected`; section titles do not advance it.
    fn result_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let mut idx = 0usize;

        if let Some(results) = &self.results {
            if !results.artist.is_empty() {
                rows.push(Self::section_title("Artists", cx));
                for (i, artist) in results.artist.iter().take(MAX_ARTISTS).enumerate() {
                    let id = artist.id.clone();
                    let sel = self.row_selected(idx);
                    rows.push(
                        h_flex()
                            .id(("sb-artist", i))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .when(sel, |s| {
                                s.bg(cx.theme().muted)
                                    .border_l_2()
                                    .border_color(cx.theme().primary)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(SearchBarEvent::OpenArtist(id.clone()));
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(artist.cover_art.clone(), IconName::CircleUser, cx))
                            .child(
                                div()
                                    .text_sm()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(artist.name.clone()),
                            )
                            .into_any_element(),
                    );
                    idx += 1;
                }
            }

            if !results.album.is_empty() {
                rows.push(Self::section_title("Albums", cx));
                for (i, album) in results.album.iter().take(MAX_ALBUMS).enumerate() {
                    let id = album.id.clone();
                    let sel = self.row_selected(idx);
                    rows.push(
                        h_flex()
                            .id(("sb-album", i))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .when(sel, |s| {
                                s.bg(cx.theme().muted)
                                    .border_l_2()
                                    .border_color(cx.theme().primary)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(SearchBarEvent::OpenAlbum(id.clone()));
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(
                                album.cover_art.clone(),
                                IconName::LayoutDashboard,
                                cx,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().truncate().child(album.name.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(album.artist.clone().unwrap_or_default()),
                                    ),
                            )
                            .into_any_element(),
                    );
                    idx += 1;
                }
            }

            // Songs: click plays, `+` enqueues.
            if !results.song.is_empty() {
                rows.push(Self::section_title("Songs", cx));
                for (i, song) in results.song.iter().take(MAX_SONGS).enumerate() {
                    let play = song.clone();
                    let enqueue = song.clone();
                    let sel = self.row_selected(idx);
                    rows.push(
                        h_flex()
                            .id(("sb-song", i))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .when(sel, |s| {
                                s.bg(cx.theme().muted)
                                    .border_l_2()
                                    .border_color(cx.theme().primary)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.player.update(cx, |p, cx| {
                                    p.play_queue(vec![play.clone()], 0, cx);
                                });
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(
                                artwork::song_cover(song).map(|(_, key)| key),
                                IconName::Star,
                                cx,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().truncate().child(song.title.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(song.artist.clone().unwrap_or_default()),
                                    ),
                            )
                            .child(
                                Button::new(("sb-enq", i))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Plus))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| {
                                            p.enqueue(vec![enqueue.clone()], cx)
                                        });
                                        cx.stop_propagation();
                                    })),
                            )
                            .into_any_element(),
                    );
                    idx += 1;
                }
            }

            if results.song.is_empty() && results.album.is_empty() && results.artist.is_empty() {
                rows.push(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No results")
                        .into_any_element(),
                );
            }
        }

        if self.searching && rows.is_empty() {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Searching…")
                    .into_any_element(),
            );
        }
        if let Some(e) = &self.error {
            rows.push(
                div()
                    .p_3()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(e.clone())
                    .into_any_element(),
            );
        }

        rows
    }

    fn render_dropdown(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .id("search-dropdown")
            // Swallow mouse events so clicks land on rows, not the page beneath.
            .occlude()
            .absolute()
            .top(px(38.))
            .right_0()
            .w(px(360.))
            .max_h(px(440.))
            .overflow_y_scroll()
            .p_1()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .children(self.result_rows(cx))
            .into_any_element()
    }

    /// Centered command-palette box: large input on top, scrollable results
    /// below. Arrow/Enter/Escape are handled in the capture phase so they
    /// drive selection instead of reaching the input or the root shortcuts.
    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let has_query = !self.input.read(cx).value().trim().is_empty();
        let rows = self.result_rows(cx);
        v_flex()
            .id("search-palette")
            .occlude()
            .w(px(620.))
            .max_h(px(560.))
            .rounded_xl()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .capture_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "down" => {
                        this.move_selection(1, cx);
                        cx.stop_propagation();
                    }
                    "up" => {
                        this.move_selection(-1, cx);
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.activate_selected(window, cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.dismiss(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(IconName::Search)),
                    )
                    .child(div().flex_1().child(Input::new(&self.input))),
            )
            .when(has_query || self.searching, |this| {
                this.child(
                    v_flex()
                        .id("palette-scroll")
                        .max_h(px(480.))
                        .overflow_y_scroll()
                        .track_scroll(&self.results_scroll)
                        .p_1()
                        .children(rows),
                )
            })
            .when(!has_query && !self.searching, |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Type to search artists, albums and songs…"),
                )
            })
            .into_any_element()
    }
}

impl Render for SearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.palette {
            // The centered box only; root supplies the full-window backdrop.
            return self.render_palette(cx);
        }

        let has_query = !self.input.read(cx).value().trim().is_empty();

        h_flex()
            .relative()
            .w(px(280.))
            .gap_1()
            .items_center()
            .child(div().flex_1().child(Input::new(&self.input).small()))
            .when(has_query, |this| {
                this.child(
                    Button::new("search-clear")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx))),
                )
            })
            .when(self.open, |this| {
                // Deferred so the dropdown paints above the page content
                // rendered after this header row.
                this.child(deferred(self.render_dropdown(cx)))
            })
            .into_any_element()
    }
}
