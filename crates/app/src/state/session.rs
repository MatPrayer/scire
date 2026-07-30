//! Connection/session state: server config, credentials, connect flow.

use gpui::{AppContext as _, Context, Entity};
use subsonic::{Credentials, MusicFolder, SubsonicClient};

use crate::config::{self, ServerConfig, Settings};
use crate::services::runtime;

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

pub struct Session {
    pub settings: Settings,
    pub client: Option<SubsonicClient>,
    pub status: ConnectionStatus,
    /// Selected library ids; empty = all libraries.
    pub library_ids: Vec<String>,
    /// Libraries the user can access (from getMusicFolders). The switcher only
    /// shows when there is more than one.
    pub music_folders: Vec<MusicFolder>,
}

impl Session {
    /// Load settings; if a server is configured, start connecting.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = Settings::load().unwrap_or_else(|e| {
            tracing::warn!("failed to load settings: {e:#}");
            Settings::default()
        });
        let library_ids = settings.library_ids.clone();
        let mut this = Self {
            settings,
            client: None,
            status: ConnectionStatus::Disconnected,
            library_ids,
            music_folders: Vec::new(),
        };
        if let Some(server) = this.settings.server.clone() {
            this.connect_saved(server, cx);
        }
        this
    }

    /// Reconnect using stored credentials (keyring, falling back to any
    /// plaintext field in settings).
    fn connect_saved(&mut self, server: ServerConfig, cx: &mut Context<Self>) {
        let password = config::load_password(&server.url, &server.username)
            .ok()
            .or(server.password_plaintext.clone());
        match password {
            Some(pw) => self.connect(server.url, server.username, pw, false, cx),
            None => {
                self.status =
                    ConnectionStatus::Failed("stored password not found; log in again".into());
            }
        }
    }

    /// Validate credentials via `ping`, then persist them on success.
    ///
    /// When `persist` is true, the server config is written to disk immediately
    /// (before the async connect) so that settings survive an app restart even
    /// when the ping is still in-flight or fails.
    /// Client construction runs on the IO runtime to avoid blocking the
    /// gpui main thread (reqwest may do DNS/proxy/TLS init synchronously).
    pub fn connect(
        &mut self,
        url: String,
        username: String,
        password: String,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        // Persist the server config immediately so first-run detection works
        // across restarts even if the async ping never completes.
        if persist {
            self.settings.server = Some(ServerConfig {
                url: url.clone(),
                username: username.clone(),
                password_plaintext: Some(password.clone()),
            });
            self.persist_settings();
        }

        self.status = ConnectionStatus::Connecting;
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Build the SubsonicClient on the IO runtime so reqwest can init
            // its connector (DNS, proxy, TLS) without freezing the UI.
            let client = match runtime::spawn_io({
                let url = url.clone();
                let username = username.clone();
                let password = password.clone();
                async move {
                    runtime::enter(|| {
                        SubsonicClient::new(&url, Credentials::new(&username, &password))
                            .map_err(|e| anyhow::anyhow!("{}", e))
                    })
                }
            })
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = this.update(cx, |session, cx| {
                        session.client = None;
                        session.status = ConnectionStatus::Failed(format!("{e}"));
                        cx.notify();
                    });
                    return;
                }
            };

            let ping_client = client.clone();
            let result = runtime::spawn_io(async move {
                tokio::time::timeout(std::time::Duration::from_secs(10), ping_client.ping())
                    .await
                    .map_err(|_| anyhow::anyhow!("ping timed out after 10 s"))?
                    .map_err(anyhow::Error::from)
            })
            .await;

            let _ = this.update(cx, |session, cx| {
                match result {
                    Ok(_info) => {
                        session.client = Some(client);
                        session.status = ConnectionStatus::Connected;
                        session.load_music_folders(cx);
                        if persist {
                            let mut server = ServerConfig {
                                url: url.clone(),
                                username: username.clone(),
                                password_plaintext: None,
                            };
                            if let Err(e) = config::store_password(&url, &username, &password) {
                                tracing::warn!(
                                    "keyring unavailable ({e:#}); storing password in settings"
                                );
                                server.password_plaintext = Some(password.clone());
                            }
                            session.settings.server = Some(server);
                            session.persist_settings();
                        }
                    }
                    Err(e) => {
                        session.client = None;
                        session.status = ConnectionStatus::Failed(friendly_error(&e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Forget server, credentials and connection.
    pub fn logout(&mut self, cx: &mut Context<Self>) {
        if let Some(server) = &self.settings.server {
            config::delete_password(&server.url, &server.username);
        }
        self.settings.server = None;
        self.client = None;
        self.status = ConnectionStatus::Disconnected;
        self.persist_settings();
        cx.notify();
    }

    /// Fetch the accessible libraries after connecting.
    fn load_music_folders(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move {
                client
                    .get_music_folders()
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
            let _ = this.update(cx, |session, cx| {
                if let Ok(folders) = result {
                    // Drop stale selections that the server no longer offers.
                    session
                        .library_ids
                        .retain(|sel| folders.iter().any(|f| &f.id() == sel));
                    session.settings.library_ids = session.library_ids.clone();
                    session.music_folders = folders;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Toggle one library in the selection. Persisted; observers reload
    /// their views. Selecting every library collapses to "all" (empty).
    pub fn toggle_library(&mut self, library_id: String, cx: &mut Context<Self>) {
        if let Some(pos) = self.library_ids.iter().position(|id| *id == library_id) {
            self.library_ids.remove(pos);
        } else {
            self.library_ids.push(library_id);
            if self.library_ids.len() == self.music_folders.len() {
                self.library_ids.clear();
            }
        }
        self.settings.library_ids = self.library_ids.clone();
        self.persist_settings();
        cx.notify();
    }

    /// Clear the selection back to "all libraries".
    pub fn select_all_libraries(&mut self, cx: &mut Context<Self>) {
        if self.library_ids.is_empty() {
            return;
        }
        self.library_ids.clear();
        self.settings.library_ids.clear();
        self.persist_settings();
        cx.notify();
    }

    /// Library ids catalog fetches should query: `[None]` for all libraries,
    /// otherwise one entry per selected library (requests are merged by the
    /// caller — the Subsonic API takes a single musicFolderId per request).
    pub fn library_query_ids(&self) -> Vec<Option<String>> {
        if self.library_ids.is_empty() {
            vec![None]
        } else {
            self.library_ids.iter().cloned().map(Some).collect()
        }
    }

    pub fn persist_settings(&self) {
        if let Err(e) = self.settings.save() {
            tracing::warn!("failed to save settings: {e:#}");
        }
    }
}

fn friendly_error(e: &anyhow::Error) -> String {
    if let Some(api_err) = e.downcast_ref::<subsonic::Error>()
        && api_err.is_auth_failure()
    {
        return "wrong username or password".into();
    }
    format!("{e:#}")
}

pub fn init(cx: &mut gpui::App) -> Entity<Session> {
    cx.new(Session::new)
}
