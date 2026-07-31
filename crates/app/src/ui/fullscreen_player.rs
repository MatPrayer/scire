//! Full-window now-playing overlay with dynamic blurred-art background.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, Context, Entity, EventEmitter, IntoElement, Render, Window, div,
    img, linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::popover::Popover;
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::SubsonicClient;

use crate::assets::{app_icon, icons};
use crate::config::{FullscreenBackground, VisualizerMode, VisualizerSettings};
use crate::services::{artwork, runtime};
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::format_duration;
use crate::ui::visualizer::Visualizer;

const ART_SIZE: u32 = 600;

/// Text width inside the info card: its 480px minus the 24px padding either
/// side. Needed as a number because the marquee measures against it.
const CARD_TEXT_WIDTH: f32 = 432.;
/// Tiny fetch for color extraction — low-res average is a fast palette sample.
const BG_ART_SIZE: u32 = 32;

/// Entrance duration. Long enough for the zoom to read, short enough that the
/// overlay never feels like it is loading.
const ENTER: Duration = Duration::from_millis(340);
/// Exit is deliberately faster than the entrance — dismissals should feel
/// immediate. `begin_close` waits this out (plus a frame) before unmounting.
const EXIT: Duration = Duration::from_millis(220);

/// Decelerating curve — fast start, soft landing.
fn out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Decelerating curve that overshoots 1.0 slightly before settling. Only used
/// for geometry (never for opacity, and never as an `Animation` easing — gpui
/// asserts easings stay inside 0..=1).
fn out_back(t: f32) -> f32 {
    const C1: f32 = 1.7;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

/// One tunable of the visualizer. Everything the tuning card needs to render
/// and write a knob lives here, so adding one is a single entry in
/// [`VizKnob::ALL`] rather than a new field, slider and setter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VizKnob {
    Sensitivity,
    Smoothing,
    Intensity,
    Motion,
    SwitchSensitivity,
    SwitchHold,
}

impl VizKnob {
    const ALL: [VizKnob; 6] = [
        VizKnob::Sensitivity,
        VizKnob::Smoothing,
        VizKnob::Intensity,
        VizKnob::Motion,
        VizKnob::SwitchSensitivity,
        VizKnob::SwitchHold,
    ];

    fn id(self) -> &'static str {
        match self {
            VizKnob::Sensitivity => "viz-sensitivity",
            VizKnob::Smoothing => "viz-smoothing",
            VizKnob::Intensity => "viz-intensity",
            VizKnob::Motion => "viz-motion",
            VizKnob::SwitchSensitivity => "viz-switch-sensitivity",
            VizKnob::SwitchHold => "viz-switch-hold",
        }
    }

    fn label(self) -> &'static str {
        match self {
            VizKnob::Sensitivity => "Sensitivity",
            VizKnob::Smoothing => "Smoothing",
            VizKnob::Intensity => "Reaction depth",
            VizKnob::Motion => "Motion speed",
            VizKnob::SwitchSensitivity => "Auto switch",
            VizKnob::SwitchHold => "Scene hold",
        }
    }

    /// Only shown for the two Auto-mode knobs, which do nothing on a pinned
    /// scene and would otherwise look broken.
    fn auto_only(self) -> bool {
        matches!(self, VizKnob::SwitchSensitivity | VizKnob::SwitchHold)
    }

    /// (min, max, step) for the slider.
    fn range(self) -> (f32, f32, f32) {
        match self {
            VizKnob::Sensitivity => (0.4, 3.0, 0.05),
            VizKnob::Smoothing => (0.0, 1.0, 0.02),
            VizKnob::Intensity => (0.3, 2.5, 0.05),
            VizKnob::Motion => (0.2, 2.5, 0.05),
            VizKnob::SwitchSensitivity => (0.4, 2.5, 0.05),
            VizKnob::SwitchHold => (3.0, 40.0, 1.0),
        }
    }

    fn get(self, s: &VisualizerSettings) -> f32 {
        match self {
            VizKnob::Sensitivity => s.sensitivity,
            VizKnob::Smoothing => s.smoothing,
            VizKnob::Intensity => s.intensity,
            VizKnob::Motion => s.motion,
            VizKnob::SwitchSensitivity => s.switch_sensitivity,
            VizKnob::SwitchHold => s.switch_hold,
        }
    }

    fn set(self, s: &mut VisualizerSettings, v: f32) {
        let (min, max, _) = self.range();
        let v = v.clamp(min, max);
        match self {
            VizKnob::Sensitivity => s.sensitivity = v,
            VizKnob::Smoothing => s.smoothing = v,
            VizKnob::Intensity => s.intensity = v,
            VizKnob::Motion => s.motion = v,
            VizKnob::SwitchSensitivity => s.switch_sensitivity = v,
            VizKnob::SwitchHold => s.switch_hold = v,
        }
    }

    fn format(self, v: f32) -> String {
        match self {
            VizKnob::SwitchHold => format!("{v:.0}s"),
            _ => format!("{v:.2}×"),
        }
    }
}

/// Remap `t` onto the sub-range `start..end` of the timeline, clamped.
fn phase(t: f32, start: f32, end: f32) -> f32 {
    ((t - start) / (end - start)).clamp(0.0, 1.0)
}

