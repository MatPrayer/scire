//! Artist list (grouped by index letter) and artist detail (their albums).

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, img, prelude::*, px};
use gpui_component::{ActiveTheme as _, StyledExt, h_flex, v_flex};
use subsonic::{Album, ArtistIndex, ArtistWithAlbums, SubsonicClient};

use crate::services::{artwork, runtime, spotify};
use crate::state::session::Session;

const ART_SIZE: u32 = 320;

pub enum ArtistsEvent {
    OpenArtist(String),
}

pub struct ArtistsView {
    session: Entity<Session>,
    index: Vec<ArtistIndex>,
    loading: bool,
    error: Option<String>,
}

impl EventEmitter<ArtistsEvent> for ArtistsView {}

impl ArtistsView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            session,
            index: Vec::new(),
            loading: false,
            error: None,
        };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.session.read(cx).client.clone() else {
            return;
        };
        let library_id = self.session.read(cx).library_id.clone();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_artists(library_id.as_ref())
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.loading = false;
                match result {
                    Ok(index) => view.index = index,
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for ArtistsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for bucket in &self.index {
            rows.push(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .mt_2()
                    .child(bucket.name.clone())
                    .into_any_element(),
            );
            for artist in &bucket.artist {
                let id = artist.id.clone();
                let albums = artist
                    .album_count
                    .map(|n| format!("{n} albums"))
                    .unwrap_or_default();
                rows.push(
                    h_flex()
                        .id(gpui::SharedString::from(format!("artist-{}", artist.id)))
                        .justify_between()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ArtistsEvent::OpenArtist(id.clone()));
                        }))
                        .child(div().child(artist.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(albums),
                        )
                        .into_any_element(),
                );
            }
        }

        v_flex()
            .id("artists-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_1()
            .child(div().text_lg().child("Artists"))
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .children(rows)
    }
}

/// One artist's albums.
pub struct ArtistDetailView {
    session: Entity<Session>,
    artist_id: String,
    artist: Option<ArtistWithAlbums>,
    art_paths: HashMap<String, PathBuf>,
    artist_image_path: Option<PathBuf>,
    error: Option<String>,
    top_track: Option<String>,
    artist_info: Option<spotify::ArtistInfoSummary>,
}

pub enum ArtistDetailEvent {
    OpenAlbum(String),
}

impl EventEmitter<ArtistDetailEvent> for ArtistDetailView {}

