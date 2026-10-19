//! Byte counters with the semantics trackers expect (SPEC 7B.2, FINDINGS.md "Counters").
//!
//! * **Lifetime** counters never reset; they feed ratio and statistics and are persisted.
//! * **Session** counters are the lifetime counters minus a base captured at each tracker
//!   `started` (pause/resume included). That is what `uploaded=`/`downloaded=`/`corrupt=` carry.
//! * `left` is exact and independent of `downloaded` (it counts verified bytes only).
//!
//! What each counter counts (Transmission's rule, SPEC 7B.2):
//!
//! * `downloaded` ([`Counters::add_down`]): the size of a piece that came from peers and passed
//!   its hash check, added when it passes. Hash-failed data, duplicate blocks and pieces still in
//!   progress are never in it. It is both what announces carry and the lifetime total behind the
//!   ratio and the statistics. Rates (the speed shown) are a separate, block-based measure kept by
//!   the peer tasks and never read from here.
//! * `corrupt` ([`Counters::add_corrupt`]): the size of a piece whose hash check failed, per
//!   session (not persisted). Sent on HTTP announces as `corrupt=` when above zero.
//! * `uploaded` ([`Counters::add_up`]): piece payload bytes the connection actually took from our
//!   send queue (written to the transport); protocol overhead never counts.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared by a torrent actor and its peer tasks (atomics) and read by the session.
// size: 56 bytes
#[derive(Debug, Default)]
pub struct Counters {
    up_total: AtomicU64,
    down_total: AtomicU64,
    up_base: AtomicU64,
    down_base: AtomicU64,
    /// Bytes of pieces that failed their hash check (since the process started).
    corrupt: AtomicU64,
    corrupt_base: AtomicU64,
    /// Bytes of the torrent not yet verified.
    left: AtomicU64,
    /// Verified bytes of the files the user wants (what the progress of a download measures).
    wanted_done: AtomicU64,
}

/// A plain copy of the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    /// Lifetime uploaded payload bytes.
    pub up_total: u64,
    /// Lifetime downloaded payload bytes.
    pub down_total: u64,
    /// Lifetime uploaded at the last `started`.
    pub up_base: u64,
    /// Lifetime downloaded at the last `started`.
    pub down_base: u64,
    /// Corrupt bytes counted since the process started.
    pub corrupt_total: u64,
    /// `corrupt_total` at the last `started`.
    pub corrupt_base: u64,
}

impl Counters {
    /// Counters restored from persisted lifetime totals; the session starts at zero.
    pub fn restore(up_total: u64, down_total: u64, left: u64) -> Self {
        let c = Self::default();
        c.up_total.store(up_total, Ordering::Relaxed);
        c.down_total.store(down_total, Ordering::Relaxed);
        c.up_base.store(up_total, Ordering::Relaxed);
        c.down_base.store(down_total, Ordering::Relaxed);
        c.left.store(left, Ordering::Relaxed);
        c
    }

    /// Restore from a snapshot, keeping the session base (used when a Cold torrent wakes up).
    pub fn from_snapshot(s: CounterSnapshot, left: u64) -> Self {
        let c = Self::restore(s.up_total, s.down_total, left);
        c.up_base.store(s.up_base, Ordering::Relaxed);
        c.down_base.store(s.down_base, Ordering::Relaxed);
        c.corrupt.store(s.corrupt_total, Ordering::Relaxed);
        c.corrupt_base.store(s.corrupt_base, Ordering::Relaxed);
        c
    }

