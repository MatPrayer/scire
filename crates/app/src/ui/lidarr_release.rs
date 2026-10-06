//! *Change release* on a library album page: the editions (MusicBrainz
//! releases) Lidarr knows for the album, and switching the one it monitors.
//!
//! Lidarr matches an album's files against its monitored release, and a
//! search only accepts that edition once `anyReleaseOk` is off — so the
//! switch writes both ([`with_release`](crate::services::lidarr::with_release)) and, by default, starts a
//! search for the new edition. The download lands through the usual import
//! path, which reloads the album page.
//!
//! [`ReleasePicker`] is a plain model; `RootView` owns it and draws it as a
//! modal with [`render`], the way it draws the song details dialog.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Disableable, Sizable, StyledExt,
    button::{Button, ButtonVariants},
    h_flex,
    switch::Switch,
    v_flex,
};

use crate::services::lidarr::{Album, AlbumRelease, Lookup};
use crate::services::runtime;
use crate::state::lidarr::lidarr as lidarr_state;

enum Phase {
    Finding,
    /// Lidarr doesn't track the album (or none of its artists).
    NotTracked,
    Failed(String),
    Ready,
}

pub struct ReleasePicker {
    /// The library album's title, for the header and the lookup.
    pub title: String,
    /// Credited artists, tried in order.
    artists: Vec<String>,
    phase: Phase,
    album: Option<Lookup<Album>>,
    /// Monitored release first.
    releases: Vec<AlbumRelease>,
    picked: Option<i64>,
    /// Start Lidarr's search for the new edition once switched.
    search_after: bool,
    saving: bool,
    error: Option<String>,
    /// What the last switch did, shown under the list.
    done: Option<String>,
}

pub enum ReleasePickerEvent {
    OpenLidarrAlbum(i64),
}

impl EventEmitter<ReleasePickerEvent> for ReleasePicker {}

