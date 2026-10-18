//! Content detection and locations (SPEC 7A): which category a torrent is, where its files go
//! (flat: the torrent's own layout below its finished location), which files are junk, plus the
//! file-system chores around it (Trash, archive).
//!
//! Everything here is blocking and runs on `spawn_blocking`, never on the session actor. Nothing
//! is kept per Cold torrent: the result of a plan is a category byte and a few flags.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use trs_classify::{Category, Classification, FileEntry, Input, JunkKind};
use trs_metainfo::Metainfo;

use crate::api::{AddOptions, AddPreview, ChoicePreview, EngineError, PreviewFile, Priority, ProfileBucket, SeedGoal};
use crate::config::{AfterAdd, EngineConfig};
use crate::persist::Placement;
use crate::policy::{self, PolicyData, ProfileMatch, Resolved};

/// Most files listed in a preview.
const PREVIEW_FILES: usize = 2000;

/// Destinations held by moves that are queued or running (and by legacy placed torrents being
/// added back), so two moves never pick the same name.
pub(crate) type DestReservations = Mutex<HashSet<PathBuf>>;

/// Inputs of [`plan`].
pub(crate) struct PlanCtx<'a> {
    pub cfg: &'a EngineConfig,
    pub policy: Option<&'a PolicyData>,
    /// Compute the smart-add structure and reasons (SPEC 7A.6) into `Plan::choice`.
    pub choice: bool,
    /// The watch folder says "ask first" (adds `ChoiceReason::FolderAsk`).
    pub folder_ask: bool,
}

/// Everything decided about a torrent before it is added.
#[derive(Debug)]
pub(crate) struct Plan {
    pub profile: ProfileMatch,
    pub bucket: ProfileBucket,
    pub detected: Category,
    pub confidence: f32,
    pub uncertain: bool,
    /// Category in effect (`None` when detection is off).
    pub category: Option<Category>,
    pub category_user: bool,
    /// The clean display title (file-aware, in the category in effect).
    pub title: String,
    pub resolved: Resolved,
    pub priority: Priority,
    pub priority_user: bool,
    pub goal: SeedGoal,
    pub goal_user: bool,
    /// Where the torrent's own layout lives: its finished location, below the status folder
    /// `Active/` when files are organized by status (and the torrent is not pinned).
    pub save_path: PathBuf,
    /// The finished location itself (rule > category > app default), without a status folder.
    pub finished_base: PathBuf,
    /// Staging root of unfinished data (`<root>/<info-hash>/`): the category's incomplete
    /// location, else the app default; `None` = below `<save>/.incomplete`.
    pub stage_root: Option<PathBuf>,
    /// Per-file priorities, empty when all are Normal.
    pub file_prios: Vec<u8>,
    pub junk: Vec<Option<JunkKind>>,
    /// The folder the torrent's own layout goes into.
    pub destination: Option<PathBuf>,
    /// Smart add: structure, preselection and reasons (only when the context asked for it).
    pub choice: Option<ChoicePreview>,
}

/// Name of a junk kind for previews.
pub(crate) fn junk_name(k: JunkKind) -> &'static str {
    match k {
        JunkKind::Sample => "sample",
        JunkKind::Nfo => "nfo",
        JunkKind::Ad => "ad",
        JunkKind::Promo => "promo",
        JunkKind::Suspicious => "suspicious",
        JunkKind::System => "system",
        JunkKind::Screens => "screens",
    }
}

/// `~` and `~/x` expanded with `$HOME`; other strings are used as they are.
pub(crate) fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" || s.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            if let Some(rest) = s.strip_prefix("~/") {
                p.push(rest);
            }
            return p;
        }
    }
    PathBuf::from(s)
}

