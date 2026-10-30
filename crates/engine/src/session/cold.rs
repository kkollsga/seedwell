//! The Cold tier: everything the session keeps for a torrent that has no actor.
//!
//! Target (ENGINE.md 3): <= 256 bytes plus the name. Tracker state lives in `trs-tracker`.

use std::sync::Arc;

use trs_classify::Category;

use crate::api::{Priority, SeedGoal, TorrentId, TorrentState};
use crate::counters::Counters;
use crate::persist::{Placed, StoredState, TorrentRecord};
use crate::policy::IdTable;

pub(crate) const F_PRIVATE: u8 = 1;
pub(crate) const F_META: u8 = 2;
pub(crate) const F_ADDED_COMPLETE: u8 = 4;
pub(crate) const F_COMPLETE: u8 = 8;
pub(crate) const F_WANTED_DONE: u8 = 16;
pub(crate) const F_STAGED: u8 = 32;
pub(crate) const F_QUEUED: u8 = 64;
pub(crate) const F_TRACKER_REG: u8 = 128;

pub(crate) const G_MISSING: u16 = 1;
pub(crate) const G_RECHECK: u16 = 2;
pub(crate) const G_TRACKER_STARTED: u16 = 4;
/// The user chose the priority; rule edits leave it alone.
pub(crate) const G_PRIO_USER: u16 = 8;
/// The user chose the seeding goal kind; rule edits leave it alone.
pub(crate) const G_GOAL_USER: u16 = 16;
/// Added but not yet verified: the first actor run rechecks existing data.
pub(crate) const G_UNVERIFIED: u16 = 32;
/// The user chose the category (or a watch folder forced it).
pub(crate) const G_CAT_USER: u16 = 64;
/// Files live at recorded per-file paths (`Store::load_placement`).
pub(crate) const G_PLACED: u16 = 128;
/// The trackers are failing (the cause of `error_since`, or one of its causes).
pub(crate) const G_TRACKER_ERR: u16 = 4096;
/// The role last handed to the tracker driver was Seeding / SeedingDormant (both clear = Downloading).
pub(crate) const G_ROLE_SEED: u16 = 8192;
pub(crate) const G_ROLE_DORMANT: u16 = 16384;
/// Waiting for the user to choose what to download (SPEC 7A.6).
pub(crate) const G_AWAITING: u16 = 1024;
/// Was finished and downloads again (not persisted): its next finish is no new completed torrent.
pub(crate) const G_REOPENED: u16 = 2048;
/// The user chose the location (custom save path, "Move to…"); location changes leave it alone.
pub(crate) const G_PINNED: u16 = 32768;

/// What the user wants the torrent to be doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    Active,
    Paused,
    GoalReached,
    Error,
}

/// Compact per-torrent record. `size_of` is asserted in the tests below.
#[derive(Debug)]
pub(crate) struct ColdTorrent {
    pub id: TorrentId,
    pub name: Box<str>,
    /// Clean display title (persisted; `None` = not computed yet).
    pub title: Option<Box<str>>,
    pub counters: Arc<Counters>,
    pub total_len: u64,
    pub wanted_len: u64,
    /// Wanted bytes held when the torrent became a download again (0 = none, SPEC 8.3).
    pub session_base: u64,
    pub seed_goal: SeedGoal,
    /// Upload history of the inactive stop condition; only while the condition is on for this torrent.
    pub idle: Option<Box<crate::idle::IdleRing>>,
    pub file_prios: Option<Box<[u8]>>,
    pub error: Option<Box<str>>,
    /// Only for magnet links whose metadata has not arrived.
    pub magnet: Option<Box<str>>,
    pub piece_len: u32,
    pub save_root: u32,
    /// Interned staging root (`NONE_U32` = below the save root's `.incomplete`, or the configured folder).
    pub stage_root: u32,
    pub added_at: u32,
    pub finished_at: u32,
    pub queue_pos: u32,
    /// Interned id of the matched tracker profile (`NONE_U32` = Public / Other private).
    pub tracker_profile: u32,
    /// Interned id of the rule forced for this torrent (`NONE_U32` = by matching).
    pub rule: u32,
    /// Per-torrent upload limit from the rule, KiB/s (0 = none).
    pub up_limit_kib: u32,
    /// Unix seconds of the last upload or interested peer (Sharing groups, SPEC 8.3).
    pub last_activity: u32,
    /// Unix seconds when the current error began (0 = none).
    pub error_since: u32,
    /// Upload rate, bytes/s, averaged over about 10 minutes (only meaningful while `last_activity` is recent).
    pub avg_up: u32,
    pub seeded_secs: u32,
    /// Unix seconds when the current seeding stretch began (0 = not seeding).
    pub seed_since: u32,
    pub host: u16,
    pub want: Want,
    pub priority: u8,
    /// Index into `Category::ALL`; `NONE_CAT` = not detected.
    pub category: u8,
    pub flags: u8,
    pub flags2: u16,
    /// `H3_*` bits. `H3_NO_HOLD` and `H3_FORCE_HOLD` only matter between an add and the arrival of
    /// metadata and are not persisted; `H3_PENDING` and `H3_COUNTDOWN_STOPPED` are (as
    /// `TorrentRecord::pending_start` / `pending_stopped`); `H3_APP_PAUSED` is deliberately not.
    pub flags3: u8,
}

