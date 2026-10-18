//! Moving files after a location changed (SPEC 7A.5).
//!
//! The manager is a queue in the session actor. A job is only `(torrent, target)`; its plan
//! (`moveplan::plan_move`) is computed when its turn comes, off the actor, from the stored
//! `.torrent`, so a queue of hundreds of torrents costs a few dozen bytes each.
//!
//! ```text
//! queue --> plan (blocking) --> launch ----------------------------> exec task ----------------------> done
//!                               (one cross-volume copy per          relocate_prepare   (journaled; verified copy,
//!                                destination volume; downloads      -- MoveSwitch --> session persists the new
//!                                are stopped while their files       record, tells the actor (SwitchLocation)
//!                                move)                              mark_switched, relocate_commit (sources go)
//! ```
//!
//! The source of a copy is deleted only after the session persisted a record that points at the
//! destination and the running actor switched its layout. A crash leaves a journal in `moves/`;
//! `recover_moves` resolves it at the next start by comparing the journal's plan with the record.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use trs_metainfo::Metainfo;
use trs_storage::{CancelToken, Location, Mode, PieceSet, RecoverState, RelocateOptions, StorageError, TorrentConfig};

use super::cold::*;
use super::{RemoveWait, Session, SessionMsg};
use crate::api::*;
use crate::moveplan::{MovePlan, MoveSrc, MoveTarget, plan_move, piece_hashes};
use crate::organize;
use crate::persist::{Batch, Placed};
use crate::shared::Shared;
use crate::torrent::{Switch, TorrentCmd, TorrentMsg};

/// Moves planned or running at once (each running one occupies a storage maintenance thread).
pub(crate) const MAX_PARALLEL_MOVES: usize = 3;
/// A torrent that is still finishing is tried again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// How often a finishing torrent is retried before giving up.
const MAX_TRIES: u8 = 30;

/// Progress of a running move, shared with the exec task.
#[derive(Default)]
pub(crate) struct MoveProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
}

/// A queued move: only the torrent and where it goes (the plan comes later).
pub(crate) struct MoveJob {
    pub id: TorrentId,
    pub target: MoveTarget,
    pub tries: u8,
    /// Started by the engine itself (the hourly status check), not by the user: a failure is
    /// reported once per reason and the move is tried again only when its cheap preflight passes.
    pub auto: bool,
}

/// How an exec task ended.
pub(crate) enum ExecEnd {
    /// The files are at the new place and the old ones are gone.
    Done,
    /// Nothing changed (or was rolled back); the text says why.
    Failed(String),
    /// Test hook: the process "crashed" here; leave everything as it is.
    Crashed,
}

struct Active {
    progress: Arc<MoveProgress>,
    cancel: CancelToken,
    plan: Arc<MovePlan>,
    auto: bool,
}

/// Session-side state of the move manager. Empty (no allocation) while nothing moves.
#[derive(Default)]
pub(crate) struct Mover {
    active: HashMap<TorrentId, Active>,
    /// Torrents that must not run right now (their files are in the middle of a move).
    pub blocked: HashSet<TorrentId>,
    queue: VecDeque<MoveJob>,
    queued: HashSet<TorrentId>,
    /// Plans waiting for their destination volume to be free of another copy.
    parked: Vec<MovePlan>,
    /// Destination device -> the torrent copying to it.
    copying: HashMap<u64, TorrentId>,
    /// Jobs being planned right now.
    planning: usize,
    /// Downloading torrents whose actor is stopping before their files move.
    pending_exit: HashMap<TorrentId, Arc<MovePlan>>,
    /// `remove` calls that arrived while the torrent was moving.
    deferred_remove: HashMap<TorrentId, (bool, Option<Arc<RemoveWait>>)>,
    /// Automatic moves that failed, with the reason that was reported. The torrent keeps seeding
    /// where it is; the reason is shown on its row until a move works or none is due.
    pub auto_failed: HashMap<TorrentId, String>,
}

impl Mover {
    /// The torrent's files are in the middle of a move (or its journal is being resolved): no
    /// actor may start for it.
    pub(crate) fn blocks(&self, id: &TorrentId) -> bool {
        self.blocked.contains(id)
    }

    /// Everything that is moving or waiting (for shutdown).
    pub(crate) fn cancel_all(&self) {
        for a in self.active.values() {
            a.cancel.cancel();
        }
    }
}

fn journal_path(dir: &Path, id: &TorrentId) -> PathBuf {
    dir.join(format!("{}.journal", crate::util::hex(id)))
}

/// "Not allowed" errors in words, naming the file or folder instead of a file number.
fn denied_text(file: Option<u32>, layout: Option<&trs_storage::Layout>, dest: &Path) -> String {
    match (file, layout) {
        (Some(n), Some(l)) if (n as usize) < l.file_count() => {
            let name = match l.file_path(n as usize) {
                trs_storage::PathRef::Relative(p) | trs_storage::PathRef::Absolute(p) => trs_storage::leaf(Path::new(p)),
            };
            format!("No permission to change \u{201c}{name}\u{201d}, so the torrent stays where it is")
        }
        _ => format!("No permission to change \u{201c}{}\u{201d}", trs_storage::leaf(dest)),
    }
}

fn fail_text(e: &StorageError, dest: &Path, layout: Option<&trs_storage::Layout>) -> String {
    match e {
        StorageError::Locked { .. } => format!("{e}, so the torrent stays where it is"),
        StorageError::NoPermission { .. } => e.to_string(),
        StorageError::Io { file, source } if source.raw_os_error().is_some_and(|c| c == libc::EPERM || c == libc::EACCES) => {
            denied_text(*file, layout, dest)
        }
        StorageError::InsufficientSpace { needed, available } => format!(
            "Not enough free space on {}: {} needed, {} available",
            dest.display(),
            crate::util::human_bytes(*needed),
            crate::util::human_bytes(*available)
        ),
        StorageError::VerifyFailed { piece } => {
            format!("The copy did not match the torrent (piece {piece}); the original was left where it was")
        }
        StorageError::DestinationExists { .. } => "A file with that name already exists at the destination".into(),
        StorageError::Cancelled => "Cancelled".into(),
        other => other.to_string(),
    }
}

