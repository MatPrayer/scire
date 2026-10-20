//! Who is on a song, read from every place tags spell it.
//!
//! A server's `artists` array lists a guest only when the file carries a
//! multi-valued `ARTISTS` tag. Most libraries don't: the guest lives in the
//! display string (`artist: "Marracash feat. Calcutta"`), in the title
//! (`"CH4NGE (feat. 可不)"`, `"Cinema (Virtual Riot remix)"`), or in an
//! OpenSubsonic `remixer` contributor. [`Song::credits`] reads all of them and
//! files each name under one [`CreditRole`].
//!
//! Splitting a joined string is guesswork — "Earth, Wind & Fire" is one band,
//! "Charli xcx, Rico Nasty & Kero Kero Bonito" three guests — so the splitter
//! takes a `known` test: a run of pieces that names an artist the caller knows
//! stays whole. A song's own `artists`/`contributors`/`albumArtists` are always
//! known; the sync adds every artist the cache holds.

use crate::Song;

/// What a credited name did on a song.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CreditRole {
    /// A performer the song is filed under.
    Main,
    /// A guest: "feat.", "ft.", "featuring", "(with …)".
    Featured,
    /// Whoever remixed this version.
    Remixer,
}

impl CreditRole {
    /// The stable name stored in the cache.
    pub fn as_str(self) -> &'static str {
        match self {
            CreditRole::Main => "artist",
            CreditRole::Featured => "featured",
            CreditRole::Remixer => "remixer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "artist" => Some(CreditRole::Main),
            "featured" => Some(CreditRole::Featured),
            "remixer" => Some(CreditRole::Remixer),
            _ => None,
        }
    }
}

/// Where a credit was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CreditSource {
    /// The server's structured credits: OpenSubsonic `artists`,
    /// `contributors`, or a vanilla `artistId`.
    Server,
    /// Parsed out of the text the tags carry: "A feat. B", "Song (C Remix)".
    Tags,
}

/// One credited name, with the server's artist id when one is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    pub role: CreditRole,
    pub name: String,
    pub id: Option<String>,
    pub source: CreditSource,
}

impl Song {
    /// Every credit on the song, ids from the song's own arrays only.
    pub fn credits(&self) -> Vec<Credit> {
        self.credits_with(|_| None)
    }

    /// Every credit on the song: main artists first, then guests, then
    /// remixers, each name once (the first role wins). `lookup` resolves a
    /// name the song carries no id for — and a name it resolves also counts as
    /// known when splitting joined strings.
    pub fn credits_with(&self, lookup: impl Fn(&str) -> Option<String>) -> Vec<Credit> {
        let d = &self.details;
        // Names the song itself pins to an id.
        let mut ids: Vec<(String, String)> = Vec::new();
        let own = self
            .artists
            .iter()
            .chain(d.album_artists.iter())
            .chain(d.contributors.iter().map(|c| &c.artist));
        for a in own {
            if !a.id.is_empty() && !a.name.trim().is_empty() {
                ids.push((fold(&a.name), a.id.clone()));
            }
        }
        let id_of = |name: &str| -> Option<String> {
            let folded = fold(name);
            ids.iter()
                .find(|(n, _)| *n == folded)
                .map(|(_, id)| id.clone())
                .or_else(|| lookup(name.trim()))
        };
        let known = |name: &str| id_of(name).is_some();

        let display = d
            .display_artist
            .as_deref()
            .or(self.artist.as_deref())
            .unwrap_or_default();
        let (display_main, display_feat) = split_display_artist(display, &known);
        let (title_feat, title_remix) = title_credits(&self.title, &known);

        let mut featured: Vec<String> = display_feat;
        featured.extend(title_feat);
        let mut remixers: Vec<String> = title_remix;
        remixers.extend(
            d.contributors
                .iter()
                .filter(|c| c.role.eq_ignore_ascii_case("remixer"))
                .map(|c| c.artist.name.clone()),
        );

        // The `artists` array is authoritative for *who*, not for the role: a
        // remix tagged `ARTISTS=Virtual Riot` on "Benny Benassi feat. Gary Go"
        // is Virtual Riot's remix, not Virtual Riot's song.
        let has = |list: &[String], name: &str| {
            let folded = fold(name);
            list.iter().any(|n| fold(n) == folded)
        };
        let mut mains: Vec<String> = Vec::new();
        for a in &self.artists {
            let name = &a.name;
            if has(&display_main, name) || !(has(&featured, name) || has(&remixers, name)) {
                mains.push(name.clone());
            }
        }
        mains.extend(display_main);

        // Names the server spells out itself, rather than us reading them.
        let structured: Vec<String> = self
            .artists
            .iter()
            .map(|a| fold(&a.name))
            .chain(
                d.contributors
                    .iter()
                    .filter(|c| c.role.eq_ignore_ascii_case("remixer"))
                    .map(|c| fold(&c.artist.name)),
            )
            .collect();

        let mut out: Vec<Credit> = Vec::new();
        let roles = [
            (CreditRole::Main, mains),
            (CreditRole::Featured, featured),
            (CreditRole::Remixer, remixers),
        ];
        for (role, names) in roles {
            for name in names {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                let folded = fold(name);
                if out.iter().any(|c| fold(&c.name) == folded) {
                    continue;
                }
                let source = if structured.contains(&folded) {
                    CreditSource::Server
                } else {
                    CreditSource::Tags
                };
                out.push(Credit {
                    role,
                    name: name.to_string(),
                    id: id_of(name),
                    source,
                });
            }
        }
        // A vanilla server has no arrays, only `artistId`: the first credit's.
        if self.artists.is_empty()
            && let Some(id) = self.artist_id.as_deref().filter(|id| !id.is_empty())
            && let Some(first) = out.first_mut()
            && first.role == CreditRole::Main
            && first.id.is_none()
        {
            first.id = Some(id.to_string());
            first.source = CreditSource::Server;
        }
        out
    }
}

