//! Rate limiting: a deterministic token bucket (integer milli-tokens, caller-supplied time) and a
//! small fixed-size per-IP limiter for responses.

/// Token bucket. One token is 1000 milli-tokens; `rate` tokens/s refills `rate` milli-tokens per ms.
#[derive(Clone, Copy, Debug)]
pub struct TokenBucket {
    cap_milli: u64,
    rate: u64,
    tokens_milli: u64,
    last_ms: u64,
}

impl TokenBucket {
    /// Bucket holding `burst` tokens, refilled at `rate_per_s`, starting full at `now_ms`.
    pub fn new(burst: u32, rate_per_s: u32, now_ms: u64) -> Self {
        let cap = u64::from(burst) * 1000;
        Self {
            cap_milli: cap,
            rate: u64::from(rate_per_s),
            tokens_milli: cap,
            last_ms: now_ms,
        }
    }

    fn refill(&mut self, now_ms: u64) {
        let dt = now_ms.saturating_sub(self.last_ms);
        self.tokens_milli = (self.tokens_milli + dt * self.rate).min(self.cap_milli);
        self.last_ms = self.last_ms.max(now_ms);
    }

    /// Takes `n` tokens if available.
    pub fn try_take(&mut self, now_ms: u64, n: u32) -> bool {
        self.refill(now_ms);
        let need = u64::from(n) * 1000;
        if self.tokens_milli >= need {
            self.tokens_milli -= need;
            true
        } else {
            false
        }
    }

    /// Whole tokens currently available.
    pub fn available(&mut self, now_ms: u64) -> u32 {
        self.refill(now_ms);
        (self.tokens_milli / 1000) as u32
    }

    /// Milliseconds until `n` tokens are available (0 if now).
    pub fn wait_ms(&mut self, now_ms: u64, n: u32) -> u64 {
        self.refill(now_ms);
        let need = u64::from(n) * 1000;
        if self.tokens_milli >= need || self.rate == 0 {
            return 0;
        }
        (need - self.tokens_milli).div_ceil(self.rate)
    }
}

const SLOTS: usize = 512;

#[derive(Clone, Copy, Default)]
struct Slot {
    ip: u32,
    tokens_milli: u32,
    last_ms: u32,
}

/// Direct-mapped per-source-IP limiter (`512 * 12` = 6 KiB). Collisions evict the
/// previous occupant, which then simply gets a fresh burst: this is damping, not security.
pub struct IpLimiter {
    slots: Box<[Slot; SLOTS]>,
    salt: u32,
    cap_milli: u32,
    rate: u32,
}

impl IpLimiter {
    /// `burst` tokens per IP, refilled at `rate_per_s`; `salt` randomises slot placement.
    pub fn new(burst: u32, rate_per_s: u32, salt: u32) -> Self {
        Self {
            slots: Box::new([Slot::default(); SLOTS]),
            salt,
            cap_milli: burst.saturating_mul(1000),
            rate: rate_per_s,
        }
    }

    /// Bytes of limiter state.
    pub const fn size_bytes() -> usize {
        SLOTS * size_of::<Slot>()
    }

    /// Charges one token to `ip`; false if it is over its limit.
    pub fn allow(&mut self, ip: u32, now_ms: u64) -> bool {
        let h = (ip ^ self.salt).wrapping_mul(0x9E37_79B1);
        let idx = (h >> 16) as usize % SLOTS;
        let now = now_ms as u32; // wraps after 49 days; wrapping_sub keeps deltas right
        let cap = self.cap_milli;
        let rate = self.rate;
        let s = &mut self.slots[idx];
        if s.ip != ip {
            *s = Slot {
                ip,
                tokens_milli: cap,
                last_ms: now,
            };
        } else {
            let dt = now.wrapping_sub(s.last_ms);
            let dt = if dt > u32::MAX / 2 { 0 } else { dt };
            s.tokens_milli = (u64::from(s.tokens_milli) + u64::from(dt) * u64::from(rate))
                .min(u64::from(cap)) as u32;
            s.last_ms = now;
        }
        if s.tokens_milli >= 1000 {
            s.tokens_milli -= 1000;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_burst_then_sustained() {
        let mut b = TokenBucket::new(50, 10, 0);
        let mut got = 0;
        for _ in 0..100 {
            if b.try_take(0, 1) {
                got += 1;
            }
        }
        assert_eq!(got, 50);
        // 10/s sustained: after 1 s, exactly 10 more.
        let mut got = 0;
        for _ in 0..100 {
            if b.try_take(1000, 1) {
                got += 1;
            }
        }
        assert_eq!(got, 10);
        assert_eq!(b.wait_ms(1000, 1), 100);
        assert!(b.try_take(1100, 1));
        // Refill never exceeds capacity.
        assert_eq!(b.available(1_000_000), 50);
    }

    #[test]
    fn bucket_time_going_backwards_is_harmless() {
        let mut b = TokenBucket::new(5, 1, 1000);
        assert!(b.try_take(500, 5));
        assert!(!b.try_take(500, 1));
    }

    #[test]
    fn ip_limiter_per_ip() {
        let mut l = IpLimiter::new(5, 2, 0x1234);
        let a = 0x0102_0304u32;
        let b = 0x0a0b_0c0du32;
        let ok = (0..20).filter(|_| l.allow(a, 1000)).count();
        assert_eq!(ok, 5);
        assert!(l.allow(b, 1000), "other IPs unaffected");
        // 2 tokens/s: after 1s, two more for `a`.
        let ok = (0..20).filter(|_| l.allow(a, 2000)).count();
        assert_eq!(ok, 2);
        assert_eq!(IpLimiter::size_bytes(), 512 * 12);
    }
}
