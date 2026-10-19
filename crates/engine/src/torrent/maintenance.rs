//! Periodic work, piece results, choking and tier changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use trs_storage::{Mode, PieceOutcome};
use trs_wire::target_depth;

use super::{DetailParts, PeerHave, Torrent, TorrentMsg, TorrentReport};
use crate::api::{FileInfo, FilePriority, PeerInfo};
use crate::choker::PeerView;
use crate::peer::PeerCmd;
use crate::picker::piece_priorities;
use crate::tier::{self, Tier, TierInputs};

/// A peer that owes us blocks and sends none for this long is snubbed.
const SNUB_AFTER: Duration = Duration::from_secs(60);
/// A snubbed peer that stays silent this much longer is dropped.
const SNUB_DROP_AFTER: Duration = Duration::from_secs(120);

impl Torrent {
    /// Called once startup finished and the torrent can talk to peers.
    pub(super) async fn after_ready(&mut self, first: Vec<TorrentMsg>) {
        let now = Instant::now();
        if let Some(p) = self.pending_prios.take() {
            self.set_file_priorities(p);
        }
        if let Some(sw) = self.pending_switch.take() {
            self.switch_location(*sw);
        }
        self.publish_stats();
        for m in first {
            self.handle(m).await;
        }
        self.connect_more(now);
        self.maybe_finalize();
    }

    /// The torrent's files live somewhere else now (SPEC 7A.5). The session persisted the change
    /// already; this makes storage use the new paths. Reads in flight finish on the old file
    /// (their descriptors stay valid), the next ones open the new path.
    pub(super) fn switch_location(&mut self, sw: super::Switch) {
        if self.ctx.disk.get().is_none() || self.meta.is_none() {
            // Still starting up: apply when the storage is attached. A newer switch replaces an
            // older one that never ran; its sender hears that nothing was done.
            if let Some(old) = self.pending_switch.replace(Box::new(sw))
                && let Some(r) = old.reply {
                    let _ = r.send(Err("superseded by a newer change of location".into()));
                }
            return;
        }
        let super::Switch { layout, save_path, staging, placement, renames, absent, reply } = sw;
        let result = self.apply_switch(layout, save_path, staging, placement, &renames, &absent);
        if let Some(r) = reply {
            let _ = r.send(result);
        }
    }

    pub(super) fn apply_switch(
        &mut self,
        layout: Arc<trs_storage::Layout>,
        save_path: PathBuf,
        staging: Option<PathBuf>,
        placement: Option<crate::persist::Placement>,
        renames: &[(PathBuf, PathBuf)],
        absent: &[PathBuf],
    ) -> Result<(), String> {
        let (Some(d), Some(m)) = (self.ctx.disk.get(), self.meta.as_mut()) else { return Err("storage is not attached".into()) };
        let ops = !renames.is_empty() || !absent.is_empty();
        if ops && self.finalizing {
            // The finished files are being moved out of staging with the old layout's paths.
            return Err(super::SWITCH_BUSY.into());
        }
        let stage = staging.clone().filter(|_| m.staged);
        d.handle.set_paths(save_path.clone(), stage);
        if ops {
            let call = self.cfg.edge_hook.clone().map(|h| move || (h.0)());
            let r = d.handle.switch_layout_renaming(
                layout.clone(),
                renames,
                absent,
                call.as_ref().map(|c| c as &dyn Fn()),
            );
            r.map_err(|e| e.to_string())?;
        } else {
            d.handle.switch_layout(layout.clone()).map_err(|e| e.to_string())?;
        }
        m.layout = layout;
        self.save_path = save_path;
        self.staging = staging;
        self.placement = placement;
        // File fingerprints (size + mtime) stay valid: a move keeps both. Resume data is rewritten
        // with the next periodic save.
        self.resume_dirty = true;
        Ok(())
    }

