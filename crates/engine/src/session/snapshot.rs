//! Read-only views: `snapshot()` and `torrent_detail()`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::oneshot;
use trs_metainfo::Metainfo;

use super::cold::*;
use super::{DetailJob, Entry, Session, SessionMsg};
use crate::api::*;
use crate::policy::PolicyData;
use crate::tier::Tier;
use crate::torrent::{DetailParts, TorrentCmd, TorrentMsg};
use crate::tracker_driver::TrackerCmd;
use crate::util::unix_secs;

impl Session {
    pub(super) fn row(&self, e: &Entry, pol: Option<&PolicyData>) -> TorrentRow {
        let c = &e.cold;
        let live = e.live.as_ref().map(|l| &*l.stats);
        let mut state = c.state(live);
        // Needs to verify and every hashing slot is taken: "Queued for checking".
        let waiting_verify = self.waiting_to_verify(&c.id);
        if waiting_verify {
            state = TorrentState::Checking;
        }
        let now = crate::util::unix_secs();
        let group = self.seed_group(c, live, now);
        let terr = self.tracker_errs.get(&c.id);
        let g = |f: fn(&crate::torrent::LiveStats) -> u64| live.map_or(0, f);
        let down = g(|l| l.down_rate.load(Ordering::Relaxed));
        let up = g(|l| l.up_rate.load(Ordering::Relaxed));
        // Time left is what the wanted files still need (skipped files do not count).
        let (session_done, session_size) = c.session();
        let wanted_left = c.wanted_len.saturating_sub(c.wanted_done());
        let eta = (state == TorrentState::Downloading && down > 0).then(|| wanted_left / down);
        TorrentRow {
            id: c.id,
            name: c.name.to_string(),
            state,
            total_bytes: c.total_len,
            wanted_bytes: c.wanted_len,
            done_bytes: c.done_bytes(),
            session_done,
            session_size,
            down_rate: down,
            up_rate: up,
            peers: live.map_or(0, |l| l.peers.load(Ordering::Relaxed)),
            seeds: live.map_or(0, |l| l.seeds.load(Ordering::Relaxed)),
            swarm_seeds: self.swarm.get(&c.id).and_then(|s| s.0),
            swarm_leechers: self.swarm.get(&c.id).and_then(|s| s.1),
            known_peers: live.map_or(0, |l| l.known.load(Ordering::Relaxed)),
            eta_secs: eta,
            ratio: c.ratio(),
            tracker_host: self.hosts.get(usize::from(c.host)).map(|h| h.to_string()).unwrap_or_default(),
            tier: live.map_or(Tier::Cold, |l| l.tier()),
            private: c.private(),
            finished_at: (c.finished_at != 0).then_some(u64::from(c.finished_at)),
            added_at: u64::from(c.added_at),
            // The short reason of the Sharing row: a disk error, or what the trackers said.
            error: c.error.as_deref().map(str::to_owned).or_else(|| {
                matches!(group, Some(SeedGroup::Error | SeedGroup::Dead)).then(|| terr.map(|t| t.reason.to_string())).flatten()
            }),
            save_path: self.save_path(c).join(&*c.name),
            priority: c.priority(),
            queue_pos: c.queue_pos,
            uploaded_total: c.counters.up_total(),
            downloaded_total: c.counters.down_total(),
            category: c.category(),
            seed_goal: c.seed_goal,
            tracker_profile: ColdTorrent::opt(c.tracker_profile).and_then(|i| self.ids.get(i)).map(str::to_owned),
            rule: ColdTorrent::opt(c.rule).and_then(|i| self.ids.get(i)).map(str::to_owned),
            rule_in_effect: pol.and_then(|p| self.rule_in_effect(c, p)),
            kind: RowKind::Torrent,
            url: None,
            up_limit: c.up_limit(),
            organized: c.has2(G_PLACED),
            seed_group: group,
            last_activity: c.finished().then_some(u64::from(c.last_activity)),
            error_since: (c.error_since != 0).then_some(u64::from(c.error_since)),
            next_retry_ms: terr.and_then(|t| t.retry_at_ms).or_else(|| self.retry_at_ms(&c.id)),
            moving: self.moving_progress(&c.id),
            move_note: self.mover.auto_failed.get(&c.id).cloned(),
            title: c.title.as_deref().map(str::to_owned).unwrap_or_default(),
            seeded_secs: c.seeded_secs(unix_secs()),
            seed_goal_user: c.has2(G_GOAL_USER),
            pending: self.pending_status(&c.id),
            tracker_error: terr.map(|t| t.reason.to_string()),
            tracker_halted: terr.is_some_and(|t| t.permanent),
            tracker_detail: terr.map(|t| t.detail.to_string()),
            announcing_since_ms: self.announcing.get(&c.id).copied(),
            tracker_failures: terr.map_or(0, |t| t.failures),
            disk_error: c.error.as_deref().map(str::to_owned),
            error_transient: terr.is_some_and(|t| !t.permanent)
                && c.error.is_none()
                && c.want != Want::Error
                && c.error_since != 0
                && now.saturating_sub(u64::from(c.error_since)) <= super::sharing::DAY,
            stalled: self.stall.get(&c.id).is_some_and(|s| s.inactive()) && state == TorrentState::Downloading,
            hold_note: self.hold_note(&c.id, state, waiting_verify),
            idle: c.idle_status(),
        }
    }

