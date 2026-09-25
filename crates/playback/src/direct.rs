//! Direct output: a DAC opened on its own, without the sound server.
//!
//! On Linux that is an ALSA `hw:` device. PulseAudio/PipeWire resample
//! everything to one rate and mix it with every other program; `hw:` is the
//! card itself, which takes one stream, at a rate and format it supports, and
//! does nothing to the samples. Opening it at the track's own rate is what
//! makes playback bit-perfect: symphonia turns 16- and 24-bit PCM into `f32`
//! by dividing by a power of two, which `f32` holds exactly, and the way back
//! to an integer format multiplies by the same power — so with the volume at
//! exactly 1.0 and nothing else in the mixer, the DAC receives the file's
//! samples unchanged.
//!
//! The cost is exclusivity. While the sound server has the card open (another
//! program playing through it, or our own shared stream in its last seconds
//! before the server suspends the idle node) the open fails as busy, and while
//! we hold it nothing else can play there.
//!
//! macOS has no equivalent through cpal (hog mode is not exposed), so there
//! the device list is empty and opening fails with a message saying so.

use rodio::cpal::SampleFormat;

/// A DAC that can be opened directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectDevice {
    /// What the engine opens it by — an ALSA PCM id such as
    /// `hw:CARD=Audio,DEV=0`. Card names rather than indices, so the id
    /// survives the cards being enumerated in another order after a reboot.
    pub id: String,
    /// What a person calls it: the card and the PCM on it.
    pub name: String,
}

/// What the track about to play needs from the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Want {
    pub rate: u32,
    pub channels: u16,
}

/// What a direct output was opened at, and for what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectFormat {
    pub rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// The request this was the best answer to. A later track asking the same
    /// gets the same output, so gapless holds; asking anything else reopens.
    pub want: Want,
}

impl DirectFormat {
    /// Whether this output was opened for a track shaped like `want`.
    pub fn serves(&self, want: Want) -> bool {
        self.want == want
    }

    /// Whether the samples reach the card as decoded: no rate conversion.
    /// A channel count other than the track's only duplicates or drops
    /// channels, which changes no sample, but is still reported.
    pub fn bit_perfect(&self) -> bool {
        self.rate == self.want.rate && self.channels == self.want.channels
    }

    /// `44.1 kHz · 24-bit`, plus what was changed when the card could not take
    /// the track as it is.
    pub fn label(&self) -> String {
        let mut out = format!("{} · {}", khz(self.rate), bits(self.format));
        if self.bit_perfect() {
            return out;
        }
        if self.rate != self.want.rate {
            out.push_str(&format!(" (resampled from {})", khz(self.want.rate)));
        } else if self.channels != self.want.channels {
            out.push_str(&format!(
                " ({} ch → {} ch)",
                self.want.channels, self.channels
            ));
        }
        out
    }
}

fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1000.)
    }
}

fn bits(format: SampleFormat) -> &'static str {
    match format {
        SampleFormat::I16 | SampleFormat::U16 => "16-bit",
        SampleFormat::I24 | SampleFormat::U24 => "24-bit",
        SampleFormat::I32 | SampleFormat::U32 => "32-bit",
        SampleFormat::F32 => "32-bit float",
        SampleFormat::F64 => "64-bit float",
        _ => "8-bit",
    }
}

/// One of a card's supported configurations, reduced to what planning reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Range {
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    pub format: SampleFormat,
}

/// How much a sample format is worth to us. Wide integers first: 16- and
/// 24-bit samples both land in them unchanged. `f32` is exact too, but few
/// cards take it. 16-bit last, since it truncates a 24-bit source.
fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::I32 => Some(5),
        SampleFormat::I24 => Some(4),
        SampleFormat::F32 => Some(3),
        SampleFormat::I16 => Some(2),
        // Unsigned and 8-bit formats exist on odd hardware only; a converted
        // stream there is still better than none.
        SampleFormat::U32 | SampleFormat::U24 | SampleFormat::U16 => Some(1),
        _ => None,
    }
}

/// Pick the configuration to open a card at for `want`.
///
/// The track's rate matters most — a rate the card lacks means resampling,
/// which is exactly what direct output is for avoiding — then its channel
/// count, then the sample format. A card that cannot take the rate at all
/// gets the nearest one it can, above rather than below where there is a
/// choice, so the result is still something that plays.
pub(crate) fn plan(ranges: &[Range], want: Want) -> Option<DirectFormat> {
    ranges
        .iter()
        .filter_map(|r| {
            let rank = format_rank(r.format)?;
            let rate = want.rate.clamp(r.min_rate, r.max_rate);
            let exact_rate = rate == want.rate;
            let exact_channels = r.channels == want.channels;
            // Not the track's count: stereo is what everything plays on.
            let stereo = r.channels == 2;
            let key = (
                exact_rate,
                exact_channels,
                stereo,
                rank,
                // Closest rate, and above before below.
                std::cmp::Reverse(rate.abs_diff(want.rate)),
                rate >= want.rate,
            );
            Some((key, r, rate))
        })
        .max_by_key(|(key, _, _)| *key)
        .map(|(_, r, rate)| DirectFormat {
            rate,
            channels: r.channels,
            format: r.format,
            want,
        })
}

