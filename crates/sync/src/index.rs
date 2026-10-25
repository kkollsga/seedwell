//! A folder's index: what this device has (local), what every other device has announced (remote),
//! and, derived from both, what this device still needs.
//!
//! Storage is behind [`IndexStore`] ([`MemStore`] here, a redb file in `store_redb.rs`). Every change
//! goes through a [`Batch`], applied in one transaction, so the derived need list is always consistent
//! with the records it came from, also across a crash.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{Causality, ChunkRef, FileKind, IndexEntry, SignedFile};
use trs_sync_proto::sign::Writer;

use crate::global::{self, Candidate, Need};

/// The disk state a local record was taken from: unchanged means the content is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// Bytes.
    pub size: u64,
    /// Modification time, Unix ns.
    pub mtime_ns: i64,
    /// Status-change time, Unix ns.
    pub ctime_ns: i64,
    /// Inode.
    pub inode: u64,
}

/// The chunk-size tier a file was cut with, and the size that chose it (for hysteresis).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierMemo {
    /// Average chunk size in bytes.
    pub avg: u32,
    /// The file size when this tier was chosen.
    pub chosen_at: u64,
}

/// What this device has at one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRecord {
    /// The version, as announced (for a read-only device's local change: the last version received).
    pub file: SignedFile,
    /// Our sequence when this record last changed (assigned by [`Batch::put_local`]).
    pub sequence: u64,
    /// The disk content differs from `file` and must not be served (a read-only device's own change).
    pub invalid: bool,
    /// Disk stamp when the record matched the disk; `None` for tombstones.
    pub stamp: Option<Stamp>,
    /// Chunk tier memo (files only).
    pub tier: Option<TierMemo>,
}

/// What we know of another device's index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerState {
    /// The index id it announced (a new one means it reset its database: we drop what we had).
    pub index_id: u64,
    /// The highest sequence of its index we have stored.
    pub max_seq: u64,
}


/// One path this device needs, with the version to take and where to get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedRecord {
    /// What to do.
    pub need: Need,
    /// The current version.
    pub file: SignedFile,
    /// Devices that hold exactly that version.
    pub holders: Vec<ShortId>,
}

/// This device's own counters for the folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// Random id of this index; a new one tells peers to forget what they had of it.
    pub index_id: u64,
    /// The last sequence given to a local record.
    pub sequence: u64,
}

/// A change to the store. Built by [`Batch`]; stores apply a list of them atomically.
#[derive(Clone, Debug)]
pub enum Op {
    /// Our counters.
    SetMeta(Meta),
    /// A local record at `file.info.path` (replacing any previous one).
    PutLocal(LocalRecord),
    /// Forget the local record at this path (only for paths no device knows any more).
    DelLocal(String),
    /// A remote device's entry at `file.info.path`.
    PutRemote(ShortId, IndexEntry),
    /// A remote device's state.
    SetPeer(ShortId, PeerState),
    /// Forget everything a device announced (its index id changed, or it left the folder).
    DropPeer(ShortId),
    /// A writer.
    PutWriter(ShortId, Writer),
    /// The need of a path (`None` = nothing needed).
    SetNeed(String, Option<NeedRecord>),
}

