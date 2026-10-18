//! [`Store`] over `trs-state`: the engine's persistence in `state.redb` plus the `torrents/`,
//! `placement/` and `downloads/` folders of the state directory.
//!
//! # How the engine's records map onto `trs_state::TorrentRecord`
//!
//! `trs-state` knows torrents, not the engine's internals, so a few fields ride along in places
//! the record already offers (the format is unchanged):
//!
//! | Engine field | Where it is stored |
//! |---|---|
//! | has metadata, added complete, staged, complete, placed | spare bits 8..15 of `TorrentFlags` (unknown bits survive round trips) |
//! | waiting for a choice, location pinned by the user | `TorrentFlags::AWAITING_CHOICE` / `LOCATION_PINNED` (bits 6, 7) |
//! | staging root of unfinished data | `TorrentRecord::stage_root` (record version 2, an interned root like `save_root`) |
//! | magnet URI (only while the metadata is missing) | `rel_path` |
//! | announce tiers, piece length, wanted bytes | read back from `torrents/<hash>.torrent` at load |
//! | priority / seed goal | `priority` / `goal`, only when the user chose them (`Some`); otherwise the rules decide |
//! | skipped files | `skipped_files` bitset (Low/High per-file priorities are not persisted) |
//! | queue position, last activity, error start | `torrent-extra.bin` next to the database (losing it falls back to the order of adding) |
//! | per-file paths of placed torrents | `placement/<hash>.bin` |
//! | direct downloads | `downloads/<id>.rec` (the resumable bytes are trs-http's own `.part` + sidecar) |
//! | launch history for the crash-loop guard | `launches.log` |
//!
//! Resume data of a complete torrent is stored as `piece_count = 0` plus the file stamps.
//! Statistics deltas become `record_stats` calls on the right bucket (profile id, `@public` or
//! `@other-private`) and the local calendar day.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use trs_classify::Category;
use trs_metainfo::{Metainfo, MAX_FILES};
use trs_state::record::{FileStamp, ResumeData, TorrentFlags, TorrentRecord as SRecord};
use trs_state::{Counters, Day, StatsBucket, StatsScope};
use trs_tracker::store::TimingStore;

use crate::api::{SeedGoal, TorrentId};
use crate::persist::{
    Batch, DirectRecord, DirectState, LifetimeCounters, Placed, Placement, ResumeRecord, StatDelta, Store,
    StoreError, StoredState, StoredTorrent, TorrentRecord,
};
use crate::policy::{goal_from_state, state_goal, state_priority};

// Spare bits of `TorrentFlags` (bits 0..5 belong to trs-state).
const E_META: u16 = 1 << 8;
const E_ADDED_COMPLETE: u16 = 1 << 9;
const E_STAGED: u16 = 1 << 10;
const E_COMPLETE: u16 = 1 << 11;
const E_RENAMED: u16 = 1 << 12;
// Bits 13..15 were "hardlinks pending", "hardlinks done" and "keep own layout" (retired with the
// flat layout, SPEC 7A.3): old records may carry them; they are ignored.

/// Largest placement / direct record file we read back (they live on disk and are untrusted).
const MAX_SIDE_FILE: u64 = 64 << 20;

fn se(e: impl std::fmt::Display) -> StoreError {
    StoreError(e.to_string())
}

/// Per-torrent values that have no field in `trs_state::TorrentRecord`: 33 bytes + padding each.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Extra {
    queue_pos: u32,
    last_activity: u32,
    error_since: u32,
    error_tracker: bool,
    /// Held for the grace period before an auto-add starts (byte 33, bit 0).
    pending_start: bool,
}

const EXTRA_LEN: usize = 36;

type TzFn = Arc<dyn Fn(i64) -> i32 + Send + Sync>;

/// The engine's store on top of `trs-state`.
pub struct StateStore {
    inner: trs_state::Store,
    dir: PathBuf,
    tz: TzFn,
    /// Piece count of the torrents that are still incomplete, for turning a bitfield into
    /// `ResumeData`. Complete torrents (the vast majority) cost nothing here.
    pieces: Mutex<HashMap<TorrentId, u32>>,
}

impl StateStore {
    /// Wrap an opened store. Day boundaries use the local zone as `/bin/date` reports it; install
    /// the app's own with [`StateStore::with_time_zone`].
    pub fn new(inner: trs_state::Store) -> Arc<StateStore> {
        Arc::new(Self::build(inner))
    }

    fn build(inner: trs_state::Store) -> StateStore {
        let dir = inner.dir().to_path_buf();
        StateStore {
            inner,
            dir,
            tz: Arc::new(crate::util::local_utc_offset),
            pieces: Mutex::new(HashMap::new()),
        }
    }