    /// How long the actor may sleep.
    pub(super) fn next_wait(&self) -> Duration {
        let busy = !self.peers.is_empty() || self.downloading || self.checking || self.meta_fetch.is_some();
        if busy {
            return Duration::from_secs(1);
        }
        let now = Instant::now();
        let inputs = self.tier_inputs(now);
        let mut d = tier::next_check(&inputs, &self.cfg.tiers).unwrap_or(Duration::from_secs(3600));
        if self.resume_dirty {
            d = d.min((self.last_resume + self.cfg.resume_interval).saturating_duration_since(now));
        }
        // An upload-only torrent with nobody to dial (no leechers, or only known seeds) sleeps.
        let nothing_to_dial = self.upload_only_now() && (self.no_leechers || self.pool.len() <= self.pool.seed_count());
        if self.pool.len() > 0 && self.meta.is_some() && !nothing_to_dial {
            d = d.min(Duration::from_secs(15));
        }
        d.clamp(Duration::from_millis(500), Duration::from_secs(3600))
    }

    fn tier_inputs(&self, now: Instant) -> TierInputs {
        TierInputs {
            tier: self.tier,
            downloading: self.downloading || self.meta.as_ref().is_some_and(|m| m.picker.is_some()),
            verifying: self.checking,
            interested_peers: self.peers.values().filter(|p| p.peer_interested).count(),
            uploading_for: self.uploading_since.map(|s| now.saturating_duration_since(s)),
            last_demand: self.last_demand,
            now,
        }
    }

    pub(super) async fn tick(&mut self) {
        let now = Instant::now();
        if self.shutting_down {
            return;
        }
        self.sample_rates(now);
        self.expire_requests(now);
        self.refresh_all_interest();
        self.refill_all();
        self.connect_more(now);
        if now.saturating_duration_since(self.last_rechoke) >= self.cfg.rechoke_interval.min(Duration::from_secs(10)) {
            self.rechoke(now);
        } else {
            self.unchoke_waiting(now);
        }
        self.pex_tick(now);
        self.meta_tick(now);
        self.resume_tick(now);
        self.tier_tick(now);
        self.want_peers_tick(now);
        self.publish_stats();
    }

    fn sample_rates(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f64().max(0.001);
        self.last_tick = now;
        let (mut down, mut up) = (0.0f64, 0.0f64);
        let mut interested = 0u32;
        let mut uploading = 0u32;
        for p in self.peers.values_mut() {
            p.down.update(p.shared.down.load(Ordering::Relaxed), dt);
            p.up.update(p.shared.up.load(Ordering::Relaxed), dt);
            down += p.down.per_sec;
            up += p.up.per_sec;
            if p.peer_interested {
                interested += 1;
            }
            if p.up.per_sec > 0.0 {
                uploading += 1;
            }
            p.depth = target_depth(p.down.per_sec as u64, 250, Some(p.reqq)).max(8);
        }
        if down > 0.0 || up > 0.0 {
            // Payload moving over established connections proves the network works (SPEC 7C.4c):
            // one clock read a second per busy torrent, none per packet. Without this a seed with
            // steady peers and no new handshakes reads two dead tracker hosts as an outage.
            self.sh.net.alive();
        }
        if up > 0.0 {
            self.uploading_since.get_or_insert(now);
            self.last_demand = now;
        } else {
            self.uploading_since = None;
        }
        if interested > 0 || self.downloading || self.checking {
            self.last_demand = now;
        }
        self.live.down_rate.store(down as u64, Ordering::Relaxed);
        self.live.up_rate.store(up as u64, Ordering::Relaxed);
        self.live.interested.store(interested, Ordering::Relaxed);
        self.live.uploading.store(uploading, Ordering::Relaxed);
        self.publish_peer_counts();
    }

