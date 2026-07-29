//! Player state entity: bridges the playback engine's events into gpui and
//! drives the play queue (shuffle/repeat/prefetch).

use std::time::Duration;

use gpui::{AppContext as _, Context, Entity};
use playback::{Event, Player, TrackSource};
use souvlaki::{MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use subsonic::{Song, StreamOptions, SubsonicClient};
use tokio::sync::mpsc;

use crate::config::{ReplayGainMode, Settings};
use crate::services::runtime;
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
    media_controls: Option<MediaControls>,
    /// Most-recently-played tracks (newest first, max 50). Persisted to disk.
    pub recently_played: Vec<Song>,
    /// Local path of the current track's cover art (for OS media controls).
    pub current_art_path: Option<std::path::PathBuf>,
    /// OS output device name, known once the engine opens the audio output.
    pub output_device: Option<String>,
    /// Clear the queue + player bar when playback reaches the queue end.
    clear_on_end: bool,
    /// ReplayGain mode applied to the effective playback volume.
    replay_gain_mode: ReplayGainMode,
    /// Linear gain factor for the current track from ReplayGain (1.0 = none).
    current_gain: f32,
    /// The engine has a track loaded. False after startup with a restored
    /// queue — play must (re)start the current song, not resume.
    engine_has_track: bool,
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

        // OS media keys / Now Playing: best-effort init, forward presses.
        let (media_tx, mut media_rx) = mpsc::unbounded_channel();
        let media_controls = PlatformConfig {
            display_name: "Scirè",
            dbus_name: "scire",
            hwnd: None,
        };
        let media_controls = match MediaControls::new(media_controls) {
            Ok(mut controls) => {
                let attach = controls.attach(move |event| {
                    let _ = media_tx.send(event);
                });
                if let Err(e) = attach {
                    tracing::warn!("media controls attach failed: {e:?}");
                    None
                } else {
                    Some(controls)
                }
            }
            Err(e) => {
                tracing::warn!("media controls unavailable: {e:?}");
                None
            }
        };
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
            media_controls,
            recently_played: load_recent(),
            current_art_path: None,
            output_device: None,
            clear_on_end: false,
            replay_gain_mode: ReplayGainMode::Off,
            current_gain: 1.0,
            engine_has_track: false,
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

    pub fn set_client(&mut self, client: Option<SubsonicClient>, cx: &mut Context<Self>) {
        self.client = client;
        // A restored queue shows the current track before any playback —
        // fetch its cover as soon as the server connection is available.
        if self.client.is_some()
            && self.current_art_path.is_none()
            && self.queue.current_song().is_some()
        {
            self.fetch_current_art(cx);
        }
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
        persist_queue(&self.queue);
        self.engine_has_track = true;
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
            path: None,
        });
        self.sync_media_metadata();
        if let Some(c) = &mut self.media_controls {
            let _ = c.set_playback(MediaPlayback::Playing {
                progress: Some(MediaPosition(Duration::ZERO)),
            });
        }
        cx.notify();
    }

    // ----- queue operations -----

    /// Replace the queue with `songs` and start playing at `index`.
    pub fn play_queue(&mut self, songs: Vec<Song>, index: usize, cx: &mut Context<Self>) {
        self.queue.replace(songs, index);
        self.start_current(cx);
    }

    /// Replace the queue with `songs`, enable shuffle, and start at a random
    /// song.
    pub fn play_queue_shuffled(&mut self, songs: Vec<Song>, cx: &mut Context<Self>) {
        if songs.is_empty() {
            return;
        }
        let start = rand::random_range(0..songs.len());
        self.queue.replace(songs, start);
        self.queue.set_shuffle(true);
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
            persist_queue(&self.queue);
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
            persist_queue(&self.queue);
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
        persist_queue(&self.queue);
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
        persist_queue(&self.queue);
        self.stop(cx);
    }

    /// Reorder a queue item (from the queue panel).
    pub fn move_queue_item(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.queue.move_item(from, to);
        persist_queue(&self.queue);
        self.refresh_prefetch(cx);
        cx.notify();
    }

    pub fn toggle_shuffle(&mut self, cx: &mut Context<Self>) {
        let on = !self.queue.shuffle;
        self.queue.set_shuffle(on);
        persist_queue(&self.queue);
        self.refresh_prefetch(cx);
        cx.notify();
    }

    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.queue.repeat = self.queue.repeat.cycle();
        persist_queue(&self.queue);
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

    pub fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playing {
            self.player.pause();
        } else if !self.engine_has_track && !self.is_radio() && self.current_song().is_some() {
            // Nothing loaded in the engine (restored queue or failed track):
            // resume would be a no-op, start the current song instead.
            self.start_current(cx);
        } else {
            self.player.resume();
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.player.stop();
        self.engine_has_track = false;
        self.playing = false;
        self.position = Duration::ZERO;
        self.duration = None;
        self.radio_title = None;
        self.scrobble.clear();
        if let Some(c) = &mut self.media_controls {
            let _ = c.set_playback(MediaPlayback::Stopped);
        }
        cx.notify();
    }

    pub fn seek(&mut self, position: Duration) {
        self.player.seek(position);
    }

    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = volume.clamp(0.0, 1.0);
        self.push_volume();
        cx.notify();
    }

    /// Effective engine volume = user volume × ReplayGain factor.
    fn effective_volume(&self) -> f32 {
        (self.volume * self.current_gain).clamp(0.0, 4.0)
    }

    /// Push the effective volume to the engine.
    fn push_volume(&self) {
        self.player.set_volume(self.effective_volume());
    }

    /// Resolve the effective ReplayGain mode: Auto picks Album when the queue
    /// is a single album, Track otherwise. Other modes pass through.
    fn effective_rg_mode(&self) -> ReplayGainMode {
        match self.replay_gain_mode {
            ReplayGainMode::Auto => {
                if self.queue_is_album() {
                    ReplayGainMode::Album
                } else {
                    ReplayGainMode::Track
                }
            }
            other => other,
        }
    }

    /// True when the queue holds more than one track and they all share the
    /// same album id (i.e. we're playing a whole album).
    fn queue_is_album(&self) -> bool {
        let mut album: Option<&str> = None;
        let mut count = 0usize;
        for (_, song) in self.queue.iter_ordered() {
            let Some(aid) = song.album_id.as_deref() else {
                return false;
            };
            match album {
                None => album = Some(aid),
                Some(a) if a == aid => {}
                _ => return false,
            }
            count += 1;
        }
        count > 1
    }

    /// Recompute the ReplayGain factor for the current track and reapply the
    /// effective volume. Radio and missing tags fall back to unity gain.
    fn recompute_gain(&mut self) {
        let mode = self.effective_rg_mode();
        self.current_gain = if self.is_radio() {
            1.0
        } else {
            self.current_song()
                .and_then(|s| s.replay_gain.as_ref())
                .map(|rg| replaygain_linear(rg, mode))
                .unwrap_or(1.0)
        };
        self.push_volume();
    }

    /// Clear the queue and player bar when the queue ends (from Settings).
    pub fn set_clear_on_end(&mut self, clear: bool, cx: &mut Context<Self>) {
        self.clear_on_end = clear;
        cx.notify();
    }

    /// Change ReplayGain mode (from Settings) and reapply to the current track.
    pub fn set_replay_gain(&mut self, mode: ReplayGainMode, cx: &mut Context<Self>) {
        self.replay_gain_mode = mode;
        self.recompute_gain();
        cx.notify();
    }

    /// Switch the audio output device (None = system default).
    pub fn set_output_device(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.player.set_output_device(name);
        cx.notify();
    }

    /// ReplayGain state for display when the mode is on (and not radio):
    /// (mode label, applied gain in dB). Auto shows the resolved mode marked
    /// `(auto)`. The gain is `None` when the current track has no usable tags.
    pub fn replay_gain_active(&self) -> Option<(String, Option<f32>)> {
        if self.replay_gain_mode == ReplayGainMode::Off || self.is_radio() {
            return None;
        }
        let mode = self.effective_rg_mode();
        let base = if mode == ReplayGainMode::Album {
            "album"
        } else {
            "track"
        };
        let label = if self.replay_gain_mode == ReplayGainMode::Auto {
            format!("{base} (auto)")
        } else {
            base.to_string()
        };
        let db = self
            .current_song()
            .and_then(|s| s.replay_gain.as_ref())
            .map(|rg| 20.0 * replaygain_linear(rg, mode).log10());
        Some((label, db))
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
        let Some((title, subtitle)) = self.now_playing() else {
            return;
        };
        let album = self.current_song().and_then(|s| s.album.clone());
        let cover_url_str: Option<String> = self
            .current_art_path
            .as_ref()
            .and_then(|p| p.to_str())
            .map(|s| format!("file://{s}"));
        if let Some(c) = &mut self.media_controls {
            let _ = c.set_metadata(MediaMetadata {
                title: Some(&title),
                artist: (!subtitle.is_empty()).then_some(subtitle.as_str()),
                album: album.as_deref(),
                duration: self.duration,
                cover_url: cover_url_str.as_deref(),
            });
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
        persist_queue(&self.queue);
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
        persist_queue(&self.queue);
        let Some(song) = self.queue.current_song() else {
            return;
        };
        self.buffering = true;
        let song_id = song.id.clone();
        let song_clone = song.clone();
        let duration = song.duration.map(|s| Duration::from_secs(s as u64));

        // Local file: use the filesystem path directly; URL is ignored for IO.
        let (url, path) = if let Some(local) = &song.local_path {
            ("local".to_string(), Some(std::path::PathBuf::from(local)))
        } else {
            match self.stream_url(song) {
                Ok(u) => (u, None),
                Err(e) => {
                    self.buffering = false;
                    self.last_error = Some(e);
                    cx.notify();
                    return;
                }
            }
        };

        self.position = Duration::ZERO;
        self.duration = duration;
        self.last_error = None;
        self.current_art_path = None;
        self.recompute_gain();
        self.player.play(TrackSource {
            url,
            duration_hint: duration,
            path,
        });
        self.engine_has_track = true;
        push_recent(&mut self.recently_played, song_clone);
        self.refresh_prefetch(cx);
        let action = self.scrobble.start(song_id);
        self.fire_scrobble(action, cx);
        self.fetch_current_art(cx);
        self.sync_media_metadata();
        if let Some(c) = &mut self.media_controls {
            let _ = c.set_playback(MediaPlayback::Playing {
                progress: Some(MediaPosition(Duration::ZERO)),
            });
        }
        cx.notify();
    }

    /// Point the engine's prefetch slot at whatever plays after the current
    /// track (honoring repeat), or clear it.
    fn refresh_prefetch(&mut self, _cx: &mut Context<Self>) {
        let next = self.queue.next_pos().and_then(|pos| {
            let song = self.queue.iter_ordered().nth(pos).map(|(_, s)| s)?;
            let duration = song.duration.map(|s| Duration::from_secs(s as u64));
            let (url, path) = if let Some(local) = &song.local_path {
                ("local".to_string(), Some(std::path::PathBuf::from(local)))
            } else {
                self.stream_url(song).ok().map(|u| (u, None))?
            };
            Some(TrackSource {
                url,
                duration_hint: duration,
                path,
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
                if let Some(c) = &mut self.media_controls {
                    let _ = c.set_playback(MediaPlayback::Playing {
                        progress: Some(MediaPosition(self.position)),
                    });
                }
            }
            Event::Paused => {
                self.playing = false;
                if let Some(c) = &mut self.media_controls {
                    let _ = c.set_playback(MediaPlayback::Paused {
                        progress: Some(MediaPosition(self.position)),
                    });
                }
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
                        persist_queue(&self.queue);
                    }
                    self.position = Duration::ZERO;
                    self.duration = self
                        .queue
                        .current_song()
                        .and_then(|s| s.duration)
                        .map(|s| Duration::from_secs(s as u64));
                    self.refresh_prefetch(cx);
                    // The gapless track carries its own ReplayGain.
                    self.recompute_gain();
                    if let Some(song) = self.queue.current_song() {
                        let action = self.scrobble.start(song.id.clone());
                        self.fire_scrobble(action, cx);
                    }
                    self.sync_media_metadata();
                    if let Some(c) = &mut self.media_controls {
                        let _ = c.set_playback(MediaPlayback::Playing {
                            progress: Some(MediaPosition(Duration::ZERO)),
                        });
                    }
                } else {
                    self.playing = false;
                    self.engine_has_track = false;
                    if let Some(pos) = self.queue.next_pos() {
                        self.queue.advance_to(pos);
                        self.start_current(cx);
                    } else if self.clear_on_end {
                        // Reached the queue end: clear it and reset the bar.
                        self.clear_queue(cx);
                    }
                }
            }
            Event::Failed(msg) => {
                self.playing = false;
                self.buffering = false;
                self.engine_has_track = false;
                self.last_error = Some(msg);
            }
            Event::OutputOpened { device } => {
                self.output_device = device;
            }
        }
        cx.notify();
    }
}

