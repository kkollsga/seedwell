//! The app's wording for sizes, rates, times and counts (`trs_control::display`), for every host app.

use trs_control::display as d;

/// "6.2 GB", "412 MB", "999 bytes".
#[uniffi::export]
pub fn fmt_bytes(n: u64) -> String {
    d::bytes(n)
}

/// "2.1 MB/s"; "0 KB/s" when nothing flows.
#[uniffi::export]
pub fn fmt_rate(n: u64) -> String {
    d::rate(n)
}

/// "↑ 2.1 MB/s", or a bare "–" while nothing flows.
#[uniffi::export]
pub fn fmt_live_rate(n: u64, arrow: &str) -> String {
    d::live_rate(n, arrow)
}

/// "4.6 of 6.2 GB".
#[uniffi::export]
pub fn fmt_progress(done: u64, total: u64) -> String {
    d::progress(done, total)
}

/// "2 minutes remaining", "2 min left" (`compact`); `None`: not known yet.
#[uniffi::export]
pub fn fmt_remaining(secs: Option<u64>, compact: bool) -> String {
    d::remaining(secs, compact)
}

/// "12 minutes ago", "12 min ago" (`compact`), "Yesterday"; Unix seconds.
#[uniffi::export]
pub fn fmt_ago(unix: u64, now: u64, compact: bool) -> String {
    d::ago(unix, now, compact)
}

/// "12 minutes", "9 days", "2 years".
#[uniffi::export]
pub fn fmt_duration(secs: u64) -> String {
    d::duration(secs)
}

/// "2 h", "15 min", "3 days".
#[uniffi::export]
pub fn fmt_until(secs: u64) -> String {
    d::until(secs)
}

/// "in 28 min", "due now".
#[uniffi::export]
pub fn fmt_in_future(unix: u64, now: u64) -> String {
    d::in_future(unix, now)
}

/// "8 / 57 peers".
#[uniffi::export]
pub fn fmt_peers(connected: u32, known: u32) -> String {
    d::peers(connected, known)
}

/// Where the known peers come from (hover text).
#[uniffi::export]
pub fn fmt_peers_help() -> String {
    d::PEERS_HELP.into()
}

/// "1 torrent", "3 torrents".
#[uniffi::export]
pub fn fmt_count(n: u64, noun: &str) -> String {
    d::count(n, noun)
}

/// "0.27".
#[uniffi::export]
pub fn fmt_ratio(r: f64) -> String {
    d::ratio(r)
}

/// "1.0".
#[uniffi::export]
pub fn fmt_goal(r: f64) -> String {
    d::goal(r)
}

/// `text` cut in the middle with "…" past `limit` characters.
#[uniffi::export]
pub fn fmt_middle_truncated(text: &str, limit: u32) -> String {
    d::middle_truncated(text, limit as usize)
}

/// Which way a rate goes, for the words around a limit ("Download limit: 18 MB/s").
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RateDirection {
    Download,
    Upload,
}

/// "5 MB/s", "2.5 MB/s", "500 KB/s": a speed limit as it was typed (MB/s from 1 MB/s up, KB/s below, at most three
/// decimals); "Unlimited" for 0.
#[uniffi::export]
pub fn rate_limit_label(bytes: u64) -> String {
    if bytes == 0 {
        return "Unlimited".into();
    }
    let (factor, unit) = if bytes >= 1_000_000 { (1_000_000.0, "MB/s") } else { (1_000.0, "KB/s") };
    let mut n = format!("{:.3}", bytes as f64 / factor);
    while n.ends_with('0') {
        n.pop();
    }
    if n.ends_with('.') {
        n.pop();
    }
    format!("{n} {unit}")
}

/// The tooltip of a live rate: "Download limit: 18 MB/s (Limit Speed)", "No upload limit".
#[uniffi::export]
pub fn rate_limit_help(direction: RateDirection, limit: Option<u64>, alt_speed: bool) -> String {
    let kind = match direction {
        RateDirection::Download => "Download",
        RateDirection::Upload => "Upload",
    };
    match limit.filter(|&l| l > 0) {
        None => format!("No {} limit{}", kind.to_lowercase(), if alt_speed { " (Limit Speed is on)" } else { "" }),
        Some(l) => format!("{kind} limit: {}{}", rate_limit_label(l), if alt_speed { " (Limit Speed)" } else { "" }),
    }
}

/// A rate against its limit: "2.0 / 5 MB/s" (the unit once when both share it), "850 KB/s / 5 MB/s"; the bare rate
/// without a limit.
#[uniffi::export]
pub fn rate_with_limit_line(rate: u64, limit: Option<u64>) -> String {
    let r = d::rate(rate);
    let Some(limit) = limit.filter(|&l| l > 0) else { return r };
    let l = rate_limit_label(limit);
    let split = |s: &str| match s.rsplit_once(' ') {
        Some((n, u)) => (n.to_owned(), u.to_owned()),
        None => (String::new(), s.to_owned()),
    };
    let ((rn, ru), (ln, lu)) = (split(&r), split(&l));
    if ru == lu { format!("{rn} / {ln} {lu}") } else { format!("{r} / {l}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_read_as_they_were_typed() {
        assert_eq!(rate_limit_label(5_000_000), "5 MB/s");
        assert_eq!(rate_limit_label(2_500_000), "2.5 MB/s");
        assert_eq!(rate_limit_label(500_000), "500 KB/s");
        assert_eq!(rate_limit_label(1_234_567), "1.235 MB/s");
        assert_eq!(rate_limit_label(0), "Unlimited");
    }

    #[test]
    fn a_rate_against_its_limit_names_the_unit_once_when_both_share_it() {
        assert_eq!(rate_with_limit_line(2_000_000, Some(5_000_000)), "2.0 / 5 MB/s");
        assert_eq!(rate_with_limit_line(850_000, Some(5_000_000)), "850 KB/s / 5 MB/s");
        assert_eq!(rate_with_limit_line(120_000, Some(500_000)), "120 / 500 KB/s");
        assert_eq!(rate_with_limit_line(2_000_000, None), "2.0 MB/s");
        assert_eq!(rate_with_limit_line(2_000_000, Some(0)), "2.0 MB/s");
    }

    #[test]
    fn the_sidebar_rates_say_their_limit_in_the_tooltip() {
        assert_eq!(rate_limit_help(RateDirection::Download, Some(18_000_000), false), "Download limit: 18 MB/s");
        assert_eq!(rate_limit_help(RateDirection::Upload, None, false), "No upload limit");
        assert_eq!(rate_limit_help(RateDirection::Upload, Some(500_000), true), "Upload limit: 500 KB/s (Limit Speed)");
        assert_eq!(rate_limit_help(RateDirection::Download, Some(0), true), "No download limit (Limit Speed is on)");
    }
}
