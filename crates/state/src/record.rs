//! Compact binary records stored in `state.redb`.
//!
//! Every record starts with a version byte. Decoders are bounds-checked and return
//! [`DecodeError`] on any malformed input (records come from disk and are untrusted).
//!
//! # Sizes
//!
//! A [`TorrentRecord`] of a typical torrent (60-byte name, tracker id `tl`, no overrides) encodes
//! to about 90 bytes; the key adds 20. [`Counters`] are 4-12 bytes. A [`ResumeData`] is
//! `piece_count / 8` bytes plus 2 bytes per partial piece plus ~10 bytes per file.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use trs_classify::Category;

use crate::codec::{Reader, Writer};
use crate::duration::Secs;
use crate::error::DecodeError;
use crate::rules::{Priority, SeedingGoal};

/// Written version. Version 1 records (no `stage_root`) are still read.
const TORRENT_V: u8 = 2;
const RESUME_V: u8 = 1;
const COUNTERS_V: u8 = 1;
const PEERS_V: u8 = 1;

/// Longest torrent name stored (bytes).
pub const MAX_NAME: usize = 1024;
/// Longest display title stored (bytes).
pub const MAX_TITLE: usize = 256;
/// Longest id string (profile / rule) stored.
pub const MAX_ID: usize = 64;
/// Longest relative path stored.
pub const MAX_REL_PATH: usize = 4096;
/// Most pieces a resume record may describe.
pub const MAX_PIECES: u32 = 1 << 24;
/// Most partial pieces kept per torrent.
pub const MAX_PARTIAL: usize = 4096;
/// Most files with stamps.
pub const MAX_FILES: usize = 1 << 18;
/// Longest block bitmap of one partial piece (128 MiB piece / 16 KiB blocks / 8).
pub const MAX_BLOCK_MAP: usize = 1024;
/// Most peers cached per torrent.
pub const MAX_PEERS: usize = 256;

/// Bit flags of a torrent; unknown bits are preserved on round trip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TorrentFlags(pub u16);

impl TorrentFlags {
    /// BEP 27 private torrent.
    pub const PRIVATE: u16 = 1;
    /// User paused it.
    pub const PAUSED: u16 = 1 << 1;
    /// All wanted data verified.
    pub const FINISHED: u16 = 1 << 2;
    /// The seeding goal was reached (stopped or about to be removed).
    pub const GOAL_REACHED: u16 = 1 << 3;
    /// The category was chosen by the user, do not re-detect.
    pub const CATEGORY_USER_SET: u16 = 1 << 4;
    /// Files were placed by the organizer.
    pub const ORGANIZED: u16 = 1 << 5;
    /// The torrent waits for the user to choose what to download (SPEC §7A.6); nothing is
    /// announced or allocated for it.
    pub const AWAITING_CHOICE: u16 = 1 << 6;
    /// The user picked this torrent's location ("Move to…", a custom save path): changes of the
    /// app-level or category locations leave it alone.
    pub const LOCATION_PINNED: u16 = 1 << 7;

    /// Is `bit` set?
    pub fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }
    /// Set or clear `bit`.
    pub fn set(&mut self, bit: u16, on: bool) {
        if on { self.0 |= bit } else { self.0 &= !bit }
    }
}

