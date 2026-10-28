//! Internet radio: station list, play, add, edit, delete.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Context, Entity, Focusable as _, IntoElement, Render,
    Window, div, prelude::*, pulsating_between, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
};

use crate::assets::{app_icon, icons};
use crate::state::player::{PlayerState, RADIO_RECONNECT_ATTEMPTS};
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

/// How long a first press on a station's Delete stays armed.
const DELETE_ARM: Duration = Duration::from_secs(4);

/// What the radio page draws out of the player: which station is live, what
/// it says is on, and how the stream is doing. Everything else the player
/// reports — position above all — changes nothing here.
#[derive(Clone, Debug, Default, PartialEq)]
struct Live {
    /// Stream URL of the live station. Rows match on it, not on the name:
    /// two bookmarks may share a name, and the announced track title is not
    /// the station's.
    url: Option<String>,
    track: Option<String>,
    playing: bool,
    buffering: bool,
    reconnect: Option<u32>,
}

impl Live {
    fn read(player: &Entity<PlayerState>, cx: &App) -> Self {
        let p = player.read(cx);
        Self {
            url: p.radio_stream_url().map(str::to_string),
            track: p.radio_now_playing().map(|(artist, title)| match artist {
                Some(artist) => format!("{artist} — {title}"),
                None => title,
            }),
            playing: p.playing,
            buffering: p.buffering,
            reconnect: p.radio_reconnect_attempt(),
        }
    }
}

/// How a station row reads while it is the live one.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RowStatus {
    Connecting,
    Reconnecting(u32),
    OnAir,
    Paused,
}

/// The row's status, or `None` when `stream_url` is not the live station.
fn row_status(live: &Live, stream_url: &str) -> Option<RowStatus> {
    if live.url.as_deref() != Some(stream_url) {
        return None;
    }
    Some(match live.reconnect {
        Some(attempt) => RowStatus::Reconnecting(attempt),
        None if live.buffering => RowStatus::Connecting,
        None if live.playing => RowStatus::OnAir,
        None => RowStatus::Paused,
    })
}

/// The row's second line: the stream's state while it isn't simply on air,
/// else what the station announced, else its URL.
fn row_detail(status: Option<RowStatus>, track: Option<&str>, stream_url: &str) -> String {
    match status {
        Some(RowStatus::Connecting) => "Connecting…".into(),
        Some(RowStatus::Reconnecting(attempt)) => {
            format!("Signal lost — reconnecting ({attempt}/{RADIO_RECONNECT_ATTEMPTS})…")
        }
        Some(RowStatus::Paused) => {
            track.map_or_else(|| "Paused".into(), |t| format!("Paused · {t}"))
        }
        Some(RowStatus::OnAir) => track.unwrap_or(stream_url).to_string(),
        None => stream_url.to_string(),
    }
}

/// "1 station" / "4 stations".
fn station_count_label(n: usize) -> String {
    if n == 1 {
        "1 station".into()
    } else {
        format!("{n} stations")
    }
}