impl ArtistDetailView {
    pub fn new(session: Entity<Session>, artist_id: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            session,
            artist_id,
            artist: None,
            art_paths: HashMap::new(),
            artist_image_path: None,
            error: None,
            top_track: None,
            artist_info: None,
        };
        this.load(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let id = self.artist_id.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_artist(&id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(artist) => {
                        let artist_id = artist.artist.id.clone();
                        let first_album_id = artist.album.first().map(|album| album.id.clone());
                        if let Some(image) = artist.artist.artist_image_url.clone() {
                            view.fetch_artist_image(Some(image), cx);
                        } else if let Some(cover) = artist.artist.cover_art.clone() {
                            view.fetch_artist_image(Some(cover), cx);
                        }
                        for album in &artist.album {
                            view.fetch_art(album.id.clone(), album.cover_art.clone(), cx);
                        }
                        view.artist = Some(artist);
                        if let Some(album_id) = first_album_id {
                            view.fetch_top_track(album_id, cx);
                        }
                        view.fetch_navidrome_info(&artist_id, cx);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_art(&self, album_id: String, cover_art: Option<String>, cx: &mut Context<Self>) {
        let Some(cover_id) = cover_art else { return };
        let Some(client) = self.client(cx) else {
            return;
        };
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

    fn fetch_artist_image(&self, source: Option<String>, cx: &mut Context<Self>) {
        let Some(source) = source else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        let is_remote = source.starts_with("http://") || source.starts_with("https://");
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                if is_remote {
                    download_remote_image(&source).await
                } else {
                    artwork::fetch(client, source, ART_SIZE).await
                }
            })
            .await;
            if let Ok(path) = result {
                let _ = this.update(cx, |view, cx| {
                    view.artist_image_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn fetch_top_track(&self, album_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_album(&album_id).await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(album) = result {
                    view.top_track = album.song.first().and_then(|song| {
                        let title = song.title.trim();
                        (!title.is_empty()).then(|| title.to_string())
                    });
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_navidrome_info(&self, artist_id: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let artist_id = artist_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                spotify::fetch_artist_info(&client, &artist_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(Some(info)) = result {
                    view.artist_info = Some(info);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

impl Render for ArtistDetailView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .artist
            .as_ref()
            .map(|a| a.artist.name.clone())
            .unwrap_or_else(|| "…".into());
        let bio = self
            .artist
            .as_ref()
            .and_then(|a| a.artist.biography.as_deref())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string())
            .unwrap_or_else(|| "No biography is available for this artist yet.".into());
        let top_track = self
            .top_track
            .clone()
            .unwrap_or_else(|| "No track preview available yet.".into());
        let navidrome_name = self
            .artist_info
            .as_ref()
            .map(|info| info.name.clone())
            .unwrap_or_default();
        let navidrome_desc = self.artist_info.as_ref().and_then(|info| {
            let genres = info.genres.join(", ");
            if genres.is_empty() {
                None
            } else {
                Some(format!("Genres: {genres}"))
            }
        });
        let fallback_art = self
            .artist
            .as_ref()
            .and_then(|a| a.album.first())
            .and_then(|album| self.art_paths.get(&album.id).cloned());
        let hero_art = self.artist_image_path.clone().or(fallback_art);

        let mut album_cards: Vec<gpui::AnyElement> = Vec::new();
        let mut single_cards: Vec<gpui::AnyElement> = Vec::new();
        if let Some(artist) = self.artist.as_ref() {
            for album in &artist.album {
                let id = album.id.clone();
                let art = self.art_paths.get(&album.id).cloned();
                let year = album.year.map(|y| y.to_string()).unwrap_or_default();
                let card = v_flex()
                    .id(gpui::SharedString::from(format!("aalbum-{}", album.id)))
                    .w(px(172.))
                    .p_1p5()
                    .gap_1p5()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ArtistDetailEvent::OpenAlbum(id.clone()));
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
                            .child(div().text_sm().truncate().child(album.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(year),
                            ),
                    )
                    .into_any_element();
                if is_single_or_ep(album) {
                    single_cards.push(card);
                } else {
                    album_cards.push(card);
                }
            }
        }

        let make_section = |title: String, cards: Vec<gpui::AnyElement>| {
            let title_text = title.clone();
            let mut section = v_flex()
                .gap_2()
                .child(div().text_sm().font_medium().child(title_text.clone()));
            if cards.is_empty() {
                section = section.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("No {} yet.", title_text.to_lowercase())),
                );
            } else {
                section = section.child(h_flex().flex_wrap().gap_4().children(cards));
            }
            section.into_any_element()
        };

        v_flex()
            .id("artist-detail-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(
                v_flex()
                    .rounded_2xl()
                    .p_4()
                    .gap_4()
                    .bg(cx.theme().sidebar)
                    .child(
                        h_flex()
                            .gap_4()
                            .flex_wrap()
                            .child(
                                div()
                                    .size(px(220.))
                                    .rounded_2xl()
                                    .overflow_hidden()
                                    .bg(cx.theme().muted)
                                    .when_some(hero_art, |this, path| {
                                        this.child(img(path).size(px(220.)).rounded_2xl())
                                    }),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(260.))
                                    .gap_2()
                                    .child(div().text_2xl().font_medium().child(name))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Bio"),
                                    )
                                    .child(div().text_sm().child(bio))
                                    .when_some(navidrome_desc.clone(), |this, desc| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(desc),
                                        )
                                    })
                                    .when(!navidrome_name.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("Navidrome: {navidrome_name}")),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Most famous track"),
                                    )
                                    .child(div().text_sm().font_medium().child(top_track)),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(make_section("Albums".to_string(), album_cards))
            .child(make_section("Singles / EPs".to_string(), single_cards))
    }
}

fn is_single_or_ep(album: &Album) -> bool {
    let name = album.name.to_lowercase();
    let song_count = album.song_count.unwrap_or_default();
    name.contains("single") || name.contains("ep") || song_count <= 4
}

async fn download_remote_image(url: &str) -> anyhow::Result<PathBuf> {
    let dir = crate::config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{}.img", sanitize_name(url), ART_SIZE));
    std::fs::create_dir_all(&dir)?;
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_single_or_ep;
    use subsonic::Album;

    #[test]
    fn singles_and_eps_are_grouped_separately() {
        let single = Album {
            id: "1".into(),
            name: "Single".into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: Some(2),
            duration: None,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
        };
        let album = Album {
            id: "2".into(),
            name: "Studio Album".into(),
            artist: None,
            artist_id: None,
            cover_art: None,
            song_count: Some(10),
            duration: None,
            created: None,
            year: None,
            genre: None,
            starred: None,
            user_rating: None,
            play_count: None,
        };
        assert!(is_single_or_ep(&single));
        assert!(!is_single_or_ep(&album));
    }
}