/// The storage behind an [`Index`]: reads, plus an atomic apply of [`Op`]s.
pub trait IndexStore: Send {
    /// Our counters.
    fn meta(&self) -> io::Result<Meta>;
    /// Our record at a path.
    fn local(&self, path: &str) -> io::Result<Option<LocalRecord>>;
    /// Our records with a sequence above `after`, in sequence order, at most `limit`.
    fn local_since(&self, after: u64, limit: usize) -> io::Result<Vec<LocalRecord>>;
    /// Our paths at or below `prefix` (`""` = all) in path order, starting after `after` (exclusive,
    /// for paging), at most `limit`.
    fn local_paths(&self, prefix: &str, after: Option<&str>, limit: usize) -> io::Result<Vec<String>>;
    /// Every remote entry at a path.
    fn remote(&self, path: &str) -> io::Result<Vec<(ShortId, IndexEntry)>>;
    /// Paths a device has announced (for dropping a device), at most `limit` after `after`.
    fn remote_paths(&self, device: ShortId, after: Option<&str>, limit: usize) -> io::Result<Vec<String>>;
    /// A remote device's state.
    fn peer(&self, device: ShortId) -> io::Result<Option<PeerState>>;
    /// Every known writer.
    fn writers(&self) -> io::Result<Vec<(ShortId, Writer)>>;
    /// What this device needs at a path.
    fn need(&self, path: &str) -> io::Result<Option<NeedRecord>>;
    /// Needed paths in path order after `after`, at most `limit`.
    fn needs(&self, after: Option<&str>, limit: usize) -> io::Result<Vec<(String, NeedRecord)>>;
    /// How many paths are needed, and their total size.
    fn need_totals(&self) -> io::Result<(u64, u64)>;
    /// How many live files this device holds, and their total size (from the records: valid,
    /// not deleted, regular files). O(1): kept as counters with the records.
    fn local_totals(&self) -> io::Result<(u64, u64)>;
    /// Local files holding a chunk with this hash: (path, offset) pairs, at most `limit`.
    fn chunk_sources(&self, hash: &[u8; 32], limit: usize) -> io::Result<Vec<(String, u64)>>;
    /// Applies the ops in one transaction (all or nothing).
    fn apply(&mut self, ops: Vec<Op>) -> io::Result<()>;
    /// Makes every applied change durable. A store whose applies are durable already does nothing
    /// (the default); [`RedbStore`](crate::store_redb::RedbStore) with eventual durability commits.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Changes to one index, applied together. Reads see the batch's own writes, so the need of a path
/// is always computed from what the store will hold after the batch. The batch only reads the store;
/// [`Batch::finish`] hands back the ops, and the caller applies them (`store.apply(ops)`).
pub struct Batch<'a, S: IndexStore + ?Sized> {
    store: &'a S,
    me: ShortId,
    /// This device is a writer (it is in the writer table): it can sign the versions `Absorb` needs.
    writer: bool,
    meta: Meta,
    local: BTreeMap<String, Option<LocalRecord>>,
    remote: BTreeMap<(String, ShortId), IndexEntry>,
    dropped: BTreeSet<ShortId>,
    touched: BTreeSet<String>,
    ops: Vec<Op>,
}

impl<'a, S: IndexStore + ?Sized> Batch<'a, S> {
    /// Starts a batch on `store`, for the device `me`.
    pub fn new(store: &'a S, me: ShortId) -> io::Result<Self> {
        Ok(Batch {
            store,
            me,
            writer: store.writers()?.iter().any(|(d, _)| *d == me),
            meta: store.meta()?,
            local: BTreeMap::new(),
            remote: BTreeMap::new(),
            dropped: BTreeSet::new(),
            touched: BTreeSet::new(),
            ops: Vec::new(),
        })
    }

    /// Our record at a path, including this batch's writes.
    pub fn local(&self, path: &str) -> io::Result<Option<LocalRecord>> {
        match self.local.get(path) {
            Some(r) => Ok(r.clone()),
            None => self.store.local(path),
        }
    }

    /// Stores our record; it gets the next sequence.
    pub fn put_local(&mut self, mut rec: LocalRecord) {
        self.meta.sequence += 1;
        rec.sequence = self.meta.sequence;
        let path = rec.file.info.path.clone();
        self.touched.insert(path.clone());
        self.local.insert(path, Some(rec.clone()));
        self.ops.push(Op::PutLocal(rec));
    }

    /// Updates the disk stamp or tier of our record without a new sequence (nothing peers need to hear).
    pub fn restamp_local(&mut self, rec: LocalRecord) {
        let path = rec.file.info.path.clone();
        self.local.insert(path, Some(rec.clone()));
        self.ops.push(Op::PutLocal(rec));
    }

    /// Stores a remote entry.
    pub fn put_remote(&mut self, device: ShortId, entry: IndexEntry) {
        let path = entry.file.info.path.clone();
        self.touched.insert(path.clone());
        self.remote.insert((path, device), entry.clone());
        self.ops.push(Op::PutRemote(device, entry));
    }