pub struct RadioView {
    radio: Entity<RadioState>,
    player: Entity<PlayerState>,
    /// Read for the vi cursor's glow and whether a server is connected.
    session: Entity<Session>,
    name_input: Entity<InputState>,
    url_input: Entity<InputState>,
    homepage_input: Entity<InputState>,
    /// Id of the station the form is editing; `None` = the form adds one.
    editing: Option<String>,
    /// Why the form was not sent, shown under it until the next submit.
    form_error: Option<&'static str>,
    /// The form went to the server and hasn't been answered. What was typed
    /// stays until the server takes it, so a failed save loses nothing.
    saving: bool,
    /// Station whose Delete was pressed once; a second press deletes. The
    /// counter keeps an older arm's timer from disarming a newer one.
    delete_armed: Option<String>,
    delete_arm_generation: u64,
    /// Kept in step by the player observer rather than re-read in `render`.
    live: Live,
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
        // Going offline hides the form: a cursor on one of its targets
        // would sit on nothing while Enter still fired.
        cx.observe(&session, |this: &mut Self, _, cx| {
            let count = this.station_count(cx) + this.form_targets(cx);
            if this.vi_cursor.is_some_and(|i| i >= count) {
                this.vi_cursor = None;
            }
            cx.notify();
        })
        .detach();
        // PlayerState notifies on every event, position ticks included; only a
        // change to what `Live` holds shows up here.
        cx.observe(&player, |this: &mut Self, player, cx| {
            let now = Live::read(&player, cx);
            if now != this.live {
                this.live = now;
                cx.notify();
            }
        })
        .detach();
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Station name"));
        let url_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Stream URL (https://…)"));
        let homepage_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Homepage (optional)"));
        // Enter in any field sends the form, as the Add/Save button does.
        for input in [&name_input, &url_input, &homepage_input] {
            cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(window, cx);
                }
            })
            .detach();
        }
        let live = Live::read(&player, cx);
        Self {
            radio,
            player,
            session,
            name_input,
            url_input,
            homepage_input,
            editing: None,
            form_error: None,
            saving: false,
            delete_armed: None,
            delete_arm_generation: 0,
            live,
            vi_cursor: None,
        }
    }

    /// Send the form: add a station, or save the one being edited. The form
    /// empties only once the server has taken it.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
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
        self.form_error = None;
        self.saving = true;
        let editing = self.editing.clone();
        let sent = editing.clone();
        let task = self.radio.update(cx, |r, cx| match sent {
            Some(id) => r.update(id, form.name, form.stream_url, form.homepage_url, cx),
            None => r.create(form.name, form.stream_url, form.homepage_url, cx),
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let saved = task.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.saving = false;
                // Only the form that was sent: Edit on another station while
                // this was out has put something else in the fields.
                if saved && view.editing == editing {
                    view.reset_form(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
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

    /// First press arms, second deletes; the arm lapses after `DELETE_ARM`.
    fn press_delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.delete_armed.as_deref() == Some(id.as_str()) {
            self.delete_armed = None;
            // Saving an edit to a station that is gone would fail on the server.
            if self.editing.as_deref() == Some(id.as_str()) {
                self.reset_form(window, cx);
            }
            self.radio.update(cx, |r, cx| r.delete(id, cx)).detach();
            cx.notify();
            return;
        }
        self.delete_armed = Some(id);
        self.delete_arm_generation += 1;
        let generation = self.delete_arm_generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DELETE_ARM).await;
            let _ = this.update(cx, |view, cx| {
                if view.delete_arm_generation == generation {
                    view.delete_armed = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A click (or Enter) on station `i`: tune in, or — on the station
    /// already live — pause/resume it instead of starting it over.
    fn press_station(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some((name, url)) = self
            .radio
            .read(cx)
            .stations
            .get(i)
            .map(|s| (s.name.clone(), s.stream_url.clone()))
        else {
            return;
        };
        self.player.update(cx, |p, cx| {
            if p.radio_stream_url() == Some(url.as_str()) {
                p.toggle_play(cx);
            } else {
                p.play_radio(name, url, cx);
            }
        });
    }

    /// Form targets after the station rows: three inputs, the submit button,
    /// and Cancel while editing. None while offline, where the form isn't drawn.
    fn form_targets(&self, cx: &App) -> usize {
        if self.session.read(cx).client.is_none() {
            return 0;
        }
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
        // The form targets follow the station rows, so with a server there
        // is always something to land on even with no stations.
        let count = self.station_count(cx) + self.form_targets(cx);
        if count == 0 {
            return;
        }
        let cur = self.vi_cursor.unwrap_or(0);
        let next = if delta > 0 {
            (cur + delta as usize).min(count - 1)
        } else {
            cur.saturating_sub(delta.unsigned_abs())
        };
        self.vi_cursor = Some(next);
        cx.notify();
    }

    /// Enter on the focused station/form target: play (or pause) the station,
    /// focus the pinned input, or send the form.
    pub fn vi_activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.station_count(cx);
        let Some(i) = self.vi_cursor else {
            return;
        };
        if i < n {
            self.press_station(i, cx);
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

    /// What stands in for the list when it has no rows.
    fn empty_note(&self, cx: &App) -> Option<&'static str> {
        let r = self.radio.read(cx);
        if !r.stations.is_empty() || r.error.is_some() {
            return None;
        }
        Some(if self.session.read(cx).client.is_none() {
            "Connect to a server to see its radio stations."
        } else if r.loading {
            "Loading stations…"
        } else {
            "No stations yet. Add one below by its stream URL — stations are \
             saved on your server, so every client connected to it sees them."
        })
    }
}

impl Render for RadioView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self.live.clone();
        let (stations, error) = {
            let r = self.radio.read(cx);
            (r.stations.clone(), r.error.clone())
        };
        let empty_note = self.empty_note(cx);
        let connected = self.session.read(cx).client.is_some();

        let editing = self.editing.is_some();
        let glow = self.session.read(cx).settings.selection_glow_vi;
        let hover_glow = self.session.read(cx).settings.selection_glow_hover;
        // The form targets sit after the station rows in the same flat vi
        // index space `vi_move`/`vi_activate` walk.
        let station_count = stations.len();
        let editing_name = self
            .editing
            .as_deref()
            .and_then(|id| stations.iter().find(|s| s.id == id))
            .map(|s| s.name.clone());
        let rows: Vec<_> = stations
            .into_iter()
            .enumerate()
            .map(|(i, station)| {
                let status = row_status(&live, &station.stream_url);
                let detail = row_detail(status, live.track.as_deref(), &station.stream_url);
                let id = station.id.clone();
                let homepage = station.home_page_url.clone().filter(|u| is_web_url(u));
                let being_edited = self.editing.as_deref() == Some(station.id.as_str());
                let armed = self.delete_armed.as_deref() == Some(station.id.as_str());
                let focused = self.vi_cursor == Some(i);
                let accent = cx.theme().primary;
                let muted = cx.theme().muted_foreground;
                let lead = match status {
                    Some(RowStatus::OnAir) => {
                        crate::ui::live_badge("radio-row-live", accent, None, cx)
                    }
                    Some(RowStatus::Paused) => div()
                        .flex_none()
                        .text_color(muted)
                        .child(app_icon(icons::PAUSE).small())
                        .into_any_element(),
                    Some(RowStatus::Connecting | RowStatus::Reconnecting(_)) => div()
                        .flex_none()
                        .text_color(accent)
                        .child(app_icon(icons::RADIO).small())
                        .with_animation(
                            ("radio-row-wait", i),
                            Animation::new(Duration::from_millis(900))
                                .repeat()
                                .with_easing(pulsating_between(0.35, 1.0)),
                            |el, t| el.opacity(t),
                        )
                        .into_any_element(),
                    None => div()
                        .flex_none()
                        .text_color(muted)
                        .child(app_icon(icons::RADIO).small())
                        .into_any_element(),
                };
                let row = h_flex()
                    .id(("radio", i))
                    .px_2()
                    .py_1p5()
                    .gap_3()
                    .items_center()
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
                    .on_click(cx.listener(move |this, _, _, cx| this.press_station(i, cx)))
                    .child(lead)
                    // Name over its detail line, so a long announced title
                    // gets the row's width rather than a narrow slot.
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .when(status.is_some(), |d| d.text_color(accent))
                                    .child(station.name.clone()),
                            )
                            .child(div().text_xs().text_color(muted).truncate().child(detail)),
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
                            .xsmall()
                            .map(|b| {
                                if armed {
                                    b.danger().label("Delete?").tooltip("Press again to delete")
                                } else {
                                    b.ghost()
                                        .icon(Icon::new(IconName::Close))
                                        .tooltip("Delete station")
                                }
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.press_delete(id.clone(), window, cx);
                            })),
                    );
                with_focus_cursor(format!("vi-radio-{i}"), row, focused, glow, None, cx)
            })
            .collect();

        let radio = self.radio.clone();
        v_flex()
            .id("radio-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_lg().child("Internet Radio"))
                    .when(station_count > 0, |h| {
                        h.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(station_count_label(station_count)),
                        )
                    }),
            )
            .when_some(error, |this, note| {
                this.child(crate::ui::error_banner(
                    &note,
                    move |_, _, cx| radio.update(cx, |r, cx| r.reload(cx)),
                    cx,
                ))
            })
            .when_some(empty_note, |this, note| {
                this.child(
                    div()
                        .px_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(note),
                )
            })
            .child(v_flex().gap_0p5().children(rows))
            // Add/edit form. Each control is a vi target in its own right,
            // so each carries the cursor ring — without it j/k past the last
            // station leaves nothing highlighted while Enter still fires.
            .when(connected, |this| {
                this.child(
                    v_flex()
                        .mt_2()
                        .pt_3()
                        .gap_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .text_color(cx.theme().muted_foreground)
                                .child(match &editing_name {
                                    Some(name) => format!("Edit “{name}”"),
                                    None => "Add a station".into(),
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .flex_wrap()
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
                                            .loading(self.saving)
                                            .disabled(self.saving)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit(window, cx)
                                            })),
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
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Live, RowStatus, StationForm, is_web_url, row_detail, row_status, station_count_label,
        validate_station,
    };

    fn live(url: &str) -> Live {
        Live {
            url: Some(url.into()),
            playing: true,
            ..Live::default()
        }
    }

    #[test]
    fn only_the_live_stream_gets_a_status() {
        let on = live("https://a.b/s");
        assert_eq!(row_status(&on, "https://a.b/s"), Some(RowStatus::OnAir));
        assert_eq!(row_status(&on, "https://a.b/other"), None);
        assert_eq!(row_status(&Live::default(), "https://a.b/s"), None);
    }

    #[test]
    fn reconnecting_outranks_buffering_and_pause() {
        let mut l = live("u");
        l.buffering = true;
        assert_eq!(row_status(&l, "u"), Some(RowStatus::Connecting));
        l.reconnect = Some(2);
        l.playing = false;
        assert_eq!(row_status(&l, "u"), Some(RowStatus::Reconnecting(2)));
        let mut paused = live("u");
        paused.playing = false;
        assert_eq!(row_status(&paused, "u"), Some(RowStatus::Paused));
    }

    #[test]
    fn row_detail_prefers_state_then_track_then_url() {
        assert_eq!(row_detail(None, Some("A — B"), "u"), "u");
        assert_eq!(
            row_detail(Some(RowStatus::OnAir), Some("A — B"), "u"),
            "A — B"
        );
        assert_eq!(row_detail(Some(RowStatus::OnAir), None, "u"), "u");
        assert_eq!(row_detail(Some(RowStatus::Paused), None, "u"), "Paused");
        assert_eq!(
            row_detail(Some(RowStatus::Paused), Some("A — B"), "u"),
            "Paused · A — B"
        );
        assert_eq!(
            row_detail(Some(RowStatus::Connecting), Some("x"), "u"),
            "Connecting…"
        );
        assert!(row_detail(Some(RowStatus::Reconnecting(1)), None, "u").contains("(1/5)"));
    }

    #[test]
    fn station_counts_read_naturally() {
        assert_eq!(station_count_label(1), "1 station");
        assert_eq!(station_count_label(3), "3 stations");
    }

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
