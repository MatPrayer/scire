//! Album grid with cover art, pagination, and All / New tabs.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, img, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use subsonic::{Album, AlbumListType, SubsonicClient};

use crate::services::{artwork, runtime};
use crate::state::session::Session;

const PAGE_SIZE: u32 = 100;
const ART_SIZE: u32 = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlbumTab {
    All,
    New,
}

pub enum AlbumsEvent {
    OpenAlbum(String),
}

pub struct AlbumsView {
    session: Entity<Session>,
    /// Albums for each tab, loaded independently.
    all_albums: Vec<Album>,
    new_albums: Vec<Album>,
    art_paths: HashMap<String, PathBuf>,
    active_tab: AlbumTab,
    loading_all: bool,
    loading_new: bool,
    exhausted_all: bool,
    exhausted_new: bool,
    error: Option<String>,
}

impl EventEmitter<AlbumsEvent> for AlbumsView {}

impl AlbumsView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            session,
            all_albums: Vec::new(),
            new_albums: Vec::new(),
            art_paths: HashMap::new(),
            active_tab: AlbumTab::All,
            loading_all: false,
            loading_new: false,
            exhausted_all: false,
            exhausted_new: false,
            error: None,
        };
        this.load_more_all(cx);
        this.load_more_new(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn load_more_all(&mut self, cx: &mut Context<Self>) {
        if self.loading_all || self.exhausted_all {
            return;
        }
        self.load_tab(
            AlbumListType::AlphabeticalByName,
            self.all_albums.len() as u32,
            cx,
        );
    }

    fn load_more_new(&mut self, cx: &mut Context<Self>) {
        if self.loading_new || self.exhausted_new {
            return;
        }
        self.load_tab(AlbumListType::Newest, self.new_albums.len() as u32, cx);
    }

    fn load_tab(&mut self, list_type: AlbumListType, offset: u32, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let library_id = self.session.read(cx).library_id.clone();
        self.error = None;
        match list_type {
            AlbumListType::AlphabeticalByName => self.loading_all = true,
            AlbumListType::Newest => self.loading_new = true,
            _ => {}
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_album_list2(list_type, PAGE_SIZE, offset, library_id.as_ref())
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;

            let _ = this.update(cx, |view, cx| {
                match list_type {
                    AlbumListType::AlphabeticalByName => {
                        view.loading_all = false;
                        match result {
                            Ok(batch) => {
                                view.exhausted_all = batch.len() < PAGE_SIZE as usize;
                                for album in &batch {
                                    view.fetch_art(album, cx);
                                }
                                view.all_albums.extend(batch);
                            }
                            Err(e) => view.error = Some(format!("{e:#}")),
                        }
                    }
                    AlbumListType::Newest => {
                        view.loading_new = false;
                        match result {
                            Ok(batch) => {
                                view.exhausted_new = batch.len() < PAGE_SIZE as usize;
                                for album in &batch {
                                    view.fetch_art(album, cx);
                                }
                                view.new_albums.extend(batch);
                            }
                            Err(e) => view.error = Some(format!("{e:#}")),
                        }
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_art(&self, album: &Album, cx: &mut Context<Self>) {
        let Some(cover_id) = album.cover_art.clone() else {
            return;
        };
        if self.art_paths.contains_key(&album.id) {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let album_id = album.id.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_paths.insert(album_id, path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn render_card(&self, album: &Album, cx: &Context<Self>) -> impl IntoElement + use<> {
        let id = album.id.clone();
        let art = self.art_paths.get(&album.id).cloned();
        let name = album.name.clone();
        let artist = album.artist.clone().unwrap_or_default();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();

        v_flex()
            .id(gpui::SharedString::from(format!("album-{}", album.id)))
            .w(px(172.))
            .p_1p5()
            .gap_1p5()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().muted))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(AlbumsEvent::OpenAlbum(id.clone()));
            }))
            .child(
                div()
                    .size(px(160.))
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .shadow_sm()
                    .when_some(art, |this, path| {
                        this.child(img(path).size(px(160.)).rounded_lg())
                    }),
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
    }
}

impl Render for AlbumsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab;

        let tab_btn = |label: &'static str, t: AlbumTab| {
            let active = tab == t;
            Button::new(label)
                .ghost()
                .xsmall()
                .label(label)
                .when(active, |b: Button| b.primary())
        };

        let tabs = h_flex()
            .gap_1()
            .child(
                tab_btn("All", AlbumTab::All).on_click(cx.listener(|this, _, _, cx| {
                    this.active_tab = AlbumTab::All;
                    cx.notify();
                })),
            )
            .child(
                tab_btn("New", AlbumTab::New).on_click(cx.listener(|this, _, _, cx| {
                    this.active_tab = AlbumTab::New;
                    if this.new_albums.is_empty() {
                        this.load_more_new(cx);
                    }
                    cx.notify();
                })),
            );

        let (albums, loading, exhausted) = match tab {
            AlbumTab::All => (&self.all_albums, self.loading_all, self.exhausted_all),
            AlbumTab::New => (&self.new_albums, self.loading_new, self.exhausted_new),
        };

        let cards: Vec<_> = albums
            .iter()
            .map(|album| self.render_card(album, cx).into_any_element())
            .collect();

        v_flex()
            .id("albums-scroll")
            .size_full()
            .overflow_y_scroll()
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
            .child(h_flex().flex_wrap().gap_4().children(cards))
            .when(!exhausted, |this| {
                this.child(
                    h_flex().justify_center().child(
                        Button::new("load-more")
                            .ghost()
                            .label("Load more")
                            .loading(loading)
                            .on_click(cx.listener(|view, _, _, cx| match view.active_tab {
                                AlbumTab::All => view.load_more_all(cx),
                                AlbumTab::New => view.load_more_new(cx),
                            })),
                    ),
                )
            })
    }
}
