//! Navidrome → LibraryDb sync.
//!
//! Fetches all albums (paginated) and their tracks from the Subsonic API and
//! upserts them into the local SQLite DB so the app can query both local and
//! remote music from a single database.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use subsonic::SubsonicClient;

use crate::services::library_db::LibraryDb;

const PAGE_SIZE: u32 = 500;

/// Sync all Navidrome data into the local database.
///
/// 1. Fetch all albums (paginated `getAlbumList2` → `alphabeticalByName`).
/// 2. For each album, fetch its tracks via `getAlbum` and upsert.
/// 3. Record last-sync timestamp.
pub async fn sync_navidrome(
    db: Arc<LibraryDb>,
    client: &SubsonicClient,
    music_folder_id: Option<&str>,
) -> Result<()> {
    tracing::info!("navidrome sync start");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Mark existing navidrome tracks as stale; remove after successful re-import.
    // ponytail: truncate-and-resync. Simpler than diff-based sync. Good until
    // library exceeds ~50k tracks.
    let count = remove_navidrome(&db)?;
    tracing::info!("removed {count} stale navidrome tracks/albums/artists");

    let folder_id: Option<String> = music_folder_id.map(|s| s.to_string());
    let mut offset = 0u32;
    loop {
        let albums = client
            .get_album_list2(
                subsonic::AlbumListType::AlphabeticalByName,
                PAGE_SIZE,
                offset,
                folder_id.as_ref(),
            )
            .await?;
        if albums.is_empty() {
            break;
        }
        for album in &albums {
            let album_id = format!("navidrome:album:{}", album.id);
            let artist_id = album
                .artist_id
                .as_ref()
                .map(|id| format!("navidrome:artist:{id}"));

            // Upsert artist
            if let Some(artist_name) = &album.artist
                && let Some(aid) = &artist_id
            {
                let _ = db.upsert_artist(aid, "navidrome", artist_name, None);
            }

            // Upsert album
            let _ = db.upsert_album(
                &album_id,
                "navidrome",
                &album.name,
                album.artist.as_deref(),
                artist_id.as_deref(),
                album.year,
                album.cover_art.as_deref(),
                album.song_count.unwrap_or(0) as i64,
                album.duration.unwrap_or(0) as f64,
            );

            // Fetch tracks for this album
            if let Ok(album_with) = client.get_album(&album.id).await {
                for song in &album_with.song {
                    let track_id = format!("navidrome:track:{}", song.id);
                    let _ = db.upsert_track(
                        &track_id,
                        "navidrome",
                        &song.title,
                        song.artist.as_deref(),
                        artist_id.as_deref(),
                        song.album.as_deref(),
                        Some(&album_id),
                        None, // album_artist
                        song.track.map(|t| t as i32),
                        song.disc_number.map(|d| d as i32),
                        song.year,
                        song.genre.as_deref(),
                        song.duration.map(|d| d as f64),
                        None, // local_path
                        song.cover_art.as_deref(),
                        Some(now),
                    );
                }
            }
        }
        offset += PAGE_SIZE;
    }

    // Record sync timestamp
    db.upsert_config("navidrome_last_sync", &now.to_string())?;

    let c = db.track_count_by_source("navidrome")?;
    tracing::info!("navidrome sync done: {c} tracks");
    Ok(())
}

/// Remove all navidrome-sourced tracks, albums, and artists from the DB.
fn remove_navidrome(db: &LibraryDb) -> Result<i64> {
    let count = db.track_count_by_source("navidrome")?;
    db.execute("DELETE FROM tracks WHERE source = 'navidrome'")?;
    db.execute("DELETE FROM albums WHERE source = 'navidrome'")?;
    db.execute("DELETE FROM artists WHERE source = 'navidrome'")?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Arc<LibraryDb> {
        Arc::new(LibraryDb::open_in_memory().unwrap())
    }

    #[test]
    fn remove_navidrome_clears_tracks() {
        let db = test_db();
        db.upsert_track(
            "n1",
            "navidrome",
            "Song",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.upsert_track(
            "l1", "local", "Local", None, None, None, None, None, None, None, None, None, None,
            None, None, None,
        )
        .unwrap();
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 1);
        assert_eq!(db.track_count_by_source("local").unwrap(), 1);
        remove_navidrome(&db).unwrap();
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 0);
        assert_eq!(db.track_count_by_source("local").unwrap(), 1);
    }

    #[test]
    fn remove_navidrome_idempotent() {
        let db = test_db();
        remove_navidrome(&db).unwrap();
        remove_navidrome(&db).unwrap(); // no error on empty
        assert_eq!(db.track_count_by_source("navidrome").unwrap(), 0);
    }
}
