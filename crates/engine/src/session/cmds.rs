//! Command handlers and the lifecycle of torrents (start, stop, queue, remove).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::oneshot;
use trs_metainfo::Metainfo;
use trs_wire::Handshake;

use super::cold::*;
use super::{After, Cmd, Live, RemoveWait, Session, SessionMsg};
use crate::api::*;
use crate::budget::ConnPermit;
use crate::persist::{Placement, StatDelta};
use crate::queue::{QueueItem, plan};
use crate::tier::Tier;
use crate::torrent::{self, LiveStats, TorrentCmd, TorrentInit, TorrentMsg};
use crate::tracker_driver::TrackerCmd;
use crate::util::{hex, unix_secs};

impl Session {
    pub(super) fn on_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::AddPrepared { p, opts, origin, reply } => {
                let r = if self.shutting_down { Err(EngineError::ShuttingDown) } else { self.add_prepared(*p, *opts, origin) };
                let _ = reply.send(r);
            }
            Cmd::AddMagnet { uri, opts, reply } => {
                let r = if self.shutting_down { Err(EngineError::ShuttingDown) } else { self.add_magnet(&uri, *opts) };
                let _ = reply.send(r);
            }
            Cmd::Exists(id, reply) => {
                let _ = reply.send(self.table.get(&id).map(|e| e.cold.name.to_string()));
            }
            Cmd::AddDownload { url, opts, reply } => {
                let _ = reply.send(self.add_download(url, *opts));
            }
            Cmd::SetDownloadLimit(id, l) => self.set_download_limit(id, l),
            Cmd::ForgetFinished(ids) => self.forget_finished(ids),
            Cmd::SetPolicy(p) => self.set_policy(p),
            Cmd::Pause(ids) => {
                for id in ids {
                    self.set_want(id, Want::Paused);
                }
            }
            Cmd::Resume(ids) => {
                for id in ids {
                    self.user_resume(id);
                }
            }
            Cmd::Remove { ids, delete_files, reply } => {
                // The reply comes when every torrent is stopped and (if asked) its files are gone.
                let w = RemoveWait::new(ids.len(), reply);
                for id in ids {
                    self.remove_waiting(id, delete_files, Some(w.clone()));
                }
            }
            Cmd::Recheck(ids) => {
                for id in ids {
                    self.recheck(id);
                }
            }
            Cmd::MoveToTop(ids) => {
                for id in ids.into_iter().rev() {
                    self.move_to_top(id);
                }
                self.replan();
            }
            Cmd::SetPriority(ids, p) => {
                for id in ids {
                    if let Some(e) = self.table.get_mut(&id) {
                        e.cold.priority = p.to_u8();
                        e.cold.set2(G_PRIO_USER, true);
                        if let Some(l) = &e.live {
                            let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SetPriority(p)));
                        }
                        self.mark_record(id);
                    }
                }
                self.replan();
            }
            Cmd::SetFilePriorities(id, p, reply) => {
                let r = self.set_file_priorities(id, p);
                if r.is_ok() {
                    self.apply_edge_policy(id);
                }
                let _ = reply.send(r);
            }
            Cmd::SetSeedGoal(ids, g) => {
                for id in ids {
                    if let Some(e) = self.table.get_mut(&id) {
                        e.cold.seed_goal = g;
                        e.cold.set2(G_GOAL_USER, true);
                        self.mark_record(id);
                    }
                    // The floor and the action at goal stay the rule's.
                    if let Some(p) = self.sh.policy() {
                        self.apply_policy_to(id, &p);
                    }
                }
                self.ensure_goal_job();
                self.goal_check();
            }
            Cmd::ClearSeedGoal(ids) => {
                let pol = self.sh.policy();
                for id in ids {
                    let Some(e) = self.table.get_mut(&id) else { continue };
                    if !e.cold.has2(G_GOAL_USER) {
                        continue;
                    }
                    e.cold.set2(G_GOAL_USER, false);
                    // Without rules the goal is "seed forever"; with them the rule decides again.
                    e.cold.seed_goal = SeedGoal::FOREVER;
                    if let Some(p) = &pol {
                        self.apply_policy_to(id, p);
                    }
                    self.mark_record(id);
                }
                self.ensure_goal_job();
                self.goal_check();
            }
            Cmd::SetCategory(ids, cat) => self.set_category(ids, cat),
            Cmd::ChoiceInfo(id, reply) => {
                let _ = reply.send(self.choice_job(id));
            }
            Cmd::ConfirmChoice { id, prios, reply } => {
                let _ = reply.send(self.confirm_choice(id, prios));
            }
            Cmd::CancelChoice { id, reply } => self.cancel_choice(id, reply),
            Cmd::PendingAct { id, act, reply } => {
                let _ = reply.send(self.pending_act(id, act));
            }
            Cmd::MoveTorrents { ids, target, reply } => {
                let _ = reply.send(self.queue_moves(ids, target));
            }
            Cmd::ProposeMoves(reply) => self.propose_moves(reply),
            Cmd::CancelMoves(ids) => self.cancel_moves(ids),
            Cmd::SetRule(ids, rule) => self.set_rule(ids, rule),
            Cmd::PauseAll => self.pause_all(),
            Cmd::ResumeAll => self.resume_all(),
            Cmd::VolumesChanged => self.volumes_changed(),
            Cmd::UpdateConfig(f) => self.on_update_config(f),
            Cmd::Reannounce(ids, reply) => {
                let mut waits = Vec::new();
                for id in ids {
                    let (tx, rx) = oneshot::channel();
                    if self.tracker.is_some() && self.table.contains_key(&id) {
                        self.tracker_send(TrackerCmd::Reannounce(id, tx));
                        waits.push(rx);
                    }
                }
                tokio::spawn(async move {
                    let mut out = Vec::new();
                    for rx in waits {
                        if let Ok(v) = rx.await {
                            out.extend(v);
                        }
                    }
                    let _ = reply.send(out);
                });
            }
            Cmd::AddPeers(id, peers) => {
                if let Some(e) = self.table.get(&id) {
                    if let Some(l) = &e.live {
                        let _ = l.tx.try_send(TorrentMsg::AddPeers(peers));
                    } else if e.cold.want == Want::Active && !e.cold.has(F_QUEUED) && !e.cold.awaiting() && !self.mover.blocks(&id) && !self.boot.holds(&id) {
                        self.spawn_torrent(id, Vec::new(), peers);
                    }
                }
            }
            Cmd::NetworkChanged => self.on_network_changed(true),
            Cmd::PathChanged(satisfied) => {
                self.tracker_send(TrackerCmd::Path(satisfied));
                // Online and a path appeared: the old network-change work (port mapping refresh,
                // failed announces). During an outage the same work happens once, at recovery.
                if satisfied && !self.sh.net.held() {
                    self.on_network_changed(true);
                }
            }
            Cmd::WillSleep => self.tracker_send(TrackerCmd::WillSleep),
            Cmd::DidWake => self.tracker_send(TrackerCmd::DidWake),
            Cmd::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
            Cmd::Totals(reply) => {
                let _ = reply.send(self.totals());
            }
            Cmd::Detail(id, reply) => self.detail(id, reply),
            Cmd::Config(reply) => {
                let _ = reply.send((*self.sh.cfg()).clone());
            }
            Cmd::Flush(reply) => {
                self.flush_persist();
                self.writer.flush();
                let _ = reply.send(());
            }
            Cmd::Shutdown { .. } => {}
            c @ (Cmd::StreamOpen { .. } | Cmd::StreamsClosed) => self.on_stream_cmd(c),
        }
    }

    // ---- lifecycle ----------------------------------------------------------------------

    /// Pause everything that runs, in one pass: the wants change first and the queue is planned
    /// once, so n torrents cost O(n), not n plans of n.
    fn pause_all(&mut self) {
        let ids: Vec<_> = self.table.iter().filter(|(_, e)| e.cold.want == Want::Active).map(|(i, _)| *i).collect();
        for id in ids {
            self.set_want_inner(id, Want::Paused, false, false);
        }
        let dids: Vec<_> = self.directs.keys().copied().collect();
        for id in dids {
            self.direct_set_want_with(id, false, false);
        }
        self.replan();
    }

    /// Resume everything the user paused, in one pass. Torrents that need an actor (unfinished,
    /// rechecking, fetching metadata) come up through the start queue, a couple per tick, in queue
    /// order, exactly as at launch; finished ones stay Cold and cost nothing.
    fn resume_all(&mut self) {
        let mut ids: Vec<_> = self.table.iter().filter(|(_, e)| e.cold.want == Want::Paused).map(|(i, e)| (e.cold.queue_pos, e.cold.added_at, *i)).collect();
        ids.sort_unstable();
        for (_, _, id) in ids {
            if self.prepare_resume(id) {
                self.set_want_inner(id, Want::Active, false, true);
            }
        }
        let dids: Vec<_> = self.directs.keys().copied().collect();
        for id in dids {
            self.direct_set_want_with(id, true, false);
        }
        self.replan();
        self.boot_begin();
    }

    /// What a user resume does before the want changes (a reached goal becomes "keep sharing", an
    /// error is forgotten). `false` = nothing to resume.
    fn prepare_resume(&mut self, id: TorrentId) -> bool {
        self.retries.remove(&id);
        let Some(e) = self.table.get_mut(&id) else { return false };
        if !matches!(e.cold.want, Want::Paused | Want::GoalReached | Want::Error) {
            return false;
        }
        // Resuming past a reached goal means "keep sharing": with the goal left as it was, the
        // next goal check would stop it again within a minute. The goal becomes the user's own
        // "forever" (the rule's floor and action stay); Rule Default hands it back.
        if e.cold.want == Want::GoalReached {
            let g = e.cold.seed_goal;
            e.cold.seed_goal =
                SeedGoal { min_seed_secs: g.min_seed_secs, at_goal: g.at_goal, idle_mb: g.idle_mb, idle_hours: g.idle_hours, ..SeedGoal::FOREVER };
            e.cold.set2(G_GOAL_USER, true);
        }
        // A resume by hand restarts the inactive condition's window (SPEC §7.5): the user's choice
        // is not undone an hour later.
        e.cold.idle = None;
        e.cold.error = None;
        e.cold.set2(G_MISSING, false);
        true
    }

    pub(crate) fn user_resume(&mut self, id: TorrentId) {
        if self.directs.contains_key(&id) {
            return self.direct_set_want(id, true);
        }
        if self.prepare_resume(id) {
            self.set_want(id, Want::Active);
            // The inactive condition's window starts again from this resume.
            self.goal_check_soon();
        }
    }

    pub(crate) fn set_want(&mut self, id: TorrentId, want: Want) {
        if self.directs.contains_key(&id) {
            return self.direct_set_want(id, want == Want::Active);
        }
        self.set_want_inner(id, want, true, false);
    }

    /// `replan: false` for the bulk callers, which plan the queue once at the end. `hold`: a
    /// torrent that needs an actor to come back waits in the start queue instead of starting now.
    pub(crate) fn set_want_inner(&mut self, id: TorrentId, want: Want, replan: bool, hold: bool) {
        let Some(e) = self.table.get_mut(&id) else { return };
        if e.cold.want == want {
            return;
        }
        e.cold.want = want;
        // Somebody asked for this one: it does not wait for its turn.
        self.boot.release(&id);
        self.update_error(id);
        self.update_seed_clock(id);
        self.mark_record(id);
        self.mark_counters(id);
        if let Some(e) = self.table.get(&id) {
            let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
        if replan {
            self.replan();
        }
        if hold && want == Want::Active && self.table.get(&id).is_some_and(|e| e.live.is_none()) && self.actor_need(&id).1 {
            self.boot.hold(id);
            self.boot_begin();
        }
        self.reconcile(id);
    }

    /// Make the world match what the torrent should be doing.
    pub(crate) fn reconcile(&mut self, id: TorrentId) {
        if self.shutting_down {
            return;
        }
        let Some(e) = self.table.get(&id) else { return };
        let (run_ok, need_task) = self.actor_need(&id);
        let (has_live, stopping) = (e.live.is_some(), e.live.as_ref().is_some_and(|l| l.stopping));
        if need_task && !has_live && self.boot.holds(&id) {
            // Start-up: this one waits for its turn (`boot_step`), so that not every torrent
            // that needs an actor is verified and resident at the same moment.
        } else if need_task && !has_live && self.must_wait_to_verify(&id) {
            // All hashing slots are taken: wait in the start queue, with no actor.
            self.boot.hold(id);
            self.boot_begin();
        } else if need_task && !has_live {
            let peers = self.hint_peers.remove(&id).unwrap_or_default();
            self.spawn_torrent(id, Vec::new(), peers);
        } else if !run_ok && has_live && !stopping {
            self.stop_live(id, After::Nothing);
        }
        self.sync_tracker(id);
        self.update_seed_clock(id);
    }

    /// `(may run, needs an actor)`: a torrent that is verifying, fetching metadata or downloading
    /// needs one; a finished one does not (Cold) until a peer shows up.
    pub(crate) fn actor_need(&self, id: &TorrentId) -> (bool, bool) {
        let Some(e) = self.table.get(id) else { return (false, false) };
        let c = &e.cold;
        let run_ok = c.want == Want::Active && !c.has(F_QUEUED) && !c.awaiting() && !self.mover_blocks(id);
        let need = run_ok && (!c.has_metadata() || c.has2(G_UNVERIFIED) || c.has2(G_RECHECK) || (!c.complete() && !c.has(F_WANTED_DONE)));
        (run_ok, need)
    }

    /// Tell the tracker driver whether a session should be open.
    pub(crate) fn sync_tracker(&mut self, id: TorrentId) {
        if self.tracker.is_none() {
            return;
        }
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        let want = c.want == Want::Active && !c.has(F_QUEUED) && !c.awaiting() && c.has(F_TRACKER_REG);
        let started = c.has2(G_TRACKER_STARTED);
        if want && !started {
            c.set2(G_TRACKER_STARTED, true);
            self.tracker_send(TrackerCmd::Start(id));
        } else if !want && started {
            c.set2(G_TRACKER_STARTED, false);
            self.tracker_send(TrackerCmd::Stop(id));
        }
    }

    pub(crate) fn spawn_torrent(&mut self, id: TorrentId, first: Vec<TorrentMsg>, peers: Vec<SocketAddr>) {
        let cfg = self.sh.cfg();
        let epoch = self.next_epoch();
        let staging = self.table.get(&id).and_then(|e| self.staging_for(&e.cold));
        let Some(e) = self.table.get_mut(&id) else { return };
        if e.live.is_some() {
            return;
        }
        self.sh.diag.actors_spawned.fetch_add(1, Ordering::Relaxed);
        let c = &mut e.cold;
        let placement = if c.has2(G_PLACED) { self.sh.store.load_placement(&id).ok().flatten() } else { None };
        // A crash between an edge file's rename and its record is repaired here; a file that
        // exists under two names is settled by the actor, and the torrent is rechecked after it.
        let edge_conflicts = placement.as_ref().map(crate::edgefix::reconcile).unwrap_or_default();
        let fresh = c.has2(G_UNVERIFIED);
        let force = c.has2(G_RECHECK) || !edge_conflicts.is_empty();
        c.set2(G_RECHECK, false);
        let resume = if fresh { None } else { self.sh.store.load_resume(&id).ok().flatten() };
        let mut peers = peers;
        if let Some(p) = self.paused_peers.remove(&id) {
            peers.extend(p);
        }
        if !fresh
            && let Ok(p) = self.sh.store.load_peers(&id) {
                peers.extend(p.into_iter().take(32));
            }
        let incomplete = !c.complete() && !c.has(F_WANTED_DONE);
        let tier = if incomplete || fresh { Tier::Hot } else { Tier::Warm };
        let stats = Arc::new(LiveStats::new(tier));
        // Counts as verifying from the moment it is started, so the gate sees it before the actor runs.
        stats.verifying.store(c.has_metadata(), Ordering::Relaxed);
        let (tx, rx) = crate::chan::channel(4096);
        let init = TorrentInit {
            id,
            epoch,
            shared: self.sh.clone(),
            private: c.private(),
            has_metadata: c.has_metadata(),
            save_path: self.roots.get(c.save_root as usize).cloned().unwrap_or_default(),
            staged: c.has(F_STAGED),
            file_prios: c.file_prios.as_deref().map(<[u8]>::to_vec).unwrap_or_default(),
            priority: c.priority(),
            resume,
            counters: c.counters.clone(),
            download_allowed: !c.has(F_QUEUED),
            fresh,
            force_recheck: force,
            peers,
            first,
            up_limit: c.up_limit(),
            group: ColdTorrent::opt(c.tracker_profile).and_then(|i| self.ids.get(i)).and_then(|p| self.sh.groups.get(p)),
            placement,
            edge_conflicts,
            staging,
            tier,
            rx,
            tx: tx.clone(),
            live: stats.clone(),
        };
        let _ = cfg;
        tokio::spawn(torrent::run(init));
        e.live = Some(Box::new(Live { tx, stats, epoch, stopping: false, after: After::Nothing }));
        self.sh.diag.promotions.fetch_add(0, std::sync::atomic::Ordering::Relaxed);
        self.stat_baseline(id);
        self.ensure_counters_job();
    }

    pub(crate) fn stop_live(&mut self, id: TorrentId, after: After) {
        // Responses end before storage is torn down or the files move (otherwise a read could meet
        // `MissingData`); a pause, a move, a recheck and a removal all come through here.
        if self.table.get(&id).is_some_and(|e| e.live.is_some()) {
            self.close_streams(&id);
        }
        let Some(e) = self.table.get_mut(&id) else { return };
        let Some(l) = e.live.as_mut() else { return };
        if l.stopping {
            if after != After::Nothing {
                l.after = after;
            }
            return;
        }
        l.stopping = true;
        l.after = after;
        let (rtx, rrx) = oneshot::channel();
        let msg = TorrentMsg::Cmd(TorrentCmd::Shutdown(rtx));
        let epoch = l.epoch;
        let tx = l.tx.clone();
        let stx = self.sh.session.clone();
        tokio::spawn(async move {
            // The actor queue is large, but a stop must never be lost.
            let sent = tx.send(msg).await.is_ok();
            let info = if sent { rrx.await.ok() } else { None };
            let _ = stx.send(SessionMsg::Exited { id, epoch, info }).await;
        });
    }

    pub(crate) fn on_exited(&mut self, id: TorrentId, epoch: u64, info: Option<torrent::ShutdownInfo>) {
        let Some(e) = self.table.get_mut(&id) else { return };
        if e.live.as_ref().map(|l| l.epoch) != Some(epoch) {
            return;
        }
        let Some(live) = e.live.take() else { return };
        if let Some(info) = info {
            if let Some(r) = info.resume {
                self.dirty_resume(id, Some(r));
            }
            if !info.peers.is_empty() {
                // A restart (recheck, retry after a refused write) redials the peers it had.
                if matches!(live.after, After::Respawn { .. }) && self.hint_peers.len() < 1024 {
                    self.hint_peers.insert(id, info.peers.iter().copied().take(64).collect());
                }
                // A pause is not a demotion: remember who we were talking to for the resume.
                let paused = self.table.get(&id).is_some_and(|e| e.cold.want != Want::Active);
                if paused && self.paused_peers.len() < 1024 {
                    self.paused_peers.insert(id, info.peers.iter().copied().take(64).collect());
                }
                self.dirty_peers(id, info.peers);
            }
        }
        self.mark_counters(id);
        self.mark_record(id);
        self.final_stats();
        self.update_seed_clock(id);
        // A download whose files are about to move was waiting for its actor to stop.
        self.on_actor_gone_for_move(id);
        match live.after {
            After::Nothing => {}
            After::Respawn { force_recheck } => {
                if let Some(e) = self.table.get_mut(&id) {
                    e.cold.set2(G_RECHECK, force_recheck);
                }
                self.reconcile(id);
            }
            After::Remove { delete_files } => self.finish_remove(id, delete_files),
        }
    }

    // ---- queue --------------------------------------------------------------------------

    /// Apply the queue plan: hold back or release downloads.
    pub(crate) fn replan(&mut self) {
        if self.shutting_down {
            return;
        }
        self.sh.diag.replans.fetch_add(1, Ordering::Relaxed);
        let max = self.sh.cfg().max_active_downloads;
        let mut ids: Vec<TorrentId> = self.table.iter().filter(|(_, e)| e.cold.wants_slot()).map(|(i, _)| *i).collect();
        ids.sort_unstable();
        let directs = self.direct_queue_items();
        // Torrents and direct downloads compete for the same slots, in one user-visible order.
        let mut items: Vec<QueueItem> = ids
            .iter()
            .map(|i| {
                let c = &self.table[i].cold;
                // A torrent somebody streams jumps the queue (SPEC 7F), without touching its own priority.
                let priority = if self.streaming.contains(i) { Priority::High } else { c.priority() };
                QueueItem { pos: c.queue_pos, priority, wants_slot: true, inactive: self.queue_inactive(i) }
            })
            .collect();
        items.extend(directs.iter().map(|(_, pos)| QueueItem { pos: *pos, priority: Priority::Normal, wants_slot: true, inactive: false }));
        let run = plan(&items, max);
        let n = ids.len();
        for (id, ok) in ids.into_iter().zip(run.iter().copied()) {
            let Some(e) = self.table.get_mut(&id) else { continue };
            let queued = e.cold.has(F_QUEUED);
            if !ok && !queued {
                e.cold.set(F_QUEUED, true);
                let st = e.cold.state(None);
                self.sh.emit(EngineEvent::StateChanged { id, state: st });
                self.reconcile(id);
            } else if ok && queued {
                e.cold.set(F_QUEUED, false);
                self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::Downloading });
                self.reconcile(id);
            }
        }
        for ((id, _), ok) in directs.into_iter().zip(run.into_iter().skip(n)) {
            self.direct_queue_verdict(id, ok);
        }
        self.direct_queue_cleanup();
        // Torrents that stopped wanting a slot (finished, paused) may have left the queued flag.
        let stale: Vec<TorrentId> = self
            .table
            .iter()
            .filter(|(_, e)| e.cold.has(F_QUEUED) && !e.cold.wants_slot())
            .map(|(i, _)| *i)
            .collect();
        for id in stale {
            if let Some(e) = self.table.get_mut(&id) {
                e.cold.set(F_QUEUED, false);
            }
            self.reconcile(id);
        }
        self.arm_stall_check();
    }

    fn move_to_top(&mut self, id: TorrentId) {
        if !self.table.contains_key(&id) {
            return;
        }
        let mut order: Vec<(u32, u32, TorrentId)> =
            self.table.iter().filter(|(i, _)| **i != id).map(|(i, e)| (e.cold.queue_pos, e.cold.added_at, *i)).collect();
        order.sort_unstable();
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.queue_pos = 0;
        }
        for (n, (_, _, i)) in order.into_iter().enumerate() {
            if let Some(e) = self.table.get_mut(&i) {
                e.cold.queue_pos = n as u32 + 1;
            }
            self.mark_record(i);
        }
        self.next_pos = self.table.len() as u32 + 1;
        self.mark_record(id);
    }

    // ---- other commands -----------------------------------------------------------------

    fn recheck(&mut self, id: TorrentId) {
        let Some(e) = self.table.get_mut(&id) else { return };
        e.cold.set2(G_RECHECK, true);
        self.boot.release(&id);
        if e.live.is_some() {
            self.stop_live(id, After::Respawn { force_recheck: true });
        } else {
            self.reconcile(id);
        }
    }

    /// Are all pieces under the wanted files verified, according to the saved resume data?
    /// `None` when the resume data cannot say.
    /// Bytes of the wanted pieces the resume data does not have (`None` = no resume data).
    fn wanted_missing_bytes(&self, id: &TorrentId, mi: &Metainfo, prios: &[u8]) -> Option<u64> {
        let r = self.sh.store.load_resume(id).ok().flatten()?;
        if r.complete {
            return Some(0);
        }
        let n = mi.piece_count() as u32;
        let bf = trs_wire::Bitfield::from_wire(&r.bitfield, n).ok()?;
        let pp = crate::picker::piece_priorities(
            mi.piece_length,
            n,
            mi.files().map(|f| (f.offset, f.length, prios.get(f.index).copied().unwrap_or(2), f.is_padding())),
        );
        Some(
            (0..n)
                .filter(|&i| pp[i as usize] != 0 && !bf.has(i))
                .map(|i| {
                    let start = u64::from(i) * u64::from(mi.piece_length);
                    mi.total_length.saturating_sub(start).min(u64::from(mi.piece_length))
                })
                .sum(),
        )
    }

    pub(super) fn set_file_priorities(&mut self, id: TorrentId, p: Vec<FilePriority>) -> Result<(), EngineError> {
        let e = self.table.get_mut(&id).ok_or(EngineError::NotFound)?;
        if !e.cold.has_metadata() {
            return Err(EngineError::NoMetadata);
        }
        if p.len() > trs_metainfo::MAX_FILES {
            return Err(EngineError::Invalid("too many file priorities".into()));
        }
        let raw: Vec<u8> = p.iter().map(|x| x.to_u8()).collect();
        let old_done = e.cold.wanted_done();
        e.cold.file_prios = (!p.iter().all(|x| *x == FilePriority::Normal)).then(|| raw.clone().into_boxed_slice());
        // "Wanted" is what the priorities leave: the torrent file knows the lengths.
        let mut all_present = None;
        if let Ok(Some(bytes)) = self.sh.store.load_torrent_file(&id)
            && let Ok(mi) = Metainfo::from_bytes(&bytes) {
                if let Some(e) = self.table.get_mut(&id) {
                    e.cold.wanted_len = crate::organize::wanted_bytes(&mi, &raw);
                }
                if self.table.get(&id).is_some_and(|e| e.live.is_none()) {
                    // Nobody is counting: take the verified wanted bytes from the resume data (what
                    // the old selection held, at most, when there is none).
                    let missing = self.wanted_missing_bytes(&id, &mi, &raw);
                    all_present = missing.map(|m| m == 0);
                    if let Some(e) = self.table.get(&id) {
                        let wl = e.cold.wanted_len;
                        e.cold.counters.set_wanted_done(missing.map_or(old_done.min(wl), |m| wl.saturating_sub(m)));
                    }
                }
            }
        let e = self.table.get_mut(&id).ok_or(EngineError::NotFound)?;
        if let Some(l) = &e.live {
            // The actor tells the session (`Unfinished`) when the wanted set grew.
            let _ = l.tx.try_send(TorrentMsg::Cmd(TorrentCmd::SetFilePriorities(raw)));
        } else if e.cold.finished() && !e.cold.complete() && all_present != Some(true) {
            // More files are wanted now (or the resume data cannot tell): a download again until
            // the actor has looked at the pieces. The record keeps its `finished_at` in
            // case the pieces turn out to be there (then the original time stays).
            let was = e.cold.finished_at;
            self.reopen_download(id);
            if was != 0 {
                self.reopened_at.insert(id, was);
            }
        }
        self.mark_record(id);
        self.replan();
        self.reconcile(id);
        Ok(())
    }

    pub(crate) fn remove_torrent(&mut self, id: TorrentId, delete_files: bool) {
        self.remove_waiting(id, delete_files, None);
    }

    pub(crate) fn remove_waiting(&mut self, id: TorrentId, delete_files: bool, wait: Option<Arc<RemoveWait>>) {
        if !self.directs.contains_key(&id) && !self.table.contains_key(&id) {
            if let Some(w) = wait {
                w.done();
            }
            return;
        }
        // A torrent that is moving is removed when the move has ended (a copy is cancelled first).
        let Some(wait) = self.defer_remove_if_moving(id, delete_files, wait) else { return };
        if let Some(w) = wait {
            self.remove_waits.entry(id).or_default().push(w);
        }
        if self.directs.contains_key(&id) {
            return self.direct_remove(id, delete_files);
        }
        self.close_streams(&id);
        let Some(e) = self.table.get_mut(&id) else { return };
        e.cold.want = Want::Paused;
        let has_live = e.live.is_some();
        self.tracker_send(TrackerCmd::Remove(id));
        if has_live {
            self.stop_live(id, After::Remove { delete_files });
        } else {
            self.finish_remove(id, delete_files);
        }
    }

    fn finish_remove(&mut self, id: TorrentId, delete_files: bool) {
        let Some(e) = self.table.remove(&id) else { return };
        self.pending.remove(&id);
        self.sh.release_source(&id);
        self.swarm.remove(&id);
        self.retries.remove(&id);
        self.stall.remove(&id);
        // A torrent that never started owns no files: its destinations may name files the user
        // already had (that is why it was held), so "delete files" has nothing to delete.
        let delete_files = delete_files && !e.cold.awaiting();
        self.paused_peers.remove(&id);
        let save = self.save_path(&e.cold);
        let staging = self.staging_for(&e.cold);
        let placement = if e.cold.has2(G_PLACED) {
            self.sh.store.load_placement(&id).ok().flatten()
        } else {
            None
        };
        if let Some(p) = &placement {
            crate::organize::release(&self.sh.reserved, p);
        }
        let waits = self.remove_waits.remove(&id).unwrap_or_default();
        let mut deleting = false;
        if delete_files && e.cold.has_metadata()
            && let Ok(Some(bytes)) = self.sh.store.load_torrent_file(&id) {
                let placed = e.cold.placed();
                deleting = true;
                let waits2 = waits.clone();
                tokio::task::spawn_blocking(move || {
                    if let Ok(mi) = Metainfo::from_bytes(&bytes) {
                        delete_data(&save, &mi, &id, staging.as_deref(), placement.as_ref(), placed);
                    }
                    for w in waits2 {
                        w.done();
                    }
                });
            }
        if !deleting {
            for w in waits {
                w.done();
            }
        }
        self.sh.alloc.remove(&id);
        self.forget_stat(&id);
        self.dirty_remove(id);
        self.push_stat(StatDelta {
            domain: self.stat_domain(&e.cold),
            private: e.cold.private(),
            at: unix_secs(),
            torrents_removed: 1,
            ..StatDelta::default()
        });
        self.sh.emit(EngineEvent::Removed { id });
        self.replan();
    }

    // ---- incoming connections -----------------------------------------------------------

    pub(super) fn on_incoming(&mut self, stream: crate::net::PeerIo, hs: Handshake, addr: SocketAddr, permit: ConnPermit) {
        let diag = self.sh.diag.clone();
        let refuse = || {
            diag.incoming_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        };
        if self.shutting_down {
            return refuse();
        }
        let id = hs.info_hash;
        let Some(e) = self.table.get_mut(&id) else { return refuse() };
        if e.cold.want != Want::Active || e.cold.has(F_QUEUED) || e.cold.awaiting() || self.mover.blocks(&id) {
            return refuse();
        }
        // A peer asking for a finished torrent is activity (Sharing groups); the tick refines it.
        if e.cold.finished() {
            e.cold.last_activity = unix_secs() as u32;
        }
        let msg = TorrentMsg::Incoming(Box::new(crate::torrent::IncomingConn { stream, hs, addr, permit }));
        match &e.live {
            Some(l) if l.stopping => refuse(),
            Some(l) => {
                if l.tx.try_send(msg).is_err() {
                    refuse();
                }
            }
            None => self.spawn_torrent(id, vec![msg], Vec::new()),
        }
    }
}