    /// Records a peer's state.
    pub fn set_peer(&mut self, device: ShortId, state: PeerState) {
        self.ops.push(Op::SetPeer(device, state));
    }

    /// Forgets everything `device` announced; the paths it had are re-evaluated.
    pub fn drop_peer(&mut self, device: ShortId) -> io::Result<()> {
        let mut after: Option<String> = None;
        loop {
            let page = self.store.remote_paths(device, after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { break };
            self.touched.extend(page);
            after = Some(last);
        }
        self.remote.retain(|(_, d), _| *d != device);
        self.dropped.insert(device);
        self.ops.push(Op::DropPeer(device));
        Ok(())
    }

    /// Records a writer.
    pub fn put_writer(&mut self, device: ShortId, w: Writer) {
        self.ops.push(Op::PutWriter(device, w));
    }

    /// Sets our index id (a reset database).
    pub fn set_index_id(&mut self, index_id: u64) {
        self.meta.index_id = index_id;
    }

    /// Every remote entry at a path, including this batch's writes and drops.
    fn remote_at(&self, path: &str) -> io::Result<Vec<(ShortId, IndexEntry)>> {
        let mut out: BTreeMap<ShortId, IndexEntry> =
            self.store.remote(path)?.into_iter().filter(|(d, _)| !self.dropped.contains(d)).collect();
        for ((p, d), e) in self.remote.range((path.to_string(), ShortId(0))..) {
            if p != path {
                break;
            }
            out.insert(*d, e.clone());
        }
        Ok(out.into_iter().collect())
    }

    /// The versions other devices announce at a path (valid ones), including this batch's writes.
    pub fn remote_versions(&self, path: &str) -> io::Result<Vec<trs_sync_proto::model::Version>> {
        Ok(self.remote_at(path)?.into_iter().filter(|(_, e)| !e.invalid).map(|(_, e)| e.file.info.version).collect())
    }

    /// The changes so far (to commit in batches of a bounded size).
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// No changes.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// What the store says this device needs at a path (not including this batch's changes).
    pub fn need(&self, path: &str) -> io::Result<Option<NeedRecord>> {
        self.store.need(path)
    }

    /// Recomputes the need of every touched path and returns everything to apply in one transaction:
    /// `store.apply(batch.finish()?)`.
    pub fn finish(mut self) -> io::Result<Vec<Op>> {
        for path in std::mem::take(&mut self.touched) {
            let need = self.need_of(&path)?;
            self.ops.push(Op::SetNeed(path, need));
        }
        self.ops.insert(0, Op::SetMeta(self.meta));
        Ok(self.ops)
    }

    fn need_of(&self, path: &str) -> io::Result<Option<NeedRecord>> {
        let local = self.local(path)?;
        let remote = self.remote_at(path)?;
        let mut cands: Vec<Candidate<'_>> = remote
            .iter()
            .map(|(d, e)| Candidate { device: *d, file: &e.file, invalid: e.invalid })
            .collect();
        if let Some(l) = &local {
            cands.push(Candidate { device: self.me, file: &l.file, invalid: l.invalid });
        }
        let Some(cur) = global::current(&cands) else { return Ok(None) };
        let need = global::need(local.as_ref().map(|l| &l.file.info), Some(&cur.info));
        if need == Need::Nothing {
            // We hold the current version and a concurrent delete lost to it: absorb the delete.
            if let Some(l) = &local
                && self.writer
                && !l.invalid
                && !l.file.info.deleted
                && l.file.info.version.compare(&cur.info.version) == Causality::Equal
                && remote.iter().any(|(_, e)| {
                    !e.invalid && e.file.info.deleted && e.file.info.version.compare(&l.file.info.version) == Causality::Concurrent
                })
                // Not while a conflict over other content is open: the new version is ours (`modified_by`
                // decides ties), so it could lose to a version ours had beaten. Its holders take ours
                // first (keeping theirs as conflict copies), then the delete is absorbed.
                && !remote.iter().any(|(_, e)| {
                    let theirs = &e.file.info;
                    // (A version another one has replaced, a delete say, is no contender.)
                    let replaced = cands.iter().any(|c| !c.invalid && c.file.info.version.compare(&theirs.version) == Causality::Greater);
                    !e.invalid
                        && !replaced
                        && !theirs.deleted
                        && !theirs.same_content(&l.file.info)
                        && theirs.version.compare(&l.file.info.version) == Causality::Concurrent
                })
            {
                return Ok(Some(NeedRecord { need: Need::Absorb, file: l.file.clone(), holders: Vec::new() }));
            }
            return Ok(None);
        }
        let holders = global::holders(&cands, cur).into_iter().filter(|d| *d != self.me).collect();
        Ok(Some(NeedRecord { need, file: cur.clone(), holders }))
    }
}

/// Byte offsets of each chunk in a chunk list.
pub fn offsets(chunks: &[ChunkRef]) -> impl Iterator<Item = (u64, &ChunkRef)> {
    chunks.iter().scan(0u64, |off, c| {
        let at = *off;
        *off += u64::from(c.len);
        Some((at, c))
    })
}

/// An in-memory [`IndexStore`] for tests and the simulator.
#[derive(Clone, Debug, Default)]
pub struct MemStore {
    meta: Meta,
    local: BTreeMap<String, LocalRecord>,
    by_seq: BTreeMap<u64, String>,
    remote: BTreeMap<(String, ShortId), IndexEntry>,
    peers: BTreeMap<ShortId, PeerState>,
    writers: BTreeMap<ShortId, Writer>,
    need: BTreeMap<String, NeedRecord>,
    chunks: BTreeMap<[u8; 32], BTreeSet<(String, u64)>>,
    /// [`IndexStore::local_totals`].
    totals: (u64, u64),
}

/// What a record adds to [`IndexStore::local_totals`]: its size, if it is a live file we hold.
pub(crate) fn counted(rec: &LocalRecord) -> Option<u64> {
    let i = &rec.file.info;
    (!rec.invalid && !i.deleted && i.kind == FileKind::File).then_some(i.size)
}

impl MemStore {
    /// An empty store with this index id.
    pub fn new(index_id: u64) -> Self {
        MemStore { meta: Meta { index_id, sequence: 0 }, ..Default::default() }
    }

