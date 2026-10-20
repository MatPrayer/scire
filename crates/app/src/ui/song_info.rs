//! Song details dialog: every field the server (or the file) has on a track.
//!
//! Any view can ask for it with [`show`], including context-menu closures that
//! only hold `&mut App`: the request goes through a gpui global that
//! `RootView` observes, so no view needs an event of its own wired through
//! the root for it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

use crate::assets::{app_icon, icons};
use crate::errors::ErrorNote;
use crate::services::genius::GeniusSong;
use crate::ui::song_credits::{CreditGroup, Person, Sources, credit_groups};
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
    pub tab: SongInfoTab,
    pub genius: GeniusState,
    /// Genius refused the user's token, with its reason.
    pub token_rejected: Option<String>,
    /// Genius's bot check refused a request that carried the token.
    pub genius_blocked: bool,
    /// Artist pages for names the song carries no id for, by folded name
    /// (`subsonic::credits::fold`), from the cache.
    pub credit_ids: HashMap<String, String>,
    /// Library albums holding a related song (samples, covers …), by
    /// [`related_key`].
    pub related_albums: HashMap<String, String>,
    pub about_expanded: bool,
    /// The song's cover, from the art cache or the scanner's art.
    pub cover: Option<PathBuf>,
}

impl SongInfo {
    pub fn new(song: Song, loading: bool, generation: u64) -> Self {
        Self {
            song,
            file_tags: None,
            loading,
            generation,
            tab: SongInfoTab::Credits,
            genius: GeniusState::Off,
            token_rejected: None,
            genius_blocked: false,
            credit_ids: HashMap::new(),
            related_albums: HashMap::new(),
            about_expanded: false,
            cover: None,
        }
    }

