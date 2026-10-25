//! Which archived versions to keep (SPEC §7G.2): staggered over 30 days by default, within a size cap.
//!
//! Pure: the folder lists its archive and deletes what [`prune`] returns.

/// One archived version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archived {
    /// The path of the file it is a version of (wire form).
    pub of: String,
    /// When it was archived, Unix ns.
    pub at_ns: i64,
    /// Bytes.
    pub size: u64,
    /// Where it is (relative to the folder root).
    pub stored_at: String,
}

/// How long and how much to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// Nothing older than this is kept (0 = keep nothing: the archive is off).
    pub max_age_s: i64,
    /// The archive may hold at most this share of the folder's size, in percent (0 = no cap).
    pub max_percent: u8,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { max_age_s: 30 * 86_400, max_percent: 10 }
    }
}

const HOUR: i64 = 3600 * 1_000_000_000;
const DAY: i64 = 24 * HOUR;

/// The versions to delete, given everything in the archive, the folder's size and now.
///
/// - older than `max_age_s`: gone;
/// - per file, in the last day one version per hour is kept, before that one per day (the newest
///   version of each hour or day: the state the file was in at the end of it);
/// - then, while the archive is over the size cap, the oldest go first.
pub fn prune(items: &[Archived], folder_size: u64, now_ns: i64, r: Retention) -> Vec<String> {
    let max_age = r.max_age_s.saturating_mul(1_000_000_000);
    let mut keep: Vec<&Archived> = Vec::new();
    let mut drop: Vec<String> = Vec::new();

    let mut sorted: Vec<&Archived> = items.iter().collect();
    // Per file, newest first, so the first version seen in a bucket is the one kept.
    sorted.sort_by(|a, b| a.of.cmp(&b.of).then(b.at_ns.cmp(&a.at_ns)));
    let mut last: Option<(&str, i64)> = None;
    for a in sorted {
        let age = now_ns.saturating_sub(a.at_ns);
        if max_age == 0 || age > max_age {
            drop.push(a.stored_at.clone());
            continue;
        }
        let bucket = if age < DAY { a.at_ns.div_euclid(HOUR) } else { -1 - a.at_ns.div_euclid(DAY) };
        if last == Some((a.of.as_str(), bucket)) {
            drop.push(a.stored_at.clone());
            continue;
        }
        last = Some((a.of.as_str(), bucket));
        keep.push(a);
    }

    if r.max_percent > 0 {
        let cap = folder_size / 100 * u64::from(r.max_percent);
        let mut total: u64 = keep.iter().map(|a| a.size).sum();
        keep.sort_by_key(|a| a.at_ns);
        for a in keep {
            if total <= cap {
                break;
            }
            total -= a.size;
            drop.push(a.stored_at.clone());
        }
    }
    drop.sort();
    drop
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(of: &str, at_ns: i64, size: u64) -> Archived {
        Archived { of: of.into(), at_ns, size, stored_at: format!("{of}@{at_ns}") }
    }

    #[test]
    fn staggered_keeps_one_per_hour_then_one_per_day() {
        let now = 100 * DAY;
        let m = 60 * 1_000_000_000;
        let items = [
            item("a", now - 10 * m, 1),            // this hour: kept (newest of its hour)
            item("a", now - 20 * m, 1),            // same hour (now is on the hour): dropped
            item("a", now - 3 * DAY + HOUR, 1),     // a day bucket
            item("a", now - 3 * DAY + 2 * HOUR, 1), // same day bucket, newer: kept
            item("a", now - 40 * DAY, 1),          // too old
        ];
        let dropped = prune(&items, 1_000_000, now, Retention::default());
        assert!(dropped.contains(&items[4].stored_at), "older than 30 days");
        assert!(dropped.contains(&items[1].stored_at), "older version in the same hour");
        assert!(dropped.contains(&items[2].stored_at), "older version in the same day");
        assert!(!dropped.contains(&items[3].stored_at));
        assert!(!dropped.contains(&items[0].stored_at), "the newest is always kept");
    }

    #[test]
    fn the_size_cap_drops_the_oldest_first() {
        let now = 10 * DAY;
        let items = [item("a", now - 3 * DAY, 60), item("b", now - 2 * DAY, 30), item("c", now - DAY - HOUR, 30)];
        // Folder 1000 bytes, 10 % cap = 100 bytes; 120 archived → the oldest (60) goes.
        assert_eq!(prune(&items, 1000, now, Retention::default()), vec![items[0].stored_at.clone()]);
    }

    #[test]
    fn off_means_nothing_is_kept() {
        let items = [item("a", 0, 1)];
        assert_eq!(prune(&items, 10, 1, Retention { max_age_s: 0, max_percent: 0 }).len(), 1);
    }
}
