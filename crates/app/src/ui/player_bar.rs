//! Bottom transport bar: track info, play/pause/next/prev, seek, volume.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, ElementId, Entity, EventEmitter, IntoElement, Render,
    SharedString, Window, div, ease_out_quint, hsla, img, linear_color_stop, linear_gradient,
    prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::popover::Popover;
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::config::{ReplayGainMode, ThemePref};
use crate::services::{runtime, waveform};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;

/// Widest the now-playing and volume columns flanking the transport are drawn.
const SIDE_WIDTH: f32 = 348.;
/// Narrowest they shrink to: the cover, its gap, and enough room for a title
/// to scroll through.
const SIDE_MIN: f32 = 170.;
/// Room the transport needs between them (buttons plus a usable seek bar).
const TRANSPORT_MIN: f32 = 300.;
/// The bar's own horizontal padding (`px_4`) and the gaps either side of the
/// transport (`gap_4`).
const BAR_CHROME: f32 = 32. + 32.;

/// Width of the columns flanking the transport, at a given window width.
///
/// Both shrink together: a fixed width pushes the volume block off the edge of
/// a narrow window, and shrinking only one puts the transport off centre, which
/// is the reason the volume column is width-matched to the now-playing block in
/// the first place.
fn side_width(window_width: f32) -> f32 {
    let free = window_width - TRANSPORT_MIN - BAR_CHROME;
    (free / 2.).clamp(SIDE_MIN, SIDE_WIDTH)
}

/// Bubbled to RootView.
pub enum PlayerBarEvent {
    ToggleQueue,
    ToggleFullscreen,
    OpenAlbum(String),
    OpenArtist(String),
}

impl EventEmitter<PlayerBarEvent> for PlayerBar {}

pub struct PlayerBar {
    player: Entity<PlayerState>,
    session: Entity<Session>,
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
    /// Editable dB readout for the detailed volume control.
    vol_input: Entity<InputState>,
    /// True while the dB input has focus (don't overwrite the user's typing).
    vol_input_focused: bool,
    /// Waveform peaks for the track in `waveform_for` (when the waveform
    /// seek bar is enabled and the decode finished).
    waveform: Option<Vec<f32>>,
    waveform_for: Option<String>,
    /// Fraction of the seek bar under the cursor, for the hover indicator.
    seek_hover: Option<f32>,
    /// Hover styles can't underline text in gpui (text is shaped at layout,
    /// before hover state exists), so track hover ourselves and re-render.
    title_hovered: bool,
    /// Index of the artist credit currently hovered (for the underline).
    artist_hovered: Option<usize>,
}

impl PlayerBar {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        window: &mut Window,
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
        let vol_input =
            cx.new(|cx| InputState::new(window, cx).default_value(db_string(initial_volume)));

        cx.subscribe(
            &vol_input,
            |this: &mut Self, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let raw = this.vol_input.read(cx).value().to_string();
                    if let Some(v) = parse_db(&raw) {
                        this.player.update(cx, |p, cx| p.set_volume(v, cx));
                        this.session.update(cx, |s, _| {
                            s.settings.volume = v;
                            s.persist_settings();
                        });
                    }
                    cx.notify();
                }
                InputEvent::Focus => {
                    this.vol_input_focused = true;
                }
                InputEvent::Blur => {
                    this.vol_input_focused = false;
                    cx.notify();
                }
                InputEvent::Change => {}
            },
        )
        .detach();

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
                    player.seek(crate::ui::seek_position(total, fraction));
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
            vol_input,
            vol_input_focused: false,
            waveform: None,
            waveform_for: None,
            seek_hover: None,
            title_hovered: false,
            artist_hovered: None,
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
        let opts = waveform::stream_options();
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

    /// Fine volume adjustment in the dB domain (factor multiplies amplitude).
    /// +1 dB ≈ ×1.122, −1 dB ≈ ×0.891. The slider and dB input resync in
    /// render from the new volume.
    fn nudge_volume(&mut self, factor: f32, _window: &mut Window, cx: &mut Context<Self>) {
        let cur = self.player.read(cx).volume;
        let v = if factor > 1.0 {
            (cur.max(0.001) * factor).min(1.0)
        } else {
            (cur * factor).max(0.0)
        };
        self.player.update(cx, |p, cx| p.set_volume(v, cx));
        self.session.update(cx, |s, _| {
            s.settings.volume = v;
            s.persist_settings();
        });
        cx.notify();
    }
}