impl Session {
    /// The torrent's actor must not (re)start now: its files are being moved.
    pub(crate) fn mover_blocks(&self, id: &TorrentId) -> bool {
        self.mover.blocked.contains(id)
    }

    /// Progress 0..=1 of a running move.
    pub(crate) fn moving_progress(&self, id: &TorrentId) -> Option<f32> {
        let a = self.mover.active.get(id)?;
        let total = a.progress.total.load(Ordering::Relaxed);
        Some(if total == 0 { 0.0 } else { (a.progress.done.load(Ordering::Relaxed) as f64 / total as f64).clamp(0.0, 1.0) as f32 })
    }

    /// Is a move queued, planned or running for this torrent?
    pub(crate) fn is_moving(&self, id: &TorrentId) -> bool {
        self.mover.active.contains_key(id) || self.mover.queued.contains(id) || self.mover.parked.iter().any(|p| p.id == *id)
    }

    fn moves_dir(&self) -> PathBuf {
        self.sh.store.moves_dir().unwrap_or_else(|| std::env::temp_dir().join(format!("trs-moves-{}", std::process::id())))
    }

    /// Why this torrent cannot be moved now.
    fn check_movable(&self, id: &TorrentId) -> Result<(), String> {
        if self.directs.contains_key(id) {
            return Err("direct downloads are not moved".into());
        }
        let e = self.table.get(id).ok_or("unknown torrent")?;
        let c = &e.cold;
        if !c.has_metadata() {
            return Err("metadata not available yet".into());
        }
        if c.awaiting() {
            return Err("waiting for a choice".into());
        }
        if self.is_moving(id) {
            return Err("already moving".into());
        }
        if c.want == Want::Error && c.has2(G_MISSING) {
            return Err("its files are missing".into());
        }
        // Between "all wanted data is in" and "the files reached their place" the data is in motion.
        if c.has(F_STAGED) && c.finished() {
            return Err(FINISHING.into());
        }
        Ok(())
    }

    fn move_src(&self, id: &TorrentId) -> Result<MoveSrc, String> {
        self.check_movable(id)?;
        let c = &self.table[id].cold;
        Ok(MoveSrc {
            id: *id,
            name: c.name.to_string(),
            opts: self.opts_of(c),
            placed: c.placed(),
            staged: c.has(F_STAGED),
            save_path: self.save_path(c),
            stage_dir: self.staging_for(c),
            pinned: c.has2(G_PINNED),
            live_folder: self.live_status_folder(id),
        })
    }

    // ---- commands ---------------------------------------------------------------------------

    /// Queue moves of `ids` to `target`; torrents that cannot be moved now are reported.
    pub(crate) fn queue_moves(&mut self, ids: Vec<TorrentId>, target: MoveTarget) -> MoveReport {
        let mut rep = MoveReport::default();
        let auto = matches!(target, MoveTarget::Status(_));
        for id in ids {
            match self.check_movable(&id) {
                Err(why) if why == FINISHING && matches!(target, MoveTarget::Config) => {
                    self.retry_move_later(MoveJob { id, target: target.clone(), tries: 0, auto });
                    rep.queued.push(id);
                }
                Err(why) => rep.refused.push((id, why)),
                Ok(()) => {
                    self.mover.queued.insert(id);
                    self.mover.queue.push_back(MoveJob { id, target: target.clone(), tries: 0, auto });
                    rep.queued.push(id);
                }
            }
        }
        self.pump_moves();
        rep
    }

    /// An automatic move failed: remember why, and say so only when the reason is new. The
    /// torrent keeps seeding where it is, so there is nothing to do but try again later.
    fn auto_move_failed(&mut self, id: TorrentId, reason: String) {
        if self.mover.auto_failed.get(&id) == Some(&reason) {
            return;
        }
        self.mover.auto_failed.insert(id, reason.clone());
        self.sh.emit(EngineEvent::MoveFailed { id, reason });
    }