/// Why a direct open failed.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// Something else has the card — worth waiting a moment and retrying,
    /// since the likeliest something is the sound server letting go.
    Busy(String),
    Other(String),
}

impl OpenError {
    pub fn message(self) -> String {
        match self {
            Self::Busy(m) | Self::Other(m) => m,
        }
    }
}

/// The DACs that can be opened directly.
pub fn devices() -> Vec<DirectDevice> {
    #[cfg(target_os = "linux")]
    {
        linux::devices()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Open `id` for a track shaped like `want`.
pub(crate) fn open(
    id: &str,
    want: Want,
) -> Result<(rodio::MixerDeviceSink, DirectFormat, String), OpenError> {
    #[cfg(target_os = "linux")]
    {
        linux::open(id, want)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (id, want);
        Err(OpenError::Other(
            "direct output is only available on Linux".into(),
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::num::NonZero;

    use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _};
    use rodio::cpal::{self, BuildStreamError, SupportedStreamConfigsError};

    use super::{DirectDevice, DirectFormat, OpenError, Range, Want, plan};

    fn host() -> Option<cpal::Host> {
        cpal::host_from_id(cpal::HostId::Alsa).ok()
    }

    /// The device's ALSA id, with a card index swapped for the card's name.
    fn pcm_id(device: &cpal::Device) -> Option<String> {
        device.id().ok().map(|id| stable_id(&id.1, card_name))
    }

    /// ALSA's own short name for card `index` (`/proc/asound/card0/id`).
    fn card_name(index: &str) -> Option<String> {
        std::fs::read_to_string(format!("/proc/asound/card{index}/id"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `hw:CARD=0,DEV=0` → `hw:CARD=Audio,DEV=0`. Card indices follow the
    /// order the cards were found in, which a USB DAC plugged in before or
    /// after boot changes; the name stays. ALSA opens either spelling.
    pub(super) fn stable_id(id: &str, name_of: impl Fn(&str) -> Option<String>) -> String {
        if let Some(rest) = id.strip_prefix("hw:CARD=") {
            let (card, tail) = rest.split_once(',').unwrap_or((rest, ""));
            if card.chars().all(|c| c.is_ascii_digit())
                && let Some(name) = name_of(card)
            {
                let sep = if tail.is_empty() { "" } else { "," };
                return format!("hw:CARD={name}{sep}{tail}");
            }
        }
        id.to_string()
    }

    fn device_name(device: &cpal::Device) -> Option<String> {
        device.description().ok().map(|d| d.name().to_string())
    }

    /// A card index in the id (`hw:CARD=0,…`) rather than its name. cpal lists
    /// every card both ways; the named one is kept.
    fn numbered(id: &str) -> bool {
        id.strip_prefix("hw:CARD=")
            .and_then(|rest| rest.split(',').next())
            .is_some_and(|card| card.chars().all(|c| c.is_ascii_digit()))
    }

    pub(super) fn devices() -> Vec<DirectDevice> {
        let Some(host) = host() else {
            return Vec::new();
        };
        let Ok(list) = host.output_devices() else {
            return Vec::new();
        };
        let mut found: Vec<DirectDevice> = Vec::new();
        for device in list {
            let Some(id) = pcm_id(&device).filter(|id| id.starts_with("hw:")) else {
                continue;
            };
            let name = device_name(&device).unwrap_or_else(|| id.clone());
            match found.iter_mut().find(|d| d.name == name) {
                // The same card seen by index and by name: keep the name.
                Some(seen) if numbered(&seen.id) && !numbered(&id) => seen.id = id,
                Some(_) => {}
                None => found.push(DirectDevice { id, name }),
            }
        }
        found
    }

    pub(super) fn open(
        id: &str,
        want: Want,
    ) -> Result<(rodio::MixerDeviceSink, DirectFormat, String), OpenError> {
        let host = host().ok_or_else(|| OpenError::Other("ALSA is not available".into()))?;
        let device = host
            .output_devices()
            .map_err(|e| OpenError::Other(e.to_string()))?
            .find(|d| pcm_id(d).as_deref() == Some(id))
            .ok_or_else(|| OpenError::Other(format!("{id} is not connected")))?;
        let name = device_name(&device).unwrap_or_else(|| id.to_string());
        let busy = || {
            OpenError::Busy(format!(
                "{name} is busy — another program (or the sound server) is using it"
            ))
        };
        let ranges: Vec<Range> = device
            .supported_output_configs()
            .map_err(|e| match e {
                SupportedStreamConfigsError::DeviceNotAvailable => busy(),
                e => OpenError::Other(e.to_string()),
            })?
            .map(|r| Range {
                channels: r.channels(),
                min_rate: r.min_sample_rate(),
                max_rate: r.max_sample_rate(),
                format: r.sample_format(),
            })
            .collect();
        let format = plan(&ranges, want)
            .ok_or_else(|| OpenError::Other(format!("{name} offers no usable format")))?;
        let (Some(rate), Some(channels)) =
            (NonZero::new(format.rate), NonZero::new(format.channels))
        else {
            return Err(OpenError::Other(format!("{name} reported an empty format")));
        };
        let mut sink = rodio::DeviceSinkBuilder::default()
            .with_device(device)
            .with_sample_rate(rate)
            .with_channels(channels)
            .with_sample_format(format.format)
            .open_stream()
            .map_err(|e| match e {
                rodio::DeviceSinkError::BuildError(BuildStreamError::DeviceNotAvailable) => busy(),
                e => OpenError::Other(format!("{name}: {e}")),
            })?;
        // Reopened on every rate change; the drop is routine, not news.
        sink.log_on_drop(false);
        Ok((sink, format, name))
    }

    #[cfg(test)]
    mod tests {
        use super::{numbered, stable_id};

        #[test]
        fn a_card_index_is_told_from_a_card_name() {
            assert!(numbered("hw:CARD=0,DEV=0"));
            assert!(!numbered("hw:CARD=Audio,DEV=0"));
        }

        #[test]
        fn a_card_index_becomes_the_card_name() {
            let names = |i: &str| (i == "0").then(|| "Audio".to_string());
            assert_eq!(stable_id("hw:CARD=0,DEV=0", names), "hw:CARD=Audio,DEV=0");
            // Unknown index, or already a name: left alone.
            assert_eq!(stable_id("hw:CARD=5,DEV=0", names), "hw:CARD=5,DEV=0");
            assert_eq!(
                stable_id("hw:CARD=Audio,DEV=1", names),
                "hw:CARD=Audio,DEV=1"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(channels: u16, min_rate: u32, max_rate: u32, format: SampleFormat) -> Range {
        Range {
            channels,
            min_rate,
            max_rate,
            format,
        }
    }

    const CD: Want = Want {
        rate: 44_100,
        channels: 2,
    };

    /// A typical USB DAC: 16- and 32-bit, 44.1k–384k, stereo only.
    fn usb_dac() -> Vec<Range> {
        vec![
            range(2, 44_100, 384_000, SampleFormat::I16),
            range(2, 44_100, 384_000, SampleFormat::I32),
        ]
    }

    #[test]
    fn the_track_rate_and_the_widest_integer_format_win() {
        let f = plan(&usb_dac(), CD).unwrap();
        assert_eq!(
            (f.rate, f.channels, f.format),
            (44_100, 2, SampleFormat::I32)
        );
        assert!(f.bit_perfect());
        assert_eq!(f.label(), "44.1 kHz · 32-bit");
    }

    #[test]
    fn a_hi_res_track_opens_at_its_own_rate() {
        let want = Want {
            rate: 96_000,
            channels: 2,
        };
        let f = plan(&usb_dac(), want).unwrap();
        assert_eq!(f.rate, 96_000);
        assert!(f.serves(want));
        assert!(!f.serves(CD));
    }

    /// Rate outranks format: a 16-bit config at the right rate beats a 32-bit
    /// one that would need resampling.
    #[test]
    fn the_rate_outranks_the_format() {
        let ranges = vec![
            range(2, 48_000, 48_000, SampleFormat::I32),
            range(2, 44_100, 48_000, SampleFormat::I16),
        ];
        let f = plan(&ranges, CD).unwrap();
        assert_eq!((f.rate, f.format), (44_100, SampleFormat::I16));
    }

    /// A card without the rate still plays — at the nearest rate above,
    /// and says so.
    #[test]
    fn a_missing_rate_takes_the_nearest_above_and_says_so() {
        let ranges = vec![range(2, 48_000, 192_000, SampleFormat::I32)];
        let f = plan(&ranges, CD).unwrap();
        assert_eq!(f.rate, 48_000);
        assert!(!f.bit_perfect());
        assert_eq!(f.label(), "48 kHz · 32-bit (resampled from 44.1 kHz)");
    }

    /// Mono on a stereo-only card: duplicated, not resampled.
    #[test]
    fn mono_falls_back_to_stereo() {
        let want = Want {
            rate: 44_100,
            channels: 1,
        };
        let ranges = vec![
            range(8, 44_100, 48_000, SampleFormat::I32),
            range(2, 44_100, 48_000, SampleFormat::I32),
        ];
        let f = plan(&ranges, want).unwrap();
        assert_eq!(f.channels, 2);
        assert_eq!(f.label(), "44.1 kHz · 32-bit (1 ch → 2 ch)");
    }

    #[test]
    fn nothing_usable_plans_nothing() {
        assert!(plan(&[], CD).is_none());
        assert!(plan(&[range(2, 44_100, 44_100, SampleFormat::I8)], CD).is_none());
    }
}
