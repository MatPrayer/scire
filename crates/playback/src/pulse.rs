//! PulseAudio/PipeWire control through `pactl` (Linux only).
//!
//! cpal's ALSA backend is the wrong lens for a modern Linux desktop: it
//! enumerates ALSA *hints*, which is a list of plugins ("Rate Converter Plugin
//! Using Speex Resampler"), sound servers and raw cards, includes capture-only
//! devices, and — the reason this module exists — cannot see PipeWire sinks at
//! all, so a Bluetooth headset never appears in it. `pactl` sees exactly the
//! outputs the rest of the desktop offers, Bluetooth included.
//!
//! Everything here is best-effort and shells out: `None`/`false` means "could
//! not ask", never "the answer is no", so callers fall back to cpal rather than
//! act on a missing `pactl`.

use std::process::Command;

/// An output sink: `name` is the stable id `pactl` takes as an argument,
/// `description` the human string shown in the picker (and the one persisted in
/// settings, since ids change across reboots for USB/Bluetooth devices), `id`
/// the per-boot number a stream reports as the sink it is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sink {
    pub id: u32,
    pub name: String,
    pub description: String,
}

/// One of this process's playback streams and the sink it is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stream {
    id: u32,
    sink: u32,
}

/// The sink id a stream reports while it is between sinks — read back straight
/// after a move, this is "ask again", not "somewhere else".
const UNATTACHED: u32 = u32::MAX;

/// Outcome of asking the sound server to move our audio somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveResult {
    /// Our stream is on the requested sink.
    Landed,
    /// The move was accepted and then undone: something else on this desktop
    /// routes streams (EasyEffects and similar sit in front of every output),
    /// and it put ours back on the named sink. `pactl` reports success in this
    /// case too, so the only way to know is to look again.
    Grabbed(String),
    /// No stream of ours is registered yet — worth another look in a moment.
    NoStream,
}

/// Every output sink the sound server currently offers.
pub(crate) fn sinks() -> Option<Vec<Sink>> {
    let text = pactl(&["list", "sinks"])?;
    let sinks = parse_sinks(&text);
    (!sinks.is_empty()).then_some(sinks)
}

/// Descriptions of every output sink, for the picker and presence checks.
pub(crate) fn descriptions() -> Option<Vec<String>> {
    Some(sinks()?.into_iter().map(|s| s.description).collect())
}

/// Description of the sink new streams land on by default.
pub(crate) fn default_sink_description() -> Option<String> {
    let default = pactl(&["get-default-sink"])?.trim().to_string();
    if default.is_empty() {
        return None;
    }
    sinks()?
        .into_iter()
        .find(|s| s.name == default)
        .map(|s| s.description)
}

/// Point this process's playback stream at the sink with this description.
///
/// The stream is opened on the default sink by cpal and moved here afterwards —
/// there is no way to ask cpal for a PipeWire sink directly. The move is always
/// read back: opening a device briefly registers a node that is not the one
/// that ends up playing, so a move reported as done can have landed on nothing.
pub(crate) fn move_output_to(description: &str) -> MoveResult {
    let Some(sink) = sinks().and_then(|s| s.into_iter().find(|s| s.description == description))
    else {
        return MoveResult::NoStream;
    };
    let Some(ours) = our_streams() else {
        return MoveResult::NoStream;
    };
    for stream in &ours {
        if pactl(&["move-sink-input", &stream.id.to_string(), &sink.name]).is_none() {
            tracing::warn!("could not move stream {} to {}", stream.id, sink.name);
        }
    }
    match our_streams() {
        None => MoveResult::NoStream,
        Some(after) if after.iter().all(|s| s.sink == sink.id) => MoveResult::Landed,
        // Still moving: nothing has been decided yet.
        Some(after) if after.iter().any(|s| s.sink == UNATTACHED) => MoveResult::NoStream,
        Some(after) => {
            let elsewhere = after.iter().find(|s| s.sink != sink.id).map(|s| s.sink);
            let name = elsewhere
                .and_then(|id| sinks()?.into_iter().find(|s| s.id == id))
                .map(|s| s.description)
                .unwrap_or_else(|| "another sink".to_string());
            MoveResult::Grabbed(name)
        }
    }
}

/// This process's playback streams, or None when it has none registered.
fn our_streams() -> Option<Vec<Stream>> {
    let text = pactl(&["list", "sink-inputs"])?;
    let streams = parse_streams(&text, &stream_tags());
    (!streams.is_empty()).then_some(streams)
}

/// Property values that identify a stream as ours. pipewire-alsa names the node
/// after the binary (`alsa_playback.scire`) and does not publish a process id,
/// so the executable name is what there is to match on; a native PulseAudio
/// client would instead carry `application.process.id`.
fn stream_tags() -> Vec<String> {
    let mut tags = vec![format!(
        "application.process.id = \"{}\"",
        std::process::id()
    )];
    if let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        tags.push(format!("node.name = \"alsa_playback.{exe}\""));
        tags.push(format!("application.name = \"PipeWire ALSA [{exe}]\""));
    }
    tags
}