    fn retry_move_later(&mut self, job: MoveJob) {
        if job.tries >= MAX_TRIES {
            self.sh.emit(EngineEvent::MoveFailed { id: job.id, reason: "the download did not finish in time to be moved".into() });
            return;
        }
        let tx = self.sh.session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RETRY_AFTER).await;
            let _ = tx.send(SessionMsg::MoveRetry(MoveJob { tries: job.tries + 1, ..job })).await;
        });
    }

    pub(super) fn on_move_retry(&mut self, job: MoveJob) {
        if self.shutting_down {
            return;
        }
        match self.check_movable(&job.id) {
            Ok(()) => {
                self.mover.queued.insert(job.id);
                self.mover.queue.push_back(job);
                self.pump_moves();
            }
            Err(why) if why == FINISHING => self.retry_move_later(job),
            Err(_) => {}
        }
    }

    /// The torrents the current settings would move, planned without moving anything.
    pub(super) fn propose_moves(&mut self, reply: oneshot::Sender<MoveProposal>) {
        let mut srcs = Vec::new();
        for id in self.table.keys().copied().collect::<Vec<_>>() {
            if let Ok(s) = self.move_src(&id)
                && !s.pinned {
                    srcs.push(s);
                }
        }
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            let mut p = MoveProposal::default();
            for s in &srcs {
                if let Ok(Some(plan)) = plan_move(&sh, s, &MoveTarget::Config) {
                    p.total_bytes = p.total_bytes.saturating_add(plan.bytes);
                    p.cross_volume += u32::from(plan.cross_volume);
                    p.items.push(MoveItem {
                        id: plan.id,
                        name: plan.name.clone(),
                        bytes: plan.bytes,
                        from: plan.from_display.clone(),
                        to: plan.to_display.clone(),
                        cross_volume: plan.cross_volume,
                        kind: plan.kind,
                    });
                }
            }
            p.items.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
            let _ = reply.send(p);
        });
    }

    /// A change of the settings may have left torrents elsewhere than they now belong: say so.
    pub(super) fn announce_location_changes(&mut self) {
        let (tx, rx) = oneshot::channel();
        self.propose_moves(tx);
        let sh = self.sh.clone();
        tokio::spawn(async move {
            if let Ok(p) = rx.await
                && !p.items.is_empty() {
                    sh.emit(EngineEvent::LocationsChanged { torrents: p.items.len() as u32, bytes: p.total_bytes });
                }
        });
    }

    pub(super) fn cancel_moves(&mut self, ids: Vec<TorrentId>) {
        for id in ids {
            self.mover.queue.retain(|j| j.id != id);
            self.mover.queued.remove(&id);
            if let Some(pos) = self.mover.parked.iter().position(|p| p.id == id) {
                let p = self.mover.parked.remove(pos);
                crate::organize::release_paths(&self.sh.reserved, &p.reserved);
                self.sh.emit(EngineEvent::MoveFailed { id, reason: "Cancelled".into() });
            }
            if let Some(a) = self.mover.active.get(&id) {
                a.cancel.cancel();
            }
        }
    }

    /// A `remove` for a torrent that is moving waits for the move to end (a copy in progress is
    /// cancelled and cleaned up first).
    pub(super) fn defer_remove_if_moving(&mut self, id: TorrentId, delete_files: bool, wait: Option<Arc<RemoveWait>>) -> Option<Option<Arc<RemoveWait>>> {
        if !self.mover.active.contains_key(&id) && !self.mover.pending_exit.contains_key(&id) {
            self.mover.queue.retain(|j| j.id != id);
            self.mover.queued.remove(&id);
            if let Some(pos) = self.mover.parked.iter().position(|p| p.id == id) {
                let p = self.mover.parked.remove(pos);
                crate::organize::release_paths(&self.sh.reserved, &p.reserved);
            }
            return Some(wait);
        }
        if let Some(a) = self.mover.active.get(&id) {
            a.cancel.cancel();
        }
        self.mover.deferred_remove.insert(id, (delete_files, wait));
        None
    }

    // ---- scheduling -------------------------------------------------------------------------

    pub(crate) fn pump_moves(&mut self) {
        while !self.shutting_down && self.mover.planning + self.mover.active.len() < MAX_PARALLEL_MOVES {
            let Some(job) = self.mover.queue.pop_front() else { break };
            self.mover.queued.remove(&job.id);
            self.begin_planning(job);
        }
    }

    fn begin_planning(&mut self, job: MoveJob) {
        let src = match self.move_src(&job.id) {
            Ok(s) => s,
            Err(why) if why == FINISHING => return self.retry_move_later(job),
            Err(why) => {
                self.sh.emit(EngineEvent::MoveFailed { id: job.id, reason: why });
                return;
            }
        };
        self.mover.planning += 1;
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            let mut result = plan_move(&sh, &src, &job.target).map(|o| o.map(Box::new));
            // The cheap preflight, before anything is announced or touched.
            if job.auto
                && let Ok(Some(plan)) = &result
                    && let Err(e) = plan.preflight() {
                        let dest = plan.dst.iter().flatten().next().and_then(|p| p.parent()).unwrap_or(&plan.to_save);
                        let why = fail_text(&e, dest, Some(&plan.layout_old));
                        crate::organize::release_paths(&sh.reserved, &plan.reserved);
                        result = Err(why);
                    }
            let _ = sh.session.blocking_send(SessionMsg::MovePlanned { job, result });
        });
    }

    pub(super) fn on_move_planned(&mut self, job: MoveJob, result: Result<Option<Box<MovePlan>>, String>) {
        self.mover.planning = self.mover.planning.saturating_sub(1);
        let id = job.id;
        match result {
            Err(reason) if job.auto => self.auto_move_failed(id, reason),
            Err(reason) => self.sh.emit(EngineEvent::MoveFailed { id, reason }),
            Ok(None) => {
                self.mover.auto_failed.remove(&id);
            }
            Ok(Some(mut plan)) => {
                plan.auto = job.auto;
                // The torrent may have changed while the plan was made.
                let still_there = self.table.get(&id).is_some_and(|e| !e.cold.awaiting() && self.save_path(&e.cold) == plan.from_save);
                if !still_there || self.mover.active.contains_key(&id) || self.shutting_down {
                    crate::organize::release_paths(&self.sh.reserved, &plan.reserved);
                } else if plan.cross_volume && self.mover.copying.contains_key(&plan.dest_dev) {
                    // One copy per destination volume at a time.
                    {
                        let mut r = self.sh.reserved.lock().unwrap_or_else(|e| e.into_inner());
                        r.extend(plan.reserved.iter().cloned());
                    }
                    self.mover.parked.push(*plan);
                } else {
                    self.launch_move(*plan);
                }
            }
        }
        self.pump_moves();
    }

    fn launch_move(&mut self, plan: MovePlan) {
        let id = plan.id;
        let auto = plan.auto;
        {
            let mut r = self.sh.reserved.lock().unwrap_or_else(|e| e.into_inner());
            r.extend(plan.reserved.iter().cloned());
        }
        let plan = Arc::new(plan);
        let progress = Arc::new(MoveProgress { done: AtomicU64::new(0), total: AtomicU64::new(plan.bytes) });
        let cancel = CancelToken::new();
        if plan.cross_volume {
            self.mover.copying.insert(plan.dest_dev, id);
        }
        self.mover.active.insert(id, Active { progress, cancel, plan: plan.clone(), auto });
        // Players end cleanly before the files go elsewhere (a stream does not survive a move).
        self.close_streams(&id);
        self.sh.emit(EngineEvent::MoveStarted {
            id,
            from: plan.from_display.clone(),
            to: plan.to_display.clone(),
            bytes: plan.bytes,
            cross_volume: plan.cross_volume,
        });
        let Some(e) = self.table.get(&id) else { return };
        // A torrent that is still downloading is paused while its files move (SPEC 7A.5). One
        // that has everything it wants but skips files can still start writing when the user
        // ticks one: a copy takes long enough for that, so it is stopped too; a rename is
        // short, and the running actor's writes are held instead (`exec`).
        let downloading = !e.cold.complete() && (!e.cold.has(F_WANTED_DONE) || plan.cross_volume);
        if plan.kind != MoveKind::Retarget && downloading {
            self.mover.blocked.insert(id);
            if e.live.is_some() {
                self.mover.pending_exit.insert(id, plan.clone());
                self.stop_live(id, super::After::Nothing);
                return;
            }
            self.start_exec(plan, None);
            return;
        }
        let live = e.live.as_ref().filter(|l| !l.stopping).map(|l| l.tx.clone());
        if plan.kind != MoveKind::Retarget && !plan.cross_volume && live.is_none() {
            // A rename takes milliseconds; nothing may wake the torrent in the middle of it.
            self.mover.blocked.insert(id);
        }
        self.start_exec(plan, live);
    }

    /// The actor of a downloading torrent has stopped: its files may move now.
    pub(super) fn on_actor_gone_for_move(&mut self, id: TorrentId) {
        if let Some(plan) = self.mover.pending_exit.remove(&id) {
            // The resume record the actor handed over must be readable by the exec task.
            self.flush_persist();
            self.writer.flush();
            self.start_exec(plan, None);
        }
    }

    fn start_exec(&mut self, plan: Arc<MovePlan>, live: Option<crate::chan::Sender<TorrentMsg>>) {
        let Some(a) = self.mover.active.get(&plan.id) else { return };
        let ctx = ExecCtx {
            sh: self.sh.clone(),
            plan: plan.clone(),
            progress: a.progress.clone(),
            cancel: a.cancel.clone(),
            live,
            complete: self.table.get(&plan.id).is_some_and(|e| e.cold.complete()),
            journal: journal_path(&self.moves_dir(), &plan.id),
        };
        let sh = self.sh.clone();
        let id = plan.id;
        tokio::spawn(async move {
            let end = exec(ctx).await;
            let _ = sh.session.send(SessionMsg::MoveDone { id, end }).await;
        });
    }

    /// The exec task asks for the switch: persist the new record, then tell the actor.
    pub(super) fn on_move_switch(&mut self, id: TorrentId, plan: Arc<MovePlan>, reply: oneshot::Sender<Result<(), String>>) {
        let Some(e) = self.table.get(&id) else {
            let _ = reply.send(Err("the torrent was removed".into()));
            return;
        };
        let old = (e.cold.save_root, e.cold.stage_root, e.cold.placed(), e.cold.has2(G_PINNED));
        let save_root = self.intern_root(plan.to_save.clone());
        let stage_root = plan.to_stage_root.clone().map(|p| self.intern_root(p));
        if let Some(e) = self.table.get_mut(&id) {
            let c = &mut e.cold;
            c.save_root = save_root;
            c.stage_root = stage_root.unwrap_or(NONE_U32);
            c.set_placed(plan.to_placed);
            // The user's explicit choice of place keeps later settings changes away; a move that
            // follows the settings leaves the flag as it was.
            if plan.pin {
                c.set2(G_PINNED, true);
            }
        }
        let rec = self.table.get(&id).map(|e| self.record_of(&e.cold, None));
        let placements = match plan.to_placed {
            Placed::Renamed => vec![(id, plan.to_placement.clone())],
            _ if plan.to_placed != Placed::Renamed && old.2 == Placed::Renamed => vec![(id, None)],
            _ => Vec::new(),
        };
        let batch = Batch { torrents: rec.into_iter().collect(), placements, ..Batch::default() };
        if let Err(err) = self.commit_now(batch) {
            // Put the record back: the files are still (or again) where it said.
            if let Some(e) = self.table.get_mut(&id) {
                let c = &mut e.cold;
                c.save_root = old.0;
                c.stage_root = old.1;
                c.set_placed(old.2);
                c.set2(G_PINNED, old.3);
            }
            let _ = reply.send(Err(err.to_string()));
            return;
        }
        self.mark_record(id);
        // From here on the record points at the new place: the move can only go forward. The
        // running actor is told; if it cannot switch (or is gone) it is restarted from the record,
        // and only then does the exec task go on to delete the old files.
        let (atx, arx) = oneshot::channel();
        self.notify_switch_layout(id, plan.layout_new.clone(), Some(atx));
        let tx = self.sh.session.clone();
        tokio::spawn(async move {
            let switched = arx.await.is_ok_and(|r| r.is_ok());
            if !switched {
                let _ = tx.send(SessionMsg::MoveRestartActor(id)).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let _ = reply.send(Ok(()));
        });
    }

    /// The actor could not switch to the moved files: let it restart from the persisted record.
    pub(super) fn on_move_restart_actor(&mut self, id: TorrentId) {
        if self.table.get(&id).is_some_and(|e| e.live.is_some()) {
            self.stop_live(id, super::After::Respawn { force_recheck: false });
        }
    }

    /// Tell a running actor about the new layout (the record is already persisted). `reply` hears
    /// the storage layer's answer, or `Ok` at once when no actor runs.
    pub(crate) fn notify_switch_layout(
        &mut self,
        id: TorrentId,
        layout: Arc<trs_storage::Layout>,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    ) {
        let Some(e) = self.table.get(&id) else {
            if let Some(r) = reply {
                let _ = r.send(Ok(()));
            }
            return;
        };
        let Some(l) = e.live.as_ref().filter(|l| !l.stopping) else {
            if let Some(r) = reply {
                let _ = r.send(Ok(()));
            }
            return;
        };
        let placement = if e.cold.has2(G_PLACED) { self.sh.store.load_placement(&id).ok().flatten() } else { None };
        let sw = Switch::plain(layout, self.save_path(&e.cold), self.staging_for(&e.cold), placement, reply);
        if let Err(err) = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SwitchLocation(Box::new(sw))))
            && let TorrentMsg::Cmd(TorrentCmd::SwitchLocation(sw)) = err.into_inner()
                && let Some(r) = sw.reply {
                    let _ = r.send(Err("the torrent's queue is full".into()));
                }
    }

    pub(super) fn on_move_done(&mut self, id: TorrentId, end: ExecEnd) {
        let active = self.mover.active.remove(&id);
        if let Some(a) = &active {
            if self.mover.copying.get(&a.plan.dest_dev) == Some(&id) {
                self.mover.copying.remove(&a.plan.dest_dev);
            }
            crate::organize::release_paths(&self.sh.reserved, &a.plan.reserved);
        }
        if matches!(end, ExecEnd::Crashed) {
            // A simulated crash: the torrent stays blocked, as a dead process would leave it.
            return;
        }
        self.mover.blocked.remove(&id);
        self.mover.pending_exit.remove(&id);
        match end {
            ExecEnd::Done => {
                self.mover.auto_failed.remove(&id);
                let to = active.as_ref().map(|a| a.plan.to_display.clone()).unwrap_or_default();
                self.sh.emit(EngineEvent::MoveFinished { id, to });
            }
            ExecEnd::Failed(reason) if active.as_ref().is_some_and(|a| a.auto) => self.auto_move_failed(id, reason),
            ExecEnd::Failed(reason) => self.sh.emit(EngineEvent::MoveFailed { id, reason }),
            ExecEnd::Crashed => {}
        }
        self.reconcile(id);
        self.update_error(id);
        if let Some((delete, wait)) = self.mover.deferred_remove.remove(&id) {
            self.remove_waiting(id, delete, wait);
        }
        // A copy to this volume was waiting.
        if let Some(pos) = self.mover.parked.iter().position(|p| !self.mover.copying.contains_key(&p.dest_dev)) {
            let plan = self.mover.parked.remove(pos);
            crate::organize::release_paths(&self.sh.reserved, &plan.reserved);
            self.launch_move(plan);
        }
        self.pump_moves();
    }

    // ---- recovery ----------------------------------------------------------------------------

    /// At start-up: block torrents that have a move journal until it is resolved (`recover_moves`).
    pub(super) fn block_journaled(&mut self) {
        let dir = self.moves_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(hex) = name.strip_suffix(".journal") else { continue };
            if let Some(id) = parse_hex_id(hex)
                && self.table.contains_key(&id) {
                    self.mover.blocked.insert(id);
                }
        }
    }

    /// Resolve the journals of moves that were interrupted (a crash, a kill): see the recovery
    /// matrix in the trs-storage README. The journal's plan is compared with the record: if the
    /// record points at the destination the move is finished (sources deleted), otherwise it is
    /// rolled back and the torrent seeds from where it was.
    pub(super) fn recover_moves(&mut self) {
        let dir = self.moves_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let mut ids = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(hex) = name.strip_suffix(".journal") else { continue };
            let Some(id) = parse_hex_id(hex) else { continue };
            if self.table.contains_key(&id) {
                ids.push(id);
            } else {
                // The torrent is gone; a journal alone cannot be resolved.
                let _ = std::fs::remove_file(e.path());
            }
        }
        for id in ids {
            let Some(e) = self.table.get(&id) else { continue };
            let c = &e.cold;
            let cur = Cur {
                save: self.save_path(c),
                stage_dir: self.staging_for(c),
                placed: c.placed(),
                complete: c.complete(),
            };
            let (sh, journal) = (self.sh.clone(), journal_path(&dir, &id));
            tokio::task::spawn_blocking(move || {
                let r = recover_one(&sh, &id, &cur, &journal);
                let _ = sh.session.blocking_send(SessionMsg::MoveRecovered { id, result: r });
            });
        }
    }

    pub(super) fn on_move_recovered(&mut self, id: TorrentId, result: Result<String, String>) {
        self.mover.blocked.remove(&id);
        match result {
            Ok(_) => {}
            Err(reason) => self.sh.emit(EngineEvent::MoveFailed { id, reason }),
        }
        self.reconcile(id);
    }
}