    fn unchunk(&mut self, rec: &LocalRecord) {
        for (off, c) in offsets(&rec.file.info.chunks) {
            if let Some(set) = self.chunks.get_mut(&c.hash) {
                set.remove(&(rec.file.info.path.clone(), off));
                if set.is_empty() {
                    self.chunks.remove(&c.hash);
                }
            }
        }
    }

    fn uncount(&mut self, rec: &LocalRecord) {
        if let Some(size) = counted(rec) {
            self.totals = (self.totals.0 - 1, self.totals.1 - size);
        }
    }

    fn chunk(&mut self, rec: &LocalRecord) {
        if rec.invalid || rec.file.info.deleted {
            return;
        }
        for (off, c) in offsets(&rec.file.info.chunks) {
            self.chunks.entry(c.hash).or_default().insert((rec.file.info.path.clone(), off));
        }
    }
}

impl IndexStore for MemStore {
    fn meta(&self) -> io::Result<Meta> {
        Ok(self.meta)
    }

    fn local(&self, path: &str) -> io::Result<Option<LocalRecord>> {
        Ok(self.local.get(path).cloned())
    }

    fn local_since(&self, after: u64, limit: usize) -> io::Result<Vec<LocalRecord>> {
        Ok(self
            .by_seq
            .range(after + 1..)
            .take(limit)
            .filter_map(|(_, p)| self.local.get(p).cloned())
            .collect())
    }

    fn local_paths(&self, prefix: &str, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        Ok(paths_under(self.local.keys(), prefix, after, limit))
    }

    fn remote(&self, path: &str) -> io::Result<Vec<(ShortId, IndexEntry)>> {
        Ok(self
            .remote
            .range((path.to_string(), ShortId(0))..)
            .take_while(|((p, _), _)| p == path)
            .map(|((_, d), e)| (*d, e.clone()))
            .collect())
    }

