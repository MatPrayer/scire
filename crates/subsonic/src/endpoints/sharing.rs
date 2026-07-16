use serde::Deserialize;

use crate::client::SubsonicClient;
use crate::error::Error;
use crate::models::Share;

#[derive(Debug, Deserialize)]
struct SharesWrapper {
    shares: SharesInner,
}

#[derive(Debug, Deserialize)]
struct SharesInner {
    #[serde(rename = "share", default)]
    items: Vec<Share>,
}

impl SubsonicClient {
    /// Create a public share for one or more item ids (albums, songs,
    /// playlists). Returns the created share (its `url` is the public link).
    ///
    /// Errors with a "not authorized" / "not supported" code when sharing is
    /// disabled server-side — callers should surface that gracefully.
    pub async fn create_share(
        &self,
        ids: &[&str],
        description: Option<&str>,
    ) -> Result<Share, Error> {
        let mut params: Vec<(&str, &str)> = ids.iter().map(|id| ("id", *id)).collect();
        if let Some(desc) = description {
            params.push(("description", desc));
        }
        let w: SharesWrapper = self.get("createShare", &params).await?;
        w.shares
            .items
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnexpectedResponse("createShare returned no share".into()))
    }
}
