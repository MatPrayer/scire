//! Bottom transport bar: track info, play/pause/next/prev, seek, volume.

use std::time::Duration;

use gpui::{
    Context, Entity, EventEmitter, IntoElement, MouseButton, Render, Window, div, img, prelude::*,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};

use crate::assets::{app_icon, icons};
use crate::services::{runtime, waveform};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;

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
    /// Waveform peaks for the track in `waveform_for` (when the waveform
    /// seek bar is enabled and the decode finished).
    waveform: Option<Vec<f32>>,
    waveform_for: Option<String>,
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

        cx.observe(&player, |this: &mut Self, _, cx| {
            this.maybe_fetch_waveform(cx);
            cx.notify();
        })
        .detach();

        // Settings toggle for the waveform seek bar lives on the session.
        cx.observe(&session, |this: &mut Self, _, cx| {
            this.maybe_fetch_waveform(cx);
            cx.notify();
        })
        .detach();

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
            waveform: None,
            waveform_for: None,
        }
    }

    /// Kick off a waveform decode when the current track changed (and the
    /// setting is on). Drops stale peaks when playback moved on or the
    /// setting was turned off.
    fn maybe_fetch_waveform(&mut self, cx: &mut Context<Self>) {
        let enabled = self.session.read(cx).settings.waveform_seekbar;
        let song_id = {
            let p = self.player.read(cx);
            if !enabled || p.is_radio() {
                None
            } else {
                p.current_song().map(|s| s.id.clone())
            }
        };
        let Some(id) = song_id else {
            self.waveform = None;
            self.waveform_for = None;
            return;
        };
        if self.waveform_for.as_deref() == Some(id.as_str()) {
            return;
        }
        // A low-bitrate transcode keeps the extra download small — the
        // amplitude envelope survives lossy compression just fine.
        let opts = subsonic::StreamOptions {
            format: Some("mp3".into()),
            max_bit_rate: Some(96),
        };
        let url = self
            .session
            .read(cx)
            .client
            .as_ref()
            .and_then(|c| c.stream_url(&id, &opts).ok().map(|u| u.to_string()));
        let Some(url) = url else { return };
        self.waveform = None;
        self.waveform_for = Some(id.clone());
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(waveform::fetch_peaks(url, id.clone())).await;
            let _ = this.update(cx, |bar, cx| {
                // Ignore results for a track that is no longer current.
                if bar.waveform_for.as_deref() == Some(id.as_str()) {
                    match result {
                        Ok(peaks) => bar.waveform = Some(peaks),
                        Err(e) => tracing::warn!("waveform peaks failed: {e:#}"),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The waveform seek bar: one bar per peak bucket, played part accented,
    /// click seeks to that spot.
    /// Continuous filled waveform: a symmetric amplitude envelope built as a
    /// single polygon per region (played / remaining) and painted on a canvas.
    /// Click seeks to the fraction under the cursor.
    fn render_waveform(
        &self,
        peaks: &[f32],
        fraction: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use std::cell::Cell;
        use std::rc::Rc;

        let peaks: Rc<Vec<f32>> = Rc::new(peaks.to_vec());
        let played_color = cx.theme().primary;
        let rest_color = cx.theme().muted_foreground.opacity(0.35);
        // Canvas bounds captured at paint time so the click handler can map
        // the mouse x back to a seek fraction.
        let bounds_cell: Rc<Cell<Option<gpui::Bounds<gpui::Pixels>>>> = Rc::new(Cell::new(None));
        let bounds_for_paint = bounds_cell.clone();
        let bounds_for_click = bounds_cell.clone();

        // Build the envelope polygon for buckets [from, to): across the top
        // edge, then back along the mirrored bottom edge.
        fn envelope(
            peaks: &[f32],
            from: usize,
            to: usize,
            bounds: gpui::Bounds<gpui::Pixels>,
        ) -> Option<gpui::Path<gpui::Pixels>> {
            if to <= from {
                return None;
            }
            let n = peaks.len().max(1) as f32;
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let cy = y0 + h / 2.;
            let x_at = |i: usize| x0 + w * i as f32 / n;
            let half = |p: f32| (p * h / 2.).max(0.75);

            let mut pb = gpui::PathBuilder::fill();
            pb.move_to(gpui::point(px(x_at(from)), px(cy - half(peaks[from]))));
            let buckets = || peaks.iter().enumerate().take(to).skip(from);
            for (i, &peak) in buckets() {
                // Two points per bucket keep the envelope step-accurate
                // without lyon having to interpolate long diagonals.
                let y = cy - half(peak);
                pb.line_to(gpui::point(px(x_at(i)), px(y)));
                pb.line_to(gpui::point(px(x_at(i + 1)), px(y)));
            }
            for (i, &peak) in buckets().rev() {
                let y = cy + half(peak);
                pb.line_to(gpui::point(px(x_at(i + 1)), px(y)));
                pb.line_to(gpui::point(px(x_at(i)), px(y)));
            }
            pb.build().ok()
        }

        div()
            .flex_1()
            .h(px(26.))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    let Some(bounds) = bounds_for_click.get() else {
                        return;
                    };
                    let w = f32::from(bounds.size.width);
                    if w <= 0. {
                        return;
                    }
                    let x = f32::from(event.position.x) - f32::from(bounds.origin.x);
                    let target = (x / w).clamp(0., 1.);
                    this.player.update(cx, |player, _| {
                        if let Some(total) = player.duration {
                            player.seek(Duration::from_secs_f32(total.as_secs_f32() * target));
                        }
                    });
                }),
            )
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        bounds_for_paint.set(Some(bounds));
                        let n = peaks.len();
                        if n == 0 {
                            return;
                        }
                        let split = ((fraction.clamp(0., 1.) * n as f32).round() as usize).min(n);
                        if let Some(path) = envelope(&peaks, split, n, bounds) {
                            window.paint_path(path, rest_color);
                        }
                        if let Some(path) = envelope(&peaks, 0, split, bounds) {
                            window.paint_path(path, played_color);
                        }
                    },
                )
                .size_full(),
            )
            .into_any_element()
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
            art_path,
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
                p.current_art_path.clone(),
            )
        };

        let seek_fraction = match duration {
            Some(total) if total > Duration::ZERO => {
                (position.as_secs_f32() / total.as_secs_f32()).clamp(0., 1.)
            }
            _ => 0.,
        };
        // Keep the seek slider in sync with playback position.
        if duration.is_some_and(|total| total > Duration::ZERO) {
            self.seek
                .update(cx, |s, cx| s.set_value(seek_fraction, window, cx));
        }

        let waveform_enabled = self.session.read(cx).settings.waveform_seekbar;

        let time_now = format_duration(position);
        let time_total = duration
            .map(format_duration)
            .unwrap_or_else(|| "-:--".into());

        // Small, quiet transport icon buttons; primary circular play.
        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .small()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        h_flex()
            .w_full()
            .h(px(108.))
            .px_4()
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            // Cover + track info — click to open fullscreen player.
            .child(
                h_flex()
                    .id("track-info")
                    .w(px(300.))
                    .gap_3()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(PlayerBarEvent::ToggleFullscreen);
                    }))
                    .child(
                        div()
                            .size(px(64.))
                            .flex_none()
                            .rounded_md()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .shadow_sm()
                            .when_some(art_path, |this, path| {
                                this.child(img(path).size(px(64.)).rounded_md())
                            })
                            // Placeholder icon while no artwork.
                            .when(!has_track, |this| {
                                this.flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(app_icon(icons::MUSIC))
                            }),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            .min_w_0()
                            .flex_1()
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
                    ),
            )
            // Transport + seek. The waveform is a taller, busier shape than
            // the slider — give it more breathing room below the controls.
            .child(
                v_flex()
                    .flex_1()
                    .gap(if waveform_enabled && self.waveform.is_some() {
                        px(10.)
                    } else {
                        px(2.)
                    })
                    .items_center()
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(icon_btn("shuffle", icons::SHUFFLE, shuffle).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.player.update(cx, |p, cx| p.toggle_shuffle(cx));
                                }),
                            ))
                            .child(
                                icon_btn("prev", icons::SKIP_BACK, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.previous(cx));
                                    })),
                            )
                            .child(
                                Button::new("play-pause")
                                    .primary()
                                    .icon(if buffering {
                                        Icon::new(IconName::LoaderCircle)
                                    } else if playing {
                                        app_icon(icons::PAUSE)
                                    } else {
                                        app_icon(icons::PLAY)
                                    })
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.toggle_play(cx));
                                    })),
                            )
                            .child(
                                icon_btn("next", icons::SKIP_FORWARD, false)
                                    .disabled(!has_track)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.next(cx));
                                    })),
                            )
                            .child(
                                icon_btn(
                                    "repeat",
                                    if repeat == RepeatMode::One {
                                        icons::REPEAT_1
                                    } else {
                                        icons::REPEAT
                                    },
                                    repeat != RepeatMode::Off,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.cycle_repeat(cx));
                                    },
                                )),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .max_w(px(640.))
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
                                .map(|this| {
                                    // Waveform seek bar when enabled and
                                    // decoded; slider otherwise.
                                    match (waveform_enabled, self.waveform.clone()) {
                                        (true, Some(peaks)) => this.child(self.render_waveform(
                                            &peaks,
                                            seek_fraction,
                                            cx,
                                        )),
                                        _ => this
                                            .child(div().flex_1().child(Slider::new(&self.seek))),
                                    }
                                })
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
                    .w(px(220.))
                    .gap_2()
                    .items_center()
                    .justify_end()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(app_icon(icons::VOLUME_HIGH)),
                    )
                    .child(div().w(px(130.)).child(Slider::new(&self.volume)))
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