    fn remote_paths(&self, device: ShortId, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        Ok(self
            .remote
            .keys()
            .filter(|(p, d)| *d == device && after.is_none_or(|a| p.as_str() > a))
            .take(limit)
            .map(|(p, _)| p.clone())
            .collect())
    }

    fn peer(&self, device: ShortId) -> io::Result<Option<PeerState>> {
        Ok(self.peers.get(&device).copied())
    }

    fn writers(&self) -> io::Result<Vec<(ShortId, Writer)>> {
        Ok(self.writers.iter().map(|(d, w)| (*d, *w)).collect())
    }

    fn need(&self, path: &str) -> io::Result<Option<NeedRecord>> {
        Ok(self.need.get(path).cloned())
    }

    fn needs(&self, after: Option<&str>, limit: usize) -> io::Result<Vec<(String, NeedRecord)>> {
        Ok(self
            .need
            .iter()
            .filter(|(p, _)| after.is_none_or(|a| p.as_str() > a))
            .take(limit)
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect())
    }

    fn need_totals(&self) -> io::Result<(u64, u64)> {
        Ok((self.need.len() as u64, self.need.values().map(|n| n.file.info.size).sum()))
    }

    fn local_totals(&self) -> io::Result<(u64, u64)> {
        Ok(self.totals)
    }

    fn chunk_sources(&self, hash: &[u8; 32], limit: usize) -> io::Result<Vec<(String, u64)>> {
        Ok(self.chunks.get(hash).map(|s| s.iter().take(limit).cloned().collect()).unwrap_or_default())
    }

    fn apply(&mut self, ops: Vec<Op>) -> io::Result<()> {
        for op in ops {
            match op {
                Op::SetMeta(m) => self.meta = m,
                Op::PutLocal(rec) => {
                    let path = rec.file.info.path.clone();
                    if let Some(old) = self.local.remove(&path) {
                        self.by_seq.remove(&old.sequence);
                        self.unchunk(&old);
                        self.uncount(&old);
                    }
                    self.by_seq.insert(rec.sequence, path.clone());
                    self.chunk(&rec);
                    if let Some(size) = counted(&rec) {
                        self.totals = (self.totals.0 + 1, self.totals.1 + size);
                    }
                    self.local.insert(path, rec);
                }
                Op::DelLocal(path) => {
                    if let Some(old) = self.local.remove(&path) {
                        self.by_seq.remove(&old.sequence);
                        self.unchunk(&old);
                        self.uncount(&old);
                    }
                }
                Op::PutRemote(d, e) => {
                    self.remote.insert((e.file.info.path.clone(), d), e);
                }
                Op::SetPeer(d, s) => {
                    self.peers.insert(d, s);
                }
                Op::DropPeer(d) => {
                    self.remote.retain(|(_, dev), _| *dev != d);
                    self.peers.remove(&d);
                }
                Op::PutWriter(d, w) => {
                    self.writers.insert(d, w);
                }
                Op::SetNeed(p, n) => match n {
                    Some(n) => {
                        self.need.insert(p, n);
                    }
                    None => {
                        self.need.remove(&p);
                    }
                },
            }
        }
        Ok(())
    }
}

/// `path` is `prefix` or inside it (`""` covers everything).
pub fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/')
}

/// Paths at or below `prefix` from a sorted list, after `after`, at most `limit`. Paths below a
/// directory are not contiguous with it in byte order (`a`, `a-b`, `a/c`), so the directory itself and
/// the range `prefix/…` are taken separately.
fn paths_under<'a>(
    sorted: impl Iterator<Item = &'a String> + Clone,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let past = |p: &str| after.is_none_or(|a| p > a);
    if prefix.is_empty() {
        return sorted.filter(|p| past(p)).take(limit).cloned().collect();
    }
    let below = format!("{prefix}/");
    let exact = sorted.clone().find(|p| p.as_str() == prefix).filter(|p| past(p)).cloned();
    exact
        .into_iter()
        .chain(sorted.filter(|p| p.starts_with(&below) && past(p)).cloned())
        .take(limit)
        .collect()
}