/// What a track list adds after a title: the guests and remixers the title
/// doesn't already name, as "feat. A & B" / "remix by C". `None` when the
/// title says it all.
pub fn guest_summary(credits: &[Credit], title: &str) -> Option<String> {
    let title = fold(title);
    let unnamed = |role: CreditRole| -> Vec<&str> {
        credits
            .iter()
            .filter(|c| c.role == role && !title.contains(&fold(&c.name)))
            .map(|c| c.name.as_str())
            .collect()
    };
    let mut parts = Vec::new();
    let featured = unnamed(CreditRole::Featured);
    if !featured.is_empty() {
        parts.push(format!("feat. {}", join_names(&featured)));
    }
    let remixers = unnamed(CreditRole::Remixer);
    if !remixers.is_empty() {
        parts.push(format!("remix by {}", join_names(&remixers)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// "A", "A & B", "A, B & C".
pub fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [head @ .., last] => format!("{} & {last}", head.join(", ")),
    }
}

/// Case- and spacing-insensitive form of a name, for comparing credits.
pub fn fold(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Split a display artist ("A & B feat. C, D") into main and featured names.
pub fn split_display_artist(
    display: &str,
    known: &dyn Fn(&str) -> bool,
) -> (Vec<String>, Vec<String>) {
    let display = display.trim();
    if display.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // A name that is itself an artist ("Feat & Co.") is never taken apart.
    if known(display) {
        return (vec![display.to_string()], Vec::new());
    }
    match find_feat(display) {
        Some((start, end)) => {
            let main = clean_name(&display[..start]);
            let mut featured = Vec::new();
            // "A feat. B feat. C": every later marker is another guest list.
            let mut rest = &display[end..];
            while let Some((s, e)) = find_feat(rest) {
                featured.extend(split_names(&rest[..s], known, Soft::Always));
                rest = &rest[e..];
            }
            featured.extend(split_names(rest, known, Soft::Always));
            (split_names(main, known, Soft::IfKnown), featured)
        }
        None => (split_names(display, known, Soft::IfKnown), Vec::new()),
    }
}

/// Guests and remixers a title names: `(feat. X)`, `[ft. X]`, a bare
/// `feat. X` before the next bracket or " - ", `(with X)` when X is a known
/// artist, `(X Remix)`, `(remix by X)`.
pub fn title_credits(title: &str, known: &dyn Fn(&str) -> bool) -> (Vec<String>, Vec<String>) {
    let mut featured = Vec::new();
    let mut remixers = Vec::new();
    let groups = bracket_groups(title);

    for &(open, close) in &groups {
        let content = title[open..close].trim();
        let lower = content.to_ascii_lowercase();
        if let Some((0, end)) = find_feat(content) {
            featured.extend(split_names(&content[end..], known, Soft::Always));
        } else if let Some(rest) = lower.strip_prefix("with ") {
            let rest = &content[content.len() - rest.len()..];
            // "(with choir & organ)" is an arrangement, not a guest: only
            // names the caller knows count.
            featured.extend(
                split_names(rest, known, Soft::Always)
                    .into_iter()
                    .filter(|n| known(n)),
            );
        } else if let Some(names) = remixer_part(content) {
            remixers.extend(
                split_names(&names, known, Soft::Always)
                    .into_iter()
                    .filter(|n| plausible_name(n)),
            );
        }
    }

    // A bare "feat." outside every bracket runs to the next bracket or dash.
    let mut from = 0;
    while let Some((s, e)) = find_feat(&title[from..]) {
        let (s, e) = (from + s, from + e);
        from = e;
        if groups.iter().any(|&(o, c)| s >= o && s < c) {
            continue;
        }
        let tail = &title[e..];
        let stop = [tail.find(" - "), tail.find(['(', '[', '（', '【'])]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(tail.len());
        featured.extend(split_names(&tail[..stop], known, Soft::Always));
    }
    (featured, remixers)
}

const FEAT_MARKERS: [&str; 5] = ["featuring", "feat.", "feat", "ft.", "ft"];

/// Byte range of the first feat marker (with the spaces after it). A marker
/// starts the string or follows a space or an opening bracket, and an undotted
/// one must be followed by a space — "Feathers" and "Daft Punk" are names.
fn find_feat(s: &str) -> Option<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find('f') {
        let i = search + rel;
        search = i + 1;
        let before_ok = lower[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '（' | '【'));
        if !before_ok {
            continue;
        }
        for marker in FEAT_MARKERS {
            if !lower[i..].starts_with(marker) {
                continue;
            }
            let after = &s[i + marker.len()..];
            let ok = match after.chars().next() {
                Some(c) if c.is_whitespace() => true,
                Some(_) => marker.ends_with('.'),
                None => false,
            };
            let names = after.trim_start();
            if ok && !names.is_empty() {
                return Some((i, s.len() - names.len()));
            }
            break;
        }
    }
    None
}

/// Byte ranges of the insides of `(…)` / `[…]` groups (not nested).
fn bracket_groups(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some((rel, c)) = s[i..]
        .char_indices()
        .find(|(_, c)| matches!(c, '(' | '[' | '（' | '【'))
    {
        let close = match c {
            '(' => ')',
            '[' => ']',
            '（' => '）',
            _ => '】',
        };
        let open = i + rel + c.len_utf8();
        match s[open..].find(close) {
            Some(r) => {
                out.push((open, open + r));
                i = open + r + close.len_utf8();
            }
            None => i = open,
        }
    }
    out
}

/// The name half of a remix credit: "X Remix", "X's rmx", "Remix by X".
fn remixer_part(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    for prefix in ["remixed by ", "remix by "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return Some(content[content.len() - rest.len()..].to_string());
        }
    }
    const SUFFIXES: [&str; 6] = [" remix", " rmx", " rework", " bootleg", " re-edit", " edit"];
    let cut = SUFFIXES
        .iter()
        .find_map(|s| lower.ends_with(s).then(|| content.len() - s.len()))?;
    let mut name = &content[..cut];
    // "Djrum remix – short edit": the credit ends at the first mix word or dash.
    let lower = name.to_ascii_lowercase();
    if let Some(end) = [" remix", " rmx", " – ", " - "]
        .iter()
        .filter_map(|m| lower.find(m))
        .min()
    {
        name = &name[..end];
    }
    // A quoted nickname closing the credit is the remix's name, not its
    // author's: "Danny L Harle 'Harlecore' remix", "Camellia’s “Bad Psy!!”
    // Remix". One inside a name ("Wasei "JJ" Chikada") stays.
    let trimmed = name.trim_end();
    if trimmed.ends_with(['”', '"', '’', '\''])
        && let Some(last) = trimmed.char_indices().next_back().map(|(i, _)| i)
        && let Some(quote) = trimmed[..last]
            .char_indices()
            .find(|&(i, c)| {
                matches!(c, '“' | '"' | '‘') || (c == '\'' && trimmed[..i].ends_with(' '))
            })
            .map(|(i, _)| i)
    {
        name = &trimmed[..quote];
    }
    let mut words: Vec<&str> = name.split_whitespace().collect();
    // "J Majik VIP", "A-Trak radio": the mix kind trails the name.
    const TRAILING: [&str; 13] = [
        "vip",
        "radio",
        "vocal",
        "extended",
        "instrumental",
        "dub",
        "edit",
        "mix",
        "version",
        "original",
        "piano",
        "acoustic",
        "short",
    ];
    while words.len() > 1
        && words
            .last()
            .is_some_and(|w| TRAILING.contains(&w.to_lowercase().as_str()))
    {
        words.pop();
    }
    let mut name = words.join(" ");
    for suffix in ["'s", "’s", "'", "’"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_string();
            break;
        }
    }
    // Remix credits spell pairs out: "Synova and Clickbate Remix".
    Some(name.replace(" and ", " & "))
}

