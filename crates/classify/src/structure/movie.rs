//! Films and books: titles made of one or more files, grouped by folder or by name.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{
    Build, Ctx, DEEP_PARSE_ITEMS, F, Group, GroupKind, Item, StructureKind, attach,
    group_by_top_dir, last_comp, plain_title, slug, stem_key, strip_disc_dirs, trunc, within,
};
use crate::classifier::{classify, clean_title};
use crate::nameinfo::{NameInfo, split_ext};
use crate::profile::FClass;
use crate::names::split_name;
use crate::types::{Category, Classification, FileEntry, Input};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum K {
    Dir(String),
    Stem(String, String),
}

/// Turn main files into builds: a folder that holds one title owns everything inside it,
/// otherwise a title is `(folder, stem without cd/part marker)`.
pub(super) fn make_builds(cx: &mut Ctx<'_>, mains: &[usize], merge_discs: bool) -> Vec<Build> {
    let mut per_dir: HashMap<String, HashSet<String>> = HashMap::new();
    let mut info: Vec<(String, bool, String)> = Vec::with_capacity(mains.len());
    for &i in mains {
        let f = &cx.f[i];
        let (sdir, forced) = if merge_discs {
            strip_disc_dirs(f.dir)
        } else {
            (f.dir, false)
        };
        let stem = stem_key(f.base);
        per_dir
            .entry(sdir.to_owned())
            .or_default()
            .insert(stem.clone());
        info.push((sdir.to_owned(), forced, stem));
    }
    let mut keyed: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for (&i, (sdir, forced, stem)) in mains.iter().zip(info) {
        let one = per_dir.get(&sdir).is_some_and(|s| s.len() == 1);
        let k = if forced || (!sdir.is_empty() && one) {
            K::Dir(sdir)
        } else {
            K::Stem(sdir, stem)
        };
        keyed.entry(k).or_default().push(i);
    }
    let mut out = Vec::new();
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
            K::Dir(d) => (Some(d.clone()), d, stem_key(cx.f[lead].base)),
            K::Stem(d, s) => (None, d, s),
        };
        out.push(Build {
            files,
            root,
            dir,
            stem,
            lead: lead as u32,
        });
    }
    out
}

const DISC_DIRS: [&str; 3] = ["bdmv", "video_ts", "audio_ts"];

