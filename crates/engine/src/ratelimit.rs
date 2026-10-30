//! Global download/upload limiter with priority classes (SPEC 7.3, ENGINE.md 2).
//!
//! A token bucket per direction, split into three class buckets (High / Normal / Low) with
//! weights 4 : 2 : 1. Tokens are produced at the configured rate and shared by weight among the
//! classes that asked for bandwidth in the last second, so an idle class never wastes its share
//! and a saturated limit divides 4 : 2 : 1 between busy classes.
//!
//! Callers *charge* the bytes they moved and get back how long to wait before moving more
//! ("debt model"). That keeps the hot path to one short mutex section and needs no timers when a
//! direction is unlimited.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::api::Priority;

/// Transfer direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Bytes received.
    Down,
    /// Bytes sent.
    Up,
}

const CLASSES: usize = 3;
const WEIGHTS: [f64; CLASSES] = [4.0, 2.0, 1.0];
/// A class that has not been charged for this long stops receiving a share.
const ACTIVE_WINDOW: Duration = Duration::from_millis(1000);

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last_demand: Option<Instant>,
}

#[derive(Debug)]
struct Direction {
    /// Bytes per second; 0 = unlimited.
    rate: u64,
    last_refill: Option<Instant>,
    buckets: [Bucket; CLASSES],
}

impl Direction {
    fn new(rate: Option<u64>) -> Self {
        Self {
            rate: rate.unwrap_or(0),
            last_refill: None,
            buckets: [Bucket {
                tokens: 0.0,
                last_demand: None,
            }; CLASSES],
        }
    }

    fn active_weight(&self, now: Instant, also: usize) -> f64 {
        let mut w = 0.0;
        for (i, b) in self.buckets.iter().enumerate() {
            let busy = i == also
                || b.last_demand
                    .is_some_and(|t| now.saturating_duration_since(t) < ACTIVE_WINDOW);
            if busy {
                w += WEIGHTS[i];
            }
        }
        w
    }

    fn refill(&mut self, now: Instant, charging: usize) {
        let Some(last) = self.last_refill else {
            self.last_refill = Some(now);
            return;
        };
        let dt = now.saturating_duration_since(last).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        self.last_refill = Some(now);
        let produced = self.rate as f64 * dt;
        let total_w = self.active_weight(now, charging);
        // One second of burst per class share keeps short stalls from losing throughput without
        // letting a long-idle class bank unbounded credit.
        let burst = (self.rate as f64).max(32.0 * 1024.0);
        for (i, (b, w)) in self.buckets.iter_mut().zip(WEIGHTS).enumerate() {
            let busy = i == charging
                || b.last_demand.is_some_and(|t| now.saturating_duration_since(t) < ACTIVE_WINDOW);
            if !busy {
                // Spend nothing on idle classes, and forgive their debt.
                if b.tokens < 0.0 {
                    b.tokens = 0.0;
                }
                continue;
            }
            let share = produced * w / total_w;
            let cap = burst * w / total_w;
            b.tokens = (b.tokens + share).min(cap.max(16.0 * 1024.0));
        }
    }
}

/// Shared limiter. Cheap to clone behind an `Arc`.
#[derive(Debug)]
pub struct RateLimiter {
    down_limited: AtomicBool,
    up_limited: AtomicBool,
    inner: Mutex<[Direction; 2]>,
}

fn class_of(p: Priority) -> usize {
    match p {
        Priority::High => 0,
        Priority::Normal => 1,
        Priority::Low => 2,
    }
}

impl RateLimiter {
    /// New limiter; `None` is unlimited.
    pub fn new(down: Option<u64>, up: Option<u64>) -> Self {
        Self {
            down_limited: AtomicBool::new(down.is_some()),
            up_limited: AtomicBool::new(up.is_some()),
            inner: Mutex::new([Direction::new(down), Direction::new(up)]),
        }
    }