/// Words that say what kind of mix a version is, not who made it.
const MIX_KINDS: &[&str] = &[
    "original",
    "extended",
    "radio",
    "club",
    "dub",
    "short",
    "long",
    "official",
    "instrumental",
    "single",
    "album",
    "bonus",
    "live",
    "the",
    "version",
    "vocal",
    "acapella",
    "a",
    "cappella",
    "remastered",
    "remaster",
    "edit",
    "mix",
    "vip",
    "full",
    "alternate",
    "alt",
    "special",
    "main",
    "clean",
    "explicit",
    "new",
    "remix",
    "rmx",
    "demo",
    "mono",
    "stereo",
    "deluxe",
    "extra",
    "slowed",
    "sped",
    "up",
    "reverb",
    "unreleased",
    "dj",
    "re-edit",
    "rework",
    "bootleg",
    "and",
    "&",
    "-",
    "uk",
    "us",
    "late",
    "night",
    "my",
    "own",
    "self",
    "tv",
    "film",
    "censored",
    "uncensored",
    "christmas",
    "xmas",
    "dubstep",
    "bass",
    "drum",
    "dnb",
    "house",
    "techno",
    "trap",
    "acoustic",
    "orchestral",
    "piano",
    "jazz",
    "rock",
    "ambient",
    "lofi",
    "lo-fi",
    "hardstyle",
    "garage",
    "chiptune",
    "8-bit",
    "8bit",
    "symphonic",
    "nightcore",
    "english",
    "japanese",
    "ver.",
    "ver",
];