/// Everything persisted about one torrent except counters and resume data.
///
/// The key is the 20-byte info-hash. In memory this is the Cold-tier source of truth: the engine
/// keeps only what it needs per tier.
///
/// size: 184 bytes inline (checked by a test) plus the heap bytes of `name` and the optional
/// strings; on disk about 90 bytes for a typical torrent (see the README).
#[derive(Clone, Debug, PartialEq)]
pub struct TorrentRecord {
    /// Torrent name (info dict `name`), at most 1 KiB.
    pub name: String,
    /// Total size in bytes.
    pub total_size: u64,
    /// Piece count (for resume validation without the `.torrent`).
    pub piece_count: u32,
    /// Number of files.
    pub file_count: u32,
    /// Unix seconds when added.
    pub added_at: i64,
    /// Unix seconds when the download completed.
    pub completed_at: Option<i64>,
    /// Flag bits.
    pub flags: TorrentFlags,
    /// Interned save root (see `Store::intern_root`).
    pub save_root: u32,
    /// Content location below the root; empty = the torrent name.
    pub rel_path: String,
    /// Detected or chosen category.
    pub category: Option<Category>,
    /// Matched tracker profile id.
    pub tracker_profile: Option<String>,
    /// Explicit rule id chosen for this torrent (overrides matching).
    pub rule: Option<String>,
    /// Per-torrent priority override.
    pub priority: Option<Priority>,
    /// Per-torrent goal override ("Seeding Goal ▸ Custom…").
    pub goal: Option<SeedingGoal>,
    /// Bitset of unwanted files (bit i set = file i skipped); `None` = all wanted.
    pub skipped_files: Option<Box<[u8]>>,
    /// Interned staging root for unfinished data (`<root>/<info-hash>/`); `None` = below the
    /// save root's `.incomplete` (version 1 records, and torrents that are not staged).
    pub stage_root: Option<u32>,
    /// Cleaned display title ("Sintel (2010)"), computed once by the engine from the name, the
    /// file list and the category; `None` = not computed yet (records written before it existed).
    pub title: Option<String>,
}

impl TorrentRecord {
    /// A record with the required fields and everything else unset.
    pub fn new(name: impl Into<String>, total_size: u64, added_at: i64) -> Self {
        TorrentRecord {
            name: name.into(),
            total_size,
            piece_count: 0,
            file_count: 1,
            added_at,
            completed_at: None,
            flags: TorrentFlags::default(),
            save_root: 0,
            rel_path: String::new(),
            category: None,
            tracker_profile: None,
            rule: None,
            priority: None,
            goal: None,
            skipped_files: None,
            stage_root: None,
            title: None,
        }
    }

