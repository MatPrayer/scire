//! Artist list (grouped by index letter) and artist detail (their albums).

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, img, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::link::Link;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt, h_flex, v_flex};
use subsonic::{Album, ArtistIndex, ArtistInfo2, ArtistWithAlbums, SubsonicClient};

use crate::services::{artwork, runtime};
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
        let libraries = self.session.read(cx).library_query_ids();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                // One request per selected library; merge the index buckets
                // and dedupe artists that live in several libraries.
                let mut merged: Vec<subsonic::ArtistIndex> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for lib in &libraries {
                    let index = client
                        .get_artists(lib.as_ref())
                        .await
                        .map_err(anyhow::Error::from)?;
                    for bucket in index {
                        let artists: Vec<_> = bucket
                            .artist
                            .into_iter()
                            .filter(|a| seen.insert(a.id.clone()))
                            .collect();
                        if artists.is_empty() {
                            continue;
                        }
                        match merged.iter_mut().find(|b| b.name == bucket.name) {
                            Some(existing) => existing.artist.extend(artists),
                            None => merged.push(subsonic::ArtistIndex {
                                name: bucket.name,
                                artist: artists,
                            }),
                        }
                    }
                }
                merged.sort_by(|a, b| a.name.cmp(&b.name));
                for bucket in &mut merged {
                    bucket.artist.sort_by(|a, b| a.name.cmp(&b.name));
                }
                Ok::<_, anyhow::Error>(merged)
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
    /// Biography + image URLs from getArtistInfo2 (Navidrome's agents).
    info: Option<ArtistInfo2>,
    /// An artist-image fetch has started; stops info2's fallback from
    /// racing/overwriting the primary coverArt fetch.
    image_requested: bool,
    /// Long bios render clamped to a few lines until expanded.
    bio_expanded: bool,
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
            info: None,
            image_requested: false,
            bio_expanded: false,
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
                        for album in &artist.album {
                            view.fetch_art(album.id.clone(), album.cover_art.clone(), cx);
                        }
                        let cover = artist.artist.cover_art.clone();
                        view.artist = Some(artist);
                        view.fetch_artist_image(cover, cx);
                        view.fetch_artist_info(&artist_id, cx);
                    }
                    Err(e) => view.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fetch_art(&mut self, album_id: String, cover_art: Option<String>, cx: &mut Context<Self>) {
        let Some(cover_id) = cover_art else { return };
        // Synchronous cache hit: render instantly on restart, no task.
        if let Some(path) = artwork::cached(&cover_id, ART_SIZE) {
            self.art_paths.insert(album_id, path);
            return;
        }
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

    fn fetch_artist_image(&mut self, source: Option<String>, cx: &mut Context<Self>) {
        let Some(source) = source else {
            return;
        };
        let Some(client) = self.client(cx) else {
            return;
        };
        self.image_requested = true;
        let is_remote = source.starts_with("http://") || source.starts_with("https://");
        // Synchronous cache hit: no empty-frame flash on revisit.
        if !is_remote && let Some(path) = artwork::cached(&source, ART_SIZE) {
            self.artist_image_path = Some(path);
            cx.notify();
            return;
        }
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

    /// Biography and artist image from Navidrome (getArtistInfo2). Falls back
    /// to the artist's own image fields when info2 has no usable image.
    fn fetch_artist_info(&self, artist_id: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let artist_id = artist_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_artist_info2(&artist_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                let info = result.unwrap_or_default();
                // The primary image (artist coverArt) started in load();
                // info2 URLs are only a fallback for artists without one.
                if !view.image_requested {
                    let image = info.image_url().map(str::to_string).or_else(|| {
                        view.artist
                            .as_ref()
                            .and_then(|a| a.artist.artist_image_url.clone())
                    });
                    view.fetch_artist_image(image, cx);
                }
                view.info = Some(info);
                cx.notify();
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
            .info
            .as_ref()
            .and_then(|i| i.biography.as_deref())
            .map(strip_html)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "No biography is available for this artist yet.".into());
        // Collapse long bios by truncating the string itself: gpui's
        // line_clamp lets the last line run past the container and its text
        // measurement cache ignores clamp changes, so it can't do this job.
        let bio_long = bio.chars().count() > BIO_PREVIEW_CHARS;
        let bio_text = if self.bio_expanded || !bio_long {
            bio
        } else {
            truncate_at_word(&bio, BIO_PREVIEW_CHARS)
        };
        // External links from getArtistInfo2 (same sources as Navidrome's UI).
        let musicbrainz_url = self
            .info
            .as_ref()
            .and_then(|i| i.music_brainz_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://musicbrainz.org/artist/{id}"));
        let lastfm_url = self
            .info
            .as_ref()
            .and_then(|i| i.last_fm_url.as_deref())
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        let genres = self.artist.as_ref().map(|a| {
            let mut seen = std::collections::HashSet::new();
            a.album
                .iter()
                .filter_map(|album| album.genre.as_deref())
                .map(str::trim)
                .filter(|g| !g.is_empty() && seen.insert(g.to_lowercase()))
                .map(str::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        });
        let genres_line = genres
            .filter(|g| !g.is_empty())
            .map(|g| format!("Genres: {g}"));
        let hero_art = self.artist_image_path.clone();

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
                            .items_start()
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
                                    .child(div().text_sm().child(bio_text))
                                    .when(bio_long, |this| {
                                        let expanded = self.bio_expanded;
                                        this.child(
                                            h_flex().child(
                                                Button::new("bio-toggle")
                                                    .ghost()
                                                    .xsmall()
                                                    .label(if expanded { "Less" } else { "More" })
                                                    .icon(Icon::new(if expanded {
                                                        IconName::ChevronUp
                                                    } else {
                                                        IconName::ChevronDown
                                                    }))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.bio_expanded = !this.bio_expanded;
                                                        cx.notify();
                                                    })),
                                            ),
                                        )
                                    })
                                    .when_some(genres_line, |this, desc| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(desc),
                                        )
                                    })
                                    .when(
                                        musicbrainz_url.is_some() || lastfm_url.is_some(),
                                        |this| {
                                            this.child(
                                                h_flex()
                                                    .gap_3()
                                                    .text_sm()
                                                    .when_some(musicbrainz_url, |this, url| {
                                                        this.child(
                                                            Link::new("mb-link")
                                                                .href(url)
                                                                .child("MusicBrainz"),
                                                        )
                                                    })
                                                    .when_some(lastfm_url, |this, url| {
                                                        this.child(
                                                            Link::new("lastfm-link")
                                                                .href(url)
                                                                .child("Last.fm"),
                                                        )
                                                    }),
                                            )
                                        },
                                    ),
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

/// Collapsed-bio length; roughly four lines at typical window widths.
const BIO_PREVIEW_CHARS: usize = 400;

/// Cut `text` down to at most `max_chars`, backing up to the last word
/// boundary, with a trailing ellipsis.
fn truncate_at_word(text: &str, max_chars: usize) -> String {
    let byte_cut = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = &text[..byte_cut];
    let cut = head.rfind(char::is_whitespace).unwrap_or(byte_cut);
    format!("{} …", head[..cut].trim_end())
}

/// Strip HTML tags and decode the handful of entities Last.fm bios use
/// (Navidrome forwards agent bios verbatim, tags included).
fn strip_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

fn is_single_or_ep(album: &Album) -> bool {
    let name = album.name.to_lowercase();
    let song_count = album.song_count.unwrap_or_default();
    name.contains("single") || name.contains("ep") || song_count <= 4
}

async fn download_remote_image(url: &str) -> anyhow::Result<PathBuf> {
    let dir = crate::config::artwork_cache_dir()?;
    let path = dir.join(format!("{}-{}.img", sanitize_name(url), ART_SIZE));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    // Temp file + rename so a partial download never poisons the cache.
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
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
