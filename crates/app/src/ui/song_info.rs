//! Song details dialog: every field the server (or the file) has on a track.
//!
//! Any view can ask for it with [`show`], including context-menu closures that
//! only hold `&mut App`: the request goes through a gpui global that
//! `RootView` observes, so no view needs an event of its own wired through
//! the root for it.

use std::time::Duration;

use gpui::{
    App, ClipboardItem, Context, Global, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    ActiveTheme, IconName, Sizable, StyledExt,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use subsonic::Song;

use crate::ui::{format_added_date, format_bytes, format_count, format_duration, format_khz};

/// The pending "show details for this song" request. Taken by `RootView`.
#[derive(Default)]
pub struct SongInfoRequest(pub Option<Song>);

impl Global for SongInfoRequest {}

/// Open the details dialog for `song`.
pub fn show(song: Song, cx: &mut App) {
    cx.set_global(SongInfoRequest(Some(song)));
}

/// What the dialog is showing. Kept after close so the exit can play.
pub struct SongInfo {
    pub song: Song,
    /// Every tag in a local file, read on open; `None` until it lands (and
    /// always for server tracks).
    pub file_tags: Option<Vec<(String, String)>>,
    /// A fresher copy is being fetched (`getSong` or the file's tags).
    pub loading: bool,
    /// Bumped per open, so a late answer for an earlier song is dropped.
    pub generation: u64,
}

/// One titled group of `(label, value)` rows.
#[derive(Debug, PartialEq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

/// Everything worth showing about a song, grouped; empty fields and empty
/// groups are left out.
pub fn song_sections(song: &Song) -> Vec<Section> {
    let d = &song.details;
    let mut out = Vec::new();
    let mut rows = Rows::default();

    // Track
    rows.push("Title", Some(song.title.clone()));
    let artists = if !song.artists.is_empty() {
        Some(join(song.artists.iter().map(|a| a.name.as_str())))
    } else {
        d.display_artist.clone().or_else(|| song.artist.clone())
    };
    rows.push("Artist", artists);
    rows.push("Album", song.album.clone());
    let album_artists = if !d.album_artists.is_empty() {
        Some(join(d.album_artists.iter().map(|a| a.name.as_str())))
    } else {
        d.display_album_artist.clone()
    };
    rows.push("Album artist", album_artists);
    rows.push("Track", song.track.map(|n| n.to_string()));
    rows.push("Disc", song.disc_number.map(|n| n.to_string()));
    rows.push("Year", song.year.map(|y| y.to_string()));
    let genres = if !d.genres.is_empty() {
        Some(join(d.genres.iter().map(|g| g.name.as_str())))
    } else {
        song.genre.clone()
    };
    rows.push("Genre", genres);
    let composers = d.composers();
    rows.push(
        "Composer",
        (!composers.is_empty()).then(|| join(composers.iter().map(|(_, n)| n.as_str()))),
    );
    rows.push("BPM", d.bpm.filter(|b| *b > 0).map(|b| b.to_string()));
    rows.push(
        "Mood",
        (!d.moods.is_empty()).then(|| join(d.moods.iter().map(String::as_str))),
    );
    rows.push("Explicit", explicit_label(d.explicit_status.as_deref()));
    rows.push("Comment", d.comment.clone());
    out.push(rows.section("Track"));

    // Every other credited role, one row per role in the order the server
    // lists them. Composers are already above.
    let mut roles: Vec<(String, Vec<&str>)> = Vec::new();
    for c in d
        .contributors
        .iter()
        .filter(|c| !c.role.eq_ignore_ascii_case("composer"))
    {
        let role = match c.sub_role.as_deref().filter(|s| !s.is_empty()) {
            Some(sub) => format!("{} ({sub})", capitalise(&c.role)),
            None => capitalise(&c.role),
        };
        match roles.iter_mut().find(|(r, _)| *r == role) {
            Some((_, names)) => names.push(&c.artist.name),
            None => roles.push((role, vec![&c.artist.name])),
        }
    }
    for (role, names) in roles {
        rows.rows.push((role, join(names.into_iter())));
    }
    out.push(rows.section("Credits"));

    // Audio
    let format = match (song.suffix.as_deref(), song.content_type.as_deref()) {
        (Some(s), Some(t)) => Some(format!("{} ({t})", s.to_uppercase())),
        (Some(s), None) => Some(s.to_uppercase()),
        (None, t) => t.map(str::to_string),
    };
    rows.push("Format", format);
    rows.push(
        "Duration",
        song.duration
            .map(|s| format_duration(Duration::from_secs(s as u64))),
    );
    rows.push(
        "Bitrate",
        song.bit_rate
            .filter(|b| *b > 0)
            .map(|b| format!("{b} kbps")),
    );
    rows.push(
        "Sample rate",
        song.sampling_rate.filter(|r| *r > 0).map(format_khz),
    );
    rows.push(
        "Bit depth",
        song.bit_depth
            .filter(|b| *b > 0)
            .map(|b| format!("{b}-bit")),
    );
    rows.push("Channels", song.channel_count.map(channels_label));
    rows.push("Size", song.size.filter(|s| *s > 0).map(format_bytes));
    rows.push("Media type", d.media_type.clone().filter(|m| m != "song"));
    out.push(rows.section("Audio"));

    // Loudness
    if let Some(rg) = &song.replay_gain {
        rows.push("Track gain", rg.track_gain.map(|g| format!("{g:+.2} dB")));
        rows.push("Track peak", rg.track_peak.map(|p| format!("{p:.6}")));
        rows.push("Album gain", rg.album_gain.map(|g| format!("{g:+.2} dB")));
        rows.push("Album peak", rg.album_peak.map(|p| format!("{p:.6}")));
        rows.push(
            "Base gain",
            rg.base_gain
                .filter(|g| *g != 0.)
                .map(|g| format!("{g:+.2} dB")),
        );
        rows.push(
            "Fallback gain",
            rg.fallback_gain.map(|g| format!("{g:+.2} dB")),
        );
    }
    out.push(rows.section("ReplayGain"));

    // Library
    rows.push("Path", song.local_path.clone().or_else(|| d.path.clone()));
    rows.push(
        "Added",
        d.created.as_deref().map(|c| format_added_date(c, true)),
    );
    rows.push(
        "Last played",
        d.played.as_deref().map(|p| format_added_date(p, true)),
    );
    rows.push(
        "Plays",
        song.play_count
            .map(|n| format_count(i64::try_from(n).unwrap_or(i64::MAX))),
    );
    rows.push(
        "Rating",
        song.user_rating
            .filter(|r| *r > 0)
            .map(|r| format!("{r} / 5")),
    );
    rows.push(
        "Starred",
        song.starred.as_deref().map(|s| format_added_date(s, true)),
    );
    rows.push(
        "ISRC",
        (!d.isrc.is_empty()).then(|| join(d.isrc.iter().map(String::as_str))),
    );
    rows.push("MusicBrainz ID", d.music_brainz_id.clone());
    rows.push(
        "Sort title",
        d.sort_name.clone().filter(|s| *s != song.title),
    );
    rows.push("ID", Some(song.id.clone()));
    out.push(rows.section("Library"));

    out.retain(|s| !s.rows.is_empty());
    out
}

/// The dialog as plain text, for the Copy button.
pub fn as_text(sections: &[Section], file_tags: Option<&[(String, String)]>) -> String {
    let mut out = String::new();
    let tags = file_tags.map(|t| Section {
        title: "File tags",
        rows: t.to_vec(),
    });
    for section in sections.iter().chain(tags.as_ref()) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(section.title);
        out.push('\n');
        for (label, value) in &section.rows {
            out.push_str(&format!("  {label}: {value}\n"));
        }
    }
    out
}

