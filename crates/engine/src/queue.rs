//! Queue manager: which downloads run (SPEC 7.3, Transfers settings "max active downloads").
//!
//! Pure planning. `High` priority torrents are never held back by the limit. Torrents that are not
//! waiting for data (seeding, paused, fetching magnet metadata) are outside the queue.

use crate::api::Priority;

/// One torrent as the queue sees it.
#[derive(Clone, Copy, Debug)]
pub struct QueueItem {
    /// Position in the user's order (0 = front).
    pub pos: u32,
    /// Priority.
    pub priority: Priority,
    /// Wants a download slot (incomplete, not paused, has metadata).
    pub wants_slot: bool,
    /// Runs but moves (almost) nothing: a stalled download, or one whose disk refuses writes.
    /// It does not use up one of the `max_active` slots (qBittorrent's "slow torrents do not
    /// count"), so the next queued download starts; it keeps running while the total stays within
    /// `2 x max_active`, which bounds the memory of the actors.
    pub inactive: bool,
}

/// Running downloads never exceed this many times `max_active` (inactive ones count, High overflow aside).
pub const TOTAL_FACTOR: usize = 2;

/// For every item: may it run now? Items that do not want a slot are reported as `true`
/// (they are not held back), the caller ignores them.
///
/// Active items fill `max_active` slots in queue order; `High` priority is never held back.
/// Inactive items then take what is left of `TOTAL_FACTOR x max_active`, earliest first.
/// `max_active == 0` means unlimited.
pub fn plan(items: &[QueueItem], max_active: usize) -> Vec<bool> {
    let mut order: Vec<usize> = (0..items.len()).filter(|&i| items[i].wants_slot).collect();
    order.sort_by_key(|&i| (items[i].priority, items[i].pos));
    let mut run = vec![true; items.len()];
    if max_active == 0 {
        return run;
    }
    let mut counted = 0usize;
    let mut total = 0usize;
    for &i in &order {
        let it = &items[i];
        if it.inactive {
            run[i] = false; // decided in the second pass
            continue;
        }
        let ok = counted < max_active || it.priority == Priority::High;
        run[i] = ok;
        if ok {
            counted += 1;
            total += 1;
        }
    }
    let cap = max_active.saturating_mul(TOTAL_FACTOR);
    for &i in &order {
        if items[i].inactive && (total < cap || items[i].priority == Priority::High) {
            run[i] = true;
            total += 1;
        }
    }
    run
}

/// Is a download moving? Tracks one running download (pure: the caller passes the time).
///
/// Below `threshold` bytes/s for `hold` it becomes inactive; above it for `hold` it becomes active
/// again. A streak in the other direction starts the count over, so a download that flickers
/// around the threshold never flips.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stall {
    inactive: bool,
    /// Start (ms) of the current streak that would flip the state.
    since: Option<u64>,
}

impl Stall {
    /// Is it currently not counted?
    pub fn inactive(&self) -> bool {
        self.inactive
    }

    /// Forget the streak (the download stopped, or is busy verifying). Keeps the verdict.
    pub fn pause(&mut self) {
        self.since = None;
    }