    /// Serialise (version 2). Over-long strings are cut at a char boundary to their cap.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(TORRENT_V);
        w.varint(u64::from(self.flags.0));
        let opts = u8::from(self.completed_at.is_some())
            | u8::from(self.category.is_some()) << 1
            | u8::from(self.tracker_profile.is_some()) << 2
            | u8::from(self.rule.is_some()) << 3
            | u8::from(self.priority.is_some()) << 4
            | u8::from(self.goal.is_some()) << 5
            | u8::from(self.skipped_files.is_some()) << 6
            | u8::from(!self.rel_path.is_empty()) << 7;
        w.u8(opts);
        w.u8(u8::from(self.stage_root.is_some()) | u8::from(self.title.is_some()) << 1);
        w.varint(self.total_size);
        w.varint(u64::from(self.piece_count));
        w.varint(u64::from(self.file_count));
        w.svarint(self.added_at);
        if let Some(t) = self.completed_at {
            w.svarint(t);
        }
        w.varint(u64::from(self.save_root));
        w.bytes(cut(&self.name, MAX_NAME).as_bytes());
        if !self.rel_path.is_empty() {
            w.bytes(cut(&self.rel_path, MAX_REL_PATH).as_bytes());
        }
        if let Some(c) = self.category {
            w.u8(c.index() as u8);
        }
        if let Some(p) = &self.tracker_profile {
            w.bytes(cut(p, MAX_ID).as_bytes());
        }
        if let Some(r) = &self.rule {
            w.bytes(cut(r, MAX_ID).as_bytes());
        }
        if let Some(p) = self.priority {
            w.u8(match p {
                Priority::Low => 0,
                Priority::Normal => 1,
                Priority::High => 2,
            });
        }
        if let Some(g) = &self.goal {
            encode_goal(&mut w, g);
        }
        if let Some(s) = &self.skipped_files {
            w.bytes(s);
        }
        if let Some(s) = self.stage_root {
            w.varint(u64::from(s));
        }
        if let Some(t) = &self.title {
            w.bytes(cut(t, MAX_TITLE).as_bytes());
        }
        w.0
    }

    /// Parse a record. Never panics.
    pub fn decode(b: &[u8]) -> Result<TorrentRecord, DecodeError> {
        let mut r = Reader::new(b);
        let version = r.u8()?;
        if version != 1 && version != TORRENT_V {
            return Err(DecodeError("unknown torrent record version"));
        }
        let flags = TorrentFlags(u16::try_from(r.varint()?).map_err(|_| DecodeError("flags overflow"))?);
        let opts = r.u8()?;
        let opts2 = if version >= 2 { r.u8()? } else { 0 };
        let total_size = r.varint()?;
        let piece_count = r.u32()?;
        let file_count = r.u32()?;
        let added_at = r.svarint()?;
        let completed_at = if opts & 1 != 0 { Some(r.svarint()?) } else { None };
        let save_root = r.u32()?;
        let name = r.string(MAX_NAME)?.to_string();
        let rel_path = if opts & 0x80 != 0 { r.string(MAX_REL_PATH)?.to_string() } else { String::new() };
        let category = if opts & 2 != 0 {
            Some(*Category::ALL.get(usize::from(r.u8()?)).ok_or(DecodeError("bad category"))?)
        } else {
            None
        };
        let tracker_profile = if opts & 4 != 0 { Some(r.string(MAX_ID)?.to_string()) } else { None };
        let rule = if opts & 8 != 0 { Some(r.string(MAX_ID)?.to_string()) } else { None };
        let priority = if opts & 16 != 0 {
            Some(match r.u8()? {
                0 => Priority::Low,
                1 => Priority::Normal,
                2 => Priority::High,
                _ => return Err(DecodeError("bad priority")),
            })
        } else {
            None
        };
        let goal = if opts & 32 != 0 { Some(decode_goal(&mut r)?) } else { None };
        let skipped_files = if opts & 64 != 0 {
            Some(r.bytes(MAX_FILES.div_ceil(8))?.to_vec().into_boxed_slice())
        } else {
            None
        };
        let stage_root = if opts2 & 1 != 0 { Some(r.u32()?) } else { None };
        let title = if opts2 & 2 != 0 { Some(r.string(MAX_TITLE)?.to_string()) } else { None };
        r.finish()?;
        Ok(TorrentRecord {
            name,
            total_size,
            piece_count,
            file_count,
            added_at,
            completed_at,
            flags,
            save_root,
            rel_path,
            category,
            tracker_profile,
            rule,
            priority,
            goal,
            skipped_files,
            stage_root,
            title,
        })
    }
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

fn encode_goal(w: &mut Writer, g: &SeedingGoal) {
    match *g {
        SeedingGoal::StopWhenFinished => w.u8(0),
        SeedingGoal::Ratio { ratio } => {
            w.u8(1);
            w.f64(ratio);
        }
        SeedingGoal::Time { time } => {
            w.u8(2);
            w.varint(time.0);
        }
        SeedingGoal::RatioOrTime { ratio, time } => {
            w.u8(3);
            w.f64(ratio);
            w.varint(time.0);
        }
        SeedingGoal::RatioAndTime { ratio, time } => {
            w.u8(4);
            w.f64(ratio);
            w.varint(time.0);
        }
        SeedingGoal::Forever => w.u8(5),
    }
}

fn decode_goal(r: &mut Reader<'_>) -> Result<SeedingGoal, DecodeError> {
    let secs = |r: &mut Reader<'_>| -> Result<Secs, DecodeError> {
        let v = r.varint()?;
        if v > crate::duration::MAX_SECS { Err(DecodeError("goal time out of range")) } else { Ok(Secs(v)) }
    };
    let ratio = |r: &mut Reader<'_>| -> Result<f64, DecodeError> {
        let v = r.f64()?;
        if v.is_finite() { Ok(v) } else { Err(DecodeError("goal ratio not finite")) }
    };
    Ok(match r.u8()? {
        0 => SeedingGoal::StopWhenFinished,
        1 => SeedingGoal::Ratio { ratio: ratio(r)? },
        2 => SeedingGoal::Time { time: secs(r)? },
        3 => {
            let ra = ratio(r)?;
            SeedingGoal::RatioOrTime { ratio: ra, time: secs(r)? }
        }
        4 => {
            let ra = ratio(r)?;
            SeedingGoal::RatioAndTime { ratio: ra, time: secs(r)? }
        }
        5 => SeedingGoal::Forever,
        _ => return Err(DecodeError("bad goal tag")),
    })
}

