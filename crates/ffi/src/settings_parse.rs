//! What the user types into Settings, read and written back (docs/CROSS-PLATFORM.md §1): speed limits, sizes, hours,
//! times of day, ports, seeding goals and durations. The settings view model ([`crate::settings_view`]) shows values
//! with these and `settings_apply` reads the edits with them; a host checks a field while it is typed with
//! [`settings_field_valid`] (drawn in the error colour when it is not).
//!
//! Units are decimal, like the file manager and the rate columns: 1 MB/s = 1,000,000 B/s, 1 GB = 1,000,000,000 B.
//! A comma or a dot is the decimal mark.

use crate::settings::GoalKind;
use trs_control::display as d;

/// Highest speed limit the core accepts (1 TiB/s).
pub(crate) const RATE_MAX: u64 = 1 << 40;
/// Highest inactive-seed threshold (1 PB).
pub(crate) const SIZE_MAX: u64 = 1_000_000_000_000_000;
/// The inactive-seed window: the default and the longest, in hours of seeding.
pub(crate) const IDLE_HOURS_DEFAULT: u32 = 150;
pub(crate) const IDLE_HOURS_MAX: u32 = 8760;
const DAY: u64 = 86_400;
const HOUR: u64 = 3_600;

/// The units of a speed limit, in the menu's order.
pub(crate) const RATE_UNITS: [&str; 2] = ["MB/s", "KB/s"];
/// The units of the inactive-seed threshold, in the menu's order.
pub(crate) const SIZE_UNITS: [&str; 3] = ["MB", "GB", "TB"];
/// The units of a duration (a seeding goal's time, a minimum seed time), in the menu's order.
pub(crate) const DURATION_UNITS: [&str; 2] = ["hours", "days"];

/// What kind of text a Settings field takes: how it is read and checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsFieldKind {
    /// A whole number (connections, minutes, gigabytes).
    Number,
    /// A speed limit with a unit (`RATE_UNITS`); empty is no limit.
    Rate,
    /// A size with a unit (`SIZE_UNITS`); empty is off.
    Size,
    /// Whole hours, 1 to 8760.
    Hours,
    /// A time of day, `HH:MM`.
    Time,
    /// The torrent listening port: 1024 to 65535, empty is Automatic.
    Port,
    /// Folder sync's port: 1024 to 65535 (empty goes back to the port in use).
    SyncPort,
    /// A seeding ratio ("1.5").
    Ratio,
    /// A number of hours or days (`DURATION_UNITS`).
    Duration,
    /// Any text (names, folders).
    Text,
    /// An address (a tracker's announce URL): any text, drawn monospaced and as wide as the row.
    Address,
}

/// A decimal number as typed: digits and at most one decimal mark (a comma or a dot), no sign, no unit.
fn decimal(text: &str) -> Option<f64> {
    let t = text.trim().replace(',', ".");
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || c == '.') || t.matches('.').count() > 1 {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The number of `bytes` in a unit of `factor`, with up to three decimals and no trailing zeros ("2.5", "500").
fn number(bytes: u64, factor: f64) -> String {
    if bytes == 0 {
        return String::new();
    }
    let s = format!("{:.3}", bytes as f64 / factor);
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

/// Bytes for `text` in a unit of `factor`: empty is 0, too much is `max`, `None` when it is not a number.
fn bytes(text: &str, factor: f64, max: u64) -> Option<u64> {
    if text.trim().is_empty() {
        return Some(0);
    }
    let b = (decimal(text)? * factor).round();
    Some(if b >= max as f64 { max } else { b as u64 })
}

// MARK: Speed limits

fn rate_factor(unit: &str) -> f64 {
    if unit == "KB/s" { 1_000.0 } else { 1_000_000.0 }
}

/// Bytes per second for what was typed in `unit` ("MB/s", "KB/s"): empty is unlimited (0).
pub(crate) fn rate_parse(text: &str, unit: &str) -> Option<u64> {
    bytes(text, rate_factor(unit), RATE_MAX)
}

/// The number as typed in `unit`: "2.5", "500", "" for unlimited.
pub(crate) fn rate_number(bytes: u64, unit: &str) -> String {
    number(bytes, rate_factor(unit))
}

/// The unit a stored limit is shown in first: MB/s from 1 MB/s up (and for unlimited), KB/s below.
pub(crate) fn rate_unit(bytes: u64) -> &'static str {
    if bytes >= 1_000_000 || bytes == 0 { RATE_UNITS[0] } else { RATE_UNITS[1] }
}

// MARK: Sizes and hours (the inactive-seed condition, SPEC §7.5)

fn size_factor(unit: &str) -> f64 {
    match unit {
        "MB" => 1e6,
        "TB" => 1e12,
        _ => 1e9,
    }
}

/// Bytes for what was typed in `unit` ("MB", "GB", "TB"): empty is off (0).
pub(crate) fn size_parse(text: &str, unit: &str) -> Option<u64> {
    bytes(text, size_factor(unit), SIZE_MAX)
}

/// The number as typed in `unit`: "1", "0.5", "" for off.
pub(crate) fn size_number(bytes: u64, unit: &str) -> String {
    number(bytes, size_factor(unit))
}

/// The unit a stored size is shown in first: TB from 1 TB, GB from 100 MB (and for off), else MB.
pub(crate) fn size_unit(bytes: u64) -> &'static str {
    if bytes >= 1_000_000_000_000 {
        "TB"
    } else if bytes >= 100_000_000 || bytes == 0 {
        "GB"
    } else {
        "MB"
    }
}