impl ReleasePicker {
    pub fn new(artists: Vec<String>, title: String, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            title,
            artists,
            phase: Phase::Finding,
            album: None,
            releases: Vec::new(),
            picked: None,
            search_after: true,
            saving: false,
            error: None,
            done: None,
        };
        this.find(cx);
        this
    }

    fn find(&mut self, cx: &mut Context<Self>) {
        let Some(client) = lidarr_state(cx).read(cx).client.clone() else {
            self.phase = Phase::Failed("Lidarr is not connected.".into());
            return;
        };
        self.phase = Phase::Finding;
        let artists = self.artists.clone();
        let title = self.title.clone();
        cx.spawn(async move |this, cx| {
            let found =
                runtime::spawn_io(async move { client.find_album(&artists, &title).await }).await;
            let _ = this.update(cx, |this, cx| {
                this.phase = match found {
                    Ok(Some(album)) => {
                        this.set_album(album);
                        Phase::Ready
                    }
                    Ok(None) => Phase::NotTracked,
                    Err(e) => Phase::Failed(crate::errors::error_text(&e)),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_album(&mut self, album: Lookup<Album>) {
        self.releases = album.releases();
        self.picked = self.current();
        self.album = Some(album);
    }

    /// The release Lidarr monitors now.
    fn current(&self) -> Option<i64> {
        self.releases.iter().find(|r| r.monitored).map(|r| r.id)
    }

    fn pick(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.saving {
            self.picked = Some(id);
            self.error = None;
            cx.notify();
        }
    }

    fn switch(&mut self, cx: &mut Context<Self>) {
        let (Some(album), Some(id)) = (self.album.clone(), self.picked) else {
            return;
        };
        if self.saving || self.current() == Some(id) {
            return;
        }
        let Some(client) = lidarr_state(cx).read(cx).client.clone() else {
            self.error = Some("Lidarr is not connected.".into());
            cx.notify();
            return;
        };
        let name = self
            .releases
            .iter()
            .find(|r| r.id == id)
            .map(AlbumRelease::name)
            .unwrap_or_default();
        let search = self.search_after;
        let album_id = album.item.id;
        self.saving = true;
        self.error = None;
        self.done = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(async move { client.set_release(&album, id).await }).await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(updated) => {
                        this.set_album(updated);
                        let lidarr = lidarr_state(cx);
                        if search {
                            lidarr
                                .update(cx, |state, cx| state.search_album(album_id, cx))
                                .detach();
                            this.done =
                                Some(format!("Lidarr now wants {name} and is searching for it."));
                        } else {
                            lidarr.update(cx, |state, cx| state.poke(cx));
                            this.done = Some(format!("Lidarr now wants {name}."));
                        }
                    }
                    Err(e) => this.error = Some(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// A click handler that runs `f` on the picker.
fn on<E: 'static>(
    picker: &Entity<ReleasePicker>,
    f: impl Fn(&mut ReleasePicker, &E, &mut Context<ReleasePicker>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let picker = picker.downgrade();
    move |e, _, cx| {
        let _ = picker.update(cx, |p, cx| f(p, e, cx));
    }
}

/// The dialog: dimmed backdrop, centred card, the editions in one scrolling
/// column. `t` is the reveal's openness; `active` false while it leaves, so a
/// closing dialog takes no clicks.
pub fn render<V: 'static>(
    picker: &Entity<ReleasePicker>,
    t: f32,
    active: bool,
    on_close: impl Fn(&mut V, &mut Context<V>) + 'static + Clone,
    window: &Window,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let p = picker.read(cx);
    let theme = cx.theme().clone();
    let current = p.current();
    let muted = |text: String| {
        div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(text)
    };

    let body = match &p.phase {
        Phase::Finding => muted("Looking the album up in Lidarr…".into()).into_any_element(),
        Phase::NotTracked => muted(
            "Lidarr doesn't track this album. Add it from the search palette's Lidarr row \
             to choose an edition."
                .into(),
        )
        .into_any_element(),
        Phase::Failed(why) => v_flex()
            .gap_2()
            .items_start()
            .child(div().text_sm().text_color(theme.danger).child(why.clone()))
            .child(
                Button::new("release-retry")
                    .small()
                    .label("Retry")
                    .when(active, |b| b.on_click(on(picker, |p, _, cx| p.find(cx)))),
            )
            .into_any_element(),
        Phase::Ready if p.releases.is_empty() => {
            muted("Lidarr lists no editions for this album yet.".into()).into_any_element()
        }
        Phase::Ready => v_flex()
            .gap_1()
            .children(p.releases.iter().enumerate().map(|(i, release)| {
                let id = release.id;
                let picked = p.picked == Some(id);
                let summary = release.summary();
                h_flex()
                    .id(("release-row", i))
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if picked { theme.primary } else { theme.border })
                    .when(picked, |this| this.bg(theme.accent))
                    .when(!picked, |this| this.hover(|s| s.bg(theme.accent)))
                    .cursor_pointer()
                    .when(active, |this| {
                        this.on_click(on(picker, move |p, _, cx| p.pick(id, cx)))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .truncate()
                                    .child(release.name()),
                            )
                            .when(!summary.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(summary),
                                )
                            }),
                    )
                    .when(current == Some(id), |this| {
                        this.child(
                            div()
                                .flex_none()
                                .px_2()
                                .rounded_md()
                                .bg(theme.secondary)
                                .text_xs()
                                .child("Current"),
                        )
                    })
            }))
            .into_any_element(),
    };

    let ready = matches!(p.phase, Phase::Ready) && !p.releases.is_empty();
    let can_switch = ready && !p.saving && p.picked.is_some() && p.picked != current;
    let album_id = p.album.as_ref().map(|a| a.item.id);
    let footer = ready.then(|| {
        v_flex()
            .gap_2()
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "Lidarr matches the album's files to the edition you choose; \
                         tracks they don't cover show as missing until a download of \
                         that edition replaces them.",
            ))
            .when_some(p.done.clone(), |this, done| {
                this.child(div().text_sm().child(done))
            })
            .when_some(p.error.clone(), |this, why| {
                this.child(div().text_sm().text_color(theme.danger).child(why))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Switch::new("release-search-after")
                            .checked(p.search_after)
                            .small()
                            .when(active, |s| {
                                s.on_click(on(picker, |p, checked: &bool, cx| {
                                    p.search_after = *checked;
                                    cx.notify();
                                }))
                            }),
                    )
                    .child(div().flex_1().text_sm().child("Search for it"))
                    .when_some(album_id, |this, id| {
                        this.child(
                            Button::new("release-open-lidarr")
                                .ghost()
                                .small()
                                .label("Lidarr page")
                                .when(active, |b| {
                                    b.on_click(on(picker, move |_, _, cx| {
                                        cx.emit(ReleasePickerEvent::OpenLidarrAlbum(id))
                                    }))
                                }),
                        )
                    })
                    .child(
                        Button::new("release-switch")
                            .primary()
                            .small()
                            .label(if p.saving {
                                "Switching…"
                            } else {
                                "Switch release"
                            })
                            .disabled(!can_switch)
                            .when(active, |b| b.on_click(on(picker, |p, _, cx| p.switch(cx)))),
                    ),
            )
    });

    let title = p.title.clone();
    crate::ui::dialog(
        "release",
        560.,
        "Change release",
        title,
        t,
        active,
        on_close,
        body,
        footer.map(IntoElement::into_any_element),
        None,
        window,
        cx,
    )
}

/// The picker for a library album, from what the album page holds.
pub fn open(artists: Vec<String>, title: String, cx: &mut App) -> Entity<ReleasePicker> {
    cx.new(|cx| ReleasePicker::new(artists, title, cx))
}
