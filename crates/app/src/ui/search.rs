//! Debounced global search over songs / albums / artists (search3).

use std::time::Duration;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use subsonic::SearchResult3;

use crate::services::runtime;
use crate::state::player::PlayerState;
use crate::state::session::Session;

const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    Songs,
    Albums,
    Artists,
}

pub enum SearchEvent {
    OpenAlbum(String),
    OpenArtist(String),
}

pub struct SearchView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    input: Entity<InputState>,
    results: Option<SearchResult3>,
    filter: SearchFilter,
    searching: bool,
    error: Option<String>,
    generation: u64,
}

impl EventEmitter<SearchEvent> for SearchView {}

impl SearchView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search music…"));

        cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_query_changed(cx);
            }
        })
        .detach();

        input.update(cx, |state, cx| state.focus(window, cx));

        Self {
            session,
            player,
            input,
            results: None,
            filter: SearchFilter::All,
            searching: false,
            error: None,
            generation: 0,
        }
    }

    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |view, cx| {
                if view.generation == generation {
                    view.run_search(cx);
                }
            });
        })
        .detach();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().trim().to_string();
        if query.is_empty() {
            self.results = None;
            self.searching = false;
            cx.notify();
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let library_id = self.session.read(cx).library_id.clone();
        self.searching = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .search3(&query, library_id.as_ref())
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.searching = false;
                match result {
                    Ok(r) => {
                        view.results = Some(r);
                        view.error = None;
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for SearchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter;

        // Filter tab buttons.
        let tab_btn = |label: &'static str, tab: SearchFilter| {
            let is_active = filter == tab;
            Button::new(label)
                .ghost()
                .xsmall()
                .label(label)
                .when(is_active, |b| b.primary())
        };

        let tabs = h_flex()
            .gap_1()
            .child(
                tab_btn("All", SearchFilter::All).on_click(cx.listener(|this, _, _, cx| {
                    this.filter = SearchFilter::All;
                    cx.notify();
                })),
            )
            .child(
                tab_btn("Songs", SearchFilter::Songs).on_click(cx.listener(|this, _, _, cx| {
                    this.filter = SearchFilter::Songs;
                    cx.notify();
                })),
            )
            .child(
                tab_btn("Albums", SearchFilter::Albums).on_click(cx.listener(|this, _, _, cx| {
                    this.filter = SearchFilter::Albums;
                    cx.notify();
                })),
            )
            .child(
                tab_btn("Artists", SearchFilter::Artists).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.filter = SearchFilter::Artists;
                        cx.notify();
                    },
                )),
            );

        let mut sections: Vec<gpui::AnyElement> = Vec::new();

        if let Some(results) = &self.results {
            let show_songs = filter == SearchFilter::All || filter == SearchFilter::Songs;
            let show_albums = filter == SearchFilter::All || filter == SearchFilter::Albums;
            let show_artists = filter == SearchFilter::All || filter == SearchFilter::Artists;

            // Songs
            if show_songs && !results.song.is_empty() {
                sections.push(section_title("Songs", cx));
                for (i, song) in results.song.iter().enumerate() {
                    let s1 = song.clone();
                    let s2 = song.clone();
                    sections.push(
                        h_flex()
                            .id(("search-song", i))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.player.update(cx, |p, cx| {
                                    p.play_queue(vec![s1.clone()], 0, cx);
                                });
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(song.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(song.artist.clone().unwrap_or_default()),
                            )
                            .child(
                                Button::new(("search-enq", i))
                                    .ghost()
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.player
                                            .update(cx, |p, cx| p.enqueue(vec![s2.clone()], cx));
                                        cx.stop_propagation();
                                    })),
                            )
                            .into_any_element(),
                    );
                }
            }

            // Albums
            if show_albums && !results.album.is_empty() {
                sections.push(section_title("Albums", cx));
                for (i, album) in results.album.iter().enumerate() {
                    let id = album.id.clone();
                    sections.push(
                        h_flex()
                            .id(("search-album", i))
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SearchEvent::OpenAlbum(id.clone()));
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(album.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(album.artist.clone().unwrap_or_default()),
                            )
                            .into_any_element(),
                    );
                }
            }

            // Artists
            if show_artists && !results.artist.is_empty() {
                sections.push(section_title("Artists", cx));
                for (i, artist) in results.artist.iter().enumerate() {
                    let id = artist.id.clone();
                    sections.push(
                        h_flex()
                            .id(("search-artist", i))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SearchEvent::OpenArtist(id.clone()));
                            }))
                            .child(div().child(artist.name.clone()))
                            .into_any_element(),
                    );
                }
            }

            let has_results = match filter {
                SearchFilter::All => {
                    !results.song.is_empty()
                        || !results.album.is_empty()
                        || !results.artist.is_empty()
                }
                SearchFilter::Songs => !results.song.is_empty(),
                SearchFilter::Albums => !results.album.is_empty(),
                SearchFilter::Artists => !results.artist.is_empty(),
            };
            if !has_results {
                sections.push(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("No results")
                        .into_any_element(),
                );
            }
        }

        v_flex()
            .id("search-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(div().max_w(px(480.)).child(Input::new(&self.input)))
            .when(self.results.is_some(), |this| this.child(tabs))
            .when(self.searching, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Searching…"),
                )
            })
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .children(sections)
    }
}

fn section_title(label: &'static str, cx: &Context<SearchView>) -> gpui::AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .mt_2()
        .child(label)
        .into_any_element()
}
