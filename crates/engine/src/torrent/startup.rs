//! Loading, validating and (if needed) rechecking a torrent before it serves peers.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use trs_metainfo::Metainfo;
use trs_storage::{
    FileFingerprint, FilePath, FileSpec, FileStatus, Layout, Location, Mode, RecheckOptions,
    TorrentConfig,
};
use trs_wire::Bitfield;

use super::{Meta, ReadyInfo, Torrent, TorrentCmd, TorrentMsg, TorrentReport};
use crate::peer::{DiskCtx, SharedHave};
use crate::persist::{Placement, ResumeRecord};
use crate::picker::{Picker, piece_priorities};

/// How startup ended.
pub(super) enum Flow {
    Run,
    Exit,
}

/// Why a torrent could not start.
pub(super) enum Fail {
    /// Data is gone (unplugged volume, deleted files): nothing is rewritten.
    Missing(String),
    /// The disk cannot take what is still to come (full, read-only, not permitted); may clear.
    Blocked(String),
    /// Anything else.
    Error(String),
}

/// Inputs of startup that are not needed afterwards.
pub(crate) struct StartParams {
    pub resume: Option<ResumeRecord>,
    pub fresh: bool,
    pub force_recheck: bool,
    pub staged: bool,
    pub file_prios: Vec<u8>,
    pub has_metadata: bool,
    pub first: Vec<TorrentMsg>,
    /// Files that exist under two edge names (`edgefix`): settled before anything is trusted.
    pub edge_conflicts: Vec<crate::edgefix::Conflict>,
}

/// Layout for a parsed torrent: multi-file torrents live below a directory named like the torrent.
/// Files with recorded per-file paths get their own absolute path.
pub(crate) fn build_layout(mi: &Metainfo, placement: Option<&Placement>) -> Result<Layout, String> {
    let multi = mi.is_multi_file();
    let files = mi.files().map(|f| {
        let custom = placement.and_then(|p| p.files.get(f.index)).and_then(|d| d.as_ref());
        let path = match custom {
            Some(abs) if !f.is_padding() => FilePath::Absolute(abs.clone()),
            _ => {
                let mut comps: Vec<String> = Vec::new();
                if multi {
                    comps.push(mi.name.to_string());
                }
                comps.extend(f.components().map(str::to_owned));
                FilePath::Relative(comps)
            }
        };
        FileSpec { path, length: f.length, is_padding: f.is_padding() }
    });
    Layout::new(mi.piece_length, mi.total_length, mi.piece_count() as u32, files)
        .map_err(|e| format!("cannot build the file layout: {e:?}"))
}

/// Zero-length files get no piece and so no write; create them so the folder is complete.
fn create_empty_files(mi: &Metainfo, handle: &trs_storage::TorrentHandle) -> std::io::Result<()> {
    for f in mi.files() {
        if f.length != 0 || f.is_padding() || f.is_symlink() {
            continue;
        }
        let Ok(p) = handle.file_path(f.index as u32, Location::Final) else { continue };
        if p.exists() {
            continue;
        }
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&p)?;
    }
    Ok(())
}

impl Torrent {
    /// A file exists under its clean name and its hidden one. Check the torrent's pieces against
    /// each and keep the better one (ties keep the recorded path) at the recorded path; the other
    /// is set aside, never deleted. `false`: the actor was told to exit meanwhile.
    async fn settle_edge_conflicts(
        &mut self,
        mi: &Metainfo,
        hashes: &Arc<[u8]>,
        conflicts: &[crate::edgefix::Conflict],
    ) -> Result<bool, Fail> {
        let Some(pl) = self.placement.clone() else { return Ok(true) };
        for c in conflicts {
            let mut other = pl.clone();
            if let Some(f) = other.files.get_mut(c.file) {
                *f = Some(c.alt.clone());
            }
            let mut valid = [0u32; 2];
            for (k, p) in [&pl, &other].into_iter().enumerate() {
                let layout = Arc::new(build_layout(mi, Some(p)).map_err(Fail::Error)?);
                let h = self.sh.disk.add_torrent(
                    layout,
                    TorrentConfig {
                        root: self.save_path.clone(),
                        staging: None,
                        mode: Mode::Seed,
                        location: Location::Final,
                        preallocate: false,
                    },
                );
                let opts = RecheckOptions::new(hashes.clone());
                let cancel = opts.cancel.clone();
                let pending = h.recheck(opts).map_err(|e| Fail::Error(e.to_string()))?;
                match self.interruptible(pending).await {
                    None => {
                        cancel.cancel();
                        return Ok(false);
                    }
                    Some(Ok(rep)) => valid[k] = rep.valid,
                    Some(Err(e)) => return Err(Fail::Error(e.to_string())),
                }
            }
            crate::edgefix::settle(c, valid[0] >= valid[1]).map_err(|e| Fail::Error(e.to_string()))?;
        }
        Ok(true)
    }