    /// Open (or create) the state directory.
    pub fn open(dir: impl AsRef<Path>) -> Result<(Arc<StateStore>, trs_state::OpenReport), StoreError> {
        let opened = trs_state::Store::open(dir).map_err(se)?;
        Ok((Self::new(opened.store), opened.report))
    }

    /// Replace the zone lookup (`unix seconds -> UTC offset seconds`) used to bucket statistics by day.
    pub fn with_time_zone(store: trs_state::Store, tz: impl Fn(i64) -> i32 + Send + Sync + 'static) -> Arc<StateStore> {
        let mut s = Self::build(store);
        s.tz = Arc::new(tz);
        Arc::new(s)
    }

    /// The underlying store (statistics queries, flushing, backups).
    pub fn state(&self) -> &trs_state::Store {
        &self.inner
    }

    /// The state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// After a damaged database was set aside: make records from `torrents/*.torrent`. Counters,
    /// statistics and resume data are gone; the engine rechecks the files it finds at the default
    /// location (per-file paths are not known any more).
    pub fn rebuild_from_torrent_files(&self, default_save: &Path) -> Result<usize, StoreError> {
        let root = self.inner.intern_root(&default_save.to_string_lossy()).map_err(se)?;
        let mut n = 0;
        for hash in self.inner.list_torrent_files().map_err(se)? {
            let Ok(bytes) = self.inner.read_torrent_file(&hash) else { continue };
            let Ok(mi) = Metainfo::from_bytes(&bytes) else { continue };
            let mut rec = SRecord::new(mi.name.to_string(), mi.total_length, crate::util::unix_secs() as i64);
            rec.piece_count = mi.piece_count() as u32;
            rec.file_count = mi.file_count().min(u32::MAX as usize) as u32;
            rec.save_root = root;
            rec.flags.set(TorrentFlags::PRIVATE, mi.private);
            rec.flags.0 |= E_META;
            self.inner.put_torrent(hash, &rec).map_err(se)?;
            n += 1;
        }
        self.inner.flush_now().map_err(se)?;
        Ok(n)
    }

    // ---- side files ---------------------------------------------------------------------------

    fn placement_path(&self, id: &TorrentId) -> PathBuf {
        self.dir.join("placement").join(format!("{}.bin", crate::util::hex(id)))
    }

    fn idle_path(&self, id: &TorrentId) -> PathBuf {
        self.dir.join("idle").join(format!("{}.bin", crate::util::hex(id)))
    }

    fn direct_path(&self, id: &TorrentId) -> PathBuf {
        self.dir.join("downloads").join(format!("{}.rec", crate::util::hex(id)))
    }

    fn write_side(&self, path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(se)?;
        }
        trs_state::fsutil::atomic_write(path, bytes).map_err(se)
    }

    fn read_side(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        use std::io::Read;
        let f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(se(e)),
        };
        let mut buf = Vec::new();
        f.take(MAX_SIDE_FILE + 1).read_to_end(&mut buf).map_err(se)?;
        if buf.len() as u64 > MAX_SIDE_FILE {
            return Err(StoreError("side file too large".into()));
        }
        Ok(Some(buf))
    }

    fn extra_path(&self) -> PathBuf {
        self.dir.join("torrent-extra.bin")
    }

    fn load_extra(&self) -> HashMap<TorrentId, Extra> {
        let mut m = HashMap::new();
        if let Ok(Some(b)) = Self::read_side(&self.extra_path()) {
            for c in b.as_chunks::<EXTRA_LEN>().0 {
                let mut id = [0u8; 20];
                id.copy_from_slice(&c[..20]);
                let u = |i: usize| u32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
                m.insert(id, Extra { queue_pos: u(20), last_activity: u(24), error_since: u(28), error_tracker: c[32] & 1 != 0, pending_start: c[33] & 1 != 0 });
            }
        }
        m
    }

    /// Fold this batch into `torrent-extra.bin` (queue position, last activity, error start);
    /// the file is only rewritten when something changed. Nothing is kept in memory between commits.
    fn update_extra(&self, set: &[(TorrentId, Extra)], removed: &[TorrentId]) {
        if set.is_empty() && removed.is_empty() {
            return;
        }
        let mut q = self.load_extra();
        let mut changed = false;
        for id in removed {
            changed |= q.remove(id).is_some();
        }
        for (id, x) in set {
            changed |= q.insert(*id, *x) != Some(*x);
        }
        if changed {
            let mut v: Vec<(&TorrentId, &Extra)> = q.iter().collect();
            v.sort_by_key(|(id, x)| (x.queue_pos, **id));
            let mut out = Vec::with_capacity(v.len() * EXTRA_LEN);
            for (id, x) in v {
                out.extend_from_slice(id);
                out.extend_from_slice(&x.queue_pos.to_le_bytes());
                out.extend_from_slice(&x.last_activity.to_le_bytes());
                out.extend_from_slice(&x.error_since.to_le_bytes());
                out.push(u8::from(x.error_tracker));
                out.push(u8::from(x.pending_start));
                out.extend_from_slice(&[0, 0]);
            }
            // Losing this file only loses the order of the queue and the activity history.
            let _ = self.write_side(&self.extra_path(), &out);
        }
    }

    fn day_of(&self, at: u64) -> Day {
        let at = at as i64;
        Day::from_unix(at, (self.tz)(at))
    }
}