    /// The rule that governs `c` (None = the Default rule).
    pub(super) fn rule_in_effect(&self, c: &ColdTorrent, pol: &PolicyData) -> Option<String> {
        let profile = ColdTorrent::opt(c.tracker_profile).and_then(|i| self.ids.get(i)).filter(|p| pol.profiles.get(p).is_some());
        let m = crate::policy::ProfileMatch {
            profile: profile.map(str::to_owned),
            linked_rule: profile.and_then(|p| pol.profiles.get(p)).and_then(|p| p.rule.clone()),
        };
        let forced = ColdTorrent::opt(c.rule).and_then(|i| self.ids.get(i));
        crate::policy::resolve(&pol.rules, &m, forced, c.private(), c.category().unwrap_or(trs_classify::Category::Other), pol.private_min_seed, pol.idle).rule
    }

    /// Sums over the table: no rows, no strings.
    pub(super) fn totals(&self) -> Totals {
        let mut t = Totals {
            connections: self.sh.budget.established() as u32,
            half_open: self.sh.budget.half_open() as u32,
            ..Totals::default()
        };
        for e in self.table.values() {
            let c = &e.cold;
            let live = e.live.as_ref().map(|l| &*l.stats);
            t.torrents += 1;
            t.done_bytes += c.done_bytes();
            t.total_bytes += c.total_len;
            if let Some(l) = live {
                t.down_rate += l.down_rate.load(Ordering::Relaxed);
                t.up_rate += l.up_rate.load(Ordering::Relaxed);
                t.peers += u64::from(l.peers.load(Ordering::Relaxed));
            }
            match c.state(live) {
                TorrentState::Checking => t.checking += 1,
                TorrentState::Seeding | TorrentState::LowPower => t.seeding += 1,
                TorrentState::Error | TorrentState::MissingData => t.errors += 1,
                _ => t.other += 1,
            }
        }
        t
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        let pol = self.sh.policy();
        let mut rows: Vec<TorrentRow> = self.table.values().map(|e| self.row(e, pol.as_deref())).collect();
        rows.extend(self.direct_rows());
        rows.sort_by_key(|r| (r.queue_pos, r.added_at, r.id));
        let mut counts = Counts { total: rows.len() as u32, ..Counts::default() };
        let (mut down, mut up) = (0, 0);
        for r in &rows {
            down += r.down_rate;
            up += r.up_rate;
            match r.state {
                TorrentState::Downloading | TorrentState::Checking | TorrentState::FetchingMetadata => counts.downloading += 1,
                TorrentState::Queued => counts.queued += 1,
                // Direct downloads are never seeded, so they are never "Sharing".
                TorrentState::Seeding | TorrentState::LowPower => counts.seeding += 1,
                TorrentState::Paused => counts.paused += 1,
                TorrentState::MissingData | TorrentState::Error => counts.errors += 1,
                TorrentState::Completed | TorrentState::AwaitingChoice | TorrentState::Pending => {}
            }
            // Tiers describe torrents only.
            if r.kind == RowKind::Torrent {
                match r.tier {
                    Tier::Cold => counts.cold += 1,
                    Tier::Warm => counts.warm += 1,
                    Tier::Hot => counts.hot += 1,
                }
            }
        }
        let cfg = self.sh.cfg();
        Snapshot {
            rows,
            down_rate: down,
            up_rate: up,
            counts,
            alt_speed: cfg.alt_active(),
            alt_speed_scheduled: cfg.alt_scheduled_on && !cfg.alt_speed,
            down_limit: cfg.effective_down(),
            up_limit: cfg.effective_up(),
            connections: self.sh.budget.established() as u32,
            connecting: self.sh.budget.half_open() as u32,
            listen_port: self.sh.port(),
            port_fallback: self.sh.fallback(),
            online: self.sh.net.offline_since().is_none(),
            offline_since_ms: self.sh.net.offline_since(),
        }
    }