    /// The Credits tab's model, as the dialog would draw it now.
    pub fn groups(&self) -> Vec<CreditGroup> {
        let genius = match &self.genius {
            GeniusState::Found(song) => Some(song.as_ref()),
            _ => None,
        };
        credit_groups(&self.song, genius, &|name| {
            self.credit_ids.get(&subsonic::credits::fold(name)).cloned()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongInfoTab {
    Credits,
    Details,
}

/// Where the Genius lookup stands.
#[derive(Debug, Clone)]
pub enum GeniusState {
    /// Switched off in Settings.
    Off,
    Loading(Instant),
    Missing,
    /// Switched on, but no API token is set.
    NeedsToken,
    Failed(ErrorNote),
    Found(Box<GeniusSong>),
}

/// What the dialog asks its host to do.
#[derive(Debug, Clone)]
pub enum SongInfoAction {
    Close,
    Tab(SongInfoTab),
    /// An artist page, by the id `RootView::open_any_artist` takes.
    OpenArtist(String),
    /// An album page, by the id `RootView::open_any_album` takes.
    OpenAlbum(String),
    /// The search page on a genre's albums.
    OpenGenre(String),
    ToggleAbout,
    RetryGenius,
}

/// Key of a related song in [`SongInfo::related_albums`].
pub fn related_key(title: &str, artist: &str) -> String {
    format!(
        "{}\u{0}{}",
        subsonic::credits::fold(title),
        subsonic::credits::fold(artist)
    )
}

/// One titled group of `(label, value)` rows.
#[derive(Debug, PartialEq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

/// The song's genres: OpenSubsonic's list, else the single `genre`.
fn song_genres(song: &Song) -> Vec<String> {
    song.genre_names().into_iter().map(String::from).collect()
}

/// Everything worth showing about a song, grouped; empty fields and empty
/// groups are left out.
pub fn song_sections(song: &Song) -> Vec<Section> {
    let d = &song.details;
    let mut out = Vec::new();
    let mut rows = Rows::default();

    // Track
    rows.push("Title", Some(song.title.clone()));
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
    let genres = song_genres(song);
    rows.push(
        "Genre",
        (!genres.is_empty()).then(|| join(genres.iter().map(String::as_str))),
    );
    rows.push("BPM", d.bpm.filter(|b| *b > 0).map(|b| b.to_string()));
    rows.push(
        "Mood",
        (!d.moods.is_empty()).then(|| join(d.moods.iter().map(String::as_str))),
    );
    rows.push("Explicit", explicit_label(d.explicit_status.as_deref()));
    rows.push("Comment", d.comment.clone());
    out.push(rows.section("Track"));

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

/// The Credits tab as plain text, for the Copy button.
pub fn credits_text(groups: &[CreditGroup], genius: Option<&GeniusSong>) -> String {
    let mut out = String::from("Credits\n");
    for g in groups {
        let names: Vec<&str> = g.people.iter().map(|p| p.name.as_str()).collect();
        out.push_str(&format!("  {}: {}\n", g.role, names.join(", ")));
    }
    if let Some(song) = genius {
        for f in facts(song) {
            let hot = if f.hot { " (hot)" } else { "" };
            out.push_str(&format!("  {}: {}{hot}\n", f.label, f.value));
        }
        if !song.tags.is_empty() {
            out.push_str(&format!("  Tags: {}\n", song.tags.join(", ")));
        }
        for (label, url) in listen_links(song)
            .into_iter()
            .filter(|(l, _)| l != "Genius")
        {
            out.push_str(&format!("  {label}: {url}\n"));
        }
        for rel in &song.relations {
            let songs: Vec<String> = rel
                .songs
                .iter()
                .map(|s| related_label(&s.title, &s.artist))
                .collect();
            out.push_str(&format!("  {}: {}\n", rel.label, songs.join("; ")));
        }
        if let Some(about) = &song.description {
            out.push_str(&format!("\nAbout\n  {about}\n"));
        }
        out.push_str(&format!("  Genius: {}\n", song.url));
    }
    out
}

fn related_label(title: &str, artist: &str) -> String {
    if artist.is_empty() {
        title.to_string()
    } else {
        format!("{title} — {artist}")
    }
}

/// How much of a Genius description shows before *More*.
const ABOUT_PREVIEW_CHARS: usize = 480;

/// The preview of a long description, cut at a word, or `None` when the
/// whole text is short enough to show.
fn about_preview(text: &str) -> Option<String> {
    if text.chars().count() <= ABOUT_PREVIEW_CHARS {
        return None;
    }
    let cut: String = text.chars().take(ABOUT_PREVIEW_CHARS).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(i) => &cut[..i],
        None => &cut,
    };
    Some(format!("{}…", cut.trim_end()))
}

/// A click handler that hands `action` to the dialog's host.
fn act<V: 'static>(
    on_action: &(impl Fn(&mut V, SongInfoAction, &mut Context<V>) + Clone + 'static),
    action: SongInfoAction,
    cx: &mut Context<V>,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let on_action = on_action.clone();
    cx.listener(move |this: &mut V, _: &gpui::ClickEvent, _, cx| {
        on_action(this, action.clone(), cx)
    })
}

/// Which sources named a person, for the chip's tooltip.
fn sources_tip(p: &Person) -> String {
    let mut from = Vec::new();
    if p.sources.server {
        from.push("the server");
    }
    if p.sources.tags {
        from.push("the file's tags");
    }
    if p.sources.genius {
        from.push("Genius");
    }
    let mut tip = format!("From {}", from.join(", "));
    if p.id.is_some() {
        tip.push_str(" · open artist page");
    } else if p.url.is_some() {
        tip.push_str(" · open on Genius");
    }
    tip
}

/// "1.2M", "34K", "999": a view count at a glance.
pub fn compact_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    const UNITS: [(f64, &str); 3] = [(1e3, "K"), (1e6, "M"), (1e9, "B")];
    let mut i = UNITS
        .iter()
        .rposition(|(scale, _)| n as f64 >= *scale)
        .unwrap_or(0);
    loop {
        let (scale, unit) = UNITS[i];
        let value = n as f64 / scale;
        // One decimal below ten ("1.2M"), none above ("34K").
        let text = if value < 9.95 {
            format!("{value:.1}")
        } else {
            format!("{value:.0}")
        };
        // 999,999 rounds to "1000K": that is the next unit's "1M".
        if text == "1000" && i + 1 < UNITS.len() {
            i += 1;
            continue;
        }
        return format!("{}{unit}", text.trim_end_matches(".0"));
    }
}

/// The icon a fact tile carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactIcon {
    Released,
    Recorded,
    Language,
    Views,
}

/// One tile of the Credits tab's overview: what Genius knows about the song
/// itself rather than who is on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub icon: FactIcon,
    pub label: &'static str,
    pub value: String,
    /// Shown as a flame beside the value: trending on Genius.
    pub hot: bool,
}

/// Past this many characters a fact takes a row of its own.
const FACT_SHORT_CHARS: usize = 20;

impl Fact {
    /// Too long to share a row with the others.
    pub fn wide(&self) -> bool {
        self.value.chars().count() > FACT_SHORT_CHARS
    }
}

/// The overview tiles, in drawing order; empty fields are left out.
pub fn facts(song: &GeniusSong) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut push = |icon, label, value: Option<String>, hot| {
        if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
            out.push(Fact {
                icon,
                label,
                value,
                hot,
            });
        }
    };
    push(
        FactIcon::Released,
        "Released",
        song.release_date.clone(),
        false,
    );
    push(
        FactIcon::Recorded,
        "Recorded at",
        song.recording_location.clone(),
        false,
    );
    push(FactIcon::Language, "Language", song.language.clone(), false);
    push(
        FactIcon::Views,
        "Genius views",
        song.pageviews.map(compact_count),
        song.hot,
    );
    out
}

