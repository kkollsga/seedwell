//! LEDBAT congestion control (RFC 6817), tuned like libutp: 100 ms target, windowed minimum base
//! delay, slow start that exits early when queuing delay approaches the target.
//!
//! The controller is pure arithmetic over delay samples. A *delay sample* is the
//! `timestamp_difference` the peer echoes back: `their_receive_time - our_send_time` in wrapping
//! microseconds. Absolute values include an unknown clock offset, so only differences matter:
//! `our_delay = recent_min(samples) - min(samples over the last ~10 minutes)`.

use crate::seq::lt32;

const CUR_SAMPLES: usize = 3;
const BASE_BUCKETS: usize = 10;
const BUCKET_US: u64 = 60_000_000;

/// Base-delay history and current-delay filter.
// size: 64 bytes
#[derive(Clone, Debug)]
pub struct DelayFilter {
    cur: [u32; CUR_SAMPLES],
    base: [u32; BASE_BUCKETS],
    bucket_start: u64,
    cur_idx: u8,
    base_idx: u8,
    init: bool,
}

impl Default for DelayFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayFilter {
    /// Empty filter; `our_delay` is 0 until the first sample.
    pub const fn new() -> Self {
        DelayFilter {
            cur: [0; CUR_SAMPLES],
            base: [0; BASE_BUCKETS],
            bucket_start: 0,
            cur_idx: 0,
            base_idx: 0,
            init: false,
        }
    }

    /// True once at least one sample arrived.
    pub fn has_samples(&self) -> bool {
        self.init
    }

    /// Absorb a delay sample taken at time `now` (microseconds).
    pub fn add(&mut self, now: u64, sample: u32) {
        if !self.init {
            self.init = true;
            self.cur = [sample; CUR_SAMPLES];
            self.base = [sample; BASE_BUCKETS];
            self.bucket_start = now;
            return;
        }
        if now.saturating_sub(self.bucket_start) >= BUCKET_US {
            self.base_idx = (self.base_idx + 1) % BASE_BUCKETS as u8;
            self.base[self.base_idx as usize] = sample;
            self.bucket_start = now;
        } else {
            let b = &mut self.base[self.base_idx as usize];
            if lt32(sample, *b) {
                *b = sample;
            }
        }
        self.cur[self.cur_idx as usize] = sample;
        self.cur_idx = (self.cur_idx + 1) % CUR_SAMPLES as u8;
    }

    /// Minimum over the base history (wrapping-aware).
    pub fn base(&self) -> u32 {
        self.base.iter().copied().reduce(|a, b| if lt32(b, a) { b } else { a }).unwrap_or(0)
    }

    /// Minimum over the most recent samples: robust against a single delayed packet.
    pub fn current(&self) -> u32 {
        self.cur.iter().copied().reduce(|a, b| if lt32(b, a) { b } else { a }).unwrap_or(0)
    }

    /// Estimated queuing delay on our outbound path, microseconds.
    pub fn our_delay(&self) -> u32 {
        if !self.init {
            return 0;
        }
        let d = self.current().wrapping_sub(self.base());
        if (d as i32) < 0 { 0 } else { d }
    }
}

/// Inputs to one window update.
#[derive(Clone, Copy, Debug)]
pub struct AckInfo {
    /// Bytes newly acknowledged by this packet (cumulative + selective).
    pub acked_bytes: u32,
    /// Bytes in flight just before this ack was applied.
    pub flight_before: u32,
    /// Maximum payload per packet.
    pub mss: u32,
    /// Target queuing delay, microseconds.
    pub target: u32,
    /// Hard ceiling on the window.
    pub max_window: u32,
}

/// LEDBAT window state.
// size: 12 bytes (+ the 64-byte [`DelayFilter`] kept next to it)
#[derive(Clone, Debug)]
pub struct Ledbat {
    window: u32,
    ssthresh: u32,
    slow_start: bool,
}

