//! Pure planning of what the picker should hurry (SPEC 7F.2).
//!
//! Inputs: the live read cursors of one stream, the index ranges still wanted, and a bitrate
//! estimate. Output: the stream's complete [`UrgentRange`] list. No I/O, no clock.
//!
//! * Each cursor contributes an **urgent window** (~30 s of playback, at least 8 MiB) starting at
//!   its read offset, then a **read-ahead** (~2 min) behind it.
//! * Index ranges (head, tail, `moov`, `Cues`) are urgent with deadline 0.
//! * Overlapping contributions (several small ranged requests from one player, a player probing
//!   the index while playing) are merged so each piece appears once, with its earliest deadline.
//!   A piece that is both urgent and read-ahead is urgent.

use crate::source::{FileRange, Geometry, Priority, UrgentRange};
use std::ops::Range;
use std::time::Duration;

pub const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PlanConfig {
    /// Seconds of playback in the urgent window.
    pub urgent_secs: u64,
    /// Lower bound of the urgent window in bytes.
    pub urgent_min_bytes: u64,
    /// Seconds of playback in the read-ahead.
    pub readahead_secs: u64,
    /// Upper bound of the read-ahead in bytes.
    pub readahead_max_bytes: u64,
    /// Bytes per second assumed until the duration is known (8 Mbit/s).
    pub fallback_bytes_per_sec: u64,
    /// Bytes at the head and the tail of a file fetched first.
    pub index_bytes: u64,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            urgent_secs: 30,
            urgent_min_bytes: 8 * MIB,
            readahead_secs: 120,
            readahead_max_bytes: 1024 * MIB,
            fallback_bytes_per_sec: 1_000_000,
            index_bytes: 2 * MIB,
        }
    }
}

/// Bytes per second: size / duration when the duration is known, else the fallback. Clamped to a
/// sane band so a bad `mvhd` cannot produce a zero or absurd window.
pub fn bitrate(file_len: u64, duration: Option<Duration>, cfg: &PlanConfig) -> u64 {
    let est = match duration {
        Some(d) if d.as_secs_f64() >= 1.0 => (file_len as f64 / d.as_secs_f64()) as u64,
        _ => cfg.fallback_bytes_per_sec,
    };
    est.clamp(50_000, 200 * MIB)
}

/// Size of the urgent window in bytes for a bitrate.
pub fn urgent_bytes(bps: u64, cfg: &PlanConfig) -> u64 {
    bps.saturating_mul(cfg.urgent_secs).max(cfg.urgent_min_bytes)
}

/// Size of the read-ahead in bytes for a bitrate.
pub fn readahead_bytes(bps: u64, cfg: &PlanConfig) -> u64 {
    bps.saturating_mul(cfg.readahead_secs).min(cfg.readahead_max_bytes)
}

/// The head and tail of a file (file offsets) that are fetched before anything else.
pub fn head_tail(file_len: u64, cfg: &PlanConfig) -> [Range<u64>; 2] {
    let n = cfg.index_bytes.min(file_len);
    [0..n, file_len - n..file_len]
}

#[derive(Clone, Copy, Debug)]
struct Cand {
    first: u32,
    end: u32,
    prio: Priority,
    /// Piece whose deadline is zero; `None` = every piece has deadline zero (index).
    anchor: Option<u32>,
    /// Candidate identity (cursor index, or 1000+ for index ranges), to merge neighbours.
    id: usize,
}

fn rank(p: Priority) -> u8 {
    match p {
        Priority::Urgent => 0,
        Priority::ReadAhead => 1,
    }
}

