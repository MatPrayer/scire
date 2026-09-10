//! Bulk cover-art warm-up.
//!
//! The grids fetch covers for what is on screen, which is the right thing while
//! browsing but means a library only ever caches the parts of itself that have
//! been scrolled past. This walks the synced catalog instead and pulls every
//! album and artist cover into the artwork cache once, so the grid draws from
//! disk wherever the user jumps.
//!
//! Only Navidrome rows are walked: a local album's `cover_art` is a hash into
//! the scanner's own art directory (`local_library::local_art_path`), already
//! extracted to disk at scan time, so there is nothing to fetch.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use subsonic::SubsonicClient;

use crate::services::artwork;
use crate::services::library_db::LibraryDb;

/// Covers downloaded at once.
///
/// Deliberately well under `artwork`'s own `MAX_CONCURRENT_FETCHES`: that
/// semaphore is shared with the grids' downloads, and a precache holding every
/// permit would put the cover the user is looking at behind a few thousand they
/// are not.
const PRECACHE_CONCURRENCY: usize = 2;

/// Counters a running precache publishes for the Settings status line.
#[derive(Default)]
pub struct PrecacheProgress {
    /// Covers missing from the cache, known once the catalog has been walked.
    pub total: AtomicUsize,
    /// Covers attempted so far, downloaded or failed.
    pub done: AtomicUsize,
    /// Covers that landed in the cache.
    pub fetched: AtomicUsize,
}

impl PrecacheProgress {
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }
}

/// What one pass ended up doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrecacheOutcome {
    /// Covers downloaded.
    pub fetched: usize,
    /// Covers that could not be fetched (deleted art, a server error). Logged
    /// and counted rather than aborting the pass.
    pub failed: usize,
}

/// Album and artist cover ids in the cache that have no artwork entry at
/// `size` yet, deduplicated.
///
/// The skip is what makes the setting cheap to leave on: a second run over a
/// warm cache issues no requests at all, so this can be re-run after every sync
/// to pick up what is new.
fn missing_covers(db: &LibraryDb, size: u32) -> Result<Vec<String>> {
    let albums = db.albums_by_source("navidrome")?;
    let artists = db.artists_by_source("navidrome")?;
    let mut seen = HashSet::new();
    let covers = albums
        .into_iter()
        .filter_map(|row| row.cover_art)
        .chain(artists.into_iter().filter_map(|row| row.cover_art))
        .filter(|cover| seen.insert(cover.clone()))
        .filter(|cover| artwork::cached(cover, size).is_none())
        .collect();
    Ok(covers)
}

/// Fetch every catalog cover missing from the artwork cache at `size`.
///
/// Must be called inside the tokio runtime (`runtime::spawn_io`) — it spawns.
pub async fn precache_art(
    db: Arc<LibraryDb>,
    client: SubsonicClient,
    size: u32,
    progress: Arc<PrecacheProgress>,
) -> Result<PrecacheOutcome> {
    let before = missing_covers(&db, size)?;
    let total = before.len();
    progress.total.store(total, Ordering::Relaxed);
    tracing::info!("art precache: {total} covers missing at {size}px");

    let mut pending = before.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    let mut outcome = PrecacheOutcome::default();
    loop {
        while tasks.len() < PRECACHE_CONCURRENCY
            && let Some(cover) = pending.next()
        {
            let client = client.clone();
            tasks.spawn(async move { artwork::fetch(client, cover, size).await });
        }
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        progress.done.fetch_add(1, Ordering::Relaxed);
        match joined {
            Ok(Ok(_)) => {
                outcome.fetched += 1;
                progress.fetched.fetch_add(1, Ordering::Relaxed);
            }
            // One unreadable cover is not a reason to abandon the rest: art
            // deleted server-side outlives the row pointing at it.
            Ok(Err(e)) => {
                outcome.failed += 1;
                tracing::debug!("art precache: cover failed: {e:#}");
            }
            Err(e) => {
                outcome.failed += 1;
                tracing::debug!("art precache: task failed: {e}");
            }
        }
    }
    tracing::info!(
        "art precache done: {} fetched, {} failed",
        outcome.fetched,
        outcome.failed
    );
    Ok(outcome)
}

/// Human-readable result for the Settings status line.
pub fn outcome_message(outcome: PrecacheOutcome) -> String {
    match (outcome.fetched, outcome.failed) {
        (0, 0) => "All cover art is already cached.".into(),
        (n, 0) => format!("Cached {n} covers."),
        (0, f) => format!("No covers cached ({f} unavailable)."),
        (n, f) => format!("Cached {n} covers, {f} unavailable."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_what_happened() {
        assert_eq!(
            outcome_message(PrecacheOutcome::default()),
            "All cover art is already cached."
        );
        assert_eq!(
            outcome_message(PrecacheOutcome {
                fetched: 12,
                failed: 0,
            }),
            "Cached 12 covers."
        );
        assert_eq!(
            outcome_message(PrecacheOutcome {
                fetched: 12,
                failed: 3,
            }),
            "Cached 12 covers, 3 unavailable."
        );
    }

    #[test]
    fn a_row_without_a_cover_id_is_nothing_to_fetch() {
        let db = LibraryDb::open_in_memory().unwrap();
        // Ids no real server would hand out, since `artwork::cached` reads the
        // machine's own cache directory.
        let mut with_art =
            crate::services::library_db::AlbumRow::new("al-precache-test-1", "navidrome", "One");
        with_art.cover_art = Some("al-precache-test-1_abc".into());
        let without =
            crate::services::library_db::AlbumRow::new("al-precache-test-2", "navidrome", "Two");
        db.upsert_album(&with_art).unwrap();
        db.upsert_album(&without).unwrap();
        db.upsert_artist(
            "ar-precache-test-1",
            "navidrome",
            "Someone",
            Some("ar-precache-test-1_abc"),
            None,
        )
        .unwrap();

        let covers = missing_covers(&db, 256).unwrap();
        // Album two has no cover id at all; the artist's is listed alongside
        // album one's.
        assert_eq!(
            covers,
            vec![
                "al-precache-test-1_abc".to_string(),
                "ar-precache-test-1_abc".to_string()
            ]
        );
    }
}