/// The add said `hold: Some(false)`: start at once, whatever the smart-add settings say.
pub(crate) const H3_NO_HOLD: u8 = 1;
/// The add said `hold: Some(true)`: hold for a choice in any case.
pub(crate) const H3_FORCE_HOLD: u8 = 2;
/// Held for the grace period before an auto-add starts (SPEC 7A.7). Always together with `G_AWAITING`.
pub(crate) const H3_PENDING: u8 = 4;
/// The user stopped the grace countdown (`H3_PENDING` only); it stays stopped across a restart.
pub(crate) const H3_COUNTDOWN_STOPPED: u8 = 8;
/// `Want::Paused` by the app itself (on battery), not by the user: stored as running, so the pause
/// ends with the run (SPEC §10). A user's pause or resume clears it.
pub(crate) const H3_APP_PAUSED: u8 = 16;

pub(crate) const NONE_U32: u32 = u32::MAX;
pub(crate) const NONE_CAT: u8 = u8::MAX;

impl ColdTorrent {
    pub(crate) fn has(&self, f: u8) -> bool {
        self.flags & f != 0
    }

    pub(crate) fn set(&mut self, f: u8, on: bool) {
        if on {
            self.flags |= f;
        } else {
            self.flags &= !f;
        }
    }

    pub(crate) fn has2(&self, f: u16) -> bool {
        self.flags2 & f != 0
    }

    pub(crate) fn set2(&mut self, f: u16, on: bool) {
        if on {
            self.flags2 |= f;
        } else {
            self.flags2 &= !f;
        }
    }

    pub(crate) fn priority(&self) -> Priority {
        Priority::from_u8(self.priority)
    }

    pub(crate) fn private(&self) -> bool {
        self.has(F_PRIVATE)
    }

    pub(crate) fn has_metadata(&self) -> bool {
        self.has(F_META)
    }

    pub(crate) fn complete(&self) -> bool {
        self.has(F_COMPLETE)
    }

    /// Everything the user wants is here (all pieces, or every wanted file when some are skipped).
    pub(crate) fn finished(&self) -> bool {
        self.complete() || self.has(F_WANTED_DONE)
    }

    /// Needs a download slot: has metadata, not finished with the wanted files.
    pub(crate) fn wants_slot(&self) -> bool {
        self.want == Want::Active
            && self.has_metadata()
            && !self.complete()
            && !self.has(F_WANTED_DONE)
            && !self.has2(G_AWAITING)
    }

    /// Held until the user chooses what to download (SPEC 7A.6).
    pub(crate) fn awaiting(&self) -> bool {
        self.has2(G_AWAITING)
    }

    /// Held for the grace period (a kind of awaiting that starts by itself).
    pub(crate) fn pending(&self) -> bool {
        self.has2(G_AWAITING) && self.flags3 & H3_PENDING != 0
    }

    pub(crate) fn opt(v: u32) -> Option<u32> {
        (v != NONE_U32).then_some(v)
    }

    /// Detected or chosen category.
    pub(crate) fn category(&self) -> Option<Category> {
        Category::ALL.get(usize::from(self.category)).copied()
    }

    pub(crate) fn set_category(&mut self, c: Option<Category>) {
        self.category = c.map_or(NONE_CAT, |c| c.index() as u8);
    }

    /// Upload limit in bytes/s.
    pub(crate) fn up_limit(&self) -> Option<u64> {
        (self.up_limit_kib != 0).then(|| u64::from(self.up_limit_kib) * 1024)
    }

    pub(crate) fn set_up_limit(&mut self, v: Option<u64>) {
        self.up_limit_kib = v.map_or(0, |b| u32::try_from(b.div_ceil(1024)).unwrap_or(u32::MAX).max(1));
    }

