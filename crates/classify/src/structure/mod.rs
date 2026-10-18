//! Smart add: group a torrent's files into the *items* a person chooses between (SPEC §7A.6).
//!
//! Pure and allocation-light: nothing is cached per torrent, the engine calls
//! [`structure`] when a chooser opens (or when it decides whether one is needed) and drops
//! the result. The grouping is driven by `Classification::category`:
//!
//! | Category | Items | Groups |
//! |---|---|---|
//! | Tv | episodes (video + its subtitles) | season, `Specials`, year (daily shows) |
//! | Movie | films (all files of one film, `cd1`/`cd2` stacks, disc folders) | one, or one per top folder |
//! | Music | albums (disc folders merged) | one, or one per artist |
//! | Book | titles (all formats of one title) | one, or one per top folder (author) |
//! | Audiobook | titles (a folder of chapters, or an `.m4b`) | one, or one per top folder |
//! | Software | installers, `.app` bundles, archives (with their build variant) | one per operating system |
//! | Other | none (all files are `unassigned`) | none |
//!
//! Every file index below the cap appears **exactly once** across `groups[..].items[..].file_indices`,
//! `extras` and `unassigned`.

mod audio;
mod movie;
mod software;
mod tv;
mod variant;

use std::collections::HashMap;

use crate::profile::{FClass, class_of_ext, in_dir, is_extras_dir};
use crate::names::split_name;
use crate::types::{Category, Classification, FileEntry};
pub use variant::{Cpu, Os, PkgKind, Variant};

/// Files beyond this index are not analysed; they are listed in [`Structure::unassigned`].
pub const MAX_FILES: usize = 20_000;
/// Labels are cut to this many bytes (on a char boundary).
const MAX_LABEL: usize = 120;
/// Number of items that get the full (per-item classifier) title parse; the rest get the cheap one.
const DEEP_PARSE_ITEMS: usize = 400;

/// What kind of torrent the grouping describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructureKind {
    /// One film: nothing to choose.
    Single,
    /// Episodes by season.
    Tv,
    /// Two or more films.
    MovieCollection,
    /// Albums.
    Music,
    /// E-book titles.
    Books,
    /// Audiobook titles.
    Audiobooks,
    /// Installers and builds.
    Software,
    /// Nothing was recognised as an item; every file is in `unassigned`.
    Flat,
}

/// What a [`Group`] stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// A TV season (number >= 1).
    Season(u16),
    /// Season 0, or a `Specials` folder.
    Specials,
    /// A set of films, or of books.
    Collection,
    /// A set of albums (one artist, or all).
    Album,
    /// Software builds for one operating system.
    Platform(Os),
    /// Anything else (daily-show years, un-numbered episodes, audiobooks).
    Other,
}

/// One selectable thing: an episode, a film, an album, a title, a build.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Display text: `S03E01`, `Night of the Living Dead (1968)`, `Album (2007)`, `App-1.2-arm64.dmg`.
    pub label: String,
    /// Stable normalised key for library matching (grammar in the README).
    pub identity: String,
    /// Further identities this item also stands for: the single-episode keys of a double episode.
    pub alt_identities: Vec<String>,
    /// Every file of the item (video + subtitles, tracks + cover, ...), ascending.
    pub file_indices: Vec<u32>,
    /// Sum of the sizes of `file_indices` (saturating).
    pub bytes: u64,
    /// Artist (albums) or author (books, audiobooks) when known.
    pub creator: Option<String>,
    /// Resolution for video (`1080p`), format for music/books (`FLAC`).
    pub quality: Option<&'static str>,
    /// Release source for video (`WEB-DL`, `BluRay`).
    pub source: Option<&'static str>,
    /// TV season (`0` = specials).
    pub season: Option<u16>,
    /// TV episode (first of a double episode).
    pub episode: Option<u32>,
    /// Last episode of a double episode.
    pub episode_end: Option<u32>,
    /// Air date of a daily-show episode.
    pub air_date: Option<(u16, u8, u8)>,
    /// Release year (films, albums).
    pub year: Option<u16>,
    /// Software build.
    pub variant: Option<Variant>,
}