    fn expire_requests(&mut self, now: Instant) {
        let mut drop_keys = Vec::new();
        let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) else { return };
        for (k, p) in self.peers.iter_mut() {
            if let Some(at) = p.snubbed_at {
                if now.saturating_duration_since(at) >= SNUB_DROP_AFTER {
                    drop_keys.push(*k);
                }
                continue;
            }
            if p.inflight.is_empty() || now.saturating_duration_since(p.last_block) < SNUB_AFTER {
                continue;
            }
            // Snubbed: take the work back and let someone else do it.
            for r in p.inflight.drain(..) {
                pk.request_lost(r);
                let _ = p.tx.try_send(PeerCmd::Cancel(r));
            }
            p.snubbed_at = Some(now);
        }
        for k in drop_keys {
            self.disconnect(k);
        }
    }

    /// Report our demand to the cross-torrent allocator and take the upload slots it grants.
    /// Called by every rechoke and by the first `interested` of an idle torrent, so a new leecher
    /// need not wait for the next rechoke: a torrent without demand holds 0 slots, which used to
    /// delay its first unchoke by up to 10 s (the "slow first unchoke" of the hot benchmark).
    pub(super) fn refresh_slots(&mut self, now: Instant) {
        self.slots_at = now;
        let interested = self.peers.values().filter(|p| p.connected && p.peer_interested).count();
        self.sh.alloc.report(self.id, interested as u32, self.effective_priority().weight(), now);
        self.slots = self
            .sh
            .alloc
            .slots_for(&self.id, self.cfg.total_upload_slots(), now)
            .max(usize::from(interested > 0));
    }

    /// Interested peers that `quick_unchoke` could not serve (it re-asks the allocator at most 4 times a
    /// second, so the third leecher of a torrent whose peers arrive within 250 ms of each other was left
    /// choked until the next 10 s rechoke) get a free slot at the next tick instead. Found as the 8-10 s
    /// tail of a hot-bench round over uTP, whose slower handshakes make such near-simultaneous arrivals likely.
    fn unchoke_waiting(&mut self, now: Instant) {
        let mut waiting: Vec<u32> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && p.peer_interested && p.am_choking)
            .map(|(k, _)| *k)
            .collect();
        if waiting.is_empty() {
            return;
        }
        self.refresh_slots(now);
        let unchoked = self.peers.values().filter(|p| !p.am_choking).count();
        waiting.sort_unstable();
        for k in waiting.into_iter().take(self.slots.saturating_sub(unchoked)) {
            self.unchoke(k);
        }
    }

    /// Choke/unchoke decisions (every 10 s) and the demand report for the cross-torrent allocator.
    pub(super) fn rechoke(&mut self, now: Instant) {
        self.last_rechoke = now;
        self.refresh_slots(now);
        let mode = self.mode();
        let views: Vec<PeerView> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(k, p)| PeerView {
                key: *k,
                interested: p.peer_interested,
                unchoked: !p.am_choking,
                rate: match mode {
                    crate::choker::Mode::Leeching => p.down.per_sec as u64,
                    crate::choker::Mode::Seeding => p.up.per_sec as u64,
                },
                snubbed: p.snubbed_at.is_some(),
                last_unchoked: p.last_unchoked,
            })
            .collect();
        let set = self.choker.rechoke(&views, self.slots, mode, now);
        let keys: Vec<u32> = views.iter().map(|v| v.key).collect();
        for k in keys {
            let want = set.contains(&k);
            let choking = self.peers.get(&k).is_none_or(|p| p.am_choking);
            if want && choking {
                self.unchoke(k);
            } else if !want && !choking {
                self.choke(k);
            }
        }
    }

    fn resume_tick(&mut self, now: Instant) {
        if self.resume_dirty && now.saturating_duration_since(self.last_resume) >= self.cfg.resume_interval {
            self.last_resume = now;
            self.resume_dirty = false;
            if let Some(r) = self.resume_record(None) {
                self.report(TorrentReport::Resume(r));
            }
        }
        if let Some((at, _)) = &self.info_cache
            && now.saturating_duration_since(*at) > Duration::from_secs(60) {
                self.info_cache = None;
            }
    }

    fn want_peers_tick(&mut self, now: Instant) {
        if self.private || !self.downloading || self.peers.len() >= 8 || self.pool.len() >= 32 {
            return;
        }
        if self.last_want_peers.is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(60)) {
            self.last_want_peers = Some(now);
            self.report(TorrentReport::WantPeers);
        }
    }

    fn tier_tick(&mut self, now: Instant) {
        if self.meta.is_none() {
            return;
        }
        let inputs = self.tier_inputs(now);
        let next = if inputs.downloading && self.meta.as_ref().is_some_and(|m| m.picker.is_some()) {
            // An incomplete torrent keeps its picker state: always Hot.
            Tier::Hot
        } else {
            tier::next_tier(&inputs, &self.cfg.tiers)
        };
        match (self.tier, next) {
            (a, b) if a == b => {}
            (Tier::Warm, Tier::Hot) => {
                self.tier = Tier::Hot;
                self.sh.diag.promotions.fetch_add(1, Ordering::Relaxed);
            }
            (Tier::Hot, Tier::Warm) => {
                self.tier = Tier::Warm;
                self.sh.diag.demotions.fetch_add(1, Ordering::Relaxed);
                self.trim_peers();
            }
            (_, Tier::Cold) if !self.demote_sent => {
                self.demote_sent = true;
                self.report(TorrentReport::Demote);
            }
            _ => {}
        }
        self.live.tier.store(
            match self.tier {
                Tier::Cold => 0,
                Tier::Warm => 1,
                Tier::Hot => 2,
            },
            Ordering::Relaxed,
        );
    }

    /// After a demotion to Warm, shrink the peer set to the smaller cap (idle peers first).
    fn trim_peers(&mut self) {
        let cap = self.peer_cap();
        if self.peers.len() <= cap {
            return;
        }
        let mut v: Vec<(u32, bool, f64)> = self
            .peers
            .iter()
            .map(|(k, p)| (*k, p.peer_interested, p.up.per_sec))
            .collect();
        v.sort_by_key(|a| (a.1, a.2.to_bits()));
        for (k, _, _) in v.into_iter().take(self.peers.len() - cap) {
            self.disconnect(k);
        }
    }

    pub(super) fn publish_stats(&self) {
        if let Some(m) = &self.meta {
            self.live
                .done_bytes
                .store(m.total_len.saturating_sub(self.counters.left()), Ordering::Relaxed);
            if let Some(pk) = &m.picker {
                self.counters.set_wanted_done(m.wanted_len.saturating_sub(pk.wanted_missing_bytes()));
            }
        }
    }

    // ---- pieces -------------------------------------------------------------------------

    pub(super) fn on_piece_hashed(&mut self, piece: u32, result: Result<PieceOutcome, String>) {
        match result {
            Ok(PieceOutcome::Verified(p)) => self.piece_verified(p),
            Ok(PieceOutcome::HashFailed(p)) => self.piece_failed(p),
            // Late duplicate blocks can make storage verify a piece twice; once we are a seed
            // its write is refused, which is harmless.
            Err(_) if self.complete() => {}
            Err(e) => match crate::peer::fault_kind_of_text(&e) {
                crate::peer::FaultKind::Blocked => self.on_disk_blocked(format!("storage error: {e}")),
                kind => self.report(TorrentReport::Fault { kind, message: format!("storage error: {e}") }),
            },
        }
        let _ = piece;
    }

    /// The disk stopped taking writes (full, read-only, permission denied). Keep what is
    /// verified and keep serving it (libtorrent's "upload mode"), ask for nothing more and tell
    /// the session, which retries later (10 min growing to 1 h).
    pub(super) fn on_disk_blocked(&mut self, message: String) {
        if self.disk_blocked {
            return;
        }
        self.disk_blocked = true;
        self.download_allowed = false;
        self.downloading = false;
        self.refresh_all_interest();
        self.report(TorrentReport::DiskBlocked { message });
    }

    fn piece_verified(&mut self, piece: u32) {
        let Some(m) = self.meta.as_mut() else { return };
        let Some(pk) = m.picker.as_mut() else { return };
        if pk.have().has(piece) {
            return;
        }
        // A piece that came from peers and passed its hash check is downloaded payload (SPEC 7B.2).
        self.counters.add_down(u64::from(pk.piece_size(piece)));
        pk.piece_verified(piece);
        if let Some(d) = self.ctx.disk.get() {
            // The bit first, then the epoch: a stream that saw "missing" and then awaits the epoch
            // cannot miss this piece (the ordering `PieceSource::subscribe` documents).
            d.have.set(piece);
            if let Some(bus) = &self.stream {
                bus.note_progress(piece, 0);
                bus.bump();
            }
        }
        self.counters.set_left(pk.left_bytes());
        let done = pk.wanted_done();
        self.contrib.remove(&piece);
        self.resume_dirty = true;
        self.last_demand = Instant::now();
        for p in self.peers.values() {
            if !p.connected {
                continue;
            }
            let already = match &p.have {
                PeerHave::Set(b) => b.has(piece),
                PeerHave::All => true,
                _ => false,
            };
            if !already {
                let _ = p.tx.try_send(PeerCmd::Have(piece));
            }
        }
        self.publish_stats();
        if done && !self.finished_reported {
            self.on_wanted_done();
        } else {
            self.refresh_all_interest();
        }
    }

    fn piece_failed(&mut self, piece: u32) {
        self.sh.diag.hash_failures.fetch_add(1, Ordering::Relaxed);
        let contributors = self.contrib.remove(&piece).unwrap_or_default();
        let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) else { return };
        self.counters.add_corrupt(u64::from(pk.piece_size(piece)));
        // One contributor: it is the culprit. Several: retry from a single source to find out.
        pk.piece_failed(piece, contributors.len() > 1);
        if let Some(bus) = &self.stream {
            bus.note_progress(piece, 0);
        }
        if let [only] = contributors[..] {
            self.strike(only, 1);
        }
        self.refill_all();
    }

    /// Every wanted piece is verified.
    fn on_wanted_done(&mut self) {
        self.finished_reported = true;
        self.downloading = false;
        self.live.wanted_done.store(true, Ordering::Relaxed);
        let (complete, files_completed, files_bytes) = match &self.meta {
            Some(m) => {
                let wanted: Vec<usize> = (0..m.layout.file_count())
                    .filter(|&i| !m.layout.file_is_padding(i) && m.file_prio.get(i).copied().unwrap_or(2) > 0)
                    .collect();
                let bytes = wanted.iter().map(|&i| m.layout.file_len(i)).sum();
                (m.picker.as_ref().is_none_or(|p| p.is_complete()), wanted.len() as u32, bytes)
            }
            None => (false, 0, 0),
        };
        let keys: Vec<u32> = self.peers.keys().copied().collect();
        for k in &keys {
            self.update_interest(*k);
        }
        if complete {
            self.become_seed();
        } else {
            // Skipped files remain: the wanted ones still have to leave `.incomplete`.
            self.maybe_finalize();
        }
        self.drop_redundant_all();
        self.report(TorrentReport::Finished { complete, files_completed, files_bytes });
        self.resume_dirty = false;
        self.last_resume = Instant::now();
        if let Some(r) = self.resume_record(None) {
            self.report(TorrentReport::Resume(r));
        }
        if complete {
            self.spawn_fingerprints();
        }
    }

    /// All pieces present: free the picker and stop being a leecher.
    fn become_seed(&mut self) {
        self.ctx.complete.store(true, Ordering::Relaxed);
        let staged = self.meta.as_ref().is_some_and(|m| m.staged);
        if let Some(m) = self.meta.as_mut() {
            m.picker = None;
            m.hashes = None;
        }
        if let Some(d) = self.ctx.disk.get() {
            d.handle.detach_hashes();
            if !staged {
                d.handle.set_mode(Mode::Seed);
            }
        }
        for p in self.peers.values_mut() {
            p.have = match std::mem::replace(&mut p.have, PeerHave::Unknown) {
                PeerHave::Set(b) => PeerHave::Count(b.count_ones()),
                PeerHave::All => PeerHave::All,
                other => other,
            };
        }
        self.drop_redundant_all();
        let _ = staged;
        self.maybe_finalize();
    }

    /// Everything wanted is on disk (all pieces, or every wanted file when some are skipped) and
    /// the data still sits in `.incomplete`: move it to its place.
    pub(super) fn maybe_finalize(&mut self) {
        let staged = self.meta.as_ref().is_some_and(|m| m.staged);
        if !staged || self.finalizing {
            return;
        }
        if self.complete() || self.live.wanted_done.load(Ordering::Relaxed) {
            self.begin_finalize();
        }
    }

    /// A storage handle whose layout leaves out the skipped files that were never written, so
    /// the move does not trip over files that do not exist (SPEC 7A.1: junk starts unchecked).
    /// Skipped files that do exist (the edge of a piece reached into them) are moved with the rest.
    fn partial_finalize_handle(&self) -> Option<trs_storage::TorrentHandle> {
        use std::path::{Path, PathBuf};
        use trs_storage::{FilePath, FileSpec, Layout, Location, PathRef, TorrentConfig};
        let (m, d) = (self.meta.as_ref()?, self.ctx.disk.get()?);
        let mut files = Vec::with_capacity(m.layout.file_count());
        for i in 0..m.layout.file_count() {
            let skipped = m.file_prio.get(i).copied().unwrap_or(2) == 0;
            let present = skipped
                && d.handle.file_path(i as u32, Location::Staging).map(|p| p.exists()).unwrap_or(false);
            let path = match m.layout.file_path(i) {
                PathRef::Relative(s) => FilePath::Relative(
                    Path::new(s).components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect(),
                ),
                PathRef::Absolute(s) => FilePath::Absolute(PathBuf::from(s)),
            };
            files.push(FileSpec {
                path,
                length: m.layout.file_len(i),
                is_padding: m.layout.file_is_padding(i) || (skipped && !present),
            });
        }
        let layout = Layout::new(m.piece_len, m.total_len, m.piece_count, files).ok()?;
        Some(self.sh.disk.add_torrent(
            Arc::new(layout),
            TorrentConfig {
                root: self.save_path.clone(),
                staging: self.staging.clone().or_else(|| Some(self.save_path.join(".incomplete"))),
                mode: Mode::Download,
                location: Location::Staging,
                preallocate: false,
            },
        ))
    }

    /// Move the finished files out of `.incomplete` without blocking the actor.
    pub(super) fn begin_finalize(&mut self) {
        let Some(d) = self.ctx.disk.get() else { return };
        self.ctx.hold_uploads.store(true, Ordering::Relaxed);
        self.finalizing = true;
        let partial = !self.complete();
        let handle = if partial {
            match self.partial_finalize_handle() {
                Some(h) => h,
                None => {
                    self.finalizing = false;
                    self.ctx.hold_uploads.store(false, Ordering::Relaxed);
                    self.report(TorrentReport::Fault { kind: crate::peer::FaultKind::Other, message: "cannot plan the move of the finished files".into() });
                    return;
                }
            }
        } else {
            d.handle.clone()
        };
        let staging = self.staging.clone().unwrap_or_else(|| self.save_path.join(".incomplete"));
        let name = if partial { format!("{}.p.journal", crate::util::hex(&self.id)) } else { format!("{}.journal", crate::util::hex(&self.id)) };
        let journal = staging.join(name);
        // A journal from a run with a different set of skipped files cannot be replayed: the
        // file system tells the truth anyway, so start a new one.
        if partial {
            let _ = std::fs::remove_file(&journal);
        }
        // Skipped files can still be ticked while the finished ones move: their first writes would
        // land in the staging tree the move is emptying. Hold the torrent's writes from before the
        // first rename until `on_finalized` has switched the location (the held blocks are asked
        // for again by the peer tasks).
        let hold = partial.then(|| d.handle.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = async {
                if let Some(h) = &hold {
                    h.quiesce_writes()?.await?;
                }
                handle.finalize(journal, trs_storage::FinalizeOptions::default())?.await.map(|_| ())
            }
            .await
            .map_err(|e| e.to_string());
            if r.is_err()
                && let Some(h) = &hold {
                    h.resume_writes();
                }
            let _ = tx.send(TorrentMsg::Finalized(r)).await;
        });
    }

    pub(super) fn on_finalized(&mut self, r: Result<(), String>) {
        match r {
            Ok(()) => {
                let complete = self.complete();
                if let Some(d) = self.ctx.disk.get() {
                    d.handle.set_location(trs_storage::Location::Final);
                    d.handle.resume_writes();
                    // A torrent with skipped files may still download the rest later.
                    if complete {
                        d.handle.set_mode(Mode::Seed);
                    }
                }
                if let Some(m) = self.meta.as_mut() {
                    m.staged = false;
                }
                self.finalizing = false;
                self.ctx.hold_uploads.store(false, Ordering::Relaxed);
                self.report(TorrentReport::Finalized);
                if complete {
                    self.spawn_fingerprints();
                }
            }
            Err(e) => {
                self.finalizing = false;
                self.report(TorrentReport::Fault {
                    kind: crate::peer::FaultKind::Other,
                    message: format!("moving the finished files failed: {e}"),
                });
            }
        }
    }

    pub(super) fn set_file_priorities(&mut self, prios: Vec<u8>) {
        let Some(m) = self.meta.as_mut() else {
            self.pending_prios = Some(prios);
            return;
        };
        let n = m.layout.file_count();
        let mut fp = prios;
        fp.resize(n, 2);
        let pp = piece_priorities(
            m.piece_len,
            m.piece_count,
            (0..n).map(|i| (m.layout.file_start(i), m.layout.file_len(i), fp[i], m.layout.file_is_padding(i))),
        );
        m.wanted_len = (0..n).filter(|&i| !m.layout.file_is_padding(i) && fp[i] > 0).map(|i| m.layout.file_len(i)).sum();
        m.file_prio = fp;
        let Some(pk) = m.picker.as_mut() else { return };
        pk.set_priorities(pp);
        self.counters.set_wanted_done(m.wanted_len.saturating_sub(pk.wanted_missing_bytes()));
        let done = pk.wanted_done();
        let was_done = self.live.wanted_done.swap(done, Ordering::Relaxed);
        if !done {
            self.finished_reported = false;
            self.downloading = self.download_allowed;
            if was_done {
                // The wanted set grew after it was finished: a download again (SPEC 8.3).
                self.report(TorrentReport::Unfinished);
            }
        } else if !self.finished_reported {
            self.on_wanted_done();
        }
        self.refresh_all_interest();
        self.refill_all();
        if !done {
            // Remembered seeds are candidates again.
            self.connect_more(Instant::now());
        }
    }

    pub(super) fn detail(&self) -> DetailParts {
        let mut files = Vec::new();
        let (mut piece_count, mut piece_len, mut comment) = (0, 0, None);
        if let Some(m) = &self.meta {
            piece_count = m.piece_count;
            piece_len = m.piece_len;
            comment = m.comment.clone();
            let complete = self.complete();
            let have = m.picker.as_ref().map(|p| p.have());
            let served = self.ctx.file_up.snapshot();
            for i in 0..m.layout.file_count() {
                let len = m.layout.file_len(i);
                let range = m.layout.pieces_of_file(i);
                let done = if complete || len == 0 {
                    len
                } else if let Some(h) = have {
                    let total = range.len().max(1) as u64;
                    // A skipped file does not count pieces it only shares with a wanted neighbour.
                    let skipped = m.file_prio.get(i).copied().unwrap_or(2) == 0;
                    let pk = m.picker.as_ref();
                    let got = range
                        .clone()
                        .filter(|&p| h.has(p) && !(skipped && pk.is_some_and(|k| k.piece_wanted(p))))
                        .count() as u64;
                    len * got / total
                } else {
                    0
                };
                let path = match m.layout.file_path(i) {
                    trs_storage::PathRef::Relative(p) | trs_storage::PathRef::Absolute(p) => {
                        p.to_string_lossy().into_owned()
                    }
                };
                files.push(FileInfo {
                    path,
                    length: len,
                    done,
                    priority: FilePriority::from_u8(m.file_prio.get(i).copied().unwrap_or(2)),
                    padding: m.layout.file_is_padding(i),
                    uploaded: served.as_ref().and_then(|v| v.get(i)).copied().unwrap_or(0),
                });
            }
        }
        let pc = piece_count.max(1) as f32;
        let peers = self
            .peers
            .values()
            .filter(|p| p.connected)
            .map(|p| PeerInfo {
                addr: p.addr,
                client: p.client.as_deref().unwrap_or("").to_owned(),
                outgoing: !p.incoming,
                down_rate: p.down.per_sec as u64,
                up_rate: p.up.per_sec as u64,
                progress: match &p.have {
                    PeerHave::All => 1.0,
                    PeerHave::Set(b) => b.count_ones() as f32 / pc,
                    PeerHave::Count(n) => *n as f32 / pc,
                    PeerHave::Unknown => 0.0,
                },
                choking_us: p.peer_choking,
                choked_by_us: p.am_choking,
                interested: p.peer_interested,
                utp: p.utp,
                requests_in_flight: p.inflight.len() as u32,
                connected_secs: p.connected_at.elapsed().as_secs(),
                snubbed: p.snubbed_at.is_some(),
                upload_only: p.upload_only,
                has_pex: p.has_pex && self.pex_allowed(),
            })
            .collect();
        DetailParts { files, peers, piece_count, piece_length: piece_len, comment }
    }
}