// ---- record mapping ---------------------------------------------------------------------------

fn to_state_record(r: &TorrentRecord, root: u32) -> SRecord {
    let mut rec = SRecord::new(r.name.clone(), r.total_length, r.added_at as i64);
    rec.piece_count = if r.piece_length > 0 { r.total_length.div_ceil(u64::from(r.piece_length)).min(u64::from(u32::MAX)) as u32 } else { 0 };
    rec.file_count = r.file_priorities.len().clamp(1, MAX_FILES) as u32;
    rec.completed_at = r.finished_at.map(|t| t as i64);
    rec.save_root = root;
    let f = &mut rec.flags;
    f.set(TorrentFlags::PRIVATE, r.private);
    f.set(TorrentFlags::PAUSED, r.state == StoredState::Paused);
    f.set(TorrentFlags::GOAL_REACHED, r.state == StoredState::GoalReached);
    f.set(TorrentFlags::FINISHED, r.finished_at.is_some());
    f.set(TorrentFlags::CATEGORY_USER_SET, r.category_user);
    f.set(TorrentFlags::ORGANIZED, r.placed != Placed::No);
    f.set(E_META, r.has_metadata);
    f.set(E_ADDED_COMPLETE, r.added_complete);
    f.set(E_STAGED, r.staged);
    f.set(E_COMPLETE, r.complete);
    f.set(E_RENAMED, r.placed == Placed::Renamed);
    f.set(TorrentFlags::AWAITING_CHOICE, r.awaiting_choice);
    f.set(TorrentFlags::LOCATION_PINNED, r.pinned);
    if !r.has_metadata {
        // The magnet URI is all we have until the metadata arrives.
        rec.rel_path = r.magnet.clone().unwrap_or_default();
    }
    rec.category = r.category;
    rec.title = r.title.clone();
    rec.tracker_profile = r.tracker_profile.clone();
    rec.rule = r.rule.clone();
    rec.priority = r.priority_user.then(|| match r.priority {
        0 => state_priority(crate::api::Priority::High),
        2 => state_priority(crate::api::Priority::Low),
        _ => state_priority(crate::api::Priority::Normal),
    });
    rec.goal = r.goal_user.then(|| state_goal(&r.seed_goal));
    if r.file_priorities.contains(&0) {
        let mut bits = vec![0u8; r.file_priorities.len().div_ceil(8)];
        for (i, &p) in r.file_priorities.iter().enumerate() {
            if p == 0 {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        rec.skipped_files = Some(bits.into_boxed_slice());
    }
    rec
}

fn tiers_of(mi: &Metainfo) -> Vec<Vec<String>> {
    mi.announce_tiers.iter().map(|t| t.iter().map(|u| u.to_string()).collect()).collect()
}

impl StateStore {
    fn session_path(&self) -> PathBuf {
        self.dir.join("torrent-session.bin")
    }

    /// Download-session bases (`TorrentRecord::session_base`): only torrents that finished and
    /// downloaded again have one, so the file is tiny and usually absent.
    fn load_bases(&self) -> HashMap<TorrentId, u64> {
        let mut m = HashMap::new();
        if let Ok(Some(b)) = Self::read_side(&self.session_path()) {
            for c in b.as_chunks::<28>().0 {
                let mut id = [0u8; 20];
                id.copy_from_slice(&c[..20]);
                let mut v = [0u8; 8];
                v.copy_from_slice(&c[20..]);
                m.insert(id, u64::from_le_bytes(v));
            }
        }
        m
    }

    fn update_bases(&self, set: &[(TorrentId, u64)], removed: &[TorrentId]) {
        if set.is_empty() && removed.is_empty() {
            return;
        }
        let mut q = self.load_bases();
        let mut changed = false;
        for id in removed {
            changed |= q.remove(id).is_some();
        }
        for (id, base) in set {
            if *base == 0 {
                changed |= q.remove(id).is_some();
            } else {
                changed |= q.insert(*id, *base) != Some(*base);
            }
        }
        if changed {
            let mut v: Vec<(&TorrentId, &u64)> = q.iter().collect();
            v.sort();
            let mut out = Vec::with_capacity(v.len() * 28);
            for (id, b) in v {
                out.extend_from_slice(id);
                out.extend_from_slice(&b.to_le_bytes());
            }
            if out.is_empty() {
                let _ = std::fs::remove_file(self.session_path());
            } else {
                let _ = self.write_side(&self.session_path(), &out);
            }
        }
    }

    fn load_record(
        &self,
        id: TorrentId,
        rec: SRecord,
        counters: Counters,
        extra: &HashMap<TorrentId, Extra>,
        bases: &HashMap<TorrentId, u64>,
        order: u32,
    ) -> StoredTorrent {
        let f = rec.flags;
        let has_meta = f.has(E_META);
        let magnet = (!has_meta && !rec.rel_path.is_empty()).then(|| rec.rel_path.clone());
        let mut announce: Option<Vec<Vec<String>>> = None;
        let (mut piece_length, mut wanted) = (0u32, rec.total_size);
        let nfiles;
        let mut prios: Vec<u8> = Vec::new();
        let mut piece_prios: Option<Vec<u8>> = None;
        if has_meta {
            if let Ok(bytes) = self.inner.read_torrent_file(&id)
                && let Ok(mi) = Metainfo::from_bytes(&bytes) {
                    announce = Some(tiers_of(&mi));
                    piece_length = mi.piece_length;
                    nfiles = mi.file_count();
                    if let Some(skip) = &rec.skipped_files {
                        prios = (0..nfiles)
                            .map(|i| if skip.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1) { 0 } else { 2 })
                            .collect();
                    }
                    wanted = mi
                        .files()
                        .filter(|f| !f.is_padding() && prios.get(f.index).copied().unwrap_or(2) > 0)
                        .map(|f| f.length)
                        .sum();
                    piece_prios = Some(crate::picker::piece_priorities(
                        mi.piece_length,
                        mi.piece_count() as u32,
                        mi.files().map(|f| (f.offset, f.length, prios.get(f.index).copied().unwrap_or(2), f.is_padding())),
                    ));
                }
        } else if let Some(m) = magnet.as_deref().and_then(|m| trs_metainfo::Magnet::parse(m).ok()) {
            announce = Some(m.trackers.iter().map(|t| vec![t.clone()]).collect());
        }
        let complete = f.has(E_COMPLETE);
        if !complete
            && let Ok(mut p) = self.pieces.lock() {
                let n = if piece_length > 0 { rec.total_size.div_ceil(u64::from(piece_length)) as u32 } else { rec.piece_count };
                p.insert(id, n);
            }
        let mut wanted_done = (complete || rec.completed_at.is_some()).then_some(wanted);
        let done_bytes = if complete {
            rec.total_size
        } else if piece_length > 0 {
            match self.inner.get_resume(id) {
                Ok(Some(rd)) => {
                    if wanted_done.is_none()
                        && let Some(pp) = &piece_prios {
                            wanted_done = Some(wanted.saturating_sub(resume_wanted_missing(&rd, piece_length, rec.total_size, pp)));
                        }
                    resume_done_bytes(&rd, piece_length, rec.total_size)
                }
                _ => 0,
            }
        } else {
            0
        };
        let priority_user = rec.priority.is_some();
        let priority = match rec.priority {
            Some(trs_state::rules::Priority::High) => 0,
            Some(trs_state::rules::Priority::Low) => 2,
            _ => 1,
        };
        let placed = if f.has(E_RENAMED) {
            Placed::Renamed
        } else {
            Placed::No
        };
        let record = TorrentRecord {
            id,
            name: rec.name.clone(),
            has_metadata: has_meta,
            magnet,
            announce,
            total_length: rec.total_size,
            wanted_length: wanted,
            session_base: bases.get(&id).copied().unwrap_or(0),
            piece_length,
            private: f.has(TorrentFlags::PRIVATE),
            save_path: self.inner.root_path(rec.save_root).map(PathBuf::from).unwrap_or_default(),
            state: if f.has(TorrentFlags::PAUSED) {
                StoredState::Paused
            } else if f.has(TorrentFlags::GOAL_REACHED) {
                StoredState::GoalReached
            } else {
                StoredState::Active
            },
            file_priorities: prios,
            priority,
            priority_user,
            queue_pos: extra.get(&id).map_or(order, |x| x.queue_pos),
            category: rec.category,
            category_user: f.has(TorrentFlags::CATEGORY_USER_SET),
            tracker_profile: rec.tracker_profile.clone(),
            rule: rec.rule.clone(),
            added_at: rec.added_at.max(0) as u64,
            finished_at: rec.completed_at.map(|t| t.max(0) as u64),
            seed_goal: rec.goal.as_ref().map_or(SeedGoal::FOREVER, goal_from_state),
            goal_user: rec.goal.is_some(),
            added_complete: f.has(E_ADDED_COMPLETE),
            staged: f.has(E_STAGED),
            complete,
            placed,
            last_activity: extra.get(&id).map_or(0, |x| u64::from(x.last_activity)),
            error_since: extra.get(&id).map_or(0, |x| u64::from(x.error_since)),
            error_tracker: extra.get(&id).is_some_and(|x| x.error_tracker),
            stage_path: rec.stage_root.and_then(|i| self.inner.root_path(i)).map(PathBuf::from),
            pinned: f.has(TorrentFlags::LOCATION_PINNED),
            awaiting_choice: f.has(TorrentFlags::AWAITING_CHOICE),
            pending_start: f.has(TorrentFlags::AWAITING_CHOICE) && extra.get(&id).is_some_and(|x| x.pending_start),
            title: rec.title.clone(),
            idle: Self::read_side(&self.idle_path(&id)).ok().flatten().and_then(|b| crate::idle::IdleRing::from_bytes(&b)),
        };
        StoredTorrent {
            record,
            counters: LifetimeCounters {
                uploaded: counters.uploaded,
                downloaded: counters.downloaded,
                seeded_secs: counters.seed_secs,
                done_bytes,
                wanted_done,
            },
        }
    }
}

/// Bytes of the wanted pieces (`prio` per piece) the resume data lacks.
fn resume_wanted_missing(rd: &ResumeData, piece_len: u32, total: u64, prio: &[u8]) -> u64 {
    if rd.piece_count == 0 {
        return 0;
    }
    (0..rd.piece_count.min(prio.len() as u32))
        .filter(|&p| prio[p as usize] != 0 && !rd.bitfield.get((p / 8) as usize).is_some_and(|b| b & (0x80 >> (p % 8)) != 0))
        .map(|p| (total - (u64::from(p) * u64::from(piece_len)).min(total)).min(u64::from(piece_len)))
        .sum()
}

fn resume_done_bytes(rd: &ResumeData, piece_len: u32, total: u64) -> u64 {
    if rd.piece_count == 0 {
        return total;
    }
    let mut done = 0u64;
    for p in 0..rd.piece_count {
        if rd.bitfield.get((p / 8) as usize).is_some_and(|b| b & (0x80 >> (p % 8)) != 0) {
            let start = u64::from(p) * u64::from(piece_len);
            done += (total - start.min(total)).min(u64::from(piece_len));
        }
    }
    done
}

fn mtime_ns(secs: i64, nanos: u32) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(i64::from(nanos))
}