    pub(super) fn detail(&mut self, id: TorrentId, reply: oneshot::Sender<Result<TorrentDetail, EngineError>>) {
        if let Some(d) = self.directs.get(&id) {
            let row = d.row();
            let files = vec![FileInfo {
                path: row.name.clone(),
                length: row.total_bytes,
                done: row.done_bytes,
                priority: FilePriority::Normal,
                padding: false,
                uploaded: 0,
            }];
            let detail = TorrentDetail { row, files, trackers: Vec::new(), peers: Vec::new(), piece_count: 0, piece_length: 0, comment: None };
            let _ = reply.send(Ok(detail));
            return;
        }
        let Some(e) = self.table.get(&id) else {
            let _ = reply.send(Err(EngineError::NotFound));
            return;
        };
        let row = self.row(e, self.sh.policy().as_deref());
        let live_tx = e.live.as_ref().filter(|l| !l.stopping).map(|l| l.tx.clone());
        let has_meta = e.cold.has_metadata();
        let (complete, prios) = (e.cold.finished(), e.cold.file_prios.clone());
        let store = self.sh.store.clone();
        let (ttx, trx) = oneshot::channel();
        self.tracker_send(TrackerCmd::Info(id, ttx));
        let stx = self.sh.session.clone();
        tokio::spawn(async move {
            let parts: Option<DetailParts> = match live_tx {
                Some(tx) => {
                    let (rtx, rrx) = oneshot::channel();
                    if tx.send(TorrentMsg::Cmd(TorrentCmd::Detail(rtx))).await.is_ok() {
                        tokio::time::timeout(Duration::from_secs(3), rrx).await.ok().and_then(Result::ok)
                    } else {
                        None
                    }
                }
                None if has_meta => tokio::task::spawn_blocking(move || cold_parts(&*store, &id, complete, prios.as_deref()))
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let trackers = tokio::time::timeout(Duration::from_secs(2), trx).await.ok().and_then(Result::ok).unwrap_or_default();
            let (files, peers, pc, pl, comment) = match parts {
                Some(p) => (p.files, p.peers, p.piece_count, p.piece_length, p.comment),
                None => (Vec::new(), Vec::new(), 0, 0, None),
            };
            let detail = Ok(TorrentDetail { row, files, trackers, peers, piece_count: pc, piece_length: pl, comment });
            let _ = stx.send(SessionMsg::DetailReady(Box::new(DetailJob { reply, detail }))).await;
        });
        let _ = unix_secs();
    }
}

/// Detail of a Cold torrent, read from its stored `.torrent`.
fn cold_parts(store: &dyn crate::persist::Store, id: &TorrentId, complete: bool, prios: Option<&[u8]>) -> Option<DetailParts> {
    let bytes = store.load_torrent_file(id).ok()??;
    let mi = Metainfo::from_bytes(&bytes).ok()?;
    // Placed files are reported at their absolute destination, like a live torrent's layout does.
    let placed = store.load_placement(id).ok().flatten().map(|p| p.files).unwrap_or_default();
    let files = mi
        .files()
        .map(|f| FileInfo {
            path: match placed.get(f.index) {
                Some(Some(abs)) => abs.to_string_lossy().into_owned(),
                _ => f.path.to_owned(),
            },
            length: f.length,
            priority: FilePriority::from_u8(prios.and_then(|p| p.get(f.index)).copied().unwrap_or(2)),
            done: if complete && prios.and_then(|p| p.get(f.index)).copied().unwrap_or(2) > 0 { f.length } else { 0 },
            padding: f.is_padding(),
            uploaded: 0,
        })
        .collect();
    Some(DetailParts {
        files,
        peers: Vec::new(),
        piece_count: mi.piece_count() as u32,
        piece_length: mi.piece_length,
        comment: mi.comment.as_deref().map(str::to_owned),
    })
}