/// Fault-hook step numbers of the exec task after the switch (the storage steps count from 1).
pub const MOVE_STEP_BEFORE_MARK: u32 = 10_000;
/// See [`MOVE_STEP_BEFORE_MARK`].
pub const MOVE_STEP_BEFORE_COMMIT: u32 = 10_001;

const FINISHING: &str = "the download is still finishing; try again in a moment";

fn parse_hex_id(s: &str) -> Option<TorrentId> {
    if s.len() != 40 {
        return None;
    }
    let mut id = [0u8; 20];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
}

/// The record's view of a torrent, for recovery.
struct Cur {
    save: PathBuf,
    stage_dir: Option<PathBuf>,
    placed: Placed,
    complete: bool,
}

/// Where file `i` is according to the record: its final path and its staging path.
fn record_paths(layout: &trs_storage::Layout, i: usize, cur: &Cur) -> (PathBuf, Option<PathBuf>) {
    let fin = match layout.file_path(i) {
        trs_storage::PathRef::Relative(r) => cur.save.join(r),
        trs_storage::PathRef::Absolute(a) => PathBuf::from(a),
    };
    let stage = cur.stage_dir.as_ref().map(|s| match layout.file_path(i) {
        trs_storage::PathRef::Relative(r) => s.join(r),
        trs_storage::PathRef::Absolute(a) => {
            s.join(".custom").join(format!("{i}-{}", Path::new(a).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
        }
    });
    (fin, stage)
}

/// Resolve one journal. `Ok(text)` says what was done.
fn recover_one(sh: &Shared, id: &TorrentId, cur: &Cur, journal: &Path) -> Result<String, String> {
    let bytes = sh.store.load_torrent_file(id).map_err(|e| e.to_string())?.ok_or("the stored .torrent file is missing")?;
    let mi = Metainfo::from_bytes(&bytes).map_err(|e| e.to_string())?;
    let placement = if cur.placed == Placed::Renamed { sh.store.load_placement(id).map_err(|e| e.to_string())? } else { None };
    let layout = Arc::new(crate::torrent::build_layout(&mi, placement.as_ref())?);
    let handle = sh.disk.add_torrent(
        layout.clone(),
        TorrentConfig { root: cur.save.clone(), staging: cur.stage_dir.clone(), mode: Mode::Seed, location: Location::Final, preallocate: false },
    );
    let rec = handle.relocate_recover(journal).map_err(|e| e.to_string())?;
    let finish = |h: &trs_storage::TorrentHandle, commit: bool| -> Result<(), String> {
        let p = if commit { h.relocate_commit(journal.to_path_buf(), None) } else { h.relocate_rollback(journal.to_path_buf()) };
        p.map_err(|e| e.to_string())?.wait().map_err(|e| e.to_string())
    };
    match rec.state {
        RecoverState::Nothing => Ok("nothing to recover".into()),
        RecoverState::Incomplete => {
            finish(&handle, false)?;
            Ok("rolled back".into())
        }
        RecoverState::Switched | RecoverState::Done => {
            finish(&handle, true)?;
            Ok("finished".into())
        }
        RecoverState::Prepared => {
            // The record decides: at the destination -> the move happened; else undo it.
            let moved = rec.entries.first().is_some_and(|en| {
                let (fin, stage) = record_paths(&layout, en.file as usize, cur);
                fin == en.dst || stage.is_some_and(|s| s == en.dst)
            });
            let _ = cur.complete;
            if moved {
                handle.relocate_mark_switched(journal).map_err(|e| e.to_string())?;
                finish(&handle, true)?;
                Ok("finished".into())
            } else {
                finish(&handle, false)?;
                Ok("rolled back".into())
            }
        }
    }
}

// ---- the exec task --------------------------------------------------------------------------

struct ExecCtx {
    sh: Arc<Shared>,
    plan: Arc<MovePlan>,
    progress: Arc<MoveProgress>,
    cancel: CancelToken,
    live: Option<crate::chan::Sender<TorrentMsg>>,
    complete: bool,
    journal: PathBuf,
}

/// The verified-piece set a partial torrent's copy checks (`None`: the torrent is complete).
fn have_of(sh: &Shared, id: &TorrentId, pieces: u32) -> Option<PieceSet> {
    let rec = sh.store.load_resume(id).ok().flatten();
    let mut set = PieceSet::new(pieces);
    if let Some(r) = rec {
        if r.complete {
            return None;
        }
        for i in 0..pieces {
            if r.bitfield.get((i / 8) as usize).is_some_and(|b| b & (0x80 >> (i % 8)) != 0) {
                set.set(i, true);
            }
        }
    }
    Some(set)
}

/// Holds a running torrent's writes (`quiesce_writes`) until dropped.
struct WriteHold(trs_storage::TorrentHandle);

impl Drop for WriteHold {
    fn drop(&mut self) {
        self.0.resume_writes();
    }
}

async fn exec(c: ExecCtx) -> ExecEnd {
    let ExecCtx { sh, plan, progress, cancel, live, complete, journal } = c;
    let id = plan.id;
    let cfg = sh.cfg();
    let relocating = plan.dst.iter().any(Option::is_some);
    let mut end_handle: Option<trs_storage::TorrentHandle> = None;
    let mut hold: Option<WriteHold> = None;
    if relocating {
        if let Some(dir) = journal.parent()
            && let Err(e) = std::fs::create_dir_all(dir) {
                return ExecEnd::Failed(format!("{}: {e}", dir.display()));
            }
        // The handle: a same-volume move of a running torrent uses the torrent's own, so that a
        // read that meets a file in transit waits for the switch instead of failing.
        let own: Option<trs_storage::TorrentHandle> = match (&live, plan.cross_volume) {
            (Some(tx), false) => {
                let (rtx, rrx) = oneshot::channel();
                if tx.send(TorrentMsg::Cmd(TorrentCmd::Handle(rtx))).await.is_ok() {
                    tokio::time::timeout(Duration::from_secs(3), rrx).await.ok().and_then(Result::ok).flatten()
                } else {
                    None
                }
            }
            _ => None,
        };
        let own_live = own.is_some();
        let handle = own.unwrap_or_else(|| {
            sh.disk.add_torrent(
                plan.layout_old.clone(),
                TorrentConfig {
                    root: plan.from_save.clone(),
                    staging: plan.from_stage_dir.clone(),
                    mode: Mode::Seed,
                    location: if plan.kind == MoveKind::Incomplete { Location::Staging } else { Location::Final },
                    preallocate: false,
                },
            )
        });
        // A running torrent that is not complete may still write (a skipped file ticked in the
        // middle of the move): hold its writes from before the first rename until the switch is
        // done, so none creates a file at a path that is about to go away.
        if own_live && !complete {
            hold.get_or_insert_with(|| WriteHold(handle.clone()));
            match handle.quiesce_writes() {
                Ok(p) => {
                    if let Err(e) = p.await {
                        return ExecEnd::Failed(e.to_string());
                    }
                }
                Err(e) => return ExecEnd::Failed(e.to_string()),
            }
        }
        let pieces = plan.layout_old.piece_count();
        let (sh2, id2) = (sh.clone(), id);
        let loaded = tokio::task::spawn_blocking(move || (piece_hashes(&sh2, &id2), have_of(&sh2, &id2, pieces))).await;
        let (hashes, have) = match loaded {
            Ok((Ok(h), have)) => (h, if complete { None } else { have }),
            Ok((Err(e), _)) => return ExecEnd::Failed(e),
            Err(e) => return ExecEnd::Failed(e.to_string()),
        };
        let prog = progress.clone();
        let opts = RelocateOptions {
            hashes: Some(hashes),
            have,
            bandwidth: cfg.move_rate_limit,
            force_copy: cfg.force_copy_moves,
            reserve_bytes: cfg.move_reserve_bytes,
            cancel: cancel.clone(),
            progress: Some(Arc::new(move |done, total| {
                prog.done.store(done, Ordering::Relaxed);
                prog.total.store(total, Ordering::Relaxed);
            })),
            fault: cfg.move_fault.as_ref().map(|f| f.0.clone()),
            ..RelocateOptions::default()
        };
        // Progress events once a second while a copy runs.
        let ticker = plan.cross_volume.then(|| {
            let (sh3, prog, id3) = (sh.clone(), progress.clone(), id);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    sh3.emit(EngineEvent::MoveProgress {
                        id: id3,
                        done: prog.done.load(Ordering::Relaxed),
                        total: prog.total.load(Ordering::Relaxed),
                    });
                }
            })
        });
        let prepared = match handle.relocate_prepare(journal.clone(), plan.dst.clone(), opts) {
            Ok(p) => p.await,
            Err(e) => Err(e),
        };
        if let Some(t) = ticker {
            t.abort();
        }
        if let Err(e) = prepared {
            if matches!(e, StorageError::Interrupted) {
                return ExecEnd::Crashed;
            }
            // Whatever the move did is undone; a missing journal is a no-op.
            if let Ok(p) = handle.relocate_rollback(journal.clone()) {
                let _ = p.await;
            }
            let dest = plan.dst.iter().flatten().next().and_then(|p| p.parent()).unwrap_or(&plan.to_save);
            return ExecEnd::Failed(fail_text(&e, dest, Some(&plan.layout_old)));
        }
        end_handle = Some(handle);
    }
    // The switch: the session persists the record and the running actor changes its paths.
    let (tx, rx) = oneshot::channel();
    if sh.session.send(SessionMsg::MoveSwitch { id, plan: plan.clone(), reply: tx }).await.is_err() {
        return ExecEnd::Failed("the engine is shutting down".into());
    }
    let switched = match rx.await {
        Ok(r) => r,
        Err(_) => Err("the engine is shutting down".into()),
    };
    if let Err(reason) = switched {
        if let Some(h) = &end_handle
            && let Ok(p) = h.relocate_rollback(journal.clone()) {
                let _ = p.await;
            }
        return ExecEnd::Failed(reason);
    }
    if let Some(h) = end_handle {
        // Crash simulation for the steps after the switch (`MOVE_STEP_*`): storage's own hook
        // only covers `relocate_prepare`.
        let faulted = |n: u32| cfg.move_fault.as_ref().is_some_and(|f| !(f.0)(n));
        if faulted(MOVE_STEP_BEFORE_MARK) {
            return ExecEnd::Crashed;
        }
        if h.relocate_mark_switched(&journal).is_err() {
            // The record points at the destination already; the journal is resolved at the next start.
            return ExecEnd::Done;
        }
        if faulted(MOVE_STEP_BEFORE_COMMIT) {
            return ExecEnd::Crashed;
        }
        // An abandoned finalize journal of the old staging folder is not needed any more.
        if plan.kind == MoveKind::Incomplete
            && let Some(old) = &plan.from_stage_dir {
                let hex = crate::util::hex(&id);
                let _ = std::fs::remove_file(old.join(format!("{hex}.journal")));
                let _ = std::fs::remove_file(old.join(format!("{hex}.p.journal")));
            }
        if let Ok(p) = h.relocate_commit(journal, Some(plan.stop_at.clone())) {
            // An error here leaves old files behind; the new place is in force and the journal
            // is resolved (the sources deleted) at the next start.
            let _ = p.await;
        }
        // With "Organize by seed status" off nothing is filed into Active/, Dormant/, Stopped/ or
        // Dead/ any more: the one a torrent just left goes away once it is empty (Finder's
        // `.DS_Store` does not count; a folder that still holds anything else stays).
        if !cfg.by_status && plan.kind != MoveKind::Incomplete && crate::status::split_status(&plan.stop_at).1.is_some() {
            let _ = trs_storage::remove_dir_if_empty(&plan.stop_at);
        }
    }
    ExecEnd::Done
}

