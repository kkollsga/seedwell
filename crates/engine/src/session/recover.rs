//! Faults that clear by themselves, and downloads that stopped moving.
//!
//! * Missing data (unplugged drive, deleted folder): retried at 1, 5, 15, then every 60 minutes.
//!   The app also tells the engine when a volume is mounted or unmounted ([`Session::volumes_changed`]):
//!   each distinct save root is looked at once and the torrents on it are flipped in bulk, without
//!   starting an actor to find out.
//! * Writes that fail because the disk is full, read-only or not permitted: the actor keeps
//!   serving what it has verified and asks for nothing more ("upload mode", libtorrent-like); the
//!   session tries again at 10, 20, 40, then every 60 minutes, first checking the free space.
//! * A download that moves under 2 KiB/s for a minute stops counting toward the queue limit.
//!
//! All of it is per-torrent bookkeeping in small maps that only hold torrents that are in trouble;
//! a Cold torrent without a fault costs nothing here.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::cold::*;
use super::{After, Job, Session, SessionMsg};
use crate::api::{EngineEvent, TorrentId, TorrentState};
use crate::peer::FaultKind;
use crate::util::unix_secs;

/// Retry delays in minutes for missing data.
pub(crate) const MISSING_STEPS: [u64; 4] = [1, 5, 15, 60];
/// Retry delays in minutes for a disk that refuses writes (libtorrent retries every 10).
pub(crate) const DISK_STEPS: [u64; 4] = [10, 20, 40, 60];
/// After a retry, how long a fault must stay away before its backoff is forgotten.
const PROBE: Duration = Duration::from_secs(300);
/// Longest time since the last fault for which the backoff keeps growing (a day-old trouble starts over).
const FORGET_SECS: u64 = 2 * 3600;
/// What `volumes_changed` says about a drive that is not there.
pub(crate) const DRIVE_GONE: &str = "Drive not connected";

/// The delay of `step` in `steps`, for a "minute" of `minute`.
pub(crate) fn step_delay(steps: &[u64; 4], step: u8, minute: Duration) -> Duration {
    minute * steps[usize::from(step).min(steps.len() - 1)] as u32
}