/// Whole hours, 1 to 8760; `None` for anything else (empty included).
pub(crate) fn hours_parse(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|h| (1..=IDLE_HOURS_MAX).contains(h))
}

/// "1 GB in 150 h": the inactive-seed condition in a list.
#[uniffi::export]
pub fn settings_idle_summary(bytes: u64, hours: u32) -> String {
    let u = size_unit(bytes);
    format!("{} {u} in {hours} h", size_number(bytes, u))
}

// MARK: Times of day

/// Minutes since midnight for "HH:MM" (00:00 to 23:59).
pub(crate) fn time_parse(text: &str) -> Option<u32> {
    let mut parts = text.trim().split(':');
    let (h, m) = (parts.next()?.trim().parse::<u32>().ok()?, parts.next()?.trim().parse::<u32>().ok()?);
    (parts.next().is_none() && h < 24 && m < 60).then_some(h * 60 + m)
}

/// "07:30".
pub(crate) fn time_text(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60 % 24, minute % 60)
}

// MARK: Ports

/// What a typed port means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortEntry {
    /// Empty: a random port at every start (the torrent port), or the port in use (folder sync's).
    Automatic,
    Manual(u16),
    /// Not a number, or outside 1024 to 65535.
    Invalid,
}

pub(crate) fn port_parse(text: &str) -> PortEntry {
    let t = text.trim();
    if t.is_empty() {
        return PortEntry::Automatic;
    }
    match t.parse::<u16>() {
        Ok(v) if v >= 1024 => PortEntry::Manual(v),
        _ => PortEntry::Invalid,
    }
}

/// The field's text: empty for 0 (Automatic).
pub(crate) fn port_text(port: u16) -> String {
    if port == 0 { String::new() } else { port.to_string() }
}

// MARK: Plain numbers

/// A whole number as typed (grouping commas allowed: "1,000").
pub(crate) fn whole_parse(text: &str) -> Option<u32> {
    let t: String = text.trim().chars().filter(|c| *c != ',').collect();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    t.parse::<u32>().ok()
}

/// "1,000": whole numbers as the fields show them (US style, the UI's language).
pub(crate) fn whole_text(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// MARK: Seeding goals (SPEC §7.2)

/// The choices of a seeding goal, in the menu's order: (id, title).
pub(crate) const GOAL_CHOICES: [(&str, &str); 6] = [
    ("ratio", "Ratio"),
    ("time", "Seed time"),
    ("either", "Ratio or time"),
    ("both", "Ratio and time"),
    ("forever", "Seed forever"),
    ("stop", "Stop when finished"),
];

/// The id of a goal's choice.
pub(crate) fn goal_id(g: &GoalKind) -> &'static str {
    match g {
        GoalKind::RatioOnly { .. } => "ratio",
        GoalKind::Time { .. } => "time",
        GoalKind::RatioOrTime { .. } => "either",
        GoalKind::RatioAndTime { .. } => "both",
        GoalKind::Forever => "forever",
        GoalKind::StopWhenFinished => "stop",
    }
}