    /// Whether the files have per-file paths of their own (earlier versions' placement).
    pub(crate) fn placed(&self) -> Placed {
        if self.has2(G_PLACED) {
            Placed::Renamed
        } else {
            Placed::No
        }
    }

    pub(crate) fn set_placed(&mut self, p: Placed) {
        self.set2(G_PLACED, p == Placed::Renamed);
    }

    /// Verified bytes of the wanted files.
    pub(crate) fn wanted_done(&self) -> u64 {
        if self.complete() || self.has(F_WANTED_DONE) {
            self.wanted_len
        } else {
            self.counters.wanted_done().min(self.wanted_len)
        }
    }

    /// (done, size) of the current download session: what a download's progress counts.
    pub(crate) fn session(&self) -> (u64, u64) {
        let done = self.wanted_done();
        let base = self.session_base.min(done);
        (done - base, self.wanted_len.saturating_sub(base))
    }

    /// Verified bytes.
    pub(crate) fn done_bytes(&self) -> u64 {
        self.total_len.saturating_sub(self.counters.left())
    }

    /// Seeding seconds including the running stretch.
    pub(crate) fn seeded_secs(&self, now: u64) -> u64 {
        let running = if self.seed_since != 0 { now.saturating_sub(u64::from(self.seed_since)) } else { 0 };
        u64::from(self.seeded_secs) + running
    }

    /// Lifetime upload / what we downloaded, as trackers count it. The base is the larger of the bytes
    /// downloaded and the verified bytes on disk, so a torrent added complete (cross-seeding,
    /// downloaded = 0) is measured against what it holds and a partial selection against its part.
    pub(crate) fn ratio(&self) -> f64 {
        ratio_of(self.counters.up_total(), self.counters.down_total(), self.done_bytes())
    }

    /// The state shown to the user, given what the running actor (if any) reports.
    pub(crate) fn state(&self, live: Option<&crate::torrent::LiveStats>) -> TorrentState {
        match self.want {
            _ if self.pending() && self.want != Want::Error => TorrentState::Pending,
            _ if self.has2(G_AWAITING) && self.want != Want::Error => TorrentState::AwaitingChoice,
            Want::Error => {
                if self.has2(G_MISSING) { TorrentState::MissingData } else { TorrentState::Error }
            }
            Want::Paused | Want::GoalReached => TorrentState::Paused,
            Want::Active => {
                if !self.has_metadata() {
                    return TorrentState::FetchingMetadata;
                }
                if live.is_some_and(|l| l.verifying.load(std::sync::atomic::Ordering::Relaxed)) {
                    return TorrentState::Checking;
                }
                if self.has(F_QUEUED) {
                    return TorrentState::Queued;
                }
                if !self.complete() && !self.has(F_WANTED_DONE) {
                    return TorrentState::Downloading;
                }
                let uploading = live.is_some_and(|l| l.uploading.load(std::sync::atomic::Ordering::Relaxed) > 0);
                if uploading { TorrentState::Seeding } else { TorrentState::LowPower }
            }
        }
    }

    /// The inactive condition with its live figure (`None` = off or no history yet).
    pub(crate) fn idle_status(&self) -> Option<crate::api::IdleStatus> {
        let (below, hours) = self.seed_goal.idle()?;
        let r = self.idle.as_deref()?;
        Some(crate::api::IdleStatus { uploaded: r.uploaded(), below, hours, stopped: r.stopped && self.want == Want::GoalReached })
    }

