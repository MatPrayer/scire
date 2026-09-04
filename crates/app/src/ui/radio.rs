//! Internet radio: station list, play, add, delete.

use gpui::{Context, Entity, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::state::player::PlayerState;
use crate::state::radio::RadioState;

/// What the radio page draws out of the player: the live station's name, and
/// the track it announced.
///
/// Match on the station, not on `now_playing()`: once the station announces a
/// track, that title is the track's, not the station's.
fn player_signature(
    player: &Entity<PlayerState>,
    cx: &gpui::App,
) -> (Option<String>, Option<String>) {
    let p = player.read(cx);
    let station = p.radio_station_name().map(str::to_string);
    let track = p.radio_now_playing().map(|(artist, title)| match artist {
        Some(artist) => format!("{artist} — {title}"),
        None => title,
    });
    (station, track)
}

pub struct RadioView {
    radio: Entity<RadioState>,
    player: Entity<PlayerState>,
    name_input: Entity<InputState>,
    url_input: Entity<InputState>,
    /// The two things this view draws out of the player: which station is
    /// live, and what it says is on. Everything else the player reports —
    /// position above all — changes nothing here.
    playing: (Option<String>, Option<String>),
}

impl RadioView {
    pub fn new(
        radio: Entity<RadioState>,
        player: Entity<PlayerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&radio, |_, _, cx| cx.notify()).detach();
        // PlayerState notifies on every event, position ticks included; only a
        // change to the live station or its announced track shows up here.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let now = player_signature(&player, cx);
            if now != this.playing {
                this.playing = now;
                cx.notify();
            }
        })
        .detach();
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Station name"));
        let url_input = cx.new(|cx| InputState::new(window, cx).placeholder("Stream URL"));
        let playing = player_signature(&player, cx);
        Self {
            radio,
            player,
            name_input,
            url_input,
            playing,
        }
    }

    fn add_station(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_input.read(cx).value().trim().to_string();
        let url = self.url_input.read(cx).value().trim().to_string();
        if name.is_empty() || url.is_empty() {
            return;
        }
        self.radio.update(cx, |r, cx| r.create(name, url, cx));
        self.name_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.url_input
            .update(cx, |i, cx| i.set_value("", window, cx));
    }
}

impl Render for RadioView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Kept in step by the player observer rather than re-read here.
        let (playing_station, playing_track) = self.playing.clone();
        let (stations, error) = {
            let r = self.radio.read(cx);
            (r.stations.clone(), r.error.clone())
        };

        let rows: Vec<_> = stations
            .into_iter()
            .enumerate()
            .map(|(i, station)| {
                let is_playing = playing_station.as_deref() == Some(station.name.as_str());
                let now_playing = is_playing.then(|| playing_track.clone()).flatten();
                let name = station.name.clone();
                let url = station.stream_url.clone();
                let id = station.id.clone();
                h_flex()
                    .id(("radio", i))
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted))
                    .when(is_playing, |s| s.text_color(cx.theme().primary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let (name, url) = (name.clone(), url.clone());
                        this.player.update(cx, |p, cx| p.play_radio(name, url, cx));
                    }))
                    .when(is_playing, |this| {
                        this.child(crate::ui::live_badge(
                            "radio-row-live",
                            cx.theme().primary,
                            None,
                            cx,
                        ))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(station.name.clone()),
                    )
                    // The station's own now-playing line is worth more than its
                    // URL, so it takes that slot while the station is on.
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .max_w(px(280.))
                            .truncate()
                            .child(now_playing.unwrap_or_else(|| station.stream_url.clone())),
                    )
                    .child(
                        Button::new(("radio-del", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.radio.update(cx, |r, cx| r.delete(id.clone(), cx));
                                cx.stop_propagation();
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .id("radio-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(div().text_lg().child("Internet Radio"))
            .when_some(error, |this, e| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(e))
            })
            .child(v_flex().gap_0p5().children(rows))
            // Add-station form.
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .mt_2()
                    .child(div().w(px(200.)).child(Input::new(&self.name_input)))
                    .child(
                        div()
                            .flex_1()
                            .max_w(px(360.))
                            .child(Input::new(&self.url_input)),
                    )
                    .child(
                        Button::new("radio-add").primary().label("Add").on_click(
                            cx.listener(|this, _, window, cx| this.add_station(window, cx)),
                        ),
                    ),
            )
    }
}