/// Max window growth per RTT at zero queuing delay (libutp `MAX_CWND_INCREASE_BYTES_PER_RTT`).
const MAX_INCREASE_PER_RTT: f64 = 3000.0;

impl Ledbat {
    /// Start with `initial` bytes of window in slow start.
    pub fn new(initial: u32) -> Self {
        Ledbat { window: initial, ssthresh: u32::MAX, slow_start: true }
    }

    /// Raise the window to the two-packet floor for a packet size that just grew.
    pub fn ensure_floor(&mut self, mss: u32) {
        self.window = self.window.max(Self::floor(mss));
    }

    /// Current congestion window in bytes.
    pub fn window(&self) -> u32 {
        self.window
    }

    /// True while still in the exponential start-up phase.
    pub fn in_slow_start(&self) -> bool {
        self.slow_start
    }

    /// Smallest window we allow: two packets, so loss signals keep flowing.
    fn floor(mss: u32) -> u32 {
        mss.saturating_mul(2)
    }

    /// Apply one ack with queuing delay `our_delay`.
    pub fn on_ack(&mut self, a: AckInfo, our_delay: u32) {
        if a.acked_bytes == 0 {
            return;
        }
        // Never let the window run away from what the sender actually uses (RFC 6817 3.4.2:
        // ALLOWED_INCREASE): an application- or receiver-limited flow must not bank window.
        let usable = a.flight_before.saturating_add(a.mss.saturating_mul(2));
        if self.slow_start {
            let leave = our_delay > a.target / 10 * 9 || self.window >= self.ssthresh;
            if leave {
                self.slow_start = false;
                self.ssthresh = self.window;
            } else {
                let grown = self.window.saturating_add(a.acked_bytes);
                self.window = grown.min(self.window.max(usable)).min(a.max_window);
                return;
            }
        }
        let target = a.target.max(1) as f64;
        let off_target = target - our_delay as f64;
        let delay_factor = off_target / target;
        let w = self.window as f64;
        let window_factor = (a.acked_bytes as f64).min(w) / (a.acked_bytes as f64).max(w).max(1.0);
        let inc = MAX_INCREASE_PER_RTT.max(2.0 * a.mss as f64);
        let mut next = w + inc * delay_factor * window_factor;
        if next > w {
            next = next.min(w.max(usable as f64));
        }
        self.window = (next.max(Self::floor(a.mss) as f64) as u32).min(a.max_window);
    }

    /// Fast-retransmit style loss: halve (at most once per RTT; caller enforces).
    pub fn on_loss(&mut self, mss: u32) {
        self.window = (self.window / 2).max(Self::floor(mss));
        self.ssthresh = self.window;
        self.slow_start = false;
    }

