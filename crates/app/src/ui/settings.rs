//! Settings: window chrome, theme, playback, streaming, storage, account.

use gpui::{App, Context, Entity, IntoElement, Render, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::config::{
    CoverSize, DefaultPage, FullscreenBackground, QueueEndBehavior, ReplayGainMode, ThemePref,
};
use crate::services::library_db::LibraryDb;
use crate::services::{artwork, navidrome_sync, runtime};
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
const PAGES: &[(&str, DefaultPage)] = &[
    ("Albums", DefaultPage::Albums),
    ("Artists", DefaultPage::Artists),
    ("Favorites", DefaultPage::Favorites),
    ("Recent", DefaultPage::Recent),
    ("Radio", DefaultPage::Radio),
];
const COVER_SIZES: &[(&str, CoverSize)] = &[
    ("Small", CoverSize::Small),
    ("Medium", CoverSize::Medium),
    ("Large", CoverSize::Large),
    ("Extra large", CoverSize::ExtraLarge),
];

/// How often the two library maintenance jobs republish their progress.
const LIBRARY_TASK_POLL: Duration = Duration::from_millis(500);

/// State of one of the maintenance jobs in the Library section.
///
/// Both are long, both can fail in ways the user needs told about (a server
/// rescan is admin-only on Navidrome), and neither has a meaningful total — so
/// they report a status line rather than a bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TaskState {
    #[default]
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

impl TaskState {
    fn is_running(&self) -> bool {
        matches!(self, TaskState::Running(_))
    }

    fn message(&self) -> Option<&str> {
        match self {
            TaskState::Idle => None,
            TaskState::Running(m) | TaskState::Done(m) | TaskState::Failed(m) => Some(m),
        }
    }
}

pub struct SettingsView {
    session: Entity<Session>,
    player: Entity<PlayerState>,
    dir_input: Entity<InputState>,
    library_db: Arc<LibraryDb>,
    server_scan: TaskState,
    rebuild: TaskState,
}

/// One maintenance job: description, its button, and whatever it last reported.
///
/// Stacked rather than description-left / button-right: every other setting in
/// this pane puts its explanation above its controls, and a flex-1 text column
/// beside a button left the two on different baselines.
fn library_task_row(
    id: &'static str,
    label: &'static str,
    description: &'static str,
    state: &TaskState,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<SettingsView>,
) -> impl IntoElement {
    let running = state.is_running();
    let message = state.message().map(|m| m.to_string());
    let failed = matches!(state, TaskState::Failed(_));
    v_flex()
        .gap_1p5()
        .items_start()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
        // Wrapped so the button keeps its natural width: a bare child of a
        // column stretches to the full row.
        .child(
            h_flex().child(
                Button::new(id)
                    .outline()
                    .small()
                    .label(label)
                    .disabled(running)
                    .on_click(on_click),
            ),
        )
        .when_some(message, |this, message| {
            this.child(
                div()
                    .text_xs()
                    .text_color(if failed {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(message),
            )
        })
}

impl SettingsView {
    pub fn new(
        session: Entity<Session>,
        player: Entity<PlayerState>,
        library_db: Arc<LibraryDb>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&session, |_, _, cx| cx.notify()).detach();
        let dir_input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/music"));
        Self {
            session,
            player,
            dir_input,
            library_db,
            server_scan: TaskState::default(),
            rebuild: TaskState::default(),
        }
    }

    /// Ask Navidrome to walk its music directories.
    ///
    /// Kept out of the sidebar's Refresh: this is the server reading every file
    /// it owns, which is minutes of work and only needed when files have
    /// actually been added to disk. Refresh reconciles against what the server
    /// already knows and is the one to reach for otherwise.
    fn scan_server(&mut self, cx: &mut Context<Self>) {
        if self.server_scan.is_running() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            self.server_scan = TaskState::Failed("Not connected to a server".into());
            cx.notify();
            return;
        };
        self.server_scan = TaskState::Running("Starting scan…".into());
        cx.notify();

        let files = Arc::new(AtomicU64::new(0));
        let watched = files.clone();
        cx.spawn(async move |this, cx| {
            let work =
                runtime::spawn_io(
                    async move { navidrome_sync::run_server_scan(&client, files).await },
                );
            let result = crate::ui::poll_until_done(cx, LIBRARY_TASK_POLL, work, |cx| {
                let seen = watched.load(Ordering::Relaxed);
                let _ = this.update(cx, |this, cx| {
                    this.server_scan = TaskState::Running(if seen == 0 {
                        "Scanning…".into()
                    } else {
                        format!("Scanning… {seen} files")
                    });
                    cx.notify();
                });
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.server_scan = match result {
                    Ok(count) => TaskState::Done(format!(
                        "Server scan finished ({count} files). Refresh to pick up new albums."
                    )),
                    // Most often error 50: Navidrome only lets admins start a
                    // scan. Say what happened rather than failing silently.
                    Err(e) => TaskState::Failed(format!("Scan failed: {e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Throw away the cached catalog and re-import every album from scratch.
    ///
    /// The escape hatch behind the incremental refresh: that one re-fetches an
    /// album's tracks when the listing's track count or duration moves, so an
    /// album re-tagged without either changing is the case it cannot see.
    fn rebuild_cache(&mut self, cx: &mut Context<Self>) {
        if self.rebuild.is_running() {
            return;
        }
        let Some(client) = self.session.read(cx).client.clone() else {
            self.rebuild = TaskState::Failed("Not connected to a server".into());
            cx.notify();
            return;
        };
        self.rebuild = TaskState::Running("Reading catalog…".into());
        cx.notify();

        let db = self.library_db.clone();
        let progress = Arc::new(navidrome_sync::SyncProgress::default());
        let watched = progress.clone();
        cx.spawn(async move |this, cx| {
            let work = runtime::spawn_io(async move {
                navidrome_sync::sync_navidrome(
                    db,
                    &client,
                    None,
                    progress,
                    navidrome_sync::SyncMode::Full,
                )
                .await
            });
            let result = crate::ui::poll_until_done(cx, LIBRARY_TASK_POLL, work, |cx| {
                let (done, total) = watched.snapshot();
                let _ = this.update(cx, |this, cx| {
                    this.rebuild = TaskState::Running(if total == 0 {
                        "Reading catalog…".into()
                    } else {
                        format!("Importing {done}/{total} albums")
                    });
                    cx.notify();
                });
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.rebuild = match result {
                    Ok(()) => TaskState::Done("Library cache rebuilt.".into()),
                    Err(e) => TaskState::Failed(format!("Rebuild failed: {e}")),
                };
                cx.notify();
            });
        })
        .detach();
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
        self.session
            .update(cx, |s, _| s.settings.client_titlebar = enabled);
        self.persist(cx);
        apply_window_chrome(enabled, window, cx);
        cx.notify();
    }

    fn set_scrobble(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.scrobble_enabled = enabled);
        self.player
            .update(cx, |p, cx| p.set_scrobble_enabled(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_resume_playback(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.resume_playback = enabled);
        self.player
            .update(cx, |p, cx| p.set_resume_playback(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_default_shuffle(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_shuffle = enabled);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, enabled, p.queue.repeat, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_default_repeat(&mut self, mode: RepeatMode, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_repeat = mode);
        let scrobble = self.session.read(cx).settings.scrobble_enabled;
        let shuffle = self.session.read(cx).settings.default_shuffle;
        self.player.update(cx, |p, cx| {
            p.apply_playback_settings(scrobble, shuffle, mode, cx);
        });
        self.persist(cx);
        cx.notify();
    }

    fn add_local_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.dir_input.read(cx).value().to_string();
        let trimmed = path.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.session.update(cx, |s, _| {
            if !s
                .settings
                .local_music_dirs
                .iter()
                .any(|p| p.to_string_lossy() == trimmed.as_str())
            {
                s.settings
                    .local_music_dirs
                    .push(std::path::PathBuf::from(&trimmed));
            }
        });
        self.dir_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.persist(cx);
        cx.notify();
    }

    fn remove_local_dir(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| {
            if idx < s.settings.local_music_dirs.len() {
                s.settings.local_music_dirs.remove(idx);
            }
        });
        self.persist(cx);
        cx.notify();
    }

    fn set_cache_cap(&mut self, mb: u32, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.artwork_cache_mb = mb);
        artwork::set_cache_cap_mb(mb);
        self.persist(cx);
        cx.notify();
    }

    fn set_default_page(&mut self, page: DefaultPage, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.default_page = page);
        self.persist(cx);
        cx.notify();
    }

    fn set_cover_size(&mut self, size: CoverSize, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.cover_size = size);
        self.persist(cx);
        cx.notify();
    }

    fn set_waveform(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.waveform_seekbar = enabled);
        self.player
            .update(cx, |p, cx| p.set_waveform_enabled(enabled, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_stream_info(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.stream_info_bar = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_detailed_volume(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.detailed_volume = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_show_queue_button(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.show_queue_button = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_replay_gain(&mut self, mode: ReplayGainMode, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.replay_gain = mode);
        self.player.update(cx, |p, cx| p.set_replay_gain(mode, cx));
        self.persist(cx);
        cx.notify();
    }

    fn set_fullscreen_bg(&mut self, mode: FullscreenBackground, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.fullscreen_bg = mode);
        self.persist(cx);
        cx.notify();
    }

    fn set_fullscreen_volume(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.session
            .update(cx, |s, _| s.settings.fullscreen_volume = enabled);
        self.persist(cx);
        cx.notify();
    }

    fn set_queue_end(&mut self, mode: QueueEndBehavior, cx: &mut Context<Self>) {
        self.session.update(cx, |s, _| s.settings.queue_end = mode);
        self.player.update(cx, |p, cx| {
            p.set_clear_on_end(mode == QueueEndBehavior::Clear, cx)
        });
        self.persist(cx);
        cx.notify();
    }

    fn toggle_track_info(
        &mut self,
        toggle: fn(&mut crate::config::TrackInfo) -> &mut bool,
        cx: &mut Context<Self>,
    ) {
        self.session.update(cx, |s, _| {
            let flag = toggle(&mut s.settings.track_info);
            *flag = !*flag;
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
                s.server
                    .as_ref()
                    .map(|srv| (srv.url.clone(), srv.username.clone())),
            )
        };
        let (default_page, cover_size, track_info, waveform, stream_info, detailed_volume) = {
            let s = &self.session.read(cx).settings;
            (
                s.default_page,
                s.cover_size,
                s.track_info.clone(),
                s.waveform_seekbar,
                s.stream_info_bar,
                s.detailed_volume,
            )
        };
        let show_queue_button = self.session.read(cx).settings.show_queue_button;
        let resume_playback = self.session.read(cx).settings.resume_playback;
        let local_music_dirs = self.session.read(cx).settings.local_music_dirs.clone();
        let replay_gain = self.session.read(cx).settings.replay_gain;
        let rg_btn = |label: &'static str, mode: ReplayGainMode, active: bool| {
            Button::new(label)
                .label(label)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_replay_gain(mode, cx)))
        };
        let queue_end = self.session.read(cx).settings.queue_end;
        let qe_btn = |label: &'static str, mode: QueueEndBehavior, active: bool| {
            Button::new(label)
                .label(label)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_queue_end(mode, cx)))
        };
        let fullscreen_bg = self.session.read(cx).settings.fullscreen_bg;
        let fullscreen_volume = self.session.read(cx).settings.fullscreen_volume;
        let fsbg_btn = |label: &'static str, mode: FullscreenBackground, active: bool| {
            Button::new(label)
                .label(label)
                .when(active, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| this.set_fullscreen_bg(mode, cx)))
        };

        type InfoField = fn(&mut crate::config::TrackInfo) -> &mut bool;
        let info_fields: &[(&'static str, bool, InfoField)] = &[
            ("Artist", track_info.artist, |t| &mut t.artist),
            ("Album", track_info.album, |t| &mut t.album),
            ("Year", track_info.year, |t| &mut t.year),
            ("Genre", track_info.genre, |t| &mut t.genre),
            ("Bitrate", track_info.bitrate, |t| &mut t.bitrate),
            ("Play count", track_info.plays, |t| &mut t.plays),
        ];

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
            .pb(px(148.))
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
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
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
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Appearance"))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
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
                                        "Adaptive (from cover)",
                                        ThemePref::Adaptive,
                                        theme == ThemePref::Adaptive,
                                    ))
                                    .child(theme_btn(
                                        "Custom (theme.json)",
                                        ThemePref::Custom,
                                        theme == ThemePref::Custom,
                                    )),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Fullscreen player background"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .flex_wrap()
                                            .child(fsbg_btn(
                                                "Gradient",
                                                FullscreenBackground::Gradient,
                                                fullscreen_bg == FullscreenBackground::Gradient,
                                            ))
                                            .child(fsbg_btn(
                                                "Vibrant",
                                                FullscreenBackground::Vibrant,
                                                fullscreen_bg == FullscreenBackground::Vibrant,
                                            ))
                                            .child(fsbg_btn(
                                                "Blurred art",
                                                FullscreenBackground::BlurredArt,
                                                fullscreen_bg == FullscreenBackground::BlurredArt,
                                            ))
                                            .child(fsbg_btn(
                                                "Animated",
                                                FullscreenBackground::Animated,
                                                fullscreen_bg == FullscreenBackground::Animated,
                                            ))
                                            .child(fsbg_btn(
                                                "Solid",
                                                FullscreenBackground::Solid,
                                                fullscreen_bg == FullscreenBackground::Solid,
                                            )),
                                    )
                                    .child(
                                        Switch::new("fullscreen-volume")
                                            .checked(fullscreen_volume)
                                            .label("Volume slider in fullscreen player")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_fullscreen_volume(checked, cx);
                                            })),
                                    ),
                            ),
                    )
                    // Playback
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
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
                                        Switch::new("resume-playback")
                                            .checked(resume_playback)
                                            .label("Resume where you left off")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_resume_playback(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("default-shuffle")
                                            .checked(default_shuffle)
                                            .label("Shuffle on by default")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_default_shuffle(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("waveform-seekbar")
                                            .checked(waveform)
                                            .label("Waveform progress bar")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_waveform(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("stream-info-bar")
                                            .checked(stream_info)
                                            .label("Stream info in player bar")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_stream_info(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("detailed-volume")
                                            .checked(detailed_volume)
                                            .label("Detailed volume control")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_detailed_volume(checked, cx);
                                            })),
                                    )
                                    .child(
                                        Switch::new("show-queue-button")
                                            .checked(show_queue_button)
                                            .label("Queue button in player bar")
                                            .on_click(cx.listener(|this, &checked, _, cx| {
                                                this.set_show_queue_button(checked, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "The waveform seek bar downloads each track a \
                                                 second time to decode it, so it uses extra \
                                                 bandwidth.",
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("ReplayGain (loudness normalization)"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(rg_btn(
                                                "Off",
                                                ReplayGainMode::Off,
                                                replay_gain == ReplayGainMode::Off,
                                            ))
                                            .child(rg_btn(
                                                "Track",
                                                ReplayGainMode::Track,
                                                replay_gain == ReplayGainMode::Track,
                                            ))
                                            .child(rg_btn(
                                                "Album",
                                                ReplayGainMode::Album,
                                                replay_gain == ReplayGainMode::Album,
                                            ))
                                            .child(rg_btn(
                                                "Auto",
                                                ReplayGainMode::Auto,
                                                replay_gain == ReplayGainMode::Auto,
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "Evens out perceived volume using each file's \
                                                 ReplayGain tags. Track normalizes every song; \
                                                 Album keeps an album's relative loudness; Auto \
                                                 uses album gain when playing a whole album and \
                                                 track gain otherwise. The player bar shows the \
                                                 applied gain (and the auto-chosen mode).",
                                            ),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("When the queue ends"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(qe_btn(
                                                "Keep queue",
                                                QueueEndBehavior::Keep,
                                                queue_end == QueueEndBehavior::Keep,
                                            ))
                                            .child(qe_btn(
                                                "Clear queue",
                                                QueueEndBehavior::Clear,
                                                queue_end == QueueEndBehavior::Clear,
                                            )),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "Keep leaves the finished queue and last track in \
                                                 the player bar; Clear empties the queue and \
                                                 resets the player bar.",
                                            ),
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
                    // Browsing
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Browsing"))
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Open at startup"),
                                    )
                                    .child(h_flex().gap_2().flex_wrap().children(
                                        PAGES.iter().map(|&(label, page)| {
                                            Button::new(label)
                                                .label(label)
                                                .when(default_page == page, |b| b.primary())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.set_default_page(page, cx)
                                                }))
                                                .into_any_element()
                                        }),
                                    )),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "Album cover size — albums per row adapt to the \
                                                 window width.",
                                            ),
                                    )
                                    .child(h_flex().gap_2().flex_wrap().children(
                                        COVER_SIZES.iter().map(|&(label, size)| {
                                            Button::new(label)
                                                .label(label)
                                                .when(cover_size == size, |b| b.primary())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.set_cover_size(size, cx)
                                                }))
                                                .into_any_element()
                                        }),
                                    )),
                            )
                            .child(
                                v_flex()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                "Track info shown next to song titles in album \
                                                 and playlist views.",
                                            ),
                                    )
                                    .child(h_flex().gap_2().flex_wrap().children(
                                        info_fields.iter().map(|&(label, active, field)| {
                                            Button::new(label)
                                                .label(label)
                                                .when(active, |b| b.primary())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.toggle_track_info(field, cx)
                                                }))
                                                .into_any_element()
                                        }),
                                    )),
                            ),
                    )
                    // Streaming
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
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
                                    .child(h_flex().gap_2().children(BITRATES.iter().map(
                                        |item| {
                                            let active = bitrate == item.1;
                                            bitrate_btn(item, active).into_any_element()
                                        },
                                    ))),
                            ),
                    )
                    // Library maintenance. The everyday refresh lives in the
                    // sidebar; these two are the slow, occasional jobs.
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Library"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Refresh in the sidebar picks up albums the server already \
                                         knows about. These two are slower and rarely needed.",
                                    ),
                            )
                            .child(library_task_row(
                                "scan-server",
                                "Scan server library",
                                "Have the server re-read its music folders. Needed after adding \
                                 files to the server itself. Requires an admin account.",
                                &self.server_scan,
                                cx.listener(|this, _, _, cx| this.scan_server(cx)),
                                cx,
                            ))
                            // Two description-then-button blocks in a row read
                            // as one paragraph without something between them.
                            .child(crate::ui::divider())
                            .child(library_task_row(
                                "rebuild-cache",
                                "Rebuild local cache",
                                "Re-import every album from the server. Fixes a cache that has \
                                 drifted, e.g. after re-tagging music in place.",
                                &self.rebuild,
                                cx.listener(|this, _, _, cx| this.rebuild_cache(cx)),
                                cx,
                            )),
                    )
                    // Local Music
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                            .bg(cx.theme().sidebar)
                            .child(div().text_sm().font_medium().child("Local Music"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Directories scanned for local music files"),
                            )
                            .child(v_flex().gap_1().children(
                                local_music_dirs.iter().enumerate().map(|(i, p)| {
                                    let p_str = p.to_string_lossy().to_string();
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_sm()
                                                .truncate()
                                                .child(p_str.clone()),
                                        )
                                        .child(
                                            Button::new(("rm-local-dir", i))
                                                .ghost()
                                                .xsmall()
                                                .icon(gpui_component::Icon::new(
                                                    gpui_component::IconName::Close,
                                                ))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.remove_local_dir(i, cx);
                                                })),
                                        )
                                        .into_any_element()
                                }),
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().w(px(300.)).child(Input::new(&self.dir_input)))
                                    .child(Button::new("add-local-dir").label("Add").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.add_local_dir(window, cx);
                                        }),
                                    )),
                            ),
                    )
                    // Storage
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded_lg()
                            .border_1()
                            .border_color(gpui::hsla(0., 0., 0.5, 0.15))
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
                                    .child(h_flex().gap_2().flex_wrap().children(
                                        CACHE_SIZES_MB.iter().map(|&mb| {
                                            cache_btn(mb, artwork_cache_mb == mb).into_any_element()
                                        }),
                                    )),
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
                                .border_color(gpui::hsla(0., 0., 0.5, 0.15))
                                .bg(cx.theme().sidebar)
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_start()
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .min_w_0()
                                                .child(
                                                    div().text_sm().font_medium().child("Account"),
                                                )
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
