//! Full-window now-playing overlay with dynamic blurred-art background.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Window, div, img, linear_color_stop,
    linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::SubsonicClient;

use crate::assets::{app_icon, icons};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;

const ART_SIZE: u32 = 600;
/// Tiny fetch for color extraction — low-res average is a fast palette sample.
const BG_ART_SIZE: u32 = 32;

pub enum FullscreenEvent {
    Close,
}

/// Optional side panel next to the controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidePanel {
    Queue,
    Lyrics,
}

impl EventEmitter<FullscreenEvent> for FullscreenPlayer {}

pub struct FullscreenPlayer {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
    art_path: Option<PathBuf>,
    bg_art_path: Option<PathBuf>,
    gradient_colors: Option<(gpui::Rgba, gpui::Rgba)>,
    last_cover_id: Option<String>,
    panel: Option<SidePanel>,
    /// Lyrics text for the song in `lyrics_for`; None while loading or when
    /// the server has none.
    lyrics: Option<String>,
    lyrics_for: Option<String>,
    lyrics_loading: bool,
}

impl FullscreenPlayer {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_volume = player.read(cx).volume;
        let seek = cx.new(|_| SliderState::new().min(0.).max(1.).step(0.001));
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                .step(0.01)
                .default_value(initial_volume)
        });

        cx.subscribe(&seek, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let fraction = value.start();
            this.player.update(cx, |p, _| {
                if let Some(total) = p.duration {
                    p.seek(Duration::from_secs_f32(
                        total.as_secs_f32() * fraction.clamp(0., 1.),
                    ));
                }
            });
        })
        .detach();

        cx.subscribe(&volume, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let v = value.start().clamp(0., 1.);
            this.player.update(cx, |p, cx| p.set_volume(v, cx));
        })
        .detach();

        // Watch player for song changes to update background art and lyrics.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let cover = player
                .read(cx)
                .current_song()
                .and_then(|s| s.cover_art.clone());
            if cover != this.last_cover_id {
                this.last_cover_id = cover.clone();
                this.art_path = None;
                this.bg_art_path = None;
                this.gradient_colors = None;
                if let Some(cover_id) = cover {
                    this.fetch_art(cover_id, cx);
                }
            }
            this.maybe_fetch_lyrics(cx);
            cx.notify();
        })
        .detach();

        Self {
            player,
            session,
            seek,
            volume,
            art_path: None,
            bg_art_path: None,
            gradient_colors: None,
            last_cover_id: None,
            panel: None,
            lyrics: None,
            lyrics_for: None,
            lyrics_loading: false,
        }
    }

    fn toggle_panel(&mut self, panel: SidePanel, cx: &mut Context<Self>) {
        self.panel = if self.panel == Some(panel) {
            None
        } else {
            Some(panel)
        };
        self.maybe_fetch_lyrics(cx);
        cx.notify();
    }

    /// Fetch lyrics for the current song when the lyrics panel is open.
    fn maybe_fetch_lyrics(&mut self, cx: &mut Context<Self>) {
        if self.panel != Some(SidePanel::Lyrics) {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let (id, artist, title) = {
            let p = self.player.read(cx);
            let Some(song) = p.current_song() else {
                self.lyrics = None;
                self.lyrics_for = None;
                return;
            };
            (
                song.id.clone(),
                song.artist.clone(),
                Some(song.title.clone()),
            )
        };
        if self.lyrics_for.as_deref() == Some(id.as_str()) {
            return;
        }
        self.lyrics = None;
        self.lyrics_for = Some(id.clone());
        self.lyrics_loading = true;
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_lyrics(artist.as_deref(), title.as_deref())
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.lyrics_for.as_deref() == Some(id.as_str()) {
                    view.lyrics_loading = false;
                    view.lyrics = result.ok().and_then(|l| l.value).filter(|v| !v.is_empty());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// Right-hand queue list: click a row to jump to it.
    fn render_queue_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (rows, current) = {
            let p = self.player.read(cx);
            let rows: Vec<(usize, String, String)> = p
                .queue
                .iter_ordered()
                .map(|(pos, s)| (pos, s.title.clone(), s.artist.clone().unwrap_or_default()))
                .collect();
            (rows, p.queue.current_pos())
        };
        let items: Vec<gpui::AnyElement> = rows
            .into_iter()
            .map(|(pos, title, artist)| {
                let is_current = current == Some(pos);
                h_flex()
                    .id(gpui::SharedString::from(format!("fsq-{pos}")))
                    .px_2()
                    .py_1()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted.opacity(0.6)))
                    .when(is_current, |s| s.text_color(cx.theme().primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.player.update(cx, |p, cx| p.jump_to(pos, cx));
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(artist),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w(px(320.))
            .h_full()
            .py_12()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Queue"),
            )
            .child(
                v_flex()
                    .id("fs-queue-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(items),
            )
            .into_any_element()
    }

    /// Right-hand lyrics panel (classic getLyrics, unsynced text).
    fn render_lyrics_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body: gpui::AnyElement = if self.lyrics_loading {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Loading…")
                .into_any_element()
        } else {
            match &self.lyrics {
                Some(text) => v_flex()
                    .gap_1()
                    .children(text.lines().map(|line| {
                        div()
                            .text_sm()
                            .child(if line.is_empty() { " " } else { line }.to_string())
                    }))
                    .into_any_element(),
                None => div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No lyrics found")
                    .into_any_element(),
            }
        };
        v_flex()
            .w(px(320.))
            .h_full()
            .py_12()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Lyrics"),
            )
            .child(
                v_flex()
                    .id("fs-lyrics-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }

    fn fetch_art(&self, cover_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let client2 = client.clone();
        let cover_id2 = cover_id.clone();
        // Full-res for the center art card.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client, cover_id, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
        // Tiny version for color extraction.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch(client2, cover_id2, BG_ART_SIZE).await {
                let colors = extract_dominant_colors(&path);
                let _ = this.update(cx, |view, cx| {
                    view.bg_art_path = Some(path);
                    view.gradient_colors = colors;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

fn extract_dominant_colors(path: &std::path::Path) -> Option<(gpui::Rgba, gpui::Rgba)> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgb8();
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return None;
    }
    let mid = h / 2;
    let mut top = [0u64; 3];
    let mut bot = [0u64; 3];
    let mut top_n = 0u64;
    let mut bot_n = 0u64;
    for (_, y, pixel) in img.enumerate_pixels() {
        if y < mid {
            top[0] += pixel[0] as u64;
            top[1] += pixel[1] as u64;
            top[2] += pixel[2] as u64;
            top_n += 1;
        } else {
            bot[0] += pixel[0] as u64;
            bot[1] += pixel[1] as u64;
            bot[2] += pixel[2] as u64;
            bot_n += 1;
        }
    }
    if top_n == 0 || bot_n == 0 {
        return None;
    }
    let darken = 0.6f32;
    let avg = |acc: [u64; 3], n: u64| gpui::Rgba {
        r: (acc[0] as f32 / n as f32 / 255.0) * darken,
        g: (acc[1] as f32 / n as f32 / 255.0) * darken,
        b: (acc[2] as f32 / n as f32 / 255.0) * darken,
        a: 1.0,
    };
    Some((avg(top, top_n), avg(bot, bot_n)))
}

impl Render for FullscreenPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            title,
            artist,
            album,
            position,
            duration,
            playing,
            buffering,
            has_track,
            is_radio,
            shuffle,
            repeat,
        ) = {
            let p = self.player.read(cx);
            let np = p.now_playing();
            let album = p.current_song().and_then(|s| s.album.clone());
            (
                np.as_ref().map(|(t, _)| t.clone()),
                np.as_ref().map(|(_, a)| a.clone()),
                album,
                p.position,
                p.duration,
                p.playing,
                p.buffering,
                np.is_some(),
                p.is_radio(),
                p.queue.shuffle,
                p.queue.repeat,
            )
        };

        // Sync seek slider.
        if let Some(total) = duration
            && total > Duration::ZERO
        {
            let fraction = (position.as_secs_f32() / total.as_secs_f32()).clamp(0., 1.);
            self.seek
                .update(cx, |s, cx| s.set_value(fraction, window, cx));
        }

        let time_now = format_duration(position);
        let time_total = duration
            .map(format_duration)
            .unwrap_or_else(|| "-:--".into());

        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .small()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        div()
            .absolute()
            .left_0()
            .top_0()
            .size_full()
            .bg(cx.theme().background)
            // Gradient derived from album palette.
            .when_some(self.gradient_colors, |this, (top_color, bot_color)| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .size_full()
                        .bg(linear_gradient(
                            160.,
                            linear_color_stop(top_color, 0.),
                            linear_color_stop(bot_color, 1.),
                        )),
                )
            })
            // Readability overlay.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full()
                    .bg(cx.theme().background)
                    .opacity(0.45),
            )
            // Close button.
            .child(
                div().absolute().top_3().right_3().child(
                    Button::new("fs-close")
                        .ghost()
                        .small()
                        .icon(Icon::new(IconName::Close))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(FullscreenEvent::Close);
                        })),
                ),
            )
            // Main content: big art on the left, controls on the right,
            // optional queue/lyrics panel at the far right.
            .child(
                h_flex()
                    .size_full()
                    .items_center()
                    .gap_8()
                    .px_10()
                    // Album art — takes the left half.
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .size(px(if self.panel.is_some() { 320. } else { 420. }))
                                    .rounded_2xl()
                                    .bg(cx.theme().muted)
                                    .overflow_hidden()
                                    .shadow_xl()
                                    .when_some(self.art_path.clone(), |this, path| {
                                        this.child(
                                            img(path)
                                                .size(px(if self.panel.is_some() {
                                                    320.
                                                } else {
                                                    420.
                                                }))
                                                .rounded_2xl(),
                                        )
                                    }),
                            ),
                    )
                    // Info + controls column.
                    .child(
                        v_flex()
                            .flex_1()
                            .max_w(px(560.))
                            .gap_5()
                            .justify_center()
                            // Track info, left-aligned.
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_3xl()
                                            .font_semibold()
                                            .truncate()
                                            .when(!has_track, |s: gpui::Div| {
                                                s.text_color(cx.theme().muted_foreground)
                                            })
                                            .child(
                                                title.unwrap_or_else(|| "Nothing playing".into()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_lg()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(artist.unwrap_or_default()),
                                    )
                                    .when_some(album, |this, alb| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .truncate()
                                                .child(alb),
                                        )
                                    }),
                            )
                            // Seek bar — full column width, larger text.
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_3()
                                    .items_center()
                                    .when(is_radio, |this| {
                                        this.child(
                                            div()
                                                .flex_1()
                                                .text_sm()
                                                .text_color(cx.theme().accent)
                                                .text_center()
                                                .child("● LIVE"),
                                        )
                                    })
                                    .when(!is_radio, |this| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(time_now),
                                        )
                                        .child(div().flex_1().child(Slider::new(&self.seek)))
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(time_total),
                                        )
                                    }),
                            )
                            // Transport controls.
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_center()
                                    .child(
                                        icon_btn("fs-shuffle", icons::SHUFFLE, shuffle).on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.player
                                                    .update(cx, |p, cx| p.toggle_shuffle(cx));
                                            }),
                                        ),
                                    )
                                    .child(
                                        icon_btn("fs-prev", icons::SKIP_BACK, false)
                                            .disabled(!has_track)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.player.update(cx, |p, cx| p.previous(cx));
                                            })),
                                    )
                                    .child(
                                        Button::new("fs-play")
                                            .primary()
                                            .icon(if playing {
                                                app_icon(icons::PAUSE)
                                            } else {
                                                app_icon(icons::PLAY)
                                            })
                                            .loading(buffering)
                                            .disabled(!has_track)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.player.update(cx, |p, cx| p.toggle_play(cx));
                                            })),
                                    )
                                    .child(
                                        icon_btn("fs-next", icons::SKIP_FORWARD, false)
                                            .disabled(!has_track)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.player.update(cx, |p, cx| p.next(cx));
                                            })),
                                    )
                                    .child(
                                        icon_btn(
                                            "fs-repeat",
                                            if repeat == RepeatMode::One {
                                                icons::REPEAT_1
                                            } else {
                                                icons::REPEAT
                                            },
                                            repeat != RepeatMode::Off,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.player.update(cx, |p, cx| p.cycle_repeat(cx));
                                            }),
                                        ),
                                    ),
                            )
                            // Volume — same width as the seek bar.
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_LOW)),
                                    )
                                    .child(div().flex_1().child(Slider::new(&self.volume)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_HIGH)),
                                    ),
                            )
                            // Panel toggles.
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("fs-queue-btn")
                                            .ghost()
                                            .small()
                                            .icon(Icon::new(IconName::PanelRight))
                                            .label("Queue")
                                            .when(self.panel == Some(SidePanel::Queue), |b| {
                                                b.primary()
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_panel(SidePanel::Queue, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("fs-lyrics-btn")
                                            .ghost()
                                            .small()
                                            .icon(Icon::new(IconName::BookOpen))
                                            .label("Lyrics")
                                            .when(self.panel == Some(SidePanel::Lyrics), |b| {
                                                b.primary()
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_panel(SidePanel::Lyrics, cx);
                                            })),
                                    ),
                            ),
                    )
                    // Optional side panel.
                    .when_some(self.panel, |this, panel| {
                        this.child(match panel {
                            SidePanel::Queue => self.render_queue_panel(cx),
                            SidePanel::Lyrics => self.render_lyrics_panel(cx),
                        })
                    }),
            )
    }
}