impl Item {
    pub(crate) fn new(label: String, identity: String, files: Vec<u32>, bytes: u64) -> Item {
        Item {
            label: trunc(label, MAX_LABEL),
            identity,
            alt_identities: Vec::new(),
            file_indices: files,
            bytes,
            creator: None,
            quality: None,
            source: None,
            season: None,
            episode: None,
            episode_end: None,
            air_date: None,
            year: None,
            variant: None,
        }
    }

    /// `1080p WEB-DL`, `FLAC`, or `None`.
    pub fn quality_label(&self) -> Option<String> {
        match (self.quality, self.source) {
            (Some(q), Some(s)) => Some(format!("{q} {s}")),
            (Some(q), None) => Some(q.to_owned()),
            (None, Some(s)) => Some(s.to_owned()),
            (None, None) => None,
        }
    }

    /// Number of episodes this item covers (1 for a normal episode, 2 for `S01E01E02`).
    pub fn episode_count(&self) -> u32 {
        match (self.episode, self.episode_end) {
            (Some(a), Some(b)) if b > a => (b - a + 1).min(64),
            _ => 1,
        }
    }
}

/// A labelled set of items (a season, an artist, an operating system).
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// `Season 3`, `Specials`, `macOS`, an artist, `Movies`.
    pub label: String,
    /// What the group stands for.
    pub kind: GroupKind,
    /// Items in display order.
    pub items: Vec<Item>,
}

impl Group {
    /// Total bytes of the group's items.
    pub fn bytes(&self) -> u64 {
        self.items
            .iter()
            .fold(0u64, |a, i| a.saturating_add(i.bytes))
    }
}

/// Best macOS build so far for a product: `(rank, position)`.
type Pick = ((u8, u8), (usize, usize));

/// Where a file ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// `groups[g].items[i]`.
    Item(usize, usize),
    /// In [`Structure::extras`].
    Extra,
    /// In [`Structure::unassigned`].
    Unassigned,
}

/// A torrent's files grouped into choosable items.
#[derive(Clone, Debug, PartialEq)]
pub struct Structure {
    /// What the grouping describes.
    pub kind: StructureKind,
    /// Items by group, in display order.
    pub groups: Vec<Group>,
    /// Junk, samples, featurettes, screenshots: shown under **Extras**, unticked by default.
    pub extras: Vec<u32>,
    /// Files that belong to no item (a poster next to several films, an unnumbered video in a
    /// season pack). Small and usually harmless; the engine keeps them wanted when anything
    /// is chosen, and they are visible in the raw file list.
    pub unassigned: Vec<u32>,
    /// More than one selectable item by the SPEC §7A.6 "Several items" rule: TV >= 2 episodes,
    /// film collection >= 2, discography >= 2 albums, book or audiobook bundle >= 3 titles.
    /// Software variants are a separate trigger ([`Structure::has_platform_variants`]).
    pub multi: bool,
    /// The torrent had more than [`MAX_FILES`] files; the surplus is in `unassigned`.
    pub truncated: bool,
}

impl Structure {
    /// Number of items over all groups.
    pub fn item_count(&self) -> usize {
        self.groups.iter().map(|g| g.items.len()).sum()
    }

    /// Bytes of all items (excluding extras and unassigned files).
    pub fn total_bytes(&self) -> u64 {
        self.groups
            .iter()
            .fold(0u64, |a, g| a.saturating_add(g.bytes()))
    }

