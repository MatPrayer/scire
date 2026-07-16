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

use crate::services::artwork;
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;
use crate::ui::icons::{TransportIcon, transport_btn_small};

const ART_SIZE: u32 = 600;
/// Tiny fetch for color extraction — low-res average is a fast palette sample.
const BG_ART_SIZE: u32 = 32;

pub enum FullscreenEvent {
    Close,
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

        // Watch player for song changes to update the background art.
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
        }
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
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

        let repeat_icon = if repeat == RepeatMode::One {
            TransportIcon::RepeatOne
        } else {
            TransportIcon::Repeat
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
            // Main content column.
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_6()
                    .px_8()
                    // Album art.
                    .child(
                        div()
                            .size(px(280.))
                            .rounded_2xl()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .shadow_xl()
                            .when_some(self.art_path.clone(), |this, path| {
                                this.child(img(path).size(px(280.)).rounded_2xl())
                            }),
                    )
                    // Track info.
                    .child(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_semibold()
                                    .max_w(px(480.))
                                    .text_center()
                                    .truncate()
                                    .when(!has_track, |s: gpui::Div| {
                                        s.text_color(cx.theme().muted_foreground)
                                    })
                                    .child(title.unwrap_or_else(|| "Nothing playing".into())),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(cx.theme().muted_foreground)
                                    .max_w(px(480.))
                                    .text_center()
                                    .truncate()
                                    .child(artist.unwrap_or_default()),
                            )
                            .when_some(album, |this, alb| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(alb),
                                )
                            }),
                    )
                    // Seek bar.
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(480.))
                            .gap_2()
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
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(time_now),
                                )
                                .child(div().flex_1().child(Slider::new(&self.seek)))
                                .child(
                                    div()
                                        .text_xs()
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
                                transport_btn_small("fs-shuffle", TransportIcon::Shuffle, shuffle)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.toggle_shuffle(cx));
                                    })),
                            )
                            .child(
                                transport_btn_small("fs-prev", TransportIcon::SkipBack, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.previous(cx));
                                    })),
                            )
                            .child(
                                Button::new("fs-play")
                                    .primary()
                                    .loading(buffering)
                                    .when(!buffering, |b| {
                                        b.icon(Icon::new(if playing {
                                            TransportIcon::Pause
                                        } else {
                                            TransportIcon::Play
                                        }))
                                    })
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.toggle_play(cx));
                                    })),
                            )
                            .child(
                                transport_btn_small("fs-next", TransportIcon::SkipForward, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.next(cx));
                                    })),
                            )
                            .child(
                                transport_btn_small(
                                    "fs-repeat",
                                    repeat_icon,
                                    repeat != RepeatMode::Off,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.cycle_repeat(cx));
                                    },
                                )),
                            ),
                    )
                    // Volume — same width as the seek bar; wide slider for touch-friendly control.
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(480.))
                            .gap_3()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(TransportIcon::Volume)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(div().w(px(420.)).child(Slider::new(&self.volume))),
                    ),
            )
    }
}