fn to_resume_data(r: &ResumeRecord, piece_count: u32) -> Result<ResumeData, StoreError> {
    let files = r
        .fingerprints
        .iter()
        .map(|f| match f {
            Some(b) => {
                let size = u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8]));
                let secs = i64::from_le_bytes(b[8..16].try_into().unwrap_or([0; 8]));
                let nanos = u32::from_le_bytes(b[16..20].try_into().unwrap_or([0; 4]));
                FileStamp { size, mtime_ns: mtime_ns(secs, nanos) }
            }
            None => FileStamp::default(),
        })
        .collect();
    let (pc, bitfield) = if r.complete {
        (0, Vec::new().into_boxed_slice())
    } else {
        (piece_count, r.bitfield.clone().into_boxed_slice())
    };
    let rd = ResumeData { piece_count: pc, bitfield, partial: Vec::new(), files, saved_at: r.saved_at as i64 };
    rd.check().map_err(|e| StoreError(format!("resume data rejected: {e:?}")))?;
    Ok(rd)
}

fn from_resume_data(rd: ResumeData) -> ResumeRecord {
    let fingerprints = rd
        .files
        .iter()
        .map(|s| {
            if *s == FileStamp::default() {
                return None;
            }
            let mut b = [0u8; 20];
            b[0..8].copy_from_slice(&s.size.to_le_bytes());
            b[8..16].copy_from_slice(&s.mtime_ns.div_euclid(1_000_000_000).to_le_bytes());
            b[16..20].copy_from_slice(&(s.mtime_ns.rem_euclid(1_000_000_000) as u32).to_le_bytes());
            Some(b)
        })
        .collect();
    ResumeRecord {
        complete: rd.piece_count == 0,
        bitfield: if rd.piece_count == 0 { Vec::new() } else { rd.bitfield.into_vec() },
        fingerprints,
        saved_at: rd.saved_at.max(0) as u64,
    }
}

