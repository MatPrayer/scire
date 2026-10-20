//! Who is on a song, from every source at once, for the details dialog's
//! Credits tab.
//!
//! Three sources say overlapping things in different words: the server's
//! structured credits (OpenSubsonic `artists`/`contributors`), what
//! `subsonic::credits` reads out of the tags' text ("A feat. B"), and Genius
//! ("Produced by", "Mixing Engineer"). [`credit_groups`] files them under one
//! set of roles, merges a person named by several sources into one entry that
//! remembers each source, and keeps a stable role order. Pure; tested below.

use subsonic::{CreditRole, CreditSource, Song};

use crate::services::genius::GeniusSong;

/// Which sources named a person in a role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sources {
    pub tags: bool,
    pub server: bool,
    pub genius: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Person {
    pub name: String,
    /// An artist page to open: the server's id, else a cached artist of that
    /// name.
    pub id: Option<String>,
    /// Their Genius page, when only Genius knows them.
    pub url: Option<String>,
    pub sources: Sources,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreditGroup {
    pub role: String,
    pub people: Vec<Person>,
}

#[derive(Clone, Copy)]
enum From {
    Tags,
    Server,
    Genius,
}

/// Roles drawn first, in this order; anything else follows as it arrives.
const ORDER: [&str; 13] = [
    "Artist",
    "Featuring",
    "Remixer",
    "Composer",
    "Lyricist",
    "Writer",
    "Producer",
    "Co-Producer",
    "Mixing Engineer",
    "Mastering Engineer",
    "Recording Engineer",
    "Arranger",
    "Conductor",
];

/// The one name a role goes by, whoever spelled it.
fn canonical_role(label: &str) -> String {
    let lower = label.trim().to_lowercase();
    let known = match lower.as_str() {
        "artist" | "main artist" | "primary artist" => "Artist",
        "featuring" | "featured" | "featured artist" | "featured artists" => "Featuring",
        "remixer" | "remixed by" | "remix" => "Remixer",
        "composer" | "composed by" | "composition" => "Composer",
        "lyricist" | "lyrics by" | "lyrics" => "Lyricist",
        "writer" | "written by" | "songwriter" | "songwriters" => "Writer",
        "producer" | "produced by" | "production" => "Producer",
        "co-producer" | "co-produced by" | "coproducer" => "Co-Producer",
        "mixer" | "mixing engineer" | "mixed by" | "mix engineer" | "mixing" => "Mixing Engineer",
        "mastering engineer" | "mastered by" | "mastering" => "Mastering Engineer",
        "engineer" | "recording engineer" | "recorded by" | "recording" => "Recording Engineer",
        "arranger" | "arranged by" | "arrangement" => "Arranger",
        "conductor" | "conducted by" => "Conductor",
        "djmixer" | "dj mix" | "dj mixer" => "DJ Mix",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    // Title-case the rest: "guitar" from a server `performer` sub-role and
    // Genius's "Guitar" are the same row.
    label
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => first.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

struct Groups(Vec<CreditGroup>);

impl Groups {
    fn add(&mut self, role: &str, name: &str, id: Option<String>, url: Option<String>, from: From) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let role = canonical_role(role);
        let group = match self.0.iter().position(|g| g.role == role) {
            Some(i) => &mut self.0[i],
            None => {
                self.0.push(CreditGroup {
                    role,
                    people: Vec::new(),
                });
                self.0.last_mut().expect("just pushed")
            }
        };
        let folded = subsonic::credits::fold(name);
        let person = match group
            .people
            .iter()
            .position(|p| subsonic::credits::fold(&p.name) == folded)
        {
            Some(i) => &mut group.people[i],
            None => {
                group.people.push(Person {
                    name: name.to_string(),
                    id: None,
                    url: None,
                    sources: Sources::default(),
                });
                group.people.last_mut().expect("just pushed")
            }
        };
        if person.id.is_none() {
            person.id = id;
        }
        if person.url.is_none() {
            person.url = url;
        }
        match from {
            From::Tags => person.sources.tags = true,
            From::Server => person.sources.server = true,
            From::Genius => person.sources.genius = true,
        }
    }

    /// Mark someone already listed under one of `roles` as also named by
    /// Genius; whether anyone was.
    fn confirm(&mut self, roles: &[&str], name: &str, url: Option<&str>) -> bool {
        let folded = subsonic::credits::fold(name);
        let mut found = false;
        for group in self
            .0
            .iter_mut()
            .filter(|g| roles.contains(&g.role.as_str()))
        {
            for p in &mut group.people {
                if subsonic::credits::fold(&p.name) == folded {
                    p.sources.genius = true;
                    if p.url.is_none() {
                        p.url = url.map(str::to_string);
                    }
                    found = true;
                }
            }
        }
        found
    }
}

/// Every credit on `song`, grouped by role. `ids` finds an artist page for a
/// name the song carries no id for.
pub fn credit_groups(
    song: &Song,
    genius: Option<&GeniusSong>,
    ids: &dyn Fn(&str) -> Option<String>,
) -> Vec<CreditGroup> {
    let mut groups = Groups(Vec::new());
    let id_for = |id: Option<String>, name: &str| id.or_else(|| ids(name));

    for c in song.credits() {
        let role = match c.role {
            CreditRole::Main => "Artist",
            CreditRole::Featured => "Featuring",
            CreditRole::Remixer => "Remixer",
        };
        let from = match c.source {
            CreditSource::Server => From::Server,
            CreditSource::Tags => From::Tags,
        };
        let id = id_for(c.id.clone(), &c.name);
        groups.add(role, &c.name, id, None, from);
    }

    let d = &song.details;
    // A local file's composer is a tag; a server track's is the server's.
    let composer_from = if song.local_path.is_some() {
        From::Tags
    } else {
        From::Server
    };
    for (id, name) in d.composers() {
        let id = id_for(id, &name);
        groups.add("Composer", &name, id, None, composer_from);
    }
    for c in &d.contributors {
        let role = c.role.to_lowercase();
        if role == "composer" || role == "remixer" {
            continue;
        }
        let label = match (
            role.as_str(),
            c.sub_role.as_deref().filter(|s| !s.is_empty()),
        ) {
            ("performer", Some(sub)) => sub.to_string(),
            _ => c.role.clone(),
        };
        let id = Some(c.artist.id.clone()).filter(|id| !id.is_empty());
        let id = id_for(id, &c.artist.name);
        groups.add(&label, &c.artist.name, id, None, From::Server);
    }

    if let Some(g) = genius {
        if !g.artist.is_empty() {
            groups.confirm(&["Artist"], &g.artist, None);
        }
        for (role, people) in &g.credits {
            let role = canonical_role(role);
            for p in people {
                // Genius's "Written By" is whoever wrote words or music; the
                // tags usually say which, so confirm those rows instead.
                if role == "Writer"
                    && groups.confirm(&["Composer", "Lyricist"], &p.name, p.url.as_deref())
                {
                    continue;
                }
                let id = ids(&p.name);
                groups.add(&role, &p.name, id, p.url.clone(), From::Genius);
            }
        }
    }

    let mut out = groups.0;
    out.retain(|g| !g.people.is_empty());
    // Stable: known roles in their order, the rest as they arrived.
    out.sort_by_key(|g| {
        ORDER
            .iter()
            .position(|r| *r == g.role)
            .unwrap_or(ORDER.len())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::genius::GeniusPerson;

    fn song(json: serde_json::Value) -> Song {
        serde_json::from_value(json).unwrap()
    }

    fn person(name: &str) -> GeniusPerson {
        GeniusPerson {
            name: name.into(),
            url: Some(format!("https://genius.com/artists/{name}")),
        }
    }

    fn roles(groups: &[CreditGroup]) -> Vec<&str> {
        groups.iter().map(|g| g.role.as_str()).collect()
    }

    #[test]
    fn sources_merge_into_one_person_per_role() {
        let s = song(serde_json::json!({
            "id": "1", "title": "Laurea ad honorem",
            "artist": "Marracash feat. Calcutta",
            "artists": [{"id": "m", "name": "Marracash"}],
            "contributors": [
                {"role": "producer", "artist": {"id": "p", "name": "Marz"}},
                {"role": "mixer", "artist": {"id": "x", "name": "Mixy"}}
            ]
        }));
        let genius = GeniusSong {
            artist: "Marracash".into(),
            credits: vec![
                ("Featuring".into(), vec![person("Calcutta")]),
                ("Producer".into(), vec![person("Marz"), person("Zef")]),
                ("Mixing Engineer".into(), vec![person("Mixy")]),
            ],
            ..GeniusSong::default()
        };
        let ids = |n: &str| (n == "Calcutta").then(|| "c".to_string());
        let g = credit_groups(&s, Some(&genius), &ids);
        assert_eq!(
            roles(&g),
            ["Artist", "Featuring", "Producer", "Mixing Engineer"]
        );
        let artist = &g[0].people[0];
        assert_eq!(artist.id.as_deref(), Some("m"));
        assert!(artist.sources.server && artist.sources.genius && !artist.sources.tags);
        let calcutta = &g[1].people[0];
        assert_eq!(calcutta.id.as_deref(), Some("c"));
        assert!(calcutta.sources.tags && calcutta.sources.genius);
        let producers: Vec<_> = g[2].people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(producers, ["Marz", "Zef"]);
        assert!(g[2].people[0].sources.server && g[2].people[0].sources.genius);
        assert!(g[2].people[1].url.is_some());
        // "mixer" and "Mixing Engineer" are one row.
        assert_eq!(g[3].people.len(), 1);
    }

    #[test]
    fn genius_writers_confirm_composers_instead_of_repeating_them() {
        let s = song(serde_json::json!({
            "id": "1", "title": "Hurt", "artist": "Johnny Cash",
            "contributors": [{"role": "composer", "artist": {"id": "t", "name": "Trent Reznor"}}]
        }));
        let genius = GeniusSong {
            credits: vec![(
                "Writer".into(),
                vec![person("Trent Reznor"), person("Someone")],
            )],
            ..GeniusSong::default()
        };
        let g = credit_groups(&s, Some(&genius), &|_| None);
        assert_eq!(roles(&g), ["Artist", "Composer", "Writer"]);
        assert!(g[1].people[0].sources.genius);
        assert_eq!(g[2].people.len(), 1);
        assert_eq!(g[2].people[0].name, "Someone");
    }

    #[test]
    fn performer_sub_roles_and_unknown_labels_keep_their_place() {
        let s = song(serde_json::json!({
            "id": "1", "title": "X", "artist": "A",
            "contributors": [
                {"role": "performer", "subRole": "guitar", "artist": {"id": "g", "name": "G"}}
            ]
        }));
        let genius = GeniusSong {
            credits: vec![
                ("Label".into(), vec![person("Def Jam")]),
                ("Guitar".into(), vec![person("H")]),
                ("Producer".into(), vec![person("P")]),
            ],
            ..GeniusSong::default()
        };
        let g = credit_groups(&s, Some(&genius), &|_| None);
        assert_eq!(roles(&g), ["Artist", "Producer", "Guitar", "Label"]);
        assert_eq!(g[2].people.len(), 2);
    }

    #[test]
    fn without_genius_only_the_song_speaks() {
        let s = song(serde_json::json!({"id": "1", "title": "Song (B Remix)", "artist": "A"}));
        let g = credit_groups(&s, None, &|_| None);
        assert_eq!(roles(&g), ["Artist", "Remixer"]);
        assert!(g[1].people[0].sources.tags);
    }
}
