//! Internet radio: station list, play, add, edit, delete.

use gpui::{
    App, Context, Entity, Focusable as _, IntoElement, Render, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::state::player::PlayerState;
use crate::state::radio::RadioState;
use crate::state::session::Session;
use crate::ui::with_focus_cursor;

/// A station as the add/edit form will send it.
#[derive(Debug, PartialEq)]
struct StationForm {
    name: String,
    stream_url: String,
    homepage_url: Option<String>,
}

/// True for an `http(s)://` URL with something after the scheme and no
/// whitespace inside — enough to catch a pasted name or a missing scheme
/// before the server stores a station that can never play.
fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        !host.is_empty() && !rest.chars().any(char::is_whitespace)
    })
}

/// Check the form's three fields (already trimmed). The homepage is optional.
fn validate_station(name: &str, url: &str, homepage: &str) -> Result<StationForm, &'static str> {
    if name.is_empty() {
        return Err("Give the station a name.");
    }
    if url.is_empty() {
        return Err("Enter the station's stream URL.");
    }
    if !is_web_url(url) {
        return Err("The stream URL must start with http:// or https://.");
    }
    if !homepage.is_empty() && !is_web_url(homepage) {
        return Err("The homepage must start with http:// or https://.");
    }
    Ok(StationForm {
        name: name.to_string(),
        stream_url: url.to_string(),
        homepage_url: (!homepage.is_empty()).then(|| homepage.to_string()),
    })
}

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
    /// Read for one thing: whether the vi cursor glows.
    session: Entity<Session>,
    name_input: Entity<InputState>,
    url_input: Entity<InputState>,
    homepage_input: Entity<InputState>,
    /// Id of the station the form is editing; `None` = the form adds one.
    editing: Option<String>,
    /// Why the form was not sent, shown under it until the next submit.
    form_error: Option<&'static str>,
    /// The two things this view draws out of the player: which station is
    /// live, and what it says is on. Everything else the player reports —
    /// position above all — changes nothing here.
    playing: (Option<String>, Option<String>),
    /// Station row / form input index under the vi-mode cursor. Flattened:
    /// station rows 0..n, then name, url and homepage inputs, the Add/Save
    /// button, and Cancel while editing.
    vi_cursor: Option<usize>,
}

impl RadioView {
    pub fn new(
        radio: Entity<RadioState>,
        player: Entity<PlayerState>,
        session: Entity<Session>,
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
        let homepage_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Homepage (optional)"));
        let playing = player_signature(&player, cx);
        Self {
            radio,
            player,
            session,
            name_input,
            url_input,
            homepage_input,
            editing: None,
            form_error: None,
            playing,
            vi_cursor: None,
        }
    }