/// Lifetime counters of one torrent. Only ever grow (see `Store::add_counters`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Payload bytes downloaded, lifetime.
    pub downloaded: u64,
    /// Payload bytes uploaded, lifetime.
    pub uploaded: u64,
    /// Seconds seeded, lifetime.
    pub seed_secs: u64,
}

impl Counters {
    /// Saturating add.
    pub fn add(&mut self, o: &Counters) {
        self.downloaded = self.downloaded.saturating_add(o.downloaded);
        self.uploaded = self.uploaded.saturating_add(o.uploaded);
        self.seed_secs = self.seed_secs.saturating_add(o.seed_secs);
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(COUNTERS_V);
        w.varint(self.downloaded);
        w.varint(self.uploaded);
        w.varint(self.seed_secs);
        w.0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Counters, DecodeError> {
        let mut r = Reader::new(b);
        if r.u8()? != COUNTERS_V {
            return Err(DecodeError("unknown counters version"));
        }
        let c = Counters { downloaded: r.varint()?, uploaded: r.varint()?, seed_secs: r.varint()? };
        r.finish()?;
        Ok(c)
    }
}

/// Size and modification time of one file when the resume data was written. A mismatch on
/// restart means "recheck this torrent" (SPEC §10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStamp {
    /// Size in bytes.
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub mtime_ns: i64,
}

/// A piece that is partly downloaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialPiece {
    /// Piece index.
    pub index: u32,
    /// Bitmap of received blocks (bit i = block i), at most [`MAX_BLOCK_MAP`] bytes.
    pub blocks: Box<[u8]>,
}

/// Resume data of an incomplete torrent: verified pieces, partial pieces, file stamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeData {
    /// Number of pieces.
    pub piece_count: u32,
    /// Verified-piece bitfield, `ceil(piece_count / 8)` bytes, MSB first, spare bits zero.
    pub bitfield: Box<[u8]>,
    /// Partially downloaded pieces, sorted by index, at most [`MAX_PARTIAL`].
    pub partial: Vec<PartialPiece>,
    /// One stamp per file (may be empty if the engine does not track them).
    pub files: Vec<FileStamp>,
    /// Unix seconds when written.
    pub saved_at: i64,
}

impl ResumeData {
    /// Check the invariants the encoder and decoder rely on.
    pub fn check(&self) -> Result<(), DecodeError> {
        if self.piece_count > MAX_PIECES {
            return Err(DecodeError("too many pieces"));
        }
        let want = (self.piece_count as usize).div_ceil(8);
        if self.bitfield.len() != want {
            return Err(DecodeError("bitfield length does not match piece count"));
        }
        let spare = (want * 8 - self.piece_count as usize) as u32;
        if spare > 0 && self.bitfield.last().is_some_and(|b| b & ((1u8 << spare) - 1) != 0) {
            return Err(DecodeError("spare bitfield bits set"));
        }
        if self.partial.len() > MAX_PARTIAL || self.files.len() > MAX_FILES {
            return Err(DecodeError("too many partial pieces or files"));
        }
        let mut prev: Option<u32> = None;
        for p in &self.partial {
            if p.index >= self.piece_count || prev.is_some_and(|q| p.index <= q) || p.blocks.len() > MAX_BLOCK_MAP {
                return Err(DecodeError("partial pieces must be unique, ascending and in range"));
            }
            prev = Some(p.index);
        }
        Ok(())
    }

