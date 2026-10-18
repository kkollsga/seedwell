//! Smart add (SPEC 7A.6): what is in a torrent, what the user probably wants, and whether to ask.
//!
//! Everything here is blocking and pure apart from `stat` / directory listings of the library
//! (capped, see [`LIB_MAX_ENTRIES`], [`LIB_MAX_DIRS`], [`LIB_BUDGET`]); callers run it on
//! `spawn_blocking`, never on the session actor. Nothing is kept per torrent: the structure is
//! computed when an add is planned, when a magnet's metadata arrives, or when a chooser is
//! (re)opened, then dropped. A Cold torrent costs nothing for any of this.
//!
//! **Library matching rule.** An item is *in the library* when every primary file of the item
//! (a file of at least a tenth of the item's largest file; subtitles and cover art may be absent)
//! exists at its destination (SPEC 7A.3: the torrent's own layout below the finished location)
//! with exactly the same size. It is *at another quality* when
//! that is not so but a file in the item's destination folder yields the same identity key
//! (`trs_classify::library_identities`: same show + season + episode, same title + year, same
//! book) or, for albums, the folder holds at least half as many audio files as the album has.
//! Software builds are matched by destination only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use trs_classify::{
    Category, Classification, FileEntry, GroupKind, Input, Item, Os, Structure, StructureKind, classify,
    library_identities, structure,
};
use trs_metainfo::Metainfo;

use crate::api::*;
use crate::config::{AskMode, EngineConfig};

/// Directory entries read per library folder.
const LIB_MAX_ENTRIES: usize = 512;
/// Library folders listed per torrent.
const LIB_MAX_DIRS: usize = 400;
/// Time the library check may take in all; what is not checked by then counts as "not in library".
const LIB_BUDGET: Duration = Duration::from_millis(1500);
/// Most items listed one by one in the Extras and Other files groups.
const MAX_LISTED_EXTRAS: usize = 400;

/// Inputs of [`build`].
pub(crate) struct BuildIn<'a> {
    /// Settings.
    pub cfg: &'a EngineConfig,
    /// Category the user (or a watch folder) chose.
    pub forced_category: Option<Category>,
    /// Current per-file priorities of a torrent that exists (selection = what is wanted now);
    /// `None` for a new add (preselection by the rules of SPEC 7A.6).
    pub prios: Option<&'a [u8]>,
    /// Destination per torrent file index: the torrent's own layout below the save folder.
    /// `None` entries are not checked.
    pub dests: &'a [Option<PathBuf>],
    /// Folder the torrent's own layout goes into (its finished location).
    pub destination: Option<PathBuf>,
    /// Staging root (`<root>/<info-hash>/` is where unfinished data goes).
    pub stage_root: Option<PathBuf>,
    /// The torrent is held for a choice right now.
    pub awaiting: bool,
    /// The watch folder says "ask first".
    pub folder_ask: bool,
    /// Look at the library (stat / list) for "you have some". Off for torrents that exist.
    pub check_library: bool,
}

fn slug_eq_year(a: &str, b: &str) -> bool {
    // `movie:<slug>:<year>` against `movie:<slug>` (one side without a year): compare slugs only.
    if a == b {
        return true;
    }
    let (Some(x), Some(y)) = (a.strip_prefix("movie:"), b.strip_prefix("movie:")) else { return false };
    let slug = |s: &'_ str| s.rsplit_once(':').map_or(s.to_owned(), |(h, _)| h.to_owned());
    let (xs, ys) = (x.contains(':'), y.contains(':'));
    (xs != ys) && (if xs { slug(x) == y } else { x == slug(y) })
}

struct DirCache {
    dirs: HashMap<PathBuf, Option<Vec<(String, u64)>>>,
    started: Instant,
}

impl DirCache {
    fn new() -> Self {
        Self { dirs: HashMap::new(), started: Instant::now() }
    }

