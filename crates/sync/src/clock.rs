//! Time, injected: the simulator runs on a virtual clock, the driver on the system clock.

/// The clock the core reads.
pub trait Clock: Send + Sync {
    /// Now, Unix nanoseconds.
    fn now_ns(&self) -> i64;
}

/// `2026-10-25 14.32z`: the UTC minute, for conflict copy names (no `:`, which Finder shows as `/`). UTC, so
/// that every device names the same losing version the same way, whatever its time zone
/// (docs/CROSS-PLATFORM.md §3.2).
pub fn utc_stamp(unix_ns: i64) -> String {
    minute(unix_ns.div_euclid(1_000_000_000)) + "z"
}

/// `2026-10-25 15.32`: a minute for people, `offset_s` east of UTC (the app shows conflict copies in local
/// time).
pub fn local_stamp(unix_s: i64, offset_s: i32) -> String {
    minute(unix_s + i64::from(offset_s))
}

fn minute(secs: i64) -> String {
    let (y, mo, d, h, mi, _) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}.{mi:02}")
}

/// The time a [`utc_stamp`] names, Unix seconds (`None` when it is not one).
pub fn parse_utc_stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 17 || b[16] != b'z' || b[4] != b'-' || b[7] != b'-' || b[10] != b' ' || b[13] != b'.' {
        return None;
    }
    let compact = format!("{}{}{}-{}{}00", &s[0..4], &s[5..7], &s[8..10], &s[11..13], &s[14..16]);
    parse_compact_stamp(&compact).map(|ns| ns / 1_000_000_000)
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

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        // 2026-10-25 14:32:05 UTC
        let t = 1_792_938_725 * 1_000_000_000;
        assert_eq!(compact_stamp(t), "20261025-143205");
        assert_eq!(utc_stamp(t), "2026-10-25 14.32z");
        assert_eq!(parse_utc_stamp("2026-10-25 14.32z"), Some(1_792_938_720));
        assert_eq!(local_stamp(1_792_938_725, 3600), "2026-10-25 15.32");
        assert_eq!(local_stamp(1_792_938_725, -16 * 3600), "2026-10-24 22.32");
        for bad in ["2026-10-25 14.32", "2026-10-25 14.32Z", "2026-13-25 14.32z", "2026-10-25 14:32z"] {
            assert_eq!(parse_utc_stamp(bad), None, "{bad}");
        }
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
