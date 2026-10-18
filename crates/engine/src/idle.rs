//! The "inactive" stop condition (SPEC §7.5): a small ring of coarse upload buckets per torrent.
//!
//! The window is a *running* window of **seeding time**: the ring only advances while the torrent
//! seeds (app running, torrent active and finished), so a Mac that was off, or a torrent that was
//! paused, neither adds empty buckets nor ages old ones out. The window is cut into 24 slots of
//! `ceil(window / 24)` seconds; 25 buckets hold the 24 full slots and the one being filled, so the
//! sum always covers at least one full window (the condition errs on the side of keeping a seed).
//! Counts are KiB in `u32`; the ring is 127 bytes serialised and lives only on torrents that have
//! the condition enabled.

/// Buckets in the ring.
pub const BUCKETS: usize = 25;
const SLOTS: u32 = (BUCKETS - 1) as u32;
/// Serialised size.
pub const RING_BYTES: usize = 127;
const VERSION: u8 = 1;

/// Upload history of one finished torrent over a window of seeding time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleRing {
    window_secs: u32,
    slot_secs: u32,
    cur: u8,
    /// Seeding seconds elapsed inside the current slot.
    in_slot: u32,
    /// Seeding seconds since the ring started (stops growing at one window).
    armed_secs: u32,
    /// `seeded_secs` of the torrent at the last observation.
    last_seeded: u32,
    /// Lifetime uploaded bytes at the last observation (whole KiB only, so nothing is lost).
    last_up: u64,
    /// The torrent was stopped by this condition (so its row can say why).
    pub stopped: bool,
    kib: [u32; BUCKETS],
    /// Not persisted: unix seconds of the last time the ring was written to disk.
    pub saved_at: u32,
    /// Not persisted: changed since the last write.
    pub unsaved: bool,
}

impl IdleRing {
    /// A new ring: nothing counted yet, the window starts now.
    pub fn new(window_secs: u32, seeded_secs: u32, uploaded: u64) -> Self {
        let window_secs = window_secs.max(1);
        IdleRing {
            window_secs,
            slot_secs: window_secs.div_ceil(SLOTS).max(1),
            cur: 0,
            in_slot: 0,
            armed_secs: 0,
            last_seeded: seeded_secs,
            last_up: uploaded,
            stopped: false,
            kib: [0; BUCKETS],
            saved_at: 0,
            unsaved: true,
        }
    }

    /// The window this ring measures, seconds of seeding.
    pub fn window_secs(&self) -> u32 {
        self.window_secs
    }

    /// Account what happened since the last call: `seeded_secs` is the torrent's total seeding
    /// time now, `uploaded` its lifetime upload. Seeding time that passed ages the ring.
    pub fn observe(&mut self, seeded_secs: u32, uploaded: u64) {
        let ds = seeded_secs.saturating_sub(self.last_seeded);
        self.last_seeded = seeded_secs;
        self.credit(uploaded);
        self.advance(ds);
        self.unsaved = true;
    }

    /// After a restart: take the uploads made since the ring was saved, but do not age it by the
    /// seeding time that passed unobserved (that time is not known to have been idle).
    pub fn rebase(&mut self, seeded_secs: u32, uploaded: u64) {
        self.last_seeded = seeded_secs;
        self.credit(uploaded);
    }

    fn credit(&mut self, uploaded: u64) {
        if uploaded < self.last_up {
            self.last_up = uploaded;
            return;
        }
        let kib = (uploaded - self.last_up) / 1024;
        self.last_up += kib * 1024;
        let c = &mut self.kib[usize::from(self.cur)];
        *c = c.saturating_add(u32::try_from(kib).unwrap_or(u32::MAX));
    }

    fn advance(&mut self, ds: u32) {
        self.armed_secs = self.armed_secs.saturating_add(ds).min(self.window_secs);
        if u64::from(ds) >= u64::from(self.slot_secs) * BUCKETS as u64 {
            self.kib = [0; BUCKETS];
            self.in_slot = ds % self.slot_secs;
            return;
        }
        self.in_slot += ds;
        while self.in_slot >= self.slot_secs {
            self.in_slot -= self.slot_secs;
            self.cur = (self.cur + 1) % BUCKETS as u8;
            self.kib[usize::from(self.cur)] = 0;
        }
    }

    /// Bytes uploaded in the last window of seeding.
    pub fn uploaded(&self) -> u64 {
        self.kib.iter().map(|&k| u64::from(k)).sum::<u64>() * 1024
    }

    /// Has the torrent seeded for one full window since the ring started?
    pub fn armed(&self) -> bool {
        self.armed_secs >= self.window_secs
    }

    /// Seeding seconds left until the condition may first fire.
    pub fn arms_in(&self) -> u32 {
        self.window_secs - self.armed_secs
    }

    /// The condition holds: a full window has been seeded and less than `below` bytes went out.
    pub fn idle(&self, below: u64) -> bool {
        self.armed() && self.uploaded() < below
    }