fn is_mix_kind(word: &str) -> bool {
    let word = word.to_lowercase();
    MIX_KINDS.contains(&word.as_str())
        || word
            .chars()
            .all(|c| c.is_ascii_digit() || "'\".-".contains(c))
}

/// Whether a remix credit's name half is a name at all — "(Extended Remix)",
/// "(2012 Remix)" and "(Christmas Remix)" say what kind of mix, not who made it.
fn plausible_name(name: &str) -> bool {
    let mut words = name.split_whitespace().peekable();
    words.peek().is_some() && !words.all(is_mix_kind)
}

/// When to split on the separators bands use in their own names.
#[derive(Clone, Copy, PartialEq)]
enum Soft {
    /// Guest lists: "Charli xcx, Rico Nasty & Kero Kero Bonito" is three.
    Always,
    /// A main credit: "Simon & Garfunkel" stays whole unless one of its
    /// pieces is an artist the caller knows on its own.
    IfKnown,
}

/// Separators no artist name contains.
const HARD: [&str; 8] = [" • ", "•", "; ", ";", " / ", "、", "／", " ・ "];
/// Separators names do contain ("Earth, Wind & Fire", "Mumford & Sons").
const SOFT: [&str; 8] = [", ", ",", " & ", "＆", " x ", " × ", " vs. ", " vs "];
/// Guest lists also join with a word ("feat. Tove Lo and ALMA") and with
/// "・" ("feat. 初音ミク・鏡音リン"), which a main credit can't be split on —
/// it sits inside names too ("ラプラス・ダークネス").
const SOFT_LIST: [&str; 10] = [
    ", ", ",", " & ", "＆", " x ", " × ", " vs. ", " vs ", "・", " and ",
];

