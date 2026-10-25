//! The global version of a path: which of the versions the devices hold is current, who has it, and
//! what this device must do about it. Pure functions; the index calls them for every path that changes.
//!
//! The rules are Syncthing's (SPEC §7G.2):
//! - a version that has seen another (dominates it) replaces it;
//! - of versions that have not seen each other (concurrent), the one that [`FileInfo::wins_conflict`]
//!   is current, and the device holding a loser keeps it as a conflict copy;
//! - every device computes the same answer from the same versions, whatever order they arrived in.
//!
//! Taking the winner ends a conflict: once no device announces the losing version (its content now
//! lives on as a conflict copy, a new file), nothing is concurrent any more. One exception needs a
//! merged version, made and signed by a writer: a delete that lost to a change. Its history must live
//! on in the change's version (else a version it had replaced can come back), so the change's holder
//! absorbs it ([`Need::Absorb`]). Two concurrent deletes are merged by the device that takes the
//! other's (`scan::adopt`).

use std::cmp::Ordering;

use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{Causality, FileInfo, SignedFile};

/// One device's version of a path.
#[derive(Clone, Debug)]
pub struct Candidate<'a> {
    /// The device holding it (this device too).
    pub device: ShortId,
    /// Its version.
    pub file: &'a SignedFile,
    /// The device has the path but not this content (a read-only device's local change): it takes part
    /// in nothing, it only says "don't fetch from me".
    pub invalid: bool,
}

/// A total order on versions for picking the current one: a version that dominates another is greater;
/// otherwise [`FileInfo::wins_conflict`], then the signature bytes so that even two different
/// versions with equal counters, time and device (a bug elsewhere) give one answer everywhere.
fn precedes(a: &SignedFile, b: &SignedFile) -> Ordering {
    match a.info.version.compare(&b.info.version) {
        Causality::Greater => return Ordering::Greater,
        Causality::Lesser => return Ordering::Less,
        Causality::Equal | Causality::Concurrent => {}
    }
    if a.info.wins_conflict(&b.info) {
        Ordering::Greater
    } else if b.info.wins_conflict(&a.info) {
        Ordering::Less
    } else {
        a.sig.cmp(&b.sig)
    }
}

/// The current version of a path among `candidates` (invalid ones left out), or `None` when no device
/// holds a valid version. Order independent: only versions that no other version dominates take part,
/// and among those the conflict rule picks one.
pub fn current<'a>(candidates: &[Candidate<'a>]) -> Option<&'a SignedFile> {
    let valid: Vec<&SignedFile> = candidates.iter().filter(|c| !c.invalid).map(|c| c.file).collect();
    let maximal = valid.iter().filter(|f| {
        !valid.iter().any(|g| g.info.version.compare(&f.info.version) == Causality::Greater)
    });
    maximal.copied().max_by(|a, b| precedes(a, b))
}

/// The devices that hold exactly `current` (same version, same content) and can serve its chunks.
pub fn holders(candidates: &[Candidate<'_>], current: &SignedFile) -> Vec<ShortId> {
    candidates
        .iter()
        .filter(|c| {
            !c.invalid
                && c.file.info.version.compare(&current.info.version) == Causality::Equal
                && c.file.info.same_content(&current.info)
        })
        .map(|c| c.device)
        .collect()
}

/// What this device must do for one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Nothing: this device holds the current version (or one only it has made, which the others will
    /// fetch).
    Nothing,
    /// Take the current version: it has seen ours, or we have none.
    Take,
    /// Take the current version, which has not seen ours and won the conflict. Our content, if it is a
    /// real file that differs, becomes a conflict copy first.
    TakeOverConflict,
    /// Our version is equal in counters but differs in content from the current one (should not happen;
    /// treated as a conflict the current one won, so no data is lost).
    Repair,
    /// We hold the current version, and a delete made concurrently with it lost ("a change beats a
    /// delete"). Nothing changes on disk: a writer records a new version of the same content that has
    /// seen the delete (`merge(ours, delete).bump(me)`). Else a device that took our version over its
    /// delete drops the delete's history, and a version that delete had replaced, still held by a
    /// device that has not caught up, is concurrent with ours again and can win (the deleted content
    /// comes back). Syncthing does the same on the side that holds the change.
    Absorb,
}

