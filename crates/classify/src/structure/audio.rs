//! Albums and audiobooks.

use std::collections::{BTreeMap, HashMap};

use super::movie::book_meta;
use super::{
    Build, Ctx, DEEP_PARSE_ITEMS, Group, GroupKind, Item, StructureKind, attach, group_by_top_dir,
    last_comp, parent_of, plain_title, slug, stem_key, strip_disc_dirs,
};
use crate::classifier::{classify, clean_title, looks_like_person};
use crate::nameinfo::NameInfo;
use crate::profile::FClass;
use crate::names::split_name;
use crate::types::{Category, Classification, FileEntry, Input};

fn audio_format(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "flac" => "FLAC",
        "mp3" => "MP3",
        "m4a" => "M4A",
        "m4b" => "M4B",
        "aac" => "AAC",
        "ogg" | "oga" => "OGG",
        "opus" => "OPUS",
        "wav" => "WAV",
        "alac" => "ALAC",
        "ape" => "APE",
        "wv" => "WV",
        "aax" | "aaxc" => "AAX",
        "dsf" | "dff" => "DSD",
        _ => return None,
    })
}

// ------------------------------------------------------------------------------------ music

struct Album {
    artist: Option<String>,
    album: String,
    year: Option<u16>,
    format: Option<&'static str>,
}

pub(super) fn build_music(cx: &mut Ctx<'_>, c: &Classification) -> (StructureKind, Vec<Group>) {
    let mut by_dir: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for i in 0..cx.f.len() {
        if cx.pending(i) && cx.f[i].class == FClass::Audio {
            by_dir
                .entry(strip_disc_dirs(cx.f[i].dir).0.to_owned())
                .or_default()
                .push(i);
        }
    }
    let mut builds = Vec::new();
    for (dir, idx) in by_dir {
        let lead = idx
            .iter()
            .copied()
            .max_by_key(|&i| (cx.f[i].size, std::cmp::Reverse(i)))
            .unwrap_or(idx[0]);
        let files = cx.claim(idx.iter().map(|&i| i as u32).collect());
        if files.is_empty() {
            continue;
        }
        builds.push(Build {
            files,
            root: Some(dir.clone()),
            dir,
            stem: String::new(),
            lead: lead as u32,
        });
    }
    attach(cx, &mut builds, |_| true);

    let single = builds.len() == 1;
    let mut items: Vec<Item> = Vec::with_capacity(builds.len());
    for (n, b) in builds.iter().enumerate() {
        let a = album_of(cx, b, c, single, n < DEEP_PARSE_ITEMS);
        let mut files = b.files.clone();
        files.sort_unstable();
        files.dedup();
        let bytes = cx.bytes_of(&files);
        let label = match a.year {
            Some(y) => format!("{} ({y})", a.album),
            None => a.album.clone(),
        };
        let mut sl = slug(&a.album);
        if sl.is_empty() {
            sl = "unknown".to_owned();
        }
        let art = a.artist.as_deref().map(slug).filter(|s| !s.is_empty());
        let identity = format!("album:{}:{sl}", art.as_deref().unwrap_or("-"));
        let mut it = Item::new(label, identity, files, bytes);
        it.creator = a.artist;
        it.year = a.year;
        it.quality = a.format;
        items.push(it);
    }
    items.sort_by_cached_key(|a| (a.creator.as_deref().map(slug), a.year, slug(&a.label)));
    // one group per artist when the torrent holds several
    let mut artists: Vec<String> = items
        .iter()
        .filter_map(|i| i.creator.as_deref().map(slug))
        .collect();
    artists.sort_unstable();
    artists.dedup();
    let groups = if items.is_empty() {
        Vec::new()
    } else if artists.len() >= 2 {
        let mut gs: Vec<Group> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for it in items {
            let key = it.creator.as_deref().map(slug).unwrap_or_default();
            match at.get(&key) {
                Some(&k) => gs[k].items.push(it),
                None => {
                    at.insert(key, gs.len());
                    gs.push(Group {
                        label: it.creator.clone().unwrap_or_else(|| "Albums".to_owned()),
                        kind: GroupKind::Album,
                        items: vec![it],
                    });
                }
            }
        }
        gs
    } else {
        vec![Group {
            label: "Albums".to_owned(),
            kind: GroupKind::Album,
            items,
        }]
    };
    (StructureKind::Music, groups)
}

fn album_of(cx: &Ctx<'_>, b: &Build, c: &Classification, single: bool, deep: bool) -> Album {
    let root = b.root.as_deref().unwrap_or("");
    let lead = &cx.f[b.lead as usize];
    let format = audio_format(&lead.ext).or(c.fields.format);
    if single || root.is_empty() {
        let album = c
            .fields
            .album
            .clone()
            .or_else(|| c.fields.title.clone())
            .or_else(|| c.fields.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if root.is_empty() {
                    "Album".to_owned()
                } else {
                    plain_title(last_comp(root))
                }
            });
        return Album {
            artist: c.fields.artist.clone(),
            album,
            year: c.fields.year,
            format: c.fields.format.or(format),
        };
    }
    let full = last_comp(root);
    // `1998 - First Light`: the classifier needs `Artist - Year - Album` to see a year
    let (lead_year, dirname) = split_lead_year(full);
    let mut out = Album {
        artist: None,
        album: String::new(),
        year: None,
        format,
    };
    if deep {
        let prefix = root.len() + 1;
        let entries: Vec<FileEntry<'_>> = b
            .files
            .iter()
            .filter(|&&i| cx.f[i as usize].class == FClass::Audio)
            .take(64)
            .map(|&i| {
                let f = &cx.f[i as usize];
                FileEntry {
                    path: f.path.get(prefix..).unwrap_or(f.base),
                    size: f.size,
                }
            })
            .collect();
        let r = classify(&Input::new(dirname, &entries));
        if r.category == Category::Music {
            out.artist = r
                .fields
                .artist
                .filter(|a| !a.bytes().all(|b| b.is_ascii_digit()));
            out.album = r.fields.album.or(r.fields.title).unwrap_or_default();
            out.year = r.fields.year;
            out.format = r.fields.format.or(out.format);
        }
    }
    if out.album.is_empty() {
        let ni = NameInfo::parse(dirname, false);
        let t = clean_title(&ni, ni.raw_title());
        out.album = if t.is_empty() {
            plain_title(dirname)
        } else {
            t
        };
        out.year = out.year.or(ni.year);
    }
    out.year = out.year.or(lead_year);
    if out.artist.is_none() {
        let parent = last_comp(parent_of(root));
        if !parent.is_empty() && parent_is_artist(parent) {
            out.artist = Some(plain_title(parent));
        }
    }
    if out.artist.is_none() {
        out.artist = c.fields.artist.clone();
    }
    out
}

