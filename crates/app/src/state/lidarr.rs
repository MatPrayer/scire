//! Lidarr connection + live status: the download queue and the album searches
//! in flight, polled while the integration is on. Shared through the
//! [`LidarrHandle`] global so the sidebar, the Lidarr page and artist pages
//! read one copy.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, Global, Task};

use crate::config::Settings;
use crate::services::lidarr::{Album, HistoryItem, Lidarr, QueueItem};
use crate::services::runtime;

/// Queue poll interval; faster for a while after the user started something,
/// so a grab shows up in the queue without a long wait.
const POLL: Duration = Duration::from_secs(10);
const POLL_BUSY: Duration = Duration::from_secs(3);
/// Polls at `POLL_BUSY` after an action.
const BUSY_POLLS: u32 = 10;
/// History records read per poll to spot imports. One download writes a
/// record per track plus one for the download, so a page this size can miss
/// some of a big album's — any one is enough to know it landed.
const HISTORY_PEEK: u32 = 25;
/// How long an imported album waits for the library to list it before its
/// card gives up — a server that never scans, or a title the server spells
/// differently enough that the match never lands.
const ARRIVED_TTL: Duration = Duration::from_secs(60 * 60);

/// An album on its way into the library: in Lidarr's queue, or imported and
/// waiting for the server to list it. The album grid draws one card each.
#[derive(Clone, Debug, PartialEq)]
pub struct Incoming {
    /// Lidarr's album (id for its page, images for the cover).
    pub album: Album,
    pub artist: String,
    /// 0.0–1.0; 1.0 once the files are down.
    pub progress: f32,
    pub label: &'static str,
    pub problem: bool,
}

/// An album whose download left the queue finished, not yet in the library.
#[derive(Clone, Debug)]
struct Arrived {
    album: Album,
    artist: String,
    since: Instant,
}

/// An album whose files Lidarr just put in the music folder.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImportedAlbum {
    pub artist: String,
    pub title: String,
}

/// What one poll read.
struct Poll {
    queue: Vec<QueueItem>,
    searching: HashSet<i64>,
    searching_artists: HashSet<i64>,
    /// `None` when the history request failed; the queue still counts.
    history: Option<Vec<HistoryItem>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Connection {
    Off,
    Checking,
    Online { version: String },
    Failed(String),
}

pub struct LidarrState {
    pub client: Option<Lidarr>,
    pub connection: Connection,
    pub queue: Vec<QueueItem>,
    pub queue_loaded: bool,
    /// Albums with an `AlbumSearch` command queued or running.
    pub searching: HashSet<i64>,
    /// Artists with an `ArtistSearch` command queued or running.
    pub searching_artists: HashSet<i64>,
    /// Bumped whenever an album's files may have changed (an import landed),
    /// so pages listing missing releases know to ask again.
    pub imports: u64,
    /// Bumped when history shows files imported since the last poll; the
    /// albums are in `landed`. The library refresh keys on this, not on
    /// `imports`, which also moves for searches that found nothing.
    pub landed_generation: u64,
    pub landed: Vec<ImportedAlbum>,
    /// Finished downloads waiting for the library to list them; see
    /// `incoming` and `settle_arrived`.
    arrived: Vec<Arrived>,
    /// Newest history record seen; `None` until the first answer, which only
    /// sets it (imports from before launch are not news).
    last_history_id: Option<i64>,
    busy_polls: u32,
    poll: Option<Task<()>>,
}

pub struct LidarrHandle(pub Entity<LidarrState>);

impl Global for LidarrHandle {}

/// The shared Lidarr state.
pub fn lidarr(cx: &App) -> Entity<LidarrState> {
    cx.global::<LidarrHandle>().0.clone()
}

pub fn init(settings: &Settings, cx: &mut App) -> Entity<LidarrState> {
    let client = client_from(settings);
    let state = cx.new(|cx| {
        let mut state = LidarrState {
            client: None,
            connection: Connection::Off,
            queue: Vec::new(),
            queue_loaded: false,
            searching: HashSet::new(),
            searching_artists: HashSet::new(),
            imports: 0,
            landed_generation: 0,
            landed: Vec::new(),
            arrived: Vec::new(),
            last_history_id: None,
            busy_polls: 0,
            poll: None,
        };
        state.set_client(client, cx);
        state
    });
    cx.set_global(LidarrHandle(state.clone()));
    state
}

/// The client `settings` describe, `None` when off or incomplete. Reads the
/// keyring — not for `render`.
pub fn client_from(settings: &Settings) -> Option<Lidarr> {
    if !settings.lidarr_enabled {
        return None;
    }
    let key = crate::config::load_lidarr_key(settings).ok()?;
    Lidarr::new(&settings.lidarr_url, &key)
}

impl LidarrState {
    pub fn enabled(&self) -> bool {
        self.client.is_some()
    }