    /// Serialise. Fails if the invariants of [`ResumeData::check`] do not hold.
    pub fn encode(&self) -> Result<Vec<u8>, DecodeError> {
        self.check()?;
        let mut w = Writer::new();
        w.u8(RESUME_V);
        w.varint(u64::from(self.piece_count));
        w.svarint(self.saved_at);
        w.raw(&self.bitfield);
        w.varint(self.partial.len() as u64);
        let mut last = 0u32;
        for p in &self.partial {
            w.varint(u64::from(p.index - last)); // delta: partial pieces cluster
            last = p.index;
            w.bytes(&p.blocks);
        }
        w.varint(self.files.len() as u64);
        for f in &self.files {
            w.varint(f.size);
            w.svarint(f.mtime_ns);
        }
        Ok(w.0)
    }

    /// Parse and validate. Never panics.
    pub fn decode(b: &[u8]) -> Result<ResumeData, DecodeError> {
        let mut r = Reader::new(b);
        if r.u8()? != RESUME_V {
            return Err(DecodeError("unknown resume version"));
        }
        let piece_count = r.u32()?;
        if piece_count > MAX_PIECES {
            return Err(DecodeError("too many pieces"));
        }
        let saved_at = r.svarint()?;
        let bitfield = r.raw((piece_count as usize).div_ceil(8))?.to_vec().into_boxed_slice();
        let n = r.varint()?;
        if n > MAX_PARTIAL as u64 {
            return Err(DecodeError("too many partial pieces"));
        }
        let mut partial = Vec::with_capacity(n as usize);
        let mut last = 0u32;
        for i in 0..n {
            let delta = r.u32()?;
            let index = last.checked_add(delta).ok_or(DecodeError("piece index overflow"))?;
            if i > 0 && delta == 0 {
                return Err(DecodeError("duplicate partial piece"));
            }
            last = index;
            partial.push(PartialPiece { index, blocks: r.bytes(MAX_BLOCK_MAP)?.to_vec().into_boxed_slice() });
        }
        let nf = r.varint()?;
        if nf > MAX_FILES as u64 {
            return Err(DecodeError("too many files"));
        }
        let mut files = Vec::with_capacity((nf as usize).min(r.remaining()));
        for _ in 0..nf {
            files.push(FileStamp { size: r.varint()?, mtime_ns: r.svarint()? });
        }
        r.finish()?;
        let d = ResumeData { piece_count, bitfield, partial, files, saved_at };
        d.check()?;
        Ok(d)
    }
}

/// Encode a bounded peer list: `[version][count]` then per peer `tag(4|6) addr port`.
/// Extra peers beyond [`MAX_PEERS`] are dropped.
pub(crate) fn encode_peers(peers: &[SocketAddr]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(PEERS_V);
    let peers = &peers[..peers.len().min(MAX_PEERS)];
    w.varint(peers.len() as u64);
    for p in peers {
        match p.ip() {
            IpAddr::V4(a) => {
                w.u8(4);
                w.raw(&a.octets());
            }
            IpAddr::V6(a) => {
                w.u8(6);
                w.raw(&a.octets());
            }
        }
        w.raw(&p.port().to_be_bytes());
    }
    w.0
}