    /// Distinct season numbers present, ascending (`0` = specials).
    pub fn seasons(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self
            .groups
            .iter()
            .flat_map(|g| g.items.iter().filter_map(|i| i.season))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// All items with their `(group, item)` position.
    pub fn items(&self) -> impl Iterator<Item = ((usize, usize), &Item)> {
        self.groups.iter().enumerate().flat_map(|(g, grp)| {
            grp.items
                .iter()
                .enumerate()
                .map(move |(i, it)| ((g, i), it))
        })
    }

    /// Where file `idx` is. Linear in the number of files; call it for a few files, not all.
    pub fn place_of_file(&self, idx: u32) -> Option<Place> {
        for ((g, i), it) in self.items() {
            if it.file_indices.binary_search(&idx).is_ok() {
                return Some(Place::Item(g, i));
            }
        }
        if self.extras.contains(&idx) {
            return Some(Place::Extra);
        }
        self.unassigned.contains(&idx).then_some(Place::Unassigned)
    }

    /// `(group, item)` that holds file `idx`, if any.
    pub fn item_of_file(&self, idx: u32) -> Option<(usize, usize)> {
        match self.place_of_file(idx) {
            Some(Place::Item(g, i)) => Some((g, i)),
            _ => None,
        }
    }

    /// Software with two or more builds for different operating system / CPU combinations.
    /// Items that do not name an operating system (a bare `.zip`) are not counted.
    pub fn has_platform_variants(&self) -> bool {
        if self.kind != StructureKind::Software {
            return false;
        }
        let mut seen: Vec<(Os, Cpu)> = Vec::new();
        for (_, it) in self.items() {
            if let Some(v) = it.variant.filter(|v| v.os != Os::Any) {
                if !seen.contains(&(v.os, v.cpu)) {
                    seen.push((v.os, v.cpu));
                }
            }
        }
        seen.len() >= 2
    }

    /// The build(s) to preselect for this Mac (Apple silicon, macOS 15+): for each product the
    /// best macOS item (arm64 over universal over unspecified CPU; `.dmg` over `.pkg` over
    /// archives). Empty when no item targets macOS. Sorted by position.
    pub fn this_mac_items(&self) -> Vec<(usize, usize)> {
        let mut best: HashMap<String, Pick> = HashMap::new();
        for (pos, it) in self.items() {
            let Some(v) = it.variant.filter(Variant::is_this_mac) else {
                continue;
            };
            let product = it
                .identity
                .rsplit_once(':')
                .map_or(it.identity.as_str(), |(p, _)| p)
                .to_owned();
            let rank = (v.cpu.mac_rank(), v.kind.rank());
            let b = best.entry(product).or_insert((rank, pos));
            if rank < b.0 {
                *b = (rank, pos);
            }
        }
        let mut v: Vec<(usize, usize)> = best.into_values().map(|b| b.1).collect();
        v.sort_unstable();
        v
    }
}

// ---------------------------------------------------------------------------------- driver

/// Group `files` into choosable items. `c` must be the classification of the same torrent
/// (`classify` over the same file list: its junk indexes and category drive the grouping).
/// Pure; at most [`MAX_FILES`] files are analysed.
pub fn structure(c: &Classification, files: &[FileEntry<'_>]) -> Structure {
    let n = files.len().min(MAX_FILES);
    let mut cx = Ctx::new(&files[..n]);
    for j in &c.junk_files {
        if j.index < n {
            cx.st[j.index] = ST_EXTRA;
        }
    }
    if matches!(c.category, Category::Tv | Category::Movie) {
        for i in 0..n {
            if cx.st[i] == ST_PENDING && in_dir(cx.f[i].path, is_extras_dir) {
                cx.st[i] = ST_EXTRA;
            }
        }
    }
    let (kind, groups) = match c.category {
        Category::Tv => tv::build(&mut cx, c),
        Category::Movie => movie::build_movies(&mut cx, c),
        Category::Music => audio::build_music(&mut cx, c),
        Category::Book => movie::build_books(&mut cx, c),
        Category::Audiobook => audio::build_audiobooks(&mut cx, c),
        Category::Software => software::build(&mut cx, c),
        Category::Other | Category::Game => (StructureKind::Flat, Vec::new()),
    };
    let item_total: usize = groups.iter().map(|g| g.items.len()).sum();
    let multi = match kind {
        StructureKind::Tv | StructureKind::MovieCollection | StructureKind::Music => {
            item_total >= 2
        }
        StructureKind::Books | StructureKind::Audiobooks => item_total >= 3,
        _ => false,
    };
    let kind = if item_total == 0 && kind != StructureKind::Software {
        StructureKind::Flat
    } else {
        kind
    };
    let mut extras = Vec::new();
    let mut unassigned = Vec::new();
    for (i, s) in cx.st.iter().enumerate() {
        match *s {
            ST_EXTRA => extras.push(i as u32),
            ST_PENDING => unassigned.push(i as u32),
            _ => {}
        }
    }
    unassigned.extend((n..files.len()).map(|i| i as u32));
    Structure {
        kind,
        groups,
        extras,
        unassigned,
        multi,
        truncated: files.len() > n,
    }
}

// ------------------------------------------------------------------------------ identities

/// Identity keys of one *existing* file in a library folder, for matching against
/// [`Item::identity`] / [`Item::alt_identities`] ("You have some", SPEC §7A.6).
///
/// `hint` is the show name for `Tv` (the library folder's show, or the torrent's `fields.show`);
/// `rel_path` is any path whose last components carry the naming (`Season 03/Fallen Kingdom - S03E01.mkv`,
/// or just the file name). Returns the primary key first, then single-episode keys of a
/// double episode. Empty when nothing can be said. `Movie`, `Tv`, `Book` and `Audiobook`
/// are supported; other categories return an empty list.
pub fn library_identities(category: Category, hint: Option<&str>, rel_path: &str) -> Vec<String> {
    match category {
        Category::Tv => tv::library_identities(hint, rel_path),
        Category::Movie => movie::library_identities(rel_path),
        Category::Book | Category::Audiobook => movie::library_book_identities(category, rel_path),
        _ => Vec::new(),
    }
}

// ------------------------------------------------------------------------------- internals

pub(crate) const ST_PENDING: u8 = 0;
pub(crate) const ST_EXTRA: u8 = 1;
pub(crate) const ST_TAKEN: u8 = 2;

/// Per-file facts, computed once per call.
pub(crate) struct F<'a> {
    pub path: &'a str,
    pub dir: &'a str,
    pub base: &'a str,
    pub ext: String,
    pub class: FClass,
    pub size: u64,
}

/// Working state: file facts plus who owns each file.
pub(crate) struct Ctx<'a> {
    pub f: Vec<F<'a>>,
    pub st: Vec<u8>,
}

