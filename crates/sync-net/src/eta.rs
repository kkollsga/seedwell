//! A syncing folder's time left: the bytes still to fetch over a steady rate.
//!
//! Bytes arrive a whole chunk at a time (up to 16 MiB; [`crate::limit::Meter`] counts them when the chunk is in), so on
//! a slow link the counter stands still for many seconds and then jumps. A rate averaged per second swings between
//! nothing and many times the real one, and a time left from it jumps with it. Here the rate is measured from arrival
//! to arrival instead (the bytes of the arrivals after the first over the time between the first and the last, over
//! the last [`WINDOW`] and at least [`MIN_ARRIVALS`] arrivals), which is the real rate however lumpy the arrivals are,
//! and between arrivals the time left counts down (at most by the usual gap, so it never runs past where the next
//! arrival puts it).
//!
//! No time is given before [`WARM_UP`] of arrivals, while nothing arrives for [`STALL`] (or three usual gaps, when
//! arrivals are further apart), from a second arrival that came after a wait longer than [`STALL`] to the next one
//! (which tells a stall after a single arrival, left out of the measure, from a slow link's usual gap), for less than [`SHORT_S`] left, for more than [`LONG_S`], and when the folder does
//! not sync. A host polls [`crate::SyncHandle`]'s status as often as it likes: answers don't depend on how often.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The arrivals the rate is measured over (more when fewer than [`MIN_ARRIVALS`] are this recent).
pub const WINDOW: Duration = Duration::from_secs(30);
/// The fewest arrivals (the first one included) the rate is measured over, however old.
pub const MIN_ARRIVALS: usize = 3;
/// No time before the arrivals span this long.
pub const WARM_UP: Duration = Duration::from_secs(5);
/// Nothing arrived for this long (or three usual gaps): stalled, no time shown.
pub const STALL: Duration = Duration::from_secs(15);
/// Less than this left (seconds): over before anyone reads it.
pub const SHORT_S: u64 = 10;
/// More than this left (seconds, 30 days): a trickle, no estimate anyone can use.
pub const LONG_S: u64 = 30 * 86_400;
/// Arrivals seen closer together than this are one (a host polling ten times a second keeps the list short).
const MERGE: Duration = Duration::from_secs(1);
/// The most arrivals kept.
const KEEP: usize = 128;

/// One folder's estimate, fed by each status.
#[derive(Debug, Default)]
pub struct Eta {
    /// The last look: when, and the counter then. `None` while the folder does not sync.
    looked: Option<(Instant, u64)>,
    /// When the counter was seen to move, and its total then (about a [`MERGE`] apart at the closest). The first one
    /// starts the measure (its bytes are not counted): the wait for a sync's first bytes (finding a device, asking) is
    /// no rate.
    seen: VecDeque<(Instant, u64)>,
    /// The second arrival came after a wait longer than a stall: a stall after a single arrival, or a slow link's
    /// usual gap. The next arrival tells (a wait far longer than the gap after it was a stall); no time until then.
    unsure: bool,
}

impl Eta {
    /// The time left, seconds, at `now`: `remaining` bytes to fetch, `fetched` the folder's counter of bytes fetched
    /// so far ([`crate::limit::Meter::total`]). `syncing`: the folder syncs now; anything else ends the run, and the
    /// next one starts afresh. `None`: not known (see the module's rules).
    pub fn sample(&mut self, now: Instant, fetched: u64, remaining: u64, syncing: bool) -> Option<u64> {
        if !syncing {
            self.looked = None;
            self.restart();
            return None;
        }
        let (looked_at, looked_total) = self.looked.replace((now, fetched))?;
        if fetched < looked_total {
            // A counter that went back (never, but a new start is the safe answer).
            self.restart();
            return None;
        }
        if fetched == looked_total {
            return if self.stalled(now) { None } else { self.estimate(now, remaining) };
        }
        let n = self.seen.len();
        if n > 1 && self.stalled(looked_at) {
            // Moving again after a stall: the stall would read as a slow rate, so the measure starts here.
            self.restart();
        } else if n == 1 && self.stalled(looked_at) {
            // After a single arrival there is no usual gap yet to tell a stall from a slow link: the next one tells.
            self.unsure = true;
        } else if std::mem::take(&mut self.unsure) && n == 2 {
            // The wait after the single first arrival was a stall when it is far longer than the gap since: the
            // measure starts after it (else the stall stays in the rate until that arrival leaves the window).
            let wait = self.seen[1].0.saturating_duration_since(self.seen[0].0);
            if wait > STALL.max(now.saturating_duration_since(self.seen[1].0) * 3) {
                self.seen.pop_front();
            }
        } else if n > 1 && now.saturating_duration_since(self.seen[n - 2].0) < MERGE {
            self.seen.pop_back();
        }
        self.seen.push_back((now, fetched));
        self.trim(now);
        self.estimate(now, remaining)
    }

