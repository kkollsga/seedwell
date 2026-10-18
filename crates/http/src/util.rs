//! Small shared helpers (no extra dependencies).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SEED: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// splitmix64 over a process-wide counter mixed with the clock. Not
/// cryptographic: used for backoff jitter and collision tie-breaks only.
pub fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut z = SEED
        .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        .wrapping_add(nanos);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 128 random bits as hex (default task ids).
pub fn random_id() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// Exponential backoff with jitter: `base * 2^(attempt-1)` capped at `max`,
/// then scaled into `[50 %, 100 %]` so synchronized clients spread out.
pub fn backoff(base: Duration, max: Duration, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(20);
    let raw = base.saturating_mul(1u32 << exp).min(max);
    let frac = 0.5 + (random_u64() % 1000) as f64 / 2000.0;
    raw.mul_f64(frac)
}

/// `Retry-After`: delta-seconds or an HTTP date.
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = httpdate::parse_http_date(v).ok()?;
    Some(when.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let b = Duration::from_millis(100);
        let m = Duration::from_secs(2);
        for _ in 0..50 {
            let d1 = backoff(b, m, 1);
            assert!(d1 >= b / 2 && d1 <= b);
            let d9 = backoff(b, m, 9);
            assert!(d9 >= m / 2 && d9 <= m);
            let _ = backoff(b, m, u32::MAX);
        }
    }

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), Some(Duration::ZERO));
        assert!(parse_retry_after("soon").is_none());
        assert!(parse_retry_after("-5").is_none());
    }
}