    /// Await `f`, but stay responsive to shutdown. `None` means the actor must exit.
    pub(super) async fn interruptible<F: Future>(&mut self, f: F) -> Option<F::Output> {
        tokio::pin!(f);
        loop {
            tokio::select! {
                r = &mut f => return Some(r),
                m = self.rx.recv() => match m {
                    None => return None,
                    Some(TorrentMsg::Cmd(TorrentCmd::Shutdown(reply))) => {
                        self.shutting_down = true;
                        let _ = reply.send(self.shutdown_info());
                        return None;
                    }
                    Some(TorrentMsg::AddPeers(v)) => {
                        for a in v {
                            self.pool.add(a);
                        }
                    }
                    Some(TorrentMsg::Cmd(TorrentCmd::SetFilePriorities(p))) => {
                        self.pending_prios = Some(p);
                    }
                    Some(TorrentMsg::Cmd(TorrentCmd::SwitchLocation(sw))) => {
                        self.pending_switch = Some(sw);
                    }
                    Some(TorrentMsg::Cmd(TorrentCmd::Detail(reply))) => {
                        let _ = reply.send(self.detail());
                    }
                    // Sockets that arrive before we can serve them are simply closed.
                    Some(_) => {}
                }
            }
        }
    }

    pub(super) async fn startup(&mut self) -> Result<Flow, Fail> {
        let Some(mut sp) = self.start.take() else {
            return Err(Fail::Error("torrent started twice".into()));
        };
        let first = std::mem::take(&mut sp.first);
        if !sp.has_metadata {
            self.meta_fetch = Some(super::MetaFetch::new());
            self.after_ready(first).await;
            return Ok(Flow::Run);
        }
        self.live.verifying.store(true, Ordering::Relaxed);
        self.checking = true;
        // At most `STARTUP_SLOTS` torrents load and validate at once; a recheck (which can take
        // minutes) is waited for without holding the slot, so a Cold torrent that wakes for a
        // peer is never queued behind big rechecks.
        let mut slot = match self.interruptible(self.sh.startup_slot()).await {
            None => return Ok(Flow::Exit),
            Some(s) => s,
        };
        let store = self.sh.store.clone();
        let id = self.id;
        let parse = tokio::task::spawn_blocking(move || -> Result<Metainfo, String> {
            let bytes = store
                .load_torrent_file(&id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "the stored .torrent file is missing".to_string())?;
            Metainfo::from_bytes(&bytes).map_err(|e| e.to_string())
        });
        let mi = match self.interruptible(parse).await {
            None => return Ok(Flow::Exit),
            Some(Ok(Ok(mi))) => mi,
            Some(Ok(Err(e))) => return Err(Fail::Error(e)),
            Some(Err(e)) => return Err(Fail::Error(e.to_string())),
        };
        if mi.info_hash.swarm_id() != Some(self.id) {
            return Err(Fail::Error("the stored .torrent does not match its id".into()));
        }
        if mi.piece_count() == 0 {
            return Err(Fail::Error("BitTorrent v2-only torrents are not supported yet".into()));
        }
        let layout = Arc::new(build_layout(&mi, self.placement.as_ref()).map_err(Fail::Error)?);
        let hashes: Arc<[u8]> = Arc::from(mi.piece_hashes());
        if !sp.edge_conflicts.is_empty() && !self.settle_edge_conflicts(&mi, &hashes, &sp.edge_conflicts).await? {
            return Ok(Flow::Exit);
        }
        let piece_count = mi.piece_count() as u32;
        let mut staged = sp.staged;
        // Staged, but `.incomplete` is gone: the move finished (the process died before the
        // record said so) or the folder was removed. Either way look at the final place.
        if staged && !sp.fresh && self.staging.as_ref().is_some_and(|s| !s.exists()) {
            staged = false;
        }
        let handle = self.sh.disk.add_torrent(
            layout.clone(),
            TorrentConfig {
                root: self.save_path.clone(),
                staging: self.staging.clone().filter(|_| staged),
                mode: Mode::Download,
                location: Location::Final,
                preallocate: self.cfg.preallocate,
            },
        );

        let mut have: Option<Bitfield> = None;
        let mut fresh_fingerprints = false;
        if let (Some(r), false, false) = (sp.resume.as_ref(), sp.fresh, sp.force_recheck) {
            if staged {
                handle.set_location(Location::Staging);
            }
            have = self.validate_resume(&handle, &layout, r, piece_count).await?;
            if have.is_some() && r.complete {
                self.fps = Some(r.fingerprints.clone());
            }
            if have.is_none() && self.shutting_down {
                return Ok(Flow::Exit);
            }
        }
        if have.is_none() {
            // Fresh add (data may already exist: cross-seeding), forced recheck, or stale resume.
            handle.set_location(Location::Final);
            // The startup slot is not held while waiting for a hashing slot (a Cold torrent that
            // wakes for a peer must not queue behind rechecks).
            slot = None;
            self.live.verify_wait.store(true, Ordering::Relaxed);
            let permit = self.interruptible(self.sh.verify_slot()).await;
            self.live.verify_wait.store(false, Ordering::Relaxed);
            let Some(Some(permit)) = permit else { return Ok(Flow::Exit) };
            let in_use = self.cfg.verify_slots.max(1).saturating_sub(self.sh.verify_slots.available_permits()) as u64;
            self.sh.diag.verify_peak.fetch_max(in_use, Ordering::Relaxed);
            self.sh.diag.rechecks.fetch_add(1, Ordering::Relaxed);
            let opts = RecheckOptions::new(hashes.clone());
            let cancel = opts.cancel.clone();
            let pending = handle.recheck(opts).map_err(|e| Fail::Error(e.to_string()))?;
            let rep = match self.interruptible(pending).await {
                None => {
                    cancel.cancel();
                    return Ok(Flow::Exit);
                }
                Some(Ok(rep)) => rep,
                Some(Err(e)) => return Err(Fail::Error(e.to_string())),
            };
            drop(permit);
            let mut bf = Bitfield::new(piece_count);
            for i in rep.have.iter_ones() {
                bf.set(i);
            }
            if staged {
                if rep.valid == 0 {
                    handle.set_location(Location::Staging);
                } else {
                    // Data already lives at its final place: keep working there.
                    staged = false;
                    handle.set_paths(self.save_path.clone(), None);
                }
            }
            fresh_fingerprints = true;
            have = Some(bf);
            self.resume_dirty = true;
        }
        let have = have.unwrap_or_else(|| Bitfield::new(piece_count));
        drop(slot);
        self.live.verifying.store(false, Ordering::Relaxed);
        self.checking = false;
        let complete = have.is_complete();

        // File priorities -> piece priorities.
        let nfiles = layout.file_count();
        let mut file_prio = sp.file_prios.clone();
        file_prio.resize(nfiles, 2);
        let prio = piece_priorities(
            mi.piece_length,
            piece_count,
            (0..nfiles).map(|i| (layout.file_start(i), layout.file_len(i), file_prio[i], layout.file_is_padding(i))),
        );
        // Resumed or re-added: the bytes still to come must fit where they are written. Say so now,
        // on the row, instead of failing in the middle of a write.
        if !complete {
            let need: u64 = (0..piece_count)
                .filter(|&i| prio[i as usize] > 0 && !have.has(i))
                .map(|i| u64::from(piece_size(&mi, i)))
                .sum();
            // Unfinished data is written to staging (when staged) and ends up in the save folder: both
            // volumes must hold what is still to come (one volume is checked once, by the same number).
            let mut places = vec![self.save_path.clone()];
            if let (Some(stage), true) = (&self.staging, staged) {
                places.insert(0, stage.clone());
            }
            for dest in places {
                if let Some(msg) = space_shortfall(self.cfg.smart_add.free_space_override, &dest, need) {
                    return Err(Fail::Blocked(msg));
                }
            }
        }
        let max_open = (self.cfg.write_cache_bytes / mi.piece_length.max(1) as usize).clamp(8, 512);
        let picker = (!complete).then(|| {
            let mut seed = [0u8; 8];
            seed.copy_from_slice(&self.id[8..16]);
            Picker::new(mi.piece_length, mi.total_length, have.clone(), prio, max_open, u64::from_le_bytes(seed))
        });
        let wanted_done = picker.as_ref().is_none_or(Picker::wanted_done);
        let left = mi.total_length - have.iter_ones().map(|i| u64::from(piece_size(&mi, i))).sum::<u64>();

        let shared_have = SharedHave::new(piece_count);
        for i in have.iter_ones() {
            shared_have.set(i);
        }
        let disk_ctx = DiskCtx {
            handle: handle.clone(),
            have: shared_have,
            piece_count,
            piece_len: mi.piece_length,
            total_len: mi.total_length,
            layout: layout.clone(),
        };
        if self.ctx.disk.set(disk_ctx).is_err() {
            return Err(Fail::Error("storage attached twice".into()));
        }
        if complete {
            handle.set_mode(Mode::Seed);
            self.ctx.complete.store(true, Ordering::Relaxed);
        } else {
            handle.attach_hashes(hashes.clone()).map_err(|e| Fail::Error(e.to_string()))?;
            // Late duplicate blocks for pieces we already hold must not open fresh buffers.
            handle.set_verified(have.iter_ones());
        }
        if !complete || !staged {
            if !complete {
                handle.ensure_root().map_err(|e| match e {
                    trs_storage::StorageError::MissingRoot => {
                        Fail::Missing("the save location is not available".into())
                    }
                    other => Fail::Error(other.to_string()),
                })?;
            }
            if !staged {
                create_empty_files(&mi, &handle).map_err(|e| Fail::Error(e.to_string()))?;
            }
        }

        self.counters.set_left(left);
        self.live.done_bytes.store(mi.total_length - left, Ordering::Relaxed);
        self.live.wanted_done.store(wanted_done, Ordering::Relaxed);
        self.ctx
            .metadata_size
            .store(mi.info_range.len() as u32, Ordering::Relaxed);
        let announce: Vec<Vec<String>> = mi
            .announce_tiers
            .iter()
            .map(|t| t.iter().map(|u| u.to_string()).collect())
            .collect();
        let wanted_length: u64 = (0..nfiles)
            .filter(|&i| !layout.file_is_padding(i) && file_prio[i] > 0)
            .map(|i| layout.file_len(i))
            .sum();
        self.counters.set_wanted_done(wanted_length.saturating_sub(picker.as_ref().map_or(0, Picker::wanted_missing_bytes)));
        self.meta = Some(Meta {
            wanted_len: wanted_length,
            layout: layout.clone(),
            comment: mi.comment.as_deref().map(str::to_owned),
            piece_count,
            piece_len: mi.piece_length,
            total_len: mi.total_length,
            file_prio,
            picker,
            hashes: (!complete).then_some(hashes),
            staged,
        });
        self.downloading = !wanted_done && self.download_allowed;

        self.report(TorrentReport::Ready(Box::new(ReadyInfo {
            name: mi.name.to_string(),
            total_length: mi.total_length,
            wanted_length,
            piece_length: mi.piece_length,
            private: mi.private,
            complete,
            wanted_done,
            announce,
            staged,
        })));
        if fresh_fingerprints && complete {
            self.spawn_fingerprints();
        }
        self.live.ready.store(true, Ordering::Relaxed);
        self.after_ready(first).await;
        Ok(Flow::Run)
    }

