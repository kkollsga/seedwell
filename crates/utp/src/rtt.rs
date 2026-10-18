//! RTT estimation and retransmission timeout (RFC 6298 arithmetic, BEP 29 / libutp constants).
//!
//! Karn's rule is the caller's job: only feed samples from packets that were never retransmitted.

/// Smoothed RTT state. All values are microseconds.
// size: 16 bytes
#[derive(Clone, Copy, Debug)]
pub struct Rtt {
    srtt: u32,
    rttvar: u32,
    rto: u32,
    has_sample: bool,
}

impl Rtt {
    /// Fresh estimator that will time out after `initial_rto`.
    pub fn new(initial_rto: u32) -> Self {
        Rtt { srtt: 0, rttvar: 0, rto: initial_rto, has_sample: false }
    }

    /// Smoothed RTT, or 0 before the first sample.
    pub fn srtt(&self) -> u32 {
        self.srtt
    }

    /// RTT variance estimate.
    pub fn rttvar(&self) -> u32 {
        self.rttvar
    }

    /// Current retransmission timeout.
    pub fn rto(&self) -> u32 {
        self.rto
    }

    /// True once at least one sample was absorbed.
    pub fn has_sample(&self) -> bool {
        self.has_sample
    }

    /// Absorb one RTT measurement and recompute the RTO, clamped to `[min_rto, max_rto]`.
    pub fn sample(&mut self, rtt: u32, min_rto: u32, max_rto: u32) {
        if !self.has_sample {
            self.srtt = rtt;
            self.rttvar = rtt / 2;
            self.has_sample = true;
        } else {
            let err = self.srtt.abs_diff(rtt) as u64;
            self.rttvar = ((3 * self.rttvar as u64 + err) / 4) as u32;
            self.srtt = ((7 * self.srtt as u64 + rtt as u64) / 8) as u32;
        }
        let rto = self.srtt as u64 + 4 * self.rttvar as u64;
        self.rto = rto.clamp(min_rto as u64, max_rto.max(min_rto) as u64) as u32;
    }

    /// Exponential backoff after a timeout.
    pub fn backoff(&mut self, max_rto: u32) {
        self.rto = self.rto.saturating_mul(2).min(max_rto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u32 = 500_000;
    const MAX: u32 = 60_000_000;

    #[test]
    fn first_sample_sets_srtt_and_var() {
        let mut r = Rtt::new(1_000_000);
        assert_eq!(r.rto(), 1_000_000);
        r.sample(800_000, MIN, MAX);
        assert_eq!(r.srtt(), 800_000);
        assert_eq!(r.rttvar(), 400_000);
        // srtt + 4*var = 2.4 s
        assert_eq!(r.rto(), 2_400_000);
    }

    #[test]
    fn converges_and_respects_floor() {
        let mut r = Rtt::new(1_000_000);
        for _ in 0..200 {
            r.sample(50_000, MIN, MAX);
        }
        assert!((r.srtt() as i64 - 50_000).abs() < 100);
        assert!(r.rttvar() < 1_000);
        // 50 ms RTT: the 500 ms floor wins.
        assert_eq!(r.rto(), MIN);
    }

    #[test]
    fn variance_grows_on_jitter() {
        let mut r = Rtt::new(1_000_000);
        for i in 0..100 {
            r.sample(if i % 2 == 0 { 100_000 } else { 900_000 }, MIN, MAX);
        }
        assert!(r.rttvar() > 150_000, "{}", r.rttvar());
        assert!(r.rto() > 1_000_000);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut r = Rtt::new(500_000);
        r.backoff(MAX);
        assert_eq!(r.rto(), 1_000_000);
        for _ in 0..40 {
            r.backoff(MAX);
        }
        assert_eq!(r.rto(), MAX);
    }
}
