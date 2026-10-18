//! `Retry-After` parsing (RFC 9110 section 10.2.3): delta-seconds or an HTTP
//! date. Pure so it can be fuzzed; the transport supplies the current time.

use std::time::Duration;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) to Unix seconds.
pub fn parse_http_date(s: &str) -> Option<u64> {
    let s = s.trim();
    let (_, rest) = s.split_once(", ")?;
    let mut it = rest.split_ascii_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(mon))? as i64 + 1;
    let year: i64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    if !it.next()?.eq_ignore_ascii_case("GMT") || it.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let (h, mi, se): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if !(1..=31).contains(&day) || !(1970..=9999).contains(&year) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// How long to wait according to a `Retry-After` value, given the current Unix
/// time. Dates in the past mean "now" (zero). Unparseable values give `None`.
pub fn parse_retry_after(value: &str, now_unix: u64) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(v)?;
    Some(Duration::from_secs(at.saturating_sub(now_unix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_seconds() {
        assert_eq!(parse_retry_after("120", 0), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 0 ", 0), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("-5", 0), None);
        assert_eq!(parse_retry_after("1.5", 0), None);
        assert_eq!(parse_retry_after("", 0), None);
        assert_eq!(parse_retry_after("99999999999999999999999", 0), None);
    }

    #[test]
    fn http_dates() {
        // RFC 9110 example
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Fri, 29 Feb 2008 12:00:00 GMT"),
            Some(1_204_286_400)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:57 GMT", 784_111_777),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_900),
            Some(Duration::ZERO)
        );
        for bad in [
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 25:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "garbage",
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            assert_eq!(parse_retry_after(bad, 0), None, "{bad}");
        }
    }
}
