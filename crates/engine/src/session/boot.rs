//! Bringing torrents up after start-up, a few at a time.
//!
//! Everything resumes when the app starts (SPEC 10; what the user paused stays paused), but a torrent that needs an actor (it is
//! downloading, rechecking or still fetching its metadata) costs memory the moment it starts: file
//! tables, piece hashes, a bitfield, hashing buffers. Finished torrents need none (they stay Cold
//! until a peer asks), so the cost is in the others. Starting them all at once made the footprint
//! spike at launch for a library with many unfinished torrents; here they are queued and started a
//! small batch per tick, and never while the previous ones are still verifying their files.
//!
//! The same queue is the single verification gate (`EngineConfig::verify_slots`, 2): at most that
//! many torrents verify at once, whatever started them. A torrent that must verify its files
//! (added with existing data, Recheck, Recheck all, Resume all, back from an unplugged drive) and
//! finds the slots full waits here with no actor ("Waiting to verify"); `Shared::verify_slots`
//! is the backstop for verifications an actor only decides on itself.
//!
//! Order: downloads first (queue order), then rechecks, then torrents without metadata.
//! Tracker announces are not touched: the tracker hub paces its own first announces (a token
//! bucket per host, never earlier than the interval a tracker allowed), and a torrent that has not
//! been brought up yet simply registers its announce session as before.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::cold::*;
use super::{Job, Session};
use crate::api::TorrentId;

/// Torrents brought up per tick.
pub(crate) const BOOT_BATCH: usize = 2;
/// Time between ticks.
pub(crate) const BOOT_TICK: Duration = Duration::from_millis(200);

/// Torrents waiting for their turn.
#[derive(Default)]
pub(crate) struct BootQueue {
    order: VecDeque<TorrentId>,
    held: HashSet<TorrentId>,
}

impl BootQueue {
    pub(crate) fn holds(&self, id: &TorrentId) -> bool {
        self.held.contains(id)
    }

    /// Queue `id` (at the back) unless it already waits.
    pub(crate) fn hold(&mut self, id: TorrentId) {
        if self.held.insert(id) {
            self.order.push_back(id);
        }
    }

    /// Somebody asked for this torrent (resume, recheck): it does not wait any longer.
    pub(crate) fn release(&mut self, id: &TorrentId) {
        self.held.remove(id);
    }

    pub(crate) fn clear(&mut self) {
        self.order.clear();
        self.held.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }
}

impl Session {
    /// Hold back every torrent that needs an actor, in the order they will be started.
    pub(super) fn boot_hold_all(&mut self) {
        let mut want: Vec<(u8, u32, u32, TorrentId)> = Vec::new();
        for (id, e) in &self.table {
            if e.live.is_some() || !self.actor_need(id).1 {
                continue;
            }
            let c = &e.cold;
            let class = if !c.has_metadata() {
                2
            } else if c.has2(G_UNVERIFIED) || c.has2(G_RECHECK) || c.complete() || c.has(F_WANTED_DONE) {
                1
            } else {
                0
            };
            want.push((class, c.queue_pos, c.added_at, *id));
        }
        want.sort_unstable();
        for (_, _, _, id) in want {
            self.boot.order.push_back(id);
            self.boot.held.insert(id);
        }
        self.sh.diag.boot_waiting.store(self.boot.len() as u64, Ordering::Relaxed);
    }

    /// Torrents with an actor that is loading, validating or hashing their files.
    pub(crate) fn verifying_count(&self) -> usize {
        self.table.values().filter(|e| e.live.as_ref().is_some_and(|l| l.stats.verifying.load(Ordering::Relaxed))).count()
    }

    /// `id` must hash its files before it can run and every slot is taken.
    pub(crate) fn must_wait_to_verify(&self, id: &TorrentId) -> bool {
        let Some(e) = self.table.get(id) else { return false };
        (e.cold.has2(G_UNVERIFIED) || e.cold.has2(G_RECHECK)) && self.verifying_count() >= self.sh.cfg().verify_slots.max(1)
    }

    /// Is `id` waiting for a hashing slot ("Waiting to verify")?
    pub(crate) fn waiting_to_verify(&self, id: &TorrentId) -> bool {
        let Some(e) = self.table.get(id) else { return false };
        match &e.live {
            Some(l) => l.stats.verify_wait.load(Ordering::Relaxed),
            None => self.boot.holds(id) && (e.cold.has2(G_UNVERIFIED) || e.cold.has2(G_RECHECK)),
        }
    }

    /// Start the first batch (called after the table is loaded and reconciled).
    pub(super) fn boot_begin(&mut self) {
        self.sh.diag.boot_waiting.store(self.boot.len() as u64, Ordering::Relaxed);
        if !self.boot.order.is_empty() {
            self.schedule_after(BOOT_TICK, Job::BootStep);
        }
    }

    pub(super) fn boot_step(&mut self) {
        if self.shutting_down {
            self.boot.clear();
            self.sh.diag.boot_waiting.store(0, Ordering::Relaxed);
            return;
        }
        let cap = self.sh.cfg().verify_slots.max(1);
        let mut left = BOOT_BATCH;
        while left > 0 {
            // A started torrent counts as verifying at once, so the next one sees it.
            if self.verifying_count() >= cap {
                break;
            }
            let Some(id) = self.boot.order.pop_front() else { break };
            // Released meanwhile (the user acted on it) or gone.
            if !self.boot.held.remove(&id) {
                continue;
            }
            self.reconcile(id);
            left -= 1;
        }
        self.sh.diag.boot_waiting.store(self.boot.len() as u64, Ordering::Relaxed);
        if !self.boot.order.is_empty() {
            self.schedule_after(BOOT_TICK, Job::BootStep);
        }
    }
}