/// Name of the scene a mode pins, or `None` for the two that pin nothing.
fn pinned_scene_label(mode: VisualizerMode) -> Option<&'static str> {
    VisualizerMode::SCENES
        .contains(&mode)
        .then(|| mode.menu_label())
}

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
    gradient_palette: Option<Vec<gpui::Rgba>>,
    /// Album-scoped art key the loaded art belongs to (see `artwork::song_cover`).
    last_art_key: Option<String>,
    panel: Option<SidePanel>,
    /// Lyrics text for the song in `lyrics_for`; None while loading or when
    /// the server has none.
    lyrics: Option<String>,
    lyrics_for: Option<String>,
    lyrics_loading: bool,
    /// Waveform peaks for the track in `waveform_for` (when the waveform
    /// seek bar is enabled and the decode finished).
    waveform: Option<Vec<f32>>,
    waveform_for: Option<String>,
    /// Fraction of the seek bar under the cursor, for the hover indicator.
    seek_hover: Option<f32>,
    /// Spectrum analysis + scene state for the 3D visualizer. Ticked once per
    /// frame while a scene is running; idle (and unread) when it is off.
    visualizer: Visualizer,
    /// One slider state per visualizer knob, in `VizKnob::ALL` order.
    viz_knobs: Vec<Entity<SliderState>>,
    /// Tuning card open over the mini player.
    viz_tuning_open: bool,
    /// Start of the entrance, reset on every open.
    opened_at: Instant,
    /// Set when the exit starts; `Some` also means "closing", i.e. the overlay
    /// is playing its exit and will unmount when the timer in `begin_close`
    /// fires. Both transitions are driven from these instants rather than
    /// `with_animation`: an animation's state is keyed by its ancestor
    /// element-id path, so swapping an enter/exit wrapper around the whole
    /// overlay would reset every nested animation (content stagger, and the
    /// 40s background sweep, which would visibly jump).
    closing_at: Option<Instant>,
}

impl FullscreenPlayer {
    pub fn new(
        player: Entity<PlayerState>,
        session: Entity<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_volume = player.read(cx).volume;
        let visualizer = Visualizer::new(player.read(cx).spectrum_tap());
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
                    p.seek(crate::ui::seek_position(total, fraction));
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

        // Visualizer knobs. Each writes straight into the session settings,
        // which `tick` re-reads every frame — so a drag retunes the scene as
        // it happens rather than on the next open.
        let tuning = session.read(cx).settings.visualizer_tuning;
        let viz_knobs: Vec<Entity<SliderState>> = VizKnob::ALL
            .into_iter()
            .map(|knob| {
                let (min, max, step) = knob.range();
                let state = cx.new(|_| {
                    SliderState::new()
                        .min(min)
                        .max(max)
                        .step(step)
                        .default_value(knob.get(&tuning))
                });
                cx.subscribe(&state, move |this: &mut Self, _, event, cx| {
                    let SliderEvent::Change(value) = event;
                    let v = value.start();
                    this.session.update(cx, |s, _| {
                        knob.set(&mut s.settings.visualizer_tuning, v);
                        s.persist_settings();
                    });
                    cx.notify();
                })
                .detach();
                state
            })
            .collect();

        // Watch player for song changes to update background art and lyrics.
        // Keyed on the album-scoped art key, not the song's cover id: Navidrome
        // gives each track its own, so comparing those would reload identical
        // art (and re-extract the gradient palette) on every track change.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let cover = player.read(cx).current_song().and_then(artwork::song_cover);
            let key = cover.as_ref().map(|(_, key)| key.clone());
            if key != this.last_art_key {
                this.last_art_key = key;
                this.art_path = None;
                this.bg_art_path = None;
                this.gradient_palette = None;
                if let Some((cover_id, key)) = cover {
                    this.fetch_art(cover_id, key, cx);
                }
            }
            this.maybe_fetch_lyrics(cx);
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

        Self {
            player,
            session,
            seek,
            volume,
            art_path: None,
            bg_art_path: None,
            gradient_palette: None,
            last_art_key: None,
            panel: None,
            lyrics: None,
            lyrics_for: None,
            lyrics_loading: false,
            waveform: None,
            waveform_for: None,
            seek_hover: None,
            visualizer,
            viz_knobs,
            viz_tuning_open: false,
            opened_at: Instant::now(),
            closing_at: None,
        }
    }

