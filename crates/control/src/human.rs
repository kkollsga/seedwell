//! Human-friendly renderings of bytes, rates and durations (decimal units, as Finder shows them).

/// `1.5 GB`, `12 KB`, `0 B`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[u]) } else { format!("{v:.1} {}", UNITS[u]) }
}

/// `1.2 MB/s`.
pub fn rate(n: u64) -> String {
    format!("{}/s", bytes(n))
}

/// `45s`, `3m 05s`, `2h 05m`, `3d 4h`.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `3 min ago`, `2 h ago`, `yesterday`-free: only seconds, minutes, hours, days.
pub fn ago(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    if d < 60 {
        "just now".into()
    } else {
        format!("{} ago", duration(d))
    }
}

/// `42.5%`.
pub fn percent(frac: f64) -> String {
    format!("{:.1}%", (frac * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1500), "1.5 KB");
        assert_eq!(bytes(1_500_000_000), "1.5 GB");
        assert_eq!(bytes(250_000_000), "250 MB");
        assert_eq!(bytes(u64::MAX), "18447 PB");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(185), "3m 05s");
        assert_eq!(duration(7500), "2h 05m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
        assert_eq!(ago(100, 90), "just now");
        assert_eq!(ago(1000, 100), "15m 00s ago");
    }

    #[test]
    fn rates_and_percent() {
        assert_eq!(rate(1_200_000), "1.2 MB/s");
        assert_eq!(percent(0.4256), "42.6%");
        assert_eq!(percent(7.0), "100.0%");
    }
}
