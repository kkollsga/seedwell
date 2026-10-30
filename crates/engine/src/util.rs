//! Small shared helpers: clocks, a tiny seeded RNG and hex formatting.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix time in milliseconds. This is the clock handed to `trs-tracker`: it keeps advancing
/// while the Mac sleeps and is stable across launches, which persisted tracker timings need.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Unix time in whole seconds.
pub fn unix_secs() -> u64 {
    unix_ms() / 1000
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    trs_metainfo::hex_encode(bytes)
}

/// xorshift64* generator. Not cryptographic: it only shuffles peers and breaks ties.
// size: 8 bytes
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Generator with a fixed seed (tests).
    pub fn seeded(seed: u64) -> Self {
        Rng(seed | 1)
    }

    /// Generator seeded from OS entropy (falls back to the clock if that fails).
    pub fn from_entropy() -> Self {
        let mut b = [0u8; 8];
        if getrandom::fill(&mut b).is_err() {
            b = unix_ms().to_le_bytes();
        }
        Rng::seeded(u64::from_le_bytes(b))
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `0..n` (`n == 0` yields 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Uniform `f64` in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Fisher-Yates shuffle.
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            v.swap(i, j);
        }
    }
}

/// A 32-bit tracker `key` from OS entropy.
pub fn random_key() -> u32 {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        b = (unix_ms() as u32).to_le_bytes();
    }
    u32::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let mut a = Rng::seeded(7);
        let mut b = Rng::seeded(7);
        for _ in 0..100 {
            let x = a.below(10);
            assert_eq!(x, b.below(10));
            assert!(x < 10);
        }
        assert_eq!(a.below(0), 0);
        let u = a.unit();
        assert!((0.0..1.0).contains(&u));
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = Rng::seeded(3);
        let mut v: Vec<u32> = (0..50).collect();
        r.shuffle(&mut v);
        let mut s = v.clone();
        s.sort_unstable();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
        assert_ne!(v, s);
    }
}

/// Offset of the local time zone from UTC, in seconds, valid at Unix time `unix`, cached per half
/// hour. The app can bypass this entirely by installing a [`crate::config::ClockFn`] (Swift knows
/// the zone exactly). Falls back to UTC.
pub fn local_utc_offset(unix: i64) -> i32 {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(i64, i32)>> = Mutex::new(None);
    let window = unix.div_euclid(1800);
    {
        let g = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((w, off)) = *g
            && w == window {
                return off;
            }
    }
    let off = trs_platform::proc::utc_offset(unix);
    *CACHE.lock().unwrap_or_else(|p| p.into_inner()) = Some((window, off));
    off
}

/// `+0200` / `-0530` to seconds.
pub fn parse_utc_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i32 = rest[..2].parse().ok()?;
    let m: i32 = rest[2..].parse().ok()?;
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tz_tests {
    use super::*;

    #[test]
    fn utc_offsets_parse() {
        assert_eq!(parse_utc_offset("+0200"), Some(7200));
        assert_eq!(parse_utc_offset("-0530"), Some(-19800));
        assert_eq!(parse_utc_offset("0200"), None);
        assert_eq!(parse_utc_offset("+02:00"), None);
        assert_eq!(parse_utc_offset("+9900"), None);
    }

    #[test]
    fn local_offset_is_plausible() {
        let off = local_utc_offset(1_790_000_000);
        assert!((-14 * 3600..=14 * 3600).contains(&off));
    }
}

/// `1.4 GB`, `812 MB`, `3 KB`: decimal units, one decimal above 10 and none below.
pub(crate) fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else if v < 10.0 { format!("{v:.1} {}", U[i]) } else { format!("{v:.0} {}", U[i]) }
}