// ---- tiny binary codec for the side files -------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn var(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.var(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn path(&mut self, p: &Path) {
        self.bytes(p.to_string_lossy().as_bytes());
    }
    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            Some(s) => {
                self.u8(1);
                self.bytes(s.as_bytes());
            }
            None => self.u8(0),
        }
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn u8(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }
    fn var(&mut self) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.u8()?;
            if shift > 63 {
                return None;
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }
    fn bytes(&mut self, max: usize) -> Option<&'a [u8]> {
        let n = usize::try_from(self.var()?).ok().filter(|n| *n <= max)?;
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }
    fn string(&mut self, max: usize) -> Option<String> {
        String::from_utf8(self.bytes(max)?.to_vec()).ok()
    }
    fn path(&mut self) -> Option<PathBuf> {
        self.string(4096).map(PathBuf::from)
    }
    fn opt_string(&mut self, max: usize) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.string(max)?)),
            _ => None,
        }
    }
}

const PLACEMENT_V: u8 = 1;
const DIRECT_V: u8 = 1;

fn encode_placement(p: &Placement) -> Vec<u8> {
    let mut w = W(vec![PLACEMENT_V]);
    w.path(&p.library_root);
    w.var(p.files.len() as u64);
    for f in &p.files {
        match f {
            Some(f) => {
                w.u8(1);
                w.path(f);
            }
            None => w.u8(0),
        }
    }
    w.0
}