/// `1998 - First Light` -> `(Some(1998), "First Light")`; anything else is returned unchanged.
fn split_lead_year(name: &str) -> (Option<u16>, &str) {
    let b = name.as_bytes();
    if b.len() > 7 && b[..4].iter().all(u8::is_ascii_digit) {
        if let Ok(y) = name[..4].parse::<u16>() {
            let rest = name[4..].trim_start_matches([' ', '.', '_']);
            if (1900..=2100).contains(&y) && rest.len() < name.len() - 4 {
                if let Some(r) = rest.strip_prefix(['-', '\u{2013}']) {
                    let r = r.trim_start();
                    if !r.is_empty() {
                        return (Some(y), r);
                    }
                }
            }
        }
    }
    (None, name)
}

/// A parent folder that names an artist: no year, no format/quality words, not a "Discography" folder.
fn parent_is_artist(name: &str) -> bool {
    let ni = NameInfo::parse(name, false);
    ni.year.is_none()
        && !ni.discography
        && ni.music_words == 0
        && !ni.collection_word
        && ni.audio_format.is_none()
        && ni.audio_quality.is_none()
        && !super::is_disc_dir(name)
}

// ------------------------------------------------------------------------------- audiobooks

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum K {
    Dir(String),
    Stem(String, String),
}

pub(super) fn build_audiobooks(
    cx: &mut Ctx<'_>,
    c: &Classification,
) -> (StructureKind, Vec<Group>) {
    let mut keyed: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for i in 0..cx.f.len() {
        if !cx.pending(i) || !matches!(cx.f[i].class, FClass::Audio | FClass::Audiobook) {
            continue;
        }
        let f = &cx.f[i];
        let sdir = strip_disc_dirs(f.dir).0.to_owned();
        let k = if f.class == FClass::Audiobook {
            K::Stem(sdir, stem_key(f.base))
        } else {
            K::Dir(sdir)
        };
        keyed.entry(k).or_default().push(i);
    }
    let mut builds = Vec::new();
    for (k, idx) in keyed {
        let lead = idx
            .iter()
            .copied()
            .max_by_key(|&i| (cx.f[i].size, std::cmp::Reverse(i)))
            .unwrap_or(idx[0]);
        let files = cx.claim(idx.iter().map(|&i| i as u32).collect());
        if files.is_empty() {
            continue;
        }
        let (root, dir, stem) = match k {
            K::Dir(d) => (Some(d.clone()), d, String::new()),
            K::Stem(d, s) => (None, d, s),
        };
        builds.push(Build {
            files,
            root,
            dir,
            stem,
            lead: lead as u32,
        });
    }
    attach(cx, &mut builds, |_| true);

    let single = builds.len() == 1;
    let mut items = Vec::with_capacity(builds.len());
    for (n, b) in builds.iter().enumerate() {
        let lead = &cx.f[b.lead as usize];
        let root = b.root.as_deref().unwrap_or("");
        let (title, mut author) = if single || (b.root.is_some() && root.is_empty()) {
            match &c.fields.title {
                Some(t) if !t.is_empty() => (t.clone(), c.fields.author.clone()),
                _ => book_meta(&name_of(b, lead), lead.base, lead.size),
            }
        } else if n < DEEP_PARSE_ITEMS {
            book_meta(&name_of(b, lead), lead.base, lead.size)
        } else {
            (plain_title(&name_of(b, lead)), None)
        };
        if author.is_none() && !root.is_empty() {
            let parent = last_comp(parent_of(root));
            if !parent.is_empty() && looks_like_person(parent) {
                author = Some(plain_title(parent));
            }
        }
        if author.is_none() {
            author = c.fields.author.clone();
        }
        let mut sl = slug(&title);
        if sl.is_empty() {
            sl = "unknown".to_owned();
        }
        let mut files = b.files.clone();
        files.sort_unstable();
        files.dedup();
        let bytes = cx.bytes_of(&files);
        let mut it = Item::new(title, format!("audiobook:{sl}"), files, bytes);
        it.creator = author;
        it.quality = audio_format(&lead.ext).or(c.fields.format);
        items.push(it);
    }
    items.sort_by(|a, b| (&a.label, &a.identity).cmp(&(&b.label, &b.identity)));
    let groups = if items.is_empty() {
        Vec::new()
    } else {
        group_by_top_dir(cx, items, "Audiobooks", GroupKind::Other)
    };
    (StructureKind::Audiobooks, groups)
}

fn name_of(b: &Build, lead: &super::F<'_>) -> String {
    match b.root.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => last_comp(r).to_owned(),
        None => split_name(lead.base).0.to_owned(),
    }
}
