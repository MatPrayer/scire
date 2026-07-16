//! Player state entity: bridges the playback engine's events into gpui and
//! drives the play queue (shuffle/repeat/prefetch).

use std::time::Duration;

use gpui::{AppContext as _, Context, Entity};
use playback::{Event, Player, TrackSource};
use subsonic::{Song, StreamOptions, SubsonicClient};

use crate::services::runtime;
use crate::state::media::MediaKeys;
use crate::state::queue::{Queue, RepeatMode};
use crate::state::scrobble::{ScrobbleAction, ScrobbleTracker};

pub struct PlayerState {
    player: Player,
    pub queue: Queue,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub playing: bool,
    pub buffering: bool,
    pub volume: f32,
    pub last_error: Option<String>,
    client: Option<SubsonicClient>,
    scrobble: ScrobbleTracker,
    scrobble_enabled: bool,
    stream_opts: StreamOptions,
    /// Set while a live radio stream is playing (title for display).
    radio_title: Option<String>,
    media_keys: MediaKeys,
    /// Most-recently-played tracks (newest first, max 50). Persisted to disk.
    pub recently_played: Vec<Song>,
    /// Local path of the current track's cover art (for OS media controls).
    pub current_art_path: Option<std::path::PathBuf>,
}

impl PlayerState {
    pub fn new(volume: f32, cx: &mut Context<Self>) -> Self {
        // Engine spawns tokio tasks; enter the IO runtime for construction.
        let (player, mut events) = runtime::enter(Player::new);
        player.set_volume(volume);

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                let done = this
                    .update(cx, |state, cx| {
                        state.on_event(event, cx);
                    })
                    .is_err();
                if done {
                    break; // entity dropped
                }
            }
        })
        .detach();

        // OS media keys / Now Playing: forward presses into the entity.
        let (media_tx, mut media_rx) = tokio::sync::mpsc::unbounded_channel();
        let media_keys = MediaKeys::new(media_tx);
        cx.spawn(async move |this, cx| {
            while let Some(event) = media_rx.recv().await {
                let done = this
                    .update(cx, |state, cx| state.on_media_key(event, cx))
                    .is_err();
                if done {
                    break;
                }
            }
        })
        .detach();

        Self {
            player,
            queue: Queue::default(),
            position: Duration::ZERO,
            duration: None,
            playing: false,
            buffering: false,
            volume,
            last_error: None,
            client: None,
            scrobble: ScrobbleTracker::default(),
            scrobble_enabled: true,
            stream_opts: StreamOptions::default(),
            radio_title: None,
            media_keys,
            recently_played: load_recent(),
            current_art_path: None,
        }
    }

    /// Fire a scrobble call (now-playing or submission) in the background.
    fn fire_scrobble(&self, action: ScrobbleAction, cx: &mut Context<Self>) {
        if !self.scrobble_enabled {
            return;
        }
        let (id, submission) = match action {
            ScrobbleAction::None => return,
            ScrobbleAction::NowPlaying(id) => (id, false),
            ScrobbleAction::Submit(id) => (id, true),
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |_this, _cx| {
            let _ = runtime::spawn_io(async move {
                client
                    .scrobble(&id, submission)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await;
        })
        .detach();
    }

    pub fn set_client(&mut self, client: Option<SubsonicClient>) {
        self.client = client;
    }

    pub fn current_song(&self) -> Option<&Song> {
        self.queue.current_song()
    }

    /// Display info for the player bar: (title, subtitle). Radio wins over the
    /// queue when a live stream is playing.
    pub fn now_playing(&self) -> Option<(String, String)> {
        if let Some(title) = &self.radio_title {
            return Some((title.clone(), "Internet radio".into()));
        }
        self.current_song()
            .map(|s| (s.title.clone(), s.artist.clone().unwrap_or_default()))
    }

    /// True while a live radio stream is playing (seek is meaningless).
    pub fn is_radio(&self) -> bool {
        self.radio_title.is_some()
    }

    /// Play a live internet-radio stream by its external URL.
    pub fn play_radio(&mut self, name: String, stream_url: String, cx: &mut Context<Self>) {
        self.queue.clear();
        self.scrobble.clear();
        self.player.clear_prefetch();
        self.radio_title = Some(name);
        self.position = Duration::ZERO;
        self.duration = None;
        self.last_error = None;
        self.buffering = true;
        self.player.play(TrackSource {
            url: stream_url,
            duration_hint: None,
        });
        self.sync_media_metadata();
        self.media_keys.set_playing(true, Duration::ZERO);
        cx.notify();
    }

    // ----- queue operations -----

    /// Replace the queue with `songs` and start playing at `index`.
    pub fn play_queue(&mut self, songs: Vec<Song>, index: usize, cx: &mut Context<Self>) {
        self.queue.replace(songs, index);
        self.start_current(cx);
    }

    /// Append songs to the end of the queue.
    pub fn enqueue(&mut self, songs: Vec<Song>, cx: &mut Context<Self>) {
        let was_empty = self.queue.is_empty();
        self.queue.append(songs);
        if was_empty {
            self.queue.jump_to(0);
            self.start_current(cx);
        } else {
            self.refresh_prefetch(cx);
        }
    }

    /// Insert songs right after the current one.
    pub fn play_next(&mut self, songs: Vec<Song>, cx: &mut Context<Self>) {
        let was_empty = self.queue.is_empty();
        self.queue.play_next(songs);
        if was_empty {
            self.queue.jump_to(0);
            self.start_current(cx);
        } else {
            self.refresh_prefetch(cx);
        }
    }

    /// Jump to a queue position (from the queue panel).
    pub fn jump_to(&mut self, order_pos: usize, cx: &mut Context<Self>) {
        if self.queue.jump_to(order_pos).is_some() {
            self.start_current(cx);
        }
    }

    pub fn remove_from_queue(&mut self, order_pos: usize, cx: &mut Context<Self>) {
        let was_current = self.queue.current_pos() == Some(order_pos);
        self.queue.remove(order_pos);
        if was_current {
            if self.queue.is_empty() {
                self.stop(cx);
            } else {
                self.start_current(cx);
            }
        } else {
            self.refresh_prefetch(cx);
        }
        cx.notify();
    }

    pub fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        self.stop(cx);
    }

    pub fn toggle_shuffle(&mut self, cx: &mut Context<Self>) {
        let on = !self.queue.shuffle;
        self.queue.set_shuffle(on);
        self.refresh_prefetch(cx);
        cx.notify();
    }

    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.queue.repeat = self.queue.repeat.cycle();
        self.refresh_prefetch(cx);
        cx.notify();
    }

    // ----- transport -----

    pub fn next(&mut self, cx: &mut Context<Self>) {
        if let Some(pos) = self.queue.skip_next_pos() {
            self.queue.advance_to(pos);
            self.start_current(cx);
        }
    }

    pub fn previous(&mut self, cx: &mut Context<Self>) {
        // Standard behavior: restart current track unless near its start.
        if self.position > Duration::from_secs(3) {
            self.player.seek(Duration::ZERO);
            return;
        }
        if let Some(pos) = self.queue.prev_pos() {
            self.queue.advance_to(pos);
            self.start_current(cx);
        } else {
            self.player.seek(Duration::ZERO);
        }
    }

    pub fn toggle_play(&mut self, _cx: &mut Context<Self>) {
        if self.playing {
            self.player.pause();
        } else {
            self.player.resume();
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.player.stop();
        self.playing = false;
        self.position = Duration::ZERO;
        self.duration = None;
        self.radio_title = None;
        self.scrobble.clear();
        self.media_keys.set_stopped();
        cx.notify();
    }

    pub fn seek(&mut self, position: Duration) {
        self.player.seek(position);
    }

    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        self.player.set_volume(self.volume);
        cx.notify();
    }

    // ----- media keys -----

    fn on_media_key(&mut self, event: souvlaki::MediaControlEvent, cx: &mut Context<Self>) {
        use souvlaki::MediaControlEvent as E;
        match event {
            E::Play | E::Pause | E::Toggle => self.toggle_play(cx),
            E::Next => self.next(cx),
            E::Previous => self.previous(cx),
            E::Stop => self.stop(cx),
            _ => {}
        }
    }

    /// Push current now-playing metadata to the OS media controls.
    fn sync_media_metadata(&mut self) {
        if let Some((title, subtitle)) = self.now_playing() {
            let album = self.current_song().and_then(|s| s.album.clone());
            // Convert local cache path to file:// URL for souvlaki.
            let cover_url_str: Option<String> = self
                .current_art_path
                .as_ref()
                .and_then(|p| p.to_str())
                .map(|s| format!("file://{s}"));
            self.media_keys.set_metadata(
                &title,
                &subtitle,
                album.as_deref(),
                self.duration,
                cover_url_str.as_deref(),
            );
        }
    }

    /// Fetch cover art for the current song and update OS media controls once
    /// the path is available.
    fn fetch_current_art(&mut self, cx: &mut Context<Self>) {
        let Some(cover_id) = self.queue.current_song().and_then(|s| s.cover_art.clone()) else {
            self.current_art_path = None;
            return;
        };
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(path) = crate::services::artwork::fetch(client, cover_id, 300).await {
                let _ = this.update(cx, |state, _cx| {
                    state.current_art_path = Some(path);
                    state.sync_media_metadata();
                });
            }
        })
        .detach();
    }

    // ----- internals -----

    /// Update streaming preferences applied to new stream URLs.
    pub fn set_transcoding(&mut self, opts: StreamOptions) {
        self.stream_opts = opts;
    }

    /// Apply playback-related settings from the settings view.
    pub fn apply_playback_settings(
        &mut self,
        scrobble_enabled: bool,
        default_shuffle: bool,
        default_repeat: RepeatMode,
        cx: &mut Context<Self>,
    ) {
        self.scrobble_enabled = scrobble_enabled;
        self.queue.shuffle = default_shuffle;
        self.queue.repeat = default_repeat;
        self.refresh_prefetch(cx);
        cx.notify();
    }

    pub fn set_scrobble_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.scrobble_enabled = enabled;
        cx.notify();
    }

    fn stream_url(&self, song: &Song) -> Result<String, String> {
        let client = self.client.as_ref().ok_or("not connected")?;
        client
            .stream_url(&song.id, &self.stream_opts)
            .map(|u| u.to_string())
            .map_err(|e| e.to_string())
    }

    fn start_current(&mut self, cx: &mut Context<Self>) {
        self.radio_title = None; // leaving radio for library playback
        let Some(song) = self.queue.current_song() else {
            return;
        };
        self.buffering = true;
        let song_id = song.id.clone();
        let song_clone = song.clone();
        let duration = song.duration.map(|s| Duration::from_secs(s as u64));
        match self.stream_url(song) {
            Ok(url) => {
                self.position = Duration::ZERO;
                self.duration = duration;
                self.last_error = None;
                self.current_art_path = None;
                self.player.play(TrackSource {
                    url,
                    duration_hint: duration,
                });
                push_recent(&mut self.recently_played, song_clone);
                self.refresh_prefetch(cx);
                let action = self.scrobble.start(song_id);
                self.fire_scrobble(action, cx);
                self.fetch_current_art(cx);
                self.sync_media_metadata();
                self.media_keys.set_playing(true, Duration::ZERO);
            }
            Err(e) => {
                self.buffering = false;
                self.last_error = Some(e);
            }
        }
        cx.notify();
    }

    /// Point the engine's prefetch slot at whatever plays after the current
    /// track (honoring repeat), or clear it.
    fn refresh_prefetch(&mut self, _cx: &mut Context<Self>) {
        let next = self.queue.next_pos().and_then(|pos| {
            // Repeat One prefetches the same song — still a valid transition.
            let song = self.queue.iter_ordered().nth(pos).map(|(_, s)| s)?;
            let duration = song.duration.map(|s| Duration::from_secs(s as u64));
            self.stream_url(song).ok().map(|url| TrackSource {
                url,
                duration_hint: duration,
            })
        });
        match next {
            Some(source) => self.player.prefetch_next(source),
            None => self.player.clear_prefetch(),
        }
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Position(pos) => {
                self.position = pos;
                let action = self.scrobble.on_position(pos, self.duration);
                self.fire_scrobble(action, cx);
            }
            Event::DurationKnown(d) => {
                self.duration = Some(d);
            }
            Event::Playing => {
                self.playing = true;
                self.buffering = false;
                self.media_keys.set_playing(true, self.position);
            }
            Event::Paused => {
                self.playing = false;
                self.media_keys.set_playing(false, self.position);
            }
            Event::Buffering => {
                self.buffering = true;
            }
            Event::TrackEnded { auto_advanced } => {
                if auto_advanced {
                    // Engine already started the prefetched track; move the
                    // queue pointer to match and set up the next prefetch.
                    if let Some(pos) = self.queue.next_pos() {
                        self.queue.advance_to(pos);
                    }
                    self.position = Duration::ZERO;
                    self.duration = self
                        .queue
                        .current_song()
                        .and_then(|s| s.duration)
                        .map(|s| Duration::from_secs(s as u64));
                    self.refresh_prefetch(cx);
                    if let Some(song) = self.queue.current_song() {
                        let action = self.scrobble.start(song.id.clone());
                        self.fire_scrobble(action, cx);
                    }
                    self.sync_media_metadata();
                    self.media_keys.set_playing(true, Duration::ZERO);
                } else {
                    self.playing = false;
                    if let Some(pos) = self.queue.next_pos() {
                        self.queue.advance_to(pos);
                        self.start_current(cx);
                    }
                }
            }
            Event::Failed(msg) => {
                self.playing = false;
                self.buffering = false;
                self.last_error = Some(msg);
            }
        }
        cx.notify();
    }
}

pub fn init(
    volume: f32,
    default_shuffle: bool,
    default_repeat: RepeatMode,
    scrobble_enabled: bool,
    cx: &mut gpui::App,
) -> Entity<PlayerState> {
    cx.new(|cx| {
        let mut state = PlayerState::new(volume, cx);
        state.queue.shuffle = default_shuffle;
        state.queue.repeat = default_repeat;
        state.scrobble_enabled = scrobble_enabled;
        state
    })
}

// ---- recently-played helpers ----

fn load_recent() -> Vec<Song> {
    let Ok(path) = crate::config::recent_played_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn push_recent(list: &mut Vec<Song>, song: Song) {
    list.retain(|s| s.id != song.id);
    list.insert(0, song);
    list.truncate(50);
    persist_recent(list);
}

fn persist_recent(list: &[Song]) {
    let Ok(path) = crate::config::recent_played_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(list) {
        let _ = std::fs::write(path, json);
    }
}