impl Session {
    /// Skipped files that share a piece with a wanted file still get those bytes written. For a
    /// placed torrent they live in a hidden folder next to the clean destination, so the library
    /// never shows a full-length, mostly empty "episode" (see `organize::hidden_edge`). Called
    /// when the set of wanted files changed (a choice was confirmed, items were ticked later):
    /// edge files that stopped being edge files move to their clean destination.
    ///
    /// A running torrent may be writing these very files, so the rename is not done here: the
    /// actor hands it to storage together with the layout switch (`switch_layout_renaming`), which
    /// makes both one step for every writer. The record is persisted after storage confirmed
    /// ([`Session::on_edge_applied`]); a crash in between is repaired at the next start
    /// (`edgefix::reconcile`). Without an actor nothing writes and the files are renamed here.
    pub(crate) fn apply_edge_policy(&mut self, id: TorrentId) {
        self.edge_policy(id, 0);
    }

    /// Tries before an edge change that keeps meeting a busy or changing torrent is dropped (the
    /// files stay where they are, which is always consistent).
    const EDGE_TRIES: u8 = 20;

    pub(super) fn edge_policy(&mut self, id: TorrentId, tries: u8) {
        if let Some(again) = self.edge_busy.get_mut(&id) {
            *again = true;
            return;
        }
        let Some(e) = self.table.get(&id) else { return };
        if !e.cold.has2(G_PLACED) || !e.cold.has_metadata() {
            return;
        }
        let prios: Vec<u8> = e.cold.file_prios.as_deref().map(<[u8]>::to_vec).unwrap_or_default();
        let (Ok(Some(bytes)), Ok(Some(mut pl))) = (self.sh.store.load_torrent_file(&id), self.sh.store.load_placement(&id)) else {
            return;
        };
        let Ok(mi) = Metainfo::from_bytes(&bytes) else { return };
        let mut prios = prios;
        prios.resize(mi.file_count(), 2);
        let edge = organize::edge_files(&mi, &prios);
        let exists = |p: &std::path::Path| std::fs::symlink_metadata(p).is_ok();
        let mut changed = false;
        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        // Paths the new layout names that nothing may have created by the time it is installed.
        let mut absent: Vec<PathBuf> = Vec::new();
        for i in 0..mi.file_count() {
            let Some(Some(cur)) = pl.files.get(i).cloned() else { continue };
            let proper = organize::proper_edge(&cur);
            let hide = prios[i] == 0 && edge[i];
            if hide {
                // Only a file that was never written moves out of the library's sight.
                if cur == proper && !exists(&cur) {
                    pl.files[i] = Some(organize::hidden_edge(&proper));
                    absent.push(cur);
                    changed = true;
                }
            } else if cur != proper {
                if exists(&cur) {
                    let target = if exists(&proper) {
                        PathBuf::from(trs_classify::unique_path(&proper.to_string_lossy(), |c| exists(std::path::Path::new(c))))
                    } else {
                        proper
                    };
                    pl.files[i] = Some(target.clone());
                    renames.push((cur, target));
                } else {
                    pl.files[i] = Some(proper);
                    absent.push(cur);
                    changed = true;
                }
            }
        }
        changed |= !renames.is_empty();
        if !changed {
            return;
        }
        let live_tx = self.table.get(&id).and_then(|e| e.live.as_ref().map(|l| (l.stopping, l.tx.clone())));
        let Some((stopping, tx)) = live_tx else {
            // No actor, so nobody writes: rename, then persist. A crash in between leaves the
            // record naming a missing path next to the file; the next start repairs it.
            let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
            let mut ok = true;
            for (from, to) in &renames {
                if let Some(dir) = to.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if trs_storage::rename_excl(from, to).is_ok() {
                    done.push((from.clone(), to.clone()));
                } else {
                    ok = false;
                    break;
                }
            }
            if !ok {
                for (from, to) in done.iter().rev() {
                    let _ = trs_storage::rename_excl(to, from);
                }
                return;
            }
            if self.commit_now(Batch { placements: vec![(id, Some(pl))], ..Batch::default() }).is_ok() {
                for (from, _) in &done {
                    if let Some(parent) = from.parent() {
                        let _ = trs_storage::remove_dir_if_empty(parent);
                    }
                }
            }
            return;
        };
        if stopping {
            self.edge_retry_later(id, tries);
            return;
        }
        let Ok(layout) = crate::torrent::build_layout(&mi, Some(&pl)) else { return };
        let (atx, arx) = oneshot::channel();
        let Some(e) = self.table.get(&id) else { return };
        let sw = Switch {
            layout: Arc::new(layout),
            save_path: self.save_path(&e.cold),
            staging: self.staging_for(&e.cold),
            placement: Some(pl.clone()),
            renames,
            absent,
            reply: Some(atx),
        };
        if tx.try_send(TorrentMsg::Cmd(TorrentCmd::SwitchLocation(Box::new(sw)))).is_err() {
            self.edge_retry_later(id, tries);
            return;
        }
        self.edge_busy.insert(id, false);
        let session = self.sh.session.clone();
        tokio::spawn(async move {
            let result = match arx.await {
                Ok(r) => r,
                Err(_) => Err("the torrent stopped before it answered".into()),
            };
            let _ = session.send(SessionMsg::EdgeApplied { id, placement: Box::new(pl), result, tries }).await;
        });
    }

