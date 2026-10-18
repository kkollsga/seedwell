//! Planning a move (SPEC 7A.5): where a torrent's files are now, where they go, what the record
//! looks like afterwards, and what kind of move it is. Blocking (reads the stored `.torrent`, the
//! placement and the file system); runs on `spawn_blocking`, never on the session actor.
//!
//! Nothing is kept for Cold torrents: a plan exists only while a move is queued for its volume or
//! running, and it is computed when the move's turn comes, from the stored `.torrent`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use trs_metainfo::Metainfo;
use trs_storage::{Layout, PathRef};

use crate::api::{AddOptions, MoveKind, TorrentId};
use crate::organize::{self, PlanCtx};
use crate::persist::{Placed, Placement};
use crate::shared::Shared;
use crate::status::{StatusFolder, split_status};
use crate::torrent::build_layout;

/// Where a move goes.
#[derive(Clone, Debug)]
pub(crate) enum MoveTarget {
    /// To where the settings (and the torrent's category and rule) say it belongs now.
    Config,
    /// Into this status folder of the location the torrent is in now (SPEC 7A.3); nothing else
    /// about its location changes.
    Status(StatusFolder),
    /// To below this folder ("Move to..."): the torrent is pinned there afterwards.
    Rebase(PathBuf),
}

/// What the session knows about the torrent; enough to plan without touching the table.
pub(crate) struct MoveSrc {
    pub id: TorrentId,
    pub name: String,
    /// The options that reproduce the user's choices (category, rule, ...).
    pub opts: AddOptions,
    pub placed: Placed,
    /// The data still lives in the staging folder.
    pub staged: bool,
    pub save_path: PathBuf,
    /// The torrent's staging folder (`<root>/<info-hash>` or `<save>/.incomplete`) when staged.
    pub stage_dir: Option<PathBuf>,
    pub pinned: bool,
    /// The status folder the torrent belongs in by its sharing state right now; `None` when that
    /// is not decided (Error, or not a seeding torrent).
    pub live_folder: Option<StatusFolder>,
}

/// A planned move.
pub(crate) struct MovePlan {
    pub id: TorrentId,
    pub name: String,
    pub kind: MoveKind,
    pub from_save: PathBuf,
    pub from_stage_dir: Option<PathBuf>,
    pub to_save: PathBuf,
    /// Staging root after the move (`<root>/<info-hash>` is the folder); `None` = below the save folder.
    pub to_stage_root: Option<PathBuf>,
    pub to_placed: Placed,
    /// The placement to persist (`Some` iff `to_placed == Renamed`).
    pub to_placement: Option<Placement>,
    /// Absolute destination per file for the relocation (empty: nothing to relocate).
    pub dst: Vec<Option<PathBuf>>,
    pub bytes: u64,
    pub cross_volume: bool,
    /// Device of the destination volume (the scheduling key for "one copy per volume").
    pub dest_dev: u64,
    /// The move is the user's choice of location: the torrent is pinned afterwards.
    pub pin: bool,
    /// The geometry is the torrent's; paths as before / as after.
    pub layout_old: Arc<Layout>,
    pub layout_new: Arc<Layout>,
    /// Folder above which nothing is pruned once the old files are gone.
    pub stop_at: PathBuf,
    /// Destinations to hold in `Shared::reserved` while the move runs.
    pub reserved: Vec<PathBuf>,
    pub from_display: PathBuf,
    pub to_display: PathBuf,
    /// Started by the engine's own status check (set when the job is planned).
    pub auto: bool,
}