/// Where to hear or read more, as `(label, url)`: streaming services, then
/// Genius itself. A YouTube link on a song with a video is the video.
pub fn listen_links(song: &GeniusSong) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = song
        .media
        .iter()
        .map(|m| {
            let label = if m.service == "YouTube" && song.has_video {
                "Music video".to_string()
            } else {
                m.service.clone()
            };
            (label, m.url.clone())
        })
        .collect();
    if !song.url.is_empty() {
        out.push(("Genius".into(), song.url.clone()));
    }
    out
}

fn listen_icon(label: &str) -> gpui_component::Icon {
    match label {
        "Spotify" => app_icon(icons::BRAND_SPOTIFY),
        "Apple Music" => app_icon(icons::BRAND_APPLE_MUSIC),
        "YouTube" | "Music video" => app_icon(icons::BRAND_YOUTUBE),
        "SoundCloud" => app_icon(icons::BRAND_SOUNDCLOUD),
        "Genius" => app_icon(icons::BRAND_GENIUS),
        _ => gpui_component::Icon::new(IconName::ExternalLink),
    }
}

fn fact_icon(icon: FactIcon) -> gpui_component::Icon {
    match icon {
        FactIcon::Released => gpui_component::Icon::new(IconName::Calendar),
        FactIcon::Recorded => app_icon(icons::MIC_VOCAL),
        FactIcon::Language => app_icon(icons::LANGUAGES),
        FactIcon::Views => gpui_component::Icon::new(IconName::Eye),
    }
}

/// The header's second line: artist · album · year.
fn header_line(song: &Song) -> String {
    let artist = song
        .details
        .display_artist
        .clone()
        .or_else(|| song.artist.clone())
        .filter(|a| !a.trim().is_empty());
    let parts: Vec<String> = [
        artist,
        song.album.clone().filter(|a| !a.trim().is_empty()),
        song.year.filter(|y| *y > 0).map(|y| y.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" · ")
}

/// Rows in one bordered box, a hairline between each.
fn boxed(rows: Vec<gpui::AnyElement>, theme: &gpui_component::Theme) -> gpui::Div {
    let mut list = v_flex()
        .w_full()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden();
    for (i, row) in rows.into_iter().enumerate() {
        list = list.child(
            div()
                .w_full()
                .when(i > 0, |d| d.border_t_1().border_color(theme.border))
                .child(row),
        );
    }
    list
}

/// A section: small caps heading (with an optional trailing element) over its
/// content.
fn titled(
    title: &str,
    trailing: Option<gpui::AnyElement>,
    content: impl IntoElement,
    theme: &gpui_component::Theme,
) -> gpui::Div {
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(title.to_uppercase()),
                )
                .children(trailing),
        )
        .child(content)
}

const LABEL_W: f32 = 132.;

/// One `label · value` row of a boxed list.
fn kv_row(label: String, value: impl IntoElement, theme: &gpui_component::Theme) -> gpui::Div {
    h_flex()
        .items_start()
        .gap_3()
        .px_3()
        .py_1p5()
        .child(
            div()
                .flex_none()
                .w(px(LABEL_W))
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().text_sm().child(value))
}