/// Every text tag in a local file, as `(key, value)`, in file order. Blocking.
///
/// Keys lofty maps are shown by their generic name (`TrackTitle`), the rest
/// by the raw frame or field name the file used. Pictures and binary frames
/// are summarised rather than dumped.
pub fn read_file_tags(path: &str) -> anyhow::Result<Vec<(String, String)>> {
    use lofty::{ItemKey, ItemValue, TaggedFileExt};
    let tagged = lofty::read_from_path(path)?;
    let mut out = Vec::new();
    for tag in tagged.tags() {
        for item in tag.items() {
            let key = match item.key() {
                ItemKey::Unknown(raw) => raw.clone(),
                other => format!("{other:?}"),
            };
            let value = match item.value() {
                ItemValue::Text(t) | ItemValue::Locator(t) => t.trim().to_string(),
                ItemValue::Binary(b) => format!("<{} bytes>", b.len()),
            };
            if !value.is_empty() {
                out.push((key, value));
            }
        }
        for picture in tag.pictures() {
            let kind = format!("{:?}", picture.pic_type());
            out.push((
                "Picture".to_string(),
                format!(
                    "{kind}, {}, {}",
                    picture
                        .mime_type()
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| "unknown type".into()),
                    format_bytes(picture.data().len() as u64)
                ),
            ));
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Rows {
    rows: Vec<(String, String)>,
}

impl Rows {
    fn push(&mut self, label: &str, value: Option<String>) {
        if let Some(value) = value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            self.rows.push((label.to_string(), value));
        }
    }

    fn section(&mut self, title: &'static str) -> Section {
        Section {
            title,
            rows: std::mem::take(&mut self.rows),
        }
    }
}

fn join<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn channels_label(n: u32) -> String {
    match n {
        1 => "Mono".into(),
        2 => "Stereo".into(),
        n => format!("{n} channels"),
    }
}

/// OpenSubsonic `explicitStatus`: `explicit`, `clean`, or empty for unknown.
fn explicit_label(status: Option<&str>) -> Option<String> {
    match status? {
        "explicit" => Some("Yes".into()),
        "clean" => Some("Clean".into()),
        _ => None,
    }
}

/// The dialog: dimmed backdrop, centred card, one scrolling column of
/// sections. `t` is the reveal's openness; `active` false while it leaves,
/// so a closing dialog takes no clicks.
pub fn render<V: 'static>(
    info: &SongInfo,
    t: f32,
    active: bool,
    on_close: impl Fn(&mut V, &mut Context<V>) + 'static + Clone,
    window: &Window,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let sections = song_sections(&info.song);
    let copy_text = as_text(&sections, info.file_tags.as_deref());
    let max_h = f32::from(window.viewport_size().height) * 0.8;
    let theme = cx.theme().clone();

    let row = |label: String, value: String| {
        h_flex()
            .items_start()
            .gap_3()
            .py_0p5()
            .child(
                div()
                    .flex_none()
                    .w(px(120.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().text_sm().child(value))
    };
    let section = |title: String, rows: Vec<(String, String)>| {
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child(title.to_uppercase()),
            )
            .children(rows.into_iter().map(|(l, v)| row(l, v)))
    };

    let mut body = v_flex().gap_4();
    for s in sections {
        body = body.child(section(s.title.to_string(), s.rows));
    }
    if let Some(tags) = &info.file_tags
        && !tags.is_empty()
    {
        body = body.child(section("File tags".into(), tags.clone()));
    }

    let close_backdrop = on_close.clone();
    let close_button = on_close.clone();
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .pt(px(28. * (1. - t)))
        .occlude()
        .bg(gpui::hsla(0., 0., 0., 0.6 * t))
        .when(active, |this| {
            this.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| close_backdrop(this, cx)),
            )
        })
        .child(
            v_flex()
                .w(px(560.))
                .max_w_full()
                .max_h(px(max_h))
                .opacity(t)
                .rounded_xl()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .occlude()
                // Clicks inside the card must not reach the backdrop.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_5()
                        .pt_5()
                        .pb_3()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_semibold()
                                        .truncate()
                                        .child(info.song.title.clone()),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    if info.loading {
                                        "Song details · updating…"
                                    } else {
                                        "Song details"
                                    },
                                )),
                        )
                        .child(
                            Button::new("song-info-copy")
                                .ghost()
                                .small()
                                .label("Copy")
                                .when(active, |b| {
                                    b.on_click(move |_, _, cx: &mut App| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ))
                                    })
                                }),
                        )
                        .child(
                            Button::new("song-info-close")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .when(active, |b| {
                                    b.on_click(
                                        cx.listener(move |this, _, _, cx| close_button(this, cx)),
                                    )
                                }),
                        ),
                )
                .child(
                    div()
                        .id("song-info-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_5()
                        .pb_5()
                        .child(body),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(json: serde_json::Value) -> Song {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn sections_skip_empty_fields_and_groups() {
        let s = song(serde_json::json!({ "id": "s-1", "title": "Hurt" }));
        let sections = song_sections(&s);
        let titles: Vec<_> = sections.iter().map(|s| s.title).collect();
        assert_eq!(titles, ["Track", "Library"]);
        assert_eq!(sections[0].rows, [("Title".into(), "Hurt".into())]);
    }

    #[test]
    fn credits_group_by_role_and_leave_composers_to_the_track_group() {
        let s = song(serde_json::json!({
            "id": "s-1", "title": "Hurt",
            "contributors": [
                {"role": "composer", "artist": {"id": "a", "name": "Trent Reznor"}},
                {"role": "producer", "artist": {"id": "b", "name": "Rick Rubin"}},
                {"role": "performer", "subRole": "guitar", "artist": {"id": "c", "name": "Mike Campbell"}},
                {"role": "producer", "artist": {"id": "d", "name": "John Carter Cash"}}
            ]
        }));
        let sections = song_sections(&s);
        let track = &sections[0];
        assert!(
            track
                .rows
                .contains(&("Composer".into(), "Trent Reznor".into()))
        );
        let credits = sections.iter().find(|s| s.title == "Credits").unwrap();
        assert_eq!(
            credits.rows,
            [
                ("Producer".into(), "Rick Rubin, John Carter Cash".into()),
                ("Performer (guitar)".into(), "Mike Campbell".into()),
            ]
        );
    }

    #[test]
    fn text_copy_lists_every_section() {
        let s = song(serde_json::json!({ "id": "s-1", "title": "Hurt", "suffix": "flac" }));
        let tags = vec![("TrackTitle".to_string(), "Hurt".to_string())];
        let text = as_text(&song_sections(&s), Some(&tags));
        assert!(text.contains("Track\n  Title: Hurt\n"));
        assert!(text.contains("Audio\n  Format: FLAC\n"));
        assert!(text.ends_with("File tags\n  TrackTitle: Hurt\n"));
    }
}