/// What this device must do, given its own version (`None` = it has nothing at this path) and the
/// current one.
pub fn need(local: Option<&FileInfo>, current: Option<&FileInfo>) -> Need {
    let (Some(cur), Some(mine)) = (current, local) else {
        // Nobody has it, or we have nothing: take it unless it is a tombstone of something we never had.
        return match current {
            Some(cur) if !cur.deleted => Need::Take,
            _ => Need::Nothing,
        };
    };
    match mine.version.compare(&cur.version) {
        Causality::Greater => Need::Nothing,
        Causality::Lesser => Need::Take,
        Causality::Equal if mine.same_content(cur) => Need::Nothing,
        // `current` already chose between the two, and it was not ours.
        Causality::Equal => Need::Repair,
        Causality::Concurrent if mine.same_content(cur) => Need::Take,
        Causality::Concurrent => Need::TakeOverConflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_sync_proto::model::{Counter, FileKind, Version};

    fn ver(pairs: &[(u64, u64)]) -> Version {
        Version::from_counters(pairs.iter().map(|&(id, value)| Counter { id: ShortId(id), value }).collect())
            .unwrap()
    }

    fn file(version: Version, by: u64, mtime_ns: i64, content: u8, deleted: bool) -> SignedFile {
        SignedFile {
            info: FileInfo {
                path: "a".into(),
                kind: FileKind::File,
                size: if deleted { 0 } else { 1 },
                mtime_ns,
                exec: false,
                deleted,
                version,
                modified_by: ShortId(by),
                file_id: [0; 16],
                symlink_target: String::new(),
                conflict_of: None,
                chunks: if deleted { vec![] } else { vec![trs_sync_proto::model::ChunkRef { len: 1, hash: [content; 32] }] },
            },
            sig: [by as u8; 64],
        }
    }

    fn cand(device: u64, f: &SignedFile) -> Candidate<'_> {
        Candidate { device: ShortId(device), file: f, invalid: false }
    }

    #[test]
    fn the_newest_causal_version_is_current() {
        let old = file(ver(&[(1, 1)]), 1, 10, 1, false);
        let new = file(ver(&[(1, 1), (2, 1)]), 2, 5, 2, false);
        let c = [cand(1, &old), cand(2, &new)];
        assert_eq!(current(&c), Some(&new), "a dominating version wins even with an older mtime");
    }

    #[test]
    fn concurrent_versions_pick_one_winner_in_any_order() {
        let a = file(ver(&[(1, 2)]), 1, 10, 1, false);
        let b = file(ver(&[(2, 2)]), 2, 20, 2, false);
        let c = file(ver(&[(3, 2)]), 3, 15, 3, true);
        let orders = [[0, 1, 2], [2, 1, 0], [1, 0, 2], [2, 0, 1]];
        let all = [&a, &b, &c];
        for o in orders {
            let cands: Vec<_> = o.iter().map(|&i| cand(i as u64 + 1, all[i])).collect();
            assert_eq!(current(&cands), Some(&b), "newest mtime among concurrent, order {o:?}");
        }
    }

    #[test]
    fn a_dominated_version_never_wins_even_if_it_would_win_the_conflict_rule() {
        // x is dominated by y; y is concurrent with z; x would beat z by mtime but must not take part.
        let x = file(ver(&[(1, 1)]), 1, 100, 1, false);
        let y = file(ver(&[(1, 2)]), 1, 1, 2, false);
        let z = file(ver(&[(2, 1)]), 2, 50, 3, false);
        let c = [cand(1, &x), cand(2, &z), cand(3, &y)];
        assert_eq!(current(&c), Some(&z), "z (mtime 50) beats y (mtime 1); x is out");
    }

    #[test]
    fn invalid_copies_take_no_part() {
        let a = file(ver(&[(1, 1)]), 1, 10, 1, false);
        let b = file(ver(&[(1, 2)]), 1, 20, 2, false);
        let c = [cand(1, &a), Candidate { device: ShortId(2), file: &b, invalid: true }];
        assert_eq!(current(&c), Some(&a));
        assert_eq!(holders(&c, &a), vec![ShortId(1)]);
    }

    #[test]
    fn need_follows_causality() {
        let base = file(ver(&[(1, 1)]), 1, 10, 1, false).info;
        let newer = file(ver(&[(1, 2)]), 1, 20, 2, false).info;
        let other = file(ver(&[(1, 1), (2, 1)]), 2, 30, 3, false).info;
        let mine_concurrent = file(ver(&[(1, 2)]), 1, 5, 4, false).info;
        assert_eq!(need(Some(&base), Some(&newer)), Need::Take);
        assert_eq!(need(Some(&newer), Some(&base)), Need::Nothing);
        assert_eq!(need(Some(&base), Some(&base)), Need::Nothing);
        assert_eq!(need(None, Some(&base)), Need::Take);
        assert_eq!(need(Some(&mine_concurrent), Some(&other)), Need::TakeOverConflict);
        let tomb = file(ver(&[(2, 1)]), 2, 1, 0, true).info;
        assert_eq!(need(None, Some(&tomb)), Need::Nothing, "a delete of something we never had");
    }

    #[test]
    fn the_same_change_made_twice_is_no_conflict() {
        let a = file(ver(&[(1, 2)]), 1, 10, 7, false).info;
        let b = file(ver(&[(2, 2)]), 2, 11, 7, false).info;
        assert_eq!(need(Some(&a), Some(&b)), Need::Take, "same content: just take the version");
    }
}
