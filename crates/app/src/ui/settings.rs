//! Settings: window chrome, theme, playback, streaming, storage, account.

use gpui::{Context, Entity, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use crate::config::{SpotifyApiConfig, ThemePref};
use crate::services::artwork;
use crate::state::player::PlayerState;
use crate::state::queue::RepeatMode;
use crate::state::session::Session;
use crate::ui::{apply_theme, apply_window_chrome};

const FORMATS: &[(&str, Option<&str>)] = &[
    ("Original", None),
    ("MP3", Some("mp3")),
    ("Opus", Some("opus")),
];
const BITRATES: &[(&str, Option<u32>)] = &[
    ("No limit", None),
    ("128k", Some(128)),
    ("192k", Some(192)),
    ("320k", Some(320)),
];
const CACHE_SIZES_MB: &[u32] = &[64, 128, 256, 512, 1024];

pub struct SettingsView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    spotify_client_id: Entity<InputState>,
    spotify_client_secret: Entity<InputState>,
}

impl SettingsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let spotify_client_id = cx.new(|cx| InputState::new(window, cx));
        let spotify_client_secret = cx.new(|cx| InputState::new(window, cx).masked(true));
        let this = Self {
            session: session.clone(),
            player,
            spotify_client_id: spotify_client_id.clone(),
            spotify_client_secret: spotify_client_secret.clone(),
        };
        let initial_id = session.read(cx).settings.spotify.client_id.clone();
        let initial_secret = session.read(cx).settings.spotify.client_secret.clone();
        if let Some(value) = initial_id {
            spotify_client_id.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        if let Some(value) = initial_secret {
            spotify_client_secret.update(cx, |input, cx| {
                input.set_value(value, window, cx)
            });
        }
        this
    }

    fn persist(&self, cx: &Context<Self>) {
        self.session.read(cx).persist_settings();
    }

    fn set_theme(&mut self, pref: ThemePref, window: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.theme = pref);
        self.persist(cx);
        apply_theme(pref, window, cx);
        cx.notify();
    }

    fn apply_transcoding(&mut self, cx: &mut Context<Self>) {
        let tc = self.session.read(cx).settings.transcoding.clone();
        self.player
            .update(cx, |p, _| p.set_transcoding(tc.to_stream_options()));
        self.persist(cx);
        cx.notify();
    }

    fn set_format(&mut self, format: Option<&str>, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            s.settings.transcoding.format = format.map(String::from);
        });
        self.apply_transcoding(cx);
    }

    fn set_bitrate(&mut self, rate: Option<u32>, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            s.settings.transcoding.max_bit_rate = rate;
        });
        self.apply_transcoding(cx);
    }

    fn set_client_titlebar(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.client_titlebar = enabled);
        self.persist(cx);
        apply_window_chrome(enabled, window, cx);
        cx.notify();
    }

    fn set_scrobble(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.scrobble_enabled = enabled);
        self.player
            .update(cx, |p, cx| p.set_scrobble_enabled(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_default_shuffle(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.default_shuffle = enabled);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, enabled, p.queue.repeat, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_default_repeat(&mut self, mode: RepeatMode, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.default_repeat = mode);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        let shuffle = self.session.read(cx).settings.default_shuffle;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, shuffle, mode, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_cache_cap(&mut self, mb: u32, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.artwork_cache_mb = mb);
        artwork::set_cache_cap_mb(mb);
        self.persist(cx);
        cx.notify();
    }

    fn set_spotify_config(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let client_id = self.spotify_client_id.read(cx).value().trim().to_string();
        let client_secret = self.spotify_client_secret.read(cx).value().to_string();
        self.session.update(cx, |s, _| {
            s.settings.spotify = SpotifyApiConfig {
                enabled,
                client_id: (!client_id.is_empty()).then_some(client_id),
                client_secret: (!client_secret.is_empty()).then_some(client_secret),
            };
        });
        self.persist(cx);
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.session.update(cx, |s, cx| s.logout(cx));
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (
            theme,
            format,
            bitrate,
            client_titlebar,
            scrobble_enabled,
            default_shuffle,
            default_repeat,
            artwork_cache_mb,
            spotify_config,
            account,
        ) = {
            let s = &self.session.read(cx).settings;
            (
                s.theme,
                s.transcoding.format.clone(),
                s.transcoding.max_bit_rate,
                s.client_titlebar,
                s.scrobble_enabled,
                s.default_shuffle,
                s.default_repeat,
                s.artwork_cache_mb,
                s.spotify.clone(),
                s.server
                    .as_ref()
                    .map(|srv| (srv.url.clone(), srv.username.clone())),
            )
        };

        let theme_btn = |label: &'static str, pref: ThemePref, active: bool| {
            Button::new(label)
                .label(label)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, window, cx| this.set_theme(pref, window, cx)))
        };

        let format_btn = |item: &'static (&'static str, Option<&'static str>), active: bool| {
            Button::new(item.0)
                .label(item.0)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_format(item.1, cx)))
        };

        let bitrate_btn = |item: &'static (&'static str, Option<u32>), active: bool| {
            Button::new(item.0)
                .label(item.0)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_bitrate(item.1, cx)))
        };

        let repeat_btn = |label: &'static str, mode: RepeatMode, active: bool| {
            Button::new(label)
                .label(label)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_default_repeat(mode, cx)))
        };

        let cache_btn = |mb: u32, active: bool| {
            Button::new(("cache-mb", mb))
                .label(format!("{mb} MB"))
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_cache_cap(mb, cx)))
        };

        v_flex()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(640.))
                    .mx_auto()
                    .gap_4()
                    .child(div().text_lg().font_semibold().child("Settings"))
                    // Window
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Window"))
                            .child(
                                Switch::new("client-titlebar")
                                    .checked(client_titlebar)
                                    .label("Use in-app title bar")
                                    .on_click(cx.listener(|this, &checked, window, cx| {
                                        this.set_client_titlebar(checked, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Disable to use your desktop environment's native window \
                                         decorations (recommended on some Linux setups).",
                                    ),
                            ),
                    )
                    // Appearance
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Appearance"))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(theme_btn(
                                        "System",
                                        ThemePref::System,
                                        theme == ThemePref::System,
                                    ))
                                    .child(theme_btn(
                                        "Light",
                                        ThemePref::Light,
                                        theme == ThemePref::Light,
                                    ))
                                    .child(theme_btn(
                                        "Dark",
                                        ThemePref::Dark,
                                        theme == ThemePref::Dark,
                                    ))
                                    .child(theme_btn(
                                        "Custom (themes.json)",
                                        ThemePref::Custom,
                                        theme == ThemePref::Custom,
                                    )),
                            ),
                    )
                    // Playback
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Playback"))
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        Switch::new("scrobble")
                                            .checked(scrobble_enabled)
                                            .label("Scrobble plays to server")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_scrobble(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("default-shuffle")
                                            .checked(default_shuffle)
                                            .label("Shuffle on by default")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_default_shuffle(checked, cx);
                                            })),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Default repeat"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(repeat_btn(
                                                "Off",
                                                RepeatMode::Off,
                                                default_repeat == RepeatMode::Off,
                                            ))
                                            .child(repeat_btn(
                                                "All",
                                                RepeatMode::All,
                                                default_repeat == RepeatMode::All,
                                            ))
                                            .child(repeat_btn(
                                                "One",
                                                RepeatMode::One,
                                                default_repeat == RepeatMode::One,
                                            )),
                                    ),
                            ),
                    )
                    // Streaming
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Streaming"))
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Format"),
                                    )
                                    .child(h_flex().gap_2().children(FORMATS.iter().map(|item| {
                                        let active = format.as_deref() == item.1;
                                        format_btn(item, active).into_any_element()
                                    }))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Transcoding helps low-bandwidth connections but disables \
                                         accurate seeking. Original streams the source file.",
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Max bitrate"),
                                    )
                                    .child(h_flex().gap_2().children(BITRATES.iter().map(|item| {
                                        let active = bitrate == item.1;
                                        bitrate_btn(item, active).into_any_element()
                                    }))),
                            ),
                    )
                    // Storage
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Storage"))
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Artwork cache limit"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .flex_wrap()
                                            .children(CACHE_SIZES_MB.iter().map(|&mb| {
                                                cache_btn(mb, artwork_cache_mb == mb)
                                                    .into_any_element()
                                            })),
                                    ),
                            ),
                    )
                    // Spotify
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Spotify API"))
                            .child(
                                Switch::new("spotify-enabled")
                                    .checked(spotify_config.enabled)
                                    .label("Enable Spotify artist enrichment")
                                    .on_click(cx.listener(|this, &checked, _, cx| {
                                        this.set_spotify_config(checked, cx);
                                    })),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Client ID"),
                                    )
                                    .child(div().w_full().child(Input::new(&self.spotify_client_id))),
                            )
                            .child(
                                v_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Client secret"),
                                    )
                                    .child(div().w_full().child(Input::new(&self.spotify_client_secret))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Store credentials here to enrich artist pages with Spotify metadata.",
                                    ),
                            ),
                    )
                    // Account
                    .when_some(account, |this, (url, user)| {
                        this.child(
                            v_flex()
                                .gap_3()
                                .p_4()
                                .rounded_lg()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().sidebar)
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_start()
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .min_w_0()
                                                .child(div().text_sm().font_medium().child("Account"))
                                                .child(div().text_sm().child(user))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(cx.theme().muted_foreground)
                                                        .truncate()
                                                        .child(url),
                                                ),
                                        )
                                        .child(
                                            Button::new("sign-out")
                                                .outline()
                                                .danger()
                                                .label("Sign out")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.sign_out(cx);
                                                })),
                                        ),
                                ),
                        )
                    }),
            )
    }
}
