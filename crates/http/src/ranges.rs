//! A set of completed byte ranges (half-open), kept sorted and merged.

/// Sorted, non-overlapping, non-adjacent half-open ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeSet {
    v: Vec<(u64, u64)>,
}

/// Cap on ranges accepted from a sidecar: with <= 16 workers the live set has
/// at most ~17 fragments, so anything bigger is corrupt.
const MAX_PARSED: usize = 4096;

impl RangeSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `[start, end)`, merging overlaps and neighbours.
    pub fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let (mut s, mut e) = (start, end);
        let mut out = Vec::with_capacity(self.v.len() + 1);
        let mut placed = false;
        for &(a, b) in &self.v {
            if b < s {
                out.push((a, b));
            } else if a > e {
                if !placed {
                    out.push((s, e));
                    placed = true;
                }
                out.push((a, b));
            } else {
                s = s.min(a);
                e = e.max(b);
            }
        }
        if !placed {
            out.push((s, e));
        }
        self.v = out;
    }

    /// Total bytes covered.
    pub fn total(&self) -> u64 {
        self.v.iter().fold(0u64, |acc, &(a, b)| acc.saturating_add(b - a))
    }

    /// No bytes covered.
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    /// Length of the run starting at 0.
    pub fn contiguous_prefix(&self) -> u64 {
        match self.v.first() {
            Some(&(0, b)) => b,
            _ => 0,
        }
    }

    /// `[start, end)` is fully covered.
    pub fn covers(&self, start: u64, end: u64) -> bool {
        start >= end || self.v.iter().any(|&(a, b)| a <= start && end <= b)
    }

    /// The gaps inside `[0, total)`.
    pub fn missing(&self, total: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        for &(a, b) in &self.v {
            if a >= total {
                break;
            }
            if a > cursor {
                out.push((cursor, a));
            }
            cursor = cursor.max(b);
        }
        if cursor < total {
            out.push((cursor, total));
        }
        out
    }

    /// Covered ranges in order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.v.iter().copied()
    }

    /// `a-b,c-d` (half-open, decimal).
    pub fn encode(&self) -> String {
        let mut s = String::new();
        for (i, &(a, b)) in self.v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{a}-{b}"));
        }
        s
    }

    /// Inverse of [`encode`](Self::encode); `None` on any malformed input.
    pub fn parse(text: &str) -> Option<Self> {
        let mut set = RangeSet::new();
        if text.trim().is_empty() {
            return Some(set);
        }
        for (i, part) in text.split(',').enumerate() {
            if i >= MAX_PARSED {
                return None;
            }
            let (a, b) = part.trim().split_once('-')?;
            let (a, b) = (a.parse::<u64>().ok()?, b.parse::<u64>().ok()?);
            if a >= b {
                return None;
            }
            set.insert(a, b);
        }
        Some(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_gaps() {
        let mut r = RangeSet::new();
        r.insert(10, 20);
        r.insert(30, 40);
        r.insert(20, 30);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![(10, 40)]);
        assert_eq!(r.missing(50), vec![(0, 10), (40, 50)]);
        r.insert(0, 10);
        assert_eq!(r.contiguous_prefix(), 40);
        assert!(r.covers(5, 35));
        assert!(!r.covers(5, 45));
        assert_eq!(r.total(), 40);
    }

    #[test]
    fn insert_overlapping_many() {
        let mut r = RangeSet::new();
        r.insert(0, 5);
        r.insert(10, 15);
        r.insert(20, 25);
        r.insert(3, 22);
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![(0, 25)]);
    }

    #[test]
    fn encode_roundtrip_and_rejects() {
        let mut r = RangeSet::new();
        r.insert(0, 7);
        r.insert(9, 12);
        assert_eq!(RangeSet::parse(&r.encode()), Some(r));
        assert_eq!(RangeSet::parse(""), Some(RangeSet::new()));
        assert!(RangeSet::parse("5-5").is_none());
        assert!(RangeSet::parse("a-b").is_none());
        assert!(RangeSet::parse("1-2,3").is_none());
        assert!(RangeSet::parse("-1-2").is_none());
    }

    #[test]
    fn missing_beyond_total_ignored() {
        let mut r = RangeSet::new();
        r.insert(0, 100);
        assert!(r.missing(50).is_empty());
        assert_eq!(r.missing(150), vec![(100, 150)]);
    }
}
