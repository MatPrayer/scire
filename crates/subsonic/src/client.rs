use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::auth::Credentials;
use crate::error::{ApiErrorCode, Error};

pub(crate) const API_VERSION: &str = "1.16.1";
pub(crate) const CLIENT_NAME: &str = "Scirè";

/// Cap on establishing a connection. reqwest has no default at all, so a host
/// that drops packets (a server that moved, a laptop off the network) is left
/// to the OS: Linux retries the SYN for over two minutes. With only two IO
/// workers every request behind it waits, which is what "the whole app froze"
/// was — a listing that will never arrive has to fail quickly enough to be
/// reported.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the gap between two bytes of a response, not on the whole response:
/// `getArtists` returns an entire library in one body and a total timeout
/// would abort a large one on a slow link. A server that has stopped sending
/// mid-body still fails here.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Async Subsonic API client.
///
/// Cheap to clone; holds a shared reqwest client. Every request carries fresh
/// token-auth query params (`u`, `t`, `s`, `v`, `c`, `f=json`).
#[derive(Debug, Clone)]
pub struct SubsonicClient {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: Url,
    pub(crate) credentials: Credentials,
}

/// Top-level `subsonic-response` envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(rename = "subsonic-response")]
    inner: ResponseBody<T>,
}

#[derive(Debug, Deserialize)]
struct ResponseBody<T> {
    status: String,
    error: Option<ApiError>,
    #[serde(flatten)]
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: u32,
    message: Option<String>,
}

/// Wrap a reqwest failure **with its URL removed**.
///
/// Every request URL this crate builds carries `u`, `t` (the auth token) and
/// `s` (the salt) as query params, and `reqwest::Error`'s `Display` prints the
/// URL it failed on — so the app rendering an error into a view, or writing
/// one to a log, published the credentials with it. `without_url` is reqwest's
/// own way to drop it; the endpoint is named by the caller's context anyway.
pub(crate) fn http_error(e: reqwest::Error) -> Error {
    Error::Http(e.without_url())
}

impl SubsonicClient {
    /// Create a client for `base_url` (e.g. `https://music.example.com`).
    pub fn new(base_url: &str, credentials: Credentials) -> Result<Self, Error> {
        let mut url =
            Url::parse(base_url).map_err(|e| Error::InvalidUrl(format!("{base_url}: {e}")))?;
        // Normalize: ensure trailing slash so join() keeps any path prefix.
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Self {
            // Keep pooled connections well past reqwest's 90s default: album
            // pages are two or three requests each and a cold connection pays
            // the TLS handshake again (~90ms against a remote server), which
            // is most of what a page load costs.
            http: reqwest::Client::builder()
                .pool_idle_timeout(Duration::from_secs(300))
                .tcp_keepalive(Duration::from_secs(60))
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .map_err(http_error)?,
            base_url: url,
            credentials,
        })
    }

    /// Build a fully-authenticated URL for `rest/{endpoint}` with extra params.
    /// Used both for API requests and for stream/coverArt URLs handed to the
    /// playback and artwork layers.
    pub(crate) fn build_url(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<Url, Error> {
        let mut url = self
            .base_url
            .join(&format!("rest/{endpoint}"))
            .map_err(|e| Error::InvalidUrl(e.to_string()))?;
        let auth = self.credentials.token();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("u", &self.credentials.username);
            q.append_pair("t", &auth.token);
            q.append_pair("s", &auth.salt);
            q.append_pair("v", API_VERSION);
            q.append_pair("c", CLIENT_NAME);
            q.append_pair("f", "json");
            for (k, v) in params {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// Issue a GET request and unwrap the `subsonic-response` envelope.
    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<T, Error> {
        let url = self.build_url(endpoint, params)?;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        let envelope: Envelope<T> = resp.json().await.map_err(http_error)?;
        let body = envelope.inner;
        if body.status != "ok" {
            let (code, message) = body
                .error
                .map(|e| (e.code, e.message.unwrap_or_default()))
                .unwrap_or((0, "unknown error".into()));
            return Err(Error::Api {
                code: ApiErrorCode::from(code),
                message,
            });
        }
        body.data
            .ok_or_else(|| Error::UnexpectedResponse(format!("{endpoint}: empty ok response")))
    }

    /// GET an endpoint whose only useful payload is the ok/failed status.
    pub(crate) async fn get_empty(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<(), Error> {
        // Deserialize into an ignored map so unknown payloads don't error.
        let _: serde_json::Map<String, serde_json::Value> = self.get(endpoint, params).await?;
        Ok(())
    }
}