/// Split a joined credit into names. A run of pieces that names a known
/// artist is kept together, longest run first.
fn split_names(joined: &str, known: &dyn Fn(&str) -> bool, soft: Soft) -> Vec<String> {
    let mut out = Vec::new();
    for segment in split_on(joined, &HARD) {
        let segment = clean_name(segment);
        if segment.is_empty() {
            continue;
        }
        if known(segment) {
            out.push(segment.to_string());
            continue;
        }
        let pieces = match soft {
            Soft::Always => split_on(segment, &SOFT_LIST),
            Soft::IfKnown => split_on(segment, &SOFT),
        };
        let mut merged: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < pieces.len() {
            let start = offset_in(segment, pieces[i]);
            let mut taken = i;
            for j in (i + 1..pieces.len()).rev() {
                let end = offset_in(segment, pieces[j]) + pieces[j].len();
                if known(segment[start..end].trim()) {
                    taken = j;
                    break;
                }
            }
            let end = offset_in(segment, pieces[taken]) + pieces[taken].len();
            merged.push(clean_name(&segment[start..end]));
            i = taken + 1;
        }
        merged.retain(|n| !n.is_empty());
        let split = match soft {
            Soft::Always => true,
            Soft::IfKnown => merged.len() > 1 && merged.iter().any(|n| known(n)),
        };
        if split {
            out.extend(merged.into_iter().map(str::to_string));
        } else {
            out.push(segment.to_string());
        }
    }
    out
}