    /// Resume data accepted? `Ok(None)` means "recheck instead".
    async fn validate_resume(
        &mut self,
        handle: &trs_storage::TorrentHandle,
        layout: &Arc<Layout>,
        r: &ResumeRecord,
        piece_count: u32,
    ) -> Result<Option<Bitfield>, Fail> {
        let nfiles = layout.file_count();
        if r.complete {
            let fps: Vec<Option<FileFingerprint>> = r
                .fingerprints
                .iter()
                .map(|f| f.as_ref().and_then(|b| FileFingerprint::from_bytes(&b[..])))
                .collect();
            if fps.len() != nfiles {
                return Ok(None);
            }
            let pending = handle.check_files(fps).map_err(|e| Fail::Error(e.to_string()))?;
            let Some(rep) = self.interruptible(pending).await else {
                self.shutting_down = true;
                return Ok(None);
            };
            let rep = rep.map_err(|e| Fail::Error(e.to_string()))?;
            let mut modified = false;
            for (i, st) in rep.status.iter().enumerate() {
                let empty = layout.file_len(i) == 0;
                match st {
                    FileStatus::Ok => {}
                    FileStatus::Missing if empty => {}
                    FileStatus::NoRecord if empty => {}
                    FileStatus::Missing => {
                        return Err(Fail::Missing(format!("file {} is missing", display_path(layout, i))));
                    }
                    _ => modified = true,
                }
            }
            if modified {
                return Ok(None);
            }
            let mut bf = Bitfield::new(piece_count);
            bf.set_all();
            return Ok(Some(bf));
        }
        let Ok(bf) = Bitfield::from_wire(&r.bitfield, piece_count) else {
            return Ok(None);
        };
        let pending = handle.check_files(Vec::new()).map_err(|e| Fail::Error(e.to_string()))?;
        let Some(rep) = self.interruptible(pending).await else {
            self.shutting_down = true;
            return Ok(None);
        };
        let rep = rep.map_err(|e| Fail::Error(e.to_string()))?;
        for (i, st) in rep.status.iter().enumerate() {
            if *st == FileStatus::Missing
                && layout.file_len(i) > 0
                && layout.pieces_of_file(i).any(|p| bf.has(p))
            {
                // Pieces we claim to have lived in a file that is gone.
                return Ok(None);
            }
        }
        Ok(Some(bf))
    }