impl<'a> Ctx<'a> {
    fn new(files: &[FileEntry<'a>]) -> Ctx<'a> {
        let f = files
            .iter()
            .map(|e| {
                let (dir, base) = match e.path.rfind('/') {
                    Some(p) => (&e.path[..p], &e.path[p + 1..]),
                    None => ("", e.path),
                };
                let ext = ext_of(base);
                let class = if ext.is_empty() {
                    FClass::Unknown
                } else {
                    class_of_ext(&ext)
                };
                F {
                    path: e.path,
                    dir,
                    base,
                    ext,
                    class,
                    size: e.size,
                }
            })
            .collect::<Vec<_>>();
        let st = vec![ST_PENDING; f.len()];
        Ctx { f, st }
    }

    pub fn pending(&self, i: usize) -> bool {
        self.st[i] == ST_PENDING
    }

    pub fn take(&mut self, i: usize) {
        debug_assert_eq!(self.st[i], ST_PENDING);
        self.st[i] = ST_TAKEN;
    }

    pub fn bytes_of(&self, files: &[u32]) -> u64 {
        files
            .iter()
            .fold(0u64, |a, &i| a.saturating_add(self.f[i as usize].size))
    }

    /// Claim `idx` (must be pending) for an item; returns the sorted list to store.
    pub fn claim(&mut self, mut idx: Vec<u32>) -> Vec<u32> {
        idx.retain(|&i| self.st[i as usize] == ST_PENDING);
        idx.sort_unstable();
        idx.dedup();
        for &i in &idx {
            self.st[i as usize] = ST_TAKEN;
        }
        idx
    }
}

/// An item under construction: its main files and where companion files may attach.
pub(crate) struct Build {
    /// Main + attached files (indices, not yet sorted).
    pub files: Vec<u32>,
    /// `Some(dir)`: every pending file inside `dir` (deepest wins) belongs here.
    pub root: Option<String>,
    /// Directory of the first main file.
    pub dir: String,
    /// Normalised stem of the main file (stem-keyed items).
    pub stem: String,
    /// Largest main file (names and quality come from it).
    pub lead: u32,
}

/// `dir`, its parent, ... up to and including the root `""`.
fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    let mut cur = Some(dir);
    std::iter::from_fn(move || {
        let d = cur?;
        cur = if d.is_empty() {
            None
        } else {
            Some(parent_of(d))
        };
        Some(d)
    })
}

/// Lookup tables over the builds, so companions attach in O(depth) instead of O(items).
struct Index<'a> {
    /// directory-owning items by root
    roots: HashMap<&'a str, usize>,
    /// stem-keyed items by directory
    by_dir: HashMap<&'a str, Vec<usize>>,
    /// folder -> (number of items at or below it, the last such item)
    under: HashMap<&'a str, (u32, usize)>,
}