    /// A piece from peers passed its hash check: its size is downloaded payload.
    pub fn add_down(&self, n: u64) {
        self.down_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Piece payload bytes written to a peer's connection.
    pub fn add_up(&self, n: u64) {
        self.up_total.fetch_add(n, Ordering::Relaxed);
    }

    /// A piece failed verification: its size is corrupt payload.
    pub fn add_corrupt(&self, n: u64) {
        self.corrupt.fetch_add(n, Ordering::Relaxed);
    }

    /// New tracker session: the session counters restart from zero.
    pub fn begin_session(&self) {
        self.up_base
            .store(self.up_total.load(Ordering::Relaxed), Ordering::Relaxed);
        self.down_base
            .store(self.down_total.load(Ordering::Relaxed), Ordering::Relaxed);
        self.corrupt_base
            .store(self.corrupt.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Corrupt bytes this tracker session.
    pub fn session_corrupt(&self) -> u64 {
        self.corrupt
            .load(Ordering::Relaxed)
            .saturating_sub(self.corrupt_base.load(Ordering::Relaxed))
    }

    /// Verified bytes of the wanted files (never announced; `left` stays the tracker's figure).
    pub fn set_wanted_done(&self, n: u64) {
        self.wanted_done.store(n, Ordering::Relaxed);
    }

    /// See [`Counters::set_wanted_done`].
    pub fn wanted_done(&self) -> u64 {
        self.wanted_done.load(Ordering::Relaxed)
    }

    /// Set the exact `left`.
    pub fn set_left(&self, left: u64) {
        self.left.store(left, Ordering::Relaxed);
    }

    /// Uploaded this tracker session.
    pub fn session_up(&self) -> u64 {
        self.up_total
            .load(Ordering::Relaxed)
            .saturating_sub(self.up_base.load(Ordering::Relaxed))
    }

    /// Downloaded this tracker session.
    pub fn session_down(&self) -> u64 {
        self.down_total
            .load(Ordering::Relaxed)
            .saturating_sub(self.down_base.load(Ordering::Relaxed))
    }

    /// Lifetime uploaded.
    pub fn up_total(&self) -> u64 {
        self.up_total.load(Ordering::Relaxed)
    }

    /// Lifetime downloaded.
    pub fn down_total(&self) -> u64 {
        self.down_total.load(Ordering::Relaxed)
    }

    /// Bytes still missing.
    pub fn left(&self) -> u64 {
        self.left.load(Ordering::Relaxed)
    }

    /// Corrupt bytes since the process started.
    pub fn corrupt(&self) -> u64 {
        self.corrupt.load(Ordering::Relaxed)
    }

    /// Copy of the lifetime totals and session bases.
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            up_total: self.up_total(),
            down_total: self.down_total(),
            up_base: self.up_base.load(Ordering::Relaxed),
            down_base: self.down_base.load(Ordering::Relaxed),
            corrupt_total: self.corrupt(),
            corrupt_base: self.corrupt_base.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_resets_at_started_and_lifetime_does_not() {
        let c = Counters::restore(1000, 2000, 500);
        assert_eq!((c.session_up(), c.session_down()), (0, 0), "restart begins a session at zero");
        c.add_down(300);
        c.add_up(50);
        assert_eq!((c.session_up(), c.session_down()), (50, 300));
        // pause + resume = a new `started`
        c.begin_session();
        assert_eq!((c.session_up(), c.session_down()), (0, 0));
        assert_eq!((c.up_total(), c.down_total()), (1050, 2300), "lifetime survives");
        c.add_down(10);
        assert_eq!(c.session_down(), 10);
        assert_eq!(c.down_total(), 2310);
    }

    #[test]
    fn left_is_independent_of_downloaded() {
        let c = Counters::restore(0, 0, 1000);
        c.add_down(250); // one verified piece
        c.add_corrupt(150); // one piece that failed its hash: not downloaded, not progress
        c.set_left(1000 - 250);
        assert_eq!(c.session_down(), 250);
        assert_eq!(c.left(), 750);
        assert_eq!((c.corrupt(), c.session_corrupt()), (150, 150));
        c.begin_session();
        assert_eq!((c.corrupt(), c.session_corrupt()), (150, 0), "corrupt restarts with each started");
        c.add_corrupt(5);
        assert_eq!(c.session_corrupt(), 5);
    }

    #[test]
    fn snapshot_roundtrip_keeps_the_session_base() {
        let c = Counters::restore(10, 20, 0);
        c.add_up(5);
        c.add_down(7);
        let s = c.snapshot();
        let d = Counters::from_snapshot(s, 0);
        assert_eq!((d.session_up(), d.session_down()), (5, 7));
        assert_eq!((d.up_total(), d.down_total()), (15, 27));
    }
}