fn decode_placement(b: &[u8]) -> Option<Placement> {
    let mut r = R(b);
    if r.u8()? != PLACEMENT_V {
        return None;
    }
    let library_root = r.path()?;
    let n = usize::try_from(r.var()?).ok().filter(|n| *n <= MAX_FILES)?;
    let mut files = Vec::with_capacity(n.min(b.len()));
    for _ in 0..n {
        files.push(match r.u8()? {
            0 => None,
            1 => Some(r.path()?),
            _ => return None,
        });
    }
    Some(Placement { files, library_root })
}

fn encode_direct(d: &DirectRecord) -> Vec<u8> {
    let mut w = W(vec![DIRECT_V]);
    w.bytes(&d.id);
    w.bytes(d.url.as_bytes());
    w.bytes(d.name.as_bytes());
    w.path(&d.dest_dir);
    w.opt_str(d.file_name.as_deref());
    w.var(u64::from(d.connections));
    match d.sha256 {
        Some(h) => {
            w.u8(1);
            w.0.extend_from_slice(&h);
        }
        None => w.u8(0),
    }
    w.var(d.rate_limit.map_or(0, |r| r.saturating_add(1)));
    match &d.state {
        DirectState::Active => w.u8(0),
        DirectState::Paused => w.u8(1),
        DirectState::Completed => w.u8(2),
        DirectState::Failed(m) => {
            w.u8(3);
            w.bytes(m.as_bytes());
        }
    }
    w.u8(d.category.map_or(255, |c| c.index() as u8));
    w.u8(0); // was: organize the finished file like a torrent
    w.var(d.added_at);
    w.var(d.finished_at.map_or(0, |t| t.saturating_add(1)));
    match &d.final_path {
        Some(p) => {
            w.u8(1);
            w.path(p);
        }
        None => w.u8(0),
    }
    w.var(d.total.map_or(0, |t| t.saturating_add(1)));
    w.var(d.done);
    w.var(u64::from(d.queue_pos));
    w.0
}

fn decode_direct(b: &[u8]) -> Option<DirectRecord> {
    let mut r = R(b);
    if r.u8()? != DIRECT_V {
        return None;
    }
    let id: TorrentId = r.bytes(20)?.try_into().ok()?;
    let url = r.string(8192)?;
    let name = r.string(1024)?;
    let dest_dir = r.path()?;
    let file_name = r.opt_string(1024)?;
    let connections = u32::try_from(r.var()?).ok()?.clamp(1, 16);
    let sha256 = match r.u8()? {
        0 => None,
        1 => {
            let (h, rest) = r.0.split_at_checked(32)?;
            r.0 = rest;
            h.try_into().ok()
        }
        _ => return None,
    };
    let rate_limit = r.var()?.checked_sub(1);
    let state = match r.u8()? {
        0 => DirectState::Active,
        1 => DirectState::Paused,
        2 => DirectState::Completed,
        3 => DirectState::Failed(r.string(1024)?),
        _ => return None,
    };
    let category = Category::ALL.get(usize::from(r.u8()?)).copied();
    let _retired_organize = r.u8()?;
    let added_at = r.var()?;
    let finished_at = r.var()?.checked_sub(1);
    let final_path = match r.u8()? {
        0 => None,
        1 => Some(r.path()?),
        _ => return None,
    };
    let total = r.var()?.checked_sub(1);
    let done = r.var()?;
    let queue_pos = u32::try_from(r.var()?).ok()?;
    Some(DirectRecord {
        id,
        url,
        name,
        dest_dir,
        file_name,
        connections,
        sha256,
        rate_limit,
        state,
        category,
        added_at,
        finished_at,
        final_path,
        total,
        done,
        queue_pos,
    })
}