    fn exhausted(&self) -> bool {
        self.started.elapsed() > LIB_BUDGET
    }

    fn list(&mut self, dir: &Path) -> Option<&Vec<(String, u64)>> {
        if !self.dirs.contains_key(dir) {
            if self.dirs.len() >= LIB_MAX_DIRS || self.exhausted() {
                return None;
            }
            let listed = std::fs::read_dir(dir).ok().map(|rd| {
                rd.flatten()
                    .take(LIB_MAX_ENTRIES)
                    .filter_map(|e| {
                        let md = e.metadata().ok()?;
                        md.is_file().then(|| (e.file_name().to_string_lossy().into_owned(), md.len()))
                    })
                    .collect::<Vec<_>>()
            });
            self.dirs.insert(dir.to_path_buf(), listed);
        }
        self.dirs.get(dir).and_then(Option::as_ref)
    }
}

fn is_audio_name(n: &str) -> bool {
    let e = n.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(e.as_str(), "flac" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav" | "alac" | "ape" | "wv" | "aiff")
}

/// What the library holds of `item`.
fn library_state(
    item: &Item,
    cat: Category,
    hint: Option<&str>,
    mi: &Metainfo,
    real: &[usize],
    dests: &[Option<PathBuf>],
    cache: &mut DirCache,
) -> LibraryState {
    if cache.exhausted() {
        return LibraryState::Missing;
    }
    // Files of the item with a destination, and the primary ones among them.
    let files: Vec<(usize, u64, &PathBuf)> = item
        .file_indices
        .iter()
        .filter_map(|&i| {
            let t = *real.get(i as usize)?;
            let f = mi.file(t)?;
            Some((t, f.length, dests.get(t)?.as_ref()?))
        })
        .collect();
    let biggest = files.iter().map(|f| f.1).max().unwrap_or(0);
    let primary: Vec<&(usize, u64, &PathBuf)> = files.iter().filter(|f| f.1 > 0 && f.1 * 10 >= biggest).collect();
    let Some(first) = primary.first() else { return LibraryState::Missing };
    let exact = primary
        .iter()
        .all(|(_, len, p)| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() == *len));
    if exact {
        return LibraryState::InLibrary { path: first.2.clone() };
    }
    let Some(dir) = first.2.parent() else { return LibraryState::Missing };
    let Some(entries) = cache.list(dir) else { return LibraryState::Missing };
    let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if cat == Category::Music {
        let audio = entries.iter().filter(|(n, _)| is_audio_name(n)).count();
        if audio > 0 && audio * 2 >= primary.len() {
            return LibraryState::OtherQuality { path: dir.to_path_buf() };
        }
        return LibraryState::Missing;
    }
    let mut wanted: Vec<&str> = vec![item.identity.as_str()];
    wanted.extend(item.alt_identities.iter().map(String::as_str));
    for (name, _) in entries {
        let rel = format!("{dir_name}/{name}");
        for key in library_identities(cat, hint, &rel) {
            if wanted.iter().any(|w| slug_eq_year(w, &key)) {
                return LibraryState::OtherQuality { path: dir.join(name) };
            }
        }
    }
    LibraryState::Missing
}

fn group_kind(k: GroupKind) -> ChoiceGroupKind {
    match k {
        GroupKind::Season(n) => ChoiceGroupKind::Season(n),
        GroupKind::Specials => ChoiceGroupKind::Specials,
        GroupKind::Collection => ChoiceGroupKind::Collection,
        GroupKind::Album => ChoiceGroupKind::Album,
        GroupKind::Platform(_) => ChoiceGroupKind::Platform,
        GroupKind::Other => ChoiceGroupKind::Other,
    }
}

fn structure_name(k: StructureKind) -> &'static str {
    match k {
        StructureKind::Single => "single",
        StructureKind::Tv => "tv",
        StructureKind::MovieCollection => "movies",
        StructureKind::Music => "music",
        StructureKind::Books => "books",
        StructureKind::Audiobooks => "audiobooks",
        StructureKind::Software => "software",
        StructureKind::Flat => "flat",
    }
}

