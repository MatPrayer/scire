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
    /// Selected library id; None = all libraries.
    pub library_id: Option<String>,
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
        let library_id = settings.library_id.clone();
        let mut this = Self {
            settings,
            client: None,
            status: ConnectionStatus::Disconnected,
            library_id,
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
    /// `persist` is false when reconnecting with already-saved credentials.
    pub fn connect(
        &mut self,
        url: String,
        username: String,
        password: String,
        persist: bool,
        cx: &mut Context<Self>,
    ) {
        let client = match SubsonicClient::new(&url, Credentials::new(&username, &password)) {
            Ok(c) => c,
            Err(e) => {
                self.status = ConnectionStatus::Failed(e.to_string());
                cx.notify();
                return;
            }
        };
        self.status = ConnectionStatus::Connecting;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let ping_client = client.clone();
            let result =
                runtime::spawn_io(
                    async move { ping_client.ping().await.map_err(anyhow::Error::from) },
                )
                .await;

            this.update(cx, |session, cx| {
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
            })
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
                    // Drop a stale selection that the server no longer offers.
                    if let Some(sel) = &session.library_id
                        && !folders.iter().any(|f| &f.id() == sel)
                    {
                        session.library_id = None;
                        session.settings.library_id = None;
                    }
                    session.music_folders = folders;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Select a library (None = all). Persisted; observers reload their views.
    pub fn set_library(&mut self, library_id: Option<String>, cx: &mut Context<Self>) {
        if self.library_id == library_id {
            return;
        }
        self.library_id = library_id.clone();
        self.settings.library_id = library_id;
        self.persist_settings();
        cx.notify();
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
