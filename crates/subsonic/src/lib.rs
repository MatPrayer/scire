//! Pure Subsonic/OpenSubsonic API client for Navidrome servers.
//!
//! No UI or audio dependencies. All catalog methods accept an optional
//! `musicFolderId` for Navidrome multi-library filtering.

mod auth;
mod client;
mod endpoints;
mod error;
mod models;

pub use auth::Credentials;
pub use client::SubsonicClient;
pub use endpoints::annotation::Starred;
pub use endpoints::browsing::{AlbumInfo2, ArtistInfo2};
pub use endpoints::media::{Lyrics, StreamOptions};
pub use endpoints::system::ServerInfo;
pub use error::{ApiErrorCode, Error};
pub use models::*;
