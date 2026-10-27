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