pub fn init(settings: &Settings, cx: &mut gpui::App) -> Entity<PlayerState> {
    cx.new(|cx| {
        let mut state = PlayerState::new(settings.volume, cx);
        state.replay_gain_mode = settings.replay_gain;
        state.clear_on_end = settings.queue_end == crate::config::QueueEndBehavior::Clear;
        if settings.output_device.is_some() {
            state
                .player
                .set_output_device(settings.output_device.clone());
        }
        match load_queue() {
            Some(queue) => {
                state.duration = queue
                    .current_song()
                    .and_then(|s| s.duration)
                    .map(|s| Duration::from_secs(s as u64));
                state.queue = queue;
            }
            None => {
                state.queue.shuffle = settings.default_shuffle;
                state.queue.repeat = settings.default_repeat;
            }
        }
        state.scrobble_enabled = settings.scrobble_enabled;
        state
    })
}

/// Linear gain factor from a ReplayGain block for the given mode. Applies the
/// base gain offset and clamps against the peak to avoid clipping.
fn replaygain_linear(rg: &subsonic::ReplayGain, mode: ReplayGainMode) -> f32 {
    let (gain_db, peak) = match mode {
        // Auto is resolved to Track/Album before this call; treat as Track.
        ReplayGainMode::Off => return 1.0,
        ReplayGainMode::Track | ReplayGainMode::Auto => {
            (rg.track_gain.or(rg.fallback_gain), rg.track_peak)
        }
        ReplayGainMode::Album => (
            rg.album_gain.or(rg.track_gain).or(rg.fallback_gain),
            rg.album_peak.or(rg.track_peak),
        ),
    };
    let Some(db) = gain_db else { return 1.0 };
    let base = rg.base_gain.unwrap_or(0.0);
    let mut g = 10f32.powf((db + base) / 20.0);
    if let Some(pk) = peak.filter(|&p| p > 0.0) {
        g = g.min(1.0 / pk); // clipping prevention
    }
    g.clamp(0.0, 4.0)
}

// ---- queue persistence ----

fn load_queue() -> Option<Queue> {
    let path = crate::config::queue_path().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let queue: Queue = serde_json::from_str(&text).ok()?;
    (queue.is_valid() && !queue.is_empty()).then_some(queue)
}

fn persist_queue(queue: &Queue) {
    let Ok(path) = crate::config::queue_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(queue) {
        let _ = std::fs::write(path, json);
    }
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