/// Plan one stream. `cursors` are file offsets of live read positions, `index` file-offset
/// ranges still wanted. See the module docs.
pub fn plan(
    geom: Geometry,
    file: &FileRange,
    cursors: &[u64],
    index: &[Range<u64>],
    bps: u64,
    cfg: &PlanConfig,
) -> Vec<UrgentRange> {
    if file.len == 0 || geom.piece_length == 0 {
        return Vec::new();
    }
    let uw = urgent_bytes(bps, cfg);
    let ra = readahead_bytes(bps, cfg);
    let to_pieces = |r: Range<u64>| -> Range<u32> {
        let s = r.start.min(file.len);
        let e = r.end.min(file.len);
        if s >= e {
            return 0..0;
        }
        geom.pieces_of(file.offset + s..file.offset + e)
    };
    let mut cands: Vec<Cand> = Vec::with_capacity(cursors.len() * 2 + index.len());
    for (i, &c) in cursors.iter().enumerate() {
        if c >= file.len {
            continue;
        }
        let anchor = geom.piece_of(file.offset + c);
        let u = to_pieces(c..c.saturating_add(uw));
        cands.push(Cand { first: u.start, end: u.end, prio: Priority::Urgent, anchor: Some(anchor), id: i });
        let r = to_pieces(c.saturating_add(uw)..c.saturating_add(uw).saturating_add(ra));
        // The read-ahead may start in the same piece the window ends in; urgent wins there.
        if r.start < r.end {
            cands.push(Cand { first: r.start, end: r.end, prio: Priority::ReadAhead, anchor: Some(anchor), id: i });
        }
    }
    for (i, r) in index.iter().enumerate() {
        let p = to_pieces(r.clone());
        if p.start < p.end {
            cands.push(Cand { first: p.start, end: p.end, prio: Priority::Urgent, anchor: None, id: 1000 + i });
        }
    }
    if cands.is_empty() {
        return Vec::new();
    }
    let mut bounds: Vec<u32> = cands.iter().flat_map(|c| [c.first, c.end]).collect();
    bounds.sort_unstable();
    bounds.dedup();

    let step_ns = (u128::from(geom.piece_length) * 1_000_000_000 / u128::from(bps.max(1))) as u64;
    let step = Duration::from_nanos(step_ns);
    let deadline_at = |c: &Cand, piece: u32| -> Duration {
        match c.anchor {
            Some(a) => Duration::from_nanos(step_ns.saturating_mul(u64::from(piece.saturating_sub(a)))),
            None => Duration::ZERO,
        }
    };

    let mut out: Vec<UrgentRange> = Vec::new();
    let mut last_id: Option<usize> = None;
    for w in bounds.windows(2) {
        let (x, y) = (w[0], w[1]);
        let best = cands
            .iter()
            .filter(|c| c.first <= x && c.end >= y)
            .min_by_key(|c| (rank(c.prio), deadline_at(c, x)));
        let Some(best) = best else {
            last_id = None;
            continue;
        };
        // Same candidate continuing: extend the previous range.
        if let (Some(prev), Some(pid)) = (out.last_mut(), last_id)
            && pid == best.id
            && prev.priority == best.prio
            && prev.pieces.1 == x
        {
            prev.pieces.1 = y;
            continue;
        }
        out.push(UrgentRange { pieces: (x, y), deadline: deadline_at(best, x), step, priority: best.prio });
        last_id = Some(best.id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(piece: u32, total: u64) -> Geometry {
        Geometry { piece_length: piece, total_len: total }
    }
    fn file(off: u64, len: u64) -> FileRange {
        FileRange { index: 0, name: "a.mkv".into(), offset: off, len }
    }

    #[test]
    fn bitrate_from_duration_and_fallback() {
        let c = PlanConfig::default();
        assert_eq!(bitrate(600_000_000, Some(Duration::from_secs(600)), &c), 1_000_000);
        assert_eq!(bitrate(10, None, &c), c.fallback_bytes_per_sec);
        assert_eq!(bitrate(10, Some(Duration::from_secs(1000)), &c), 50_000);
        assert!(urgent_bytes(100_000, &c) >= 8 * MIB);
        assert_eq!(urgent_bytes(4_000_000, &c), 120_000_000);
    }

    #[test]
    fn single_cursor_urgent_then_readahead() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 2000 * MIB);
        let f = file(0, 2000 * MIB);
        // 1 MB/s => urgent 30 MB (>8 MiB), read-ahead 120 MB.
        let p = plan(g, &f, &[10 * MIB], &[], 1_000_000, &c);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].priority, Priority::Urgent);
        assert_eq!(p[0].pieces.0, 10);
        assert_eq!(p[0].deadline, Duration::ZERO);
        assert!(p[0].pieces.1 > 10 + 28 && p[0].pieces.1 <= 10 + 31);
        assert_eq!(p[1].priority, Priority::ReadAhead);
        assert!(p[1].pieces.0 <= p[0].pieces.1);
        assert!(p[1].pieces.1 - p[1].pieces.0 >= 110);
        assert!(p[1].deadline > Duration::from_secs(25));
        // step = 1 MiB at 1 MB/s
        assert!(p[0].step > Duration::from_millis(1000) && p[0].step < Duration::from_millis(1100));
    }

    #[test]
    fn min_window_applies_for_low_bitrate() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 100 * MIB);
        let f = file(0, 100 * MIB);
        let p = plan(g, &f, &[0], &[], 100_000, &c);
        assert_eq!(p[0].pieces, (0, 8));
    }

    #[test]
    fn clips_at_end_of_file_and_respects_file_offset() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 100 * MIB);
        let f = file(10 * MIB, 20 * MIB);
        let p = plan(g, &f, &[15 * MIB], &[], 1_000_000, &c);
        assert_eq!(p.len(), 1, "window runs to the file end, no read-ahead left");
        assert_eq!(p[0].pieces, (25, 30));
        assert!(plan(g, &f, &[20 * MIB], &[], 1_000_000, &c).is_empty());
    }

    #[test]
    fn overlapping_cursors_merge_with_earliest_deadlines() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 5000 * MIB);
        let f = file(0, 5000 * MIB);
        // Two small ranged requests 2 MiB apart (the AVFoundation pattern) and nothing else.
        let p = plan(g, &f, &[100 * MIB, 102 * MIB], &[], 1_000_000, &c);
        // No piece appears twice.
        let mut seen = std::collections::HashSet::new();
        for r in &p {
            for q in r.range() {
                assert!(seen.insert(q), "piece {q} twice");
            }
        }
        // First urgent range covers 100..102 from cursor A, then B's window continues with deadline 0 at 102.
        let b = p.iter().find(|r| r.pieces.0 == 102).expect("range starting at B");
        assert_eq!(b.deadline, Duration::ZERO);
        assert_eq!(b.priority, Priority::Urgent);
        assert_eq!(p[0].pieces, (100, 102));
        // Pieces 102..~132 urgent, then read-ahead continues.
        let urgent_end = p.iter().filter(|r| r.priority == Priority::Urgent).map(|r| r.pieces.1).max().unwrap();
        assert!(urgent_end >= 131);
    }

    #[test]
    fn far_cursors_stay_separate() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 5000 * MIB);
        let f = file(0, 5000 * MIB);
        let p = plan(g, &f, &[0, 3000 * MIB], &[], 1_000_000, &c);
        assert_eq!(p.iter().filter(|r| r.priority == Priority::Urgent).count(), 2);
    }

    #[test]
    fn index_ranges_urgent_deadline_zero_and_win_overlap() {
        let c = PlanConfig::default();
        let g = geom(MIB as u32, 500 * MIB);
        let f = file(0, 500 * MIB);
        let idx = head_tail(f.len, &c);
        let p = plan(g, &f, &[0], &idx, 1_000_000, &c);
        let tail = p.iter().find(|r| r.pieces.1 == 500).expect("tail");
        assert_eq!(tail.pieces, (498, 500));
        assert_eq!(tail.deadline, Duration::ZERO);
        assert_eq!(tail.priority, Priority::Urgent);
        // Head overlaps the cursor window: still one range starting at 0 with deadline 0.
        assert_eq!(p[0].pieces.0, 0);
        assert_eq!(p[0].deadline, Duration::ZERO);
    }

    #[test]
    fn head_tail_small_file() {
        let c = PlanConfig::default();
        assert_eq!(head_tail(MIB, &c), [0..MIB, 0..MIB]);
        assert_eq!(head_tail(10 * MIB, &c), [0..2 * MIB, 8 * MIB..10 * MIB]);
    }

    #[test]
    fn big_offsets_do_not_overflow() {
        let c = PlanConfig::default();
        let total = 6 * 1024 * MIB;
        let g = geom(4 * MIB as u32, total);
        let f = file(MIB, total - MIB);
        let p = plan(g, &f, &[5 * 1024 * MIB], &[], 5_000_000, &c);
        assert!(p[0].pieces.0 > 1000);
        assert!(p[0].pieces.1 <= g.piece_count());
    }
}