/// Pieces of `s` between any of `seps` (case-insensitive), as subslices.
fn split_on<'a>(s: &'a str, seps: &[&str]) -> Vec<&'a str> {
    let lower = s.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut piece_start = 0;
    let mut i = 0;
    while i < s.len() {
        if let Some(sep) = seps.iter().find(|sep| lower[i..].starts_with(*sep)) {
            out.push(&s[piece_start..i]);
            i += sep.len();
            piece_start = i;
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out.push(&s[piece_start..]);
    out
}

/// Byte offset of a subslice inside its parent.
fn offset_in(parent: &str, sub: &str) -> usize {
    sub.as_ptr() as usize - parent.as_ptr() as usize
}

/// A name with surrounding spaces, brackets and quotes trimmed.
fn clean_name(s: &str) -> &str {
    s.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | ')'
                    | '['
                    | ']'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '"'
                    | '“'
                    | '”'
                    | '/'
                    | ','
                    | '–'
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtistRef, Contributor};

    fn none(_: &str) -> bool {
        false
    }

    fn song(title: &str, artist: &str) -> Song {
        serde_json::from_value(serde_json::json!({
            "id": "s", "title": title, "artist": artist,
        }))
        .unwrap()
    }

    fn names(credits: &[Credit], role: CreditRole) -> Vec<&str> {
        credits
            .iter()
            .filter(|c| c.role == role)
            .map(|c| c.name.as_str())
            .collect()
    }

    #[test]
    fn feat_markers_need_a_word_boundary() {
        assert_eq!(find_feat("Feathers"), None);
        assert_eq!(find_feat("Daft Punk"), None);
        assert_eq!(find_feat("A feat"), None);
        assert!(find_feat("A feat. B").is_some());
        assert!(find_feat("魔法 feat.理芽").is_some());
        assert!(find_feat("A ft B").is_some());
        assert!(find_feat("A Featuring B").is_some());
        assert_eq!(find_feat("(feat. X)"), Some((1, 7)));
        assert_eq!(find_feat("feat. X"), Some((0, 6)));
    }

    #[test]
    fn display_artist_splits_main_from_guests() {
        let (main, feat) = split_display_artist("Marracash feat. Calcutta", &none);
        assert_eq!(main, ["Marracash"]);
        assert_eq!(feat, ["Calcutta"]);

        let (main, feat) = split_display_artist(
            "100 gecs feat. Charli xcx, Rico Nasty & Kero Kero Bonito",
            &none,
        );
        assert_eq!(main, ["100 gecs"]);
        assert_eq!(feat, ["Charli xcx", "Rico Nasty", "Kero Kero Bonito"]);

        let (main, feat) = split_display_artist("すりぃ feat.鏡音レン", &none);
        assert_eq!(
            (main, feat),
            (vec!["すりぃ".into()], vec!["鏡音レン".into()])
        );
    }

    #[test]
    fn band_names_with_ampersands_stay_whole() {
        let (main, _) = split_display_artist("Simon & Garfunkel", &none);
        assert_eq!(main, ["Simon & Garfunkel"]);
        let (main, _) = split_display_artist("Earth, Wind & Fire feat. X", &none);
        assert_eq!(main, ["Earth, Wind & Fire"]);
        // ...but a collaboration of two known artists is two.
        let known = |n: &str| n == "Justice" || n == "RIMON";
        let (main, _) = split_display_artist("Justice & RIMON", &known);
        assert_eq!(main, ["Justice", "RIMON"]);
        // A known band inside a guest list is not cut up.
        let known = |n: &str| n == "Mumford & Sons";
        let (_, feat) = split_display_artist("A feat. Mumford & Sons, B", &known);
        assert_eq!(feat, ["Mumford & Sons", "B"]);
    }

    #[test]
    fn hard_separators_always_split() {
        let (main, _) = split_display_artist("Pinky Pop Hepburn • CHIBANYAN", &none);
        assert_eq!(main, ["Pinky Pop Hepburn", "CHIBANYAN"]);
        let (main, _) = split_display_artist("KAF; KAFU", &none);
        assert_eq!(main, ["KAF", "KAFU"]);
        let (main, _) = split_display_artist("放課後ティータイム ・ 豊崎愛生", &none);
        assert_eq!(main, ["放課後ティータイム", "豊崎愛生"]);
        let (_, feat) = split_display_artist("A feat. 初音ミク・鏡音リン", &none);
        assert_eq!(feat, ["初音ミク", "鏡音リン"]);
        let (_, feat) = split_display_artist("A feat. Tove Lo and ALMA", &none);
        assert_eq!(feat, ["Tove Lo", "ALMA"]);
        let known = |n: &str| n == "紫咲シオン";
        let (main, _) = split_display_artist("紫咲シオン & ラプラス・ダークネス", &known);
        assert_eq!(main, ["紫咲シオン", "ラプラス・ダークネス"]);
        let (main, _) = split_display_artist("AC/DC", &none);
        assert_eq!(main, ["AC/DC"]);
    }

    #[test]
    fn titles_name_guests_and_remixers() {
        let t = |s| title_credits(s, &none);
        assert_eq!(t("CH4NGE (feat. 可不)").0, ["可不"]);
        assert_eq!(
            t("魔女(真) [feat. KAF, RIM, Isekaijoucho, Harusaruhi & KOKO]").0,
            ["KAF", "RIM", "Isekaijoucho", "Harusaruhi", "KOKO"]
        );
        assert_eq!(
            t("Sheath-Winged feat. renko (s7n Remix)"),
            (vec!["renko".into()], vec!["s7n".into()])
        );
        assert_eq!(t("カンザキイオリ Feat. 可不 - 朝日").0, ["可不"]);
        assert_eq!(
            t("The Hot Wind Blowing featuring Ferry Corsten (Platinum Mix)"),
            (vec!["Ferry Corsten".into()], vec![])
        );
        assert_eq!(t("Cinema (Virtual Riot remix)").1, ["Virtual Riot"]);
        assert_eq!(
            t("gec 2 Ü (Danny L Harle 'Harlecore' remix)").1,
            ["Danny L Harle"]
        );
        assert_eq!(t("Song (Someone's Remix)").1, ["Someone"]);
        assert_eq!(t("Song (Camellia’s “Bad Psy!!” Remix)").1, ["Camellia"]);
        assert_eq!(t("Song (J Majik VIP Remix)").1, ["J Majik"]);
        assert_eq!(t("Song (Interno 4 Remix)").1, ["Interno 4"]);
        assert_eq!(
            t(r#"Navi 3.0 [Wasei "JJ" Chikada Remix]"#).1,
            [r#"Wasei "JJ" Chikada"#]
        );
        assert_eq!(
            t("It’s You (underscores’ “It’s U” remix)").1,
            ["underscores"]
        );
        assert_eq!(t("Song (Djrum remix – short edit)").1, ["Djrum"]);
        assert_eq!(
            t("Song (Synova and Clickbate Remix)").1,
            ["Synova", "Clickbate"]
        );
        assert_eq!(t("Song [Remix by Alpha & Beta]").1, ["Alpha", "Beta"]);
        assert_eq!(
            t("Roma Sud (feat. Cris Ex) & Theme from Koh Samui").0,
            ["Cris Ex"]
        );
    }

    #[test]
    fn mix_kinds_are_not_remixers() {
        let t = |s| title_credits(s, &none).1;
        assert!(t("ringtone (remix)").is_empty());
        assert!(t("Song (Extended Remix)").is_empty());
        assert!(t("Song (2012 Remix)").is_empty());
        assert!(t("Song (Radio Edit)").is_empty());
        assert!(t("Song (Christmas Remix)").is_empty());
        assert!(t("Feathers").is_empty());
    }

    #[test]
    fn with_needs_a_known_artist() {
        assert!(
            title_credits("Dark Matter (with choir & organ)", &none)
                .0
                .is_empty()
        );
        let known = |n: &str| n == "Dua Lipa";
        assert_eq!(
            title_credits("Song (with Dua Lipa)", &known).0,
            ["Dua Lipa"]
        );
    }

    #[test]
    fn credits_merge_every_source_and_keep_ids() {
        let mut s = song("Cinema (Virtual Riot remix)", "Benny Benassi feat. Gary Go");
        s.artists = vec![ArtistRef {
            id: "vr".into(),
            name: "Virtual Riot".into(),
        }];
        let c = s.credits_with(|n| (n == "Benny Benassi").then(|| "bb".into()));
        assert_eq!(names(&c, CreditRole::Main), ["Benny Benassi"]);
        assert_eq!(names(&c, CreditRole::Featured), ["Gary Go"]);
        assert_eq!(names(&c, CreditRole::Remixer), ["Virtual Riot"]);
        assert_eq!(c[0].id.as_deref(), Some("bb"));
        assert_eq!(c[2].id.as_deref(), Some("vr"));
        assert_eq!(c[0].source, CreditSource::Tags);
        assert_eq!(c[1].source, CreditSource::Tags);
        assert_eq!(c[2].source, CreditSource::Server);
    }

    #[test]
    fn a_vanilla_artist_id_goes_to_the_lead_credit() {
        let mut s = song("Laurea ad honorem", "Marracash feat. Calcutta");
        s.artist_id = Some("m".into());
        let c = s.credits();
        assert_eq!(c[0].id.as_deref(), Some("m"));
        assert_eq!(c[1].id, None);
    }

    #[test]
    fn a_guest_named_twice_is_one_credit() {
        let s = song("CH4NGE (feat. 可不)", "Giga feat. 可不");
        let c = s.credits();
        assert_eq!(names(&c, CreditRole::Main), ["Giga"]);
        assert_eq!(names(&c, CreditRole::Featured), ["可不"]);
    }

    #[test]
    fn contributors_add_remixers_and_the_main_role_wins() {
        let mut s = song("Song", "A");
        s.details.contributors = vec![Contributor {
            role: "remixer".into(),
            sub_role: None,
            artist: ArtistRef {
                id: "r".into(),
                name: "R".into(),
            },
        }];
        let c = s.credits();
        assert_eq!(names(&c, CreditRole::Remixer), ["R"]);
        assert_eq!(c[1].id.as_deref(), Some("r"));

        let s = song("A Song (A Remix)", "A");
        assert!(names(&s.credits(), CreditRole::Remixer).is_empty());
    }

    #[test]
    fn guest_summary_skips_what_the_title_says() {
        let s = song("Laurea ad honorem", "Marracash feat. Calcutta");
        assert_eq!(
            guest_summary(&s.credits(), &s.title).as_deref(),
            Some("feat. Calcutta")
        );
        let s = song("CH4NGE (feat. 可不)", "Giga feat. 可不");
        assert_eq!(guest_summary(&s.credits(), &s.title), None);
        let s = song("Song", "A feat. B, C & D");
        assert_eq!(
            guest_summary(&s.credits(), &s.title).as_deref(),
            Some("feat. B, C & D")
        );
    }
}