// ---- Store ------------------------------------------------------------------------------------

impl Store for StateStore {
    fn moves_dir(&self) -> Option<PathBuf> {
        Some(self.dir.join("moves"))
    }

    fn load_torrents(&self) -> Result<Vec<StoredTorrent>, StoreError> {
        let loaded = self.inner.load_torrents().map_err(se)?;
        let counters: HashMap<_, _> = self.inner.load_counters().map_err(se)?.into_iter().collect();
        let extra = self.load_extra();
        let bases = self.load_bases();
        let mut recs = loaded.records;
        // Without a queue file the order of adding is the queue.
        recs.sort_by_key(|(h, r)| (r.added_at, *h));
        let mut out = Vec::with_capacity(recs.len());
        for (i, (id, rec)) in recs.into_iter().enumerate() {
            let c = counters.get(&id).copied().unwrap_or_default();
            out.push(self.load_record(id, rec, c, &extra, &bases, i as u32));
        }
        Ok(out)
    }

    fn load_resume(&self, id: &TorrentId) -> Result<Option<ResumeRecord>, StoreError> {
        Ok(self.inner.get_resume(*id).map_err(se)?.map(from_resume_data))
    }

    fn load_torrent_file(&self, id: &TorrentId) -> Result<Option<Arc<[u8]>>, StoreError> {
        if !self.inner.torrent_file_path(id).exists() {
            return Ok(None);
        }
        let bytes = self.inner.read_torrent_file(id).map_err(se)?;
        Ok(Some(Arc::from(bytes)))
    }

    fn load_peers(&self, id: &TorrentId) -> Result<Vec<SocketAddr>, StoreError> {
        self.inner.get_peers(*id).map_err(se)
    }

