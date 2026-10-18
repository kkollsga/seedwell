//! TV: episodes grouped by season.

use std::collections::{BTreeMap, HashMap};

use super::{Ctx, F, Group, GroupKind, Item, Quality, StructureKind, plain_title, slug, trunc};
use crate::nameinfo::{NameInfo, TvKind};
use crate::profile::FClass;
use crate::names::split_name;
use crate::types::Classification;

/// What a name says about the episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Ep {
        season: Option<u16>,
        ep: u32,
        end: Option<u32>,
    },
    Date(u16, u8, u8),
}

struct Parsed {
    mark: Option<Mark>,
    quality: Quality,
}

fn parse_name(name: &str) -> Parsed {
    let ni = NameInfo::parse_file(name);
    let quality = (ni.resolution, ni.source);
    let mark = ni.tv.as_ref().and_then(|tv| match tv.kind {
        TvKind::Episode | TvKind::Anime | TvKind::EpisodeWord => tv.episode.map(|ep| Mark::Ep {
            season: if tv.kind == TvKind::Anime {
                None
            } else {
                tv.season
            },
            ep,
            end: tv.episode_end.filter(|&e| e > ep && e - ep < 64),
        }),
        TvKind::Daily => tv.air_date.map(|(y, m, d)| Mark::Date(y, m, d)),
        _ => None,
    });
    Parsed { mark, quality }
}

/// Season named by a folder: `Season 2`, `S02`, `Staffel 2`, `Specials`.
fn season_of_dir(name: &str) -> Option<u16> {
    let l = name.to_ascii_lowercase();
    if matches!(
        l.as_str(),
        "specials" | "special" | "season 0" | "season 00"
    ) {
        return Some(0);
    }
    let ni = NameInfo::parse_file(name);
    let tv = ni.tv?;
    (tv.season_pack && tv.season_end.is_none() && tv.episode.is_none())
        .then_some(tv.season)
        .flatten()
}

/// Season of the nearest season-named folder above a file.
fn folder_season<'a>(memo: &mut HashMap<&'a str, Option<u16>>, dir: &'a str) -> Option<u16> {
    if let Some(&v) = memo.get(dir) {
        return v;
    }
    let mut r = None;
    for comp in dir.rsplit('/').take(4) {
        if let Some(s) = season_of_dir(comp) {
            r = Some(s);
            break;
        }
    }
    memo.insert(dir, r);
    r
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum IdKey {
    Ep(u32, u32),
    Date(u16, u8, u8),
    Named(String),
}

/// `(order, number)`: seasons first, then daily years, specials, the rest.
type GKey = (u8, u32);

struct EpB {
    files: Vec<u32>,
    season: Option<u16>,
    ep: Option<u32>,
    end: Option<u32>,
    date: Option<(u16, u8, u8)>,
    named: Option<String>,
    quality: Quality,
}

/// Resolve a mark to its group and id key.
fn key_of(
    mark: Mark,
    fseason: Option<u16>,
    default_season: Option<u16>,
) -> (GKey, IdKey, Option<u16>) {
    match mark {
        Mark::Ep { season, ep, end } => {
            let s = season.or(fseason).or(default_season).unwrap_or(1);
            let g = if s == 0 { (2, 0) } else { (0, u32::from(s)) };
            (g, IdKey::Ep(ep, end.unwrap_or(ep)), Some(s))
        }
        Mark::Date(y, m, d) => ((1, u32::from(y)), IdKey::Date(y, m, d), None),
    }
}

fn dir_mark(f: &F<'_>) -> Option<Mark> {
    // `Show.S01E02.1080p/video.mkv` and its `Subs/<name>/2_English.srt`
    f.dir
        .rsplit('/')
        .take(3)
        .filter(|c| !c.is_empty())
        .find_map(|c| match parse_name(c).mark {
            m @ Some(Mark::Ep {
                season: Some(_), ..
            }) => m,
            _ => None,
        })
}

