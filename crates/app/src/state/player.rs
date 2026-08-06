//! Player state entity: bridges the playback engine's events into gpui and
//! drives the play queue (shuffle/repeat/prefetch).

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity};
use playback::{Event, Player, TrackSource};
use souvlaki::{MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use subsonic::{Song, StreamOptions, SubsonicClient};
use tokio::sync::mpsc;

use crate::config::{ReplayGainMode, Settings};
use crate::services::{artwork, runtime};
use crate::state::queue::{Queue, RepeatMode};
use crate::state::scrobble::{ScrobbleAction, ScrobbleTracker};

/// Cover size fetched for the player bar / OS media controls.
const ART_SIZE: u32 = 300;

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
    /// Set while a live radio stream is playing: the station's name as
    /// bookmarked. Doubles as the "this is radio" flag.
    radio_title: Option<String>,
    /// Track the station says it is playing right now (ICY `StreamTitle`),
    /// which changes many times over one "track" as far as the queue knows.
    radio_stream_title: Option<String>,
    /// What the station said about itself when the stream opened.
    radio_station: Option<playback::icy::StationInfo>,
    media_controls: Option<MediaControls>,
    /// Most-recently-played tracks (newest first, max 50). Persisted to disk.
    pub recently_played: Vec<Song>,
    /// Local path of the current track's cover art (for OS media controls).
    pub current_art_path: Option<std::path::PathBuf>,
    /// Art cache key `current_art_path` belongs to (album-scoped — Navidrome
    /// gives every track its own cover id). Consecutive tracks off one album
    /// share it, so the art is kept instead of re-fetched between them.
    current_art_key: Option<String>,
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
    /// Mirror of the waveform seek-bar setting: peaks are only worth computing
    /// ahead of time when something is going to draw them.
    waveform_enabled: bool,
    /// Song id whose peaks were last prewarmed, so the work fires once.
    waveform_prewarmed_for: Option<String>,
    /// Remember the playback position across runs (from Settings).
    resume_enabled: bool,
    /// Position restored at startup with the song id it belongs to, waiting for
    /// that track to actually be playing. Seeking a stream that has not opened
    /// yet does nothing, so the seek is deferred to its first `Playing` event —
    /// and the id is kept because the user may skip before ever pressing play.
    pending_resume: Option<(String, Duration)>,
    /// Whole seconds last written to the resume file, so the position events
    /// (several a second) only touch the disk when the value moved.
    resume_written_secs: u64,
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
            radio_stream_title: None,
            radio_station: None,
            media_controls,
            recently_played: load_recent(),
            current_art_path: None,
            current_art_key: None,
            output_device: None,
            clear_on_end: false,
            replay_gain_mode: ReplayGainMode::Off,
            current_gain: 1.0,
            engine_has_track: false,
            waveform_enabled: false,
            waveform_prewarmed_for: None,
            resume_enabled: false,
            pending_resume: None,
            resume_written_secs: 0,
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
            self.refresh_current_art(cx);
        }
    }

    pub fn current_song(&self) -> Option<&Song> {
        self.queue.current_song()
    }

    /// Live window onto the samples reaching the audio device, for the
    /// visualizer. Cheap to clone and safe to read from the UI thread.
    pub fn spectrum_tap(&self) -> Arc<playback::spectrum::SpectrumTap> {
        self.player.spectrum_tap()
    }

    /// Display info for the player bar: (title, subtitle). Radio wins over the
    /// queue when a live stream is playing.
    pub fn now_playing(&self) -> Option<(String, String)> {
        if let Some(station) = &self.radio_title {
            // With ICY metadata the interesting line is the track, and the
            // station becomes the subtitle; without it the station is all there
            // is to show.
            return Some(match self.radio_now_playing() {
                Some((artist, title)) => (
                    title,
                    match artist {
                        Some(artist) => format!("{artist} · {station}"),
                        None => station.clone(),
                    },
                ),
                None => (station.clone(), "Live radio".into()),
            });
        }
        self.current_song()
            .map(|s| (s.title.clone(), s.artist.clone().unwrap_or_default()))
    }

    /// True while a live radio stream is playing (seek is meaningless).
    pub fn is_radio(&self) -> bool {
        self.radio_title.is_some()
    }

    /// The station's name as bookmarked, while radio is playing.
    pub fn radio_station_name(&self) -> Option<&str> {
        self.radio_title.as_deref()
    }

    /// The station's current track, split into `(artist, title)`.
    ///
    /// Stations publish one string, by overwhelming convention
    /// `Artist - Title`; splitting on the *first* separator is what every
    /// other client does. A title containing " - " loses a word to the artist
    /// line, which is a better failure than never showing the artist at all.
    pub fn radio_now_playing(&self) -> Option<(Option<String>, String)> {
        let raw = self.radio_stream_title.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        Some(match raw.split_once(" - ") {
            Some((artist, title)) if !artist.trim().is_empty() && !title.trim().is_empty() => {
                (Some(artist.trim().to_string()), title.trim().to_string())
            }
            _ => (None, raw.to_string()),
        })
    }

    /// Forget everything about the station being played, on leaving radio.
    fn clear_radio(&mut self) {
        self.radio_title = None;
        self.radio_stream_title = None;
        self.radio_station = None;
    }

    /// Codec / bitrate / genre the station advertises, as one line.
    pub fn radio_info_line(&self) -> Option<String> {
        let station = self.radio_station.as_ref()?;
        let mut parts: Vec<String> = Vec::new();
        if let Some(format) = &station.format {
            parts.push(format.clone());
        }
        if let Some(kbps) = station.bitrate.filter(|b| *b > 0) {
            parts.push(format!("{kbps} kbps"));
        }
        if let Some(genre) = &station.genre {
            parts.push(genre.clone());
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Play a live internet-radio stream by its external URL.
    pub fn play_radio(&mut self, name: String, stream_url: String, cx: &mut Context<Self>) {
        self.queue.clear();
        persist_queue(&self.queue);
        self.engine_has_track = true;
        self.scrobble.clear();
        self.player.clear_prefetch();
        self.radio_title = Some(name);
        self.radio_stream_title = None;
        self.radio_station = None;
        self.position = Duration::ZERO;
        self.duration = None;
        self.last_error = None;
        self.buffering = true;
        self.player.play(TrackSource {
            url: stream_url,
            duration_hint: None,
            path: None,
            id: None,
        });
        // Radio has no queue song behind it: drop the last track's cover.
        self.refresh_current_art(cx);
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
        // Nothing is playing any more, so there is no position to come back to.
        self.pending_resume = None;
        self.resume_written_secs = 0;
        clear_resume();
        self.position = Duration::ZERO;
        self.duration = None;
        self.clear_radio();
        self.scrobble.clear();
        if let Some(c) = &mut self.media_controls {
            let _ = c.set_playback(MediaPlayback::Stopped);
        }
        // Stopping can leave no current song at all (the queue ran out, or its
        // last entry was removed). The cover has to follow: left alone it keeps
        // pointing at the track that just ended, and the player bar then draws
        // stale art underneath its own "nothing playing" placeholder. When a
        // song *is* still current this is a no-op.
        self.refresh_current_art(cx);
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
        // Nothing loaded means nothing is being normalised — reporting a mode
        // here leaves a stray "RG · album" line under an empty player.
        if self.replay_gain_mode == ReplayGainMode::Off
            || self.is_radio()
            || self.current_song().is_none()
        {
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

    /// Point the cover art at whatever is current, fetching it only when the
    /// art actually changed. Tracks from one album share an art key, so the art
    /// (and the OS media-control metadata) survives the switch untouched
    /// instead of blanking and reloading between them.
    fn refresh_current_art(&mut self, cx: &mut Context<Self>) {
        // Local track: resolve cover from local_art_path directly.
        if let Some(song) = self.current_song()
            && song.local_path.is_some()
            && let Some(hash) = &song.cover_art
            && let Some(path) = crate::services::local_library::local_art_path(hash)
            && path.exists()
        {
            let new_key = Some(hash.clone());
            if new_key == self.current_art_key && self.current_art_path.is_some() {
                return;
            }
            self.current_art_key = new_key;
            self.current_art_path = Some(path);
            self.sync_media_metadata();
            return;
        }
        let cover = self.current_song().and_then(artwork::song_cover);
        let new_key = cover.as_ref().map(|(_, key)| key.clone());
        if new_key == self.current_art_key && (new_key.is_none() || self.current_art_path.is_some())
        {
            return;
        }
        self.current_art_key = new_key;
        let Some((cover_id, key)) = cover else {
            self.current_art_path = None;
            return;
        };
        // Already on disk (previous play, album view, prewarm): adopt it now so
        // the player bar never renders a frame without art.
        if let Some(path) = artwork::cached(&key, ART_SIZE) {
            self.current_art_path = Some(path);
            self.sync_media_metadata();
            return;
        }
        self.current_art_path = None;
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(path) = artwork::fetch_as(client, cover_id, key.clone(), ART_SIZE).await {
                let _ = this.update(cx, |state, cx| {
                    // Playback may have moved to another album meanwhile.
                    if state.current_art_key.as_deref() == Some(key.as_str()) {
                        state.current_art_path = Some(path);
                        state.sync_media_metadata();
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Whether the waveform seek bar is on. Peaks for the next track are only
    /// precomputed when something will draw them.
    pub fn set_waveform_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.waveform_enabled != enabled {
            self.waveform_enabled = enabled;
            self.prewarm_next_waveform(cx);
        }
    }

    /// Decode the next track's waveform peaks now, while the current track is
    /// still playing, so the seek bar has them the moment it starts instead of
    /// downloading and decoding a whole track under the user.
    fn prewarm_next_waveform(&mut self, cx: &mut Context<Self>) {
        if !self.waveform_enabled {
            return;
        }
        let Some(song_id) = self
            .queue
            .next_pos()
            .and_then(|pos| self.queue.iter_ordered().nth(pos))
            .map(|(_, s)| s.id.clone())
        else {
            return;
        };
        // Repeat One points at the current track, which already has its peaks.
        if self.waveform_prewarmed_for.as_deref() == Some(song_id.as_str())
            || self.current_song().map(|s| s.id.as_str()) == Some(song_id.as_str())
        {
            return;
        }
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let Ok(url) = client.stream_url(&song_id, &crate::services::waveform::stream_options())
        else {
            return;
        };
        self.waveform_prewarmed_for = Some(song_id.clone());
        let url = url.to_string();
        cx.spawn(async move |_, _| {
            if let Err(e) =
                runtime::spawn_io(crate::services::waveform::fetch_peaks(url, song_id)).await
            {
                tracing::debug!("waveform prewarm failed: {e:#}");
            }
        })
        .detach();
    }

    /// Warm the disk cache with the next track's cover so a hand-over into a
    /// different album shows art immediately. No-op when it is the same album
    /// or already cached.
    fn prewarm_next_art(&self, cx: &mut Context<Self>) {
        let Some((cover_id, key)) = self
            .queue
            .next_pos()
            .and_then(|pos| self.queue.iter_ordered().nth(pos))
            .and_then(|(_, s)| artwork::song_cover(s))
        else {
            return;
        };
        if Some(&key) == self.current_art_key.as_ref() || artwork::cached(&key, ART_SIZE).is_some()
        {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let _ = artwork::fetch_as(client, cover_id, key, ART_SIZE).await;
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

    /// Remember the position within the current track across runs (Settings).
    /// Turning it off drops whatever was already saved, so a later launch with
    /// it back on doesn't resume a track from a session two days ago.
    pub fn set_resume_playback(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.resume_enabled = enabled;
        if enabled {
            self.persist_resume(true);
        } else {
            self.pending_resume = None;
            clear_resume();
        }
        cx.notify();
    }

    /// Write the current track + position to the resume file. Throttled to once
    /// per whole second unless `force`d: `Event::Position` fires several times a
    /// second and each save is a file write.
    fn persist_resume(&mut self, force: bool) {
        if !self.resume_enabled || self.is_radio() {
            return;
        }
        let Some(song_id) = self.current_song().map(|s| s.id.clone()) else {
            return;
        };
        let secs = self.position.as_secs();
        if !force && secs == self.resume_written_secs {
            return;
        }
        self.resume_written_secs = secs;
        // Right at the start of a track there is nothing worth resuming, and
        // the file would only make the next launch seek to zero.
        if self.position < RESUME_MIN {
            clear_resume();
            return;
        }
        persist_resume_state(&ResumeState {
            song_id,
            position_secs: self.position.as_secs_f64(),
        });
    }

    fn stream_url(&self, song: &Song) -> Result<String, String> {
        let client = self.client.as_ref().ok_or("not connected")?;
        client
            .stream_url(&song.id, &self.stream_opts)
            .map(|u| u.to_string())
            .map_err(|e| e.to_string())
    }

    fn start_current(&mut self, cx: &mut Context<Self>) {
        self.clear_radio(); // leaving radio for library playback
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

        // A pending resume belongs to one track; starting anything else (the
        // user skipped before pressing play) drops it and begins at zero.
        let resume = resume_for(self.pending_resume.as_ref(), &song_id);
        if resume.is_none() {
            self.pending_resume = None;
        }
        self.position = resume.unwrap_or(Duration::ZERO);
        self.resume_written_secs = self.position.as_secs();
        self.duration = duration;
        self.last_error = None;
        // Apply this track's ReplayGain before playback opens so the engine
        // starts the sink at the normalized volume.
        self.recompute_gain();
        self.player.play(TrackSource {
            url,
            duration_hint: duration,
            path,
            id: Some(song_id.clone()),
        });
        self.engine_has_track = true;
        push_recent(&mut self.recently_played, song_clone);
        // Art first: the prefetch prewarms the *next* cover against the
        // current one, so the current one has to be settled by then.
        self.refresh_current_art(cx);
        self.refresh_prefetch(cx);
        let action = self.scrobble.start(song_id);
        self.fire_scrobble(action, cx);
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
    fn refresh_prefetch(&mut self, cx: &mut Context<Self>) {
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
                id: Some(song.id.clone()),
            })
        });
        match next {
            Some(source) => self.player.prefetch_next(source),
            None => self.player.clear_prefetch(),
        }
        self.prewarm_next_art(cx);
        self.prewarm_next_waveform(cx);
    }

    /// Queue position the engine actually moved into on a gapless hand-over.
    /// The next track is committed to the audio queue a few seconds ahead, so a
    /// queue edit in that window can leave `next_pos` pointing elsewhere —
    /// then the id the engine started wins.
    fn advanced_pos(&self, started: Option<&str>) -> Option<usize> {
        let next = self.queue.next_pos();
        let Some(id) = started else { return next };
        if next.is_some_and(|pos| {
            self.queue
                .iter_ordered()
                .nth(pos)
                .is_some_and(|(_, s)| s.id == id)
        }) {
            return next;
        }
        self.queue
            .iter_ordered()
            .find(|(_, s)| s.id == id)
            .map(|(pos, _)| pos)
            .or(next)
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Position(pos) => {
                self.position = pos;
                let action = self.scrobble.on_position(pos, self.duration);
                self.fire_scrobble(action, cx);
                // Saved as playback goes rather than only on quit: a crash or a
                // SIGKILL never gets to run a shutdown hook, and this costs one
                // small write a second.
                self.persist_resume(false);
            }
            Event::DurationKnown(d) => {
                self.duration = Some(d);
            }
            Event::Playing => {
                self.playing = true;
                self.buffering = false;
                // The restored position is applied here, not in `start_current`:
                // the engine can only seek a source it has actually opened.
                if let Some((id, pos)) = self.pending_resume.clone()
                    && self.current_song().is_some_and(|s| s.id == id)
                {
                    self.pending_resume = None;
                    self.position = pos;
                    self.player.seek(pos);
                }
                if let Some(c) = &mut self.media_controls {
                    let _ = c.set_playback(MediaPlayback::Playing {
                        progress: Some(MediaPosition(self.position)),
                    });
                }
            }
            Event::Paused => {
                self.playing = false;
                // Pausing is the likeliest thing to precede a quit; don't wait
                // for the throttle window to come round.
                self.persist_resume(true);
                if let Some(c) = &mut self.media_controls {
                    let _ = c.set_playback(MediaPlayback::Paused {
                        progress: Some(MediaPosition(self.position)),
                    });
                }
            }
            Event::Buffering => {
                self.buffering = true;
            }
            Event::TrackEnded {
                auto_advanced,
                started,
            } => {
                if auto_advanced {
                    // Engine already flowed into the prefetched track; move the
                    // queue pointer to match and set up the next prefetch.
                    if let Some(pos) = self.advanced_pos(started.as_deref()) {
                        self.queue.advance_to(pos);
                        persist_queue(&self.queue);
                    }
                    self.position = Duration::ZERO;
                    self.duration = self
                        .queue
                        .current_song()
                        .and_then(|s| s.duration)
                        .map(|s| Duration::from_secs(s as u64));
                    // Same album as the track that just ended: the art stays as
                    // it is. A new album swaps it (prewarmed, so it is instant).
                    self.refresh_current_art(cx);
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
            Event::StationInfo(info) => {
                // Only a live stream produces this, but a library track that
                // somehow did must not turn the UI into radio.
                if self.is_radio() {
                    self.radio_station = Some(info);
                }
            }
            Event::StreamTitle(title) => {
                if self.is_radio() {
                    self.radio_stream_title = title;
                    // The OS now-playing panel should follow the station, not
                    // sit on the station name for the whole session.
                    self.sync_media_metadata();
                }
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
        state.waveform_enabled = settings.waveform_seekbar;
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
        state.resume_enabled = settings.resume_playback;
        // Restore the saved position, but only for the track the restored queue
        // is actually sitting on — the queue file and the resume file are
        // written at different moments and can disagree.
        if state.resume_enabled
            && let Some(saved) = load_resume()
            && state
                .queue
                .current_song()
                .is_some_and(|s| s.id == saved.song_id)
        {
            let pos = Duration::from_secs_f64(saved.position_secs.max(0.0));
            // Shown in the player bar right away, so the restored track reads
            // as paused mid-way instead of at 0:00 until playback starts.
            state.position = pos;
            state.resume_written_secs = pos.as_secs();
            state.pending_resume = Some((saved.song_id, pos));
        }
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

// ---- playback-position persistence (Settings::resume_playback) ----

/// Below this the track has barely started; resuming it is indistinguishable
/// from playing it from the top, so nothing is stored.
const RESUME_MIN: Duration = Duration::from_secs(5);

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState {
    song_id: String,
    position_secs: f64,
}

fn load_resume() -> Option<ResumeState> {
    read_resume_at(&crate::config::resume_path().ok()?)
}

fn read_resume_at(path: &std::path::Path) -> Option<ResumeState> {
    let text = std::fs::read_to_string(path).ok()?;
    let state: ResumeState = serde_json::from_str(&text).ok()?;
    // A non-finite position would panic `Duration::from_secs_f64`.
    state.position_secs.is_finite().then_some(state)
}

fn persist_resume_state(state: &ResumeState) {
    let Ok(path) = crate::config::resume_path() else {
        return;
    };
    write_resume_at(&path, state);
}

fn write_resume_at(path: &std::path::Path, state: &ResumeState) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(path, json);
    }
}

/// Position to start `song_id` at, given whatever resume is pending. A resume
/// is only ever applied to the track it was saved for.
fn resume_for(pending: Option<&(String, Duration)>, song_id: &str) -> Option<Duration> {
    pending.filter(|(id, _)| id == song_id).map(|(_, pos)| *pos)
}

fn clear_resume() {
    if let Ok(path) = crate::config::resume_path() {
        let _ = std::fs::remove_file(path);
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ResumeState, read_resume_at, resume_for, write_resume_at};

    #[test]
    fn resume_state_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join("scire-resume-roundtrip");
        let path = dir.join("resume.json");
        let _ = std::fs::remove_file(&path);
        write_resume_at(
            &path,
            &ResumeState {
                song_id: "song-1".into(),
                position_secs: 91.5,
            },
        );
        let back = read_resume_at(&path).unwrap();
        assert_eq!(back.song_id, "song-1");
        assert!((back.position_secs - 91.5).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_resume_file_is_ignored() {
        let dir = std::env::temp_dir().join("scire-resume-broken");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.json");
        assert!(read_resume_at(&missing).is_none());
        // A NaN position would panic Duration::from_secs_f64 downstream.
        let nan = dir.join("nan.json");
        std::fs::write(&nan, r#"{"song_id":"s","position_secs":null}"#).unwrap();
        assert!(read_resume_at(&nan).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_applies_only_to_the_track_it_was_saved_for() {
        let pending = ("song-1".to_string(), Duration::from_secs(42));
        assert_eq!(
            resume_for(Some(&pending), "song-1"),
            Some(Duration::from_secs(42))
        );
        assert_eq!(resume_for(Some(&pending), "song-2"), None);
        assert_eq!(resume_for(None, "song-1"), None);
    }
}