/// The goal's ratio, or 1.0 when it has none (what a switch to a ratio goal starts from).
pub(crate) fn goal_ratio(g: &GoalKind) -> f64 {
    match *g {
        GoalKind::RatioOnly { ratio } | GoalKind::RatioOrTime { ratio, .. } | GoalKind::RatioAndTime { ratio, .. } => ratio,
        _ => 1.0,
    }
}

/// The goal's time, or 7 days when it has none.
pub(crate) fn goal_secs(g: &GoalKind) -> u64 {
    match *g {
        GoalKind::Time { secs } | GoalKind::RatioOrTime { secs, .. } | GoalKind::RatioAndTime { secs, .. } => secs,
        _ => 7 * DAY,
    }
}

pub(crate) fn goal_has_ratio(g: &GoalKind) -> bool {
    matches!(g, GoalKind::RatioOnly { .. } | GoalKind::RatioOrTime { .. } | GoalKind::RatioAndTime { .. })
}

pub(crate) fn goal_has_time(g: &GoalKind) -> bool {
    matches!(g, GoalKind::Time { .. } | GoalKind::RatioOrTime { .. } | GoalKind::RatioAndTime { .. })
}

/// The goal of another choice, keeping the ratio and the time it had. `None` for an unknown id.
pub(crate) fn goal_with_id(g: &GoalKind, id: &str) -> Option<GoalKind> {
    let (ratio, secs) = (goal_ratio(g), goal_secs(g));
    Some(match id {
        "ratio" => GoalKind::RatioOnly { ratio },
        "time" => GoalKind::Time { secs },
        "either" => GoalKind::RatioOrTime { ratio, secs },
        "both" => GoalKind::RatioAndTime { ratio, secs },
        "forever" => GoalKind::Forever,
        "stop" => GoalKind::StopWhenFinished,
        _ => return None,
    })
}

pub(crate) fn goal_with_ratio(g: &GoalKind, ratio: f64) -> GoalKind {
    match *g {
        GoalKind::RatioOnly { .. } => GoalKind::RatioOnly { ratio },
        GoalKind::RatioOrTime { secs, .. } => GoalKind::RatioOrTime { ratio, secs },
        GoalKind::RatioAndTime { secs, .. } => GoalKind::RatioAndTime { ratio, secs },
        other => other,
    }
}

pub(crate) fn goal_with_secs(g: &GoalKind, secs: u64) -> GoalKind {
    match *g {
        GoalKind::Time { .. } => GoalKind::Time { secs },
        GoalKind::RatioOrTime { ratio, .. } => GoalKind::RatioOrTime { ratio, secs },
        GoalKind::RatioAndTime { ratio, .. } => GoalKind::RatioAndTime { ratio, secs },
        other => other,
    }
}

/// A ratio as typed; at least 0.1.
pub(crate) fn ratio_parse(text: &str) -> Option<f64> {
    decimal(text).map(|r| r.max(0.1))
}

/// "1.0".
pub(crate) fn ratio_text(r: f64) -> String {
    d::goal(r)
}

/// "ratio 1.0 or 7 days": a goal in a rule's summary.
#[uniffi::export]
pub fn settings_goal_summary(goal: GoalKind) -> String {
    let span = |s: u64| if s.is_multiple_of(DAY) { format!("{} days", s / DAY) } else { format!("{} h", (s / HOUR).max(1)) };
    match goal {
        GoalKind::RatioOnly { ratio } => format!("ratio {}", d::goal(ratio)),
        GoalKind::Time { secs } => span(secs),
        GoalKind::RatioOrTime { ratio, secs } => format!("ratio {} or {}", d::goal(ratio), span(secs)),
        GoalKind::RatioAndTime { ratio, secs } => format!("ratio {} and {}", d::goal(ratio), span(secs)),
        GoalKind::Forever => "seed forever".into(),
        GoalKind::StopWhenFinished => "stop when done".into(),
    }
}

// MARK: Durations

fn duration_factor(unit: &str) -> u64 {
    if unit == "hours" { HOUR } else { DAY }
}

/// The unit a duration is shown in first: days for whole days, else hours.
pub(crate) fn duration_unit(secs: u64) -> &'static str {
    if secs.is_multiple_of(DAY) && secs >= DAY { "days" } else { "hours" }
}