pub(super) fn build(cx: &mut Ctx<'_>, c: &Classification) -> (StructureKind, Vec<Group>) {
    let show = c
        .fields
        .show
        .as_deref()
        .or(c.fields.title.as_deref())
        .or(c.fields.name.as_deref())
        .unwrap_or("");
    let show_slug = match slug(show) {
        s if s.is_empty() => "unknown".to_owned(),
        s => s,
    };
    let default_season = c.fields.season.filter(|_| c.fields.season_end.is_none());
    let fq: Quality = (c.fields.resolution, c.fields.source);
    let mut memo: HashMap<&str, Option<u16>> = HashMap::new();
    let mut map: BTreeMap<(GKey, IdKey), EpB> = BTreeMap::new();
    let n = cx.f.len();

    // pass 1: the videos decide which episodes exist
    for i in 0..n {
        if !cx.pending(i) || cx.f[i].class != FClass::Video {
            continue;
        }
        let (dir, base) = (cx.f[i].dir, cx.f[i].base);
        let fs = folder_season(&mut memo, dir);
        let p = parse_name(base);
        let mark = p.mark.or_else(|| dir_mark(&cx.f[i]));
        let (key, builder) = match mark {
            Some(m) => {
                let (g, id, season) = key_of(m, fs, default_season);
                let (ep, end, date) = match (m, &id) {
                    (Mark::Ep { .. }, IdKey::Ep(a, b)) => (Some(*a), (b > a).then_some(*b), None),
                    (Mark::Date(y, mo, d), _) => (None, None, Some((y, mo, d))),
                    _ => (None, None, None),
                };
                (
                    (g, id),
                    EpB {
                        files: Vec::new(),
                        season,
                        ep,
                        end,
                        date,
                        named: None,
                        quality: p.quality,
                    },
                )
            }
            None if fs == Some(0) => {
                let stem = split_name(base).0;
                (
                    ((2, 0), IdKey::Named(slug(stem))),
                    EpB {
                        files: Vec::new(),
                        season: Some(0),
                        ep: None,
                        end: None,
                        date: None,
                        named: Some(plain_title(stem)),
                        quality: p.quality,
                    },
                )
            }
            None => continue,
        };
        let e = map.entry(key).or_insert(builder);
        e.files.push(i as u32);
        if e.quality == (None, None) {
            e.quality = p.quality;
        }
    }

    // fallback: no marked episodes at all, every video is its own un-numbered item
    if map.is_empty() {
        for i in 0..n {
            if cx.pending(i) && cx.f[i].class == FClass::Video {
                let stem = split_name(cx.f[i].base).0;
                let q = parse_name(cx.f[i].base).quality;
                let e = map
                    .entry(((3, 0), IdKey::Named(format!("{i:06}"))))
                    .or_insert(EpB {
                        files: Vec::new(),
                        season: None,
                        ep: None,
                        end: None,
                        date: None,
                        named: Some(plain_title(stem)),
                        quality: q,
                    });
                e.files.push(i as u32);
            }
        }
    }

    // pass 2: companions (subtitles, per-episode images, ...) follow their episode
    if !map.is_empty() {
        for i in 0..n {
            if !cx.pending(i) || cx.f[i].class == FClass::Video {
                continue;
            }
            let (dir, base) = (cx.f[i].dir, cx.f[i].base);
            let fs = folder_season(&mut memo, dir);
            let mark = parse_name(base).mark.or_else(|| dir_mark(&cx.f[i]));
            let key = match mark {
                Some(m) => {
                    let (g, id, _) = key_of(m, fs, default_season);
                    Some((g, id))
                }
                None if fs == Some(0) => {
                    let stem = split_name(base).0;
                    let s = slug(stem);
                    Some(((2, 0), IdKey::Named(s)))
                }
                None => None,
            };
            if let Some(k) = key
                && let Some(e) = map.get_mut(&k) {
                    e.files.push(i as u32);
                }
        }
    }

    // assemble
    let mut groups: Vec<Group> = Vec::new();
    let mut cur: Option<GKey> = None;
    for ((gk, id), mut e) in map {
        e.files = cx.claim(std::mem::take(&mut e.files));
        if e.files.is_empty() {
            continue;
        }
        let bytes = cx.bytes_of(&e.files);
        let (label, identity, alts) = describe(&e, &id, &show_slug);
        let mut it = Item::new(label, identity, e.files, bytes);
        it.alt_identities = alts;
        it.season = e.season;
        it.episode = e.ep;
        it.episode_end = e.end;
        it.air_date = e.date;
        it.quality = e.quality.0.or(fq.0);
        it.source = e.quality.1.or(fq.1);
        if cur != Some(gk) {
            cur = Some(gk);
            let (label, kind) = match gk {
                (0, n) => (format!("Season {n}"), GroupKind::Season(n as u16)),
                (1, y) => (y.to_string(), GroupKind::Other),
                (2, _) => ("Specials".to_owned(), GroupKind::Specials),
                _ => ("Episodes".to_owned(), GroupKind::Other),
            };
            groups.push(Group {
                label,
                kind,
                items: Vec::new(),
            });
        }
        if let Some(g) = groups.last_mut() {
            g.items.push(it);
        }
    }
    (StructureKind::Tv, groups)
}

fn describe(e: &EpB, id: &IdKey, show: &str) -> (String, String, Vec<String>) {
    match id {
        IdKey::Ep(a, b) => {
            let s = e.season.unwrap_or(1);
            let ep_id = |n: u32| format!("tv:{show}:s{s:02}e{n:02}");
            if b > a {
                let label = format!("S{s:02}E{a:02}-E{b:02}");
                let primary = format!("tv:{show}:s{s:02}e{a:02}-e{b:02}");
                let alts = (*a..=*b).map(ep_id).collect();
                (label, primary, alts)
            } else {
                (format!("S{s:02}E{a:02}"), ep_id(*a), Vec::new())
            }
        }
        IdKey::Date(y, m, d) => {
            let t = format!("{y:04}-{m:02}-{d:02}");
            (t.clone(), format!("tv:{show}:{t}"), Vec::new())
        }
        IdKey::Named(sl) => {
            let label = e.named.clone().unwrap_or_else(|| sl.clone());
            let ident = match e.season {
                Some(s) => format!("tv:{show}:s{s:02}:{}", trunc(slug(&label), 80)),
                None => format!("tv:{show}:{}", trunc(slug(&label), 80)),
            };
            (label, ident, Vec::new())
        }
    }
}

pub(super) fn library_identities(hint: Option<&str>, rel_path: &str) -> Vec<String> {
    let show = slug(hint.unwrap_or(""));
    let show = if show.is_empty() {
        "unknown".to_owned()
    } else {
        show
    };
    let (dir, base) = match rel_path.rfind('/') {
        Some(p) => (&rel_path[..p], &rel_path[p + 1..]),
        None => ("", rel_path),
    };
    let p = parse_name(base);
    let mut memo = HashMap::new();
    let fs = folder_season(&mut memo, dir);
    let Some(mark) = p.mark else {
        return Vec::new();
    };
    let (_, id, season) = key_of(mark, fs, None);
    let e = EpB {
        files: Vec::new(),
        season,
        ep: None,
        end: None,
        date: None,
        named: None,
        quality: (None, None),
    };
    let (_, primary, mut alts) = describe(&e, &id, &show);
    let mut out = vec![primary];
    out.append(&mut alts);
    out
}
