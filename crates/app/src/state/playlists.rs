//! Shared playlists state: the sidebar list, the playlists page and the
//! add-to-playlist menus all observe this entity.

use std::future::Future;

use gpui::{App, Context, Entity, EventEmitter};
use subsonic::{Playlist, SubsonicClient};

use crate::services::runtime;
use crate::state::session::Session;

pub enum PlaylistsEvent {
    /// A playlist made by `duplicate` exists on the server; carries its id.
    Duplicated(String),
}

pub struct PlaylistsState {
    session: Entity<Session>,
    pub playlists: Vec<Playlist>,
    pub error: Option<String>,
    /// Bumped by every edit this state makes, so a page showing one playlist
    /// can tell "something changed, re-ask" from the list merely reloading.
    pub revision: u64,
}

impl EventEmitter<PlaylistsEvent> for PlaylistsState {}

impl PlaylistsState {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        // Reload when the session (re)connects.
        cx.observe(&session, |this: &mut Self, _, cx| this.reload(cx))
            .detach();
        let mut this = Self {
            session,
            playlists: Vec::new(),
            error: None,
            revision: 0,
        };
        this.reload(cx);
        this
    }

    fn client(&self, cx: &Context<Self>) -> Option<SubsonicClient> {
        self.session.read(cx).client.clone()
    }

    /// The listing's copy of one playlist (name, owner, counts, cover).
    pub fn get(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == id)
    }

    /// Whether the signed-in user may edit `playlist`. Subsonic only lets the
    /// owner change one; a playlist with no owner field is taken as ours.
    pub fn editable(&self, playlist: &Playlist, cx: &App) -> bool {
        let me = self
            .session
            .read(cx)
            .settings
            .server
            .as_ref()
            .map(|s| s.username.as_str());
        owned_by(playlist.owner.as_deref(), me)
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
                    Err(e) => state.error = Some(crate::errors::error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run one server edit, then reload the list (counts, names and covers
    /// move) or record the failure. Every mutating call goes through here.
    fn edit<F, Fut>(&mut self, cx: &mut Context<Self>, call: F)
    where
        F: FnOnce(SubsonicClient) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(call(client)).await;
            let _ = this.update(cx, |state, cx| {
                state.revision += 1;
                match result {
                    Ok(()) => {
                        state.error = None;
                        state.reload(cx);
                    }
                    Err(e) => {
                        state.error = Some(crate::errors::error_text(&e));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Create a playlist (optional description + initial songs), then reload.
    pub fn create(
        &mut self,
        name: String,
        description: Option<String>,
        song_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.edit(cx, move |client| async move {
            let ids: Vec<&str> = song_ids.iter().map(String::as_str).collect();
            let pl = client.create_playlist(&name, &ids).await?;
            // Description is a separate updatePlaylist `comment` call.
            if let Some(comment) = description
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
            {
                client
                    .update_playlist(&pl.playlist.id, None, Some(comment), &[], &[])
                    .await?;
            }
            Ok(())
        });
    }

    /// Add one song to a playlist, then reload (song counts change).
    pub fn add_song(&mut self, playlist_id: String, song_id: String, cx: &mut Context<Self>) {
        self.add_songs(playlist_id, vec![song_id], cx);
    }

    /// Add several songs to a playlist in one call, then reload.
    pub fn add_songs(
        &mut self,
        playlist_id: String,
        song_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if song_ids.is_empty() {
            return;
        }
        self.edit(cx, move |client| async move {
            let ids: Vec<&str> = song_ids.iter().map(String::as_str).collect();
            client
                .update_playlist(&playlist_id, None, None, &ids, &[])
                .await?;
            Ok(())
        });
    }

    pub fn rename(&mut self, playlist_id: String, new_name: String, cx: &mut Context<Self>) {
        self.edit(cx, move |client| async move {
            client
                .update_playlist(&playlist_id, Some(&new_name), None, &[], &[])
                .await?;
            Ok(())
        });
    }

    /// Set the playlist's description. An empty one clears it.
    pub fn set_comment(&mut self, playlist_id: String, comment: String, cx: &mut Context<Self>) {
        self.edit(cx, move |client| async move {
            client
                .update_playlist(&playlist_id, None, Some(comment.trim()), &[], &[])
                .await?;
            Ok(())
        });
    }

    pub fn set_public(&mut self, playlist_id: String, public: bool, cx: &mut Context<Self>) {
        // Shown at once: the switch would otherwise flick back until the
        // reload lands.
        if let Some(pl) = self.playlists.iter_mut().find(|p| p.id == playlist_id) {
            pl.public = Some(public);
            cx.notify();
        }
        self.edit(cx, move |client| async move {
            client.set_playlist_public(&playlist_id, public).await?;
            Ok(())
        });
    }

    /// Store `song_ids` as the playlist's whole song list, in that order.
    pub fn reorder(&mut self, playlist_id: String, song_ids: Vec<String>, cx: &mut Context<Self>) {
        self.edit(cx, move |client| async move {
            let ids: Vec<&str> = song_ids.iter().map(String::as_str).collect();
            client.replace_playlist_songs(&playlist_id, &ids).await?;
            Ok(())
        });
    }

    /// Copy a playlist (songs and description) under `name`; emits
    /// `PlaylistsEvent::Duplicated` with the copy's id.
    pub fn duplicate(&mut self, playlist_id: String, name: String, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                let source = client.get_playlist(&playlist_id).await?;
                let ids: Vec<&str> = source.songs.iter().map(|s| s.id.as_str()).collect();
                let copy = client.create_playlist(&name, &ids).await?;
                if let Some(comment) = source
                    .playlist
                    .comment
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                {
                    client
                        .update_playlist(&copy.playlist.id, None, Some(comment), &[], &[])
                        .await?;
                }
                Ok::<_, anyhow::Error>(copy.playlist.id)
            })
            .await;
            let _ = this.update(cx, |state, cx| {
                state.revision += 1;
                match result {
                    Ok(id) => {
                        state.error = None;
                        state.reload(cx);
                        cx.emit(PlaylistsEvent::Duplicated(id));
                    }
                    Err(e) => {
                        state.error = Some(crate::errors::error_text(&e));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn delete(&mut self, playlist_id: String, cx: &mut Context<Self>) {
        self.edit(cx, move |client| async move {
            client.delete_playlist(&playlist_id).await?;
            Ok(())
        });
    }

    /// Remove songs at the given playlist positions, then reload.
    pub fn remove_songs(&mut self, playlist_id: String, indices: Vec<u32>, cx: &mut Context<Self>) {
        self.edit(cx, move |client| async move {
            client
                .update_playlist(&playlist_id, None, None, &[], &indices)
                .await?;
            Ok(())
        });
    }
}

/// `owner` is the playlist's owner field, `me` the signed-in user. A missing
/// owner (older servers omit it) is treated as ours — the server still refuses
/// an edit that isn't, and the error says so.
pub fn owned_by(owner: Option<&str>, me: Option<&str>) -> bool {
    match (owner, me) {
        (Some(owner), Some(me)) => owner == me,
        _ => true,
    }
}

/// A name for a copy of `name` that none of `taken` uses: "X (copy)", then
/// "X (copy 2)", "X (copy 3)", …
pub fn copy_name(name: &str, taken: &[&str]) -> String {
    let first = format!("{name} (copy)");
    if !taken.contains(&first.as_str()) {
        return first;
    }
    (2..)
        .map(|n| format!("{name} (copy {n})"))
        .find(|c| !taken.contains(&c.as_str()))
        .expect("an unbounded range finds a free name")
}

pub fn init(session: Entity<Session>, cx: &mut gpui::App) -> Entity<PlaylistsState> {
    use gpui::AppContext as _;
    cx.new(|cx| PlaylistsState::new(session, cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_owner_edits() {
        assert!(owned_by(Some("joe"), Some("joe")));
        assert!(!owned_by(Some("ann"), Some("joe")));
    }

    #[test]
    fn a_missing_owner_is_ours() {
        assert!(owned_by(None, Some("joe")));
        assert!(owned_by(Some("joe"), None));
    }

    #[test]
    fn copy_names_count_up_past_taken_ones() {
        assert_eq!(copy_name("Mix", &["Mix"]), "Mix (copy)");
        assert_eq!(copy_name("Mix", &["Mix", "Mix (copy)"]), "Mix (copy 2)");
        assert_eq!(
            copy_name("Mix", &["Mix (copy)", "Mix (copy 2)"]),
            "Mix (copy 3)"
        );
    }
}