/// Delete a torrent's files and the directories that become empty (never outside the save
/// directory, and for placed torrents never above the library root).
fn delete_data(
    save: &Path,
    mi: &Metainfo,
    id: &TorrentId,
    staging: Option<&Path>,
    placement: Option<&Placement>,
    placed: crate::persist::Placed,
) {
    use crate::persist::Placed;
    let is_file = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    // Files that have paths of their own (placed by an earlier version, or by a move that found a
    // name taken).
    if let Some(pl) = placement {
        for f in mi.files() {
            let Some(Some(dest)) = pl.files.get(f.index) else { continue };
            if f.is_padding() || !is_file(dest) {
                continue;
            }
            if placed == Placed::Renamed {
                let _ = std::fs::remove_file(dest);
            }
            crate::organize::prune_empty_dirs(dest, &pl.library_root);
        }
    }
    let mut bases = vec![save.to_path_buf(), save.join(".incomplete")];
    if let Some(s) = staging {
        bases.push(s.to_path_buf());
    }
    for base in bases {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for f in mi.files() {
            if f.is_padding() {
                continue;
            }
            let mut p = base.clone();
            if mi.is_multi_file() {
                p.push(&*mi.name);
            }
            p.extend(f.components());
            // Never follow a symlink out of the save directory.
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
                let _ = std::fs::remove_file(&p);
            }
            let mut d = p.parent().map(Path::to_path_buf);
            while let Some(dir) = d {
                if dir == base || !dir.starts_with(&base) {
                    break;
                }
                if !dirs.contains(&dir) {
                    dirs.push(dir.clone());
                }
                d = dir.parent().map(Path::to_path_buf);
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            let _ = trs_storage::remove_dir_if_empty(&d);
        }
    }
    // Staged copies of custom-path files and the move journal.
    let sdir = staging.map_or_else(|| save.join(".incomplete"), Path::to_path_buf);
    let _ = std::fs::remove_file(sdir.join(format!("{}.journal", hex(id))));
    if let Ok(rd) = std::fs::read_dir(sdir.join(".custom")) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = std::fs::remove_dir(sdir.join(".custom"));
    let _ = std::fs::remove_dir(&sdir);
}