fn announce_urls(mi: &Metainfo) -> Vec<&str> {
    mi.announce_tiers.iter().flat_map(|t| t.iter().map(|u| &**u)).collect()
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Name of the rule `id` (`None` = the Default rule) for display.
pub(crate) fn rule_name(policy: Option<&PolicyData>, id: Option<&str>) -> String {
    match id {
        Some(id) => policy.and_then(|p| p.rules.rules.iter().find(|r| r.id == id)).map_or_else(|| id.to_owned(), |r| r.name.clone()),
        None => "Default".into(),
    }
}

/// The persisted display title of `mi` in `category` (see `trs_classify::torrent_title`).
pub(crate) fn title_of(mi: &Metainfo, real: &[usize], category: Option<Category>) -> String {
    let entries: Vec<FileEntry<'_>> =
        real.iter().filter_map(|&i| mi.file(i)).map(|f| FileEntry { path: f.path, size: f.length }).collect();
    trs_classify::torrent_title(&mi.name, &entries, category)
}

/// Decide everything about `mi`: profile, category, rule, priorities, junk, destinations.
pub(crate) fn plan(mi: &Metainfo, opts: &AddOptions, ctx: &PlanCtx<'_>) -> Plan {
    let cfg = ctx.cfg;
    let urls = announce_urls(mi);
    let (profile, bucket) = match ctx.policy {
        Some(p) => (
            policy::match_profile(&p.profiles, urls.iter().copied()),
            policy::bucket_of(&p.profiles, urls.iter().copied(), mi.private),
        ),
        None => (
            ProfileMatch::default(),
            if mi.private { ProfileBucket::OtherPrivate } else { ProfileBucket::Public },
        ),
    };

    // ---- detection -----------------------------------------------------------------------
    let nfiles = mi.file_count();
    let real: Vec<usize> = mi.files().filter(|f| !f.is_padding()).map(|f| f.index).collect();
    let mut junk: Vec<Option<JunkKind>> = vec![None; nfiles];
    let (mut detected, mut confidence, mut uncertain) = (Category::Other, 0.0f32, false);
    let mut classification: Option<Classification> = None;
    if let Some(det) = cfg.detect {
        let entries: Vec<FileEntry<'_>> = real
            .iter()
            .filter_map(|&i| mi.file(i))
            .map(|f| FileEntry { path: f.path, size: f.length })
            .collect();
        let mut input = Input::new(&mi.name, &entries);
        input.tracker_source_hint = mi.source.as_deref();
        let c = trs_classify::classify(&input);
        for j in &c.junk_files {
            if let Some(&i) = real.get(j.index) {
                junk[i] = Some(j.kind);
            }
        }
        detected = c.category;
        confidence = c.confidence;
        uncertain = c.confidence < det.confidence_threshold;
        classification = Some(c);
    }
    let category_user = opts.category.is_some();
    let category = match (opts.category, &classification) {
        (Some(c), _) => Some(c),
        (None, Some(c)) => Some(if uncertain { Category::Other } else { c.category }),
        (None, None) => None,
    };
    if category_user {
        uncertain = false;
    }
    let title = title_of(mi, &real, category);

    // ---- rule ----------------------------------------------------------------------------
    let resolved = match ctx.policy {
        Some(p) => policy::resolve(
            &p.rules,
            &profile,
            opts.rule.as_deref(),
            mi.private,
            category.unwrap_or(Category::Other),
            p.private_min_seed,
            p.idle,
        ),
        None => Resolved::default(),
    };
    let priority = opts.priority.unwrap_or(resolved.priority);
    let goal = match opts.seed_goal {
        // With policy installed, the floor and the action at goal always come from the rule.
        Some(g) if ctx.policy.is_some() => SeedGoal { min_seed_secs: resolved.goal.min_seed_secs, at_goal: resolved.goal.at_goal, idle_mb: resolved.goal.idle_mb, idle_hours: resolved.goal.idle_hours, ..g },
        Some(g) => g,
        None => resolved.goal,
    };

    // ---- file priorities -----------------------------------------------------------------
    let skip_junk = cfg.detect.is_some_and(|d| d.skip_junk);
    let mut prios: Vec<u8> = if opts.file_priorities.is_empty() {
        Vec::new()
    } else {
        opts.file_priorities.iter().map(|p| p.to_u8()).collect()
    };
    if opts.file_priorities.is_empty() && skip_junk {
        prios = junk.iter().map(|j| if j.is_some() { 0 } else { 2 }).collect();
    }
    if prios.iter().all(|&p| p == 2) {
        prios.clear();
    }

    // ---- location ------------------------------------------------------------------------
    // The finished location (SPEC 7A.4): the rule's folder > the category's > the app default.
    // The torrent keeps its own layout below it (SPEC 7A.3). Organized by status, a torrent that
    // is not pinned starts in `Active/`; the status check files it elsewhere when it belongs
    // elsewhere (session/status.rs).
    let cat_in_effect = category.unwrap_or(Category::Other);
    let finished_base = match resolved.finished_dir.as_deref() {
        Some(d) => expand_tilde(d.trim_end_matches('/')),
        None => cfg.category_locations[cat_in_effect.index()].finished.clone().unwrap_or_else(|| cfg.default_save_dir.clone()),
    };
    let save_path = opts.save_path.clone().unwrap_or_else(|| {
        if cfg.by_status { finished_base.join(crate::status::StatusFolder::Active.name()) } else { finished_base.clone() }
    });
    let stage_root = cfg.category_locations[cat_in_effect.index()].incomplete.clone().or_else(|| cfg.incomplete_dir.clone());
    let destination = Some(save_path.clone());

    let choice = ctx.choice.then(|| {
        // What the finished location should hold: the torrent's own layout.
        let plain: Vec<Option<PathBuf>> =
            mi.files().map(|f| (!f.is_padding()).then(|| source_path(&save_path, mi, f.path))).collect();
        crate::choice::build(
            mi,
            &crate::choice::BuildIn {
                cfg,
                forced_category: opts.category,
                prios: None,
                dests: &plain,
                destination: destination.clone(),
                stage_root: stage_root.clone().or_else(|| Some(save_path.clone())),
                awaiting: false,
                folder_ask: ctx.folder_ask,
                check_library: true,
            },
        )
    });
    let choice = choice.map(|mut c| {
        c.rule_name = rule_name(ctx.policy, resolved.rule.as_deref());
        c.goal = goal;
        c.private = mi.private;
        c
    });

    Plan {
        profile,
        bucket,
        detected,
        confidence,
        uncertain,
        category,
        category_user,
        title,
        resolved,
        priority,
        priority_user: opts.priority.is_some(),
        goal,
        goal_user: opts.seed_goal.is_some(),
        save_path,
        finished_base,
        stage_root,
        file_prios: prios,
        junk,
        destination,
        choice,
    }
}

/// Folder (next to a placed file) that holds the edge data of files nobody wants (see [`edge_files`]).
pub(crate) const EDGE_DIR: &str = ".seedwell-partial";
/// The name before the product was renamed: records and folders written by older builds still
/// use it. It is recognised everywhere ([`proper_edge`], [`edge_names`]) and never written.
pub(crate) const LEGACY_EDGE_DIR: &str = ".dispatch-partial";

fn is_edge_dir(n: &std::ffi::OsStr) -> bool {
    n == EDGE_DIR || n == LEGACY_EDGE_DIR
}

/// A skipped file whose first or last piece is shared with a wanted file still gets that piece's
/// bytes written (the piece is verified and seeded as a whole). Such a file would show up in the
/// library as a full-length, mostly empty "episode". This is the hidden place for it: the same
/// name inside a dot folder next to the clean destination, which media apps ignore.
pub(crate) fn hidden_edge(p: &Path) -> PathBuf {
    let proper = proper_edge(p);
    match (proper.parent(), proper.file_name()) {
        (Some(dir), Some(name)) => dir.join(EDGE_DIR).join(name),
        _ => proper,
    }
}

/// The clean destination of a file whose destination may be in [`EDGE_DIR`].
pub(crate) fn proper_edge(p: &Path) -> PathBuf {
    let in_edge = p.parent().and_then(Path::file_name).is_some_and(is_edge_dir);
    match (in_edge, p.parent().and_then(Path::parent), p.file_name()) {
        (true, Some(grand), Some(name)) => grand.join(name),
        _ => p.to_path_buf(),
    }
}

/// Every name one torrent file can have on disk: the clean destination, the hidden one, and the
/// hidden one of older builds. Order: clean, hidden, legacy.
pub(crate) fn edge_names(p: &Path) -> [PathBuf; 3] {
    let proper = proper_edge(p);
    let hidden = hidden_edge(&proper);
    let legacy = match (proper.parent(), proper.file_name()) {
        (Some(dir), Some(name)) => dir.join(LEGACY_EDGE_DIR).join(name),
        _ => proper.clone(),
    };
    [proper, hidden, legacy]
}

/// Per file: skipped, and sharing a piece with a wanted file (so some of its bytes are written
/// anyway).
pub(crate) fn edge_files(mi: &Metainfo, prios: &[u8]) -> Vec<bool> {
    let pl = u64::from(mi.piece_length.max(1));
    // (start, end, wanted, padding)
    let f: Vec<(u64, u64, bool, bool)> = mi
        .files()
        .map(|x| (x.offset, x.offset.saturating_add(x.length), prios.get(x.index).copied().unwrap_or(2) > 0, x.is_padding()))
        .collect();
    let mut out = vec![false; f.len()];
    for (i, &(s, e, wanted, pad)) in f.iter().enumerate() {
        if wanted || pad || e <= s {
            continue;
        }
        let (first, last) = (s / pl, (e - 1) / pl);
        let (first_lo, first_hi) = (first * pl, (first + 1) * pl);
        let (last_lo, last_hi) = (last * pl, (last + 1) * pl);
        let shares = |a: u64, b: u64, lo: u64, hi: u64| a < hi && b > lo;
        let mut hit = false;
        for j in (0..i).rev() {
            let (s2, e2, w2, p2) = f[j];
            if e2 <= first_lo && s2 < e2 {
                break;
            }
            if w2 && !p2 && e2 > s2 && (shares(s2, e2, first_lo, first_hi) || shares(s2, e2, last_lo, last_hi)) {
                hit = true;
                break;
            }
        }
        if !hit {
            for &(s2, e2, w2, p2) in &f[i + 1..] {
                if s2 >= last_hi && s2 < e2 {
                    break;
                }
                if w2 && !p2 && e2 > s2 && (shares(s2, e2, first_lo, first_hi) || shares(s2, e2, last_lo, last_hi)) {
                    hit = true;
                    break;
                }
            }
        }
        out[i] = hit;
    }
    out
}

/// Forget reserved destinations.
pub(crate) fn release_paths(reserved: &DestReservations, paths: &[PathBuf]) {
    let mut g = reserved.lock().unwrap_or_else(|e| e.into_inner());
    for p in paths {
        g.remove(p);
    }
}

/// Forget reservations (the files are in place, or the torrent was removed).
pub(crate) fn release(reserved: &DestReservations, p: &Placement) {
    let mut g = reserved.lock().unwrap_or_else(|e| e.into_inner());
    for d in p.files.iter().flatten() {
        g.remove(d);
    }
}

/// A parsed torrent plus what the engine decided about it.
pub(crate) struct Prepared {
    pub id: [u8; 20],
    pub bytes: std::sync::Arc<[u8]>,
    pub name: String,
    pub total_length: u64,
    pub wanted_length: u64,
    pub piece_length: u32,
    pub private: bool,
    pub file_count: usize,
    pub announce: Vec<Vec<String>>,
    pub plan: Plan,
    pub tracker_host: String,
    /// Hold the torrent for a choice (smart add, SPEC 7A.6) instead of starting it.
    pub hold: bool,
}

/// Where file `rel` (a `FileRef::path`) of `mi` lives in the torrent's own layout below `save`.
pub(crate) fn source_path(save: &Path, mi: &Metainfo, rel: &str) -> PathBuf {
    let mut p = save.to_path_buf();
    if mi.is_multi_file() {
        p.push(&*mi.name);
    }
    p.extend(rel.split('/'));
    p
}

/// Wanted bytes given per-file priorities (0 = skip). Padding never counts.
pub(crate) fn wanted_bytes(mi: &Metainfo, prios: &[u8]) -> u64 {
    mi.files()
        .filter(|f| !f.is_padding() && prios.get(f.index).copied().unwrap_or(2) > 0)
        .map(|f| f.length)
        .sum()
}

/// Parse `.torrent` bytes and plan the add. Runs on a blocking thread.
pub(crate) fn prepare(
    bytes: Vec<u8>,
    opts: &AddOptions,
    ctx: &PlanCtx<'_>,
) -> Result<Prepared, EngineError> {
    let mi = Metainfo::from_bytes(&bytes).map_err(|e| EngineError::Invalid(e.to_string()))?;
    let id = mi.info_hash.swarm_id().ok_or_else(|| EngineError::Invalid("no info hash".into()))?;
    if mi.piece_count() == 0 {
        return Err(EngineError::Invalid("BitTorrent v2-only torrents are not supported yet".into()));
    }
    if opts.file_priorities.len() > mi.file_count() {
        return Err(EngineError::Invalid("more file priorities than files".into()));
    }
    let plan = plan(&mi, opts, ctx);
    let announce: Vec<Vec<String>> =
        mi.announce_tiers.iter().map(|t| t.iter().map(|u| u.to_string()).collect()).collect();
    let tracker_host = announce
        .first()
        .and_then(|t| t.first())
        .and_then(|u| trs_tracker::TrackerUrl::parse(u).ok())
        .map(|u| u.host().to_owned())
        .unwrap_or_default();
    let wanted = wanted_bytes(&mi, &plan.file_prios);
    let hold = match opts.hold {
        Some(h) => h,
        None => opts.file_priorities.is_empty() && plan.choice.as_ref().is_some_and(|c| c.needs_choice),
    };
    Ok(Prepared {
        id,
        name: mi.name.to_string(),
        total_length: mi.total_length,
        wanted_length: wanted,
        piece_length: mi.piece_length,
        private: mi.private,
        file_count: mi.file_count(),
        announce,
        tracker_host,
        plan,
        hold,
        bytes: std::sync::Arc::from(bytes),
    })
}

/// The preview of a prepared torrent. `mi` is parsed again by the caller only for the file list.
pub(crate) fn preview(p: &Prepared, mi: &Metainfo, duplicate: bool) -> AddPreview {
    let pl = &p.plan;
    let files = mi
        .files()
        .filter(|f| !f.is_padding())
        .take(PREVIEW_FILES)
        .map(|f| PreviewFile {
            path: f.path.to_owned(),
            length: f.length,
            junk: pl.junk.get(f.index).copied().flatten().map(junk_name),
        })
        .collect();
    AddPreview {
        id: p.id,
        name: p.name.clone(),
        total_bytes: p.total_length,
        file_count: p.file_count as u32,
        private: p.private,
        tracker_host: p.tracker_host.clone(),
        bucket: pl.bucket.clone(),
        category: pl.category.unwrap_or(Category::Other),
        detected: pl.detected,
        confidence: pl.confidence,
        uncertain: pl.uncertain,
        rule: pl.resolved.rule.clone(),
        priority: pl.priority,
        seed_goal: pl.goal,
        up_limit: pl.resolved.up_limit,
        destination: pl.destination.clone(),
        files,
        duplicate,
        choice: pl.choice.clone().unwrap_or_default(),
    }
}

/// Where a finished direct download goes for its category (see `Session::direct_place`): into the
/// category's own finished folder, when it has one (SPEC 7A.4); otherwise it stays where it is.
/// `Some(new path)` when the file moved.
pub(crate) fn place_direct(cfg: &EngineConfig, rec: &crate::persist::DirectRecord, old: Option<Category>) -> Option<PathBuf> {
    let path = rec.final_path.clone()?;
    if !path.is_file() {
        return None;
    }
    let folder_of = |c: Option<Category>| c.and_then(|c| cfg.category_locations[c.index()].finished.clone());
    let target = folder_of(rec.category).or_else(|| folder_of(old).map(|_| rec.dest_dir.clone()))?;
    if path.parent() == Some(target.as_path()) {
        return None;
    }
    move_unique(&path, &target).ok()
}

// ---- file-system chores --------------------------------------------------------------------

/// Copy a file across volumes and keep what Finder shows: extended attributes (tags, comments),
/// ACLs and the mode. (`std::fs::copy` keeps the mode only.) The copy never replaces a file.
pub(crate) fn copy_keeping_metadata(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    if let (Ok(src), Ok(dst)) = (std::fs::File::open(from), std::fs::OpenOptions::new().write(true).open(to)) {
        let _ = trs_storage::copy_metadata(&src, &dst);
        // Metadata copies reset the times on some systems: the file keeps its own.
        if let Ok(m) = src.metadata().and_then(|m| m.modified()) {
            let _ = dst.set_modified(m);
        }
    }
    Ok(())
}

/// Move `file` to `dir`, never overwriting: `name.torrent`, `name (2).torrent`, ...
pub(crate) fn move_unique(file: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let want = dir.join(&name).to_string_lossy().into_owned();
    let target = PathBuf::from(trs_classify::unique_path(&want, |c| exists(Path::new(c))));
    match std::fs::rename(file, &target) {
        Ok(()) => Ok(target),
        Err(_) => {
            // Another volume: copy, then remove the source.
            copy_keeping_metadata(file, &target)?;
            std::fs::remove_file(file)?;
            Ok(target)
        }
    }
}

/// Apply the "after adding" choice to the `.torrent` at `path`. Returns where the file lies afterwards
/// when it is still the user's to deal with: `path` itself for "leave", the archive copy for "archive";
/// `None` when it was deleted or put in the trash folder.
pub(crate) fn finish_file(path: &Path, action: &AfterAdd, trash_dir: Option<&Path>) -> std::io::Result<Option<PathBuf>> {
    match action {
        AfterAdd::Leave => Ok(Some(path.to_path_buf())),
        AfterAdd::Delete => std::fs::remove_file(path).map(|()| None),
        AfterAdd::Archive(dir) => move_unique(path, dir).map(Some),
        AfterAdd::Trash => {
            let dir = match trash_dir {
                Some(d) => d.to_path_buf(),
                None => expand_tilde("~/.Trash"),
            };
            move_unique(path, &dir).map(|_| None)
        }
    }
}

/// The `.torrent` file at `path` if it is safe to offer for the trash on behalf of torrent `id`: a regular
/// file (never a symlink) named `*.torrent` whose own info-hash is `id`. Anything else is `None`.
pub(crate) fn verified_torrent_file(path: &Path, id: &[u8; 20]) -> Option<PathBuf> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent")) {
        return None;
    }
    let limit = trs_state::store::MAX_TORRENT_FILE as u64;
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(limit + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > limit {
        return None;
    }
    let mi = trs_metainfo::Metainfo::from_bytes(&buf).ok()?;
    (mi.info_hash.swarm_id()? == *id).then(|| path.to_path_buf())
}

