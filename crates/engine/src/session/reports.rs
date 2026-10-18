//! Reports from torrent actors, peers from trackers and the DHT, and shutdown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use trs_metainfo::{Magnet, Metainfo};

use super::cold::*;
use super::{After, Session};
use crate::api::*;
use crate::persist::{Batch, StatDelta};
use crate::torrent::{ReadyInfo, TorrentMsg, TorrentReport};
use crate::tracker_driver::{TrackerCmd, TrackerTorrent};
use crate::util::unix_secs;

impl Session {
    pub(super) fn on_report(&mut self, id: TorrentId, epoch: u64, r: TorrentReport) {
        let Some(e) = self.table.get(&id) else { return };
        if e.live.as_ref().map(|l| l.epoch) != Some(epoch) {
            return;
        }
        match r {
            TorrentReport::Ready(info) => {
                self.on_ready(id, *info);
                self.request_trim();
            }
            TorrentReport::Finished { complete, files_completed, files_bytes } => {
                self.on_finished(id, complete, files_completed, files_bytes)
            }
            TorrentReport::Unfinished => self.reopen_download(id),
            TorrentReport::Resume(r) => self.dirty_resume(id, Some(r)),
            TorrentReport::MetadataReady { info, peers } => self.on_metadata(id, info, peers),
            TorrentReport::Fault { kind, message } => self.on_fault(id, kind, message),
            TorrentReport::DiskBlocked { message } => self.on_disk_blocked(id, message),
            TorrentReport::Demote => {
                self.sh.diag.demotions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.stop_live(id, After::Nothing);
                self.request_trim();
            }
            TorrentReport::WantPeers => {
                self.dht_lookup(id, false);
            }
            TorrentReport::Banned => {}
            TorrentReport::Finalized => {
                if let Some(e) = self.table.get_mut(&id) {
                    e.cold.set(F_STAGED, false);
                    if e.cold.has(F_WANTED_DONE) {
                        self.sh.emit(EngineEvent::Finished { id });
                    }
                }
                self.release_reservations(id);
                self.mark_record(id);
                // A goal that waited for the files to reach their place (stop-when-finished).
                self.goal_check();
            }
        }
    }