    /// Downloads not yet imported, for the sidebar badge.
    pub fn active_downloads(&self) -> usize {
        self.queue
            .iter()
            .filter(|q| q.tracked_download_state.as_deref() != Some("imported"))
            .count()
    }

    pub fn queue_problems(&self) -> usize {
        self.queue.iter().filter(|q| q.has_problem()).count()
    }

    /// The queue entry downloading `album_id`, if any.
    pub fn queued_album(&self, album_id: i64) -> Option<&QueueItem> {
        self.queue.iter().find(|q| q.album_id == Some(album_id))
    }

    /// Albums on their way into the library, queue order first, then the
    /// imported ones still waiting for the server to list them.
    pub fn incoming(&self) -> Vec<Incoming> {
        let mut out: Vec<Incoming> = Vec::new();
        for item in &self.queue {
            let Some(album) = item.album.as_ref() else {
                continue;
            };
            // A release split over several downloads is one card.
            if let Some(seen) = out.iter_mut().find(|i| i.album.id == album.id) {
                seen.progress = seen.progress.min(item.progress());
                seen.problem |= item.has_problem();
                continue;
            }
            out.push(Incoming {
                album: album.clone(),
                artist: queue_artist(item),
                progress: queue_progress(item),
                label: item.state_label(),
                problem: item.has_problem(),
            });
        }
        for arrived in &self.arrived {
            if out.iter().any(|i| i.album.id == arrived.album.id) {
                continue;
            }
            out.push(Incoming {
                album: arrived.album.clone(),
                artist: arrived.artist.clone(),
                progress: 1.0,
                label: "Waiting for library",
                problem: false,
            });
        }
        out
    }

    /// Drops imported albums the library now lists. `in_library(artist,
    /// title)` asks the caller's copy of the catalog.
    pub fn settle_arrived(
        &mut self,
        in_library: impl Fn(&str, &str) -> bool,
        cx: &mut Context<Self>,
    ) {
        let before = self.arrived.len();
        self.arrived
            .retain(|a| !in_library(&a.artist, &a.album.title));
        if self.arrived.len() != before {
            cx.notify();
        }
    }

    pub fn has_arrived(&self) -> bool {
        !self.arrived.is_empty()
    }

    /// Swaps the connection; unchanged settings keep the running poll.
    pub fn set_client(&mut self, client: Option<Lidarr>, cx: &mut Context<Self>) {
        if client == self.client && self.poll.is_some() == client.is_some() {
            return;
        }
        self.client = client.clone();
        self.queue.clear();
        self.queue_loaded = false;
        self.searching.clear();
        self.searching_artists.clear();
        self.arrived.clear();
        self.last_history_id = None;
        self.poll = None;
        self.connection = match &client {
            Some(_) => Connection::Checking,
            None => Connection::Off,
        };
        cx.notify();
        let Some(client) = client else {
            return;
        };
        self.poll = Some(cx.spawn(async move |this, cx| {
            let status = {
                let client = client.clone();
                runtime::spawn_io(async move { client.status().await }).await
            };
            if this
                .update(cx, |state, cx| {
                    state.connection = match status {
                        Ok(s) => Connection::Online { version: s.version },
                        Err(e) => Connection::Failed(crate::errors::error_text(&e)),
                    };
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            loop {
                let poll = poll_once(&client).await;
                let Ok(busy) = this.update(cx, |state, cx| {
                    state.apply_poll(poll, cx);
                    state.busy_polls = state.busy_polls.saturating_sub(1);
                    state.busy_polls > 0
                }) else {
                    return;
                };
                let wait = if busy { POLL_BUSY } else { POLL };
                cx.background_executor().timer(wait).await;
            }
        }));
    }

    fn apply_poll(&mut self, poll: anyhow::Result<Poll>, cx: &mut Context<Self>) {
        match poll {
            Ok(Poll {
                queue,
                searching,
                searching_artists,
                history,
            }) => {
                let landed = history.map(|history| {
                    let (last, albums) = new_imports(&history, self.last_history_id);
                    self.last_history_id = last;
                    albums
                });
                let imports_before = self.imports;
                let imported = self
                    .queue
                    .iter()
                    .filter(|old| !queue.iter().any(|q| q.id == old.id))
                    .count();
                let connection = match &self.connection {
                    Connection::Online { .. } => self.connection.clone(),
                    // Back after a failure: version unknown until the next
                    // reconfigure, which is fine for a status line.
                    _ => Connection::Online {
                        version: String::new(),
                    },
                };
                let changed = queue != self.queue
                    || searching != self.searching
                    || searching_artists != self.searching_artists
                    || !self.queue_loaded
                    || connection != self.connection;
                // A search that finished may have grabbed and imported too.
                let finished_search = self.searching.iter().any(|id| !searching.contains(id))
                    || self
                        .searching_artists
                        .iter()
                        .any(|id| !searching_artists.contains(id));
                let arrived_before = self.arrived.len();
                note_arrivals(&mut self.arrived, &self.queue, &queue, Instant::now());
                let changed = changed || self.arrived.len() != arrived_before;
                let landed = landed.unwrap_or_default();
                if imported > 0 || finished_search || !landed.is_empty() {
                    self.imports += 1;
                }
                if !landed.is_empty() {
                    self.landed_generation += 1;
                    self.landed = landed;
                }
                self.queue = queue;
                self.searching = searching;
                self.searching_artists = searching_artists;
                self.queue_loaded = true;
                self.connection = connection;
                if changed || self.imports != imports_before {
                    cx.notify();
                }
            }
            Err(e) => {
                let failed = Connection::Failed(crate::errors::error_text(&e));
                if self.connection != failed {
                    self.connection = failed;
                    cx.notify();
                }
            }
        }
    }

    /// Polls quickly for a while (after a search or grab) and right now.
    pub fn poke(&mut self, cx: &mut Context<Self>) {
        self.busy_polls = BUSY_POLLS;
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let poll = poll_once(&client).await;
            let _ = this.update(cx, |state, cx| {
                if state.client.as_ref() == Some(&client) {
                    state.apply_poll(poll, cx);
                }
            });
        })
        .detach();
    }

