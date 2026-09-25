//! Navidrome's own (non-Subsonic) API, for the one question the Subsonic API
//! cannot answer: whether the server forwards this user's scrobbles to
//! Last.fm and ListenBrainz.
//!
//! Subsonic's `scrobble` only reports a play to the server; what the server
//! then does with it is configured in Navidrome's web UI (Personal → link
//! Last.fm / ListenBrainz) and published only on its native API, which takes a
//! JWT from `POST /auth/login` rather than Subsonic's token auth. Each service
//! is asked separately at `GET /api/{service}/link` → `{"status": bool}`, and
//! Navidrome only mounts that route when the agent is enabled in its config —
//! so a 404 there is "this server does not do that", not a failure.

use reqwest::StatusCode;
use serde::Deserialize;

use crate::client::{SubsonicClient, http_error};
use crate::error::Error;

/// Navidrome's header for the native API's JWT (`consts.UIAuthorizationHeader`).
const AUTH_HEADER: &str = "X-ND-Authorization";

/// Whether the server forwards this user's plays to one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleLink {
    /// Enabled on the server and linked to this user's account there.
    Linked,
    /// Enabled on the server, but this user has not linked an account.
    NotLinked,
    /// The server has the service switched off (or is too old to have it).
    Disabled,
}

/// What the server does with the scrobbles it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrobbleForwarding {
    pub lastfm: ScrobbleLink,
    pub listenbrainz: ScrobbleLink,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: String,
}

#[derive(Deserialize)]
struct LinkStatus {
    #[serde(default)]
    status: bool,
}

impl SubsonicClient {
    /// Ask a Navidrome server where it forwards this user's scrobbles.
    ///
    /// `Ok(None)` is a server without Navidrome's native API (any other
    /// Subsonic server): nothing to report rather than an error. This sends
    /// the **password itself** to `/auth/login`, where the Subsonic calls only
    /// ever send a salted hash — callers should gate it on a transport they
    /// trust (see [`SubsonicClient::is_secure_transport`]).
    pub async fn scrobble_forwarding(&self) -> Result<Option<ScrobbleForwarding>, Error> {
        let Some(token) = self.native_login().await? else {
            return Ok(None);
        };
        let lastfm = self.link_status(&token, "lastfm").await?;
        let listenbrainz = self.link_status(&token, "listenbrainz").await?;
        Ok(Some(ScrobbleForwarding {
            lastfm,
            listenbrainz,
        }))
    }

    /// Whether sending the plaintext password to this server is reasonable:
    /// TLS, or a host that never leaves the machine or the local network.
    pub fn is_secure_transport(&self) -> bool {
        if self.base_url.scheme() == "https" {
            return true;
        }
        let Some(host) = self.base_url.host_str() else {
            return false;
        };
        match host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(ip)) => {
                ip.is_loopback() || ip.is_private() || ip.is_link_local()
            }
            Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
            Err(_) => host == "localhost" || host.ends_with(".local"),
        }
    }

    /// `None` when the server has no `/auth/login`, i.e. is not Navidrome.
    async fn native_login(&self) -> Result<Option<String>, Error> {
        let url = self
            .base_url
            .join("auth/login")
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        let body = serde_json::json!({
            "username": self.credentials.username,
            "password": self.credentials.password,
        });
        let resp = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(http_error)?;
        if matches!(
            resp.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ) {
            return Ok(None);
        }
        let resp = resp.error_for_status().map_err(http_error)?;
        let login: LoginResponse = resp.json().await.map_err(|_| {
            Error::UnexpectedResponse("auth/login: no token in the response".into())
        })?;
        Ok(Some(login.token))
    }

    async fn link_status(&self, token: &str, service: &str) -> Result<ScrobbleLink, Error> {
        let url = self
            .base_url
            .join(&format!("api/{service}/link"))
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        let resp = self
            .http
            .get(url)
            .header(AUTH_HEADER, format!("Bearer {token}"))
            .send()
            .await
            .map_err(http_error)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(ScrobbleLink::Disabled);
        }
        let resp = resp.error_for_status().map_err(http_error)?;
        let link: LinkStatus = resp
            .json()
            .await
            .map_err(|_| Error::UnexpectedResponse(format!("api/{service}/link")))?;
        Ok(if link.status {
            ScrobbleLink::Linked
        } else {
            ScrobbleLink::NotLinked
        })
    }
}