    fn on_ready(&mut self, id: TorrentId, info: ReadyInfo) {
        let (info_complete, info_wanted_done) = (info.complete, info.wanted_done);
        let cfg = self.sh.cfg();
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        let first = c.has2(G_UNVERIFIED);
        c.name = info.name.clone().into();
        c.total_len = info.total_length;
        c.wanted_len = info.wanted_length;
        c.piece_len = info.piece_length;
        c.set(F_PRIVATE, info.private);
        c.set(F_COMPLETE, info.complete);
        let was_finished = c.finished_at != 0;
        c.set(F_WANTED_DONE, info.wanted_done);
        c.set(F_STAGED, info.staged);
        c.set2(G_UNVERIFIED, false);
        if first && info.complete && c.counters.down_total() == 0 {
            c.set(F_ADDED_COMPLETE, true);
        }
        if info.wanted_done && c.finished_at == 0 {
            // Reopened while idle but nothing was missing after all: the original time stands.
            c.finished_at = self.reopened_at.remove(&id).unwrap_or(unix_secs() as u32);
            c.set2(G_REOPENED, false);
        } else if !info.wanted_done && was_finished {
            // A recheck (or a launch) found wanted pieces missing: a download again, not "recent".
            c.finished_at = 0;
            c.set2(G_REOPENED, true);
            c.session_base = c.counters.wanted_done().min(c.wanted_len);
        }
        if info.wanted_done {
            c.session_base = 0;
            c.counters.set_wanted_done(c.wanted_len);
        }
        let register = cfg.trackers_enabled && !c.has(F_TRACKER_REG) && info.announce.iter().any(|t| !t.is_empty());
        if register {
            c.set(F_TRACKER_REG, true);
            let t = TrackerTorrent {
                id,
                private: info.private,
                added_complete: c.has(F_ADDED_COMPLETE),
                tiers: info.announce,
                counters: c.counters.clone(),
                size: info.total_length,
            };
            self.tracker_send(TrackerCmd::Add(t));
        }
        self.mark_record(id);
        self.mark_counters(id);
        self.update_seed_clock(id);
        self.update_error(id);
        self.sync_role(id);
        self.replan();
        self.reconcile(id);
        if info_complete || info_wanted_done {
            // Found finished (cross-seeding, restart): a goal may already be reached.
            self.goal_check_soon();
        }
        if self.table.get(&id).is_some_and(|e| self.dht_ok(&e.cold)) {
            self.dht_lookup(id, false);
        }
        if let Some(e) = self.table.get(&id) {
            let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
    }

    fn on_finished(&mut self, id: TorrentId, complete: bool, files_completed: u32, files_bytes: u64) {
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        c.set(F_WANTED_DONE, true);
        c.session_base = 0;
        c.counters.set_wanted_done(c.wanted_len);
        c.set(F_COMPLETE, complete);
        if c.finished_at == 0 {
            c.finished_at = unix_secs() as u32;
        }
        let again = c.has2(G_REOPENED);
        c.set2(G_REOPENED, false);
        let domain = self.hosts.get(usize::from(c.host)).filter(|h| !h.is_empty()).map(|h| trs_state::trackers::registrable_domain(h));
        let private = c.private();
        let send_complete = complete && c.has(F_TRACKER_REG) && !c.has(F_ADDED_COMPLETE);
        self.update_seed_clock(id);
        // Finishing counts as the last thing that happened: a fresh seed starts Active.
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.last_activity = unix_secs() as u32;
        }
        self.sync_role(id);
        self.mark_record(id);
        self.mark_counters(id);
        if send_complete {
            self.tracker_send(TrackerCmd::Complete(id));
        }
        // A torrent that was finished before and downloaded more is no new completed torrent.
        if !again {
            self.push_stat(StatDelta {
                domain,
                private,
                at: unix_secs(),
                torrents_completed: 1,
                files_completed,
                files_bytes,
                ..StatDelta::default()
            });
        }
        // With staging the files are not in their final place until `Finalized`; announce
        // "finished" only then so a notification never points at a missing file.
        let waiting_for_move = self.table.get(&id).is_some_and(|e| e.cold.has(F_STAGED));
        if !waiting_for_move {
            self.sh.emit(EngineEvent::Finished { id });
        }
        self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::LowPower });
        self.ensure_goal_job();
        self.replan();
        // "Stop when the download finishes": the `completed` announce is queued above, the goal
        // check queues `stopped` right behind it, and nothing is ever uploaded.
        self.goal_check();
    }

    /// The wanted set of a finished torrent grew (files ticked again, or a recheck found wanted
    /// pieces missing): it is a download again. The row leaves Recent and Sharing, takes a queue
    /// slot like any download and keeps seeding what it has. `completed` is not repeated (the
    /// tracker layer keeps `completed_sent`) and `left` follows the counters. SPEC 8.3.
    pub(super) fn reopen_download(&mut self, id: TorrentId) {
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        if !c.finished() && c.finished_at == 0 {
            return;
        }
        // What the torrent holds of the new selection is where this download's progress starts. The
        // counters already describe the new selection (the actor, or the session for a Cold torrent).
        c.session_base = c.counters.wanted_done().min(c.wanted_len);
        c.set(F_WANTED_DONE, false);
        c.finished_at = 0;
        c.set2(G_REOPENED, true);
        // The seeding stretch ends here; the seconds so far stay counted (hit-and-run floor).
        self.update_seed_clock(id);
        self.sync_role(id);
        self.mark_record(id);
        self.mark_counters(id);
        self.replan();
        self.reconcile(id);
        if let Some(e) = self.table.get(&id) {
            let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
    }

    fn on_metadata(&mut self, id: TorrentId, info: Vec<u8>, peers: Vec<SocketAddr>) {
        let Ok(mut mi) = Metainfo::from_info_bytes(&info) else {
            self.on_fault(id, crate::peer::FaultKind::Other, "the downloaded metadata is not a valid torrent".into());
            return;
        };
        if mi.info_hash.swarm_id() != Some(id) || mi.piece_count() == 0 {
            self.on_fault(id, crate::peer::FaultKind::Other, "the downloaded metadata does not match".into());
            return;
        }
        let Some(e) = self.table.get(&id) else { return };
        let trackers: Vec<String> = e
            .cold
            .magnet
            .as_deref()
            .and_then(|m| Magnet::parse(m).ok())
            .map(|m| m.trackers)
            .unwrap_or_default();
        // A full .torrent: {announce-list, info}. Keys are emitted in sorted order.
        let mut enc = trs_bencode::Encoder::new();
        enc.begin_dict();
        if !trackers.is_empty() {
            enc.str("announce-list").begin_list();
            for t in &trackers {
                enc.begin_list().str(t).end();
            }
            enc.end();
        }
        enc.str("info").raw(&info).end();
        let file: Arc<[u8]> = Arc::from(enc.finish());
        // The magnet's trackers are the torrent's trackers (profiles match on them).
        mi.announce_tiers = trackers.iter().map(|t| vec![t.as_str().into()].into_boxed_slice()).collect();
        self.plan_metadata(id, mi, file, peers);
    }

    pub(super) fn on_tracker_peers(&mut self, id: TorrentId, peers: Vec<SocketAddr>, incomplete: Option<u64>) {
        let Some(e) = self.table.get(&id) else { return };
        if let Some(l) = &e.live {
            if !l.stopping {
                let _ = l.tx.try_send(TorrentMsg::TrackerPeers(peers, incomplete));
            }
            return;
        }
        // A Cold torrent wakes for a tracker response only if the tracker knows of leechers.
        if e.cold.want == Want::Active && !e.cold.has(F_QUEUED) && incomplete.is_some_and(|n| n > 0) {
            let cap = self.sh.cfg().warm_connections_per_torrent;
            self.spawn_torrent(id, Vec::new(), peers.into_iter().take(cap).collect());
        }
    }

    pub(super) fn on_dht_peers(&mut self, id: TorrentId, peers: Vec<SocketAddr>) {
        if peers.is_empty() {
            self.dht_done(&id);
            return;
        }
        let Some(e) = self.table.get(&id) else { return };
        if let Some(l) = &e.live
            && !l.stopping {
                let _ = l.tx.try_send(TorrentMsg::AddPeers(peers));
            }
    }

    // ---- shutdown -----------------------------------------------------------------------

    pub(super) async fn shutdown(&mut self, deadline: Instant, reply: oneshot::Sender<ShutdownReport>) {
        self.shutting_down = true;
        self.boot.clear();
        if let Some(l) = self.listener.take() {
            l.abort();
        }
        if let Some(l) = self.utp_listener.take() {
            l.abort();
        }
        if let Some(w) = self.watcher.take() {
            w.abort();
        }
        self.shutdown_directs();
        // Players get a clean end of their responses before anything else goes away.
        if let Some(srv) = self.sh.streams.get() {
            srv.shutdown_shared().await;
        }
        // Copies in progress stop and clean up; the torrents seed from where they were next time.
        self.mover.cancel_all();
        // The gateway exchange (deleting the port mappings, its own 3 s) and the DHT's last save do
        // not depend on the torrents or on the trackers: they start now and are joined at the end.
        let pm_task = self.portmap.take().map(|pm| tokio::spawn(async move { pm.shutdown().await }));
        let dht_task = self.dht.take().map(|d| {
            tokio::spawn(async move { tokio::time::timeout(Duration::from_secs(1), d.shutdown()).await.ok().and_then(Result::ok) })
        });
        // 1. Stop the actors; they disconnect peers and hand back resume data.
        let mut waits = Vec::new();
        for (id, e) in self.table.iter_mut() {
            if let Some(l) = e.live.as_mut() {
                if l.stopping {
                    continue;
                }
                l.stopping = true;
                let (rtx, rrx) = oneshot::channel();
                let _ = l.tx.try_send(TorrentMsg::Cmd(crate::torrent::TorrentCmd::Shutdown(rtx)));
                waits.push((*id, rrx));
            }
        }
        let budget = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(5));
        let until = Instant::now() + budget;
        for (id, rrx) in waits {
            let left = until.saturating_duration_since(Instant::now());
            if let Ok(Ok(info)) = tokio::time::timeout(left, rrx).await {
                if let Some(r) = info.resume {
                    self.dirty_resume(id, Some(r));
                }
                if !info.peers.is_empty() {
                    self.dirty_peers(id, info.peers);
                }
            }
        }
        // 2. Seed-time accounting and final statistics.
        let ids: Vec<TorrentId> = self.table.keys().copied().collect();
        for id in &ids {
            self.update_seed_clock(*id);
            self.mark_record(*id);
            self.mark_counters(*id);
        }
        self.final_stats();
        // Everything the torrents know is on disk before the (possibly long) wait for the trackers.
        self.flush_persist();
        self.writer.flush();
        // 3. Tracker: `stopped` for every open session, bounded by the deadline.
        let report = match &self.tracker {
            Some(t) => {
                let (rtx, rrx) = oneshot::channel();
                t.send(TrackerCmd::Shutdown { deadline, reply: rtx });
                let wait = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(500);
                tokio::time::timeout(wait, rrx).await.ok().and_then(Result::ok).unwrap_or_default()
            }
            None => ShutdownReport::default(),
        };
        // 3b. Port mappings are gone and the DHT state is in: both ran alongside everything above,
        // so these joins are normally instant (the gateway's own bound is 3 s, the DHT's 1 s).
        if let Some(t) = pm_task {
            let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(3));
            let _ = tokio::time::timeout(left.max(Duration::from_millis(500)), t).await;
        }
        let dht_state = match dht_task {
            Some(t) => tokio::time::timeout(Duration::from_secs(2), t).await.ok().and_then(Result::ok).flatten(),
            None => None,
        };
        // 4. The last batch.
        self.flush_persist();
        if dht_state.is_some() {
            self.writer.submit(Batch { dht_state, ..Batch::default() });
        }
        self.writer.flush();
        // An orderly quit: the next launch is not a crash (crash-loop breaker, SPEC 7C.2).
        let _ = self.sh.store.record_clean_exit();
        let _ = reply.send(report);
    }
}
