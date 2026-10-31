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
    /// An on-demand folder (SPEC §7G.11) holds this version without its content: the file is online only here.
    /// Never served; a missing file is not a delete. Always another device's version (a device records a version of
    /// its own only with its bytes), announced invalid ("do not fetch from me").
    pub dataless: bool,
    /// Fetch the content of this dataless record (asked for, or below a folder kept on this device).
    pub want: bool,
}

impl LocalRecord {
    /// A read-only device's own change: the disk differs from the version recorded (and not because the content is
    /// online only).
    pub fn own_change(&self) -> bool {
        self.invalid && !self.dataless
    }
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
    /// A writer (the folder's membership lets it write).
    PutWriter(ShortId, Writer),
    /// No longer a writer.
    DropWriter(ShortId),
    /// The need of a path (`None` = nothing needed).
    SetNeed(String, Option<NeedRecord>),
    /// The host's feed log (SPEC §7G.11): what it shows of the path changed at this sequence without a new version
    /// (pinned, downloading). Only the latest per path is kept.
    Shown(String, u64),
    /// The path's live record was replaced, at this sequence, by one with another file id: the identifier of this old
    /// file id names nothing there any more (the host's feed reports it deleted). On-demand folders only.
    Retired(u64, String, [u8; 16]),
    /// Forget the retired file ids up to this sequence: a host's anchor from before it has expired (it lists again).
    PruneRetired(u64),
    /// The version at this path is one no device can give right now (with its size), or no longer (`None`) (SPEC
    /// §7G.11). Decided by the folder, read by anyone (the host's items).
    Unavailable(String, Option<u64>),
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
    /// Every remote device whose index is stored here.
    fn peer_ids(&self) -> io::Result<Vec<ShortId>>;
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
    /// The host's feed log after `after`, in sequence order, at most `limit`: (sequence, path), the latest per path.
    fn shown_since(&self, after: u64, limit: usize) -> io::Result<Vec<(u64, String)>>;
    /// The sequence of the path's entry in the host's feed log, if any.
    fn shown_at(&self, path: &str) -> io::Result<Option<u64>>;
    /// File ids retired from their paths after `after` and up to `upto`: (sequence, path, file id).
    fn retired(&self, after: u64, upto: u64) -> io::Result<Vec<(u64, String, [u8; 16])>>;
    /// The versions no device can give right now ([`Op::Unavailable`]), with their sizes, in path order.
    fn unavailable(&self) -> io::Result<Vec<(String, u64)>>;
    /// Whether the version at `path` is one no device can give right now (its size).
    fn unavailable_at(&self, path: &str) -> io::Result<Option<u64>>;
    /// Whether any version is one no device can give right now (without reading them all).
    fn any_unavailable(&self) -> io::Result<bool> {
        Ok(!self.unavailable()?.is_empty())
    }
    /// What this device needs at each of `paths` (in one read where the store can), and whether each is unavailable.
    fn needs_at(&self, paths: &[String]) -> io::Result<Vec<(Option<NeedRecord>, bool)>> {
        paths.iter().map(|p| Ok((self.need(p)?, self.unavailable_at(p)?.is_some()))).collect()
    }
    /// The sequence up to which retired file ids were forgotten ([`Op::PruneRetired`]): a host's anchor below it
    /// has expired.
    fn feed_floor(&self) -> io::Result<u64>;
    /// This index feeds a host (an on-demand folder): [`Batch::put_local`] notes retired file ids. Set by the folder;
    /// not stored.
    fn feeds_host(&self) -> bool {
        false
    }
    /// Sets [`Self::feeds_host`].
    fn set_feeds_host(&mut self, _on: bool) {}
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
    /// [`IndexStore::feeds_host`].
    retire: bool,
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
            retire: store.feeds_host(),
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
        let old = if self.retire { self.local(&rec.file.info.path).ok().flatten().filter(|o| !o.file.info.deleted) } else { None };
        self.meta.sequence += 1;
        rec.sequence = self.meta.sequence;
        if let Some(o) = old.filter(|o| o.file.info.file_id != rec.file.info.file_id) {
            self.ops.push(Op::Retired(rec.sequence, rec.file.info.path.clone(), o.file.info.file_id));
        }
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

    /// Updates our record's on-demand flags without a new sequence (they are this device's own: peers are not sent
    /// the version again), computes its need again (an online-only file asked for needs its content now), and notes
    /// the path in the host's feed log ([`Self::shown`]).
    pub fn reflag_local(&mut self, rec: LocalRecord) {
        let path = rec.file.info.path.clone();
        self.shown(&path);
        self.touched.insert(path.clone());
        self.local.insert(path, Some(rec.clone()));
        self.ops.push(Op::PutLocal(rec));
    }