    /// Fingerprint the files (size + mtime) in the background and send them back as resume data.
    pub(super) fn spawn_fingerprints(&self) {
        let Some(d) = self.ctx.disk.get() else { return };
        let Ok(p) = d.handle.fingerprints() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(v) = p.await {
                let enc = v.into_iter().map(|f| f.map(|f| f.to_bytes())).collect();
                let _ = tx.send(TorrentMsg::Fingerprints(enc)).await;
            }
        });
    }

}

/// `Some(message)` when `need` bytes do not fit on the volume holding `dest`. `override_free` is the
/// test hook of `smart_add.free_space_override`.
pub(crate) fn space_shortfall(override_free: Option<u64>, dest: &std::path::Path, need: u64) -> Option<String> {
    if need == 0 {
        return None;
    }
    let free = override_free.or_else(|| trs_storage::available_space(dest).ok())?;
    (need > free).then(|| {
        format!("Not enough space: {} still to download, {} free", human_bytes(need), human_bytes(free))
    })
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn piece_size(mi: &Metainfo, i: u32) -> u32 {
    mi.piece_size(i as usize).unwrap_or(0)
}

pub(super) fn display_path(layout: &Layout, i: usize) -> String {
    match layout.file_path(i) {
        trs_storage::PathRef::Relative(p) | trs_storage::PathRef::Absolute(p) => p.to_string_lossy().into_owned(),
    }
}
