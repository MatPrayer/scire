//! Bottom transport bar: track info, play/pause/next/prev, seek, volume.

use std::time::Duration;

use gpui::{Context, Entity, EventEmitter, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;
use crate::ui::icons::{TransportIcon, transport_btn};

/// Bubbled to RootView.
pub enum PlayerBarEvent {
    ToggleQueue,
    ToggleFullscreen,
}

impl EventEmitter<PlayerBarEvent> for PlayerBar {}

pub struct PlayerBar {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
}

impl PlayerBar {
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

        cx.observe(&player, |_, _, cx| cx.notify()).detach();

        // User dragged the seek slider (set_value does not emit Change).
        cx.subscribe(&seek, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let fraction = value.start();
            this.player.update(cx, |player, _| {
                if let Some(total) = player.duration {
                    player.seek(Duration::from_secs_f32(
                        total.as_secs_f32() * fraction.clamp(0., 1.),
                    ));
                }
            });
        })
        .detach();

        cx.subscribe(&volume, |this: &mut Self, _, event, cx| {
            let SliderEvent::Change(value) = event;
            let v = value.start().clamp(0., 1.);
            this.player
                .update(cx, |player, cx| player.set_volume(v, cx));
            this.session.update(cx, |session, _| {
                session.settings.volume = v;
                session.persist_settings();
            });
        })
        .detach();

        Self {
            player,
            session,
            seek,
            volume,
        }
    }
}

impl Render for PlayerBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            title,
            artist,
            position,
            duration,
            playing,
            buffering,
            has_track,
            is_radio,
            error,
            shuffle,
            repeat,
        ) = {
            let p = self.player.read(cx);
            let np = p.now_playing();
            (
                np.as_ref().map(|(t, _)| t.clone()),
                np.as_ref().map(|(_, a)| a.clone()),
                p.position,
                p.duration,
                p.playing,
                p.buffering,
                np.is_some(),
                p.is_radio(),
                p.last_error.clone(),
                p.queue.shuffle,
                p.queue.repeat,
            )
        };

        // Keep the seek slider in sync with playback position.
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

        // Transport icon buttons; primary play/pause with loading spinner.
        let repeat_icon = if repeat == RepeatMode::One {
            TransportIcon::RepeatOne
        } else {
            TransportIcon::Repeat
        };

        h_flex()
            .w_full()
            .h(px(64.))
            .px_4()
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            // Track info — click to open fullscreen player.
            .child(
                v_flex()
                    .id("track-info")
                    .w(px(220.))
                    .gap_0()
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(PlayerBarEvent::ToggleFullscreen);
                    }))
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .truncate()
                            .when(!has_track, |s| s.text_color(cx.theme().muted_foreground))
                            .child(title.unwrap_or_else(|| "Not playing".into())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(artist.unwrap_or_default()),
                    )
                    .when_some(error, |this, e| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .truncate()
                                .child(e),
                        )
                    }),
            )
            // Transport + seek
            .child(
                v_flex()
                    .flex_1()
                    .gap_0p5()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                transport_btn("shuffle", TransportIcon::Shuffle, shuffle).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.toggle_shuffle(cx));
                                    }),
                                ),
                            )
                            .child(
                                transport_btn("prev", TransportIcon::SkipBack, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.previous(cx));
                                    })),
                            )
                            .child(
                                Button::new("play-pause")
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
                                transport_btn("next", TransportIcon::SkipForward, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.next(cx));
                                    })),
                            )
                            .child(
                                transport_btn("repeat", repeat_icon, repeat != RepeatMode::Off)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.cycle_repeat(cx));
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(520.))
                            .gap_2()
                            .items_center()
                            // Live radio has no timeline — show a label instead.
                            .when(is_radio, |this| {
                                this.child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(cx.theme().accent)
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
                    ),
            )
            // Volume + queue toggle
            .child(
                h_flex()
                    .w(px(190.))
                    .gap_2()
                    .items_center()
                    .justify_end()
                    .child(
                        Icon::new(TransportIcon::Volume)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().w(px(110.)).child(Slider::new(&self.volume)))
                    .child(
                        Button::new("queue-toggle")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::PanelRight))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PlayerBarEvent::ToggleQueue);
                            })),
                    ),
            )
    }
}