fn os_name(o: Os) -> &'static str {
    match o {
        Os::MacOs => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Android => "android",
        Os::Ios => "ios",
        Os::Any => "any",
    }
}

fn cpu_name(c: trs_classify::Cpu) -> &'static str {
    match c {
        trs_classify::Cpu::Arm64 => "arm64",
        trs_classify::Cpu::X86_64 => "x86_64",
        trs_classify::Cpu::Universal => "universal",
        trs_classify::Cpu::X86 => "x86",
        trs_classify::Cpu::Any => "any",
    }
}

/// Free bytes at the destination and the staging root (the smaller), when known.
fn free_space(cfg: &EngineConfig, destination: Option<&Path>, stage: Option<&Path>) -> Option<u64> {
    if let Some(v) = cfg.smart_add.free_space_override {
        return Some(v);
    }
    let a = destination.and_then(|p| trs_storage::available_space(p).ok());
    let b = stage.and_then(|p| trs_storage::available_space(p).ok());
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Classify `mi` the way an add does (the info-dict `source` is the tracker hint).
pub(crate) fn classify_torrent(mi: &Metainfo) -> (Classification, Vec<usize>) {
    let real: Vec<usize> = mi.files().filter(|f| !f.is_padding()).map(|f| f.index).collect();
    let entries: Vec<FileEntry<'_>> =
        real.iter().filter_map(|&i| mi.file(i)).map(|f| FileEntry { path: f.path, size: f.length }).collect();
    let mut input = Input::new(&mi.name, &entries);
    input.tracker_source_hint = mi.source.as_deref();
    (classify(&input), real)
}

/// The chooser's content for `mi` (SPEC 7A.6). See the module docs for the matching rule.
pub(crate) fn build(mi: &Metainfo, b: &BuildIn<'_>) -> ChoicePreview {
    let cfg = b.cfg;
    let nfiles = mi.file_count();
    let total = mi.total_length;
    let det = cfg.detect;
    let mut out = ChoicePreview {
        awaiting: b.awaiting,
        total_bytes: total,
        destination: b.destination.clone(),
        files: vec![true; nfiles],
        ..ChoicePreview::default()
    };
    if det.is_none() {
        // Detection is off: no categories, no structure, nothing to ask about.
        out.selected_files = mi.files().filter(|f| !f.is_padding()).count() as u32;
        out.selected_bytes = total;
        out.free_bytes = free_space(cfg, b.destination.as_deref(), b.stage_root.as_deref());
        out.needs_choice = b.folder_ask;
        if b.folder_ask {
            out.reasons.push(ChoiceReason::FolderAsk);
        }
        return out;
    }
    let (c, real) = classify_torrent(mi);
    let entries: Vec<FileEntry<'_>> =
        real.iter().filter_map(|&i| mi.file(i)).map(|f| FileEntry { path: f.path, size: f.length }).collect();
    let threshold = det.map_or(0.8, |d| d.confidence_threshold);
    let uncertain = b.forced_category.is_none() && c.confidence < threshold;
    let cat = b.forced_category.unwrap_or(c.category);
    let mut shown = c.clone();
    shown.category = cat;
    let st: Structure = structure(&shown, &entries);
    out.structure = structure_name(st.kind);
    out.category = Some(if uncertain { Category::Other } else { cat });
    out.confidence = c.confidence;

    let skip_junk = det.is_some_and(|d| d.skip_junk);
    let to_t = |i: u32| real.get(i as usize).map(|t| *t as u32);
    let hint = shown.fields.show.as_deref().or(shown.fields.title.as_deref());
    // The library is only looked at when there is a real choice to inform: several items, or
    // builds for several systems. A single movie is never checked.
    let library_on = b.check_library && !uncertain && (st.multi || st.has_platform_variants());
    let mut cache = DirCache::new();
    let this_mac: Vec<(usize, usize)> = st.this_mac_items();
    let variants = st.has_platform_variants();

    let mut groups: Vec<ChoiceGroup> = Vec::new();
    let mut in_library = 0u32;
    let mut sel_idx: Vec<Vec<bool>> = Vec::new();
    let prio = |t: usize| b.prios.map(|p| p.get(t).copied().unwrap_or(2) > 0);
    for (gi, g) in st.groups.iter().enumerate() {
        let mut items = Vec::with_capacity(g.items.len());
        let mut sels = Vec::with_capacity(g.items.len());
        for (ii, it) in g.items.iter().enumerate() {
            let files: Vec<u32> = it.file_indices.iter().filter_map(|&i| to_t(i)).collect();
            let lib = if library_on {
                library_state(it, cat, hint, mi, &real, b.dests, &mut cache)
            } else {
                LibraryState::Missing
            };
            if lib.present() {
                in_library += 1;
            }
            let mac = this_mac.contains(&(gi, ii));
            let selected = match b.prios {
                Some(_) => !files.is_empty() && files.iter().all(|&t| prio(t as usize).unwrap_or(true)),
                None => {
                    if lib.present() {
                        false
                    } else if variants && !this_mac.is_empty() {
                        mac
                    } else {
                        true
                    }
                }
            };
            sels.push(selected);
            items.push(ChoiceItem {
                label: it.label.clone(),
                identity: it.identity.clone(),
                bytes: it.bytes,
                files,
                selected,
                library: lib,
                quality: it.quality_label(),
                this_mac: mac,
                platform: it.variant.map(|v| (os_name(v.os), cpu_name(v.cpu))),
            });
        }
        sel_idx.push(sels);
        groups.push(ChoiceGroup { label: g.label.clone(), kind: group_kind(g.kind), items });
    }
    // When every item is already in the library nothing is preselected, and the reason below
    // still tells the user why.
    let any_selected = groups.iter().flat_map(|g| &g.items).any(|i| i.selected);

    // Files that belong to no item are kept whenever something is chosen.
    let others: Vec<u32> = st.unassigned.iter().filter_map(|&i| to_t(i)).collect();
    if !others.is_empty() {
        let bytes = others.iter().filter_map(|&t| mi.file(t as usize)).map(|f| f.length).sum();
        let keep = match b.prios {
            Some(_) => others.iter().all(|&t| prio(t as usize).unwrap_or(true)),
            None => any_selected || st.item_count() == 0,
        };
        groups.push(ChoiceGroup {
            label: "Other files".into(),
            kind: ChoiceGroupKind::Other,
            items: vec![ChoiceItem {
                label: format!("{} other file{}", others.len(), if others.len() == 1 { "" } else { "s" }),
                identity: String::new(),
                bytes,
                files: others,
                selected: keep,
                library: LibraryState::Missing,
                quality: None,
                this_mac: false,
                platform: None,
            }],
        });
    }
    // Extras: junk, samples, featurettes. One item per file, unticked unless junk is not skipped.
    let extras: Vec<u32> = st.extras.iter().filter_map(|&i| to_t(i)).collect();
    if !extras.is_empty() {
        let mut items = Vec::new();
        for (n, &t) in extras.iter().enumerate() {
            let Some(f) = mi.file(t as usize) else { continue };
            if n >= MAX_LISTED_EXTRAS {
                break;
            }
            let selected = match b.prios {
                Some(_) => prio(t as usize).unwrap_or(false),
                None => !skip_junk,
            };
            items.push(ChoiceItem {
                label: f.path.to_owned(),
                identity: String::new(),
                bytes: f.length,
                files: vec![t],
                selected,
                library: LibraryState::Missing,
                quality: None,
                this_mac: false,
                platform: None,
            });
        }
        // Extras beyond the listing share one last item so that no file is lost.
        if extras.len() > MAX_LISTED_EXTRAS {
            let rest: Vec<u32> = extras[MAX_LISTED_EXTRAS..].to_vec();
            let bytes = rest.iter().filter_map(|&t| mi.file(t as usize)).map(|f| f.length).sum();
            let selected = match b.prios {
                Some(_) => rest.iter().all(|&t| prio(t as usize).unwrap_or(false)),
                None => !skip_junk,
            };
            items.push(ChoiceItem {
                label: format!("{} more extra files", rest.len()),
                identity: String::new(),
                bytes,
                files: rest,
                selected,
                library: LibraryState::Missing,
                quality: None,
                this_mac: false,
                platform: None,
            });
        }
        groups.push(ChoiceGroup { label: "Extras".into(), kind: ChoiceGroupKind::Extras, items });
    }

    // Preselection per file.
    let mut files = vec![false; nfiles];
    for g in &groups {
        for it in &g.items {
            for &t in &it.files {
                if let Some(slot) = files.get_mut(t as usize) {
                    *slot = it.selected;
                }
            }
        }
    }
    // Padding files (BEP 47) are never chosen or skipped: they stay `true` so that applying the
    // preselection as priorities leaves them alone.
    for f in mi.files() {
        if f.is_padding() {
            if let Some(slot) = files.get_mut(f.index) {
                *slot = true;
            }
        }
    }
    let (mut sel_bytes, mut sel_files) = (0u64, 0u32);
    for f in mi.files() {
        if files.get(f.index).copied().unwrap_or(false) {
            sel_bytes = sel_bytes.saturating_add(f.length);
            sel_files += 1;
        }
    }
    out.groups = groups;
    out.files = files;
    out.selected_bytes = sel_bytes;
    out.selected_files = sel_files;
    out.library_checked = library_on;
    out.free_bytes = free_space(cfg, b.destination.as_deref(), b.stage_root.as_deref());

    // Reasons (SPEC 7A.6 table).
    let ss = &cfg.smart_add;
    let mut reasons = Vec::new();
    if st.multi {
        reasons.push(ChoiceReason::SeveralItems { items: st.item_count() as u32 });
    }
    if in_library > 0 {
        reasons.push(ChoiceReason::InLibrary { items: in_library });
    }
    if variants {
        reasons.push(ChoiceReason::PlatformVariants);
    }
    if ss.large_bytes > 0 && total > ss.large_bytes {
        reasons.push(ChoiceReason::VeryLarge { bytes: total, limit: ss.large_bytes });
    }
    if let Some(free) = out.free_bytes {
        if sel_bytes > free {
            reasons.push(ChoiceReason::NotEnoughSpace { needed: sel_bytes, free });
        }
    }
    if uncertain && det.is_some_and(|d| d.ask_when_unsure) {
        reasons.push(ChoiceReason::Unsure { confidence: c.confidence });
    }
    if b.folder_ask {
        reasons.push(ChoiceReason::FolderAsk);
    }
    if ss.mode == AskMode::Always && nfiles > 0 {
        reasons.push(ChoiceReason::Always);
    }
    // A torrent that exists is not held: its chooser (Get Info > Files) only edits the selection.
    out.needs_choice = b.prios.is_none() && !reasons.is_empty() && (ss.mode != AskMode::Never || b.folder_ask);
    out.reasons = reasons;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movie_identities_without_a_year_match_on_the_slug() {
        assert!(slug_eq_year("movie:night-of-the-living-dead:1968", "movie:night-of-the-living-dead"));
        assert!(slug_eq_year("movie:night-of-the-living-dead", "movie:night-of-the-living-dead:1968"));
        assert!(slug_eq_year("movie:a:1999", "movie:a:1999"));
        assert!(!slug_eq_year("movie:a:1999", "movie:a:2001"));
        assert!(!slug_eq_year("movie:a:1999", "movie:b"));
        assert!(!slug_eq_year("tv:a:s01e01", "tv:a:s01e02"));
    }
}