    fn load_dht_state(&self) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_dht_table().map_err(se)
    }

    fn load_placement(&self, id: &TorrentId) -> Result<Option<Placement>, StoreError> {
        match Self::read_side(&self.placement_path(id))? {
            Some(b) => Ok(decode_placement(&b)),
            None => Ok(None),
        }
    }

    fn load_directs(&self) -> Result<Vec<DirectRecord>, StoreError> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.dir.join("downloads")) else { return Ok(out) };
        for e in rd.flatten().take(10_000) {
            if e.path().extension().is_none_or(|x| x != "rec") {
                continue;
            }
            // A damaged record is skipped, never deleted: the `.part` can still be adopted by hand.
            if let Some(d) = Self::read_side(&e.path()).ok().flatten().and_then(|b| decode_direct(&b)) {
                out.push(d);
            }
        }
        Ok(out)
    }

    fn commit(&self, batch: Batch) -> Result<(), StoreError> {
        // Removals first: a torrent removed and added again within one batch (remove, then re-add
        // before the debounced write) must end up stored, not deleted with its new .torrent file.
        // A removal after an add in the same batch never carries the add (`dirty_remove` drops it).
        for id in &batch.removed_directs {
            let _ = std::fs::remove_file(self.direct_path(id));
        }
        for id in &batch.removed {
            self.inner.remove_torrent(*id).map_err(se)?;
            let _ = std::fs::remove_file(self.placement_path(id));
            let _ = std::fs::remove_file(self.idle_path(id));
            if let Ok(mut p) = self.pieces.lock() {
                p.remove(id);
            }
        }
        for (id, bytes) in &batch.torrent_files {
            self.inner.put_torrent_file(id, bytes).map_err(se)?;
        }
        for r in &batch.torrents {
            let root = self.inner.intern_root(&r.save_path.to_string_lossy()).map_err(se)?;
            let stage = match &r.stage_path {
                Some(p) => Some(self.inner.intern_root(&p.to_string_lossy()).map_err(se)?),
                None => None,
            };
            let mut rec = to_state_record(r, root);
            rec.stage_root = stage;
            if let Ok(mut p) = self.pieces.lock() {
                if r.complete {
                    p.remove(&r.id);
                } else {
                    p.insert(r.id, rec.piece_count);
                }
            }
            self.inner.put_torrent(r.id, &rec).map_err(se)?;
            // The inactive condition's history is a small side file (SPEC §7.5); losing it only
            // restarts the window.
            match &r.idle {
                Some(ring) => {
                    let _ = self.write_side(&self.idle_path(&r.id), &ring.to_bytes());
                }
                None => {
                    let _ = std::fs::remove_file(self.idle_path(&r.id));
                }
            }
        }
        let extras: Vec<(TorrentId, Extra)> = batch
            .torrents
            .iter()
            .map(|r| {
                let x = Extra {
                    queue_pos: r.queue_pos,
                    last_activity: r.last_activity.min(u64::from(u32::MAX)) as u32,
                    error_since: r.error_since.min(u64::from(u32::MAX)) as u32,
                    error_tracker: r.error_tracker,
                    pending_start: r.pending_start && r.awaiting_choice,
                };
                (r.id, x)
            })
            .collect();
        self.update_extra(&extras, &batch.removed);
        let bases: Vec<(TorrentId, u64)> = batch.torrents.iter().map(|r| (r.id, r.session_base)).collect();
        self.update_bases(&bases, &batch.removed);
        for (id, c) in &batch.counters {
            self.inner
                .set_counters_at_least(*id, Counters { downloaded: c.downloaded, uploaded: c.uploaded, seed_secs: c.seeded_secs })
                .map_err(se)?;
        }
        for (id, r) in &batch.resume {
            match r {
                Some(r) => {
                    let pc = self.pieces.lock().ok().and_then(|p| p.get(id).copied());
                    let pc = match pc {
                        Some(pc) => pc,
                        None => self.inner.get_torrent(*id).map_err(se)?.map_or(0, |t| t.piece_count),
                    };
                    match to_resume_data(r, pc) {
                        Ok(rd) => self.inner.put_resume(*id, &rd).map_err(se)?,
                        // Unusable resume data is dropped: the next start rechecks instead.
                        Err(_) => self.inner.remove_resume(*id).map_err(se)?,
                    }
                }
                None => self.inner.remove_resume(*id).map_err(se)?,
            }
        }
        for (id, peers) in &batch.peers {
            self.inner.put_peers(*id, peers).map_err(se)?;
        }
        for d in &batch.stat_deltas {
            self.record_delta(d)?;
        }
        if let Some(t) = &batch.dht_state {
            self.inner.put_dht_table(t).map_err(se)?;
        }
        for (id, p) in &batch.placements {
            let path = self.placement_path(id);
            match p {
                Some(p) => self.write_side(&path, &encode_placement(p))?,
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        for d in &batch.directs {
            self.write_side(&self.direct_path(&d.id), &encode_direct(d))?;
        }
        // The engine already debounces; make this batch durable before returning.
        self.inner.flush_now().map_err(se)
    }

    fn timing_store(&self) -> Box<dyn TimingStore + Send> {
        Box::new(self.inner.timing_handle())
    }

    fn record_launch(&self, now_ms: u64) -> Result<Vec<u64>, StoreError> {
        let path = self.dir.join("launches.log");
        let mut log = Self::launch_log(&path);
        // The log keeps an hour (more than the breaker looks at) and marks which runs ended
        // cleanly; only this launch and the unclean ones come back (`trs_tracker::crash`).
        let counted = log.record_launch(trs_tracker::Millis(now_ms));
        self.write_side(&path, log.render().as_bytes())?;
        Ok(counted.into_iter().map(|t| t.0).collect())
    }

    fn record_clean_exit(&self) -> Result<(), StoreError> {
        let path = self.dir.join("launches.log");
        let mut log = Self::launch_log(&path);
        log.mark_clean_exit();
        self.write_side(&path, log.render().as_bytes())
    }
}

impl StateStore {
    fn launch_log(path: &Path) -> trs_tracker::crash::LaunchLog {
        let text = Self::read_side(path).ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
        trs_tracker::crash::LaunchLog::parse(&text.unwrap_or_default())
    }

    fn record_delta(&self, d: &StatDelta) -> Result<(), StoreError> {
        let scope = match (&d.domain, d.private) {
            (Some(dom), _) => StatsScope::domain(dom),
            (None, true) => StatsScope::OtherPrivate,
            (None, false) => StatsScope::Public,
        };
        let b = StatsBucket {
            downloaded: d.downloaded,
            uploaded: d.uploaded,
            torrents_added: u64::from(d.torrents_added),
            torrents_completed: u64::from(d.torrents_completed),
            torrents_removed: u64::from(d.torrents_removed),
            files_completed: u64::from(d.files_completed),
            files_bytes: d.files_bytes,
            seed_secs: d.seed_secs,
        };
        if b.is_zero() {
            return Ok(());
        }
        self.inner.record_stats(&scope, self.day_of(d.at), &b).map_err(se)
    }
}