/// Streams in `pactl list sink-inputs` output whose property block carries one
/// of `tags`, with the sink each is attached to.
fn parse_streams(text: &str, tags: &[String]) -> Vec<Stream> {
    let mut streams = Vec::new();
    let mut id: Option<u32> = None;
    let mut sink: Option<u32> = None;
    let mut ours = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(next) = trimmed.strip_prefix("Sink Input #") {
            if let (true, Some(id), Some(sink)) = (ours, id, sink) {
                streams.push(Stream { id, sink });
            }
            id = next.trim().parse().ok();
            sink = None;
            ours = false;
        } else if let Some(s) = trimmed.strip_prefix("Sink:") {
            sink = s.trim().parse().ok();
        } else if tags.iter().any(|tag| trimmed == tag) {
            ours = true;
        }
    }
    if let (true, Some(id), Some(sink)) = (ours, id, sink) {
        streams.push(Stream { id, sink });
    }
    streams
}

/// The `Name:`/`Description:` pair of every sink in `pactl list sinks` output.
/// Only the top-level fields count — the `Properties:` block repeats them in
/// `key = "value"` form, and ports have their own descriptions.
fn parse_sinks(text: &str) -> Vec<Sink> {
    let mut sinks = Vec::new();
    let mut id: Option<u32> = None;
    let mut name: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(next) = trimmed.strip_prefix("Sink #") {
            id = next.trim().parse().ok();
            name = None;
        } else if let Some(n) = trimmed.strip_prefix("Name:") {
            name = Some(n.trim().to_string());
        } else if let Some(d) = trimmed.strip_prefix("Description:")
            && let (Some(id), Some(name)) = (id, name.take())
        {
            sinks.push(Sink {
                id,
                name,
                description: d.trim().to_string(),
            });
        }
    }
    sinks
}

/// Run `pactl` and return its stdout, or None if it is missing or failed.
fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C") // keep field labels unlocalized
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINKS: &str = "Sink #33\n\tState: RUNNING\n\tName: easyeffects_sink\n\
        \tDescription: Easy Effects Sink\n\tProperties:\n\
        \t\tdevice.description = \"ignored\"\n\
        \t\tNode Latency: 1024\n\
        Sink #40\n\tState: SUSPENDED\n\tName: bluez_output.AC_17.1\n\
        \tDescription: Mattia's AirPods\n\tActive Port: headset-output\n\
        \tPorts:\n\t\theadset-output: Headset (priority: 0)\n";

    const STREAMS: &str = "Sink Input #2100\n\tSink: 33\n\tProperties:\n\
        \t\tapplication.name = \"Firefox\"\n\
        \t\tnode.name = \"alsa_playback.firefox\"\n\
        Sink Input #2195\n\tSink: 40\n\tCorked: no\n\tProperties:\n\
        \t\tapplication.name = \"PipeWire ALSA [scire]\"\n\
        \t\tnode.name = \"alsa_playback.scire\"\n";

    fn tags() -> Vec<String> {
        vec!["node.name = \"alsa_playback.scire\"".to_string()]
    }

    #[test]
    fn sinks_pair_each_name_with_its_own_description() {
        assert_eq!(
            parse_sinks(SINKS),
            vec![
                Sink {
                    id: 33,
                    name: "easyeffects_sink".into(),
                    description: "Easy Effects Sink".into()
                },
                Sink {
                    id: 40,
                    name: "bluez_output.AC_17.1".into(),
                    description: "Mattia's AirPods".into()
                },
            ]
        );
    }

    #[test]
    fn a_sink_without_a_description_does_not_borrow_the_next_ones() {
        // A block missing `Description:` must not pair its name with the
        // following sink's description.
        let text = "Sink #1\n\tName: only_a_name\nSink #2\n\tName: real\n\tDescription: Real\n";
        assert_eq!(
            parse_sinks(text),
            vec![Sink {
                id: 2,
                name: "real".into(),
                description: "Real".into()
            }]
        );
    }

    #[test]
    fn our_stream_is_found_by_node_name_with_the_sink_it_sits_on() {
        assert_eq!(
            parse_streams(STREAMS, &tags()),
            vec![Stream { id: 2195, sink: 40 }]
        );
    }

    #[test]
    fn a_stream_matching_nothing_is_not_ours() {
        let text = "Sink Input #2100\n\tSink: 33\n\tProperties:\n\
            \t\tnode.name = \"alsa_playback.firefox\"\n";
        assert!(parse_streams(text, &tags()).is_empty());
    }

    #[test]
    fn a_stream_between_sinks_reports_the_unattached_id() {
        let text = "Sink Input #2195\n\tSink: 4294967295\n\tProperties:\n\
            \t\tnode.name = \"alsa_playback.scire\"\n";
        assert_eq!(
            parse_streams(text, &tags()),
            vec![Stream {
                id: 2195,
                sink: UNATTACHED
            }]
        );
    }

    #[test]
    fn a_stream_keeps_its_own_sink_when_another_block_follows() {
        // The `Sink:` line precedes the properties that identify the stream, so
        // a parser that flushed on the tag rather than at the block boundary
        // would be reading the previous block's sink.
        let text = format!(
            "{STREAMS}Sink Input #2400\n\tSink: 33\n\tProperties:\n\
            \t\tnode.name = \"alsa_playback.mpv\"\n"
        );
        assert_eq!(
            parse_streams(&text, &tags()),
            vec![Stream { id: 2195, sink: 40 }]
        );
    }
}