    /// Change the limits at runtime (alt-speed toggles, settings).
    pub fn set_limits(&self, down: Option<u64>, up: Option<u64>) {
        let mut g = self.lock();
        g[0].rate = down.unwrap_or(0);
        g[1].rate = up.unwrap_or(0);
        self.down_limited.store(down.is_some(), Ordering::Relaxed);
        self.up_limited.store(up.is_some(), Ordering::Relaxed);
    }

    /// Whether `dir` has a limit. Lock-free, for the unlimited fast path.
    pub fn is_limited(&self, dir: Dir) -> bool {
        match dir {
            Dir::Down => self.down_limited.load(Ordering::Relaxed),
            Dir::Up => self.up_limited.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, [Direction; 2]> {
        // The state is plain numbers; a poisoned lock is still consistent.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Account `n` bytes moved by a torrent of `priority`. Returns how long the caller should wait
    /// before moving more; `Duration::ZERO` means go on.
    pub fn charge(&self, dir: Dir, priority: Priority, n: u64, now: Instant) -> Duration {
        if !self.is_limited(dir) {
            return Duration::ZERO;
        }
        let class = class_of(priority);
        let mut g = self.lock();
        let d = &mut g[dir as usize];
        if d.rate == 0 {
            return Duration::ZERO;
        }
        d.refill(now, class);
        d.buckets[class].last_demand = Some(now);
        d.buckets[class].tokens -= n as f64;
        let tokens = d.buckets[class].tokens;
        if tokens >= 0.0 {
            return Duration::ZERO;
        }
        let share = d.rate as f64 * WEIGHTS[class] / d.active_weight(now, class);
        Duration::from_secs_f64((-tokens / share.max(1.0)).min(5.0))
    }
}

/// A per-torrent cap (rule `up_limit`), applied on top of the global limiter. Same debt model:
/// charge what was moved, wait as long as the answer says.
#[derive(Debug, Default)]
pub struct TorrentLimit {
    /// Bytes per second; 0 = unlimited.
    rate: std::sync::atomic::AtomicU64,
    state: Mutex<(f64, Option<Instant>)>,
}

impl TorrentLimit {
    /// Set or clear the limit.
    pub fn set(&self, bytes_per_sec: Option<u64>) {
        self.rate.store(bytes_per_sec.unwrap_or(0), Ordering::Relaxed);
    }

    /// Whether a limit is set (lock-free fast path).
    pub fn is_limited(&self) -> bool {
        self.rate.load(Ordering::Relaxed) != 0
    }

    /// Account `n` bytes; returns how long to wait before moving more.
    pub fn charge(&self, n: u64, now: Instant) -> Duration {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return Duration::ZERO;
        }
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (tokens, last) = &mut *g;
        if let Some(l) = *last {
            let dt = now.saturating_duration_since(l).as_secs_f64();
            // One second of burst: enough to ride out scheduling jitter, not to bank credit.
            *tokens = (*tokens + dt * rate as f64).min(rate as f64);
        } else {
            *tokens = rate as f64;
        }
        *last = Some(now);
        *tokens -= n as f64;
        if *tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((-*tokens / rate as f64).min(5.0))
        }
    }
}

/// The shared caps of one favorite tracker (SPEC 7E.4): one bucket per direction for all the
/// torrents of that tracker together. Only favorites that have a limit own one.
#[derive(Debug, Default)]
pub struct GroupBucket {
    /// Download cap shared by the group's torrents.
    pub down: TorrentLimit,
    /// Upload cap shared by the group's torrents.
    pub up: TorrentLimit,
}

impl GroupBucket {
    /// The bucket of `dir`.
    pub fn of(&self, dir: Dir) -> &TorrentLimit {
        match dir {
            Dir::Down => &self.down,
            Dir::Up => &self.up,
        }
    }
}

/// The buckets of all limited favorites, by profile id. An unlimited favorite has no entry, so a
/// library without tracker limits holds an empty map and pays nothing per transfer.
#[derive(Debug, Default)]
pub struct GroupLimits {
    map: Mutex<std::collections::HashMap<String, std::sync::Arc<GroupBucket>>>,
}

impl GroupLimits {
    /// Bring the buckets in line with the profiles: `(id, down, up)` of every profile. A bucket
    /// that already exists keeps its identity (torrents hold it) and just takes the new rates;
    /// a profile that lost its limits drops its bucket after the bucket is switched off.
    pub fn sync<'a>(&self, profiles: impl IntoIterator<Item = (&'a str, Option<u64>, Option<u64>)>) {
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let mut keep = std::collections::HashSet::new();
        for (id, down, up) in profiles {
            if down.is_none() && up.is_none() {
                continue;
            }
            keep.insert(id.to_owned());
            let b = m.entry(id.to_owned()).or_default();
            b.down.set(down);
            b.up.set(up);
        }
        m.retain(|id, b| {
            let k = keep.contains(id);
            if !k {
                b.down.set(None);
                b.up.set(None);
            }
            k
        });
    }

    /// The bucket of profile `id`, if it has a limit.
    pub fn get(&self, id: &str) -> Option<std::sync::Arc<GroupBucket>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned()
    }

    /// How many buckets exist.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// No bucket exists.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A torrent's link to its tracker group; empty for the common case.
