//! The file model every device agrees on: version vectors, file entries and their chunks.
//!
//! A [`FileInfo`] is one version of one path. Its [`Version`] says which changes it has seen; two
//! versions that have not seen each other are [`Causality::Concurrent`], a conflict. The rules are
//! Syncthing's (BEP v1), so their behaviour in the field carries over.

use std::cmp::Ordering;

use crate::ids::ShortId;

/// One device's counter in a [`Version`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Counter {
    /// The device that made the changes counted here.
    pub id: ShortId,
    /// Never 0 (a device with no changes has no counter).
    pub value: u64,
}

/// A version vector: counters sorted by device, at most one per device, none zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version {
    counters: Vec<Counter>,
}

/// How two versions relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Causality {
    /// The same changes.
    Equal,
    /// `self` has seen every change of `other`, and more.
    Greater,
    /// `other` has seen every change of `self`, and more.
    Lesser,
    /// Each has changes the other has not seen: a conflict if the content differs.
    Concurrent,
}

/// Counters that are not sorted, repeat a device or hold 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadVersion;

/// Most devices a version may name (a folder of more devices than this is refused on the wire).
pub const MAX_COUNTERS: usize = 256;

impl Version {
    /// The empty version (nothing seen yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// From counters as they come off the wire: sorted by id, unique, none zero, at most [`MAX_COUNTERS`].
    pub fn from_counters(counters: Vec<Counter>) -> Result<Self, BadVersion> {
        let sorted = counters.windows(2).all(|w| w[0].id < w[1].id);
        if !sorted || counters.iter().any(|c| c.value == 0) || counters.len() > MAX_COUNTERS {
            return Err(BadVersion);
        }
        Ok(Version { counters })
    }

    /// The counters, sorted by device.
    pub fn counters(&self) -> &[Counter] {
        &self.counters
    }

    /// No counters.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// The counter of `id`, 0 when it has none.
    pub fn get(&self, id: ShortId) -> u64 {
        match self.counters.binary_search_by_key(&id, |c| c.id) {
            Ok(i) => self.counters[i].value,
            Err(_) => 0,
        }
    }

    /// This version plus one local change by `id` at `now_s` (Unix seconds). The counter becomes
    /// `max(old + 1, now_s)`: still a counter, but roughly a clock too, so a device that lost its database
    /// and starts again from 0 does not reuse old values (Syncthing's `updateWithNow`).
    #[must_use]
    pub fn bump(&self, id: ShortId, now_s: u64) -> Version {
        let mut counters = self.counters.clone();
        match counters.binary_search_by_key(&id, |c| c.id) {
            Ok(i) => counters[i].value = (counters[i].value.saturating_add(1)).max(now_s),
            Err(i) => counters.insert(i, Counter { id, value: now_s.max(1) }),
        }
        Version { counters }
    }

    /// Every change either version has seen (the counter-wise maximum).
    #[must_use]
    pub fn merge(&self, other: &Version) -> Version {
        let (mut a, mut b) = (self.counters.iter().peekable(), other.counters.iter().peekable());
        let mut out = Vec::with_capacity(self.counters.len().max(other.counters.len()));
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => match x.id.cmp(&y.id) {
                    Ordering::Less => a.next().copied(),
                    Ordering::Greater => b.next().copied(),
                    Ordering::Equal => {
                        let v = x.value.max(y.value);
                        let id = x.id;
                        a.next();
                        b.next();
                        Some(Counter { id, value: v })
                    }
                },
                (Some(_), None) => a.next().copied(),
                (None, Some(_)) => b.next().copied(),
                (None, None) => None,
            };
            match next {
                Some(c) => out.push(c),
                None => break,
            }
        }
        Version { counters: out }
    }

    /// How `self` relates to `other`.
    pub fn compare(&self, other: &Version) -> Causality {
        let (mut ahead, mut behind) = (false, false);
        let (mut a, mut b) = (self.counters.iter().peekable(), other.counters.iter().peekable());
        loop {
            match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => match x.id.cmp(&y.id) {
                    Ordering::Less => {
                        ahead = true;
                        a.next();
                    }
                    Ordering::Greater => {
                        behind = true;
                        b.next();
                    }
                    Ordering::Equal => {
                        match x.value.cmp(&y.value) {
                            Ordering::Greater => ahead = true,
                            Ordering::Less => behind = true,
                            Ordering::Equal => {}
                        }
                        a.next();
                        b.next();
                    }
                },
                (Some(_), None) => {
                    ahead = true;
                    a.next();
                }
                (None, Some(_)) => {
                    behind = true;
                    b.next();
                }
                (None, None) => break,
            }
            if ahead && behind {
                return Causality::Concurrent;
            }
        }
        match (ahead, behind) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Greater,
            (false, true) => Causality::Lesser,
            (true, true) => Causality::Concurrent,
        }
    }

    /// `self` has seen every change of `other` (equal or greater).
    pub fn dominates(&self, other: &Version) -> bool {
        matches!(self.compare(other), Causality::Equal | Causality::Greater)
    }
}

/// What a path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (synced as a link, never followed).
    Symlink,
}

