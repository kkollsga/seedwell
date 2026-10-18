//! The Sharing view's bookkeeping (SPEC 8.3): when a torrent last did anything useful, since when
//! it has been failing, which group that makes it, and which retry pace the tracker driver should
//! use for it (SPEC 7C.4). A Cold torrent carries 12 bytes for this; the rest is computed.

use std::time::{Duration, Instant};

use super::cold::*;
use super::{Job, Session};
use crate::api::*;
use crate::torrent::LiveStats;
use crate::tracker_driver::{Health, Role, TrackerCmd};
use crate::util::unix_secs;

/// "Dormant" and "dead" thresholds (SPEC 8.3).
const DAY: u64 = 86_400;
/// Popular: this many peers downloading from the torrent right now ...
const POPULAR_PEERS: u32 = 3;
/// ... or this upload rate (bytes/s) averaged over ten minutes.
const POPULAR_RATE: u32 = 250_000;
/// Time constant of the averaged upload rate.
const AVG_WINDOW: f64 = 600.0;
/// `last_activity` is persisted at most this often.
const PERSIST_EVERY: u64 = 600;

/// What the trackers of a failing torrent last said (only kept while failing).
#[derive(Debug)]
pub(crate) struct TrackerErr {
    pub reason: Box<str>,
    pub retry_at_ms: Option<u64>,
    /// Unregistered / banned / bad passkey: nothing will fix this by waiting.
    pub permanent: bool,
}

/// Popular / Active / Dormant from activity alone.
pub(crate) fn activity_group(c: &ColdTorrent, live: Option<&LiveStats>, now: u64) -> SeedGroup {
    use std::sync::atomic::Ordering::Relaxed;
    let idle = now.saturating_sub(u64::from(c.last_activity));
    let downloading_from_us = live.map_or(0, |l| l.uploading.load(Relaxed));
    // The averaged rate only counts while the torrent was busy in the last ten minutes, so a
    // stale average of a torrent that went Cold cannot keep it Popular.
    if downloading_from_us >= POPULAR_PEERS || (c.avg_up > POPULAR_RATE && idle < 600) {
        SeedGroup::Popular
    } else if idle < DAY || live.is_some_and(|l| l.interested.load(Relaxed) > 0) {
        SeedGroup::Active
    } else {
        SeedGroup::Dormant
    }
}

impl Session {
    /// The Sharing group of `c`, `None` unless it is a finished torrent that is not stopped.
    pub(crate) fn seed_group(&self, c: &ColdTorrent, live: Option<&LiveStats>, now: u64) -> Option<SeedGroup> {
        if !c.finished() || !matches!(c.want, Want::Active | Want::Error) {
            return None;
        }
        let te = self.tracker_errs.get(&c.id);
        if c.error_since != 0 && (c.want == Want::Error || te.is_some()) {
            let dead = te.is_some_and(|t| t.permanent) || now.saturating_sub(u64::from(c.error_since)) > DAY;
            return Some(if dead { SeedGroup::Dead } else { SeedGroup::Error });
        }
        Some(activity_group(c, live, now))
    }

    /// Once a tick: note uploads and interested peers, and average the upload rate.
    pub(super) fn update_activity(&mut self) {
        let now = unix_secs();
        let dt = self.activity_tick.replace(Instant::now()).map_or(60.0, |t| t.elapsed().as_secs_f64().clamp(1.0, 3600.0));
        let a = (dt / AVG_WINDOW).min(1.0);
        let mut persist = Vec::new();
        for (id, e) in self.table.iter_mut() {
            let c = &mut e.cold;
            if !c.finished() {
                continue;
            }
            let up = c.counters.up_total();
            let prev = self.flushed.get(id).map_or(up, |f| f.0);
            let delta = up.saturating_sub(prev);
            let rate = delta as f64 / dt;
            c.avg_up = (f64::from(c.avg_up) + (rate - f64::from(c.avg_up)) * a).clamp(0.0, f64::from(u32::MAX)) as u32;
            let interested = e.live.as_ref().is_some_and(|l| {
                l.stats.interested.load(std::sync::atomic::Ordering::Relaxed) > 0
                    || l.stats.uploading.load(std::sync::atomic::Ordering::Relaxed) > 0
            });
            if delta > 0 || interested {
                if now.saturating_sub(u64::from(c.last_activity)) >= PERSIST_EVERY {
                    persist.push(*id);
                }
                c.last_activity = now as u32;
            }
        }
        for id in persist {
            self.mark_record(id);
        }
    }

    /// Recompute `error_since` after a disk error appeared or went, or the trackers started or
    /// stopped failing.
    pub(crate) fn update_error(&mut self, id: TorrentId) {
        let te = self.tracker_errs.contains_key(&id);
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        let active = c.want == Want::Error || te;
        let before = (c.error_since, c.has2(G_TRACKER_ERR));
        c.set2(G_TRACKER_ERR, te);
        if active && c.error_since == 0 {
            c.error_since = (unix_secs() as u32).max(1);
        } else if !active {
            c.error_since = 0;
        }
        if before != (c.error_since, te) {
            self.mark_record(id);
        }
    }

    pub(super) fn on_tracker_health(&mut self, id: TorrentId, h: Health) {
        if !self.table.contains_key(&id) {
            return;
        }
        match h {
            Health::Ok => {
                self.tracker_errs.remove(&id);
            }
            Health::Failing { reason, retry_at_ms } => {
                self.tracker_errs.insert(id, TrackerErr { reason: reason.into(), retry_at_ms, permanent: false });
            }
            Health::Dead { reason } => {
                self.tracker_errs.insert(id, TrackerErr { reason: reason.into(), retry_at_ms: None, permanent: true });
            }
        }
        self.update_error(id);
    }

    /// Hand the torrent's role to the tracker driver if it changed (SPEC 7C.4 retry pace).
    pub(crate) fn sync_role(&mut self, id: TorrentId) {
        let now = unix_secs();
        let Some(e) = self.table.get(&id) else { return };
        let c = &e.cold;
        let role = if !c.finished() {
            Role::Downloading
        } else {
            match activity_group(c, e.live.as_ref().map(|l| &*l.stats), now) {
                SeedGroup::Dormant => Role::SeedingDormant,
                _ => Role::Seeding,
            }
        };
        let current = if c.has2(G_ROLE_DORMANT) {
            Role::SeedingDormant
        } else if c.has2(G_ROLE_SEED) {
            Role::Seeding
        } else {
            Role::Downloading
        };
        if role == current {
            return;
        }
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.set2(G_ROLE_SEED, role == Role::Seeding);
            e.cold.set2(G_ROLE_DORMANT, role == Role::SeedingDormant);
        }
        self.tracker_send(TrackerCmd::Role(id, role));
    }

    /// Every ten minutes: torrents drift into Dormant with time alone.
    pub(super) fn role_check(&mut self) {
        let ids: Vec<TorrentId> = self.table.keys().copied().collect();
        for id in ids {
            self.sync_role(id);
        }
        if !self.shutting_down {
            self.schedule_after(Duration::from_secs(600), Job::RoleCheck);
        }
    }
}