    /// Fixed-size form for the side file.
    pub fn to_bytes(&self) -> [u8; RING_BYTES] {
        let mut o = [0u8; RING_BYTES];
        o[0] = VERSION;
        o[1] = u8::from(self.stopped);
        o[2..6].copy_from_slice(&self.window_secs.to_le_bytes());
        o[6] = self.cur;
        o[7..11].copy_from_slice(&self.in_slot.to_le_bytes());
        o[11..15].copy_from_slice(&self.armed_secs.to_le_bytes());
        o[15..19].copy_from_slice(&self.last_seeded.to_le_bytes());
        o[19..27].copy_from_slice(&self.last_up.to_le_bytes());
        for (i, k) in self.kib.iter().enumerate() {
            o[27 + i * 4..31 + i * 4].copy_from_slice(&k.to_le_bytes());
        }
        o
    }

    /// Read the side-file form; `None` for anything that does not look like a ring.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != RING_BYTES || b[0] != VERSION || usize::from(b[6]) >= BUCKETS {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let window_secs = u32_at(2);
        if window_secs == 0 {
            return None;
        }
        let mut r = IdleRing::new(window_secs, u32_at(15), u64::from_le_bytes(b[19..27].try_into().ok()?));
        r.stopped = b[1] & 1 != 0;
        r.cur = b[6];
        r.in_slot = u32_at(7).min(r.slot_secs - 1);
        r.armed_secs = u32_at(11).min(window_secs);
        for i in 0..BUCKETS {
            r.kib[i] = u32_at(27 + i * 4);
        }
        r.unsaved = false;
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u32 = 3600;
    const MB: u64 = 1_000_000;

    /// Seed for `secs` while uploading `bytes` in total, observing once a minute.
    fn run(r: &mut IdleRing, seeded: &mut u32, up: &mut u64, secs: u32, bytes: u64) {
        let steps = (secs / 60).max(1);
        for _ in 0..steps {
            *seeded += secs / steps;
            *up += bytes / u64::from(steps);
            r.observe(*seeded, *up);
        }
    }

    #[test]
    fn window_slides_over_seeding_time() {
        let (mut s, mut u) = (0u32, 0u64);
        let mut r = IdleRing::new(150 * H, s, u);
        run(&mut r, &mut s, &mut u, 10 * H, 600 * MB);
        assert!(!r.armed(), "not before a full window");
        assert!(!r.idle(1_000_000_000), "never fires before a full window has been seeded");
        // 140 more hours with no uploads: the 600 MB are still inside the last 150 h.
        run(&mut r, &mut s, &mut u, 140 * H, 0);
        assert!(r.armed());
        assert!(r.uploaded() >= 590 * MB, "{}", r.uploaded());
        assert!(!r.idle(500 * MB), "enough went out in the window");
        // Another 20 hours of nothing: the early upload has slid out.
        run(&mut r, &mut s, &mut u, 20 * H, 0);
        assert_eq!(r.uploaded(), 0);
        assert!(r.idle(1_000_000_000));
    }

    #[test]
    fn a_busy_seed_never_fires() {
        let (mut s, mut u) = (0u32, 0u64);
        let mut r = IdleRing::new(150 * H, s, u);
        for _ in 0..40 {
            run(&mut r, &mut s, &mut u, 10 * H, 300 * MB);
            assert!(!r.idle(1_000_000_000), "about 4.5 GB per window");
        }
    }

    #[test]
    fn time_without_seeding_changes_nothing() {
        let (mut s, mut u) = (0u32, 0u64);
        let mut r = IdleRing::new(150 * H, s, u);
        run(&mut r, &mut s, &mut u, 100 * H, 5_000 * MB);
        let before = r.clone();
        // The torrent was off for a month: observing with unchanged seeded_secs ages nothing.
        r.observe(s, u);
        assert_eq!(r.uploaded(), before.uploaded());
        assert_eq!(r.arms_in(), before.arms_in());
    }

    #[test]
    fn restart_keeps_history_and_does_not_age_the_gap() {
        let (mut s, mut u) = (0u32, 0u64);
        let mut r = IdleRing::new(150 * H, s, u);
        run(&mut r, &mut s, &mut u, 100 * H, 2_000 * MB);
        let mut back = IdleRing::from_bytes(&r.to_bytes()).unwrap();
        assert_eq!(back.uploaded(), r.uploaded());
        assert_eq!(back.arms_in(), r.arms_in());
        // Counters moved on a bit before the crash: uploads are credited, time is not aged.
        back.rebase(s + 300, u + 5 * MB);
        assert!(back.uploaded() > r.uploaded());
        assert_eq!(back.arms_in(), r.arms_in());
        assert!(IdleRing::from_bytes(&[0; 5]).is_none());
        assert_eq!(RING_BYTES, r.to_bytes().len());
    }

    #[test]
    fn a_new_ring_after_a_manual_resume_starts_its_window_again() {
        let (mut s, mut u) = (0u32, 0u64);
        let mut r = IdleRing::new(150 * H, s, u);
        run(&mut r, &mut s, &mut u, 200 * H, 0);
        assert!(r.idle(1));
        r = IdleRing::new(150 * H, s, u);
        assert!(!r.idle(1), "the resume restarted the window");
        run(&mut r, &mut s, &mut u, 149 * H, 0);
        assert!(!r.idle(1));
        run(&mut r, &mut s, &mut u, 2 * H, 0);
        assert!(r.idle(1));
    }

    #[test]
    fn one_huge_gap_clears_the_ring_without_looping() {
        let mut r = IdleRing::new(10, 0, 0);
        r.observe(1, 5 * 1024 * 1024);
        r.observe(u32::MAX, 5 * 1024 * 1024);
        assert_eq!(r.uploaded(), 0);
        assert!(r.armed());
    }
}
