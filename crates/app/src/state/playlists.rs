//! Shared playlists state: the sidebar list and add-to-playlist menus all
//! observe this entity.

use gpui::{Context, Entity};
use subsonic::{Playlist, SubsonicClient};

use crate::services::runtime;
use crate::state::session::Session;

pub struct PlaylistsState {
    session: Entity<Session>,
    pub playlists: Vec<Playlist>,
    pub error: Option<String>,
}

impl PlaylistsState {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        // Reload when the session (re)connects.
        cx.observe(&session, |this: &mut Self, _, cx| this.reload(cx))
            .detach();
        let mut this = Self {
            session,
            playlists: Vec::new(),
            error: None,
        };
        this.reload(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            self.playlists.clear();
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client.get_playlists().await.map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok(playlists) => state.playlists = playlists,
                    Err(e) => state.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Create a playlist (optionally with initial songs), then reload.
    pub fn create(&mut self, name: String, song_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let ids: Vec<&str> = song_ids.iter().map(String::as_str).collect();
                client
                    .create_playlist(&name, &ids)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Err(e) = result {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                } else {
                    state.reload(cx);
                }
            });
        })
        .detach();
    }

    /// Add one song to a playlist, then reload (song counts change).
    pub fn add_song(&mut self, playlist_id: String, song_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .update_playlist(&playlist_id, None, &[&song_id], &[])
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Err(e) = result {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                } else {
                    state.reload(cx);
                }
            });
        })
        .detach();
    }

    pub fn rename(&mut self, playlist_id: String, new_name: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .update_playlist(&playlist_id, Some(&new_name), &[], &[])
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Err(e) = result {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                } else {
                    state.reload(cx);
                }
            });
        })
        .detach();
    }

    pub fn delete(&mut self, playlist_id: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .delete_playlist(&playlist_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Err(e) = result {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                } else {
                    state.reload(cx);
                }
            });
        })
        .detach();
    }

    /// Remove songs at the given playlist positions, then reload.
    pub fn remove_songs(&mut self, playlist_id: String, indices: Vec<u32>, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .update_playlist(&playlist_id, None, &[], &indices)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                if let Err(e) = result {
                    state.error = Some(format!("{e:#}"));
                    cx.notify();
                } else {
                    state.reload(cx);
                }
            });
        })
        .detach();
    }
}

pub fn init(session: Entity<Session>, cx: &mut gpui::App) -> Entity<PlaylistsState> {
    use gpui::AppContext as _;
    cx.new(|cx| PlaylistsState::new(session, cx))
}