    /// Starts Lidarr's automatic search for an album.
    pub fn search_album(
        &mut self,
        album_id: i64,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Lidarr is not connected")));
        };
        self.searching.insert(album_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move { client.search_album(album_id).await })
                .await
                .map(drop);
            let _ = this.update(cx, |state, cx| {
                if result.is_err() {
                    state.searching.remove(&album_id);
                    cx.notify();
                }
                state.poke(cx);
            });
            result
        })
    }

    /// Starts Lidarr's search for every monitored album of an artist.
    pub fn search_artist(
        &mut self,
        artist_id: i64,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Lidarr is not connected")));
        };
        self.searching_artists.insert(artist_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::spawn_io(async move { client.search_artist(artist_id).await })
                .await
                .map(drop);
            let _ = this.update(cx, |state, cx| {
                if result.is_err() {
                    state.searching_artists.remove(&artist_id);
                    cx.notify();
                }
                state.poke(cx);
            });
            result
        })
    }

    pub fn remove_from_queue(
        &mut self,
        id: i64,
        blocklist: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(client) = self.client.clone() else {
            return Task::ready(Err(anyhow::anyhow!("Lidarr is not connected")));
        };
        cx.spawn(async move |this, cx| {
            let result =
                runtime::spawn_io(async move { client.remove_from_queue(id, blocklist).await })
                    .await;
            let _ = this.update(cx, |state, cx| {
                if result.is_ok() {
                    state.queue.retain(|q| q.id != id);
                    cx.notify();
                }
                state.poke(cx);
            });
            result
        })
    }
}

fn queue_artist(item: &QueueItem) -> String {
    item.artist
        .as_ref()
        .map(|a| a.artist_name.clone())
        .unwrap_or_else(|| {
            item.album
                .as_ref()
                .map(|a| a.artist_name().to_string())
                .unwrap_or_default()
        })
}

/// Download progress, held at full once the files are down and Lidarr is
/// importing them — the size fields can lag behind that.
fn queue_progress(item: &QueueItem) -> f32 {
    match item.tracked_download_state.as_deref() {
        Some("importPending") | Some("importing") | Some("imported") => 1.0,
        _ => item.progress(),
    }
}

/// Moves downloads that left the queue finished into `arrived`, drops
/// arrivals that are back in the queue or too old.
fn note_arrivals(arrived: &mut Vec<Arrived>, old: &[QueueItem], new: &[QueueItem], now: Instant) {
    arrived.retain(|a| {
        now.duration_since(a.since) < ARRIVED_TTL
            && !new
                .iter()
                .any(|q| q.album.as_ref().is_some_and(|al| al.id == a.album.id))
    });
    for item in old {
        if new.iter().any(|q| q.id == item.id) || item.has_problem() {
            continue;
        }
        // Removed half-way (by hand, or a failed grab) is not an arrival.
        if queue_progress(item) < 0.999 {
            continue;
        }
        let Some(album) = item.album.as_ref() else {
            continue;
        };
        if arrived.iter().any(|a| a.album.id == album.id) {
            continue;
        }
        arrived.push(Arrived {
            album: album.clone(),
            artist: queue_artist(item),
            since: now,
        });
    }
}