/// Roots of disc structures (`Movie/BDMV/...`, `VIDEO_TS/...`), longest first.
fn disc_roots(cx: &Ctx<'_>) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for (i, f) in cx.f.iter().enumerate() {
        if !cx.pending(i) {
            continue;
        }
        let comps: Vec<&str> = f.dir.split('/').collect();
        if let Some(p) = comps
            .iter()
            .position(|c| DISC_DIRS.contains(&c.to_ascii_lowercase().as_str()))
        {
            let root = comps[..p].join("/");
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
    roots
}

fn is_extra_video_name(base: &str) -> bool {
    let l = base.to_ascii_lowercase();
    let words: Vec<&str> = l.split(|c: char| !c.is_alphanumeric()).collect();
    words.iter().any(|w| {
        matches!(
            *w,
            "trailer"
                | "featurette"
                | "featurettes"
                | "teaser"
                | "bloopers"
                | "interview"
                | "interviews"
        )
    }) || l.contains("behind the scenes")
        || l.contains("deleted scene")
        || l.contains("making of")
        || l.contains("making.of")
}

pub(super) fn build_movies(cx: &mut Ctx<'_>, c: &Classification) -> (StructureKind, Vec<Group>) {
    let mut builds: Vec<Build> = Vec::new();
    // disc folders: everything inside is the film
    for root in disc_roots(cx) {
        let idx: Vec<u32> = (0..cx.f.len())
            .filter(|&i| cx.pending(i) && within(cx.f[i].dir, &root))
            .map(|i| i as u32)
            .collect();
        let lead = idx
            .iter()
            .copied()
            .max_by_key(|&i| (cx.f[i as usize].size, std::cmp::Reverse(i)))
            .unwrap_or(0);
        let files = cx.claim(idx);
        if !files.is_empty() {
            builds.push(Build {
                files,
                root: Some(root.clone()),
                dir: root,
                stem: String::new(),
                lead,
            });
        }
    }
    // main videos
    let mut mains: Vec<usize> = (0..cx.f.len())
        .filter(|&i| cx.pending(i) && matches!(cx.f[i].class, FClass::Video | FClass::Disc))
        .collect();
    if mains.len() >= 2 {
        let largest = mains.iter().map(|&i| cx.f[i].size).max().unwrap_or(0);
        mains.retain(|&i| {
            let extra =
                cx.f[i].size.saturating_mul(2) < largest && is_extra_video_name(cx.f[i].base);
            if extra {
                cx.st[i] = super::ST_EXTRA;
            }
            !extra
        });
    }
    builds.extend(make_builds(cx, &mains, true));
    attach(cx, &mut builds, |_| true);

    let single = builds.len() == 1;
    let mut items: Vec<Item> = builds
        .iter()
        .map(|b| movie_item(cx, b, c, single))
        .collect();
    disambiguate(cx, &builds, &mut items);
    items.sort_by(|a, b| (&a.identity, &a.label).cmp(&(&b.identity, &b.label)));
    let kind = if items.len() == 1 {
        StructureKind::Single
    } else {
        StructureKind::MovieCollection
    };
    let groups = if items.is_empty() {
        Vec::new()
    } else {
        group_by_top_dir(cx, items, "Movies", GroupKind::Collection)
    };
    (kind, groups)
}

/// Items that came out with the same identity (several episodes read as films of one title, two
/// qualities of one film) get the part of their file name that tells them apart ("Fallen Kingdom —
/// S01E02"), so that no two rows look identical and library matching keeps them apart.
/// `items` is in the order of `builds`.
fn disambiguate(cx: &Ctx<'_>, builds: &[Build], items: &mut [Item]) {
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    for (n, it) in items.iter().enumerate() {
        by_id.entry(it.identity.clone()).or_default().push(n);
    }
    for idx in by_id.into_values().filter(|v| v.len() > 1) {
        let words = |n: usize| -> Vec<String> {
            let lead = &cx.f[builds[n].lead as usize];
            split_name(lead.base)
                .0
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let all: Vec<Vec<String>> = idx.iter().map(|&n| words(n)).collect();
        let common = |w: &str| {
            all.iter()
                .all(|ws| ws.iter().any(|x| x.eq_ignore_ascii_case(w)))
        };
        for (&n, ws) in idx.iter().zip(&all) {
            let mut rest: Vec<&str> = ws
                .iter()
                .map(String::as_str)
                .filter(|w| !common(w))
                .collect();
            if rest.is_empty() {
                // Same file name in different folders: the folder tells them apart.
                let dir = last_comp(cx.f[builds[n].lead as usize].dir);
                if dir.is_empty() {
                    continue;
                }
                rest = vec![dir];
            }
            let tag = rest.join(" ");
            let it = &mut items[n];
            it.label = trunc(format!("{} — {tag}", it.label), super::MAX_LABEL);
            it.identity = format!("{}:{}", it.identity, slug(&tag));
        }
    }
}

fn movie_item(cx: &Ctx<'_>, b: &Build, c: &Classification, single: bool) -> Item {
    let lead = &cx.f[b.lead as usize];
    let mut names: Vec<NameInfo> = Vec::new();
    if let Some(r) = b.root.as_deref().filter(|r| !r.is_empty()) {
        names.push(NameInfo::parse(last_comp(r), false));
    }
    names.push(NameInfo::parse(lead.base, true));
    let title = names
        .iter()
        .map(|ni| clean_title(ni, ni.raw_title()))
        .find(|t| !t.is_empty());
    let year = names.iter().find_map(|ni| ni.year);
    let res = names.iter().find_map(|ni| ni.resolution);
    let src = names.iter().find_map(|ni| ni.source);
    let (title, year) = if single {
        (
            c.fields.title.clone().filter(|t| !t.is_empty()).or(title),
            c.fields.year.or(year),
        )
    } else {
        (title, year)
    };
    let title = title.unwrap_or_else(|| plain_title(split_name(lead.base).0));
    let label = match year {
        Some(y) => format!("{title} ({y})"),
        None => title.clone(),
    };
    let mut sl = slug(&title);
    if sl.is_empty() {
        sl = "unknown".to_owned();
    }
    let identity = match year {
        Some(y) => format!("movie:{sl}:{y}"),
        None => format!("movie:{sl}"),
    };
    let mut files = b.files.clone();
    files.sort_unstable();
    files.dedup();
    let bytes = cx.bytes_of(&files);
    let mut it = Item::new(label, identity, files, bytes);
    it.year = year;
    it.quality = res.or(c.fields.resolution);
    it.source = src.or(c.fields.source);
    it
}

// ----------------------------------------------------------------------------------- books

/// Title, author and format of one book name, using the full classifier when it agrees
/// that this is a book (`Author - Title`, `Title (Author)`, ...).
pub(super) fn book_meta(name: &str, file: &str, size: u64) -> (String, Option<String>) {
    let files = [FileEntry { path: file, size }];
    let c = classify(&Input::new(name, &files));
    if matches!(c.category, Category::Book | Category::Audiobook) {
        if let Some(t) = c.fields.title.filter(|t| !t.is_empty()) {
            return (t, c.fields.author);
        }
    }
    let ni = NameInfo::parse(name, true);
    let t = clean_title(&ni, ni.raw_title());
    (if t.is_empty() { plain_title(name) } else { t }, None)
}

fn book_format(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "epub" => "EPUB",
        "pdf" => "PDF",
        "mobi" => "MOBI",
        "azw" | "azw3" | "azw4" => "AZW3",
        "cbz" => "CBZ",
        "cbr" => "CBR",
        "cb7" => "CB7",
        "djvu" | "djv" => "DJVU",
        "fb2" => "FB2",
        "kfx" => "KFX",
        _ => return None,
    })
}