impl<'a> Index<'a> {
    fn new(items: &'a [Build]) -> Index<'a> {
        let mut ix = Index {
            roots: HashMap::new(),
            by_dir: HashMap::new(),
            under: HashMap::new(),
        };
        for (k, b) in items.iter().enumerate() {
            let anchor = match &b.root {
                Some(r) => {
                    ix.roots.entry(r.as_str()).or_insert(k);
                    r.as_str()
                }
                None => {
                    if !b.stem.is_empty() {
                        ix.by_dir.entry(b.dir.as_str()).or_default().push(k);
                    }
                    b.dir.as_str()
                }
            };
            for d in ancestors(anchor) {
                let e = ix.under.entry(d).or_insert((0, k));
                e.0 += 1;
                e.1 = k;
            }
        }
        ix
    }
}

/// Attach pending files accepted by `eligible` to the item they belong with (companions:
/// subtitles, covers, cue sheets, posters).
pub(crate) fn attach(cx: &mut Ctx<'_>, items: &mut [Build], eligible: impl Fn(&F<'_>) -> bool) {
    if items.is_empty() {
        return;
    }
    let single = items.len() == 1;
    let mut targets: Vec<(usize, usize)> = Vec::new();
    {
        let ix = Index::new(items);
        for i in 0..cx.f.len() {
            if !cx.pending(i) || !eligible(&cx.f[i]) {
                continue;
            }
            let f = &cx.f[i];
            let stem_raw = split_name(f.base).0.to_ascii_lowercase();
            if let Some(t) = pick_target(items, &ix, single, f.dir, &stem_raw) {
                targets.push((i, t));
            }
        }
    }
    for (i, t) in targets {
        items[t].files.push(i as u32);
        cx.take(i);
    }
}

fn pick_target(
    items: &[Build],
    ix: &Index<'_>,
    single: bool,
    dir: &str,
    stem_raw: &str,
) -> Option<usize> {
    // a) the deepest directory-owning item that contains the file
    if let Some(&k) = ancestors(dir).find_map(|d| ix.roots.get(d)) {
        return Some(k);
    }
    // the folder the file is in, skipping a `Subs`-like folder
    let (base_dir, last) = {
        let last = last_comp(dir);
        let l = last.to_ascii_lowercase();
        if matches!(
            l.as_str(),
            "subs"
                | "sub"
                | "subtitles"
                | "subtitle"
                | "cover"
                | "covers"
                | "artwork"
                | "scans"
                | "extras"
        ) {
            (parent_of(dir), "")
        } else {
            (dir, last)
        }
    };
    // b1) stem match with an item of this folder or of a folder above it
    let last_norm = strip_stack(&last.to_ascii_lowercase()).to_owned();
    let stem = strip_stack(stem_raw);
    for d in ancestors(base_dir) {
        let Some(cands) = ix.by_dir.get(d) else {
            continue;
        };
        let mut cand: Option<(usize, usize)> = None;
        for &k in cands {
            let bs = &items[k].stem;
            let m = |s: &str| {
                !s.is_empty()
                    && (s == bs || s.starts_with(bs.as_str()) || bs.starts_with(s) && s.len() >= 8)
            };
            if (m(stem) || m(&last_norm)) && cand.is_none_or(|(_, l)| bs.len() > l) {
                cand = Some((k, bs.len()));
            }
        }
        if let Some((k, _)) = cand {
            return Some(k);
        }
    }
    // b2) the only item in this folder (nested items count)
    if let Some(&(1, k)) = ix.under.get(base_dir) {
        return Some(k);
    }
    // c) a torrent with a single item owns everything
    single.then_some(0)
}