async fn poll_once(client: &Lidarr) -> anyhow::Result<Poll> {
    let client = client.clone();
    runtime::spawn_io(async move {
        let (queue, commands, history) = tokio::join!(
            client.queue(),
            client.commands(),
            client.recent_history(HISTORY_PEEK)
        );
        let active: Vec<_> = commands
            .map(|commands| commands.into_iter().filter(|c| c.is_active()).collect())
            .unwrap_or_default();
        let searching = active.iter().flat_map(|c| c.album_ids()).collect();
        let searching_artists = active.iter().filter_map(|c| c.artist_id()).collect();
        Ok(Poll {
            queue: queue?,
            searching,
            searching_artists,
            history: history.ok(),
        })
    })
    .await
}

/// The albums imported in `history` (newest first) after record `last`, and
/// the newest id to remember. With no `last` it only takes the baseline.
fn new_imports(history: &[HistoryItem], last: Option<i64>) -> (Option<i64>, Vec<ImportedAlbum>) {
    let newest = history.iter().map(|h| h.id).max().max(last);
    let Some(last) = last else {
        return (newest, Vec::new());
    };
    let mut albums: Vec<ImportedAlbum> = Vec::new();
    for item in history.iter().filter(|h| h.id > last && h.is_import()) {
        let (Some(artist), Some(album)) = (&item.artist, &item.album) else {
            continue;
        };
        let imported = ImportedAlbum {
            artist: artist.artist_name.clone(),
            title: album.title.clone(),
        };
        if !albums.contains(&imported) {
            albums.push(imported);
        }
    }
    (newest, albums)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::lidarr::{Album, Artist};

    fn record(id: i64, event: &str, title: &str) -> HistoryItem {
        HistoryItem {
            id,
            event_type: event.into(),
            date: String::new(),
            source_title: String::new(),
            artist: Some(Artist {
                id: 1,
                artist_name: "Low".into(),
                monitored: true,
                ..Default::default()
            }),
            album: Some(Album {
                title: title.into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn the_first_answer_only_sets_the_baseline() {
        let history = [record(9, "downloadImported", "Hey What")];
        assert_eq!(new_imports(&history, None), (Some(9), Vec::new()));
    }

    #[test]
    fn imports_after_the_baseline_are_reported_once_per_album() {
        let history = [
            record(12, "trackFileImported", "Hey What"),
            record(11, "downloadImported", "Hey What"),
            record(10, "grabbed", "Double Negative"),
            record(9, "downloadImported", "Things We Lost"),
        ];
        let (last, albums) = new_imports(&history, Some(9));
        assert_eq!(last, Some(12));
        assert_eq!(
            albums,
            vec![ImportedAlbum {
                artist: "Low".into(),
                title: "Hey What".into()
            }]
        );
    }

    #[test]
    fn an_empty_history_keeps_the_baseline() {
        assert_eq!(new_imports(&[], Some(4)), (Some(4), Vec::new()));
        assert_eq!(new_imports(&[], None), (None, Vec::new()));
    }

    fn queued(id: i64, album_id: i64, size: f64, left: f64, state: &str) -> QueueItem {
        QueueItem {
            id,
            album_id: Some(album_id),
            size,
            sizeleft: left,
            status: "downloading".into(),
            tracked_download_state: Some(state.into()),
            album: Some(Album {
                id: album_id,
                title: format!("Album {album_id}"),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn finished_downloads_leaving_the_queue_arrive() {
        let now = Instant::now();
        let old = [
            queued(1, 10, 100., 0., "importing"),
            queued(2, 20, 100., 60., "downloading"),
            queued(3, 30, 100., 0., "downloading"),
        ];
        let mut arrived = Vec::new();
        note_arrivals(&mut arrived, &old, &old[2..], now);
        // 1 finished and left; 2 was removed half-way; 3 is still queued.
        let ids: Vec<i64> = arrived.iter().map(|a| a.album.id).collect();
        assert_eq!(ids, [10]);

        // Back in the queue (re-downloaded): no longer waiting.
        note_arrivals(
            &mut arrived,
            &[],
            &[queued(4, 10, 100., 90., "downloading")],
            now,
        );
        assert!(arrived.is_empty());
    }

    #[test]
    fn arrivals_expire() {
        let now = Instant::now();
        let mut arrived = vec![Arrived {
            album: Album::default(),
            artist: String::new(),
            since: now,
        }];
        note_arrivals(&mut arrived, &[], &[], now + ARRIVED_TTL);
        assert!(arrived.is_empty());
    }

    #[test]
    fn importing_reads_as_fully_downloaded() {
        assert_eq!(queue_progress(&queued(1, 1, 100., 40., "downloading")), 0.6);
        assert_eq!(
            queue_progress(&queued(1, 1, 100., 40., "importPending")),
            1.0
        );
    }
}