/// "7", "1.5": the duration in `unit`, with at most one decimal.
pub(crate) fn duration_number(secs: u64, unit: &str) -> String {
    let v = secs as f64 / duration_factor(unit) as f64;
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map_or(s.clone(), str::to_string)
}

/// Seconds for what was typed in `unit`; at least one unit.
pub(crate) fn duration_parse(text: &str, unit: &str) -> Option<u64> {
    decimal(text).map(|v| (v.max(1.0) * duration_factor(unit) as f64) as u64)
}

// MARK: The private-torrent floor (SPEC §7, §7B.4a)

/// The usual choices of "Private torrents: seed at least", in days (0 = Off).
pub(crate) const PRIVATE_FLOOR_DAYS: [u64; 7] = [0, 1, 3, 7, 10, 14, 30];

/// "Off", "1 day", "10 days", "36 hours", "90 min".
pub(crate) fn private_floor_label(secs: u64) -> String {
    if secs == 0 {
        return "Off".into();
    }
    let plural = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    if secs.is_multiple_of(DAY) {
        plural(secs / DAY, "day", "days")
    } else if secs.is_multiple_of(HOUR) {
        plural(secs / HOUR, "hour", "hours")
    } else {
        format!("{} min", (secs / 60).max(1))
    }
}

// MARK: Checking a field while it is typed