    /// One observation. Returns `true` when the verdict changed.
    pub fn observe(&mut self, now_ms: u64, rate: u64, threshold: u64, hold_ms: u64) -> bool {
        let slow = rate < threshold;
        // The streak that flips the state: slow while active, fast while inactive.
        if slow == self.inactive {
            self.since = None;
            return false;
        }
        let start = *self.since.get_or_insert(now_ms);
        if now_ms.saturating_sub(start) >= hold_ms {
            self.inactive = !self.inactive;
            self.since = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(pos: u32, priority: Priority, wants: bool) -> QueueItem {
        QueueItem { pos, priority, wants_slot: wants, inactive: false }
    }

    #[test]
    fn first_n_in_queue_order_run() {
        let items = [it(2, Priority::Normal, true), it(0, Priority::Normal, true), it(1, Priority::Normal, true)];
        assert_eq!(plan(&items, 2), vec![false, true, true]);
    }

    #[test]
    fn high_priority_jumps_and_is_never_held() {
        let items = [
            it(0, Priority::Normal, true),
            it(1, Priority::Normal, true),
            it(2, Priority::High, true),
            it(3, Priority::High, true),
        ];
        // both High run; they use up the budget of 2, so Normal ones wait.
        assert_eq!(plan(&items, 2), vec![false, false, true, true]);
        // High beyond the limit still runs.
        assert_eq!(plan(&items, 1), vec![false, false, true, true]);
    }

    #[test]
    fn non_downloading_items_do_not_use_slots() {
        let items = [it(0, Priority::Normal, false), it(1, Priority::Normal, true), it(2, Priority::Normal, true)];
        assert_eq!(plan(&items, 1), vec![true, true, false]);
    }

    #[test]
    fn zero_means_unlimited() {
        let items = [it(0, Priority::Low, true), it(1, Priority::Low, true)];
        assert_eq!(plan(&items, 0), vec![true, true]);
    }

    fn idle(pos: u32) -> QueueItem {
        QueueItem { pos, priority: Priority::Normal, wants_slot: true, inactive: true }
    }

    #[test]
    fn an_inactive_download_does_not_hold_a_slot() {
        // max 2: #0 and #1 run, #1 stalled -> #2 starts too; #3 waits.
        let items = [it(0, Priority::Normal, true), idle(1), it(2, Priority::Normal, true), it(3, Priority::Normal, true)];
        assert_eq!(plan(&items, 2), vec![true, true, true, false]);
    }

    #[test]
    fn running_downloads_are_capped_at_twice_the_limit() {
        // max 2, cap 4: all four stalled would run; a fifth waits.
        let items = [idle(0), idle(1), idle(2), idle(3), idle(4)];
        assert_eq!(plan(&items, 2), vec![true, true, true, true, false]);
        // Active ones always get their slots before inactive ones use the cap.
        let items = [idle(0), idle(1), idle(2), idle(3), it(4, Priority::Normal, true), it(5, Priority::Normal, true)];
        let run = plan(&items, 2);
        assert!(run[4] && run[5], "the active downloads run");
        assert_eq!(run.iter().filter(|r| **r).count(), 4, "at most 2 x max in total: {run:?}");
        assert!(run[0] && run[1] && !run[2] && !run[3], "inactive: the earliest keep running");
    }

    #[test]
    fn unlimited_ignores_inactivity() {
        assert_eq!(plan(&[idle(0), it(1, Priority::Normal, true)], 0), vec![true, true]);
    }

    #[test]
    fn stall_needs_a_full_minute_each_way() {
        let (thr, hold) = (2048, 60_000);
        let mut s = Stall::default();
        assert!(!s.observe(0, 0, thr, hold));
        assert!(!s.observe(59_000, 100, thr, hold), "59 s is not a minute");
        assert!(s.observe(60_000, 100, thr, hold));
        assert!(s.inactive());
        // A burst of speed does not bring it back until it lasts a minute.
        assert!(!s.observe(61_000, 50_000, thr, hold));
        assert!(!s.observe(100_000, 50_000, thr, hold));
        assert!(!s.observe(110_000, 10, thr, hold), "a dip restarts the count");
        assert!(!s.observe(111_000, 50_000, thr, hold));
        assert!(!s.observe(170_000, 50_000, thr, hold));
        assert!(s.observe(171_000, 50_000, thr, hold));
        assert!(!s.inactive());
    }

    #[test]
    fn a_flickering_rate_never_flips() {
        let mut s = Stall::default();
        for k in 0..100u64 {
            let rate = if k % 2 == 0 { 0 } else { 10_000 };
            assert!(!s.observe(k * 10_000, rate, 2048, 60_000));
        }
        assert!(!s.inactive());
    }
}