/// One content-defined chunk of a file: its length and SHA-256. Offsets follow from the lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkRef {
    /// Bytes in the chunk (at most [`MAX_CHUNK`]).
    pub len: u32,
    /// SHA-256 of the chunk's bytes.
    pub hash: [u8; 32],
}

/// Largest chunk any tier makes (the 4 MiB average tier's maximum).
pub const MAX_CHUNK: u32 = 16 << 20;

/// One version of one path, as a writer made it. Everything here is covered by the writer's signature
/// ([`crate::sign`]); the sequence a device announces it under is not (see [`IndexEntry`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Relative path: NFC, `/` between components (see [`validate_path`]).
    pub path: String,
    /// File, directory or symlink.
    pub kind: FileKind,
    /// Bytes (0 for directories, symlinks and tombstones).
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub mtime_ns: i64,
    /// The executable bit (the only permission synced).
    pub exec: bool,
    /// A tombstone: the path was deleted at this version.
    pub deleted: bool,
    /// What this version has seen.
    pub version: Version,
    /// The device that made this version; its writer key signs it.
    pub modified_by: ShortId,
    /// Random id given when the file was created and kept across renames: a hint for detecting moves,
    /// never needed for correctness.
    pub file_id: [u8; 16],
    /// For a symlink, its target (as stored, never resolved). Empty otherwise.
    pub symlink_target: String,
    /// For a conflict copy, the path it was a conflict of.
    pub conflict_of: Option<String>,
    /// The chunks, in order (empty for directories, symlinks and tombstones).
    pub chunks: Vec<ChunkRef>,
}

impl FileInfo {
    /// The same content as `other` (kind, size, chunks, exec bit, link target, deleted): two devices
    /// that made the same change concurrently are not in conflict.
    pub fn same_content(&self, other: &FileInfo) -> bool {
        self.kind == other.kind
            && self.deleted == other.deleted
            && self.size == other.size
            && self.exec == other.exec
            && self.symlink_target == other.symlink_target
            && self.chunks == other.chunks
    }

    /// Of two concurrent versions, does `self` win? In this order, so every device picks the same
    /// winner without talking:
    /// 1. a change beats a delete (the file comes back);
    /// 2. a directory beats a file or symlink (its files stay where they are; the file becomes a
    ///    conflict copy beside it);
    /// 3. the newer mtime;
    /// 4. the higher device id.
    pub fn wins_conflict(&self, other: &FileInfo) -> bool {
        if self.deleted != other.deleted {
            return !self.deleted;
        }
        let (dir, other_dir) = (self.kind == FileKind::Dir, other.kind == FileKind::Dir);
        if dir != other_dir {
            return dir;
        }
        match self.mtime_ns.cmp(&other.mtime_ns) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => self.modified_by > other.modified_by,
        }
    }
}

/// A [`FileInfo`] with its writer's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedFile {
    /// The version.
    pub info: FileInfo,
    /// Ed25519 signature by `info.modified_by`'s device key over [`crate::sign::file_payload`].
    pub sig: [u8; 64],
}

/// A signed file as one device announces it: under that device's own sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// The announcing device's change counter at which it took this version.
    pub sequence: u64,
    /// The announcer has this path, but its copy differs from this version: do not request chunks of it from
    /// this device. A read-only device sets it on a file it changed locally (it cannot sign its own version).
    /// Like `sequence`, this belongs to the announcer and is not covered by the signature.
    pub invalid: bool,
    /// The version.
    pub file: SignedFile,
}

/// Why a path is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadPath {
    /// Empty, or longer than [`MAX_PATH`] bytes.
    Length,
    /// Starts or ends with `/`, or has an empty component.
    Empty,
    /// A `.` or `..` component.
    Dots,
    /// A NUL, a backslash or a control character.
    Character,
    /// A component longer than 255 bytes.
    Component,
    /// Inside `.seedwell/`, the folder's own state.
    Reserved,
}

/// Longest path, in bytes.
pub const MAX_PATH: usize = 4096;

/// The folder's own directory (marker, archive, temp files); never synced.
pub const STATE_DIR: &str = ".seedwell";