/// Whether `text` would be taken by a field of `kind` (with `unit` for the fields that have one): a host draws a
/// field that would not in the error colour while it is typed. Empty is taken where it means something (no limit,
/// off, Automatic).
#[uniffi::export]
pub fn settings_field_valid(kind: SettingsFieldKind, text: String, unit: String) -> bool {
    match kind {
        SettingsFieldKind::Number => whole_parse(&text).is_some(),
        SettingsFieldKind::Rate => rate_parse(&text, &unit).is_some(),
        SettingsFieldKind::Size => size_parse(&text, &unit).is_some(),
        SettingsFieldKind::Hours => hours_parse(&text).is_some(),
        SettingsFieldKind::Time => time_parse(&text).is_some(),
        SettingsFieldKind::Port => port_parse(&text) != PortEntry::Invalid,
        SettingsFieldKind::SyncPort => port_parse(&text) != PortEntry::Invalid,
        SettingsFieldKind::Ratio => ratio_parse(&text).is_some(),
        SettingsFieldKind::Duration => duration_parse(&text, &unit).is_some(),
        SettingsFieldKind::Text | SettingsFieldKind::Address => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_speed_limit_is_a_number_in_its_unit_and_empty_is_unlimited() {
        assert_eq!(rate_parse("5", "MB/s"), Some(5_000_000));
        assert_eq!(rate_parse("2.5", "MB/s"), Some(2_500_000));
        assert_eq!(rate_parse("0.25", "MB/s"), Some(250_000));
        assert_eq!(rate_parse("500", "KB/s"), Some(500_000));
        assert_eq!(rate_parse("1.5", "KB/s"), Some(1_500));
        assert_eq!(rate_parse("2,5", "MB/s"), Some(2_500_000), "a comma is a decimal mark too");
        assert_eq!(rate_parse(" 3 ", "MB/s"), Some(3_000_000));
        assert_eq!(rate_parse("", "MB/s"), Some(0));
        assert_eq!(rate_parse("   ", "KB/s"), Some(0));
        assert_eq!(rate_parse("0", "MB/s"), Some(0));
        for bad in ["abc", "-5", "1.2.3", "5 MB", "inf", "NaN", "1e3"] {
            assert_eq!(rate_parse(bad, "MB/s"), None, "{bad}");
        }
        assert_eq!(rate_parse("99999999999", "MB/s"), Some(RATE_MAX), "absurd limits stop at the core's ceiling");
    }

    #[test]
    fn a_stored_limit_comes_back_in_its_unit() {
        assert_eq!(rate_unit(5_000_000), "MB/s");
        assert_eq!(rate_unit(2_500_000), "MB/s");
        assert_eq!(rate_unit(500_000), "KB/s");
        assert_eq!(rate_unit(0), "MB/s");
        assert_eq!(rate_number(5_000_000, "MB/s"), "5");
        assert_eq!(rate_number(2_500_000, "MB/s"), "2.5");
        assert_eq!(rate_number(500_000, "KB/s"), "500");
        assert_eq!(rate_number(0, "MB/s"), "");
        assert_eq!(rate_parse(&rate_number(1_230_000, "MB/s"), "MB/s"), Some(1_230_000));
    }

    #[test]
    fn a_size_is_a_number_in_its_unit_and_empty_is_off() {
        assert_eq!(size_parse("1", "GB"), Some(1_000_000_000));
        assert_eq!(size_parse("0,5", "GB"), Some(500_000_000));
        assert_eq!(size_parse("2.5", "TB"), Some(2_500_000_000_000));
        assert_eq!(size_parse("300", "MB"), Some(300_000_000));
        assert_eq!(size_parse("", "GB"), Some(0));
        assert_eq!(size_parse("  ", "GB"), Some(0));
        for bad in ["-1", "1 GB", "1.2.3", "x"] {
            assert_eq!(size_parse(bad, "GB"), None, "{bad}");
        }
        assert_eq!(size_unit(0), "GB");
        assert_eq!(size_unit(1_000_000_000), "GB");
        assert_eq!(size_unit(250_000_000), "GB");
        assert_eq!(size_unit(50_000_000), "MB");
        assert_eq!(size_unit(3_000_000_000_000), "TB");
        assert_eq!(size_number(1_500_000_000, "GB"), "1.5");
        assert_eq!(size_number(0, "GB"), "");
    }

    #[test]
    fn hours_are_whole_and_within_a_year() {
        assert_eq!(hours_parse("150"), Some(150));
        assert_eq!(hours_parse(" 24 "), Some(24));
        for bad in ["0", "9000", "1.5", "", "-3"] {
            assert_eq!(hours_parse(bad), None, "{bad}");
        }
        assert_eq!(settings_idle_summary(1_000_000_000, 150), "1 GB in 150 h");
        assert_eq!(settings_idle_summary(500_000_000, 72), "0.5 GB in 72 h");
    }

    #[test]
    fn a_time_of_day_is_hours_and_minutes() {
        assert_eq!(time_parse("07:30"), Some(450));
        assert_eq!(time_parse("7:05"), Some(425));
        assert_eq!(time_parse("23:59"), Some(1439));
        assert_eq!(time_parse("00:00"), Some(0));
        for bad in ["24:00", "12:60", "12", "12:30:00", "ab:cd", ""] {
            assert_eq!(time_parse(bad), None, "{bad}");
        }
        assert_eq!(time_text(450), "07:30");
        assert_eq!(time_text(0), "00:00");
    }

    #[test]
    fn an_empty_port_is_automatic_and_a_number_is_manual() {
        assert_eq!(port_parse(""), PortEntry::Automatic);
        assert_eq!(port_parse("   "), PortEntry::Automatic);
        assert_eq!(port_parse("55252"), PortEntry::Manual(55252));
        assert_eq!(port_parse(" 1024 "), PortEntry::Manual(1024));
        assert_eq!(port_parse("65535"), PortEntry::Manual(65535));
        for bad in ["80", "1023", "65536", "99999", "abc", "12 34", "-5"] {
            assert_eq!(port_parse(bad), PortEntry::Invalid, "{bad}");
        }
        assert_eq!(port_text(0), "");
        assert_eq!(port_text(55252), "55252");
    }

    #[test]
    fn whole_numbers_take_grouping_commas() {
        assert_eq!(whole_parse("40"), Some(40));
        assert_eq!(whole_parse("1,000"), Some(1000));
        for bad in ["", "-1", "1.5", "x"] {
            assert_eq!(whole_parse(bad), None, "{bad}");
        }
        assert_eq!(whole_text(500), "500");
        assert_eq!(whole_text(1000), "1,000");
        assert_eq!(whole_text(1_234_567), "1,234,567");
    }

    #[test]
    fn a_goal_keeps_its_ratio_and_time_across_choices() {
        let g = GoalKind::RatioOrTime { ratio: 2.0, secs: 3 * DAY };
        assert_eq!(goal_id(&g), "either");
        assert_eq!(goal_with_id(&g, "time"), Some(GoalKind::Time { secs: 3 * DAY }));
        assert_eq!(goal_with_id(&g, "both"), Some(GoalKind::RatioAndTime { ratio: 2.0, secs: 3 * DAY }));
        assert_eq!(goal_with_id(&GoalKind::Forever, "ratio"), Some(GoalKind::RatioOnly { ratio: 1.0 }));
        assert_eq!(goal_with_id(&GoalKind::Forever, "time"), Some(GoalKind::Time { secs: 7 * DAY }));
        assert_eq!(goal_with_id(&g, "nope"), None);
        assert_eq!(goal_with_ratio(&g, 1.5), GoalKind::RatioOrTime { ratio: 1.5, secs: 3 * DAY });
        assert_eq!(goal_with_ratio(&GoalKind::Forever, 1.5), GoalKind::Forever);
        assert_eq!(goal_with_secs(&g, HOUR), GoalKind::RatioOrTime { ratio: 2.0, secs: HOUR });
        assert_eq!(ratio_parse("1,5"), Some(1.5));
        assert_eq!(ratio_parse("0"), Some(0.1), "a ratio is at least 0.1");
        assert_eq!(ratio_parse("x"), None);
        assert_eq!(ratio_text(2.0), "2.0");
    }

    #[test]
    fn a_goal_reads_as_a_short_summary() {
        assert_eq!(settings_goal_summary(GoalKind::RatioOnly { ratio: 1.0 }), "ratio 1.0");
        assert_eq!(settings_goal_summary(GoalKind::Time { secs: 7 * DAY }), "7 days");
        assert_eq!(settings_goal_summary(GoalKind::Time { secs: 36 * HOUR }), "36 h");
        assert_eq!(settings_goal_summary(GoalKind::RatioOrTime { ratio: 2.0, secs: 3 * DAY }), "ratio 2.0 or 3 days");
        assert_eq!(settings_goal_summary(GoalKind::RatioAndTime { ratio: 1.5, secs: 60 }), "ratio 1.5 and 1 h");
        assert_eq!(settings_goal_summary(GoalKind::Forever), "seed forever");
        assert_eq!(settings_goal_summary(GoalKind::StopWhenFinished), "stop when done");
    }

    #[test]
    fn a_duration_is_hours_or_days() {
        assert_eq!(duration_unit(7 * DAY), "days");
        assert_eq!(duration_unit(36 * HOUR), "hours");
        assert_eq!(duration_unit(0), "hours");
        assert_eq!(duration_number(7 * DAY, "days"), "7");
        assert_eq!(duration_number(36 * HOUR, "days"), "1.5");
        assert_eq!(duration_number(72 * HOUR, "hours"), "72");
        assert_eq!(duration_parse("2", "days"), Some(2 * DAY));
        assert_eq!(duration_parse("1,5", "hours"), Some(5_400));
        assert_eq!(duration_parse("0", "hours"), Some(HOUR), "at least one unit");
        assert_eq!(duration_parse("soon", "days"), None);
    }

    #[test]
    fn the_private_floor_labels_its_choices_and_custom_values() {
        assert_eq!(private_floor_label(0), "Off");
        assert_eq!(private_floor_label(DAY), "1 day");
        assert_eq!(private_floor_label(10 * DAY), "10 days");
        assert_eq!(private_floor_label(HOUR), "1 hour");
        assert_eq!(private_floor_label(36 * HOUR), "36 hours");
        assert_eq!(private_floor_label(90 * 60), "90 min");
        assert_eq!(private_floor_label(10), "1 min");
    }

    #[test]
    fn a_field_is_checked_as_its_kind() {
        let ok = |k, t: &str, u: &str| settings_field_valid(k, t.into(), u.into());
        assert!(ok(SettingsFieldKind::Rate, "", "MB/s"));
        assert!(!ok(SettingsFieldKind::Rate, "fast", "MB/s"));
        assert!(ok(SettingsFieldKind::Size, "1.5", "TB"));
        assert!(!ok(SettingsFieldKind::Hours, "0", ""));
        assert!(ok(SettingsFieldKind::Time, "08:00", ""));
        assert!(ok(SettingsFieldKind::Port, "", ""));
        assert!(!ok(SettingsFieldKind::SyncPort, "80", ""));
        assert!(ok(SettingsFieldKind::Number, "1,000", ""));
        assert!(!ok(SettingsFieldKind::Ratio, "-1", ""));
        assert!(ok(SettingsFieldKind::Duration, "3", "days"));
        assert!(ok(SettingsFieldKind::Text, "anything", ""));
    }
}
