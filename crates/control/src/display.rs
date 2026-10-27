//! The app's wording for sizes, rates, times and counts, the same in every host app (docs/CROSS-PLATFORM.md §1).
//! Decimal units like Finder ("6.2 GB", "412 MB"), English, independent of the locale. The CLI keeps its terser
//! forms ([`crate::human`]): it writes for terminals and agents.
//!
//! The time of day ("22:29", "Mon 22:29") is not here: it follows the user's own clock style, which only the host
//! knows.

/// "6.2 GB", "412 MB", "999 bytes".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut k = 0;
    while value >= 1000.0 && k < UNITS.len() - 1 {
        value /= 1000.0;
        k += 1;
    }
    if k == 0 {
        return format!("{n} bytes");
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[k]) } else { format!("{value:.0} {}", UNITS[k]) }
}

/// "2.1 MB/s"; at least "1.0 KB/s" while anything flows, "0 KB/s" when nothing does.
pub fn rate(n: u64) -> String {
    if n == 0 { "0 KB/s".into() } else { bytes(n.max(1_000)) + "/s" }
}

/// A live rate in a slot that is always there: "↑ 2.1 MB/s", or a bare "–" while nothing flows (no arrow, no unit).
pub fn live_rate(n: u64, arrow: &str) -> String {
    if n == 0 { "–".into() } else { format!("{arrow} {}", rate(n)) }
}

/// "4.6 of 6.2 GB": both numbers in the unit of the total.
pub fn progress(done: u64, total: u64) -> String {
    let scale = match total {
        t if t >= 1_000_000_000_000 => 1e12,
        t if t >= 1_000_000_000 => 1e9,
        t if t >= 1_000_000 => 1e6,
        _ => 1e3,
    };
    let d = done as f64 / scale;
    let n = if d < 10.0 { format!("{d:.1}") } else { format!("{d:.0}") };
    format!("{n} of {}", bytes(total))
}

/// One unit, rounded: whole minutes first (rounded up, at least one, so "90 s left" reads "2 min"), then the
/// largest unit with halves rounded up ("23 hours", but 23.5 hours is "1 day"). `short`: "min", "hr".
fn span(secs: u64, short: bool) -> String {
    let m = secs.max(60).div_ceil(60);
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if m < 60 {
        return if short { format!("{m} min") } else { plural(m, "minute", "minutes") };
    }
    let h = (m + 30) / 60;
    if h < 24 {
        return if short { format!("{h} hr") } else { plural(h, "hour", "hours") };
    }
    plural((m + 720) / 1440, "day", "days")
}

/// "2 minutes remaining" (lists), "2 min left" (`short`, the menu); `None`: not known yet.
pub fn remaining(secs: Option<u64>, short: bool) -> String {
    match (secs, short) {
        (None, true) => "calculating…".into(),
        (None, false) => "Calculating time remaining…".into(),
        (Some(s), true) => format!("{} left", span(s, true)),
        (Some(s), false) if s < 60 => "Less than a minute remaining".into(),
        (Some(s), false) => format!("{} remaining", span(s, false)),
    }
}

/// "12 minutes ago" (lists), "12 min ago" (`short`), "just now", "Yesterday", "3 days ago". Unix seconds.
pub fn ago(unix: u64, now: u64, short: bool) -> String {
    let secs = now.saturating_sub(unix);
    match secs {
        s if s < 60 => "just now".into(),
        s if s >= 2 * 86_400 => format!("{} days ago", s / 86_400),
        s if s >= 86_400 => "Yesterday".into(),
        s => format!("{} ago", span(s, short)),
    }
}