/// `dir` equals `root` or lies below it. `root == ""` contains everything.
pub(crate) fn within(dir: &str, root: &str) -> bool {
    root.is_empty()
        || dir == root
        || (dir.len() > root.len() && dir.as_bytes()[root.len()] == b'/' && dir.starts_with(root))
}

pub(crate) fn parent_of(dir: &str) -> &str {
    dir.rfind('/').map_or("", |p| &dir[..p])
}

pub(crate) fn last_comp(dir: &str) -> &str {
    dir.rsplit('/').next().unwrap_or("")
}

/// Lower-case extension without dot (empty when none or longer than 10 bytes).
pub(crate) fn ext_of(base: &str) -> String {
    match base.rfind('.') {
        Some(d) if d > 0 && d + 1 < base.len() && base.len() - d - 1 <= 10 => {
            base[d + 1..].to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

/// Lower-case alphanumeric key: separators and punctuation become one `-`; `'` vanishes, `&` is `and`.
pub(crate) fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(160));
    let mut pending_sep = false;
    for ch in s.chars() {
        if out.len() > 160 {
            break;
        }
        if ch == '\'' || ch == '\u{2019}' {
            continue;
        }
        if ch == '&' {
            if !out.is_empty() {
                out.push('-');
            }
            out.push_str("and");
            pending_sep = true;
            continue;
        }
        if ch.is_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_sep = true;
        }
    }
    out
}

/// Cut at `max` bytes on a char boundary.
pub(crate) fn trunc(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut e = max;
        while e > 0 && !s.is_char_boundary(e) {
            e -= 1;
        }
        s.truncate(e);
    }
    s
}

/// Lower-case stem with disc/part/cd stack markers removed (`movie.cd1` -> `movie`).
pub(crate) fn stem_key(base: &str) -> String {
    let stem = split_name(base).0.to_ascii_lowercase();
    // `.partNN.rar`, `.r00`: the extension was removed by split_name, the part marker is a suffix
    strip_stack(&stem).to_owned()
}

/// Remove a trailing `cd1`, `disc 2`, `part3`, `pt.1`, `dvd2` marker (input lower-case).
pub(crate) fn strip_stack(s: &str) -> &str {
    let t = s.trim_end_matches(|c: char| !c.is_alphanumeric());
    let d = t.trim_end_matches(|c: char| c.is_ascii_digit());
    if d.len() == t.len() || d.is_empty() {
        return t;
    }
    let w = d.trim_end_matches([' ', '.', '_', '-']);
    for m in ["cd", "disc", "disk", "part", "pt", "dvd"] {
        if let Some(r) = w.strip_suffix(m) {
            if r.is_empty() || !r.ends_with(|c: char| c.is_alphanumeric()) {
                let r = r.trim_end_matches([' ', '.', '_', '-']);
                return if r.is_empty() { t } else { r };
            }
        }
    }
    t
}

