//! Global search bar (top right of every page): debounced search3 with a
//! dropdown of song / album / artist results.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Window, deferred, div, img, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use subsonic::SearchResult3;

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

pub struct SearchBar {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    input: Entity<InputState>,
    results: Option<SearchResult3>,
    /// Dropdown visibility; results stay cached while hidden so reopening
    /// the same query is instant.
    open: bool,
    searching: bool,
    error: Option<String>,
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
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search…")
                .clean_on_escape()
        });

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

    /// Close the dropdown and clear the query (root's Escape handler).
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        self.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.results = None;
        cx.notify();
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
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
        let cover_ids: Vec<String> = results
            .song
            .iter()
            .take(MAX_SONGS)
            .filter_map(|s| s.cover_art.clone())
            .chain(
                results
                    .album
                    .iter()
                    .take(MAX_ALBUMS)
                    .filter_map(|a| a.cover_art.clone()),
            )
            .chain(
                results
                    .artist
                    .iter()
                    .take(MAX_ARTISTS)
                    .filter_map(|a| a.cover_art.clone()),
            )
            .collect();
        for cover_id in cover_ids {
            if self.art_paths.contains_key(&cover_id) {
                continue;
            }
            // Synchronous cache hit: no task, renders with the results.
            if let Some(path) = artwork::cached(&cover_id, ART_SIZE) {
                self.art_paths.insert(cover_id, path);
                continue;
            }
            let Some(client) = self.session.read(cx).client.clone() else {
                return;
            };
            let id = cover_id.clone();
            cx.spawn(async move |this, cx| {
                if let Ok(path) =
                    runtime::spawn_io(artwork::fetch(client, id.clone(), ART_SIZE)).await
                {
                    let _ = this.update(cx, |bar, cx| {
                        bar.art_paths.insert(id, path);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    fn thumb(
        &self,
        cover_art: Option<&String>,
        fallback: IconName,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let path = cover_art.and_then(|id| self.art_paths.get(id).cloned());
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

    fn render_dropdown(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();

        if let Some(results) = &self.results {
            if !results.artist.is_empty() {
                rows.push(Self::section_title("Artists", cx));
                for (i, artist) in results.artist.iter().take(MAX_ARTISTS).enumerate() {
                    let id = artist.id.clone();
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(SearchBarEvent::OpenArtist(id.clone()));
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(artist.cover_art.as_ref(), IconName::CircleUser, cx))
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
                }
            }

            if !results.album.is_empty() {
                rows.push(Self::section_title("Albums", cx));
                for (i, album) in results.album.iter().take(MAX_ALBUMS).enumerate() {
                    let id = album.id.clone();
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(SearchBarEvent::OpenAlbum(id.clone()));
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(
                                album.cover_art.as_ref(),
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
                }
            }

            // Songs: click plays, `+` enqueues.
            if !results.song.is_empty() {
                rows.push(Self::section_title("Songs", cx));
                for (i, song) in results.song.iter().take(MAX_SONGS).enumerate() {
                    let play = song.clone();
                    let enqueue = song.clone();
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.player.update(cx, |p, cx| {
                                    p.play_queue(vec![play.clone()], 0, cx);
                                });
                                this.dismiss(window, cx);
                            }))
                            .child(self.thumb(song.cover_art.as_ref(), IconName::Star, cx))
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

        v_flex()
            .id("search-dropdown")
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
            .children(rows)
            .into_any_element()
    }
}

impl Render for SearchBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
    }
}