/// The next step after `step` (stays on the last).
pub(crate) fn next_step(step: u8) -> u8 {
    (step + 1).min(3)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryKind {
    Missing,
    Disk,
}

/// A torrent that is waiting to be tried again.
#[derive(Clone, Debug)]
pub(crate) struct Retry {
    pub kind: RetryKind,
    /// Index into the step table used for the next (or current) wait.
    pub step: u8,
    /// Unix seconds of the next attempt (for the row).
    pub at: u64,
    /// Unix seconds of the last fault (the backoff starts over after a long quiet time).
    pub last_fault: u64,
    /// Short words for the row ("Disk full").
    pub label: &'static str,
    /// The wait is for free space (checked before the attempt).
    pub space: bool,
    /// An attempt is running; if no fault returns within `PROBE` the trouble is over.
    pub probing: bool,
}

/// Plain words for a blocked write.
pub(crate) fn blocked_label(message: &str) -> (&'static str, bool) {
    let code = message
        .rsplit_once("(os error ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(n, _)| n.parse::<i32>().ok());
    match code {
        Some(c) if c == libc::ENOSPC || c == libc::EDQUOT => ("Disk full", true),
        Some(c) if c == libc::EROFS => ("Disk is read-only", false),
        Some(_) => ("Not allowed to write", false),
        None if message.starts_with("Not enough space") => ("Disk full", true),
        None => ("Disk not writable", false),
    }
}

/// "9 min", "1 min", "2 h" for the row.
pub(crate) fn wait_words(secs: u64) -> String {
    let mins = secs.div_ceil(60).max(1);
    if mins >= 120 { format!("{} h", mins.div_ceil(60)) } else { format!("{mins} min") }
}

impl Session {
    // ---- faults -------------------------------------------------------------------------

    pub(super) fn on_fault(&mut self, id: TorrentId, kind: FaultKind, message: String) {
        let Some(e) = self.table.get_mut(&id) else { return };
        e.cold.error = Some(message.clone().into());
        e.cold.set2(G_MISSING, kind == FaultKind::Missing);
        self.set_want(id, Want::Error);
        self.sh.emit(EngineEvent::Error { id, message: message.clone() });
        match kind {
            FaultKind::Missing => self.arm_retry(id, RetryKind::Missing, "", false),
            FaultKind::Blocked => {
                let (label, space) = blocked_label(&message);
                self.arm_retry(id, RetryKind::Disk, label, space);
            }
            FaultKind::Other => {
                self.retries.remove(&id);
            }
        }
    }

    /// The running actor met a write it cannot do and went into upload mode.
    pub(super) fn on_disk_blocked(&mut self, id: TorrentId, message: String) {
        if !self.table.contains_key(&id) {
            return;
        }
        let (label, space) = blocked_label(&message);
        self.arm_retry(id, RetryKind::Disk, label, space);
        self.sh.emit(EngineEvent::Error { id, message });
        // A torrent whose disk refuses writes does not hold a queue slot.
        self.replan();
        self.bump_state(id);
    }

    /// Set (or advance) the backoff of `id` and schedule the attempt.
    fn arm_retry(&mut self, id: TorrentId, kind: RetryKind, label: &'static str, space: bool) {
        let now = unix_secs();
        let minute = self.sh.cfg().retry_minute;
        let steps = if kind == RetryKind::Missing { &MISSING_STEPS } else { &DISK_STEPS };
        let step = match self.retries.get(&id) {
            Some(r) if r.kind == kind && now.saturating_sub(r.last_fault) < FORGET_SECS => {
                if r.probing { next_step(r.step) } else { r.step }
            }
            _ => 0,
        };
        let delay = step_delay(steps, step, minute);
        self.retries.insert(
            id,
            Retry { kind, step, at: now + delay.as_secs(), last_fault: now, label, space, probing: false },
        );
        self.jobs_retain_retry(id);
        self.schedule_after(delay, Job::Retry(id));
    }

    fn jobs_retain_retry(&mut self, id: TorrentId) {
        self.jobs_retain(|j| *j != Job::Retry(id));
    }

    pub(super) fn retry_fault(&mut self, id: TorrentId) {
        let Some(r) = self.retries.get(&id).cloned() else { return };
        let Some(e) = self.table.get(&id) else {
            self.retries.remove(&id);
            return;
        };
        let (want, complete, save_root, is_missing) = (e.cold.want, e.cold.finished(), e.cold.save_root, e.cold.has2(G_MISSING));
        // The probe window of an attempt is over without a new fault: the trouble has passed.
        if r.probing {
            self.retries.remove(&id);
            self.bump_state(id);
            return;
        }
        let minute = self.sh.cfg().retry_minute;
        match (r.kind, want) {
            (RetryKind::Missing, Want::Error) if is_missing => {
                // A finished torrent has no actor to find out: look at its folder before saying it is back.
                if complete && !self.root_present(save_root) {
                    self.bump_retry(id, minute);
                    return;
                }
                self.recover_one(id, true);
                self.replan();
            }
            (RetryKind::Disk, Want::Error) => {
                // The actor stopped on a start-up fault; the space check of the new start decides.
                if let Some(e) = self.table.get_mut(&id) {
                    e.cold.error = None;
                }
                self.begin_probe(id);
                self.set_want(id, Want::Active);
            }
            (RetryKind::Disk, Want::Active) => {
                // A running actor in upload mode: look at the free space before restarting it.
                if r.space && !self.space_back(id) {
                    self.bump_retry(id, minute);
                    return;
                }
                self.begin_probe(id);
                self.stop_live(id, After::Respawn { force_recheck: false });
            }
            _ => {
                self.retries.remove(&id);
            }
        }
    }

    /// Same wait one step longer, nothing else.
    fn bump_retry(&mut self, id: TorrentId, minute: Duration) {
        let Some(r) = self.retries.get_mut(&id) else { return };
        r.step = next_step(r.step);
        let steps = if r.kind == RetryKind::Missing { &MISSING_STEPS } else { &DISK_STEPS };
        let delay = step_delay(steps, r.step, minute);
        r.at = unix_secs() + delay.as_secs();
        self.schedule_after(delay, Job::Retry(id));
        self.bump_state(id);
    }

    /// An attempt starts: if no fault returns within `PROBE` the backoff is forgotten.
    fn begin_probe(&mut self, id: TorrentId) {
        let window = PROBE.min(self.sh.cfg().retry_minute * 5);
        if let Some(r) = self.retries.get_mut(&id) {
            r.probing = true;
        }
        self.schedule_after(window, Job::Retry(id));
    }

    /// Enough room to write again? (Unknown free space counts as "yes": the attempt finds out.)
    fn space_back(&self, id: TorrentId) -> bool {
        let Some(e) = self.table.get(&id) else { return false };
        let need = (u64::from(e.cold.piece_len) * 4).max(32 << 20);
        let dest = self.save_path(&e.cold);
        let free = self.sh.cfg().smart_add.free_space_override.or_else(|| trs_storage::available_space(&dest).ok());
        free.is_none_or(|f| f >= need)
    }

    /// Put a torrent that was waiting for its files back to work (no replan; the caller does one).
    /// `hold`: unfinished ones come up through the start queue.
    fn recover_one(&mut self, id: TorrentId, hold: bool) {
        let Some(e) = self.table.get_mut(&id) else { return };
        e.cold.error = None;
        e.cold.set2(G_MISSING, false);
        self.begin_probe(id);
        self.set_want_inner(id, Want::Active, false, hold);
    }

    pub(super) fn bump_state(&mut self, id: TorrentId) {
        if let Some(e) = self.table.get(&id) {
            let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
    }

    // ---- volumes ------------------------------------------------------------------------

    /// Is the folder `idx` there? Cached for a few seconds so that hundreds of torrents on one
    /// drive cost one `stat`.
    pub(super) fn root_present(&mut self, idx: u32) -> bool {
        let now = Instant::now();
        if let Some((ok, at)) = self.root_seen.get(&idx)
            && now.saturating_duration_since(*at) < Duration::from_secs(5) {
                return *ok;
            }
        let ok = self.roots.get(idx as usize).is_none_or(|p| path_present(p));
        self.root_seen.insert(idx, (ok, now));
        ok
    }

    /// A volume was mounted or unmounted (the app's observers; the daemon has none and relies on
    /// the retry schedule). Every distinct folder torrents save to is looked at once, off the
    /// actor; the result flips torrents in bulk.
    pub(super) fn volumes_changed(&mut self) {
        if self.shutting_down {
            return;
        }
        let mut seen = std::collections::HashSet::new();
        for e in self.table.values() {
            seen.insert(e.cold.save_root);
            if e.cold.has(F_STAGED) && e.cold.stage_root != NONE_U32 {
                seen.insert(e.cold.stage_root);
            }
        }
        let list: Vec<(u32, std::path::PathBuf)> =
            seen.into_iter().filter_map(|i| self.roots.get(i as usize).map(|p| (i, p.clone()))).collect();
        let tx = self.sh.session.clone();
        tokio::task::spawn_blocking(move || {
            let res: Vec<(u32, bool)> = list.into_iter().map(|(i, p)| (i, path_present(&p))).collect();
            let _ = tx.blocking_send(SessionMsg::Volumes(res));
        });
    }

    pub(super) fn on_volumes(&mut self, res: Vec<(u32, bool)>) {
        let now = Instant::now();
        for (i, ok) in &res {
            self.root_seen.insert(*i, (*ok, now));
        }
        let gone = |i: u32| res.iter().any(|(r, ok)| *r == i && !ok);
        let mut lost = Vec::new();
        let mut back = Vec::new();
        for (id, e) in &self.table {
            let c = &e.cold;
            let affected = gone(c.save_root) || (c.has(F_STAGED) && c.stage_root != NONE_U32 && gone(c.stage_root));
            if affected && c.want == Want::Active && !self.mover_blocks(id) {
                lost.push(*id);
            } else if !affected && c.want == Want::Error && c.has2(G_MISSING) && res.iter().any(|(r, _)| *r == c.save_root) {
                back.push(*id);
            }
        }
        // One pass each, one replan at the end: n torrents on an unplugged drive are O(n).
        lost.sort_unstable();
        for id in &lost {
            if let Some(e) = self.table.get_mut(id) {
                e.cold.error = Some(DRIVE_GONE.into());
                e.cold.set2(G_MISSING, true);
            }
            self.set_want_inner(*id, Want::Error, false, false);
            self.arm_retry(*id, RetryKind::Missing, "", false);
        }
        back.sort_unstable();
        // Back again: the oldest in the queue first, unfinished ones through the start queue.
        for id in &back {
            self.retries.remove(id);
            self.recover_one(*id, true);
        }
        if !lost.is_empty() || !back.is_empty() {
            self.replan();
            self.sh.diag.boot_waiting.store(self.boot.len() as u64, Ordering::Relaxed);
        }
    }

    // ---- stalled downloads --------------------------------------------------------------

    /// Is `id` outside the queue's count right now?
    pub(super) fn queue_inactive(&self, id: &TorrentId) -> bool {
        self.stall.get(id).is_some_and(|s| s.inactive())
            || self.retries.get(id).is_some_and(|r| r.kind == RetryKind::Disk && !r.probing)
    }

    /// Look at the rate of every running download once; flip the ones that stalled or recovered.
    pub(super) fn stall_check(&mut self) {
        if self.shutting_down {
            return;
        }
        let cfg = self.sh.cfg();
        let now_ms = self.stall_clock.elapsed().as_millis() as u64;
        let hold = cfg.stall_after.as_millis() as u64;
        let mut changed = false;
        let mut running = Vec::new();
        for (id, e) in &self.table {
            let c = &e.cold;
            let Some(l) = e.live.as_ref().filter(|l| !l.stopping) else { continue };
            if !c.wants_slot() || c.has(F_QUEUED) || !l.stats.ready.load(Ordering::Relaxed) {
                continue;
            }
            running.push((*id, l.stats.down_rate.load(Ordering::Relaxed), l.stats.verifying.load(Ordering::Relaxed)));
        }
        let live: std::collections::HashSet<TorrentId> = running.iter().map(|r| r.0).collect();
        self.stall.retain(|id, _| live.contains(id));
        for (id, rate, verifying) in running {
            let s = self.stall.entry(id).or_default();
            if verifying {
                s.pause();
            } else if s.observe(now_ms, rate, cfg.stall_rate, hold) {
                changed = true;
                self.bump_state(id);
            }
        }
        if changed {
            self.replan();
        }
        if !self.stall.is_empty() {
            self.schedule_after(stall_interval(cfg.stall_after), Job::StallCheck);
        }
    }

    /// Make sure the stall check runs while downloads run (called by `replan`).
    pub(super) fn arm_stall_check(&mut self) {
        let cfg = self.sh.cfg();
        if cfg.max_active_downloads == 0 {
            return;
        }
        let any = self.table.values().any(|e| e.live.is_some() && e.cold.wants_slot() && !e.cold.has(F_QUEUED));
        if any {
            self.schedule_after(stall_interval(cfg.stall_after), Job::StallCheck);
        }
    }

    // ---- words for the row --------------------------------------------------------------

    /// What holds a torrent that is not paused (engine text; `None` = nothing special).
    pub(super) fn hold_note(&self, id: &TorrentId, state: TorrentState, waiting_verify: bool) -> Option<String> {
        let e = self.table.get(id)?;
        if let Some(r) = self.retries.get(id)
            && r.kind == RetryKind::Disk && e.cold.want == Want::Active && !r.probing {
                let left = r.at.saturating_sub(unix_secs());
                return Some(format!("{} — retrying in {}", r.label, wait_words(left)));
            }
        (state == TorrentState::Checking && waiting_verify).then(|| "Waiting to verify".to_owned())
    }

    /// Seconds to the next automatic retry of a torrent in Error / Missing data (for the row).
    pub(super) fn retry_at_ms(&self, id: &TorrentId) -> Option<u64> {
        self.retries.get(id).filter(|r| !r.probing).map(|r| r.at * 1000)
    }
}

fn stall_interval(after: Duration) -> Duration {
    (after / 6).clamp(Duration::from_millis(50), Duration::from_secs(10))
}

/// The folder is there. Only "not found" counts as gone: a permission error or a busy volume is
/// not an unplugged drive.
fn path_present(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(_) => true,
        Err(e) => !(e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_data_backs_off_1_5_15_60_minutes() {
        let m = Duration::from_secs(60);
        let secs: Vec<u64> = (0..6u8).map(|s| step_delay(&MISSING_STEPS, s.min(3), m).as_secs()).collect();
        assert_eq!(secs, [60, 300, 900, 3600, 3600, 3600]);
        assert_eq!(next_step(3), 3);
        assert_eq!(next_step(0), 1);
    }

    #[test]
    fn a_blocked_disk_backs_off_10_20_40_60_minutes() {
        let m = Duration::from_secs(60);
        let mins: Vec<u64> = (0..4u8).map(|s| step_delay(&DISK_STEPS, s, m).as_secs() / 60).collect();
        assert_eq!(mins, [10, 20, 40, 60]);
    }

    #[test]
    fn words_for_blocked_writes() {
        assert_eq!(blocked_label("i/o error: No space left on device (os error 28)"), ("Disk full", true));
        assert_eq!(blocked_label("i/o error: Read-only file system (os error 30)"), ("Disk is read-only", false));
        assert_eq!(blocked_label("i/o error: Permission denied (os error 13)"), ("Not allowed to write", false));
        assert_eq!(blocked_label("Not enough space: 3.0 GB still to download, 1.0 GB free"), ("Disk full", true));
        assert_eq!(wait_words(540), "9 min");
        assert_eq!(wait_words(1), "1 min");
        assert_eq!(wait_words(3600), "60 min");
        assert_eq!(wait_words(7300), "3 h");
    }
}