/// A `CD1` / `Disc 2` / `Disk 01` folder name.
pub(crate) fn is_disc_dir(name: &str) -> bool {
    let l: String = name
        .chars()
        .filter(|c| !matches!(c, ' ' | '.' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    for p in ["cd", "disc", "disk", "dvd"] {
        if let Some(r) = l.strip_prefix(p) {
            if !r.is_empty() && r.len() <= 3 && r.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Strip trailing disc folders from `dir`; the flag says whether any was stripped.
pub(crate) fn strip_disc_dirs(mut dir: &str) -> (&str, bool) {
    let mut any = false;
    while !dir.is_empty() && is_disc_dir(last_comp(dir)) {
        dir = parent_of(dir);
        any = true;
    }
    (dir, any)
}

/// Quality of a name: `(resolution, source)`.
pub(crate) type Quality = (Option<&'static str>, Option<&'static str>);

/// Group items by their first path component when that separates them into folders that hold
/// several items (authors, franchises); otherwise one group.
pub(crate) fn group_by_top_dir(
    cx: &Ctx<'_>,
    items: Vec<Item>,
    one_label: &str,
    kind: GroupKind,
) -> Vec<Group> {
    let top = |it: &Item| -> String {
        it.file_indices
            .first()
            .and_then(|&i| cx.f[i as usize].path.split_once('/'))
            .map(|(t, _)| t.to_owned())
            .unwrap_or_default()
    };
    let tops: Vec<String> = items.iter().map(top).collect();
    let mut count: HashMap<&str, u32> = HashMap::new();
    for t in tops.iter().filter(|t| !t.is_empty()) {
        *count.entry(t.as_str()).or_default() += 1;
    }
    let crowded = count.values().any(|&n| n >= 2);
    if count.len() < 2 || !crowded {
        return vec![Group {
            label: one_label.to_owned(),
            kind,
            items,
        }];
    }
    let mut order: Vec<(String, Group)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (it, t) in items.into_iter().zip(tops) {
        match at.get(&t) {
            Some(&k) => order[k].1.items.push(it),
            None => {
                let label = if t.is_empty() {
                    one_label.to_owned()
                } else {
                    dir_label(&t)
                };
                at.insert(t.clone(), order.len());
                order.push((
                    t,
                    Group {
                        label,
                        kind,
                        items: vec![it],
                    },
                ));
            }
        }
    }
    order.sort_by(|a, b| a.0.cmp(&b.0));
    order.into_iter().map(|(_, g)| g).collect()
}

/// A readable label for a folder name (`Fallen.Kingdom.Collection` -> `Fallen Kingdom`).
pub(crate) fn dir_label(name: &str) -> String {
    let ni = crate::nameinfo::NameInfo::parse(name, false);
    let t = crate::classifier::clean_title(&ni, ni.raw_title());
    trunc(if t.is_empty() { name.to_owned() } else { t }, MAX_LABEL)
}

/// Cheap readable text for a file stem when no parse is wanted.
pub(crate) fn plain_title(stem: &str) -> String {
    let s: String = stem
        .chars()
        .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
        .collect();
    trunc(
        s.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_LABEL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert_eq!(slug("Don't Look Now"), "dont-look-now");
        assert_eq!(slug("Tom & Jerry!"), "tom-and-jerry");
        assert_eq!(slug("  --A..B  "), "a-b");
        assert_eq!(slug("Amélie"), "amélie");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn stack_markers() {
        assert_eq!(strip_stack("movie.1999.cd1"), "movie.1999");
        assert_eq!(strip_stack("movie disc 2"), "movie");
        assert_eq!(strip_stack("setup.part03"), "setup");
        assert_eq!(strip_stack("movie 2"), "movie 2");
        assert_eq!(strip_stack("cd1"), "cd1");
        assert!(is_disc_dir("CD 2"));
        assert!(is_disc_dir("Disc.01"));
        assert!(!is_disc_dir("Discovery"));
        assert_eq!(strip_disc_dirs("A/B/CD1"), ("A/B", true));
        assert_eq!(strip_disc_dirs("A/B"), ("A/B", false));
    }

    #[test]
    fn within_is_path_aware() {
        assert!(within("a/b", "a"));
        assert!(within("a", "a"));
        assert!(!within("ab", "a"));
        assert!(within("anything", ""));
    }

    #[test]
    fn cut_on_char_boundary() {
        assert_eq!(trunc("ééé".to_owned(), 3), "é");
    }
}