    /// Retransmission timeout: collapse to one packet and slow-start back to half.
    pub fn on_timeout(&mut self, mss: u32) {
        self.ssthresh = (self.window / 2).max(Self::floor(mss));
        self.window = mss;
        self.slow_start = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSS: u32 = 1400;
    const TARGET: u32 = 100_000;

    fn ack(flight: u32, acked: u32) -> AckInfo {
        AckInfo { acked_bytes: acked, flight_before: flight, mss: MSS, target: TARGET, max_window: 1 << 24 }
    }

    /// Run `rtts` round trips with the window fully used and a constant queuing delay.
    fn run(l: &mut Ledbat, delay: u32, rtts: usize) {
        for _ in 0..rtts {
            let w = l.window();
            let mut sent = 0;
            while sent < w {
                l.on_ack(ack(w, MSS), delay);
                sent += MSS;
            }
        }
    }

    #[test]
    fn filter_base_and_current() {
        let mut f = DelayFilter::new();
        assert_eq!(f.our_delay(), 0);
        // clock offset of 5 s; true queuing delay 0 then 40 ms
        for i in 0..10 {
            f.add(i * 1000, 5_000_000);
        }
        assert_eq!(f.our_delay(), 0);
        for i in 10..20 {
            f.add(i * 1000, 5_040_000);
        }
        assert_eq!(f.our_delay(), 40_000);
        assert_eq!(f.base(), 5_000_000);
    }

    #[test]
    fn filter_ignores_single_spike_but_tracks_trend() {
        let mut f = DelayFilter::new();
        for i in 0..10 {
            f.add(i * 1000, 1_000);
        }
        f.add(20_000, 900_000);
        assert_eq!(f.our_delay(), 0, "min-of-3 hides one spike");
        for i in 0..3 {
            f.add(30_000 + i, 900_000);
        }
        assert_eq!(f.our_delay(), 899_000);
    }

    #[test]
    fn filter_handles_wraparound_and_rebase() {
        let mut f = DelayFilter::new();
        f.add(0, u32::MAX - 10);
        f.add(1, 5); // wrapped past zero: only 16 us larger
        f.add(2, 5);
        f.add(3, 5);
        assert_eq!(f.our_delay(), 16);
        // route change: base rises permanently; after >10 minutes the old minimum ages out
        let mut t = 0u64;
        while t < 700_000_000 {
            t += 1_000_000;
            f.add(t, 500_000);
        }
        assert_eq!(f.our_delay(), 0);
    }

    #[test]
    fn slow_start_doubles_then_exits_near_target() {
        let mut l = Ledbat::new(3 * MSS);
        let w0 = l.window();
        run(&mut l, 0, 1);
        assert!(l.window() >= 2 * w0 - MSS, "{}", l.window());
        assert!(l.in_slow_start());
        l.on_ack(ack(l.window(), MSS), 95_000);
        assert!(!l.in_slow_start());
    }

    #[test]
    fn grows_below_target_shrinks_above() {
        let mut l = Ledbat::new(20 * MSS);
        l.on_loss(MSS); // leave slow start
        let w = l.window();
        run(&mut l, 20_000, 10);
        assert!(l.window() > w, "grows with spare delay budget");
        let w = l.window();
        run(&mut l, 180_000, 10);
        assert!(l.window() < w, "backs off above target");
    }

    #[test]
    fn equilibrium_at_target() {
        let mut l = Ledbat::new(40 * MSS);
        l.on_loss(MSS);
        let w = l.window();
        run(&mut l, TARGET, 20);
        assert_eq!(l.window(), w, "zero off-target means zero gain");
    }

    #[test]
    fn growth_per_rtt_bounded_by_3000_bytes() {
        let mut l = Ledbat::new(100 * MSS);
        l.on_loss(MSS);
        let w = l.window();
        run(&mut l, 0, 1);
        assert!(l.window() - w <= 3100, "grew {}", l.window() - w);
        assert!(l.window() - w >= 2500);
    }

    #[test]
    fn app_limited_flow_does_not_bank_window() {
        let mut l = Ledbat::new(100 * MSS);
        l.on_loss(MSS);
        let w = l.window();
        for _ in 0..1000 {
            l.on_ack(ack(MSS, MSS), 0); // only one packet ever in flight
        }
        assert!(l.window() <= w, "{} > {}", l.window(), w);
    }

    #[test]
    fn loss_halves_timeout_collapses() {
        let mut l = Ledbat::new(40 * MSS);
        l.on_loss(MSS);
        assert_eq!(l.window(), 20 * MSS);
        l.on_timeout(MSS);
        assert_eq!(l.window(), MSS);
        assert!(l.in_slow_start());
        for _ in 0..40 {
            l.on_loss(MSS);
        }
        assert_eq!(l.window(), 2 * MSS, "floor is two packets");
    }

    #[test]
    fn respects_ceiling() {
        let mut l = Ledbat::new(10 * MSS);
        let a = AckInfo { max_window: 12 * MSS, ..ack(1 << 20, MSS) };
        for _ in 0..100 {
            l.on_ack(a, 0);
        }
        assert_eq!(l.window(), 12 * MSS);
    }
}