    /// Starts the measure afresh.
    fn restart(&mut self) {
        self.seen.clear();
        self.unsure = false;
    }

    /// The usual time between arrivals, when there were any.
    fn gap(&self) -> Option<Duration> {
        let (first, last) = (self.seen.front()?.0, self.seen.back()?.0);
        (self.seen.len() > 1).then(|| last.saturating_duration_since(first) / (self.seen.len() as u32 - 1))
    }

    /// Nothing arrived from the last arrival to `at` for longer than a stall.
    fn stalled(&self, at: Instant) -> bool {
        let Some(&(last, _)) = self.seen.back() else { return false };
        let limit = self.gap().map_or(STALL, |g| STALL.max(g * 3));
        at.saturating_duration_since(last) > limit
    }

    /// Drops arrivals older than the window, keeping at least [`MIN_ARRIVALS`] and at most [`KEEP`].
    fn trim(&mut self, now: Instant) {
        while self.seen.len() > MIN_ARRIVALS && (self.seen.len() > KEEP || now.saturating_duration_since(self.seen[1].0) > WINDOW) {
            self.seen.pop_front();
        }
    }

    fn estimate(&self, now: Instant, remaining: u64) -> Option<u64> {
        let (&(first_at, first_total), &(last_at, last_total)) = (self.seen.front()?, self.seen.back()?);
        let span = last_at.saturating_duration_since(first_at);
        if self.unsure || span < WARM_UP || last_total <= first_total {
            return None;
        }
        let rate = (last_total - first_total) as f64 / span.as_secs_f64();
        let since = now.saturating_duration_since(last_at).min(self.gap().unwrap_or_default());
        let secs = (remaining as f64 / rate - since.as_secs_f64()).max(0.0).ceil() as u64;
        (SHORT_S..=LONG_S).contains(&secs).then_some(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    /// A folder that fetches: `remaining` bytes left at the start, the counter, and the clock.
    struct Run {
        eta: Eta,
        t0: Instant,
        at: Duration,
        fetched: u64,
        remaining: u64,
    }

    impl Run {
        fn new(remaining: u64) -> Run {
            Run { eta: Eta::default(), t0: Instant::now(), at: Duration::ZERO, fetched: 0, remaining }
        }

        /// `secs` seconds pass (polled every `poll`), `bytes` arriving at once at their end.
        fn pass(&mut self, secs: f64, poll: Duration, bytes: u64) -> Vec<Option<u64>> {
            let end = self.at + Duration::from_secs_f64(secs);
            let mut out = Vec::new();
            while self.at + poll < end {
                self.at += poll;
                out.push(self.poll());
            }
            self.at = end;
            self.fetched += bytes;
            self.remaining = self.remaining.saturating_sub(bytes);
            out.push(self.poll());
            out
        }

        fn poll(&mut self) -> Option<u64> {
            self.eta.sample(self.t0 + self.at, self.fetched, self.remaining, true)
        }
    }

    /// Bytes flowing evenly (1 MB a second, 300 MB left): about 300 s, shown once the warm-up is over and not before.
    /// Fails if a first, barely measured rate is shown (the jumpy first value), or the time is off the bytes over the
    /// rate.
    #[test]
    fn an_even_flow_gives_the_bytes_left_over_the_rate_after_the_warm_up() {
        let mut r = Run::new(300 * MB);
        r.poll();
        let early: Vec<_> = (0..4).flat_map(|_| r.pass(1.0, Duration::from_millis(250), MB)).collect();
        assert!(early.iter().all(Option::is_none), "nothing during the warm-up: {early:?}");
        let later: Vec<_> = (0..10).flat_map(|_| r.pass(1.0, Duration::from_millis(250), MB)).collect();
        let last = later.last().unwrap().expect("a time after the warm-up");
        assert!((280..=290).contains(&last), "286 MB left at 1 MB/s: {last}");
    }

    /// Chunks of 4 MB every 40 s (100 kB/s, as a slow link brings them): from the third chunk on (the gap after the
    /// first is longer than a stall: the third tells it is the usual one), the time left counts down between chunks
    /// and stays within a few seconds of the truth throughout, never hidden as a stall between chunks. Fails if the
    /// rate is averaged per second (it swings sevenfold between chunks), or the gap between chunks reads as a stall.
    #[test]
    fn lumpy_chunks_on_a_slow_link_give_a_steady_count_down() {
        let mut r = Run::new(400 * MB);
        r.poll();
        for _ in 0..3 {
            r.pass(40.0, Duration::from_secs(1), 4 * MB);
        }
        let mut seen = Vec::new();
        for _ in 0..9 {
            let polls = r.pass(40.0, Duration::from_secs(1), 4 * MB);
            seen.extend(polls.iter().map(|e| e.expect("shown between chunks too")));
        }
        for pair in seen.windows(2) {
            let step = pair[0].abs_diff(pair[1]);
            assert!(step <= 2, "a steady count down, a second per second: {pair:?}");
        }
        let last = *seen.last().unwrap();
        assert!((3_515..=3_525).contains(&last), "352 MB left at 100 kB/s: {last}");
    }

    /// One burst at ten times the rate moves the time left by little: an average over the window, not the last
    /// second. Fails if one fast second sets the rate.
    #[test]
    fn one_fast_second_barely_moves_it() {
        let mut r = Run::new(600 * MB);
        r.poll();
        for _ in 0..30 {
            r.pass(1.0, Duration::from_millis(500), MB);
        }
        let before = r.pass(1.0, Duration::from_millis(500), MB).last().unwrap().unwrap();
        let after = r.pass(1.0, Duration::from_millis(500), 10 * MB).last().unwrap().unwrap();
        let change = before.abs_diff(after) as f64 / before as f64;
        assert!(change < 0.4, "a burst moves it by under 40 % ({before} s then {after} s)");
    }

    /// Nothing arriving for longer than a stall hides the time; arrivals again show it only after a new warm-up, and
    /// from the new rate (the stall is not averaged in). Fails if a stalled folder keeps counting down to nothing, or a
    /// stale rate (or one dragged down by the stall) comes back with the flow.
    #[test]
    fn a_stall_hides_it_until_the_flow_is_measured_again() {
        let mut r = Run::new(500 * MB);
        r.poll();
        for _ in 0..10 {
            r.pass(1.0, Duration::from_secs(1), MB);
        }
        assert!(r.poll().is_some());
        let stalled = r.pass(STALL.as_secs_f64() + 2.0, Duration::from_secs(1), 0);
        assert_eq!(stalled.last(), Some(&None), "hidden while nothing comes: {stalled:?}");
        // Faster now: 2 MB a second.
        let back: Vec<_> = (0..4).flat_map(|_| r.pass(1.0, Duration::from_secs(1), 2 * MB)).collect();
        assert!(back.iter().all(Option::is_none), "a new warm-up: {back:?}");
        let later = (0..5).flat_map(|_| r.pass(1.0, Duration::from_secs(1), 2 * MB)).last().unwrap().unwrap();
        assert!((230..=240).contains(&later), "472 MB left at the new 2 MB/s: {later}");
    }

    /// One arrival, then nothing for a long while, then an even flow: the time comes from the flow alone, and none is
    /// shown from the stalled measure meanwhile. Fails if the stall after a single arrival stays in the measure (the
    /// rate dragged down by it, the time left many times too long until the arrival leaves the window).
    #[test]
    fn a_stall_after_a_single_arrival_is_not_measured() {
        let mut r = Run::new(500 * MB);
        r.poll();
        r.pass(1.0, Duration::from_secs(1), MB);
        let stalled = r.pass(60.0, Duration::from_secs(1), 0);
        assert_eq!(stalled.last(), Some(&None), "hidden while nothing comes: {stalled:?}");
        let back: Vec<_> = (0..10).flat_map(|_| r.pass(1.0, Duration::from_secs(1), MB)).collect();
        assert!(back.iter().flatten().all(|&s| s <= 600), "never from the stalled rate: {back:?}");
        let later = back.last().unwrap().expect("a time once the flow is measured");
        assert!((480..=495).contains(&later), "489 MB left at 1 MB/s: {later}");
    }

    /// A folder done or no longer syncing (up to date, paused, waiting) shows no time, and its next sync starts from
    /// nothing. Fails if the time of an earlier sync lingers.
    #[test]
    fn done_or_not_syncing_hides_it_and_the_next_sync_starts_afresh() {
        let mut r = Run::new(100 * MB);
        r.poll();
        for _ in 0..10 {
            r.pass(1.0, Duration::from_secs(1), MB);
        }
        assert!(r.poll().is_some());
        let t = r.t0 + r.at;
        assert_eq!(r.eta.sample(t, r.fetched, 0, true), None, "nothing left");
        assert_eq!(r.eta.sample(t, r.fetched, r.remaining, false), None, "up to date");
        assert_eq!(r.eta.sample(t + Duration::from_secs(1), r.fetched + MB, r.remaining, true), None, "a new sync warms up again");
    }

    /// Under ten seconds left is not shown, nor more than a month (a trickle). Fails if a sync about to end flashes
    /// "1 min left", or one at a few bytes a second says "3 years".
    #[test]
    fn very_short_and_very_long_times_are_hidden() {
        let mut short = Run::new(20 * MB);
        short.poll();
        let all: Vec<_> = (0..12).flat_map(|_| short.pass(1.0, Duration::from_secs(1), MB)).collect();
        assert!(all.iter().any(Option::is_some), "shown while ten seconds or more are left: {all:?}");
        assert_eq!(*all.last().unwrap(), None, "8 MB at 1 MB/s: hidden");

        let mut trickle = Run::new(1_000_000 * MB);
        trickle.poll();
        let all: Vec<_> = (0..10).flat_map(|_| trickle.pass(1.0, Duration::from_secs(1), 100)).collect();
        assert!(all.iter().all(Option::is_none), "a terabyte at 100 bytes a second: {all:?}");
    }

    /// A host that looks only now and then (once a minute) sees the time from its second look with bytes on, and
    /// keeps seeing it: a gap between looks is not a stall when bytes came meanwhile. Fails if rare polling reads as a
    /// stall and hides the time.
    #[test]
    fn rare_polling_is_not_a_stall() {
        let mut r = Run::new(500 * MB);
        r.poll();
        let looks: Vec<_> = (0..4).flat_map(|_| r.pass(60.0, Duration::from_secs(60), 60 * MB)).collect();
        assert!(looks[1..].iter().all(Option::is_some), "shown from the second look: {looks:?}");
        assert!((255..=265).contains(&looks[3].unwrap()), "260 MB left at 1 MB/s: {looks:?}");
    }

    /// A host polling ten times a second with bytes at every look keeps the kept arrivals bounded and gets the same
    /// answer as one polling once a second. Fails if fast polling grows the list without end or changes the estimate.
    #[test]
    fn fast_polling_changes_nothing() {
        let (mut fast, mut slow) = (Run::new(300 * MB), Run::new(300 * MB));
        fast.poll();
        slow.poll();
        for _ in 0..120 {
            for _ in 0..10 {
                fast.pass(0.1, Duration::from_millis(100), MB / 10);
            }
            slow.pass(1.0, Duration::from_secs(1), MB);
        }
        assert!(fast.eta.seen.len() <= 40, "kept: {}", fast.eta.seen.len());
        let (a, b) = (fast.poll().unwrap(), slow.poll().unwrap());
        assert!(a.abs_diff(b) <= 2, "{a} s and {b} s");
    }
}