/// Amplitude [0,1] as a dB number string for the input (0 → "-inf").
fn db_string(v: f32) -> String {
    if v <= 0.0001 {
        "-inf".into()
    } else {
        format!("{:.1}", 20.0 * v.log10())
    }
}

/// Parse a dB number the user typed into an amplitude [0,1]. Returns None for
/// unparseable text (so the current value is kept).
fn parse_db(raw: &str) -> Option<f32> {
    let t = raw
        .trim()
        .trim_end_matches("dB")
        .trim()
        .trim_end_matches("dB")
        .trim();
    if t.eq_ignore_ascii_case("-inf") || t.is_empty() {
        return Some(0.0);
    }
    let db = t.parse::<f32>().ok()?;
    Some(10f32.powf(db / 20.0).clamp(0.0, 1.0))
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
            track_id,
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
                p.current_song().map(|s| s.id.clone()),
            )
        };

        // Navigation targets for the track-info text; live radio has none.
        // `artists` is the per-credit list (id + name), each individually
        // clickable — falling back to the single artist/artistId pair.
        let (album_id, artists) = {
            let p = self.player.read(cx);
            if p.is_radio() {
                (None, Vec::new())
            } else if let Some(s) = p.current_song() {
                let artists: Vec<(String, Option<String>)> = if !s.artists.is_empty() {
                    s.artists
                        .iter()
                        .map(|a| (a.name.clone(), Some(a.id.clone())))
                        .collect()
                } else if let Some(name) = s.artist.clone() {
                    vec![(name, s.artist_id.clone())]
                } else {
                    Vec::new()
                };
                (s.album_id.clone(), artists)
            } else {
                (None, Vec::new())
            }
        };
        let stream_info = if is_radio {
            crate::ui::radio_info_line(self.player.read(cx), &self.session.read(cx).settings)
        } else {
            crate::ui::stream_info_line(self.player.read(cx), &self.session.read(cx).settings)
        };
        let volume = self.player.read(cx).volume;
        let detailed_volume = self.session.read(cx).settings.detailed_volume;
        let show_queue_button = self.session.read(cx).settings.show_queue_button;
        // ReplayGain badge: only shown while normalization is actually applied.
        let replay_gain = self.player.read(cx).replay_gain_active();
        // Output device shown under the volume slider (same setting).
        let device_line = self
            .session
            .read(cx)
            .settings
            .stream_info_bar
            .then(|| self.player.read(cx).output_device.clone())
            .flatten()
            .filter(|d| !d.trim().is_empty());
        let reduced_motion = self.session.read(cx).settings.reduced_motion;

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
        // Keep the volume slider in sync (also reflects media-key / dB-input
        // changes), and refresh the dB input unless the user is editing it.
        self.volume
            .update(cx, |s, cx| s.set_value(volume, window, cx));
        if !self.vol_input_focused {
            self.vol_input
                .update(cx, |s, cx| s.set_value(db_string(volume), window, cx));
        }

        let waveform_enabled = self.session.read(cx).settings.waveform_seekbar;

        let time_now = format_duration(position);
        let time_total = duration
            .map(format_duration)
            .unwrap_or_else(|| "-:--".into());

        // Width of the now-playing block (cover + text) and, so the transport
        // in between lands in the middle of the bar rather than 50px right of
        // it, of the volume block facing it. Long titles are handled by
        // scrolling them, not by widening the column.
        let side_width = side_width(f32::from(window.viewport_size().width));

        // Small, quiet transport icon buttons; primary circular play.
        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .small()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        // Adaptive theme: tint the bar with the cover-derived accent fading to
        // black; other themes keep the flat sidebar colour. The tint is shared
        // with the fullscreen overlay's gradient (see `ui::player_tint`).
        let theme_pref = self.session.read(cx).settings.theme;
        let is_adaptive = theme_pref == ThemePref::Adaptive;
        let sidebar = cx.theme().sidebar;
        let accent_bg = crate::ui::player_tint(theme_pref, cx);

        h_flex()
            .w_full()
            .h(px(124.))
            .flex_none()
            .px_4()
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(hsla(0., 0., 0.5, 0.15))
            .map(|this| {
                if is_adaptive {
                    this.bg(linear_gradient(
                        90.,
                        linear_color_stop(accent_bg, 0.),
                        linear_color_stop(hsla(0., 0., 0., 1.), 1.),
                    ))
                } else {
                    this.bg(sidebar)
                }
            })
            // Cover opens the fullscreen player; title/artist navigate to the
            // album/artist pages.
            .child(
                h_flex()
                    .w(px(side_width))
                    .flex_none()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .id("np-cover")
                            .group("np-cover")
                            .relative()
                            .size(px(76.))
                            .flex_none()
                            .rounded_md()
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .shadow_sm()
                            // Art only with a track: the two branches are
                            // mutually exclusive, and drawing both stacks the
                            // placeholder on top of whatever art was last set.
                            .when_some(art_path.filter(|_| has_track && !is_radio), |this, path| {
                                // Keyed by the art, not by the track: the id is
                                // what makes gpui drop the animation state and
                                // replay the fade, and consecutive tracks in one
                                // album share a cover path (`PlayerState` keeps
                                // it when the album-scoped art key is unchanged).
                                // Keyed by track, an unchanged image re-faded on
                                // every track change and the cover visibly blinked.
                                let anim_id: SharedString =
                                    format!("np-cover-art-{}", path.display()).into();
                                this.child(
                                    img(path).size(px(76.)).rounded_md().with_animation(
                                        ElementId::Name(anim_id),
                                        Animation::new(crate::ui::transition(reduced_motion, 120))
                                            .with_easing(ease_out_quint()),
                                        |this, t| this.opacity(t),
                                    ),
                                )
                            })
                            // Placeholder icon while no artwork. A station has
                            // none by definition, so it gets its own mark
                            // rather than an empty slot.
                            .when(!has_track || is_radio, |this| {
                                this.flex().items_center().justify_center().map(|this| {
                                    if is_radio {
                                        this.text_color(cx.theme().primary)
                                            .child(app_icon(icons::RADIO).with_size(px(30.)))
                                    } else {
                                        this.text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::MUSIC))
                                    }
                                })
                            })
                            // Clickable either way: the fullscreen view is
                            // worth opening with nothing playing (it is where
                            // the visualizer lives), and refusing to open it
                            // once the queue runs out is just a dead end.
                            .map(|this| {
                                this.cursor_pointer()
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(PlayerBarEvent::ToggleFullscreen);
                                    }))
                                    // Expand hint over the artwork on hover.
                                    .child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(gpui::hsla(0., 0., 0., 0.45))
                                            .text_color(gpui::white())
                                            .invisible()
                                            .group_hover("np-cover", |s| s.visible())
                                            .child(Icon::new(IconName::Maximize)),
                                    )
                            }),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            .min_w_0()
                            .flex_1()
                            .child({
                                // Titles longer than the column scroll rather
                                // than get cut off — radio track titles run
                                // long, and a clipped one is unreadable.
                                let title_width = px(side_width - 76. - 12.);
                                let base = div().child(crate::ui::scrolling_line(
                                    "np-title-text",
                                    title.unwrap_or_else(|| "Not playing".into()).into(),
                                    title_width,
                                    window.rem_size() * 0.875,
                                    gpui::FontWeight::MEDIUM,
                                    (!has_track).then(|| cx.theme().muted_foreground),
                                    window,
                                ));
                                match album_id {
                                    Some(id) => base
                                        .id("np-title")
                                        .cursor_pointer()
                                        .when(self.title_hovered, |s| s.underline())
                                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                            if this.title_hovered != *hovered {
                                                this.title_hovered = *hovered;
                                                cx.notify();
                                            }
                                        }))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(PlayerBarEvent::OpenAlbum(id.clone()));
                                        }))
                                        .into_any_element(),
                                    None => base.into_any_element(),
                                }
                            })
                            .child({
                                // Radio (or no credits): plain subtitle text.
                                if is_radio || artists.is_empty() {
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(artist.unwrap_or_default())
                                        .into_any_element()
                                } else {
                                    // One clickable span per artist, comma-joined.
                                    let mut row = h_flex()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground);
                                    let last = artists.len() - 1;
                                    for (i, (name, id)) in artists.into_iter().enumerate() {
                                        let span = match id {
                                            Some(id) => div()
                                                .id(("np-artist", i))
                                                .flex_none()
                                                .cursor_pointer()
                                                .when(self.artist_hovered == Some(i), |s| {
                                                    s.underline()
                                                })
                                                .on_hover(cx.listener(
                                                    move |this, hovered: &bool, _, cx| {
                                                        let now = hovered.then_some(i);
                                                        if this.artist_hovered != now
                                                            && (this.artist_hovered == Some(i)
                                                                || *hovered)
                                                        {
                                                            this.artist_hovered = now;
                                                            cx.notify();
                                                        }
                                                    },
                                                ))
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.emit(PlayerBarEvent::OpenArtist(id.clone()));
                                                }))
                                                .child(name)
                                                .into_any_element(),
                                            None => div().child(name).into_any_element(),
                                        };
                                        row = row.child(span);
                                        if i != last {
                                            row = row.child(div().flex_none().child(", "));
                                        }
                                    }
                                    row.into_any_element()
                                }
                            })
                            .when_some(stream_info, |this, info| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground.opacity(0.8))
                                        .child(info),
                                )
                            })
                            .when_some(error, |this, e| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().danger)
                                        .truncate()
                                        .child(e),
                                )
                            })
                            .with_animation(
                                ElementId::Name(
                                    format!("np-info-{}", track_id.as_deref().unwrap_or("none"))
                                        .into(),
                                ),
                                Animation::new(crate::ui::transition(reduced_motion, 120))
                                    .with_easing(ease_out_quint()),
                                |this, t| this.opacity(t),
                            ),
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
                            // Queue controls mean nothing on a live stream:
                            // there is no queue, and nothing to skip to.
                            .child(
                                icon_btn("shuffle", icons::SHUFFLE, shuffle && !is_radio)
                                    .disabled(is_radio)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.player.update(cx, |p, cx| p.toggle_shuffle(cx));
                                    })),
                            )
                            .child(
                                icon_btn("prev", icons::SKIP_BACK, false)
                                    .disabled(!has_track || is_radio)
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
                                    .disabled(!has_track || is_radio)
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
                                    repeat != RepeatMode::Off && !is_radio,
                                )
                                .disabled(is_radio)
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
                            // Live radio has no timeline: the on-air badge and
                            // how long the stream has been running take the
                            // seek bar's place, centred where it would be.
                            .when(is_radio, |this| {
                                this.justify_center().child(crate::ui::live_badge(
                                    "bar-live",
                                    cx.theme().primary,
                                    Some(position),
                                    cx,
                                ))
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
                                    let bar = match (waveform_enabled, self.waveform.clone()) {
                                        (true, Some(peaks)) => crate::ui::waveform_seek_bar(
                                            &peaks,
                                            seek_fraction,
                                            26.,
                                            cx.theme().primary,
                                            cx.theme().muted_foreground.opacity(0.35),
                                            self.player.clone(),
                                        ),
                                        _ => div()
                                            .flex_1()
                                            .child(Slider::new(&self.seek))
                                            .into_any_element(),
                                    };
                                    let view = cx.entity();
                                    this.child(crate::ui::seek_hover_wrap(
                                        "bar-seek-hover",
                                        self.seek_hover,
                                        duration,
                                        bar,
                                        move |fraction, cx| {
                                            view.update(cx, |bar, cx| {
                                                if bar.seek_hover != fraction {
                                                    bar.seek_hover = fraction;
                                                    cx.notify();
                                                }
                                            });
                                        },
                                        cx,
                                    ))
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
            // Volume + queue toggle (+ optional output-device line).
            // The three rows share a common left edge; the slider stretches to
            // the right so the block stays anchored to the bar's right side.
            .child(
                h_flex().w(px(side_width)).flex_none().justify_end().child(
                    v_flex()
                        // Never wider than the column it sits in, which shrinks
                        // with the window.
                        .w(px(240_f32.min(side_width)))
                        .gap_1p5()
                        // ReplayGain status chip: highlighted while playing; click
                        // opens a menu to change or disable the mode.
                        .when_some(replay_gain, |this, (label, db)| {
                            let text = match db {
                                Some(db) => format!("ReplayGain {db:+.1} dB · {label}"),
                                None => format!("ReplayGain · {label} (no tag)"),
                            };
                            let current = self.session.read(cx).settings.replay_gain;
                            let player = self.player.clone();
                            let session = self.session.clone();
                            this.child(
                                h_flex().w_full().child(
                                    Popover::new("replaygain-menu")
                                        .trigger(
                                            // Same quiet style as the device picker.
                                            Button::new("rg-trigger")
                                                .ghost()
                                                .xsmall()
                                                .text_color(cx.theme().muted_foreground)
                                                .icon(Icon::new(IconName::ChevronDown).xsmall())
                                                .label(text),
                                        )
                                        .content(move |_state, _window, cx| {
                                            let opts = [
                                                ("Off", ReplayGainMode::Off),
                                                ("Track", ReplayGainMode::Track),
                                                ("Album", ReplayGainMode::Album),
                                                ("Auto", ReplayGainMode::Auto),
                                            ];
                                            let mut menu = v_flex().gap_0p5().min_w(px(140.));
                                            for (i, (lbl, mode)) in opts.into_iter().enumerate() {
                                                let player = player.clone();
                                                let session = session.clone();
                                                menu = menu.child(
                                                    div()
                                                        .id(("rg-opt", i))
                                                        .px_2()
                                                        .py_1()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .hover(|s| s.bg(cx.theme().muted))
                                                        .when(current == mode, |s| {
                                                            s.text_color(cx.theme().primary)
                                                        })
                                                        .on_click(cx.listener(
                                                            move |state, _, window, cx| {
                                                                player.update(cx, |p, cx| {
                                                                    p.set_replay_gain(mode, cx)
                                                                });
                                                                session.update(cx, |s, _| {
                                                                    s.settings.replay_gain = mode;
                                                                    s.persist_settings();
                                                                });
                                                                state.dismiss(window, cx);
                                                            },
                                                        ))
                                                        .child(lbl),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                            )
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .gap_5()
                                .items_center()
                                // Volume group fills the row; slider stretches.
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(app_icon(icons::VOLUME_HIGH)),
                                        )
                                        .map(|this| {
                                            if detailed_volume {
                                                // [−] [editable dB] [+] — no slider.
                                                this.child(
                                                    Button::new("vol-down")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("−")
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.nudge_volume(
                                                                    0.891_25, window, cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .w(px(56.))
                                                        .child(Input::new(&self.vol_input).small()),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child("dB"),
                                                )
                                                .child(
                                                    Button::new("vol-up")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("+")
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.nudge_volume(
                                                                    1.122_02, window, cx,
                                                                )
                                                            },
                                                        )),
                                                )
                                                // Spacer keeps the queue button at the edge.
                                                .child(div().flex_1())
                                            } else {
                                                this.child(
                                                    div().flex_1().child(Slider::new(&self.volume)),
                                                )
                                            }
                                        }),
                                )
                                .when(show_queue_button, |this| {
                                    this.child(
                                        Button::new("queue-toggle")
                                            .ghost()
                                            .xsmall()
                                            .icon(Icon::new(IconName::PanelRight))
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(PlayerBarEvent::ToggleQueue);
                                            })),
                                    )
                                }),
                        )
                        .when_some(device_line, |this, device| {
                            let selected = self.session.read(cx).settings.output_device.clone();
                            let player = self.player.clone();
                            let session = self.session.clone();
                            this.child(
                                // Click the device name to switch outputs.
                                h_flex().w_full().child(
                                    Popover::new("output-device")
                                        .trigger(
                                            Button::new("output-device-trigger")
                                                .ghost()
                                                .xsmall()
                                                .text_color(cx.theme().muted_foreground)
                                                .icon(Icon::new(IconName::ChevronDown).xsmall())
                                                .label(device),
                                        )
                                        .content(move |_state, _window, cx| {
                                            let opts: Vec<(String, Option<String>)> =
                                                std::iter::once((
                                                    "System default".to_string(),
                                                    None,
                                                ))
                                                .chain(
                                                    playback::output_devices()
                                                        .into_iter()
                                                        .map(|d| (d.clone(), Some(d))),
                                                )
                                                .collect();
                                            let mut menu = v_flex()
                                                .id("output-device-menu")
                                                .gap_0p5()
                                                .min_w(px(220.))
                                                .max_h(px(280.))
                                                .overflow_y_scroll();
                                            for (i, (label, value)) in opts.into_iter().enumerate()
                                            {
                                                let is_sel =
                                                    selected.as_deref() == value.as_deref();
                                                let player = player.clone();
                                                let session = session.clone();
                                                menu = menu.child(
                                                    div()
                                                        .id(("dev-opt", i))
                                                        .px_2()
                                                        .py_1()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .hover(|s| s.bg(cx.theme().muted))
                                                        .when(is_sel, |s| {
                                                            s.text_color(cx.theme().primary)
                                                        })
                                                        .on_click(cx.listener(
                                                            move |state, _, window, cx| {
                                                                let v = value.clone();
                                                                player.update(cx, |p, cx| {
                                                                    p.set_output_device(
                                                                        v.clone(),
                                                                        cx,
                                                                    )
                                                                });
                                                                session.update(cx, |s, _| {
                                                                    s.settings.output_device =
                                                                        v.clone();
                                                                    s.persist_settings();
                                                                });
                                                                state.dismiss(window, cx);
                                                            },
                                                        ))
                                                        .child(label),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                            )
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{SIDE_MIN, SIDE_WIDTH, side_width};

    #[test]
    fn wide_windows_keep_the_full_side_columns() {
        assert_eq!(side_width(1400.), SIDE_WIDTH);
        assert_eq!(side_width(1060.), SIDE_WIDTH);
    }

    #[test]
    fn narrow_windows_shrink_both_columns_together() {
        // Both sides shrink, so the transport keeps its room and stays centred.
        for w in [1000., 900., 800., 700., 600.] {
            let side = side_width(w);
            assert!((SIDE_MIN..=SIDE_WIDTH).contains(&side));
            assert!(2. * side + 300. + 64. <= w + 0.5 || side == SIDE_MIN, "{w}");
        }
    }

    #[test]
    fn the_columns_never_shrink_past_the_cover_and_its_title() {
        assert_eq!(side_width(200.), SIDE_MIN);
    }
}
