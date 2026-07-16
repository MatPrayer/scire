//! OS media controls / media keys (macOS Now Playing, Linux MPRIS) via
//! souvlaki. Best-effort: if the platform backend fails to initialize the
//! rest of the app keeps working.

use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use tokio::sync::mpsc;

pub struct MediaKeys {
    controls: Option<MediaControls>,
}

impl MediaKeys {
    /// Create OS media controls and forward key presses to `tx`.
    pub fn new(tx: mpsc::UnboundedSender<MediaControlEvent>) -> Self {
        let config = PlatformConfig {
            display_name: "Navidrome Rusty Client",
            dbus_name: "navidrome_rusty_client",
            // Windows needs a window handle; macOS/Linux ignore it.
            hwnd: None,
        };
        let controls = match MediaControls::new(config) {
            Ok(mut controls) => {
                let attach = controls.attach(move |event| {
                    let _ = tx.send(event);
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
        Self { controls }
    }

    pub fn set_metadata(
        &mut self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration: Option<Duration>,
        cover_path: Option<&str>,
    ) {
        if let Some(c) = &mut self.controls {
            let _ = c.set_metadata(MediaMetadata {
                title: Some(title),
                artist: (!artist.is_empty()).then_some(artist),
                album,
                duration,
                cover_url: cover_path,
            });
        }
    }

    pub fn set_playing(&mut self, playing: bool, position: Duration) {
        if let Some(c) = &mut self.controls {
            let progress = Some(MediaPosition(position));
            let playback = if playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            };
            let _ = c.set_playback(playback);
        }
    }

    pub fn set_stopped(&mut self) {
        if let Some(c) = &mut self.controls {
            let _ = c.set_playback(MediaPlayback::Stopped);
        }
    }
}
