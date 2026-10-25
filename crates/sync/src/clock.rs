//! Time, injected: the simulator runs on a virtual clock, the driver on the system clock.

/// The clock the core reads.
pub trait Clock: Send + Sync {
    /// Now, Unix nanoseconds.
    fn now_ns(&self) -> i64;
    /// Seconds to add to UTC for the local time at `unix_ns` (daylight saving included). Only used for
    /// names people read (conflict copies).
    fn local_offset_s(&self, unix_ns: i64) -> i32;
}

/// `2026-10-25 14.32`: the local minute, for conflict copy names (no `:`, which Finder shows as `/`).
pub fn readable_stamp(clock: &dyn Clock, unix_ns: i64) -> String {
    let local = unix_ns.div_euclid(1_000_000_000) + i64::from(clock.local_offset_s(unix_ns));
    let (y, mo, d, h, mi, _) = civil(local);
    format!("{y:04}-{mo:02}-{d:02} {h:02}.{mi:02}")
}

/// `20261025-143205`: UTC to the second, for archived versions (sorts by time).
pub fn compact_stamp(unix_ns: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix_ns.div_euclid(1_000_000_000));
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// The time a [`compact_stamp`] names, Unix ns (`None` when it is not one).
pub fn parse_compact_stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 15 || b[8] != b'-' || !b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit()) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
    let (h, mi, sec) = (num(9..11)?, num(11..13)?, num(13..15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's `days_from_civil`.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) * 1_000_000_000)
}

/// Unix seconds → (year, month, day, hour, minute, second), proleptic Gregorian (Howard Hinnant's
/// `civil_from_days`).
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

/// The system clock with a fixed UTC offset (the driver refreshes it; the simulator uses 0).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock {
    /// Seconds east of UTC.
    pub offset_s: i32,
}

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
    }

    fn local_offset_s(&self, _unix_ns: i64) -> i32 {
        self.offset_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(i32);
    impl Clock for Fixed {
        fn now_ns(&self) -> i64 {
            0
        }
        fn local_offset_s(&self, _: i64) -> i32 {
            self.0
        }
    }

    #[test]
    fn stamps() {
        // 2026-10-25 14:32:05 UTC
        let t = 1_792_938_725 * 1_000_000_000;
        assert_eq!(compact_stamp(t), "20261025-143205");
        assert_eq!(readable_stamp(&Fixed(0), t), "2026-10-25 14.32");
        assert_eq!(readable_stamp(&Fixed(3600), t), "2026-10-25 15.32");
        assert_eq!(compact_stamp(0), "19700101-000000");
        assert_eq!(compact_stamp(-1_000_000_000), "19691231-235959");
        assert_eq!(compact_stamp(951_782_400 * 1_000_000_000), "20000229-000000", "leap day");
        for t in [t, 0, 951_782_400 * 1_000_000_000, -1_000_000_000] {
            assert_eq!(parse_compact_stamp(&compact_stamp(t)), Some(t));
        }
        assert_eq!(parse_compact_stamp("2026102-143205x"), None);
        assert_eq!(parse_compact_stamp("20261325-143205"), None);
    }
}