pub(crate) fn decode_peers(b: &[u8]) -> Result<Vec<SocketAddr>, DecodeError> {
    let mut r = Reader::new(b);
    if r.u8()? != PEERS_V {
        return Err(DecodeError("unknown peers version"));
    }
    let n = r.varint()?;
    if n > MAX_PEERS as u64 {
        return Err(DecodeError("too many peers"));
    }
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let ip = match r.u8()? {
            4 => {
                let o = r.raw(4)?;
                IpAddr::V4(Ipv4Addr::new(o[0], o[1], o[2], o[3]))
            }
            6 => {
                let mut o = [0u8; 16];
                o.copy_from_slice(r.raw(16)?);
                IpAddr::V6(Ipv6Addr::from(o))
            }
            _ => return Err(DecodeError("bad address family")),
        };
        let p = r.raw(2)?;
        out.push(SocketAddr::new(ip, u16::from_be_bytes([p[0], p[1]])));
    }
    r.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> TorrentRecord {
        let mut flags = TorrentFlags::default();
        flags.set(TorrentFlags::PRIVATE, true);
        flags.set(TorrentFlags::FINISHED, true);
        TorrentRecord {
            name: "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP".into(),
            total_size: 8_000_000_000,
            piece_count: 7630,
            file_count: 3,
            added_at: 1_791_000_000,
            completed_at: Some(1_791_003_600),
            flags,
            save_root: 7,
            rel_path: "Movies/Night of the Living Dead (1968)".into(),
            category: Some(Category::Movie),
            tracker_profile: Some("example".into()),
            rule: Some("tl-high".into()),
            priority: Some(Priority::High),
            goal: Some(SeedingGoal::RatioOrTime { ratio: 2.5, time: Secs::days(14) }),
            skipped_files: Some(vec![0b100, 0].into_boxed_slice()),
            stage_root: Some(9),
            title: Some("Night of the Living Dead (1968)".into()),
        }
    }

    #[test]
    fn torrent_roundtrip_full_and_minimal() {
        let r = full();
        assert_eq!(TorrentRecord::decode(&r.encode()), Ok(r));
        let m = TorrentRecord::new("x", 1, -5);
        assert_eq!(TorrentRecord::decode(&m.encode()), Ok(m));
    }

    #[test]
    fn version_1_records_are_still_read() {
        // Hand-built version 1 bytes: no `opts2` byte, no stage root.
        let mut w = Writer::new();
        w.u8(1);
        w.varint(u64::from(TorrentFlags::PRIVATE));
        w.u8(0);
        w.varint(1000);
        w.varint(3);
        w.varint(1);
        w.svarint(5);
        w.varint(2);
        w.bytes(b"old");
        let r = TorrentRecord::decode(&w.0).unwrap();
        assert_eq!((r.name.as_str(), r.total_size, r.save_root, r.stage_root), ("old", 1000, 2, None));
        assert!(r.flags.has(TorrentFlags::PRIVATE));
        // Re-encoding writes version 2.
        assert_eq!(r.encode()[0], 2);
        assert_eq!(TorrentRecord::decode(&r.encode()), Ok(r));
    }

    #[test]
    fn records_without_a_title_still_read_and_the_title_survives_a_round_trip() {
        // A record written before titles existed: the title bit is clear.
        let mut old = full();
        old.title = None;
        let back = TorrentRecord::decode(&old.encode()).unwrap();
        assert_eq!(back.title, None);
        let mut with = old.clone();
        with.title = Some("Sintel (2010)".into());
        assert_eq!(TorrentRecord::decode(&with.encode()).unwrap().title.as_deref(), Some("Sintel (2010)"));
        // Over-long titles are cut to the cap on a char boundary.
        with.title = Some("é".repeat(MAX_TITLE));
        let cut = TorrentRecord::decode(&with.encode()).unwrap().title.unwrap();
        assert!(cut.len() <= MAX_TITLE && cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn in_memory_size_stays_within_the_cold_tier_budget() {
        // docs/ENGINE.md §3: Cold <= 256 B + name. The record holds the name and a few short strings on the heap.
        let n = std::mem::size_of::<TorrentRecord>();
        eprintln!("size_of::<TorrentRecord>() = {n}");
        assert!(n <= 256, "TorrentRecord is {n} bytes");
    }

    #[test]
    fn typical_record_is_small() {
        let mut r = TorrentRecord::new("Ubuntu 26.04 Desktop arm64 ISO image release 2026.04 final", 6_000_000_000, 1_791_000_000);
        r.piece_count = 2863;
        r.file_count = 1;
        r.save_root = 1;
        r.tracker_profile = Some("tl".into());
        r.category = Some(Category::Software);
        r.completed_at = Some(1_791_003_600);
        let n = r.encode().len();
        assert!(n <= 100, "typical record is {n} bytes");
    }

    #[test]
    fn torrent_decode_rejects_garbage_without_panicking() {
        let good = full().encode();
        for cut in 0..good.len() {
            assert!(TorrentRecord::decode(&good[..cut]).is_err(), "prefix {cut}");
        }
        let mut longer = good.clone();
        longer.push(7);
        assert!(TorrentRecord::decode(&longer).is_err());
        let mut v3 = good.clone();
        v3[0] = 3;
        assert!(TorrentRecord::decode(&v3).is_err());
        // Every single-byte mutation either decodes or errors; it must not panic.
        for i in 0..good.len() {
            for delta in [1u8, 0x80, 0xff] {
                let mut m = good.clone();
                m[i] = m[i].wrapping_add(delta);
                let _ = TorrentRecord::decode(&m);
            }
        }
    }

    #[test]
    fn long_strings_are_cut_on_char_boundaries() {
        let r = TorrentRecord::new("é".repeat(2000), 1, 1);
        let d = TorrentRecord::decode(&r.encode()).unwrap();
        assert_eq!(d.name.len(), MAX_NAME);
        assert!(d.name.chars().all(|c| c == 'é'));
    }

    #[test]
    fn counters_roundtrip() {
        let c = Counters { downloaded: u64::MAX, uploaded: 1, seed_secs: 300 };
        assert_eq!(Counters::decode(&c.encode()), Ok(c));
        assert!(Counters::decode(&[]).is_err());
    }

    fn resume() -> ResumeData {
        ResumeData {
            piece_count: 20,
            bitfield: vec![0b1111_0000, 0b0000_1111, 0b0000_0000].into_boxed_slice(),
            partial: vec![
                PartialPiece { index: 4, blocks: vec![0b11].into_boxed_slice() },
                PartialPiece { index: 19, blocks: vec![0xff, 0x01].into_boxed_slice() },
            ],
            files: vec![FileStamp { size: 100, mtime_ns: 1_791_000_000_000_000_000 }; 2],
            saved_at: 1_791_000_000,
        }
    }

    #[test]
    fn resume_roundtrip_and_validation() {
        let r = resume();
        assert_eq!(ResumeData::decode(&r.encode().unwrap()), Ok(r.clone()));
        let mut bad = r.clone();
        bad.bitfield = vec![0; 2].into_boxed_slice();
        assert!(bad.encode().is_err(), "length mismatch");
        let mut bad = r.clone();
        bad.bitfield[2] = 1; // spare bit
        assert!(bad.encode().is_err());
        let mut bad = r.clone();
        bad.partial.swap(0, 1);
        assert!(bad.encode().is_err(), "unsorted");
        let mut bad = r;
        bad.partial[0].index = 20;
        assert!(bad.encode().is_err(), "out of range");
    }

    #[test]
    fn resume_decode_never_panics_on_mutations() {
        let good = resume().encode().unwrap();
        for cut in 0..good.len() {
            assert!(ResumeData::decode(&good[..cut]).is_err());
        }
        for i in 0..good.len() {
            for delta in [1u8, 0x7f, 0xff] {
                let mut m = good.clone();
                m[i] = m[i].wrapping_add(delta);
                let _ = ResumeData::decode(&m);
            }
        }
        // Declared sizes far beyond the data must not allocate.
        let mut huge = vec![RESUME_V];
        huge.extend_from_slice(&[0xff, 0xff, 0xff, 0x07]); // piece_count 2^25-1 > MAX
        assert!(ResumeData::decode(&huge).is_err());
    }

    #[test]
    fn peers_roundtrip_and_cap() {
        let peers: Vec<SocketAddr> = vec!["1.2.3.4:6881".parse().unwrap(), "[2001:db8::1]:51413".parse().unwrap()];
        assert_eq!(decode_peers(&encode_peers(&peers)), Ok(peers.clone()));
        let many: Vec<SocketAddr> = (0..1000u16).map(|i| SocketAddr::from(([10, 0, (i >> 8) as u8, i as u8], 1000))).collect();
        assert_eq!(decode_peers(&encode_peers(&many)).unwrap().len(), MAX_PEERS);
        assert!(decode_peers(&[PEERS_V, 5, 4]).is_err());
    }
}