#[derive(Debug, Default)]
pub struct GroupSlot {
    set: AtomicBool,
    group: std::sync::RwLock<Option<std::sync::Arc<GroupBucket>>>,
}

impl GroupSlot {
    /// Join (or leave, with `None`) a group.
    pub fn assign(&self, g: Option<std::sync::Arc<GroupBucket>>) {
        self.set.store(g.is_some(), Ordering::Relaxed);
        *self.group.write().unwrap_or_else(|p| p.into_inner()) = g;
    }

    /// Whether the group caps `dir` (lock-free when the torrent has no group).
    pub fn is_limited(&self, dir: Dir) -> bool {
        self.set.load(Ordering::Relaxed)
            && self.group.read().unwrap_or_else(|p| p.into_inner()).as_ref().is_some_and(|g| g.of(dir).is_limited())
    }

    /// Charge `n` bytes to the group; how long to wait.
    pub fn charge(&self, dir: Dir, n: u64, now: Instant) -> Duration {
        if !self.set.load(Ordering::Relaxed) {
            return Duration::ZERO;
        }
        match &*self.group.read().unwrap_or_else(|p| p.into_inner()) {
            Some(g) => g.of(dir).charge(n, now),
            None => Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_limit_paces_to_its_rate() {
        let l = TorrentLimit::default();
        assert!(!l.is_limited());
        assert_eq!(l.charge(1 << 20, Instant::now()), Duration::ZERO);
        l.set(Some(100_000));
        let t0 = Instant::now();
        let mut now = t0;
        let mut moved = 0u64;
        while now < t0 + Duration::from_secs(10) {
            let wait = l.charge(16 * 1024, now);
            moved += 16 * 1024;
            now += wait.max(Duration::from_millis(1));
        }
        // 10 s at 100 kB/s plus the one-second burst.
        assert!((900_000..=1_250_000).contains(&moved), "moved {moved}");
        l.set(None);
        assert_eq!(l.charge(1 << 30, now), Duration::ZERO);
    }

    /// Drives `classes` with greedy 16 KiB senders for `secs` of simulated time and returns the
    /// bytes each moved.
    fn simulate(rate: u64, classes: &[Priority], secs: u64) -> Vec<u64> {
        let lim = RateLimiter::new(None, Some(rate));
        let t0 = Instant::now();
        let mut now = t0;
        let mut moved = vec![0u64; classes.len()];
        let mut ready_at = vec![t0; classes.len()];
        let end = t0 + Duration::from_secs(secs);
        while now < end {
            let mut progressed = false;
            for (i, p) in classes.iter().enumerate() {
                if ready_at[i] <= now {
                    let wait = lim.charge(Dir::Up, *p, 16 * 1024, now);
                    moved[i] += 16 * 1024;
                    ready_at[i] = now + wait;
                    progressed = true;
                }
            }
            if !progressed {
                now = *ready_at.iter().min().unwrap();
            } else {
                now += Duration::from_micros(100);
            }
        }
        moved
    }

    #[test]
    fn unlimited_never_waits() {
        let lim = RateLimiter::new(None, None);
        let now = Instant::now();
        assert!(!lim.is_limited(Dir::Down));
        assert_eq!(lim.charge(Dir::Down, Priority::Normal, 1 << 30, now), Duration::ZERO);
    }

    #[test]
    fn single_class_converges_to_the_limit() {
        let rate = 1_000_000;
        let moved = simulate(rate, &[Priority::Normal], 20);
        let avg = moved[0] as f64 / 20.0;
        assert!(
            (avg - rate as f64).abs() / (rate as f64) < 0.10,
            "average {avg} vs limit {rate}"
        );
    }

    #[test]
    fn saturated_classes_share_four_two_one() {
        let rate = 7_000_000;
        let moved = simulate(rate, &[Priority::High, Priority::Normal, Priority::Low], 30);
        let total: u64 = moved.iter().sum();
        let rate_total = total as f64 / 30.0;
        assert!((rate_total - rate as f64).abs() / (rate as f64) < 0.10, "{rate_total}");
        let r = |i: usize| moved[i] as f64 / total as f64;
        assert!((r(0) - 4.0 / 7.0).abs() < 0.05, "high {}", r(0));
        assert!((r(1) - 2.0 / 7.0).abs() < 0.05, "normal {}", r(1));
        assert!((r(2) - 1.0 / 7.0).abs() < 0.05, "low {}", r(2));
    }

    #[test]
    fn an_idle_class_gives_its_share_away() {
        let rate = 3_000_000;
        // High is absent: Normal and Low split 2 : 1, using the whole limit.
        let moved = simulate(rate, &[Priority::Normal, Priority::Low], 30);
        let total: u64 = moved.iter().sum();
        assert!((total as f64 / 30.0 - rate as f64).abs() / (rate as f64) < 0.10);
        let normal = moved[0] as f64 / total as f64;
        assert!((normal - 2.0 / 3.0).abs() < 0.05, "{normal}");
    }

    #[test]
    fn unlimited_favorites_get_no_bucket_and_limits_change_live() {
        let g = GroupLimits::default();
        g.sync([("a", None, None), ("b", Some(1000), None), ("c", None, Some(5))]);
        assert!(g.get("a").is_none() && g.len() == 2);
        let b = g.get("b").unwrap();
        assert!(b.down.is_limited() && !b.up.is_limited());
        let slot = GroupSlot::default();
        assert!(!slot.is_limited(Dir::Down));
        slot.assign(Some(b.clone()));
        assert!(slot.is_limited(Dir::Down) && !slot.is_limited(Dir::Up));
        // Live: the same bucket takes the new rate, and a removed limit switches it off.
        g.sync([("b", Some(2000), Some(10))]);
        assert!(std::sync::Arc::ptr_eq(&b, &g.get("b").unwrap()));
        assert!(slot.is_limited(Dir::Up) && g.get("c").is_none());
        g.sync([("b", None, None)]);
        assert!(g.is_empty() && !slot.is_limited(Dir::Down) && !slot.is_limited(Dir::Up));
        assert_eq!(slot.charge(Dir::Down, 1 << 30, Instant::now()), Duration::ZERO);
    }

    #[test]
    fn group_bucket_is_shared_by_its_torrents() {
        let g = GroupLimits::default();
        g.sync([("t", Some(100_000), None)]);
        let slots: Vec<GroupSlot> = (0..3).map(|_| GroupSlot::default()).collect();
        for s in &slots {
            s.assign(g.get("t"));
        }
        let t0 = Instant::now();
        let mut now = t0;
        let mut ready = [t0; 3];
        let mut moved = 0u64;
        while now < t0 + Duration::from_secs(10) {
            for (i, s) in slots.iter().enumerate() {
                if ready[i] <= now {
                    ready[i] = now + s.charge(Dir::Down, 16 * 1024, now);
                    moved += 16 * 1024;
                }
            }
            now += Duration::from_micros(500);
        }
        // 10 s at 100 kB/s plus the one-second burst, however many torrents draw from it.
        assert!((900_000..=1_250_000).contains(&moved), "moved {moved}");
    }

    #[test]
    fn limits_can_change_at_runtime() {
        let lim = RateLimiter::new(Some(1000), None);
        assert!(lim.is_limited(Dir::Down));
        lim.set_limits(None, Some(10));
        assert!(!lim.is_limited(Dir::Down) && lim.is_limited(Dir::Up));
        let now = Instant::now();
        assert_eq!(lim.charge(Dir::Down, Priority::High, 1 << 20, now), Duration::ZERO);
    }
}

#[cfg(test)]
mod shared_cap_tests {
    use super::*;

    /// Virtual clock: `readers` share one cap; each moves `chunk` bytes, charges them and sleeps what the cap answers
    /// plus up to `late_ms` (a coarse system timer: Windows wakes up to ~16 ms late). Nobody reads during `stall`
    /// (`(ms, ending at ms)`). Returns bytes per second moved in [2 s, 7 s].
    fn window_rate(rate: u64, readers: usize, chunk: u64, late_ms: u64, seed: u64, stall: Option<(u64, u64)>) -> f64 {
        let lim = TorrentLimit::default();
        lim.set(Some(rate));
        let t0 = Instant::now();
        let mut next = vec![0u64; readers]; // µs
        let mut rng = seed;
        let mut rand = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let (from, to) = (2_000_000u64, 7_000_000u64);
        let mut window = 0u64;
        loop {
            let (i, &t) = next.iter().enumerate().min_by_key(|(_, t)| **t).expect("readers");
            if t >= to {
                break;
            }
            if let Some((len, end)) = stall
                && t + len * 1000 >= end * 1000
                && t < end * 1000
            {
                next[i] = end * 1000;
                continue;
            }
            let wait = lim.charge(chunk, t0 + Duration::from_micros(t));
            if t >= from {
                window += chunk;
            }
            let late = if late_ms == 0 { 0 } else { rand() % (late_ms * 1000) };
            next[i] = t + wait.as_micros() as u64 + late + 50;
        }
        window as f64 / 5.0
    }

    #[test]
    fn late_wake_ups_are_made_up_so_the_cap_holds_on_a_coarse_timer() {
        for late in [0, 16, 32] {
            for chunk in [16_384, 65_536] {
                for seed in 1..=20 {
                    let r = window_rate(800_000, 6, chunk, late, seed, None);
                    assert!((r - 800_000.0).abs() / 800_000.0 < 0.02, "late {late} ms, chunk {chunk}: {r:.0} B/s");
                }
            }
        }
    }

    #[test]
    fn after_a_pause_at_most_one_second_of_the_rate_comes_at_once() {
        for stall in [500, 1000, 3000] {
            let r = window_rate(800_000, 6, 65_536, 16, 1, Some((stall, 2000)));
            // The window gets its five seconds at the cap, plus no more than the one second banked in the pause, plus
            // what each reader moved last and pays for after the window (it is charged after it moved).
            let most = (800_000.0 * 6.0 + 6.0 * 65_536.0) / 5.0;
            assert!(r <= most * 1.001, "after a {stall} ms pause: {r:.0} B/s, at most {most:.0}");
            assert!(r >= 800_000.0 * 0.99, "{r:.0}");
        }
    }
}
