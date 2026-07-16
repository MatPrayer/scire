//! Login form: server URL, username, password.

use gpui::{Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, prelude::*};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::state::session::{ConnectionStatus, Session};

pub struct LoginView {
    session: Entity<Session>,
    url: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    focus_handle: FocusHandle,
}

impl LoginView {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = session.read(cx).settings.server.clone();
        let url = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("https://music.example.com");
            if let Some(server) = &saved {
                s.set_value(server.url.clone(), window, cx);
            }
            s
        });
        let username = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("username");
            if let Some(server) = &saved {
                s.set_value(server.username.clone(), window, cx);
            }
            s
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("password")
                .masked(true)
        });

        // Enter in the password field submits.
        cx.subscribe(&password, |this: &mut Self, _, event, cx| {
            if matches!(event, gpui_component::input::InputEvent::PressEnter { .. }) {
                this.submit(cx);
            }
        })
        .detach();

        // Re-render on connection status changes.
        cx.observe(&session, |_, _, cx| cx.notify()).detach();

        Self {
            session,
            url,
            username,
            password,
            focus_handle: cx.focus_handle(),
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().trim().to_string();
        let username = self.username.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        if url.is_empty() || username.is_empty() || password.is_empty() {
            return;
        }
        self.session.update(cx, |session, cx| {
            session.connect(url, username, password, true, cx);
        });
    }
}

impl Focusable for LoginView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LoginView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.session.read(cx).status.clone();
        let connecting = status == ConnectionStatus::Connecting;
        let error = match &status {
            ConnectionStatus::Failed(msg) => Some(msg.clone()),
            _ => None,
        };

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .w(gpui::px(380.))
                    .p_6()
                    .gap_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().sidebar)
                    .shadow_lg()
                    .child(div().text_xl().child("Navidrome"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .mb_2()
                            .child("Connect to your music server"),
                    )
                    .child(Input::new(&self.url))
                    .child(Input::new(&self.username))
                    .child(Input::new(&self.password))
                    .when_some(error, |this, msg| {
                        this.child(div().text_color(cx.theme().danger).text_sm().child(msg))
                    })
                    .child(
                        h_flex().justify_end().mt_2().child(
                            Button::new("connect")
                                .primary()
                                .label("Connect")
                                .loading(connecting)
                                .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                        ),
                    ),
            )
    }
}