/// A time as one number and unit: "12 minutes", "9 days", "2 years" (seed time).
pub fn duration(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s >= 365 * 86_400 => (s / (365 * 86_400), "year"),
        s if s >= 86_400 => (s / 86_400, "day"),
        s if s >= 3_600 => (s / 3_600, "hour"),
        s => ((s / 60).max(1), "minute"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// "2 h", "15 min", "3 days": how long until something happens.
pub fn until(secs: u64) -> String {
    if secs >= 2 * 86_400 {
        format!("{} days", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{} h", (secs + 1_799) / 3_600)
    } else {
        format!("{} min", secs.div_ceil(60).max(1))
    }
}

/// "in 28 min", or "due now" within half a minute: a Unix time ahead.
pub fn in_future(unix: u64, now: u64) -> String {
    let secs = unix as i64 - now as i64;
    if secs <= 30 { "due now".into() } else { format!("in {}", until(secs as u64)) }
}

/// "8 / 57 peers": connected over the peer addresses known (always both numbers).
pub fn peers(connected: u32, known: u32) -> String {
    format!("{connected} / {} peers", known.max(connected))
}

/// Where the known peers come from (the hover text of the peers figure).
pub const PEERS_HELP: &str = "Connected peers / peer addresses known. Known peers come from the tracker and incoming connections (private torrents; plus PEX when the tracker allows it); plus DHT and PEX for public ones.";

/// "1 torrent", "3 torrents".
pub fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// "0.27": a share ratio.
pub fn ratio(r: f64) -> String {
    format!("{r:.2}")
}

/// "1.0": a ratio goal.
pub fn goal(r: f64) -> String {
    format!("{r:.1}")
}

/// `text` cut in the middle with "…" when it has more than `limit` characters.
pub fn middle_truncated(text: &str, limit: usize) -> String {
    let n = text.chars().count();
    if n <= limit || limit <= 2 {
        return text.to_string();
    }
    let head = limit / 2;
    let tail = limit - 1 - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(n - tail));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_rates_read_like_finder() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(999), "999 bytes");
        assert_eq!(bytes(1_000), "1.0 KB");
        assert_eq!(bytes(6_200_000_000), "6.2 GB");
        assert_eq!(bytes(412_000_000), "412 MB");
        assert_eq!(bytes(9_990_000), "10.0 MB", "rounds within the unit, as %.1f does");
        assert_eq!(bytes(5_000_000_000_000_000), "5000 TB");
        assert_eq!(rate(0), "0 KB/s");
        assert_eq!(rate(10), "1.0 KB/s", "anything flowing shows");
        assert_eq!(rate(2_100_000), "2.1 MB/s");
        assert_eq!(live_rate(0, "↑"), "–");
        assert_eq!(live_rate(340_000, "↑"), "↑ 340 KB/s");
        assert_eq!(progress(4_600_000_000, 6_200_000_000), "4.6 of 6.2 GB");
        assert_eq!(progress(12_000_000, 412_000_000), "12 of 412 MB");
    }

    #[test]
    fn spans_round_like_the_mac_formatter() {
        // From DateComponentsFormatter (en_US, day/hour/minute, one unit), on the minute-rounded value.
        for (secs, full, short) in [
            (0, "1 minute", "1 min"),
            (61, "2 minutes", "2 min"),
            (3_540, "59 minutes", "59 min"),
            (3_541, "1 hour", "1 hr"),
            (5_340, "1 hour", "1 hr"),
            (5_341, "2 hours", "2 hr"),
            (41_399, "12 hours", "12 hr"),
            (84_540, "23 hours", "23 hr"),
            (84_600, "1 day", "1 day"),
            (129_599, "2 days", "2 days"),
            (215_999, "3 days", "3 days"),
            (907_200, "11 days", "11 days"),
            (31_536_000, "365 days", "365 days"),
        ] {
            assert_eq!((span(secs, false).as_str(), span(secs, true).as_str()), (full, short), "{secs}");
        }
        assert_eq!(remaining(Some(90), true), "2 min left");
        assert_eq!(remaining(Some(30), false), "Less than a minute remaining");
        assert_eq!(remaining(Some(7_200), false), "2 hours remaining");
        assert_eq!(remaining(None, false), "Calculating time remaining…");
        let now = 1_800_000_000;
        assert_eq!(ago(now - 10, now, false), "just now");
        assert_eq!(ago(now - 720, now, false), "12 minutes ago");
        assert_eq!(ago(now - 720, now, true), "12 min ago");
        assert_eq!(ago(now - 90_000, now, false), "Yesterday");
        assert_eq!(ago(now - 3 * 86_400, now, false), "3 days ago");
        assert_eq!(ago(now + 50, now, false), "just now", "a clock ahead is now");
    }

    #[test]
    fn durations_counts_and_names() {
        assert_eq!(duration(30), "1 minute");
        assert_eq!(duration(9 * 86_400), "9 days");
        assert_eq!(duration(2 * 365 * 86_400 + 5), "2 years");
        assert_eq!(until(2 * 3_600), "2 h");
        assert_eq!(until(5_399), "1 h");
        assert_eq!(until(899), "15 min");
        assert_eq!(until(3 * 86_400), "3 days");
        assert_eq!(in_future(1_000, 990), "due now");
        assert_eq!(in_future(1_000 + 28 * 60, 1_000), "in 28 min");
        assert_eq!(peers(8, 57), "8 / 57 peers");
        assert_eq!(peers(3, 0), "3 / 3 peers");
        assert_eq!(count(1, "torrent"), "1 torrent");
        assert_eq!(count(3, "torrent"), "3 torrents");
        assert_eq!((ratio(0.268), goal(1.0)), ("0.27".into(), "1.0".into()));
        let long = "a".repeat(30) + &"b".repeat(30);
        let cut = middle_truncated(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.starts_with("aaaaaaaaaa…") && cut.ends_with("bbbbbbbbb"), "{cut}");
        assert_eq!(middle_truncated("short", 20), "short");
    }
}