    /// Build the persisted record. `ids` resolves the interned profile and rule ids.
    pub(crate) fn record(
        &self,
        save_path: std::path::PathBuf,
        stage_path: Option<std::path::PathBuf>,
        announce: Option<Vec<Vec<String>>>,
        ids: &IdTable,
    ) -> TorrentRecord {
        TorrentRecord {
            id: self.id,
            name: self.name.to_string(),
            has_metadata: self.has_metadata(),
            magnet: self.magnet.as_deref().map(str::to_owned),
            announce,
            total_length: self.total_len,
            wanted_length: self.wanted_len,
            session_base: self.session_base,
            piece_length: self.piece_len,
            private: self.private(),
            save_path,
            state: match self.want {
                Want::Active => StoredState::Active,
                // What the app paused by itself resumes at the next start; the user's pause stays.
                Want::Paused if self.flags3 & H3_APP_PAUSED != 0 => StoredState::Active,
                Want::Paused => StoredState::Paused,
                Want::GoalReached => StoredState::GoalReached,
                Want::Error => StoredState::Error(self.error.as_deref().unwrap_or("error").to_owned()),
            },
            file_priorities: self.file_prios.as_deref().map(<[u8]>::to_vec).unwrap_or_default(),
            priority: self.priority,
            priority_user: self.has2(G_PRIO_USER),
            queue_pos: self.queue_pos,
            category: self.category(),
            category_user: self.has2(G_CAT_USER),
            tracker_profile: Self::opt(self.tracker_profile).and_then(|i| ids.get(i)).map(str::to_owned),
            rule: Self::opt(self.rule).and_then(|i| ids.get(i)).map(str::to_owned),
            added_at: u64::from(self.added_at),
            finished_at: (self.finished_at != 0).then_some(u64::from(self.finished_at)),
            seed_goal: self.seed_goal,
            goal_user: self.has2(G_GOAL_USER),
            added_complete: self.has(F_ADDED_COMPLETE),
            staged: self.has(F_STAGED),
            complete: self.complete(),
            placed: self.placed(),
            last_activity: u64::from(self.last_activity),
            error_since: u64::from(self.error_since),
            error_tracker: self.has2(G_TRACKER_ERR),
            stage_path,
            pinned: self.has2(G_PINNED),
            awaiting_choice: self.has2(G_AWAITING),
            pending_start: self.pending(),
            pending_stopped: self.pending() && self.flags3 & H3_COUNTDOWN_STOPPED != 0,
            title: self.title.as_deref().map(str::to_owned),
            idle: self.idle.as_deref().cloned(),
        }
    }
}

/// Upload ratio: `up` over the larger of `down` and `have` (qBittorrent's rule); 0 when nothing is held.
pub(crate) fn ratio_of(up: u64, down: u64, have: u64) -> f64 {
    let base = down.max(have);
    if base == 0 { 0.0 } else { up as f64 / base as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_record_fits_the_budget() {
        let n = std::mem::size_of::<ColdTorrent>();
        // ENGINE.md 3: <= 256 B plus the name (the name's heap bytes are separate).
        assert!(n <= 256, "ColdTorrent is {n} bytes");
        eprintln!("size_of ColdTorrent = {n}");
    }

    #[test]
    fn flags_toggle_independently() {
        let mut c = ColdTorrent {
            id: [0; 20],
            name: "x".into(),
            title: None,
            counters: Arc::new(Counters::default()),
            total_len: 10,
            wanted_len: 10,
            session_base: 0,
            seed_goal: SeedGoal::FOREVER,
            idle: None,
            file_prios: None,
            error: None,
            magnet: None,
            piece_len: 1,
            save_root: 0,
            stage_root: NONE_U32,
            added_at: 0,
            finished_at: 0,
            queue_pos: 0,
            tracker_profile: NONE_U32,
            rule: NONE_U32,
            up_limit_kib: 0,
            last_activity: 0,
            error_since: 0,
            avg_up: 0,
            seeded_secs: 0,
            seed_since: 0,
            host: 0,
            want: Want::Active,
            priority: 1,
            category: NONE_CAT,
            flags: 0,
            flags2: 0,
            flags3: 0,
        };
        c.set(F_COMPLETE, true);
        c.set(F_META, true);
        assert!(c.complete() && c.has_metadata() && !c.private());
        c.set(F_COMPLETE, false);
        assert!(!c.complete() && c.has_metadata());
        assert!(c.wants_slot());
        c.want = Want::Paused;
        assert!(!c.wants_slot());
        assert_eq!(c.state(None), TorrentState::Paused);
    }

    #[test]
    fn ratio_counts_what_was_downloaded_not_the_whole_torrent() {
        const GB: u64 = 1_000_000_000;
        // 3 of 24 files: 1.8 GB held of 14 GB, 3.7 GB uploaded.
        assert!((ratio_of(37 * GB / 10, 18 * GB / 10, 18 * GB / 10) - 2.0555).abs() < 0.001);
        // Added complete (cross-seeding): measured against what it holds.
        assert!((ratio_of(7 * GB, 0, 14 * GB) - 0.5).abs() < 1e-9);
        // Downloaded more than it kept (re-downloads, rechecks): the downloaded bytes count.
        assert!((ratio_of(GB, 2 * GB, GB) - 0.5).abs() < 1e-9);
        assert_eq!(ratio_of(5, 0, 0), 0.0);
    }
}
