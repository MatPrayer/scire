//! Full-window now-playing overlay with dynamic blurred-art background.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, Context, Entity, EventEmitter, IntoElement, Render, Window, div,
    ease_out_quint, img, linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use subsonic::SubsonicClient;

use crate::assets::{app_icon, icons};
use crate::config::FullscreenBackground;
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
    gradient_palette: Option<Vec<gpui::Rgba>>,
    last_cover_id: Option<String>,
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
    /// True while the exit animation plays, before the overlay unmounts.
    closing: bool,
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
                this.gradient_palette = None;
                if let Some(cover_id) = cover {
                    this.fetch_art(cover_id, cx);
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
            last_cover_id: None,
            panel: None,
            lyrics: None,
            lyrics_for: None,
            lyrics_loading: false,
            waveform: None,
            waveform_for: None,
            closing: false,
        }
    }

    /// Start the exit animation, then emit Close so the overlay unmounts.
    pub fn begin_close(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.closing = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(190))
                .await;
            let _ = this.update(cx, |this, cx| {
                // Skip if a quick reopen already cancelled the close.
                if this.closing {
                    this.closing = false;
                    cx.emit(FullscreenEvent::Close);
                }
            });
        })
        .detach();
    }

    /// Reset state so a fresh open plays the entrance (not a stale exit).
    pub fn reset_for_open(&mut self, cx: &mut Context<Self>) {
        self.closing = false;
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
            let result = runtime::spawn_io(crate::services::waveform::fetch_peaks(url, id.clone()))
                .await;
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
        let bot = *palette.last().unwrap();
        match mode {
            FullscreenBackground::Solid => base().bg(cx.theme().background).into_any_element(),
            FullscreenBackground::Gradient => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.5), 0.),
                    linear_color_stop(scale_rgb(bot, 0.5), 1.),
                ))
                .into_any_element(),
            FullscreenBackground::Vibrant => base()
                .bg(linear_gradient(
                    160.,
                    linear_color_stop(scale_rgb(top, 0.9), 0.),
                    linear_color_stop(scale_rgb(bot, 0.9), 1.),
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
                    let r: Vec<gpui::Rgba> =
                        palette.iter().map(|&c| scale_rgb(vivid(c, 1.5), 1.0)).collect();
                    if r.len() < 2 {
                        let b = *r.first().unwrap_or(&gpui::Rgba::from(cx.theme().background));
                        vec![b, scale_rgb(b, 1.5), scale_rgb(b, 0.6)]
                    } else {
                        r
                    }
                };
                let ring2 = ring.clone();
                let anim_layer =
                    |id: &'static str, angle: f32, ring: Vec<gpui::Rgba>, oa: f32, ob: f32, alpha: f32| {
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
                    // Solid base so the translucent layers composite over something.
                    .bg(scale_rgb(bot, 0.5))
                    .overflow_hidden()
                    .child(anim_layer("fs-bg-a", 229., ring, 0.0, 0.5, 1.0))
                    .child(anim_layer("fs-bg-b", 63., ring2, 0.28, 0.78, 0.55))
                    .into_any_element()
            }
        }
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
        let detailed_volume = self.session.read(cx).settings.detailed_volume;
        let volume_level = self.player.read(cx).volume;
        let replay_gain = self.player.read(cx).replay_gain_active();
        let stream_info = crate::ui::stream_info_line(
            self.player.read(cx),
            &self.session.read(cx).settings,
        );

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
        let panel_open = self.panel.is_some();
        let art_size = if panel_open { 360. } else { 460. };

        let icon_btn = |id: &'static str, icon_path: &'static str, active: bool| {
            Button::new(id)
                .ghost()
                .large()
                .icon(app_icon(icon_path))
                .when(active, |b| b.primary())
        };

        let root = div()
            .absolute()
            .left_0()
            .top_0()
            .size_full()
            // Swallow mouse events so clicks don't fall through to the UI below.
            .occlude()
            .bg(cx.theme().background)
            .child(self.render_background(bg_mode, cx))
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
            .child(
                h_flex()
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
                            .when_some(self.art_path.clone(), |this, path| {
                                this.child(img(path).size(px(art_size)).rounded_2xl())
                            }),
                    )
                    // Info + controls column — right of the cover.
                    .child(
                        v_flex()
                            .flex_none()
                            .w(px(440.))
                            .justify_center()
                            .gap_5()
                            // Track info.
                            .child(
                                v_flex()
                                    .max_w(px(560.))
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
                            // Seek bar.
                            .child(
                                h_flex()
                                    .w_full()
                                    .max_w(px(640.))
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
                                        .map(|this| {
                                            match (waveform_enabled, self.waveform.clone()) {
                                                (true, Some(peaks)) => {
                                                    this.child(crate::ui::waveform_seek_bar(
                                                        &peaks,
                                                        seek_fraction,
                                                        34.,
                                                        cx.theme().primary,
                                                        cx.theme().muted_foreground.opacity(0.35),
                                                        self.player.clone(),
                                                    ))
                                                }
                                                _ => this.child(
                                                    div().flex_1().child(Slider::new(&self.seek)),
                                                ),
                                            }
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
                                    .gap_4()
                                    .items_center()
                                    .child(
                                        icon_btn("fs-shuffle", icons::SHUFFLE, shuffle).on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.player
                                                    .update(cx, |p, cx| p.toggle_shuffle(cx));
                                                cx.stop_propagation();
                                            }),
                                        ),
                                    )
                                    .child(
                                        icon_btn("fs-prev", icons::SKIP_BACK, false)
                                            .disabled(!has_track)
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
                                                this.player.update(cx, |p, cx| p.toggle_play(cx));
                                                cx.stop_propagation();
                                            })),
                                    )
                                    .child(
                                        icon_btn("fs-next", icons::SKIP_FORWARD, false)
                                            .disabled(!has_track)
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
                                            repeat != RepeatMode::Off,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.player.update(cx, |p, cx| p.cycle_repeat(cx));
                                                cx.stop_propagation();
                                            }),
                                        ),
                                    ),
                            )
                            // Queue / Lyrics toggles — larger.
                            .child(
                                h_flex()
                                    .gap_3()
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
                    // Short vertical volume slider, right of the controls
                    // (hidden during live radio).
                    .when(!is_radio, |this| {
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
                    }),
            );

        // Entrance fades/slides in on mount; exit reverses it before unmount.
        if self.closing {
            root.with_animation(
                "fs-exit",
                Animation::new(Duration::from_secs_f64(0.19)).with_easing(ease_out_quint()),
                |this, delta| this.opacity(1. - delta).top(px(delta * 24.)),
            )
            .into_any_element()
        } else {
            root.with_animation(
                "fs-enter",
                Animation::new(Duration::from_secs_f64(0.22)).with_easing(ease_out_quint()),
                |this, delta| this.opacity(delta).top(px((1. - delta) * 24.)),
            )
            .into_any_element()
        }
    }
}