    /// Send the form: add a station, or save the one being edited.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field =
            |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();
        let (name, url, homepage) = (
            field(&self.name_input, cx),
            field(&self.url_input, cx),
            field(&self.homepage_input, cx),
        );
        let form = match validate_station(&name, &url, &homepage) {
            Ok(form) => form,
            Err(why) => {
                self.form_error = Some(why);
                cx.notify();
                return;
            }
        };
        let editing = self.editing.take();
        self.radio.update(cx, |r, cx| match editing {
            Some(id) => r.update(id, form.name, form.stream_url, form.homepage_url, cx),
            None => r.create(form.name, form.stream_url, form.homepage_url, cx),
        });
        self.reset_form(window, cx);
    }

    /// Load station `i` into the form for editing.
    fn start_edit(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(station) = self.radio.read(cx).stations.get(i).cloned() else {
            return;
        };
        self.editing = Some(station.id);
        self.form_error = None;
        let homepage = station.home_page_url.unwrap_or_default();
        self.name_input
            .update(cx, |s, cx| s.set_value(station.name, window, cx));
        self.url_input
            .update(cx, |s, cx| s.set_value(station.stream_url, window, cx));
        self.homepage_input
            .update(cx, |s, cx| s.set_value(homepage, window, cx));
        self.name_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Empty the form and leave edit mode.
    fn reset_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.form_error = None;
        for input in [&self.name_input, &self.url_input, &self.homepage_input] {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        cx.notify();
    }

    fn delete_station(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        // Saving an edit to a station that is gone would fail on the server.
        if self.editing.as_deref() == Some(id.as_str()) {
            self.reset_form(window, cx);
        }
        self.radio.update(cx, |r, cx| r.delete(id, cx));
    }

    /// Form targets after the station rows: three inputs, the submit button,
    /// and Cancel while editing.
    fn form_targets(&self) -> usize {
        4 + usize::from(self.editing.is_some())
    }

    /// True while one of the add-station fields has the keyboard.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        [&self.name_input, &self.url_input, &self.homepage_input]
            .iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    fn station_count(&self, cx: &App) -> usize {
        self.radio.read(cx).stations.len()
    }

    pub fn vi_move(&mut self, delta: isize, _window: &mut Window, cx: &mut Context<Self>) {
        // The form targets always follow the station rows, so there is
        // always something to land on even with no stations.
        let count = self.station_count(cx) + self.form_targets();
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        cx.notify();
    }

    /// Enter on the focused station/form target: play the station, focus the
    /// pinned input, or run `add_station`.
    pub fn vi_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.station_count(cx);
        let Some(i) = self.vi_cursor else {
            return;
        };
        if i < n {
            let (name, url) = {
                let r = self.radio.read(cx);
                let s = &r.stations[i];
                (s.name.clone(), s.stream_url.clone())
            };
            self.player.update(cx, |p, cx| p.play_radio(name, url, cx));
        } else if i == n {
            self.name_input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        } else if i == n + 1 {
            self.url_input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        } else if i == n + 2 {
            self.homepage_input.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        } else if i == n + 3 {
            self.submit(window, cx);
        } else if i == n + 4 {
            self.reset_form(window, cx);
        }
    }

    /// `i` on the radio page: focus the name field.
    pub fn vi_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.name_input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn vi_clear(&mut self, cx: &mut Context<Self>) {
        if self.vi_cursor.take().is_some() {
            cx.notify();
        }
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

        let editing = self.editing.is_some();
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        // The form targets sit after the station rows in the same flat vi
        // index space `vi_move`/`vi_activate` walk.
        let station_count = stations.len();
        let rows: Vec<_> = stations
            .into_iter()
            .enumerate()
            .map(|(i, station)| {
                let is_playing = playing_station.as_deref() == Some(station.name.as_str());
                let now_playing = is_playing.then(|| playing_track.clone()).flatten();
                let name = station.name.clone();
                let url = station.stream_url.clone();
                let id = station.id.clone();
                let homepage = station.home_page_url.clone().filter(|u| is_web_url(u));
                let being_edited = self.editing.as_deref() == Some(station.id.as_str());
                let focused = self.vi_cursor == Some(i);
                let row = h_flex()
                    .id(("radio", i))
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| {
                        let s = s.bg(cx.theme().muted);
                        if hover_glow {
                            crate::ui::hover_glow_style(s, None, cx)
                        } else {
                            s
                        }
                    })
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
                    .when_some(homepage, |row, url| {
                        row.child(
                            Button::new(("radio-home", i))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Globe))
                                .tooltip("Open the station's homepage")
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&url);
                                }),
                        )
                    })
                    .child(
                        Button::new(("radio-edit", i))
                            .ghost()
                            .xsmall()
                            .icon(app_icon(icons::PENCIL))
                            .selected(being_edited)
                            .tooltip("Edit station")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.start_edit(i, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("radio-del", i))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .tooltip("Delete station")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.delete_station(id.clone(), window, cx);
                            })),
                    );
                with_focus_cursor(format!("vi-radio-{i}"), row, focused, glow, None, cx)
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
            // Add/edit form. Each control is a vi target in its own right,
            // so each carries the cursor ring — without it j/k past the last
            // station leaves nothing highlighted while Enter still fires.
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .mt_2()
                    .child(with_focus_cursor(
                        "vi-radio-name",
                        div().w(px(200.)).child(Input::new(&self.name_input)),
                        self.vi_cursor == Some(station_count),
                        glow,
                        None,
                        cx,
                    ))
                    .child(with_focus_cursor(
                        "vi-radio-url",
                        div().w(px(320.)).child(Input::new(&self.url_input)),
                        self.vi_cursor == Some(station_count + 1),
                        glow,
                        None,
                        cx,
                    ))
                    .child(with_focus_cursor(
                        "vi-radio-homepage",
                        div().w(px(240.)).child(Input::new(&self.homepage_input)),
                        self.vi_cursor == Some(station_count + 2),
                        glow,
                        None,
                        cx,
                    ))
                    .child(with_focus_cursor(
                        "vi-radio-add",
                        div().child(
                            Button::new("radio-add")
                                .primary()
                                .label(if editing { "Save" } else { "Add" })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                        ),
                        self.vi_cursor == Some(station_count + 3),
                        glow,
                        None,
                        cx,
                    ))
                    .when(editing, |row| {
                        row.child(with_focus_cursor(
                            "vi-radio-cancel",
                            div().child(
                                Button::new("radio-cancel")
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reset_form(window, cx)
                                    })),
                            ),
                            self.vi_cursor == Some(station_count + 4),
                            glow,
                            None,
                            cx,
                        ))
                    }),
            )
            .when_some(self.form_error, |this, why| {
                this.child(div().text_color(cx.theme().danger).text_sm().child(why))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{StationForm, is_web_url, validate_station};

    #[test]
    fn web_urls_need_a_scheme_and_a_host() {
        assert!(is_web_url("https://ice.somafm.com/groovesalad"));
        assert!(is_web_url("HTTP://example.com:8000"));
        assert!(is_web_url("http://example.com?type=.mp3"));
        assert!(!is_web_url("ice.somafm.com/groovesalad"));
        assert!(!is_web_url("https://"));
        assert!(!is_web_url("https:///stream"));
        assert!(!is_web_url("https://example.com/my stream"));
        assert!(!is_web_url("ftp://example.com/stream"));
    }

    #[test]
    fn station_form_checks_each_field() {
        assert!(validate_station("", "https://a.b/s", "").is_err());
        assert!(validate_station("Soma", "", "").is_err());
        assert!(validate_station("Soma", "somafm.com", "").is_err());
        assert!(validate_station("Soma", "https://a.b/s", "somafm.com").is_err());
        assert_eq!(
            validate_station("Soma", "https://a.b/s", ""),
            Ok(StationForm {
                name: "Soma".into(),
                stream_url: "https://a.b/s".into(),
                homepage_url: None,
            })
        );
        assert_eq!(
            validate_station("Soma", "https://a.b/s", "https://somafm.com")
                .unwrap()
                .homepage_url
                .as_deref(),
            Some("https://somafm.com")
        );
    }
}