    /// Forgets our record at `path` (a version taken without its content when the folder switches to full: the need is
    /// then the version again, fetched like any).
    pub fn forget_local(&mut self, path: &str) {
        self.touched.insert(path.to_string());
        self.local.insert(path.to_string(), None);
        self.ops.push(Op::DelLocal(path.to_string()));
    }

    /// Notes that what the host shows of `path` changed (pinned, downloading) without a new version: the host's feed
    /// log gets it at the next sequence, one counter with the records' so the feed merges both under one anchor.
    /// Peers page by their records' sequences: they never see these.
    pub fn shown(&mut self, path: &str) {
        self.meta.sequence += 1;
        self.ops.push(Op::Shown(path.to_string(), self.meta.sequence));
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

    /// Forgets a writer.
    pub fn drop_writer(&mut self, device: ShortId) {
        self.ops.push(Op::DropWriter(device));
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
        // An online-only file asked for (or kept on this device) needs its content: as if we had nothing there. Not when
        // the current version is a delete: then the record itself must go, wanted or not.
        let fetching = local.as_ref().is_some_and(|l| l.dataless && l.want) && !cur.info.deleted;
        let need = global::need(local.as_ref().filter(|_| !fetching).map(|l| &l.file.info), Some(&cur.info));
        if need == Need::Nothing {
            // We hold the current version and a concurrent delete lost to it: absorb the delete.
            if let Some(l) = &local
                && self.writer
                && !l.invalid
                && !l.dataless
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
        // A device with the version online only holds no copy to fetch from.
        let dataless = |d: &ShortId| remote.iter().any(|(r, e)| r == d && e.dataless);
        let holders = global::holders(&cands, cur).into_iter().filter(|d| *d != self.me && !dataless(d)).collect();
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
    shown: BTreeMap<u64, String>,
    shown_at: BTreeMap<String, u64>,
    retired: BTreeMap<u64, (String, [u8; 16])>,
    floor: u64,
    feeds_host: bool,
    unavailable: BTreeMap<String, u64>,
}

/// What a record adds to [`IndexStore::local_totals`]: its size, if it is a live file we hold.
pub(crate) fn counted(rec: &LocalRecord) -> Option<u64> {
    let i = &rec.file.info;
    (!rec.invalid && !rec.dataless && !i.deleted && i.kind == FileKind::File).then_some(i.size)
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
        if rec.invalid || rec.dataless || rec.file.info.deleted {
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

    fn peer_ids(&self) -> io::Result<Vec<ShortId>> {
        Ok(self.peers.keys().copied().collect())
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
                Op::DropWriter(d) => {
                    self.writers.remove(&d);
                }
                Op::SetNeed(p, n) => match n {
                    Some(n) => {
                        self.need.insert(p, n);
                    }
                    None => {
                        self.need.remove(&p);
                    }
                },
                Op::Shown(p, seq) => {
                    if let Some(old) = self.shown_at.insert(p.clone(), seq) {
                        self.shown.remove(&old);
                    }
                    self.shown.insert(seq, p);
                }
                Op::Retired(seq, p, fid) => {
                    self.retired.insert(seq, (p, fid));
                }
                Op::PruneRetired(upto) => {
                    self.retired.retain(|s, _| *s > upto);
                    self.floor = self.floor.max(upto);
                }
                Op::Unavailable(p, size) => match size {
                    Some(s) => {
                        self.unavailable.insert(p, s);
                    }
                    None => {
                        self.unavailable.remove(&p);
                    }
                },
            }
        }
        Ok(())
    }

    fn shown_since(&self, after: u64, limit: usize) -> io::Result<Vec<(u64, String)>> {
        Ok(self.shown.range(after + 1..).take(limit).map(|(s, p)| (*s, p.clone())).collect())
    }

    fn shown_at(&self, path: &str) -> io::Result<Option<u64>> {
        Ok(self.shown_at.get(path).copied())
    }

    fn feed_floor(&self) -> io::Result<u64> {
        Ok(self.floor)
    }

    fn unavailable(&self) -> io::Result<Vec<(String, u64)>> {
        Ok(self.unavailable.iter().map(|(p, s)| (p.clone(), *s)).collect())
    }

    fn unavailable_at(&self, path: &str) -> io::Result<Option<u64>> {
        Ok(self.unavailable.get(path).copied())
    }

    fn feeds_host(&self) -> bool {
        self.feeds_host
    }

    fn set_feeds_host(&mut self, on: bool) {
        self.feeds_host = on;
    }

    fn retired(&self, after: u64, upto: u64) -> io::Result<Vec<(u64, String, [u8; 16])>> {
        if upto <= after {
            return Ok(Vec::new());
        }
        Ok(self.retired.range(after + 1..=upto).map(|(s, (p, f))| (*s, p.clone(), *f)).collect())
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