/// The dialog: dimmed backdrop, centred card, a Credits and a Details tab.
/// `t` is the reveal's openness; `active` false while it leaves, so a closing
/// dialog takes no clicks.
pub fn render<V: 'static>(
    info: &SongInfo,
    t: f32,
    active: bool,
    on_action: impl Fn(&mut V, SongInfoAction, &mut Context<V>) + 'static + Clone,
    window: &Window,
    cx: &mut Context<V>,
) -> gpui::AnyElement {
    let sections = song_sections(&info.song);
    let groups = info.groups();
    let genius_song = match &info.genius {
        GeniusState::Found(song) => Some(song.as_ref()),
        _ => None,
    };
    let copy_text = format!(
        "{}\n{}",
        credits_text(&groups, genius_song),
        as_text(&sections, info.file_tags.as_deref())
    );
    let max_h = f32::from(window.viewport_size().height) * 0.85;
    let theme = cx.theme().clone();

    let body = match info.tab {
        SongInfoTab::Details => {
            let mut body = v_flex().gap_5();
            let mut all = sections
                .into_iter()
                .map(|s| (s.title.to_string(), s.rows))
                .collect::<Vec<_>>();
            if let Some(tags) = &info.file_tags
                && !tags.is_empty()
            {
                all.push(("File tags".into(), tags.clone()));
            }
            let genres = song_genres(&info.song);
            for (title, rows) in all {
                let rows = rows
                    .into_iter()
                    .map(|(l, v)| {
                        // The genre row's names open the search page on
                        // that genre's albums; only the song's own row, not
                        // a raw GENRE tag under "File tags".
                        if title == "Track" && l == "Genre" && active {
                            let chips = h_flex().flex_wrap().gap_1p5().children(
                                genres.iter().enumerate().map(|(i, g)| {
                                    crate::ui::advanced_search::genre_tag(
                                        ("song-genre", i),
                                        g,
                                        crate::services::advanced_search::SearchKind::Albums,
                                        cx,
                                    )
                                    .on_click(act(
                                        &on_action,
                                        SongInfoAction::OpenGenre(g.clone()),
                                        cx,
                                    ))
                                }),
                            );
                            return kv_row(l, chips, &theme).into_any_element();
                        }
                        kv_row(l, v, &theme).into_any_element()
                    })
                    .collect();
                body = body.child(titled(&title, None, boxed(rows, &theme), &theme));
            }
            body
        }
        SongInfoTab::Credits => {
            let mut body = v_flex().gap_5();

            // What Genius knows about the song itself.
            if let Some(song) = genius_song {
                let tiles = facts(song);
                let mut overview = v_flex().gap_3();
                if !tiles.is_empty() {
                    // One line per tile, so a row of tiles is one height: short
                    // facts share a row, a long one ("Angelic Studios (Banbury,
                    // Oxfordshire, England)") gets a row of its own.
                    let tile = |i: usize, f: Fact| {
                        let tip = f.value.clone();
                        v_flex()
                            .id(("song-fact", i))
                            .min_w_0()
                            .gap_0p5()
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(theme.secondary)
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(fact_icon(f.icon).size_3())
                                    .child(f.label),
                            )
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .text_sm()
                                    .font_medium()
                                    .child(div().min_w_0().truncate().child(f.value))
                                    .when(f.hot, |this| {
                                        this.child(
                                            h_flex()
                                                .flex_none()
                                                .gap_0p5()
                                                .text_xs()
                                                .text_color(theme.danger)
                                                .child(app_icon(icons::FLAME).size_3())
                                                .child("Hot"),
                                        )
                                    }),
                            )
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                            })
                    };
                    let (wide, short): (Vec<_>, Vec<_>) =
                        tiles.into_iter().enumerate().partition(|(_, f)| f.wide());
                    let mut grid = v_flex().gap_2();
                    if !short.is_empty() {
                        grid = grid.child(
                            h_flex().items_start().gap_2().children(
                                short
                                    .into_iter()
                                    .map(|(i, f)| tile(i, f).flex_1().flex_basis(px(0.))),
                            ),
                        );
                    }
                    for (i, f) in wide {
                        grid = grid.child(tile(i, f).w_full());
                    }
                    overview = overview.child(grid);
                }
                if !song.tags.is_empty() {
                    overview = overview.child(h_flex().flex_wrap().gap_1().children(
                        song.tags.iter().map(|tag| {
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(tag.clone())
                        }),
                    ));
                }
                body = body.child(overview);
            }

            // People.
            let mut chip_ix = 0usize;
            let mut seen = Sources::default();
            let mut rows = Vec::new();
            for g in &groups {
                let mut chips = h_flex().flex_1().min_w_0().flex_wrap().gap_1p5();
                for p in &g.people {
                    chip_ix += 1;
                    seen.server |= p.sources.server;
                    seen.tags |= p.sources.tags;
                    seen.genius |= p.sources.genius;
                    let mut marks = h_flex().gap_0p5().text_color(theme.muted_foreground);
                    if p.sources.server {
                        marks = marks.child(app_icon(icons::SERVER).size_3());
                    }
                    if p.sources.tags {
                        marks = marks.child(app_icon(icons::TAG).size_3());
                    }
                    if p.sources.genius {
                        marks = marks.child(app_icon(icons::BRAND_GENIUS).size_3());
                    }
                    let tip = sources_tip(p);
                    let link = p.id.is_some() || p.url.is_some();
                    let mut chip = h_flex()
                        .id(("song-credit", chip_ix))
                        .gap_1p5()
                        .pl_2p5()
                        .pr_2()
                        .py_0p5()
                        .rounded_full()
                        .bg(theme.secondary)
                        .text_sm()
                        .child(p.name.clone())
                        .child(marks)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        });
                    if link {
                        chip = chip.cursor_pointer().hover(|s| s.bg(theme.secondary_hover));
                    }
                    if active {
                        if let Some(id) = &p.id {
                            chip = chip.on_click(act(
                                &on_action,
                                SongInfoAction::OpenArtist(id.clone()),
                                cx,
                            ));
                        } else if let Some(url) = p.url.clone() {
                            chip = chip.on_click(move |_, _, cx: &mut App| cx.open_url(&url));
                        }
                    }
                    chips = chips.child(chip);
                }
                rows.push(
                    h_flex()
                        .items_start()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .child(
                            div()
                                .flex_none()
                                .w(px(LABEL_W))
                                .pt_0p5()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(g.role.clone()),
                        )
                        .child(chips)
                        .into_any_element(),
                );
            }
            if !rows.is_empty() {
                // Which mark means what, for the marks actually drawn.
                let legend_item = |icon: gpui_component::Icon, label: &'static str| {
                    h_flex().gap_1().child(icon.size_3()).child(label)
                };
                let legend = h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(seen.server, |l| {
                        l.child(legend_item(app_icon(icons::SERVER), "Server"))
                    })
                    .when(seen.tags, |l| {
                        l.child(legend_item(app_icon(icons::TAG), "File tags"))
                    })
                    .when(seen.genius, |l| {
                        l.child(legend_item(app_icon(icons::BRAND_GENIUS), "Genius"))
                    })
                    .into_any_element();
                body = body.child(titled(
                    "People",
                    None,
                    v_flex()
                        .gap_1p5()
                        .child(boxed(rows, &theme))
                        .child(h_flex().justify_end().child(legend)),
                    &theme,
                ));
            }

            // Where Genius stands, under what the song itself says.
            let status = |text: &str| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        app_icon(icons::BRAND_GENIUS)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    )
                    .child(text.to_string())
            };
            match &info.genius {
                GeniusState::Off => {}
                GeniusState::Loading(since) => {
                    let show = crate::ui::placeholding(Some(*since));
                    body = body.child(
                        v_flex()
                            .gap_1()
                            .child(status("Asking Genius…"))
                            .child(crate::ui::skeleton_text(
                                "genius-sk-1",
                                show,
                                "Producer   Someone Name, Another",
                                cx,
                            ))
                            .child(crate::ui::skeleton_text(
                                "genius-sk-2",
                                show,
                                "Mixing Engineer   Somebody",
                                cx,
                            )),
                    );
                }
                GeniusState::Missing => body = body.child(status("Not found on Genius")),
                GeniusState::NeedsToken => body = body.child(status(
                    "Add a Genius API token in Settings → Connections for producers, writers and samples",
                )),
                GeniusState::Failed(note) => {
                    let retry = on_action.clone();
                    let view = cx.entity().downgrade();
                    body = body.child(crate::ui::error_banner(
                        note,
                        move |_, _, cx: &mut App| {
                            let retry = retry.clone();
                            let _ = view.update(cx, |this, cx| {
                                retry(this, SongInfoAction::RetryGenius, cx)
                            });
                        },
                        cx,
                    ))
                }
                GeniusState::Found(song) => {
                    if !song.relations.is_empty() {
                        let mut rel_list = v_flex().gap_3();
                        let mut rel_ix = 0usize;
                        for rel in &song.relations {
                            let mut rows = Vec::new();
                            for related in &rel.songs {
                                rel_ix += 1;
                                let album = info
                                    .related_albums
                                    .get(&related_key(&related.title, &related.artist))
                                    .cloned();
                                let trailing = if album.is_some() {
                                    div()
                                        .flex_none()
                                        .px_1p5()
                                        .rounded_sm()
                                        .bg(theme.primary.opacity(0.15))
                                        .text_xs()
                                        .text_color(theme.primary)
                                        .child("In library")
                                        .into_any_element()
                                } else if related.url.is_some() {
                                    gpui_component::Icon::new(IconName::ExternalLink)
                                        .size_3()
                                        .text_color(theme.muted_foreground)
                                        .into_any_element()
                                } else {
                                    div().into_any_element()
                                };
                                let mut line = h_flex()
                                    .id(("song-related", rel_ix))
                                    .gap_3()
                                    .px_3()
                                    .py_1p5()
                                    .child(
                                        app_icon(icons::MUSIC)
                                            .size_4()
                                            .flex_none()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .truncate()
                                                    .child(related.title.clone()),
                                            )
                                            .when(!related.artist.is_empty(), |this| {
                                                this.child(
                                                    div()
                                                        .text_xs()
                                                        .truncate()
                                                        .text_color(theme.muted_foreground)
                                                        .child(related.artist.clone()),
                                                )
                                            }),
                                    )
                                    .child(trailing);
                                if album.is_some() || related.url.is_some() {
                                    line = line
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.secondary_hover));
                                }
                                if active {
                                    if let Some(album) = album {
                                        line = line.on_click(act(
                                            &on_action,
                                            SongInfoAction::OpenAlbum(album),
                                            cx,
                                        ));
                                    } else if let Some(url) = related.url.clone() {
                                        line = line
                                            .on_click(move |_, _, cx: &mut App| cx.open_url(&url));
                                    }
                                }
                                rows.push(line.into_any_element());
                            }
                            rel_list = rel_list.child(
                                v_flex()
                                    .gap_1()
                                    .child(div().text_sm().font_medium().child(rel.label.clone()))
                                    .child(boxed(rows, &theme)),
                            );
                        }
                        body = body.child(titled("Samples & relations", None, rel_list, &theme));
                    }
                    if let Some(about) = &song.description {
                        let preview = about_preview(about);
                        let text = match (&preview, info.about_expanded) {
                            (Some(p), false) => p.clone(),
                            _ => about.clone(),
                        };
                        let verified = song.description_verified.then(|| {
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.primary)
                                .child(app_icon(icons::BADGE_CHECK).size_3())
                                .child("Verified by the artist")
                                .into_any_element()
                        });
                        body = body.child(titled(
                            "About",
                            verified,
                            v_flex()
                                .gap_2()
                                .px_4()
                                .py_3()
                                .rounded_lg()
                                .bg(theme.secondary)
                                .child(div().text_sm().line_height(gpui::rems(1.4)).child(text))
                                .when(preview.is_some(), |this| {
                                    this.child(
                                        h_flex().child(
                                            Button::new("song-about-more")
                                                .ghost()
                                                .xsmall()
                                                .label(if info.about_expanded {
                                                    "Less"
                                                } else {
                                                    "More"
                                                })
                                                .when(active, |b| {
                                                    b.on_click(act(
                                                        &on_action,
                                                        SongInfoAction::ToggleAbout,
                                                        cx,
                                                    ))
                                                }),
                                        ),
                                    )
                                }),
                            &theme,
                        ));
                    }
                    // Who verified the page, and the way there.
                    let url = song.url.clone();
                    body = body.child(
                        h_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .when(!song.verified_by.is_empty(), |this| {
                                        this.child(app_icon(icons::BADGE_CHECK).size_3()).child(
                                            div().truncate().child(format!(
                                                "Verified on Genius by {}",
                                                song.verified_by.join(", ")
                                            )),
                                        )
                                    }),
                            )
                            .when(!url.is_empty(), |this| {
                                this.child(
                                    Button::new("song-genius-link")
                                        .ghost()
                                        .xsmall()
                                        .icon(app_icon(icons::BRAND_GENIUS))
                                        .label("View on Genius")
                                        .when(active, |b| {
                                            b.on_click(move |_, _, cx: &mut App| cx.open_url(&url))
                                        }),
                                )
                            }),
                    );
                }
            }
            if let Some(why) = &info.token_rejected {
                body = body.child(status(&format!(
                    "Genius refused the API token: {why}. It must be the Client Access Token \
                     (Generate Access Token on genius.com/api-clients), not the client ID or \
                     secret."
                )));
            }
            if info.genius_blocked {
                body = body.child(status(
                    "Genius's bot check refused the request before reading the token — try again later",
                ));
            }
            body
        }
    };

    let subtitle = {
        let line = header_line(&info.song);
        let state = if info.loading { " · updating…" } else { "" };
        if line.is_empty() {
            format!("Song details{state}")
        } else {
            format!("{line}{state}")
        }
    };
    let segment = |id: &'static str, label: &'static str, tab: SongInfoTab, cx: &mut Context<V>| {
        let selected = info.tab == tab;
        div()
            .id(id)
            .px_3()
            .py_1()
            .rounded_md()
            .text_sm()
            .when(selected, |d| {
                d.bg(theme.background)
                    .text_color(theme.foreground)
                    .font_medium()
                    .shadow_sm()
            })
            .when(!selected, |d| {
                d.text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.foreground))
            })
            .child(label)
            .when(active && !selected, |d| {
                d.on_click(act(&on_action, SongInfoAction::Tab(tab), cx))
            })
    };
    let tabs = h_flex()
        .p_0p5()
        .gap_0p5()
        .rounded_lg()
        .bg(theme.muted)
        .child(segment(
            "song-info-tab-credits",
            "Credits",
            SongInfoTab::Credits,
            cx,
        ))
        .child(segment(
            "song-info-tab-details",
            "Details",
            SongInfoTab::Details,
            cx,
        ));

    let cover = match &info.cover {
        Some(path) => gpui::img(path.clone())
            .size(px(64.))
            .flex_none()
            .rounded_lg()
            .into_any_element(),
        None => div()
            .size(px(64.))
            .flex_none()
            .rounded_lg()
            .bg(theme.secondary)
            .flex()
            .items_center()
            .justify_center()
            .child(
                app_icon(icons::MUSIC)
                    .size_6()
                    .text_color(theme.muted_foreground),
            )
            .into_any_element(),
    };

    // Where else to hear it: brand marks under the artist line, named on
    // hover. The Genius page has its own link at the foot of the tab.
    let listen = genius_song.filter(|s| !s.media.is_empty()).map(|song| {
        let mut row = h_flex().gap_0p5().pt_1().ml(px(-6.));
        for (i, (label, url)) in listen_links(song)
            .into_iter()
            .filter(|(label, _)| label != "Genius")
            .enumerate()
        {
            let tip = format!("Open on {label}");
            row = row.child(
                Button::new(("song-listen", i))
                    .ghost()
                    .xsmall()
                    .icon(listen_icon(&label))
                    .tooltip(if label == "Music video" {
                        "Watch the music video on YouTube".to_string()
                    } else {
                        tip
                    })
                    .when(active, |b| {
                        b.on_click(move |_, _, cx: &mut App| cx.open_url(&url))
                    }),
            );
        }
        row
    });
    let close_button = act(&on_action, SongInfoAction::Close, cx);
    let on_backdrop = on_action.clone();
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
                cx.listener(move |this, _, _, cx| on_backdrop(this, SongInfoAction::Close, cx)),
            )
        })
        .child(
            v_flex()
                .w(px(640.))
                .max_w_full()
                .max_h(px(max_h))
                .opacity(t)
                .rounded_xl()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .shadow_xl()
                .occlude()
                // Clicks inside the card must not reach the backdrop.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    h_flex()
                        .items_center()
                        .gap_4()
                        .px_5()
                        .pt_5()
                        .pb_4()
                        .child(cover)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_semibold()
                                        .truncate()
                                        .child(info.song.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(subtitle),
                                )
                                .children(listen),
                        )
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .child(
                                    Button::new("song-info-copy")
                                        .ghost()
                                        .small()
                                        .icon(IconName::Copy)
                                        .tooltip("Copy all details")
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
                                        .when(active, |b| b.on_click(close_button)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .px_5()
                        .pb_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(tabs),
                )
                .child(
                    div()
                        .id("song-info-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_5()
                        .pt_4()
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
    fn people_live_on_the_credits_tab_not_in_details() {
        let s = song(serde_json::json!({
            "id": "s-1", "title": "Hurt", "artist": "Johnny Cash",
            "contributors": [
                {"role": "composer", "artist": {"id": "a", "name": "Trent Reznor"}},
                {"role": "producer", "artist": {"id": "b", "name": "Rick Rubin"}}
            ]
        }));
        let sections = song_sections(&s);
        assert!(sections.iter().all(|s| s.title != "Credits"));
        assert!(
            sections
                .iter()
                .flat_map(|s| &s.rows)
                .all(|(label, _)| label != "Composer" && label != "Artist")
        );
        let info = SongInfo::new(s, false, 1);
        let roles: Vec<_> = info.groups().into_iter().map(|g| g.role).collect();
        assert_eq!(roles, ["Artist", "Composer", "Producer"]);
    }

    #[test]
    fn cached_ids_make_text_credits_clickable() {
        let s = song(serde_json::json!({
            "id": "s-1", "title": "Laurea ad honorem", "artist": "Marracash feat. Calcutta"
        }));
        let mut info = SongInfo::new(s, false, 1);
        info.credit_ids.insert("calcutta".into(), "c-id".into());
        let groups = info.groups();
        let featuring = groups.iter().find(|g| g.role == "Featuring").unwrap();
        assert_eq!(featuring.people[0].id.as_deref(), Some("c-id"));
    }

    #[test]
    fn long_descriptions_get_a_preview_cut_at_a_word() {
        assert_eq!(about_preview("short"), None);
        let long = "word ".repeat(200);
        let preview = about_preview(&long).unwrap();
        assert!(preview.ends_with("word…"));
        assert!(preview.chars().count() <= ABOUT_PREVIEW_CHARS + 1);
    }

    #[test]
    fn credits_text_lists_roles_relations_and_the_link() {
        let s = song(serde_json::json!({"id": "1", "title": "Stronger", "artist": "Kanye West"}));
        let genius = GeniusSong {
            url: "https://genius.com/x".into(),
            relations: vec![crate::services::genius::Relation {
                label: "Samples".into(),
                songs: vec![crate::services::genius::RelatedSong {
                    title: "Harder, Better, Faster, Stronger".into(),
                    artist: "Daft Punk".into(),
                    url: None,
                }],
            }],
            ..GeniusSong::default()
        };
        let text = credits_text(&credit_groups(&s, Some(&genius), &|_| None), Some(&genius));
        assert!(text.contains("  Artist: Kanye West\n"));
        assert!(text.contains("  Samples: Harder, Better, Faster, Stronger — Daft Punk\n"));
        assert!(text.ends_with("  Genius: https://genius.com/x\n"));
    }

    #[test]
    fn view_counts_read_at_a_glance() {
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1K");
        assert_eq!(compact_count(1_234), "1.2K");
        assert_eq!(compact_count(9_960), "10K");
        assert_eq!(compact_count(34_600), "35K");
        assert_eq!(compact_count(999_999), "1M");
        assert_eq!(compact_count(1_234_567), "1.2M");
        assert_eq!(compact_count(2_000_000_000), "2B");
    }

    #[test]
    fn facts_and_links_skip_what_genius_lacks() {
        use crate::services::genius::MediaLink;
        let mut g = GeniusSong {
            url: "https://genius.com/x".into(),
            release_date: Some("July 31, 2007".into()),
            language: Some("English".into()),
            pageviews: Some(1_500_000),
            hot: true,
            media: vec![
                MediaLink {
                    service: "Spotify".into(),
                    url: "s".into(),
                },
                MediaLink {
                    service: "YouTube".into(),
                    url: "y".into(),
                },
            ],
            ..GeniusSong::default()
        };
        let labels: Vec<_> = facts(&g).iter().map(|f| f.label).collect();
        assert_eq!(labels, ["Released", "Language", "Genius views"]);
        assert!(facts(&g)[2].hot);
        assert_eq!(facts(&g)[2].value, "1.5M");
        let links: Vec<_> = listen_links(&g).into_iter().map(|(l, _)| l).collect();
        assert_eq!(links, ["Spotify", "YouTube", "Genius"]);
        g.has_video = true;
        assert_eq!(listen_links(&g)[1].0, "Music video");
        assert!(facts(&GeniusSong::default()).is_empty());
        assert!(!facts(&g)[0].wide());
        g.recording_location = Some("Angelic Studios (Banbury, Oxfordshire, England)".into());
        assert!(facts(&g)[1].wide());
    }

    #[test]
    fn header_line_joins_what_is_known() {
        let s = song(serde_json::json!({
            "id": "1", "title": "T", "artist": "A", "album": "B", "year": 2007
        }));
        assert_eq!(header_line(&s), "A · B · 2007");
        let s = song(serde_json::json!({"id": "1", "title": "T", "album": "B"}));
        assert_eq!(header_line(&s), "B");
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
