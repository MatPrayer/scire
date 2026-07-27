use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::auth::Credentials;
use crate::error::{ApiErrorCode, Error};

pub(crate) const API_VERSION: &str = "1.16.1";
pub(crate) const CLIENT_NAME: &str = "Scirè";

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
            http: reqwest::Client::new(),
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
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let envelope: Envelope<T> = resp.json().await?;
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