pub(super) fn build_books(cx: &mut Ctx<'_>, c: &Classification) -> (StructureKind, Vec<Group>) {
    let mains: Vec<usize> = (0..cx.f.len())
        .filter(|&i| cx.pending(i) && matches!(cx.f[i].class, FClass::Book | FClass::Pdf))
        .collect();
    let mut builds = make_builds(cx, &mains, false);
    attach(cx, &mut builds, |f: &F<'_>| {
        !matches!(f.class, FClass::Book | FClass::Pdf)
    });
    let single = builds.len() == 1;
    let mut items = Vec::with_capacity(builds.len());
    for (n, b) in builds.iter().enumerate() {
        let lead = &cx.f[b.lead as usize];
        let (title, author) = if single {
            match (&c.fields.title, &c.fields.author) {
                (Some(t), a) if !t.is_empty() => (t.clone(), a.clone()),
                _ => book_meta(&book_name(b, lead), lead.base, lead.size),
            }
        } else if n < DEEP_PARSE_ITEMS {
            book_meta(&book_name(b, lead), lead.base, lead.size)
        } else {
            (plain_title(split_name(lead.base).0), None)
        };
        let mut sl = slug(&title);
        if sl.is_empty() {
            sl = "unknown".to_owned();
        }
        let mut files = b.files.clone();
        files.sort_unstable();
        files.dedup();
        let bytes = cx.bytes_of(&files);
        let mut it = Item::new(title, format!("book:{sl}"), files, bytes);
        it.creator = author;
        it.quality = book_format(&lead.ext).or(c.fields.format);
        items.push(it);
    }
    items.sort_by(|a, b| (&a.label, &a.identity).cmp(&(&b.label, &b.identity)));
    let groups = if items.is_empty() {
        Vec::new()
    } else {
        group_by_top_dir(cx, items, "Books", GroupKind::Other)
    };
    (StructureKind::Books, groups)
}

fn book_name(b: &Build, lead: &F<'_>) -> String {
    match b.root.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => last_comp(r).to_owned(),
        None => split_ext(lead.base).0.to_owned(),
    }
}

// ----------------------------------------------------------------------------- identities

pub(super) fn library_identities(rel_path: &str) -> Vec<String> {
    let (dir, base) = match rel_path.rfind('/') {
        Some(p) => (&rel_path[..p], &rel_path[p + 1..]),
        None => ("", rel_path),
    };
    let mut names = vec![NameInfo::parse(base, true)];
    if !dir.is_empty() {
        names.insert(0, NameInfo::parse(last_comp(dir), false));
    }
    let title = names
        .iter()
        .map(|ni| clean_title(ni, ni.raw_title()))
        .find(|t| !t.is_empty());
    let year = names.iter().find_map(|ni| ni.year);
    let Some(title) = title else {
        return Vec::new();
    };
    let sl = slug(&title);
    if sl.is_empty() {
        return Vec::new();
    }
    vec![match year {
        Some(y) => format!("movie:{sl}:{y}"),
        None => format!("movie:{sl}"),
    }]
}

pub(super) fn library_book_identities(category: Category, rel_path: &str) -> Vec<String> {
    let (dir, base) = match rel_path.rfind('/') {
        Some(p) => (&rel_path[..p], &rel_path[p + 1..]),
        None => ("", rel_path),
    };
    let (prefix, name) = if category == Category::Audiobook && !dir.is_empty() {
        ("audiobook", last_comp(dir).to_owned())
    } else {
        (
            if category == Category::Audiobook {
                "audiobook"
            } else {
                "book"
            },
            split_ext(base).0.to_owned(),
        )
    };
    let (title, _) = book_meta(&name, base, 0);
    let sl = slug(&title);
    if sl.is_empty() {
        Vec::new()
    } else {
        vec![format!("{prefix}:{}", trunc(sl, 120))]
    }
}