impl MovePlan {
    /// The cheap check of `trs_storage::preflight_move` for this plan: nothing is locked and the
    /// folders can be changed. Stats only, so it can run at every automatic check.
    pub(crate) fn preflight(&self) -> Result<(), trs_storage::StorageError> {
        let mut pairs: Vec<(u32, PathBuf, &Path)> = Vec::new();
        for (i, d) in self.dst.iter().enumerate() {
            let Some(d) = d else { continue };
            if self.layout_old.file_is_padding(i) {
                continue;
            }
            let src = match (self.kind, &self.from_stage_dir, self.layout_old.file_path(i)) {
                (MoveKind::Incomplete, Some(stage), _) => staging_path(&self.layout_old, i, stage),
                (_, _, PathRef::Relative(rel)) => self.from_save.join(rel),
                (_, _, PathRef::Absolute(abs)) => PathBuf::from(abs),
            };
            pairs.push((i as u32, src, d.as_path()));
        }
        trs_storage::preflight_move(pairs.iter().map(|(i, s, d)| (*i, s.as_path(), *d)))
    }
}

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Current staging path of file `i` (SPEC: relative layouts below the staging folder, custom
/// targets as `.custom/<index>-<name>`).
fn staging_path(layout: &Layout, i: usize, stage: &Path) -> PathBuf {
    match layout.file_path(i) {
        PathRef::Relative(rel) => stage.join(rel),
        PathRef::Absolute(abs) => stage.join(".custom").join(format!("{i}-{}", file_name_of(Path::new(abs)))),
    }
}

/// Plan the move of one torrent. `Ok(None)`: nothing to do (it is where it belongs).
pub(crate) fn plan_move(sh: &Shared, src: &MoveSrc, target: &MoveTarget) -> Result<Option<MovePlan>, String> {
    let bytes = sh
        .store
        .load_torrent_file(&src.id)
        .map_err(|e| e.to_string())?
        .ok_or("the stored .torrent file is missing")?;
    let mi = Metainfo::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let cfg = sh.cfg();
    let placement_cur: Option<Placement> =
        if src.placed == Placed::Renamed { sh.store.load_placement(&src.id).map_err(|e| e.to_string())? } else { None };
    let n = mi.file_count();
    let layout_old = Arc::new(build_layout(&mi, placement_cur.as_ref())?);

    // Where every file is now (its final place) ...
    let cur_final: Vec<Option<PathBuf>> = mi
        .files()
        .map(|f| {
            if f.is_padding() {
                return None;
            }
            Some(
                placement_cur
                    .as_ref()
                    .and_then(|p| p.files.get(f.index).cloned().flatten())
                    .unwrap_or_else(|| organize::source_path(&src.save_path, &mi, f.path)),
            )
        })
        .collect();
    let own: HashSet<PathBuf> = cur_final.iter().flatten().cloned().collect();

    // ... and where it should be.
    let hex = crate::util::hex(&src.id);
    let layout_files = |save: &Path| -> Vec<Option<PathBuf>> {
        mi.files().map(|f| (!f.is_padding()).then(|| organize::source_path(save, &mi, f.path))).collect()
    };
    // The staging root of a torrent that is staged now (`<root>/<info-hash>`).
    let cur_stage_root: Option<PathBuf> = src
        .stage_dir
        .as_ref()
        .filter(|d| d.file_name().is_some_and(|n| *n == *hex))
        .and_then(|d| d.parent().map(Path::to_path_buf));
    let (new_save, new_stage_root, mut new_placed, mut per_file, lib_root, pin): (
        PathBuf,
        Option<PathBuf>,
        Placed,
        Vec<Option<PathBuf>>,
        PathBuf,
        bool,
    ) = match target {
        MoveTarget::Rebase(root) => {
            // An explicit "Move to..." is about where the finished files live; the incomplete
            // location stays as it is.
            let per_file = if let Some(pc) = &placement_cur {
                let old_root = &pc.library_root;
                cur_final
                    .iter()
                    .zip(mi.files())
                    .map(|(cur, f)| {
                        let cur = cur.as_ref()?;
                        if pc.files.get(f.index).and_then(Option::as_ref).is_none() {
                            return Some(organize::source_path(root, &mi, f.path));
                        }
                        Some(match cur.strip_prefix(old_root) {
                            Ok(rel) if !old_root.as_os_str().is_empty() => root.join(rel),
                            _ => root.join(file_name_of(cur)),
                        })
                    })
                    .collect()
            } else {
                layout_files(root)
            };
            (root.clone(), cur_stage_root.clone(), src.placed, per_file, root.clone(), true)
        }
        MoveTarget::Config | MoveTarget::Status(_) => {
            // Torrents the user placed, and those that earlier versions placed file by file, keep
            // the paths they have (SPEC 7A.3).
            if src.pinned || src.placed == Placed::Renamed {
                return Ok(None);
            }
            let (cur_base, cur_folder) = split_status(&src.save_path);
            let (save, stage_root) = match target {
                MoveTarget::Status(f) => (cur_base.join(f.name()), cur_stage_root.clone()),
                _ => {
                    let pol = sh.policy();
                    let ctx = PlanCtx { cfg: &cfg, policy: pol.as_deref(), choice: false, folder_ask: false };
                    let plan = organize::plan(&mi, &src.opts, &ctx);
                    let base = plan.finished_base;
                    let save = if !cfg.by_status {
                        base
                    } else if let Some(f) = cur_folder {
                        base.join(f.name())
                    } else if src.staged {
                        // A download in progress finishes into Active/ like a new one.
                        base.join(StatusFolder::Active.name())
                    } else {
                        match src.live_folder {
                            Some(f) => base.join(f.name()),
                            None => base,
                        }
                    };
                    (save, plan.stage_root)
                }
            };
            let pf = layout_files(&save);
            (save.clone(), stage_root, Placed::No, pf, save, false)
        }
    };

    // A download in progress keeps the names of its files (the staged copies are named after them),
    // and does not switch between "placed" and "own layout".
    if src.staged {
        if (new_placed == Placed::Renamed) != (src.placed == Placed::Renamed) && matches!(target, MoveTarget::Config) {
            return Ok(None);
        }
        for (i, slot) in per_file.iter_mut().enumerate() {
            if let (Some(new), Some(cur)) = (slot.as_ref(), cur_final.get(i).and_then(Option::as_ref))
                && new_placed == Placed::Renamed {
                    *slot = new.parent().map(|d| d.join(file_name_of(cur)));
                }
        }
    }

    // Names that exist at the destination get a suffix, never an overwrite (SPEC 7A.3).
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let mut collided = false;
    {
        let reserved = sh.reserved.lock().unwrap_or_else(|e| e.into_inner());
        for (i, slot) in per_file.iter_mut().enumerate() {
            let Some(p) = slot.clone() else { continue };
            if cur_final.get(i).and_then(Option::as_ref) == Some(&p) {
                taken.insert(p);
                continue;
            }
            let s = p.to_string_lossy().into_owned();
            let unique = trs_classify::unique_path(&s, |cand| {
                let cp = Path::new(cand);
                (exists(cp) && !own.contains(cp)) || taken.contains(cp) || reserved.contains(cp)
            });
            if unique != s {
                collided = true;
            }
            let up = PathBuf::from(unique);
            taken.insert(up.clone());
            *slot = Some(up);
        }
    }
    if collided && new_placed == Placed::No {
        if src.staged {
            return Err("files with these names already exist at the destination".into());
        }
        // Per-file destinations are the way to rename single files: the torrent becomes "placed".
        new_placed = Placed::Renamed;
    }
    let was_placed: Vec<bool> = (0..n)
        .map(|i| placement_cur.as_ref().and_then(|p| p.files.get(i)).is_some_and(Option::is_some))
        .collect();
    let to_placement = (new_placed == Placed::Renamed).then(|| Placement {
        files: per_file
            .iter()
            .zip(mi.files())
            .map(|(p, f)| {
                let default = organize::source_path(&new_save, &mi, f.path);
                // A file that was at its layout place stays "default" unless it is placed
                // explicitly now; a torrent that becomes placed lists every file.
                p.clone().filter(|p| src.placed != Placed::Renamed || was_placed[f.index] || *p != default)
            })
            .collect(),
        library_root: lib_root.clone(),
    });

    // The new layout, built exactly as a fresh start would build it.
    let layout_new = Arc::new(build_layout(&mi, to_placement.as_ref())?);
    let to_stage_dir = src.staged.then(|| match &new_stage_root {
        Some(r) => r.join(&hex),
        None => new_save.join(".incomplete"),
    });

    // What has to be relocated.
    let (kind, dst): (MoveKind, Vec<Option<PathBuf>>) = if !src.staged {
        let dst: Vec<Option<PathBuf>> = (0..n)
            .map(|i| {
                let (cur, new) = (cur_final.get(i).and_then(Option::as_ref)?, per_file.get(i).and_then(Option::as_ref)?);
                (cur != new).then(|| new.clone())
            })
            .collect();
        (MoveKind::Finished, dst)
    } else if src.stage_dir != to_stage_dir {
        let (Some(from), Some(to)) = (src.stage_dir.as_ref(), to_stage_dir.as_ref()) else { return Ok(None) };
        let dst: Vec<Option<PathBuf>> = (0..n)
            .map(|i| {
                if layout_old.file_is_padding(i) {
                    return None;
                }
                let (a, b) = (staging_path(&layout_old, i, from), staging_path(&layout_old, i, to));
                (a != b).then_some(b)
            })
            .collect();
        (MoveKind::Incomplete, dst)
    } else {
        (MoveKind::Retarget, Vec::new())
    };
    let moving = dst.iter().flatten().count();
    let changed_record = new_save != src.save_path
        || new_placed != src.placed
        || to_placement.as_ref() != placement_cur.as_ref()
        || to_stage_dir != src.stage_dir;
    if moving == 0 && !changed_record {
        return Ok(None);
    }
    let kind = if moving == 0 && kind != MoveKind::Retarget { MoveKind::Retarget } else { kind };

    let bytes: u64 = dst
        .iter()
        .enumerate()
        .filter(|(_, d)| d.is_some())
        .map(|(i, _)| mi.file(i).map_or(0, |f| f.length))
        .sum();
    // Same volume or not: judged on the first moved file (a move is one pair of folders).
    let probe_src: Option<PathBuf> = if src.staged {
        src.stage_dir.clone()
    } else {
        dst.iter().enumerate().find(|(_, d)| d.is_some()).and_then(|(i, _)| cur_final[i].clone())
    };
    let probe_dst: Option<PathBuf> = dst.iter().flatten().next().cloned();
    let (cross, dest_dev) = match (&probe_src, &probe_dst) {
        (Some(a), Some(b)) => {
            let (ad, bd) = (a.parent().unwrap_or(a), b.parent().unwrap_or(b));
            let crossing = !trs_state::volume::same_volume(ad, bd).unwrap_or(false);
            (crossing || cfg.force_copy_moves, trs_state::volume::volume_id(bd).unwrap_or(0))
        }
        _ => (false, 0),
    };
    let stop_at = if src.staged {
        match (&src.stage_dir, src.stage_dir.as_ref().and_then(|d| d.file_name())) {
            (Some(d), Some(name)) if *name == *hex => d.parent().map(Path::to_path_buf).unwrap_or_else(|| src.save_path.clone()),
            (Some(d), _) => d.clone(),
            _ => src.save_path.clone(),
        }
    } else if let Some(pc) = &placement_cur {
        if pc.library_root.as_os_str().is_empty() { src.save_path.clone() } else { pc.library_root.clone() }
    } else {
        src.save_path.clone()
    };
    // Where the content is: the folder that holds all of its files (the torrent's own folder, or the
    // library folder of a placed one).
    let display_of = |paths: &[Option<PathBuf>], fallback: &Path| -> PathBuf {
        let mut common: Option<PathBuf> = None;
        for p in paths.iter().flatten() {
            let dir = p.parent().unwrap_or(p);
            common = Some(match common {
                None => dir.to_path_buf(),
                Some(c) => c.ancestors().find(|a| dir.starts_with(a)).map(Path::to_path_buf).unwrap_or(c),
            });
        }
        common.unwrap_or_else(|| fallback.to_path_buf())
    };
    let reserved: Vec<PathBuf> = if new_placed == Placed::Renamed { per_file.iter().flatten().cloned().collect() } else { Vec::new() };
    Ok(Some(MovePlan {
        id: src.id,
        name: src.name.clone(),
        kind,
        from_save: src.save_path.clone(),
        from_stage_dir: src.stage_dir.clone(),
        to_save: new_save.clone(),
        to_stage_root: new_stage_root,
        to_placed: new_placed,
        to_placement,
        bytes,
        cross_volume: cross,
        dest_dev,
        pin,
        layout_old,
        layout_new,
        stop_at,
        reserved,
        from_display: display_of(&cur_final, &src.save_path),
        to_display: display_of(&per_file, &new_save),
        auto: false,
        dst,
    }))
}

/// Hash input of a relocation: the torrent's piece hashes.
pub(crate) fn piece_hashes(sh: &Shared, id: &TorrentId) -> Result<Arc<[u8]>, String> {
    let bytes = sh.store.load_torrent_file(id).map_err(|e| e.to_string())?.ok_or("the stored .torrent file is missing")?;
    let mi = Metainfo::from_bytes(&bytes).map_err(|e| e.to_string())?;
    Ok(Arc::from(mi.piece_hashes()))
}