    fn edge_retry_later(&mut self, id: TorrentId, tries: u8) {
        if tries >= Self::EDGE_TRIES {
            return;
        }
        let session = self.sh.session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let _ = session.send(SessionMsg::EdgeRetry { id, tries: tries + 1 }).await;
        });
    }

    /// Storage renamed the edge files and installed the layout (or refused): persist the
    /// placement, or look again at what the torrent looks like now.
    pub(super) fn on_edge_applied(&mut self, id: TorrentId, placement: crate::persist::Placement, result: Result<(), String>, tries: u8) {
        let again = self.edge_busy.remove(&id).unwrap_or(false);
        if !self.table.contains_key(&id) {
            return;
        }
        match result {
            Ok(()) => {
                // Old folders left empty by the rename go away.
                if let Ok(Some(old)) = self.sh.store.load_placement(&id) {
                    for (i, o) in old.files.iter().enumerate() {
                        if let (Some(o), Some(n)) = (o, placement.files.get(i).and_then(Option::as_ref))
                            && o != n
                                && let Some(parent) = o.parent() {
                                    let _ = trs_storage::remove_dir_if_empty(parent);
                                }
                    }
                }
                let _ = self.commit_now(Batch { placements: vec![(id, Some(placement))], ..Batch::default() });
                if again {
                    self.edge_policy(id, 0);
                }
            }
            Err(_) => self.edge_retry_later(id, tries),
        }
    }
}