/// Empty directories above `file`, up to but excluding `stop` (never leaves `stop`).
pub(crate) fn prune_empty_dirs(file: &Path, stop: &Path) {
    let mut d = file.parent().map(Path::to_path_buf);
    while let Some(dir) = d {
        if dir == stop || !dir.starts_with(stop) || stop.as_os_str().is_empty() {
            break;
        }
        // Only empty directories go (Finder's `.DS_Store` does not count).
        if !trs_storage::remove_dir_if_empty(&dir) {
            break;
        }
        d = dir.parent().map(Path::to_path_buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_destinations_round_trip() {
        let p = Path::new("/lib/TV/Show/Season 01/Show - S01E03.mkv");
        let h = hidden_edge(p);
        assert_eq!(h, Path::new("/lib/TV/Show/Season 01/.seedwell-partial/Show - S01E03.mkv"));
        assert_eq!(hidden_edge(&h), h, "idempotent");
        assert_eq!(proper_edge(&h), p);
        assert_eq!(proper_edge(p), p);
    }

    #[test]
    fn the_old_product_name_of_the_hidden_folder_is_still_recognised() {
        let p = Path::new("/lib/TV/Show/Season 01/Show - S01E03.mkv");
        let old = Path::new("/lib/TV/Show/Season 01/.dispatch-partial/Show - S01E03.mkv");
        assert_eq!(proper_edge(old), p);
        let [proper, hidden, legacy] = edge_names(old);
        assert_eq!(proper, p);
        assert_eq!(hidden, hidden_edge(p));
        assert_eq!(legacy, old);
        assert_eq!(edge_names(p), edge_names(&hidden));
    }

    #[test]
    fn tilde_expands_only_at_the_start() {
        assert_eq!(expand_tilde("/a/~/b"), PathBuf::from("/a/~/b"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_tilde("~/x"), PathBuf::from(home).join("x"));
        }
    }

    #[test]
    fn move_unique_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.torrent");
        let trash = d.path().join("trash");
        for i in 0..3 {
            std::fs::write(&src, [i]).unwrap();
            let t = move_unique(&src, &trash).unwrap();
            assert_eq!(std::fs::read(&t).unwrap(), vec![i]);
        }
        let mut names: Vec<_> =
            std::fs::read_dir(&trash).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["a (2).torrent", "a (3).torrent", "a.torrent"]);
    }

    #[test]
    fn prune_stops_at_the_root_and_at_non_empty_dirs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("lib");
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("a/keep.txt"), b"x").unwrap();
        prune_empty_dirs(&deep.join("f.mkv"), &root);
        assert!(!root.join("a/b").exists(), "empty dirs go");
        assert!(root.join("a").exists(), "a non-empty dir stays");
        std::fs::remove_file(root.join("a/keep.txt")).unwrap();
        prune_empty_dirs(&root.join("a/f.mkv"), &root);
        assert!(root.exists(), "the library root stays");
    }
}