    /// Start the exit animation, then emit Close so the overlay unmounts.
    pub fn begin_close(&mut self, cx: &mut Context<Self>) {
        if self.closing_at.is_some() {
            return;
        }
        self.closing_at = Some(Instant::now());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(EXIT + Duration::from_millis(16))
                .await;
            let _ = this.update(cx, |this, cx| {
                // Skip if a quick reopen already cancelled the close.
                if this.closing_at.is_some() {
                    this.closing_at = None;
                    cx.emit(FullscreenEvent::Close);
                }
            });
        })
        .detach();
    }

    /// Reset state so a fresh open plays the entrance (not a stale exit).
    pub fn reset_for_open(&mut self, cx: &mut Context<Self>) {
        self.closing_at = None;
        self.opened_at = Instant::now();
        // A track is usually already loaded when the overlay opens. The art
        // observer only fires on song *changes*, and a paused player never
        // notifies at all, so without this the cover stays blank until the
        // next track starts.
        if self.art_path.is_none()
            && let Some((cover_id, key)) = self
                .player
                .read(cx)
                .current_song()
                .and_then(artwork::song_cover)
        {
            self.last_art_key = Some(key.clone());
            self.fetch_art(cover_id, key, cx);
        }
        cx.notify();
    }

    /// Kick off a waveform decode when the current track changed (and the
    /// setting is on). Same flow as the player bar; the on-disk peak cache
    /// makes the second consumer effectively free.
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
        let opts = crate::services::waveform::stream_options();
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
            let result =
                runtime::spawn_io(crate::services::waveform::fetch_peaks(url, id.clone())).await;
            let _ = this.update(cx, |view, cx| {
                // Ignore results for a track that is no longer current.
                if view.waveform_for.as_deref() == Some(id.as_str()) {
                    match result {
                        Ok(peaks) => view.waveform = Some(peaks),
                        Err(e) => tracing::warn!("waveform peaks failed: {e:#}"),
                    }
                    cx.notify();
                }
            });
        })
        .detach();
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

    /// Advance the visualizer to the next scene (and off again), persisting the
    /// choice so the overlay reopens on the same one.
    fn cycle_visualizer(&mut self, cx: &mut Context<Self>) {
        let next = self.session.read(cx).settings.visualizer.next();
        self.set_visualizer(next, cx);
    }

    /// Jump straight to a mode — what the mini player's scene picker uses, so
    /// changing scene is one click rather than a walk around the cycle.
    fn set_visualizer(&mut self, mode: VisualizerMode, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.visualizer = mode);
        self.session.read(cx).persist_settings();
        cx.notify();
    }

    /// Put every visualizer knob back to its default, sliders included — a
    /// slider owns its own value, so resetting the settings alone would leave
    /// the handles where the user dragged them.
    fn reset_viz_tuning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults = VisualizerSettings::default();
        self.session.update(cx, |s, _| {
            s.settings.visualizer_tuning = defaults;
            s.persist_settings();
        });
        for (knob, state) in VizKnob::ALL.into_iter().zip(self.viz_knobs.clone()) {
            state.update(cx, |slider, cx| {
                slider.set_value(knob.get(&defaults), window, cx);
            });
        }
        cx.notify();
    }

    /// The tuning card: the visualizer's own settings, floating over the scene
    /// so a knob can be dragged while watching what it does. Lives here rather
    /// than on the settings page because every one of these is judged by eye.
    fn viz_tuning_card(&self, mode: VisualizerMode, cx: &mut Context<Self>) -> impl IntoElement {
        let tuning = self.session.read(cx).settings.visualizer_tuning;
        let auto = mode == VisualizerMode::Auto;

        let mut rows = v_flex().gap_2p5();
        for (knob, state) in VizKnob::ALL.into_iter().zip(self.viz_knobs.iter()) {
            // Auto-only knobs stay visible on a pinned scene (so the card does
            // not reflow when the mode changes) but read as inactive.
            let dim = knob.auto_only() && !auto;
            rows = rows.child(
                v_flex()
                    .gap_0p5()
                    .when(dim, |s| s.opacity(0.45))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().child(knob.label()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(knob.format(knob.get(&tuning))),
                            ),
                    )
                    .child(
                        h_flex()
                            .id(knob.id())
                            .w_full()
                            .child(Slider::new(state).disabled(dim)),
                    ),
            );
        }

        v_flex()
            .id("viz-tuning-card")
            .w(px(260.))
            .px_5()
            .py_4()
            .gap_2()
            .rounded_2xl()
            .shadow_xl()
            // Same translucent shell as the mini player: the two read as one
            // control surface, and the scene keeps running behind both.
            .bg(cx.theme().background.opacity(0.72))
            .border_1()
            .border_color(cx.theme().border.opacity(0.6))
            // The card is a control surface floating on the scene: clicks in it
            // must not reach the overlay behind (which closes panels).
            .occlude()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().font_medium().child("Visualizer"))
                    .child(
                        Button::new("viz-tuning-reset")
                            .ghost()
                            .xsmall()
                            .label("Reset")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_viz_tuning(window, cx);
                            })),
                    ),
            )
            .child(rows)
    }

    /// Menu listing scenes, for the mini player's "More" button. The scenes
    /// outgrew a row of buttons — five of them crowded the card and made the
    /// two that are not scenes at all (Off, Auto) harder to pick out.
    fn scene_menu(&self, trigger: Button, current: VisualizerMode) -> impl IntoElement {
        let session = self.session.clone();
        Popover::new("viz-scene-menu")
            // Opens upward: the trigger sits in a card near the bottom of the
            // window, and the default downward menu covers the transport.
            .anchor(gpui::Corner::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let mut menu = v_flex().gap_0p5().min_w(px(150.));
                for (i, mode) in VisualizerMode::SCENES.into_iter().enumerate() {
                    let session = session.clone();
                    menu = menu.child(
                        div()
                            .id(("viz-scene", i))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| s.bg(cx.theme().muted))
                            .when(mode == current, |s| s.text_color(cx.theme().primary))
                            .on_click(cx.listener(move |state, _, window, cx| {
                                // Reached through the session rather than this
                                // view: inside a popover the listener's view is
                                // the popover's own state. The fullscreen
                                // player observes the session, so it re-renders
                                // from this anyway.
                                session.update(cx, |s, cx| {
                                    s.settings.visualizer = mode;
                                    cx.notify();
                                });
                                session.read(cx).persist_settings();
                                state.dismiss(window, cx);
                            }))
                            .child(mode.menu_label()),
                    );
                }
                menu
            })
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
                    .border_l_2()
                    .border_color(gpui::transparent_black())
                    .hover(|s| s.bg(cx.theme().muted.opacity(0.6)))
                    .when(is_current, |s| {
                        s.bg(cx.theme().primary.opacity(0.12))
                            .border_color(cx.theme().primary)
                            .text_color(cx.theme().primary)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.player.update(cx, |p, cx| p.jump_to(pos, cx));
                        cx.stop_propagation();
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .when(is_current, |s| s.font_medium())
                                    .child(title),
                            )
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
            // Same card as the info column and the mini player, so an open
            // panel reads as part of the player rather than as a list dropped
            // onto the backdrop.
            .max_h(px(620.))
            .p_4()
            .gap_2()
            .rounded_2xl()
            .bg(cx.theme().background.opacity(0.55))
            .border_1()
            .border_color(cx.theme().border.opacity(0.5))
            .shadow_xl()
            .child(
                div()
                    .text_sm()
                    .font_medium()
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
            // Same card as the info column and the mini player, so an open
            // panel reads as part of the player rather than as a list dropped
            // onto the backdrop.
            .max_h(px(620.))
            .p_4()
            .gap_2()
            .rounded_2xl()
            .bg(cx.theme().background.opacity(0.55))
            .border_1()
            .border_color(cx.theme().border.opacity(0.5))
            .shadow_xl()
            .child(
                div()
                    .text_sm()
                    .font_medium()
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

    /// The background layer for the chosen mode (behind the readability scrim).
    fn render_background(
        &self,
        mode: FullscreenBackground,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let base = || div().absolute().left_0().top_0().size_full();
        let palette: Vec<gpui::Rgba> = self.gradient_palette.clone().unwrap_or_else(|| {
            vec![
                gpui::Rgba::from(cx.theme().muted),
                gpui::Rgba::from(cx.theme().background),
            ]
        });
        let top = palette[0];
        // Design continuity: every colour-driven background ends on the exact
        // tint the bottom player bar fades from, so the overlay and the bar
        // read as the same surface.
        let tint: gpui::Rgba =
            crate::ui::player_tint(self.session.read(cx).settings.theme, cx).into();
        match mode {
            FullscreenBackground::Solid => base().bg(cx.theme().background).into_any_element(),
            FullscreenBackground::Gradient => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.5), 0.),
                    linear_color_stop(tint, 1.),
                ))
                .into_any_element(),
            FullscreenBackground::Vibrant => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.9), 0.),
                    linear_color_stop(tint, 1.),
                ))
                .into_any_element(),
            FullscreenBackground::BlurredArt => base()
                .bg(cx.theme().muted)
                .overflow_hidden()
                // The tiny 32px art scaled to full size reads as a soft blur.
                .when_some(self.bg_art_path.clone(), |this, path| {
                    this.child(img(path).size_full())
                })
                .into_any_element(),
            FullscreenBackground::Animated => {
                // A single 2-stop ramp between an album's (usually low-variance) colours
                // reads as a static, banded diagonal — that's the "bar". Instead stack
                // TWO translucent gradients at crossing angles: their band lines never
                // align, so there's no visible seam, and they expose several hues at once
                // for a richer field. Both phases advance monotonically through the
                // palette-as-cycle (wrap is a lerp), so flow is always one direction.
                let ring: Vec<gpui::Rgba> = {
                    // Boosted saturation + a floor of variance so mono covers still move.
                    // The player-bar tint is one of the cycle's stops, so the
                    // sweep keeps passing back through the bar's colour.
                    let mut r: Vec<gpui::Rgba> = palette
                        .iter()
                        .map(|&c| scale_rgb(vivid(c, 1.5), 1.0))
                        .collect();
                    r.push(tint);
                    if r.len() < 2 {
                        let b = *r
                            .first()
                            .unwrap_or(&gpui::Rgba::from(cx.theme().background));
                        vec![b, scale_rgb(b, 1.5), scale_rgb(b, 0.6)]
                    } else {
                        r
                    }
                };
                let ring2 = ring.clone();
                let anim_layer = |id: &'static str,
                                  angle: f32,
                                  ring: Vec<gpui::Rgba>,
                                  oa: f32,
                                  ob: f32,
                                  alpha: f32| {
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .size_full()
                        .with_animation(
                            id,
                            // Slower: full palette sweep over 40s.
                            Animation::new(Duration::from_secs(40)).repeat(),
                            move |this, delta| {
                                let n = ring.len();
                                let sample = |offset: f32| -> gpui::Rgba {
                                    let pos = (delta + offset).rem_euclid(1.0) * n as f32;
                                    let i0 = pos.floor() as usize % n;
                                    let i1 = (i0 + 1) % n;
                                    let mut c = lerp_rgb(ring[i0], ring[i1], pos.fract());
                                    c.a = alpha;
                                    c
                                };
                                this.bg(linear_gradient(
                                    angle,
                                    linear_color_stop(sample(oa), 0.),
                                    linear_color_stop(sample(ob), 1.),
                                ))
                            },
                        )
                };
                base()
                    // Solid base so the translucent layers composite over
                    // something — the player-bar tint again.
                    .bg(tint)
                    .overflow_hidden()
                    .child(anim_layer("fs-bg-a", 229., ring, 0.0, 0.5, 1.0))
                    .child(anim_layer("fs-bg-b", 63., ring2, 0.28, 0.78, 0.55))
                    .into_any_element()
            }
        }
    }

    fn fetch_art(&mut self, cover_id: String, key: String, cx: &mut Context<Self>) {
        // Local track: resolve cover from local_art_path directly.
        if self.client(cx).is_none()
            && let Some(path) =
                crate::services::local_library::local_art_path(&cover_id).filter(|p| p.exists())
        {
            self.art_path = Some(path.clone());
            self.bg_art_path = Some(path.clone());
            self.gradient_palette = extract_palette(&path);
            cx.notify();
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let client2 = client.clone();
        let cover_id2 = cover_id.clone();
        let key2 = key.clone();
        // Full-res for the center art card.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch_as(client, cover_id, key, ART_SIZE).await {
                let _ = this.update(cx, |view, cx| {
                    view.art_path = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
        // Tiny version for color extraction.
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch_as(client2, cover_id2, key2, BG_ART_SIZE).await {
                let colors = extract_palette(&path);
                let _ = this.update(cx, |view, cx| {
                    view.bg_art_path = Some(path);
                    view.gradient_palette = colors;
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// Average colour of each horizontal band, top→bottom. More bands = richer
/// palette for the animated gradient to sweep through. Empty bands are skipped.
fn extract_palette(path: &std::path::Path) -> Option<Vec<gpui::Rgba>> {
    const BANDS: usize = 5;
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgb8();
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return None;
    }
    let mut acc = [[0u64; 3]; BANDS];
    let mut n = [0u64; BANDS];
    for (_, y, pixel) in img.enumerate_pixels() {
        let band = ((y as u64 * BANDS as u64) / h as u64).min(BANDS as u64 - 1) as usize;
        acc[band][0] += pixel[0] as u64;
        acc[band][1] += pixel[1] as u64;
        acc[band][2] += pixel[2] as u64;
        n[band] += 1;
    }
    // Raw averages; the render darkens/brightens per background mode.
    let palette: Vec<gpui::Rgba> = (0..BANDS)
        .filter(|&i| n[i] > 0)
        .map(|i| gpui::Rgba {
            r: acc[i][0] as f32 / n[i] as f32 / 255.0,
            g: acc[i][1] as f32 / n[i] as f32 / 255.0,
            b: acc[i][2] as f32 / n[i] as f32 / 255.0,
            a: 1.0,
        })
        .collect();
    if palette.is_empty() {
        return None;
    }
    Some(palette)
}

/// Scale an RGB colour's brightness (keeps alpha opaque).
fn scale_rgb(c: gpui::Rgba, f: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: (c.r * f).clamp(0., 1.),
        g: (c.g * f).clamp(0., 1.),
        b: (c.b * f).clamp(0., 1.),
        a: 1.0,
    }
}

/// Push a colour's channels away from their mean to boost saturation.
fn vivid(c: gpui::Rgba, amt: f32) -> gpui::Rgba {
    let mean = (c.r + c.g + c.b) / 3.0;
    gpui::Rgba {
        r: (mean + (c.r - mean) * amt).clamp(0., 1.),
        g: (mean + (c.g - mean) * amt).clamp(0., 1.),
        b: (mean + (c.b - mean) * amt).clamp(0., 1.),
        a: 1.0,
    }
}

/// Linear blend between two RGB colours.
fn lerp_rgb(a: gpui::Rgba, b: gpui::Rgba, t: f32) -> gpui::Rgba {
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
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

        let seek_fraction = match duration {
            Some(total) if total > Duration::ZERO => {
                (position.as_secs_f32() / total.as_secs_f32()).clamp(0., 1.)
            }
            _ => 0.,
        };
        let waveform_enabled = self.session.read(cx).settings.waveform_seekbar;
        let show_volume = self.session.read(cx).settings.fullscreen_volume;
        let detailed_volume = self.session.read(cx).settings.detailed_volume;
        let volume_level = self.player.read(cx).volume;
        let replay_gain = self.player.read(cx).replay_gain_active();
        let stream_info = if is_radio {
            crate::ui::radio_info_line(self.player.read(cx), &self.session.read(cx).settings)
        } else {
            crate::ui::stream_info_line(self.player.read(cx), &self.session.read(cx).settings)
        };

        let time_now = format_duration(position);
        let time_total = duration
            .map(format_duration)
            .unwrap_or_else(|| "-:--".into());

        let bg_mode = self.session.read(cx).settings.fullscreen_bg;
        // Mean perceptual luminance of the current palette (0 = black, 1 = white).
        // Bright covers wash out the grey text, so scale the dark scrim up with it.
        let luma = self
            .gradient_palette
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|p| {
                p.iter()
                    .map(|c| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b)
                    .sum::<f32>()
                    / p.len() as f32
            })
            .unwrap_or(0.0);
        // Dark scrim opacity for readability, tuned per background mode. The color-
        // dependent modes add a luminance term so bright palettes stay legible.
        let scrim = match bg_mode {
            FullscreenBackground::Solid => 0.0,
            FullscreenBackground::Gradient => 0.4 + 0.3 * luma,
            FullscreenBackground::Vibrant => 0.18 + 0.4 * luma,
            FullscreenBackground::BlurredArt => 0.55,
            FullscreenBackground::Animated => 0.3 + 0.4 * luma,
        };
        // The visualizer draws light geometry on the theme background, so the
        // per-mode scrim (tuned for album-art backdrops) would only mute it.
        // Keep a thin one — enough to seat the text over moving geometry.
        let viz_mode = self.session.read(cx).settings.visualizer;
        let scrim = if viz_mode.is_on() { 0.15 } else { scrim };
        let (mini_title, mini_artist) = (title.clone(), artist.clone());
        let (mini_time_now, mini_time_total) = (time_now.clone(), time_total.clone());
        let panel_open = self.panel.is_some();
        let art_size = if panel_open { 360. } else { 460. };

        // --- Open/close transition ---------------------------------------
        // One clock, read straight from state, so the element tree keeps the
        // same shape (and the same ids) in both directions. Ask for another
        // frame until it lands; gpui only redraws on demand.
        let closing = self.closing_at.is_some();
        let elapsed = self.closing_at.unwrap_or(self.opened_at).elapsed();
        let span = if closing { EXIT } else { ENTER };
        let raw = elapsed.as_secs_f32() / span.as_secs_f32();
        if raw < 1.0 {
            window.request_animation_frame();
        }
        let t = raw.clamp(0.0, 1.0);

        // Visualizer runs off the same on-demand redraw: one FFT + scene step
        // per frame, and a standing request for the next one while a scene is
        // up. Off costs nothing — no tick, no frame request, no canvas.
        if viz_mode.is_on() && !closing {
            let tuning = self.session.read(cx).settings.visualizer_tuning;
            self.visualizer.tick(viz_mode, tuning);
            window.request_animation_frame();
        }

        // gpui has no transform, so the zoom is four animated insets on an
        // otherwise unsized absolute element; the extra vertical term slides
        // the overlay up from / down toward the player bar. Opacity and
        // geometry run on separate curves — flat on one curve reads as a fade,
        // not as depth.
        let (fade, inset, shift) = if closing {
            // Motion starts immediately (linear term) and accelerates away;
            // a pure ease-in stalls for the first ~80ms and reads as lag.
            let motion = 0.35 * t + 0.65 * t * t;
            // Opacity holds through the first half so the drop is actually
            // seen — fade it out early and the exit degenerates into a
            // cross-fade, which is what the travel distance is for.
            (1.0 - t * t, motion * 26., motion * 64.)
        } else {
            // Opacity lands at ~60% of the timeline so the overlay is solid
            // while the geometry is still easing. `out_back` overshoots, so
            // the insets go a hair negative at the end and settle back.
            let motion = out_back(t);
            (
                out_cubic(phase(t, 0.0, 0.6)),
                (1.0 - motion) * 34.,
                (1.0 - motion) * 46.,
            )
        };
        // Positive top / negative bottom offsets the whole overlay downward:
        // the entrance shrinks that offset to zero (rises into place), the exit
        // grows it from zero (drops away).
        let (top_inset, bottom_inset) = (inset + shift, inset - shift);
        // Content lags the backdrop on the way in and is inert on the way out.
        let (content_fade, content_rise) = if closing {
            (1.0, 0.)
        } else {
            (
                out_cubic(phase(t, 0.10, 0.75)),
                (1.0 - out_cubic(phase(t, 0.06, 1.0))) * 22.,
            )
        };

        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .large()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        // Floating mini player: while a scene runs it replaces the big cover +
        // info column, so the visualizer gets the whole window and the controls
        // sit on top of it in one compact card — including the scene picker,
        // which is otherwise buried in the cycle button that just got hidden.
        // Built before the mini player's own closures: they borrow `cx`
        // immutably for their listeners, and the card needs it mutably.
        let tuning_card = (viz_mode.is_on() && self.viz_tuning_open)
            .then(|| self.viz_tuning_card(viz_mode, cx).into_any_element());

        let mini_player = viz_mode.is_on().then(|| {
            let mode_btn = |mode: VisualizerMode, current: VisualizerMode| {
                let id = match mode {
                    VisualizerMode::Off => "mini-viz-off",
                    _ => "mini-viz-auto",
                };
                Button::new(id)
                    .ghost()
                    .small()
                    .label(mode.menu_label())
                    .when(mode == current, |b| b.primary())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_visualizer(mode, cx);
                        cx.stop_propagation();
                    }))
            };
            // Scenes live behind one button: five of them in the row crowded
            // the card and buried Off and Auto, which are not scenes at all.
            // The button shows the running scene, so the row still reads as
            // the current state at a glance.
            let scene_trigger = Button::new("mini-viz-more")
                .ghost()
                .small()
                .icon(Icon::new(IconName::ChevronDown).xsmall())
                .label(pinned_scene_label(viz_mode).unwrap_or("More"))
                .when(pinned_scene_label(viz_mode).is_some(), |b| b.primary());

            div()
                .absolute()
                .bottom(px(36.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    v_flex()
                        .w(px(560.))
                        .max_w_full()
                        // The card hangs off the right edge as an absolute
                        // child rather than sitting in the flow: in the flow it
                        // would drag the mini player off centre whenever it
                        // opened, and the mini player is the thing that has to
                        // stay put.
                        .relative()
                        .when_some(tuning_card, |this, card| {
                            this.child(
                                div()
                                    .absolute()
                                    .left(gpui::relative(1.))
                                    .ml(px(12.))
                                    .bottom_0()
                                    .child(card),
                            )
                        })
                        .child(
                            v_flex()
                                .w_full()
                                .gap_3()
                                .px_5()
                                .py_4()
                                .rounded_2xl()
                                .shadow_xl()
                                // Translucent so the scene keeps running behind it,
                                // opaque enough to read against moving geometry.
                                .bg(cx.theme().background.opacity(0.72))
                                .border_1()
                                .border_color(cx.theme().border.opacity(0.6))
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(
                                            div()
                                                .size(px(56.))
                                                .flex_none()
                                                .rounded_lg()
                                                .bg(cx.theme().muted)
                                                .overflow_hidden()
                                                .when_some(
                                                    self.art_path.clone().filter(|_| !is_radio),
                                                    |this, path| {
                                                        this.child(
                                                            img(path).size(px(56.)).rounded_lg(),
                                                        )
                                                    },
                                                )
                                                .when(is_radio, |this| {
                                                    this.flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_color(cx.theme().primary)
                                                        .child(app_icon(icons::RADIO))
                                                }),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_0p5()
                                                .child(
                                                    div()
                                                        .font_semibold()
                                                        .truncate()
                                                        .when(!has_track, |s: gpui::Div| {
                                                            s.text_color(
                                                                cx.theme().muted_foreground,
                                                            )
                                                        })
                                                        .child(mini_title.unwrap_or_else(|| {
                                                            "Nothing playing".into()
                                                        })),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .truncate()
                                                        .child(mini_artist.unwrap_or_default()),
                                                ),
                                        )
                                        .child(
                                            h_flex()
                                                .flex_none()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    icon_btn("mini-prev", icons::SKIP_BACK, false)
                                                        .disabled(!has_track || is_radio)
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.player
                                                                .update(cx, |p, cx| p.previous(cx));
                                                            cx.stop_propagation();
                                                        })),
                                                )
                                                .child(
                                                    Button::new("mini-play")
                                                        .primary()
                                                        .icon(if playing {
                                                            app_icon(icons::PAUSE)
                                                        } else {
                                                            app_icon(icons::PLAY)
                                                        })
                                                        .loading(buffering)
                                                        .disabled(!has_track)
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.player.update(cx, |p, cx| {
                                                                p.toggle_play(cx)
                                                            });
                                                            cx.stop_propagation();
                                                        })),
                                                )
                                                .child(
                                                    icon_btn(
                                                        "mini-next",
                                                        icons::SKIP_FORWARD,
                                                        false,
                                                    )
                                                    .disabled(!has_track || is_radio)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.player.update(cx, |p, cx| p.next(cx));
                                                        cx.stop_propagation();
                                                    })),
                                                ),
                                        ),
                                )
                                // Seek row — live radio has nothing to seek.
                                .when(!is_radio, |this| {
                                    this.child(
                                        h_flex()
                                            .w_full()
                                            .gap_3()
                                            .items_center()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(mini_time_now),
                                            )
                                            .map(|this| {
                                                let bar =
                                                    match (waveform_enabled, self.waveform.clone())
                                                    {
                                                        (true, Some(peaks)) => {
                                                            crate::ui::waveform_seek_bar(
                                                                &peaks,
                                                                seek_fraction,
                                                                22.,
                                                                cx.theme().primary,
                                                                cx.theme()
                                                                    .muted_foreground
                                                                    .opacity(0.35),
                                                                self.player.clone(),
                                                            )
                                                        }
                                                        _ => div()
                                                            .flex_1()
                                                            .child(Slider::new(&self.seek))
                                                            .into_any_element(),
                                                    };
                                                let view = cx.entity();
                                                this.child(crate::ui::seek_hover_wrap(
                                                    "fs-mini-seek-hover",
                                                    self.seek_hover,
                                                    duration,
                                                    bar,
                                                    move |fraction, cx| {
                                                        view.update(cx, |p: &mut Self, cx| {
                                                            if p.seek_hover != fraction {
                                                                p.seek_hover = fraction;
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
                                                    .child(mini_time_total),
                                            ),
                                    )
                                })
                                .when(is_radio, |this| {
                                    this.child(h_flex().w_full().justify_center().child(
                                        crate::ui::live_badge(
                                            "mini-live",
                                            cx.theme().primary,
                                            Some(position),
                                            cx,
                                        ),
                                    ))
                                })
                                // Scene picker.
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .justify_center()
                                        .pt_1()
                                        .border_t_1()
                                        .border_color(cx.theme().border.opacity(0.4))
                                        .child(mode_btn(VisualizerMode::Off, viz_mode))
                                        .child(mode_btn(VisualizerMode::Auto, viz_mode))
                                        .child(self.scene_menu(scene_trigger, viz_mode))
                                        .child(
                                            Button::new("mini-viz-tune")
                                                .ghost()
                                                .small()
                                                .icon(Icon::new(IconName::Settings2).xsmall())
                                                .when(self.viz_tuning_open, |b| b.primary())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.viz_tuning_open = !this.viz_tuning_open;
                                                    cx.notify();
                                                    cx.stop_propagation();
                                                })),
                                        ),
                                ),
                        ),
                )
        });

        // No `size_full`: an absolute element with all four insets set and no
        // explicit size stretches between them, which is what lets the insets
        // act as a zoom.
        div()
            .absolute()
            .left(px(inset))
            .right(px(inset))
            .top(px(top_inset))
            .bottom(px(bottom_inset))
            .opacity(fade)
            // Swallow mouse events so clicks don't fall through to the UI below.
            .occlude()
            .bg(cx.theme().background)
            // Visualizer takes over the backdrop when running: it needs the
            // whole window to read as 3D, and the scrim + content still sit
            // on top of it.
            .child(if viz_mode.is_on() {
                self.visualizer.render(cx.theme().primary)
            } else {
                self.render_background(bg_mode, cx)
            })
            // Readability scrim.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full()
                    .bg(cx.theme().background)
                    .opacity(scrim),
            )
            // Close button — clearly labelled pill, top right.
            .child(
                div()
                    .absolute()
                    .top_4()
                    .right_4()
                    .rounded_full()
                    .bg(cx.theme().muted.opacity(0.55))
                    .child(
                        Button::new("fs-close")
                            .ghost()
                            // Pill rounding to match the wrapper: the button's
                            // default (theme radius) hover fill is squarer than
                            // the rounded-full backdrop, so the shape appeared
                            // to change on hover.
                            .rounded(px(9999.))
                            .icon(Icon::new(IconName::ChevronDown))
                            .label("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.begin_close(cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            // Main content: album art on the left, info + controls on the
            // right, optional side panel + vertical volume at the far right.
            // Stood down while a scene runs — the visualizer wants the whole
            // window, and the mini player carries the controls instead.
            .when(!viz_mode.is_on(), |root| {
                root.child({
                    let content = h_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_8()
                        .px_10()
                        // Album art.
                        .child(
                            div()
                                .size(px(art_size))
                                .flex_none()
                                .rounded_2xl()
                                .bg(cx.theme().muted)
                                .overflow_hidden()
                                .shadow_xl()
                                .when_some(
                                    self.art_path.clone().filter(|_| !is_radio),
                                    |this, path| {
                                        this.child(img(path).size(px(art_size)).rounded_2xl())
                                    },
                                )
                                // A station has no artwork; mark the slot as
                                // radio rather than leaving a blank square the
                                // size of a record sleeve.
                                .when(is_radio, |this| {
                                    this.flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(cx.theme().primary)
                                        .child(
                                            app_icon(icons::RADIO).with_size(px(art_size * 0.28)),
                                        )
                                }),
                        )
                        // Info + controls column — right of the cover. Same
                        // card treatment as the mini player and the same
                        // internal order (info, seek, transport, then a ruled
                        // row of toggles), so the three players read as one
                        // design at three sizes rather than three designs.
                        .child(
                            v_flex()
                                .flex_none()
                                .w(px(480.))
                                .justify_center()
                                .gap_5()
                                .p_6()
                                .rounded_2xl()
                                .bg(cx.theme().background.opacity(0.55))
                                .border_1()
                                .border_color(cx.theme().border.opacity(0.5))
                                .shadow_xl()
                                // Track info.
                                .child(
                                    v_flex()
                                        // Bounded by the card, not by a wider
                                        // guess: a max wider than the card
                                        // lets `truncate` clip at the border
                                        // with no ellipsis, which reads as a
                                        // rendering fault rather than a long
                                        // title (radio titles are long).
                                        .w_full()
                                        .gap_1()
                                        // Scrolls when it does not fit the
                                        // card, rather than being cut off.
                                        .child(crate::ui::scrolling_line(
                                            "fs-title-text",
                                            title
                                                .clone()
                                                .unwrap_or_else(|| "Nothing playing".into())
                                                .into(),
                                            px(CARD_TEXT_WIDTH),
                                            window.rem_size() * 1.875,
                                            gpui::FontWeight::SEMIBOLD,
                                            (!has_track).then(|| cx.theme().muted_foreground),
                                            window,
                                        ))
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
                                // Seek bar.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .max_w(px(640.))
                                        .gap_3()
                                        .items_center()
                                        .when(is_radio, |this| {
                                            this.justify_center().child(crate::ui::live_badge(
                                                "fs-live",
                                                cx.theme().primary,
                                                Some(position),
                                                cx,
                                            ))
                                        })
                                        .when(!is_radio, |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(time_now),
                                            )
                                            .map(|this| {
                                                let bar =
                                                    match (waveform_enabled, self.waveform.clone())
                                                    {
                                                        (true, Some(peaks)) => {
                                                            crate::ui::waveform_seek_bar(
                                                                &peaks,
                                                                seek_fraction,
                                                                34.,
                                                                cx.theme().primary,
                                                                cx.theme()
                                                                    .muted_foreground
                                                                    .opacity(0.35),
                                                                self.player.clone(),
                                                            )
                                                        }
                                                        _ => div()
                                                            .flex_1()
                                                            .child(Slider::new(&self.seek))
                                                            .into_any_element(),
                                                    };
                                                let view = cx.entity();
                                                this.child(crate::ui::seek_hover_wrap(
                                                    "fs-seek-hover",
                                                    self.seek_hover,
                                                    duration,
                                                    bar,
                                                    move |fraction, cx| {
                                                        view.update(cx, |p: &mut Self, cx| {
                                                            if p.seek_hover != fraction {
                                                                p.seek_hover = fraction;
                                                                cx.notify();
                                                            }
                                                        });
                                                    },
                                                    cx,
                                                ))
                                            })
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(time_total),
                                            )
                                        }),
                                )
                                // Stream info + ReplayGain: quiet, centered line.
                                .when(stream_info.is_some() || replay_gain.is_some(), |this| {
                                    this.child(
                                        h_flex()
                                            .gap_3()
                                            .items_center()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground.opacity(0.8))
                                            .when_some(stream_info, |this, info| {
                                                this.child(div().child(info))
                                            })
                                            .when_some(replay_gain, |this, (label, db)| {
                                                let text = match db {
                                                    Some(db) => format!("RG {db:+.1} dB · {label}"),
                                                    None => format!("RG · {label}"),
                                                };
                                                this.child(div().child(text))
                                            }),
                                    )
                                })
                                // Transport controls.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_4()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            icon_btn(
                                                "fs-shuffle",
                                                icons::SHUFFLE,
                                                shuffle && !is_radio,
                                            )
                                            .disabled(is_radio)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.toggle_shuffle(cx));
                                                    cx.stop_propagation();
                                                }),
                                            ),
                                        )
                                        .child(
                                            icon_btn("fs-prev", icons::SKIP_BACK, false)
                                                .disabled(!has_track || is_radio)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player.update(cx, |p, cx| p.previous(cx));
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-play")
                                                .primary()
                                                .large()
                                                .icon(if playing {
                                                    app_icon(icons::PAUSE)
                                                } else {
                                                    app_icon(icons::PLAY)
                                                })
                                                .loading(buffering)
                                                .disabled(!has_track)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.toggle_play(cx));
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            icon_btn("fs-next", icons::SKIP_FORWARD, false)
                                                .disabled(!has_track || is_radio)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.player.update(cx, |p, cx| p.next(cx));
                                                    cx.stop_propagation();
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
                                                repeat != RepeatMode::Off && !is_radio,
                                            )
                                            .disabled(is_radio)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.player
                                                        .update(cx, |p, cx| p.cycle_repeat(cx));
                                                    cx.stop_propagation();
                                                }),
                                            ),
                                        ),
                                )
                                // Queue / visualizer / lyrics toggles, ruled
                                // off below the transport exactly as the mini
                                // player rules off its scene picker.
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_3()
                                        .items_center()
                                        .justify_center()
                                        .pt_4()
                                        .border_t_1()
                                        .border_color(cx.theme().border.opacity(0.4))
                                        .child(
                                            Button::new("fs-queue-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::PanelRight))
                                                .label("Queue")
                                                .when(self.panel == Some(SidePanel::Queue), |b| {
                                                    b.primary()
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.toggle_panel(SidePanel::Queue, cx);
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-viz-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::Frame))
                                                .label(viz_mode.label())
                                                .when(viz_mode.is_on(), |b| b.primary())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cycle_visualizer(cx);
                                                    cx.stop_propagation();
                                                })),
                                        )
                                        .child(
                                            Button::new("fs-lyrics-btn")
                                                .ghost()
                                                .large()
                                                .icon(Icon::new(IconName::BookOpen))
                                                .label("Lyrics")
                                                .when(self.panel == Some(SidePanel::Lyrics), |b| {
                                                    b.primary()
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.toggle_panel(SidePanel::Lyrics, cx);
                                                    cx.stop_propagation();
                                                })),
                                        ),
                                ),
                        )
                        // Short vertical volume slider, right of the controls —
                        // off by default (the player bar already has one), opt-in
                        // via settings; always hidden during live radio.
                        .when(show_volume && !is_radio, |this| {
                            this.child(
                                v_flex()
                                    .h_full()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_HIGH)),
                                    )
                                    .child(
                                        // gpui-component's vertical slider is a fixed
                                        // 120px tall; match it so the high/low icons sit
                                        // symmetrically at each end (no dead space).
                                        div()
                                            .h(px(120.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(Slider::new(&self.volume).vertical()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(app_icon(icons::VOLUME_LOW)),
                                    )
                                    .when(detailed_volume, |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{}%",
                                                    (volume_level * 100.).round() as u32
                                                )),
                                        )
                                    }),
                            )
                        })
                        // Optional side panel.
                        .when_some(self.panel, |this, panel| {
                            this.child(match panel {
                                SidePanel::Queue => self.render_queue_panel(cx),
                                SidePanel::Lyrics => self.render_lyrics_panel(cx),
                            })
                        });

                    // Content trails the backdrop and travels further, so the
                    // overlay reads as art rising into place rather than one flat
                    // layer sliding. Entrance only: on the way out it stays put
                    // while the whole overlay drops, which keeps the exit from
                    // looking like two things leaving at different speeds.
                    content.opacity(content_fade).top(px(content_rise))
                })
            })
            .when_some(mini_player, |root, mini| root.child(mini))
    }
}