/// Checks a relative path from the wire or a scan: it must stay inside the folder.
/// (NFC is checked by the receiver against its normalizer; this crate has no Unicode tables.)
pub fn validate_path(p: &str) -> Result<(), BadPath> {
    if p.is_empty() || p.len() > MAX_PATH {
        return Err(BadPath::Length);
    }
    for c in p.split('/') {
        if c.is_empty() {
            return Err(BadPath::Empty);
        }
        if c == "." || c == ".." {
            return Err(BadPath::Dots);
        }
        if c.len() > 255 {
            return Err(BadPath::Component);
        }
        if c.chars().any(|ch| ch == '\0' || ch == '\\' || ch.is_control()) {
            return Err(BadPath::Character);
        }
    }
    let first = p.split('/').next().unwrap_or(p);
    if first == STATE_DIR {
        return Err(BadPath::Reserved);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(pairs: &[(u64, u64)]) -> Version {
        Version::from_counters(pairs.iter().map(|&(id, value)| Counter { id: ShortId(id), value }).collect())
            .unwrap()
    }

    #[test]
    fn compare_covers_every_case() {
        assert_eq!(v(&[]).compare(&v(&[])), Causality::Equal);
        assert_eq!(v(&[(1, 2)]).compare(&v(&[(1, 2)])), Causality::Equal);
        assert_eq!(v(&[(1, 3)]).compare(&v(&[(1, 2)])), Causality::Greater);
        assert_eq!(v(&[(1, 2), (2, 1)]).compare(&v(&[(1, 2)])), Causality::Greater);
        assert_eq!(v(&[(1, 2)]).compare(&v(&[(1, 2), (2, 1)])), Causality::Lesser);
        assert_eq!(v(&[(1, 3)]).compare(&v(&[(1, 2), (2, 1)])), Causality::Concurrent);
        assert_eq!(v(&[(2, 1)]).compare(&v(&[(1, 1)])), Causality::Concurrent);
        assert_eq!(v(&[]).compare(&v(&[(1, 1)])), Causality::Lesser);
    }

    #[test]
    fn bump_is_a_counter_and_roughly_a_clock() {
        let a = ShortId(7);
        let one = v(&[]).bump(a, 1000);
        assert_eq!(one.get(a), 1000, "a new counter starts at the clock");
        assert_eq!(one.bump(a, 1000).get(a), 1001, "same second: still increases");
        assert_eq!(one.bump(a, 5000).get(a), 5000, "jumps to the clock");
        assert_eq!(one.bump(a, 10).get(a), 1001, "a clock that went back never decreases the counter");
        assert_eq!(v(&[]).bump(a, 0).get(a), 1, "never 0");
        assert_eq!(one.bump(ShortId(3), 9).counters()[0].id, ShortId(3), "stays sorted");
    }

    #[test]
    fn merge_takes_the_maximum_and_dominates_both() {
        let a = v(&[(1, 5), (3, 1)]);
        let b = v(&[(1, 2), (2, 4)]);
        let m = a.merge(&b);
        assert_eq!(m, v(&[(1, 5), (2, 4), (3, 1)]));
        assert!(m.dominates(&a) && m.dominates(&b));
    }

    #[test]
    fn wire_counters_are_checked() {
        let c = |id, value| Counter { id: ShortId(id), value };
        assert!(Version::from_counters(vec![c(2, 1), c(1, 1)]).is_err(), "unsorted");
        assert!(Version::from_counters(vec![c(1, 1), c(1, 2)]).is_err(), "repeated");
        assert!(Version::from_counters(vec![c(1, 0)]).is_err(), "zero");
        assert!(Version::from_counters(vec![c(1, 1), c(2, 1)]).is_ok());
    }

    fn file(mtime_ns: i64, by: u64, deleted: bool) -> FileInfo {
        FileInfo {
            path: "a".into(),
            kind: FileKind::File,
            size: 0,
            mtime_ns,
            exec: false,
            deleted,
            version: v(&[(by, 1)]),
            modified_by: ShortId(by),
            file_id: [0; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks: vec![],
        }
    }

    #[test]
    fn conflict_winner_is_the_same_from_both_sides() {
        let (a, b) = (file(10, 1, false), file(20, 2, false));
        assert!(b.wins_conflict(&a) && !a.wins_conflict(&b), "newer mtime");
        let (a, b) = (file(10, 1, false), file(10, 2, false));
        assert!(b.wins_conflict(&a) && !a.wins_conflict(&b), "higher device on a tie");
        let (a, b) = (file(10, 1, false), file(99, 2, true));
        assert!(a.wins_conflict(&b) && !b.wins_conflict(&a), "a change beats a delete");
        let (f, mut d) = (file(99, 9, false), file(0, 1, false));
        d.kind = FileKind::Dir;
        assert!(d.wins_conflict(&f) && !f.wins_conflict(&d), "a directory beats a file, whatever the mtime");
        let mut dead = file(99, 9, true);
        dead.kind = FileKind::Dir;
        assert!(f.wins_conflict(&dead) && !dead.wins_conflict(&f), "a change still beats a deleted directory");
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        for ok in ["a", "a/b.txt", "Æøå/ü", ".hidden", "a/.seedwell"] {
            assert_eq!(validate_path(ok), Ok(()), "{ok}");
        }
        assert_eq!(validate_path(""), Err(BadPath::Length));
        assert_eq!(validate_path("/a"), Err(BadPath::Empty));
        assert_eq!(validate_path("a//b"), Err(BadPath::Empty));
        assert_eq!(validate_path("a/"), Err(BadPath::Empty));
        assert_eq!(validate_path("../a"), Err(BadPath::Dots));
        assert_eq!(validate_path("a/./b"), Err(BadPath::Dots));
        assert_eq!(validate_path("a\\b"), Err(BadPath::Character));
        assert_eq!(validate_path("a\0"), Err(BadPath::Character));
        assert_eq!(validate_path(&"x".repeat(256)), Err(BadPath::Component));
        assert_eq!(validate_path(".seedwell/Archive/x"), Err(BadPath::Reserved));
        assert_eq!(validate_path(".seedwell"), Err(BadPath::Reserved));
    }
}
