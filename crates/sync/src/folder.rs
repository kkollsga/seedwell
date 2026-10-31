//! One synced folder: the index, the scanner, the puller and the rules for what peers send, together.
//!
//! [`FolderCore`] is what the driver (and the simulator) talk to. It owns no threads and no sockets:
//! every method does its work and returns. The driver serialises calls per folder.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::{DeviceId, FolderId, ShortId};
use trs_sync_proto::keys::Access;
use trs_sync_proto::model::{FileInfo, FileKind, IndexEntry, STATE_DIR, SignedFile, validate_path};
use trs_sync_proto::msg::HeldFile;
use trs_sync_proto::sign::{BadWriter, Writer, WriterTable, sign_file};

use crate::archive::{self, Archived, Retention};
use crate::clock::Clock;
use crate::fs::Fs;
use crate::ignore::Ignore;
use crate::index::{Batch, IndexStore, LocalRecord, NeedRecord, PeerState};
use crate::ondemand::{self, ChangePage, Deleted, Evicted, Item, Items, Page};
use crate::pull::{ChunkSource, PullReport, Puller, Unsupported};
use crate::scan::{ScanError, ScanReport, Scanner, Signer};

/// The stamp of a file renamed as recorded: the new one (a rename changes its ctime) when its size, time and inode are
/// still the record's (`old`); `None` otherwise, so the scanner reads it again (a write between the check and the
/// rename is never taken for the recorded content).
fn restamped(old: Option<crate::index::Stamp>, m: &crate::fs::Meta) -> Option<crate::index::Stamp> {
    old.filter(|o| o.size == m.size && o.mtime_ns == m.mtime_ns && o.inode == m.inode)
        .map(|_| crate::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode })
}

/// A digest of records' paths and versions, added in the index's order (the same on every device).
#[derive(Default)]
struct Seen(sha2::Sha256);

impl Seen {
    fn add(&mut self, r: &LocalRecord) {
        use sha2::Digest;
        let i = &r.file.info;
        self.0.update((i.path.len() as u64).to_le_bytes());
        self.0.update(i.path.as_bytes());
        self.0.update((i.version.counters().len() as u64).to_le_bytes());
        for c in i.version.counters() {
            self.0.update(c.id.0.to_le_bytes());
            self.0.update(c.value.to_le_bytes());
        }
    }

    fn finish(self) -> [u8; 32] {
        use sha2::Digest;
        self.0.finalize().into()
    }
}

/// What a rename reports when no device that has the files is connected to do it.
pub const RENAME_NEEDS_HOLDER: &str = "Renaming needs a device that has these files to be connected";

/// How many sequences of retired file ids an on-demand folder keeps for its host's feed.
pub const FEED_KEPT: u64 = 1_000_000;

/// Signs this device's versions for one folder.
pub struct DeviceSigner {
    key: Arc<DeviceKey>,
    folder: FolderId,
}

impl DeviceSigner {
    /// A signer with this device key for this folder.
    pub fn new(key: Arc<DeviceKey>, folder: FolderId) -> Self {
        DeviceSigner { key, folder }
    }
}

impl Signer for DeviceSigner {
    fn sign(&self, info: FileInfo) -> SignedFile {
        sign_file(&self.key, &self.folder, info)
    }
}

/// How a folder behaves on this device.
pub struct FolderConfig {
    /// This device's id.
    pub me: DeviceId,
    /// The folder's signing public key (identifies it).
    pub folder_pk: [u8; 32],
    /// What this device may do (from its role; changed with [`FolderCore::set_membership`]).
    pub access: Access,
    /// The folder is case-insensitive (default APFS): refuse names that clash by case only.
    pub case_insensitive: bool,
    /// Names compare byte for byte on this filesystem (Linux): names not in NFC are not synced.
    pub byte_exact_names: bool,
    /// The disk keeps the executable bit ([`crate::path::probe_exec_bits`]). Off (exFAT, FAT, NTFS, many network
    /// shares, which report every file executable or none): a file keeps the bit of the version this device has,
    /// and a change of it is never read from the disk or written to it.
    pub exec_bits: bool,
    /// Archive replaced and deleted versions.
    pub archive: bool,
    /// On demand (SPEC §7G.11): the root is this device's content cache, every file is known but only the ones asked
    /// for, kept here (pinned) or changed here have their content, and the host (File Provider, Cloud Files) shows the
    /// tree and reports the user's changes ([`FolderCore::host_delete`] and the others).
    pub on_demand: bool,
}

/// Why a peer's index was refused. The peer is misbehaving or out of date; the driver closes the
/// connection with this reason.
#[derive(Debug)]
pub enum RemoteError {
    /// An entry is not signed by a writer of this folder.
    BadSignature(String),
    /// A writer could not be recorded.
    BadWriter(BadWriter),
    /// The index could not be stored.
    Io(io::Error),
}

impl From<io::Error> for RemoteError {
    fn from(e: io::Error) -> Self {
        RemoteError::Io(e)
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteError::BadSignature(p) => write!(f, "{p}: not signed by a writer of this folder"),
            RemoteError::BadWriter(e) => write!(f, "writer refused: {e}"),
            RemoteError::Io(e) => write!(f, "index: {e}"),
        }
    }
}

/// An index batch from a peer, as it came off the wire.
pub struct RemoteIndex {
    /// The peer's index id.
    pub index_id: u64,
    /// Part of a full send (everything it has), not changes since what we hold.
    pub full: bool,
    /// The last batch of a full send.
    pub done: bool,
    /// The entries.
    pub entries: Vec<IndexEntry>,
}

/// One folder on this device.
pub struct FolderCore {
    cfg: FolderConfig,
    me: ShortId,
    store: Box<dyn IndexStore>,
    fs: Arc<dyn Fs>,
    writers: WriterTable,
    device_key: Option<Arc<DeviceKey>>,
    signer: Option<DeviceSigner>,
    clock: Arc<dyn Clock>,
    new_file_id: Arc<dyn Fn() -> [u8; 16] + Send + Sync>,
    ignore: Ignore,
    /// Peers whose full send is in progress, with the index id it is of (their old entries were
    /// dropped at its start).
    receiving_full: BTreeMap<ShortId, u64>,
    /// Needed paths this system cannot make at the version they were tried at (see
    /// [`Puller::unsupported`]); kept until the folder restarts or [`Self::retry_unsupported`].
    unsupported: BTreeMap<String, Unsupported>,
    /// Joining with write access, not admitted yet (see [`FolderCore::set_awaiting_write`]).
    awaiting_write: bool,
    /// On demand: the folders (and files) kept on this device, `""` for all of it.
    pins: BTreeSet<String>,
    /// On demand: item identifiers, read from the records when first asked for.
    items: Option<Items>,
    /// On demand: the online-only files by content, for the puller.
    online_only: std::cell::RefCell<crate::pull::OnlineOnly>,
    /// The last of our sequences [`Self::follow_availability`] has looked at.
    avail_seq: u64,
    /// Devices whose index is stored here but that are no members any more ([`Self::set_members`]): their entries
    /// still decide which version is current, but they are no source or holder of anything.
    not_members: BTreeSet<ShortId>,
}

impl FolderCore {
    /// Opens a folder. `device_key` signs this device's versions while its access is Write; it is then in the
    /// writer table too. The driver keeps the table and the access in line with the folder's membership
    /// ([`FolderCore::set_membership`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: FolderConfig,
        store: Box<dyn IndexStore>,
        fs: Arc<dyn Fs>,
        device_key: Option<Arc<DeviceKey>>,
        clock: Arc<dyn Clock>,
        new_file_id: Arc<dyn Fn() -> [u8; 16] + Send + Sync>,
        ignore: Ignore,
    ) -> Result<Self, RemoteError> {
        let mut writers = WriterTable::new(cfg.folder_pk);
        for (_, w) in store.writers()? {
            writers.insert(w).map_err(RemoteError::BadWriter)?;
        }
        let me = cfg.me.short();
        let mut store = store;
        store.set_feeds_host(cfg.on_demand);
        let mut core = FolderCore {
            signer: None,
            device_key,
            cfg,
            me,
            store,
            fs,
            writers,
            clock,
            new_file_id,
            ignore,
            receiving_full: BTreeMap::new(),
            unsupported: BTreeMap::new(),
            awaiting_write: false,
            pins: BTreeSet::new(),
            items: None,
            online_only: Default::default(),
            avail_seq: 0,
            not_members: BTreeSet::new(),
        };
        if core.cfg.access == Access::Write
            && let Some(key) = core.device_key.clone()
        {
            let mut keys: Vec<[u8; 32]> = core.writers.iter().map(|w| w.device_pk).collect();
            keys.push(*key.public_key());
            core.set_membership(&keys, Access::Write)?;
        }
        // An on-demand folder knows which of its items no device can give before any batch is applied (its host
        // lists them from the start).
        if core.cfg.on_demand {
            core.availability_from_scratch(true)?;
        }
        Ok(core)
    }

    /// This device's short id.
    pub fn me(&self) -> ShortId {
        self.me
    }

    /// The folder's id.
    pub fn folder_id(&self) -> FolderId {
        self.writers.folder_id()
    }

    /// The index (read access for status and announcements).
    pub fn store(&self) -> &dyn IndexStore {
        &*self.store
    }

    /// What this device may do.
    pub fn access(&self) -> Access {
        self.cfg.access
    }

    /// Replaces the ignore patterns (the `.seedwellignore` file changed). Paths that are no longer
    /// ignored are picked up by the next scan.
    pub fn set_ignore(&mut self, ignore: Ignore) {
        self.ignore = ignore;
    }

    /// Scans `scope` (`""` = everything).
    pub fn scan(&mut self, scope: &str) -> Result<ScanReport, ScanError> {
        self.scan_with(scope, false)
    }

    fn scan_with(&mut self, scope: &str, host_write: bool) -> Result<ScanReport, ScanError> {
        let new_id = self.new_file_id.clone();
        let pins = &self.pins;
        let pinned = |p: &str| ondemand::pinned(pins, p);
        let scanner = Scanner {
            fs: &*self.fs,
            me: self.me,
            signer: self.signer.as_ref().map(|s| s as &dyn Signer),
            ignore: &self.ignore,
            byte_exact_names: self.cfg.byte_exact_names,
            case_insensitive: self.cfg.case_insensitive,
            exec_bits: self.cfg.exec_bits,
            now_s: (self.clock.now_ns() / 1_000_000_000).max(0) as u64,
            new_file_id: &*new_id,
            batch_size: 1000,
            on_demand: self.cfg.on_demand.then_some(&pinned as &dyn Fn(&str) -> bool),
            host_write,
        };
        scanner.scan(&mut *self.store, scope)
    }

    /// One pull round. `device_name` names devices in conflict copies.
    pub fn pull(&mut self, source: &dyn ChunkSource, device_name: &dyn Fn(ShortId) -> String, window: u64) -> io::Result<PullReport> {
        let pins = &self.pins;
        let wanted = |p: &str| ondemand::pinned(pins, p);
        let puller = Puller {
            fs: &*self.fs,
            me: self.me,
            signer: self.signer.as_ref().map(|s| s as &dyn Signer),
            source,
            clock: &*self.clock,
            device_name,
            case_insensitive: self.cfg.case_insensitive,
            exec_bits: self.cfg.exec_bits,
            archive: self.cfg.archive,
            window,
            unsupported: &self.unsupported,
            awaiting_write: self.awaiting_write && self.signer.is_none(),
            on_demand: self.cfg.on_demand.then_some(&wanted as &dyn Fn(&str) -> bool),
            online_only: Some(&self.online_only),
            not_members: &self.not_members,
            round: Default::default(),
        };
        let report = puller.pull(&mut *self.store)?;
        self.follow_availability(&report.holderless)?;
        if self.cfg.on_demand {
            self.prune_feed()?;
        }
        self.unsupported = report.unsupported.iter().map(|u| (u.path.clone(), u.clone())).collect();
        Ok(report)
    }

    /// The next pull tries the unsupported paths again (a rescan was asked for).
    pub fn retry_unsupported(&mut self) {
        self.unsupported.clear();
    }

    /// The need at `path` is one this system cannot make (it is not waited for).
    pub fn is_unsupported(&self, path: &str, need: &NeedRecord) -> bool {
        self.unsupported.get(path).is_some_and(|u| u.version == need.file.info.version)
    }

    /// Our entries with a sequence above `after`, at most `limit`, for a peer. A read-only device's
    /// own new files (never signed) are left out; its changed copies go out marked invalid.
    pub fn announce_since(&self, after: u64, limit: usize) -> io::Result<Vec<IndexEntry>> {
        let on_demand = self.cfg.on_demand;
        Ok(self
            .store
            .local_since(after, limit)?
            .into_iter()
            .filter(|r| !(r.invalid && r.file.info.version.is_empty()))
            .map(|r| IndexEntry { sequence: r.sequence, invalid: r.invalid, dataless: r.dataless, on_demand: on_demand && !r.dataless, file: r.file })
            .collect())
    }

    /// Our index id and the last sequence we gave out.
    pub fn index_state(&self) -> io::Result<(u64, u64)> {
        let m = self.store.meta()?;
        Ok((m.index_id, m.sequence))
    }

    /// What we hold of a peer's index (sent in `Join`, so it sends only what is newer).
    pub fn peer_state(&self, peer: ShortId) -> io::Result<PeerState> {
        Ok(self.store.peer(peer)?.unwrap_or_default())
    }

    /// Every writer.
    pub fn writers(&self) -> Vec<Writer> {
        self.writers.iter().copied().collect()
    }

    /// The folder's membership changed: these devices write (every other device's versions are refused from
    /// now on; those already stored stay), and this device has `access` (Write: it signs its versions; Read: its
    /// changes stay local). True when anything changed.
    pub fn set_membership(&mut self, writer_keys: &[[u8; 32]], access: Access) -> Result<bool, RemoteError> {
        let want: BTreeMap<ShortId, Writer> =
            writer_keys.iter().map(|pk| (DeviceId::of_public_key(pk).short(), Writer { device_pk: *pk })).collect();
        let mut table = WriterTable::new(self.cfg.folder_pk);
        for w in want.values() {
            table.insert(*w).map_err(RemoteError::BadWriter)?;
        }
        let have: BTreeMap<ShortId, Writer> = self.writers.iter().map(|w| (DeviceId::of_public_key(&w.device_pk).short(), *w)).collect();
        let mut batch = Batch::new(&*self.store, self.me)?;
        let mut changed = false;
        for (id, w) in &want {
            if have.get(id) != Some(w) {
                batch.put_writer(*id, *w);
                changed = true;
            }
        }
        for id in have.keys().filter(|id| !want.contains_key(id)) {
            batch.drop_writer(*id);
            changed = true;
        }
        if changed {
            let ops = batch.finish()?;
            self.store.apply(ops)?;
            self.writers = table;
        }
        let signs = access == Access::Write && want.contains_key(&self.me);
        if access != self.cfg.access || signs != self.signer.is_some() {
            let could_sign = self.signer.is_some();
            self.cfg.access = access;
            self.signer = signs.then(|| self.device_key.clone()).flatten().map(|k| DeviceSigner::new(k, self.writers.folder_id()));
            if !could_sign && self.signer.is_some() {
                self.claim_local_only(self.awaiting_write)?;
            }
            changed = true;
        }
        Ok(changed)
    }

    /// This device joined with an invitation that gives it write access and waits to be admitted: until it is, it
    /// cannot sign, so what it scans is recorded as local differences. Those are its user's changes as a writer: the
    /// puller leaves them alone meanwhile (it does not keep them as a read-only device's conflict copies), and once
    /// admitted they all become this device's versions ([`FolderCore::set_membership`]). The driver sets it while the
    /// invitation is pending.
    pub fn set_awaiting_write(&mut self, on: bool) {
        self.awaiting_write = on;
    }

    /// This device signs its versions (it may write and is admitted).
    pub fn signs(&self) -> bool {
        self.signer.is_some()
    }

    /// Files this device recorded while it could not write and that no device has a version of: a joining device
    /// scans the folder it joins into before it is admitted, so the files already there were kept as local
    /// differences. Now that it may write they become its own: they are sent, and one with the content another device
    /// has is merged when that version arrives, not made a conflict copy. With `all` (it waited to be admitted with
    /// write access) its changes to files it had received become its own too: the received version is kept as the
    /// base without a stamp, so the next scan reads the disk into this device's version (or only restamps it, when
    /// nothing changed). Otherwise what a read-only member recorded of files it had received stays a local difference.
    fn claim_local_only(&mut self, all: bool) -> io::Result<()> {
        let Some(signer) = self.signer.as_ref() else { return Ok(()) };
        let now_s = (self.clock.now_ns() / 1_000_000_000).max(0) as u64;
        let mut after: Option<String> = None;
        loop {
            let paths = self.store.local_paths("", after.as_deref(), 1000)?;
            let Some(last) = paths.last().cloned() else { break };
            let mut batch = Batch::new(&*self.store, self.me)?;
            for p in &paths {
                let Some(rec) = self.store.local(p)? else { continue };
                // (An online-only file is no change of ours: it stays as it is.)
                if !rec.invalid || rec.dataless {
                    continue;
                }
                if !rec.file.info.version.is_empty() {
                    if all {
                        batch.put_local(crate::index::LocalRecord { invalid: false, stamp: None, ..rec });
                    }
                    continue;
                }
                if rec.file.info.deleted {
                    continue;
                }
                let mut info = rec.file.info.clone();
                info.version = trs_sync_proto::model::Version::new().bump(self.me, now_s);
                info.modified_by = self.me;
                batch.put_local(crate::index::LocalRecord { file: signer.sign(info), sequence: 0, invalid: false, ..rec });
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
            after = Some(last);
        }
        Ok(())
    }

    /// A peer's index batch. Every entry is checked before any is stored. An entry whose path this version does
    /// not accept is skipped (a later version may send names this one refuses; the rest of the batch and the
    /// session go on, as the protocol promises: `trs_sync_proto::msg`). An entry not signed by a writer refuses
    /// the whole batch: the peer is misbehaving.
    pub fn apply_remote(&mut self, peer: ShortId, idx: RemoteIndex) -> Result<usize, RemoteError> {
        let path_ok = |p: &str| validate_path(p).is_ok() && crate::path::check_wire_path(p).is_ok();
        for e in idx.entries.iter().filter(|e| path_ok(&e.file.info.path)) {
            if !self.writers.verify(&e.file) {
                return Err(RemoteError::BadSignature(e.file.info.path.clone()));
            }
        }
        let mut dropped = false;
        let known = self.store.peer(peer)?.unwrap_or_default();
        let mut batch = Batch::new(&*self.store, self.me)?;
        let mut state = known;
        // A full send of another index than the one in progress (the peer reset its database while a
        // full send was cut off) starts over too: else the old index's entries and highest sequence
        // stay, and the new index's changes below that sequence are skipped as already held.
        if idx.full && self.receiving_full.get(&peer) != Some(&idx.index_id) {
            // A full send replaces what we had. Until it is done, we hold none of its index (so a
            // crash midway asks for a full send again).
            batch.drop_peer(peer)?;
            dropped = true;
            self.receiving_full.insert(peer, idx.index_id);
            state = PeerState { index_id: 0, max_seq: 0 };
        } else if !idx.full && known.index_id != idx.index_id {
            // Changes against an index we do not hold: ignore them (the peer sends a full index next).
            return Ok(0);
        }
        let mut stored = 0;
        let mut touched = Vec::new();
        let mut max_seq = state.max_seq;
        for e in idx.entries {
            if !idx.full && e.sequence <= known.max_seq {
                continue;
            }
            max_seq = max_seq.max(e.sequence);
            if !path_ok(&e.file.info.path) {
                continue;
            }
            touched.push(e.file.info.path.clone());
            batch.put_remote(peer, e);
            stored += 1;
        }
        let complete = !idx.full || idx.done;
        if complete {
            self.receiving_full.remove(&peer);
            batch.set_peer(peer, PeerState { index_id: idx.index_id, max_seq });
        } else {
            batch.set_peer(peer, PeerState { index_id: 0, max_seq });
        }
        let ops = batch.finish()?;
        self.store.apply(ops)?;
        // A device's entries went away (a full send starts over): an on-demand folder works its versions out anew.
        if dropped && self.cfg.on_demand {
            self.availability_from_scratch(false)?;
        }
        self.follow_availability(&touched)?;
        Ok(stored)
    }

    /// The folder's members now (`members`: every device that counts, any role). A device whose index is stored here
    /// and that is no member any more (removed, or its access ended with the folder's sharing) is no source or holder
    /// from then on (no rename holder, no device to ask before freeing, no copy for the availability report or the
    /// removal question); its entries stay, so which version is current never changes because of it, and nothing is
    /// deleted or versioned. A member that is merely offline is unchanged. Returns the devices that are no members.
    pub fn set_members(&mut self, members: &BTreeSet<ShortId>) -> io::Result<Vec<ShortId>> {
        let gone: BTreeSet<ShortId> = self.store.peer_ids()?.into_iter().filter(|d| *d != self.me && !members.contains(d)).collect();
        if gone != self.not_members {
            self.not_members = gone;
            // What they alone held is no device's now (or, let back in, theirs again). A full folder reads no records
            // for it: what is unavailable now is looked at again, and its next pull reports what only they hold.
            if self.cfg.on_demand {
                self.availability_from_scratch(false)?;
            } else {
                let now = self.store.unavailable()?.into_iter().map(|(p, _)| p).collect();
                self.evaluate_availability(now, false)?;
            }
        }
        Ok(self.not_members.iter().copied().collect())
    }

    /// Devices whose index is stored but that are no members ([`Self::set_members`]).
    pub fn not_members(&self) -> &BTreeSet<ShortId> {
        &self.not_members
    }

    /// A peer left the folder (or its key was rotated out): forget what it announced.
    pub fn forget_peer(&mut self, peer: ShortId) -> io::Result<()> {
        let mut batch = Batch::new(&*self.store, self.me)?;
        batch.drop_peer(peer)?;
        let ops = batch.finish()?;
        self.receiving_full.remove(&peer);
        self.store.apply(ops)?;
        // What it announced held holds nothing now.
        if self.cfg.on_demand {
            self.availability_from_scratch(false)?;
        }
        Ok(())
    }

    /// Whether `path` still needs pulling, so an issue about pulling it is still current.
    pub fn still_needed(&self, path: &str) -> io::Result<bool> {
        Ok(self.store.need(path)?.is_some_and(|n| n.need != crate::global::Need::Nothing))
    }

    /// How much is still needed: (paths, bytes), without the needs this system cannot make, nor the versions no device
    /// can give (counted apart: [`Self::unavailable`], worked out once it was asked for).
    pub fn need_totals(&self) -> io::Result<(u64, u64)> {
        let (mut paths, mut bytes) = self.store.need_totals()?;
        for p in self.unsupported.keys() {
            if let Some(n) = self.store.need(p)?
                && self.is_unsupported(p, &n)
            {
                paths = paths.saturating_sub(1);
                bytes = bytes.saturating_sub(n.file.info.size);
            }
        }
        for (p, _) in self.store.unavailable()? {
            if let Some(n) = self.store.need(&p)?.filter(|n| n.need != crate::global::Need::Nothing && !self.unsupported.contains_key(&p)) {
                paths = paths.saturating_sub(1);
                bytes = bytes.saturating_sub(n.file.info.size);
            }
        }
        Ok((paths, bytes))
    }

    /// The live files this device holds: (files, bytes). O(1).
    pub fn local_totals(&self) -> io::Result<(u64, u64)> {
        self.store.local_totals()
    }

    /// Deletes the archived versions that [`archive::prune`] says go (age, staggering, and the size
    /// cap against the folder's [`Self::local_totals`]). Names the archive did not make are left
    /// alone. Returns how many went.
    pub fn prune_archive(&mut self, now_ns: i64, retention: Retention) -> io::Result<usize> {
        match self.fs.lstat(STATE_DIR)? {
            Some(m) if m.kind == FileKind::Dir => {}
            _ => return Err(io::Error::new(io::ErrorKind::NotFound, "folder missing: no .seedwell marker")),
        }
        let root = format!("{STATE_DIR}/Archive");
        if self.fs.lstat(&root)?.is_none() {
            return Ok(0);
        }
        let mut items = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for e in self.fs.read_dir(&dir)?.entries {
                let at = format!("{dir}/{}", e.name);
                let Some(m) = self.fs.lstat(&at)? else { continue };
                if m.kind == FileKind::Dir {
                    stack.push(at);
                } else if let Some((of, at_ns)) = crate::conflict::archived_of(&at[root.len() + 1..]) {
                    items.push(Archived { of, at_ns, size: m.size, stored_at: at });
                }
            }
        }
        let (_, folder_bytes) = self.store.local_totals()?;
        let gone = archive::prune(&items, folder_bytes, now_ns, retention);
        for p in &gone {
            self.fs.remove_file(p)?;
        }
        Ok(gone.len())
    }

    /// The conflict copies this device holds: (copy, the path it is a conflict of). Pages through
    /// every record, so it is for a folder page that is open, not for a status line.
    pub fn conflicts(&self) -> io::Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.store.local_paths("", after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { break };
            for p in page {
                if let Some(r) = self.store.local(&p)?
                    && !r.file.info.deleted
                    && let Some(of) = r.file.info.conflict_of
                {
                    out.push((p, of));
                }
            }
            after = Some(last);
        }
        Ok(out)
    }
}

/// Why a change from the host was refused.
#[derive(Debug)]
pub enum HostError {
    /// The folder is not on demand.
    NotOnDemand,
    /// This device may not write here (a read-only member, or one not admitted yet).
    ReadOnly,
    /// No such item.
    NotFound(String),
    /// Something is there already.
    Exists(String),
    /// A name this folder cannot sync.
    BadName(String),
    /// The index or the cache could not be read or written.
    Io(io::Error),
    /// This device does not hold every file there (it records a version only with its bytes): a device that does is
    /// asked to do it.
    NeedsHolder,
    /// The files changed since the request was made: refused.
    Changed(String),
}

impl From<io::Error> for HostError {
    fn from(e: io::Error) -> Self {
        HostError::Io(e)
    }
}

impl From<ScanError> for HostError {
    fn from(e: ScanError) -> Self {
        match e {
            ScanError::FolderMissing => HostError::Io(io::Error::new(io::ErrorKind::NotFound, "folder missing: no .seedwell marker")),
            ScanError::Io(e) => HostError::Io(e),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::NotOnDemand => write!(f, "the folder is not on demand"),
            HostError::ReadOnly => write!(f, "this device may only read this folder"),
            HostError::NotFound(p) => write!(f, "{p}: no such item"),
            HostError::Exists(p) => write!(f, "{p}: something is there already"),
            HostError::BadName(p) => write!(f, "{p}: a name this folder cannot sync"),
            HostError::Io(e) => write!(f, "{e}"),
            HostError::NeedsHolder => write!(f, "{}", RENAME_NEEDS_HOLDER),
            HostError::Changed(p) => write!(f, "{p}: changed meanwhile on the device that has it; try again"),
        }
    }
}

/// What a fetch request found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested {
    /// The content is here already.
    Here,
    /// Asked for: the next pull fetches it (this many files, for a directory).
    Fetching(u64),
}

/// On-demand folders (SPEC §7G.11).
impl FolderCore {
    /// The folder is on demand.
    pub fn on_demand(&self) -> bool {
        self.cfg.on_demand
    }

    /// Switches the folder between full and on demand. To full: every online-only file is asked for, so the pulls that
    /// follow fetch the whole folder (nothing is lost meanwhile: an online-only file is never a delete). To on demand:
    /// what is here stays, downloaded, and new files arrive online only. The driver moves the root (the user's folder
    /// and the cache are different places) and saves the setting.
    pub fn set_on_demand(&mut self, on: bool) -> io::Result<u64> {
        self.cfg.on_demand = on;
        self.store.set_feeds_host(on);
        if !on {
            // Only an on-demand folder's pulls look up online-only files by content.
            self.online_only.take();
        }
        if on {
            // What is here is announced again, now as an on-demand device's (one that may let go of it), so no device
            // relies on it as on a full one's.
            let mut after: Option<String> = None;
            loop {
                let (page, next) = self.records_page("", after.as_deref())?;
                let mut batch = Batch::new(&*self.store, self.me)?;
                for r in page.into_iter().filter(|r| r.file.info.kind == FileKind::File && crate::pull::holds(r) && !r.invalid) {
                    batch.put_local(r);
                }
                let ops = batch.finish()?;
                self.store.apply(ops)?;
                match next {
                    Some(p) => after = Some(p),
                    None => return Ok(0),
                }
            }
        }
        // A version taken without its content is forgotten: the need is that version again, fetched as in any full
        // folder, and the record an earlier Seedwell reads is the one it gets then (no on-demand flags).
        self.pins.clear();
        let mut n = 0u64;
        let mut after: Option<String> = None;
        loop {
            let (page, next) = self.records_page("", after.as_deref())?;
            let mut batch = Batch::new(&*self.store, self.me)?;
            for r in page.iter().filter(|r| r.dataless) {
                batch.forget_local(&r.file.info.path);
                n += 1;
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
            match next {
                Some(p) => after = Some(p),
                None => break,
            }
        }
        Ok(n + self.reflag("", &|_| true, &BTreeSet::new())?)
    }

    /// The version at `path` that could be one no device can give: the one needed (a newer one than held here, too),
    /// or else a dataless record's.
    fn version_at(&self, path: &str) -> io::Result<Option<FileInfo>> {
        if let Some(n) = self.store.need(path)?.filter(|n| n.need != crate::global::Need::Nothing) {
            return Ok((n.file.info.kind == FileKind::File && !n.file.info.deleted).then_some(n.file.info));
        }
        Ok(self.store.local(path)?.filter(|r| r.dataless && ondemand::live(r)).map(|r| r.file.info))
    }

    /// Whether `i` is a version no device can give right now (SPEC §7G.11), conservatively: not when any device
    /// announces it (or a later one) held at its path, nor when this device holds its bytes anywhere. (A device records
    /// a version only with its bytes, so a version is held at its own path by the device that made it until a later one
    /// replaces it there.)
    fn version_unavailable(&self, i: &FileInfo) -> io::Result<bool> {
        let store = &*self.store;
        Ok(!(i.kind != FileKind::File || i.deleted || i.size == 0 || ondemand::held_at_path(store, i, &self.not_members)? || ondemand::bytes_here(store, i)?))
    }

    /// Works out again whether each of `paths` is a version no device can give, and records it with the index (the
    /// host's items read it from there). In an on-demand folder each change, either way, goes to the host's feed.
    fn evaluate_availability(&mut self, paths: BTreeSet<String>, initial: bool) -> io::Result<()> {
        let mut ops = Vec::new();
        let mut flips = Vec::new();
        for p in paths {
            let was = self.store.unavailable_at(&p)?;
            let now = match self.version_at(&p)? {
                Some(i) if self.version_unavailable(&i)? => Some(i.size),
                _ => None,
            };
            if was != now {
                ops.push(crate::index::Op::Unavailable(p.clone(), now));
                if !initial && was.is_some() != now.is_some() {
                    flips.push(p);
                }
            }
        }
        if ops.is_empty() {
            return Ok(());
        }
        let mut batch = Batch::new(&*self.store, self.me)?;
        if self.cfg.on_demand {
            for p in &flips {
                if self.store.local(p)?.is_some_and(|r| r.dataless && ondemand::live(&r)) {
                    batch.shown(p);
                }
            }
        }
        let mut all = batch.finish()?;
        all.extend(ops);
        self.store.apply(all)
    }

    /// After a change: the versions no device can give are worked out again where the change can have moved them,
    /// either way: at `touched` (paths announced by a peer, or needed with no device announcing them held) and at this
    /// device's records changed since the last look (an on-demand folder: every one; a full folder: only those that
    /// are unavailable now, and none read at all while nothing is). Each path costs a few lookups: linear in the
    /// change.
    fn follow_availability(&mut self, touched: &[String]) -> io::Result<()> {
        if !self.cfg.on_demand {
            return self.follow_availability_full(touched);
        }
        let mut paths: BTreeSet<String> = touched.iter().cloned().collect();
        loop {
            let page = self.store.local_since(self.avail_seq, 1000)?;
            let Some(last) = page.last().map(|r| r.sequence) else { break };
            paths.extend(page.into_iter().map(|r| r.file.info.path));
            self.avail_seq = last;
        }
        self.evaluate_availability(paths, false)
    }

    /// [`Self::follow_availability`] of a full folder, from what is cheap: the touched paths' needs and marks, read in
    /// one go, keep only those that may be or may have been unavailable (a need no member holds, or marked now); its
    /// own records are read only while something is marked.
    fn follow_availability_full(&mut self, touched: &[String]) -> io::Result<()> {
        let any = self.store.any_unavailable()?;
        let mut paths = BTreeSet::new();
        for chunk in touched.chunks(1000) {
            for (p, (need, marked)) in chunk.iter().zip(self.store.needs_at(chunk)?) {
                let lone = need.is_some_and(|n| {
                    n.need != crate::global::Need::Nothing && n.file.info.kind == FileKind::File && !n.file.info.deleted && n.holders.iter().all(|d| self.not_members.contains(d))
                });
                if lone || marked {
                    paths.insert(p.clone());
                }
            }
        }
        if !any {
            self.avail_seq = self.store.meta()?.sequence;
        } else {
            loop {
                let page = self.store.local_since(self.avail_seq, 1000)?;
                let Some(last) = page.last().map(|r| r.sequence) else { break };
                let changed: Vec<String> = page.into_iter().map(|r| r.file.info.path).collect();
                for (p, (_, marked)) in changed.iter().zip(self.store.needs_at(&changed)?) {
                    if marked {
                        paths.insert(p.clone());
                    }
                }
                self.avail_seq = last;
            }
        }
        self.evaluate_availability(paths, false)
    }

    /// Every need no member announces held, and in an on-demand folder every online-only file, worked out anew (at open,
    /// when a device's entries went away, when the members changed).
    fn availability_from_scratch(&mut self, at_open: bool) -> io::Result<()> {
        // A page at a time: what is unavailable now, the online-only files, the needs no device announces held.
        self.avail_seq = self.store.meta()?.sequence;
        let now: BTreeSet<String> = self.store.unavailable()?.into_iter().map(|(p, _)| p).collect();
        self.evaluate_availability(now, at_open)?;
        let mut after: Option<String> = None;
        while self.cfg.on_demand {
            let (page, next) = self.records_page("", after.as_deref())?;
            let paths = page.into_iter().filter(|r| r.dataless && ondemand::live(r)).map(|r| r.file.info.path).collect();
            self.evaluate_availability(paths, at_open)?;
            match next {
                Some(p) => after = Some(p),
                None => break,
            }
        }
        let mut after: Option<String> = None;
        loop {
            let page = self.store.needs(after.as_deref(), 1000)?;
            let Some((last, _)) = page.last().cloned() else { break };
            let paths = page
                .into_iter()
                .filter(|(_, n)| {
                    n.holders.iter().all(|d| self.not_members.contains(d))
                        && n.need != crate::global::Need::Nothing
                        && n.file.info.kind == FileKind::File
                        && !n.file.info.deleted
                })
                .map(|(p, _)| p)
                .collect();
            self.evaluate_availability(paths, at_open)?;
            after = Some(last);
        }
        Ok(())
    }

    /// The versions no device can give here right now (SPEC §7G.11), with their sizes: counted apart from what is
    /// needed, shown, never removed on their own (only a user's delete of the item removes one).
    pub fn unavailable(&self) -> io::Result<Vec<(String, u64)>> {
        self.store.unavailable()
    }

    /// Retired file ids are kept for the host's feed for the last [`FEED_KEPT`] sequences; older ones go, and an anchor
    /// from before them has expired (the host lists again).
    fn prune_feed(&mut self) -> io::Result<()> {
        let (floor, now) = (self.store.feed_floor()?, self.store.meta()?.sequence);
        if now > floor.saturating_add(2 * FEED_KEPT) {
            self.store.apply(vec![crate::index::Op::PruneRetired(now - FEED_KEPT)])?;
        }
        Ok(())
    }

    /// The paths kept on this device (pinned).
    pub fn pins(&self) -> &BTreeSet<String> {
        &self.pins
    }

    /// Sets the pins at open (from the folder's settings), without touching the records (they kept their flags).
    pub fn set_pins(&mut self, pins: impl IntoIterator<Item = String>) {
        self.pins = pins.into_iter().collect();
    }

    fn now_s(&self) -> u64 {
        (self.clock.now_ns() / 1_000_000_000).max(0) as u64
    }

    /// Every record at or below `path`, in path order (for what needs them all: a delete or a rename of them).
    fn records_under(&self, path: &str) -> io::Result<Vec<LocalRecord>> {
        let mut out = Vec::new();
        self.each_under(path, |r| {
            out.push(r);
            Ok(())
        })?;
        Ok(out)
    }

    /// The records at or below `path` after `after`, a page (at most 1000) in path order, and the path the next page
    /// starts after (`None`: none follows). A walk of a whole folder holds one page at a time.
    fn records_page(&self, path: &str, after: Option<&str>) -> io::Result<(Vec<LocalRecord>, Option<String>)> {
        let page = self.store.local_paths(path, after, 1000)?;
        let mut out = Vec::with_capacity(page.len());
        for p in &page {
            if let Some(r) = self.store.local(p)? {
                out.push(r);
            }
        }
        Ok((out, page.last().cloned()))
    }

    /// Calls `f` with every record at or below `path`, in path order, a page at a time.
    fn each_under(&self, path: &str, mut f: impl FnMut(LocalRecord) -> io::Result<()>) -> io::Result<()> {
        let mut after: Option<String> = None;
        loop {
            let (page, next) = self.records_page(path, after.as_deref())?;
            for r in page {
                f(r)?;
            }
            match next {
                Some(n) => after = Some(n),
                None => return Ok(()),
            }
        }
    }

    /// Sets `want` on every online-only file at or below `path` to what `want` says (so their needs are computed
    /// again), and gives every item there that shows something else now (on its way, kept on this device: `before` is
    /// the pins as they were) a new sequence, so the host hears of it. Returns how many online-only files changed.
    fn reflag(&mut self, path: &str, want: &dyn Fn(&str) -> bool, before: &BTreeSet<String>) -> io::Result<u64> {
        let mut n = 0;
        let mut after: Option<String> = None;
        loop {
            let (page, next) = self.records_page(path, after.as_deref())?;
            let mut batch = Batch::new(&*self.store, self.me)?;
            for r in page.into_iter().filter(ondemand::live) {
                let p = &r.file.info.path;
                let w = if r.dataless { want(p) } else { r.want };
                if w != r.want || ondemand::pinned(before, p) != ondemand::pinned(&self.pins, p) {
                    n += u64::from(w != r.want);
                    batch.reflag_local(LocalRecord { want: w, ..r });
                }
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
            match next {
                Some(p) => after = Some(p),
                None => return Ok(n),
            }
        }
    }

    /// Keeps `path` (a file or a folder, `""` for all) on this device: its online-only files are fetched, and so is
    /// every file that arrives below it later. The driver saves the pins with the folder's settings.
    pub fn pin(&mut self, path: &str) -> Result<u64, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        let before = self.pins.clone();
        self.pins.insert(path.to_string());
        Ok(self.reflag(path, &|_| true, &before)?)
    }

    /// No longer keeps `path` on this device. What is here stays (it can be removed with [`Self::evict`]); online-only
    /// files below it that were on their way are not fetched any more, unless another pin covers them.
    pub fn unpin(&mut self, path: &str) -> Result<u64, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        let before = self.pins.clone();
        self.pins.remove(path);
        let pins = self.pins.clone();
        Ok(self.reflag(path, &|p| ondemand::pinned(&pins, p), &before)?)
    }

    /// Asks for the content of `path` (a file, or every file below a folder): the next pull fetches it, ahead of
    /// nothing else (an on-demand folder pulls only what is wanted). [`Self::cancel_request`] takes it back.
    pub fn request(&mut self, path: &str) -> Result<Requested, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        let Some(rec) = self.store.local(path)?.filter(ondemand::live) else {
            return Err(HostError::NotFound(path.to_string()));
        };
        if rec.file.info.kind != FileKind::Dir && !rec.dataless {
            return Ok(Requested::Here);
        }
        let pins = self.pins.clone();
        let n = self.reflag(path, &|_| true, &pins)?;
        let mut fetching = 0u64;
        self.each_under(path, |r| {
            fetching += u64::from(r.dataless && r.want);
            Ok(())
        })?;
        Ok(if n == 0 && fetching == 0 { Requested::Here } else { Requested::Fetching(fetching) })
    }

    /// Takes back a request for `path` (a fetch the host cancelled): not fetched any more unless pinned.
    pub fn cancel_request(&mut self, path: &str) -> Result<u64, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        let pins = self.pins.clone();
        Ok(self.reflag(path, &|p| ondemand::pinned(&pins, p), &pins)?)
    }

    /// The live children of the directory `dir` (`""`: the top), in path order.
    fn children_of(&self, dir: &str) -> io::Result<Vec<String>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        'pages: loop {
            let page = self.store.local_paths(dir, after.as_deref(), 1000)?;
            if page.is_empty() {
                return Ok(out);
            }
            for p in page {
                let Some(rest) = p.strip_prefix(&prefix).filter(|r| !r.is_empty()) else {
                    after = Some(p);
                    continue;
                };
                if let Some(slash) = rest.find('/') {
                    // A grandchild: everything below that child is skipped at once.
                    after = Some(format!("{}/\u{10FFFF}", &p[..prefix.len() + slash]));
                    continue 'pages;
                }
                if self.store.local(&p)?.is_some_and(|r| ondemand::live(&r)) {
                    out.push(p.clone());
                }
                after = Some(p);
            }
        }
    }

    /// Lets `path` go from the folders kept on this device above it (OneDrive's rule): each such pin is split, i.e.
    /// every pinned folder from the outermost one down to `path` is unpinned, and its other children (files and
    /// folders, at each level) are pinned on their own, so everything else stays kept and `path` is not. Files added
    /// to those folders later are not kept any more. A pin of `path` itself stays. Items whose flags change get a new
    /// sequence (the host hears of them). Returns whether anything changed.
    fn split_pins_above(&mut self, path: &str) -> io::Result<bool> {
        let above: Vec<String> = self.pins.iter().filter(|p| p.as_str() != path && ondemand::under(path, p)).cloned().collect();
        let Some(top) = above.iter().min_by_key(|p| p.len()).cloned() else { return Ok(false) };
        let before = self.pins.clone();
        for p in &above {
            self.pins.remove(p);
        }
        let mut dir = top.clone();
        loop {
            let rest = if dir.is_empty() { path } else { &path[dir.len() + 1..] };
            let next = match rest.find('/') {
                Some(n) => path[..path.len() - rest.len() + n].to_string(),
                None => path.to_string(),
            };
            for c in self.children_of(&dir)? {
                if c != next {
                    self.pins.insert(c);
                }
            }
            if next == path {
                break;
            }
            dir = next;
        }
        let pins = self.pins.clone();
        self.reflag(&top, &|p| ondemand::pinned(&pins, p), &before)?;
        Ok(true)
    }

    /// Removes the content of `path` (a file, or the files below a folder) from this device; the files stay, online
    /// only. A pin of a folder above `path` is split ([`Self::split_pins_above`]), so `path` can go and the rest stays
    /// kept (not when nothing below `path` can go). Kept, with the reason: files pinned themselves (the user unpins them
    /// first), and files no other device that has them is connected for: `live` are the devices connected now whose
    /// index this session brought up to date ([`ondemand::held_elsewhere`]). A file that cannot be removed (open
    /// elsewhere) stays downloaded and is reported; the others go all the same. The driver saves the pins with the
    /// folder's settings.
    pub fn evict(&mut self, path: &str, confirmed: &[HeldFile]) -> Result<Evicted, HostError> {
        self.free(path, false, confirmed)
    }

    /// What [`Self::evict`] (`unpinning`: a read-only member's delete, [`Self::host_delete`]) at `path` could free
    /// once another device confirms it holds them: the files (as recorded here), and what is kept now and why. The
    /// driver asks the devices that announce them held ([`Self::holders_of`], [`Self::holds`] on their side) and passes
    /// what they confirmed.
    ///
    /// From here until the free is over the driver answers no for them ([`Self::holds`]'s `freeing`), so two devices
    /// freeing at once never both rely on the other, whatever their ranks were when they were asked.
    pub fn free_candidates(&self, path: &str, unpinning: bool) -> Result<(Vec<HeldFile>, Evicted), HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        if !path.is_empty() && self.store.local(path)?.filter(ondemand::live).is_none() {
            return Err(HostError::NotFound(path.to_string()));
        }
        // Each as a question asks about it (its version and a digest of its content): not its chunk list.
        let (free, out) = self.evict_plan(path, unpinning, None)?;
        let held = free.into_iter().map(|r| HeldFile { content: r.file.info.content_digest(), version: r.file.info.version, path: r.file.info.path }).collect();
        Ok((held, out))
    }

    /// The devices that announce `f` held (valid, with its content, this version or a later one): the ones to ask.
    pub fn holders_of(&self, f: &HeldFile) -> io::Result<Vec<ShortId>> {
        Ok(self
            .store
            .remote(&f.path)?
            .into_iter()
            .filter(|(d, e)| {
                let i = &e.file.info;
                !self.not_members.contains(d)
                    && !e.invalid
                    && !e.dataless
                    && !i.deleted
                    && match i.version.compare(&f.version) {
                        trs_sync_proto::model::Causality::Equal => i.content_digest() == f.content,
                        trs_sync_proto::model::Causality::Greater => true,
                        _ => false,
                    }
            })
            .map(|(d, _)| d)
            .collect())
    }

    /// A device about to let go of its copies asks whether this one holds them now (SPEC §7G.11): yes for a file whose
    /// content this device holds (valid, not dataless) at this version or a later one, and, when this folder is on
    /// demand too, only if this device's id sorts above the asker's (so of two on-demand holders the higher never
    /// relies on the lower; a chain ends at a full device or the top on-demand one).
    ///
    /// Never from a read-only member (a change it makes is its own, never sent: the version would be held nowhere),
    /// from a folder whose marker is gone (moved to the Trash, a disk unmounted), for a file whose disk differs from
    /// its record (as the scanner reads it), or for a file this device is letting go of itself right now. The driver
    /// answers no for a folder that is not running (paused, missing).
    pub fn holds(&self, asker: ShortId, files: &[HeldFile], freeing: &dyn Fn(&str) -> bool) -> Vec<bool> {
        let holder = ondemand::Holder {
            store: &*self.store,
            fs: &*self.fs,
            me: self.me,
            writer: self.signer.is_some(),
            on_demand: self.cfg.on_demand,
            freeing,
            ignore: &self.ignore,
        };
        holder.answer(asker, files)
    }

    /// Frees what [`Self::free_candidates`] found at `path` and another device `confirmed` it holds (asked right
    /// before); everything else is kept with the reason. A pin of a folder above `path` is split when something goes
    /// ([`Self::split_pins_above`]); `unpinning` drops the pins at or below `path` too.
    pub fn free(&mut self, path: &str, unpinning: bool, confirmed: &[HeldFile]) -> Result<Evicted, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        if !path.is_empty() && self.store.local(path)?.filter(ondemand::live).is_none() {
            return Err(HostError::NotFound(path.to_string()));
        }
        // What can go is judged first, pins above `path` left out (they are split for it): when nothing can, the pins
        // stay as they are.
        let (free, mut out) = self.evict_plan(path, unpinning, Some(confirmed))?;
        if !free.is_empty() {
            if !self.pins.contains(path) || unpinning {
                self.split_pins_above(path)?;
            }
            if unpinning {
                let before = self.pins.clone();
                self.pins.retain(|p| !ondemand::under(p, path));
                let pins = self.pins.clone();
                self.reflag(path, &|p| ondemand::pinned(&pins, p), &before)?;
            }
        }
        self.evict_apply(free, &mut out)?;
        Ok(out)
    }

    /// The files at or below `path` whose content may go, and what is kept and why. Kept: files pinned at or below
    /// `path` (a pin above it is split instead; `unpinning`: those count for nothing either), files changed in the
    /// cache since they were recorded, and (with `confirmed`) files no other device confirmed it holds.
    fn evict_plan(&self, path: &str, unpinning: bool, confirmed: Option<&[HeldFile]>) -> io::Result<(Vec<LocalRecord>, Evicted)> {
        let mut out = Evicted::default();
        let mut free = Vec::new();
        let pins_here: BTreeSet<String> = self.pins.iter().filter(|p| !unpinning && ondemand::under(p, path)).cloned().collect();
        let confirmed: Option<std::collections::HashMap<&str, &HeldFile>> = confirmed.map(|c| c.iter().map(|h| (h.path.as_str(), h)).collect());
        self.each_under(path, |r| {
            let i = &r.file.info;
            if i.kind != FileKind::File || i.deleted || r.dataless || r.own_change() || !crate::pull::holds(&r) {
                return Ok(());
            }
            let why = if ondemand::pinned(&pins_here, &i.path) {
                Some("kept on this device (pinned): unpin it first")
            } else if confirmed.as_ref().is_some_and(|c| !c.get(i.path.as_str()).is_some_and(|h| h.version == i.version && h.content == i.content_digest())) {
                Some(ondemand::NOT_HELD_ELSEWHERE)
            } else if let Ok(Some(m)) = self.fs.lstat(&i.path)
                && r.stamp != Some(crate::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode })
            {
                // Only the file we recorded: one changed since (not scanned yet) is a change. (A disk that cannot be
                // read now is reported when the file is removed.)
                Some("changed here since it was recorded")
            } else {
                None
            };
            match why {
                Some(why) => {
                    out.kept += 1;
                    out.kept_why.push((i.path.clone(), why.to_string()));
                }
                None => free.push(r),
            }
            Ok(())
        })?;
        Ok((free, out))
    }

    /// Removes the content of `free` from the cache; each record follows what happened on disk.
    fn evict_apply(&mut self, free: Vec<LocalRecord>, out: &mut Evicted) -> io::Result<()> {
        let mut gone = Vec::new();
        for r in free {
            let i = &r.file.info;
            let removed = match self.fs.lstat(&i.path) {
                Ok(Some(_)) => self.fs.remove_file(&i.path),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            match removed {
                Ok(()) => {
                    out.files += 1;
                    out.bytes += i.size;
                    gone.push(LocalRecord { invalid: true, stamp: None, tier: None, dataless: true, want: false, ..r });
                }
                Err(e) => out.failed.push((i.path.clone(), e.to_string())),
            }
        }
        for chunk in gone.chunks(1000) {
            let mut batch = Batch::new(&*self.store, self.me)?;
            for r in chunk {
                batch.put_local(r.clone());
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
        }
        Ok(())
    }

    /// A delete from the host frees the content instead (a read-only member's, [`Self::host_delete`]): the driver asks
    /// the holders first, as for an evict ([`Self::free_candidates`] with `unpinning`).
    pub fn frees_on_delete(&self) -> bool {
        self.cfg.on_demand && self.signer.is_none()
    }

    fn host_check(&self, path: &str) -> Result<(), HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        if self.signer.is_none() {
            return Err(HostError::ReadOnly);
        }
        if !path.is_empty() && (validate_path(path).is_err() || crate::path::check_wire_path(path).is_err()) {
            return Err(HostError::BadName(path.to_string()));
        }
        Ok(())
    }

    /// The folders above `path` are folders or not there yet (the host makes them first), never files.
    fn host_check_parents(&self, path: &str) -> Result<(), HostError> {
        let mut at = path;
        while let Some(n) = at.rfind('/') {
            at = &at[..n];
            if self.store.local(at)?.filter(ondemand::live).is_some_and(|r| r.file.info.kind != FileKind::Dir) {
                return Err(HostError::Exists(at.to_string()));
            }
        }
        Ok(())
    }

    /// Would [`Self::host_written`] take `path` (a directory with `dir`)? The driver asks before it touches the cache,
    /// so a refused change leaves nothing behind.
    pub fn host_check_write(&self, path: &str, dir: bool) -> Result<(), HostError> {
        self.host_check(path)?;
        if path.is_empty() || path.split('/').next() == Some(STATE_DIR) {
            return Err(HostError::BadName(path.to_string()));
        }
        self.host_check_parents(path)?;
        match self.store.local(path)?.filter(ondemand::live) {
            Some(r) if (r.file.info.kind == FileKind::Dir) != dir => Err(HostError::Exists(path.to_string())),
            _ => Ok(()),
        }
    }

    /// The host wrote `path` in the cache (a new file or directory, or new content): it becomes this device's
    /// version, as a scan of it would make. The driver puts the content in place first (written aside and renamed).
    pub fn host_written(&mut self, path: &str) -> Result<(), HostError> {
        self.host_check(path)?;
        self.scan_with(path, true)?;
        self.follow_availability(&[])?;
        Ok(())
    }

    /// The host made the directory `path` in the cache: it becomes this device's version. Only the directory itself is the
    /// host's: anything found below it is scanned as any scan does (content at an online-only path goes to the
    /// archive), never taken as the host's write.
    pub fn host_made_dir(&mut self, path: &str) -> Result<(), HostError> {
        self.host_check(path)?;
        self.record_dir(path)
    }

    /// Records the directory at `path` (it is on disk) as this device's version, itself only.
    fn record_dir(&mut self, path: &str) -> Result<(), HostError> {
        let Some(m) = self.fs.lstat(path)?.filter(|m| m.kind == FileKind::Dir) else {
            return Err(HostError::NotFound(path.to_string()));
        };
        let base = self.store.local(path)?.map(|r| r.file.info).filter(|b| !b.deleted || b.kind == FileKind::Dir);
        if base.as_ref().is_some_and(|b| !b.deleted && b.kind == FileKind::Dir) {
            return Ok(());
        }
        let signer = self.signer.as_ref().ok_or(HostError::ReadOnly)?;
        let info = FileInfo {
            path: path.to_string(),
            kind: FileKind::Dir,
            size: 0,
            mtime_ns: 0,
            exec: false,
            deleted: false,
            version: base.as_ref().map(|b| b.version.clone()).unwrap_or_default().bump(self.me, self.now_s()),
            modified_by: self.me,
            file_id: (self.new_file_id)(),
            symlink_target: String::new(),
            conflict_of: None,
            chunks: Vec::new(),
        };
        let stamp = crate::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode };
        let mut batch = Batch::new(&*self.store, self.me)?;
        batch.put_local(LocalRecord { file: signer.sign(info), sequence: 0, invalid: false, stamp: Some(stamp), tier: None, dataless: false, want: false });
        let ops = batch.finish()?;
        self.store.apply(ops)?;
        Ok(())
    }

    /// The user deleted `path` (and everything below it): every record there becomes a delete of this device, and
    /// the cache lets go of their content. What cannot be removed from the cache (open elsewhere) stays, with the
    /// folders above it, and is reported. On a read-only member (it cannot delete for everyone) the delete frees the
    /// content instead, as [`Self::evict`] does, after unpinning what is pinned there: [`Deleted::Freed`].
    pub fn host_delete(&mut self, path: &str, confirmed: &[HeldFile]) -> Result<Deleted, HostError> {
        if self.frees_on_delete() && !path.is_empty() {
            // Pinned itself or below: unpinned; pinned by a folder above: that pin is split. Only when something can
            // go: otherwise every pin stays.
            return Ok(Deleted::Freed(self.free(path, true, confirmed)?));
        }
        self.host_check(path)?;
        if path.is_empty() {
            return Err(HostError::BadName(path.to_string()));
        }
        let recs: Vec<LocalRecord> = self.records_under(path)?.into_iter().filter(ondemand::live).collect();
        if recs.is_empty() {
            return Err(HostError::NotFound(path.to_string()));
        }
        // The cache first, deepest first: a crash after this only leaves files online only (the scan's rule).
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut gone = Vec::with_capacity(recs.len());
        for r in recs.iter().rev() {
            let p = &r.file.info.path;
            // A folder with something below it that stayed stays too.
            if failed.iter().any(|(f, _)| ondemand::under(f, p)) {
                continue;
            }
            let removed = (|| -> io::Result<()> {
                match self.fs.lstat(p)? {
                    Some(m) if m.kind == FileKind::Dir => {
                        // What is left in it the host never saw (ignored, unknown, a temp): kept in the archive, never
                        // deleted for good. (What it saw went before, deepest first.)
                        for e in self.fs.read_dir(p)?.entries {
                            self.archive_untracked(&format!("{p}/{}", e.name))?;
                        }
                        self.fs.remove_dir(p)
                    }
                    Some(_) => self.fs.remove_file(p),
                    None => Ok(()),
                }
            })();
            match removed {
                Ok(()) => gone.push(r.clone()),
                Err(e) => failed.push((p.clone(), e.to_string())),
            }
        }
        let (now_s, me) = (self.now_s(), self.me);
        let signer = self.signer.as_ref().map(|s| s as &dyn crate::scan::Signer);
        let items = gone.len() as u64;
        for chunk in gone.chunks(1000) {
            let mut batch = Batch::new(&*self.store, self.me)?;
            for r in chunk {
                batch.put_local(crate::scan::tombstone(signer, me, now_s, r.clone()));
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
        }
        // A pin of what went goes with it.
        let deleted: BTreeSet<&str> = gone.iter().map(|r| r.file.info.path.as_str()).collect();
        self.pins.retain(|p| !deleted.contains(p.as_str()));
        self.follow_availability(&[])?;
        Ok(Deleted::Deleted { items, failed })
    }

    /// Moves `at` (a file, link or folder the index does not know) into the archive, as a replaced file is kept.
    fn archive_untracked(&self, at: &str) -> io::Result<()> {
        let stamp = crate::clock::compact_stamp(self.clock.now_ns());
        let mut to = crate::conflict::archive_path(at, &stamp);
        let mut n = 2;
        while self.fs.lstat(&to)?.is_some() {
            to = crate::conflict::archive_path(at, &format!("{stamp}-{n}"));
            n += 1;
        }
        let mut dir = String::new();
        for c in to.split('/').collect::<Vec<_>>().split_last().map_or(&[][..], |(_, d)| d) {
            if !dir.is_empty() {
                dir.push('/');
            }
            dir.push_str(c);
            if self.fs.lstat(&dir)?.is_none() {
                self.fs.create_dir(&dir)?;
            }
        }
        self.fs.rename(at, &to)
    }

    /// Where the host's new content for `path` goes (`base`: the item's `content_version` the user's edit started
    /// from; `None`: a new file). Content, not metadata: a rename or a new version with the same content (a merge, an
    /// absorbed delete) is no conflict. `path` itself, unless the item moved on since (another device's
    /// version arrived first) or a new file's path is taken now: then a conflict copy, named as a full folder names
    /// one (`device`: this device's name), and the version there stays. Checked as [`Self::host_check_write`].
    pub fn host_target(&self, path: &str, base: Option<&str>, device: &str) -> Result<String, HostError> {
        self.host_check_write(path, false)?;
        let Some(r) = self.store.local(path)?.filter(ondemand::live) else { return Ok(path.to_string()) };
        if base.is_some_and(|b| b == ondemand::content_version(&r.file.info)) {
            return Ok(path.to_string());
        }
        let stamp = crate::clock::utc_stamp(self.clock.now_ns());
        for n in 1.. {
            let copy = crate::conflict::conflict_path(path, device, &stamp, n);
            if self.store.local(&copy)?.filter(ondemand::live).is_none() && self.fs.lstat(&copy)?.is_none() {
                return Ok(copy);
            }
        }
        unreachable!("a free conflict copy name")
    }

    /// The host's content for `path` was written at `copy` (a conflict copy, [`Self::host_target`]): recorded as
    /// this device's new file, a conflict copy of `path`.
    pub fn host_written_copy(&mut self, copy: &str, path: &str) -> Result<(), HostError> {
        self.host_written(copy)?;
        let Some(r) = self.store.local(copy)?.filter(|r| ondemand::live(r) && r.file.info.conflict_of.is_none()) else { return Ok(()) };
        let signer = self.signer.as_ref().ok_or(HostError::ReadOnly)?;
        let info = FileInfo { conflict_of: Some(path.to_string()), ..r.file.info.clone() };
        let mut batch = Batch::new(&*self.store, self.me)?;
        batch.put_local(LocalRecord { file: signer.sign(info), ..r });
        let ops = batch.finish()?;
        self.store.apply(ops)?;
        Ok(())
    }

    /// Would [`Self::host_rename`] take `from` → `to`? Returns the records that move. The driver asks before it
    /// touches the cache.
    pub fn host_check_rename(&self, from: &str, to: &str) -> Result<Vec<LocalRecord>, HostError> {
        if !self.cfg.on_demand {
            return Err(HostError::NotOnDemand);
        }
        self.rename_check(from, to)
    }

    /// Where a rename to `to` lands: on a disk that folds case, the folders above it as they are named already (in the
    /// index, else on disk), so the renamed records name them as the disk does; its last name as asked.
    pub fn rename_target(&self, to: &str) -> io::Result<String> {
        let Some((dirs, leaf)) = to.rsplit_once('/').filter(|_| self.cfg.case_insensitive) else { return Ok(to.to_string()) };
        let mut at = String::new();
        for c in dirs.split('/') {
            let lower = c.to_lowercase();
            let recorded = self.children_of(&at)?.into_iter().find(|p| p.rsplit('/').next().is_some_and(|n| n.to_lowercase() == lower));
            let name = match recorded {
                Some(p) => p.rsplit('/').next().unwrap_or(c).to_string(),
                None => match self.fs.read_dir(&at) {
                    Ok(l) => l.entries.into_iter().map(|e| e.name).find(|n| n.to_lowercase() == lower).unwrap_or_else(|| c.to_string()),
                    Err(_) => c.to_string(),
                },
            };
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(&name);
        }
        Ok(format!("{at}/{leaf}"))
    }

    /// The checks of a rename (here or for a peer): a writer, names this folder can sync, `to` not inside `from` nor
    /// taken, the folders above `to` folders. Returns the records that move.
    fn rename_check(&self, from: &str, to: &str) -> Result<Vec<LocalRecord>, HostError> {
        self.rename_check_onto(from, to, false)
    }

    /// [`Self::rename_check`]; `replacing`: what is at `to` is about to go (checked by the caller).
    fn rename_check_onto(&self, from: &str, to: &str, replacing: bool) -> Result<Vec<LocalRecord>, HostError> {
        if self.signer.is_none() {
            return Err(HostError::ReadOnly);
        }
        for p in [from, to] {
            if p.is_empty() || validate_path(p).is_err() || crate::path::check_wire_path(p).is_err() || p.split('/').next() == Some(STATE_DIR) {
                return Err(HostError::BadName(p.to_string()));
            }
        }
        if ondemand::under(to, from) {
            return Err(HostError::BadName(to.to_string()));
        }
        let recs: Vec<LocalRecord> = self.records_under(from)?.into_iter().filter(ondemand::live).collect();
        if recs.is_empty() {
            return Err(HostError::NotFound(from.to_string()));
        }
        self.host_check_parents(to)?;
        if replacing {
            return Ok(recs);
        }
        if self.store.local(to)?.is_some_and(|r| ondemand::live(&r)) {
            return Err(HostError::Exists(to.to_string()));
        }
        // A case-only rename on a case-insensitive disk finds `from` at `to`: the same file, which is fine.
        if let Some(t) = self.fs.lstat(to)?
            && !self.fs.lstat(from)?.is_some_and(|f| f.inode == t.inode)
        {
            return Err(HostError::Exists(to.to_string()));
        }
        Ok(recs)
    }

    /// This device holds every one of `recs` (SPEC §7G.11: a device records a version only if it holds its bytes): valid,
    /// its content on disk as recorded (a folder: there).
    fn holds_all(&self, recs: &[LocalRecord]) -> io::Result<bool> {
        for r in recs {
            let i = &r.file.info;
            let Some(m) = self.fs.lstat(&i.path)? else { return Ok(false) };
            let held = match i.kind {
                FileKind::Dir => m.kind == FileKind::Dir,
                _ => {
                    !r.dataless
                        && !r.invalid
                        && crate::pull::holds(r)
                        && r.stamp == Some(crate::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode })
                }
            };
            if !held {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// What a rename of `from` is of here: a digest of every live record's path and version there, for the device that
    /// holds them to check it renames what this one saw ([`Self::rename_for_peer`]).
    pub fn rename_seen(&self, from: &str) -> io::Result<[u8; 32]> {
        let mut h = Seen::default();
        self.each_under(from, |r| {
            if ondemand::live(&r) {
                h.add(&r);
            }
            Ok(())
        })?;
        Ok(h.finish())
    }

    /// The devices that announce every live record at or below `from` held, at the version this device has (valid,
    /// with the content, not deleted): those that can rename it ([`Self::rename_for_peer`]). Full devices first.
    pub fn rename_holders(&self, from: &str) -> io::Result<Vec<ShortId>> {
        let mut holders: Option<BTreeMap<ShortId, bool>> = None;
        self.each_under(from, |r| {
            let i = &r.file.info;
            if !ondemand::live(&r) || holders.as_ref().is_some_and(BTreeMap::is_empty) {
                return Ok(());
            }
            let here: BTreeMap<ShortId, bool> = self
                .store
                .remote(&i.path)?
                .into_iter()
                .filter(|(d, e)| !self.not_members.contains(d) && !e.invalid && !e.dataless && !e.file.info.deleted && e.file.info.version == i.version)
                .map(|(d, e)| (d, e.on_demand))
                .collect();
            holders = Some(match holders.take() {
                None => here,
                Some(h) => h.into_iter().filter(|(d, _)| here.contains_key(d)).collect(),
            });
            Ok(())
        })?;
        let mut out: Vec<(ShortId, bool)> = holders.unwrap_or_default().into_iter().collect();
        out.sort_by_key(|(d, on_demand)| (*on_demand, *d));
        Ok(out.into_iter().map(|(d, _)| d).collect())
    }

    /// The user renamed or moved `from` to `to` (and everything below it): the same files under their new names, as
    /// versions of this device (their file ids kept, so the host's items keep their identifiers), and deletes of the
    /// old names. Only when this device holds every file there (a version is recorded only by a device that holds its
    /// bytes): otherwise [`HostError::NeedsHolder`], and the driver asks a device that holds them to rename them
    /// ([`Self::rename_for_peer`] there).
    pub fn host_rename(&mut self, from: &str, to: &str) -> Result<u64, HostError> {
        let to = &self.rename_target(to)?;
        let recs = self.host_check_rename(from, to)?;
        if !self.holds_all(&recs)? {
            return Err(HostError::NeedsHolder);
        }
        self.rename_held(from, to, recs, true)
    }

    /// The user keeps the file `from` in place of the file `to` (a conflict resolved for its copy): `to` goes to the
    /// archive and is deleted, then `from` takes its name. Both must be files held here; everything is checked before
    /// anything changes, so a refusal leaves both as they were.
    pub fn host_replace(&mut self, from: &str, to: &str) -> Result<u64, HostError> {
        self.host_check(to)?;
        let target = self.store.local(to)?.filter(ondemand::live).ok_or_else(|| HostError::NotFound(to.to_string()))?;
        let recs = self.rename_check_onto(from, to, true)?;
        if target.file.info.kind != FileKind::File || recs.len() != 1 || recs[0].file.info.kind != FileKind::File {
            return Err(HostError::Exists(to.to_string()));
        }
        if !self.holds_all(&recs)? || !self.holds_all(std::slice::from_ref(&target))? {
            return Err(HostError::NeedsHolder);
        }
        self.archive_untracked(to)?;
        let signer = self.signer.as_ref().map(|s| s as &dyn crate::scan::Signer);
        let gone = crate::scan::tombstone(signer, self.me, self.now_s(), target);
        let mut batch = Batch::new(&*self.store, self.me)?;
        batch.put_local(gone);
        let ops = batch.finish()?;
        self.store.apply(ops)?;
        self.rename_held(from, to, recs, true)
    }

    /// Another device (a writer, checked by the driver) asks this one, which holds the files, to rename `from` to `to`,
    /// as it saw them (`seen`, its [`Self::rename_seen`]): done here as an ordinary rename under this device's key, when
    /// its records are still those versions and it holds every file; refused otherwise ([`HostError::Changed`],
    /// [`HostError::NeedsHolder`]). Its pins stay as they are, as with any other device's rename.
    pub fn rename_for_peer(&mut self, from: &str, to: &str, seen: &[u8; 32]) -> Result<u64, HostError> {
        let to = &self.rename_target(to)?;
        let recs = self.rename_check(from, to)?;
        let mut h = Seen::default();
        recs.iter().for_each(|r| h.add(r));
        if h.finish() != *seen {
            return Err(HostError::Changed(from.to_string()));
        }
        if !self.holds_all(&recs)? {
            return Err(HostError::NeedsHolder);
        }
        self.rename_held(from, to, recs, false)
    }

    /// Renames what this device holds (checked): on disk, the folders above `to` made and recorded where missing, then
    /// each new name stored with its old name's delete, so nothing ever reads both live.
    fn rename_held(&mut self, from: &str, to: &str, recs: Vec<LocalRecord>, pins: bool) -> Result<u64, HostError> {
        let mut at = String::new();
        let parents: Vec<&str> = to.split('/').collect();
        for c in &parents[..parents.len() - 1] {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(c);
            if self.fs.lstat(&at)?.is_none() {
                self.fs.create_dir(&at)?;
            }
            if self.store.local(&at)?.filter(ondemand::live).is_none() {
                self.record_dir(&at)?;
            }
        }
        self.fs.rename(from, to)?;
        let (now_s, me) = (self.now_s(), self.me);
        let signer = self.signer.as_ref().ok_or(HostError::ReadOnly)?;
        let mut pairs = Vec::with_capacity(recs.len());
        for r in &recs {
            let i = &r.file.info;
            let path = format!("{to}{}", &i.path[from.len()..]);
            let base = self.store.local(&path)?.map(|b| b.file.info.version);
            let version = base.unwrap_or_default().bump(me, now_s);
            // Still a conflict copy only under a conflict copy's name of the same original (moved along with it, when
            // its folder is); renamed to anything else it is an ordinary file. (A full folder's scanner keeps no
            // conflict across a rename at all: the new path is a new file to it.)
            let conflict_of = i.conflict_of.as_ref().map(|o| if ondemand::under(o, from) { format!("{to}{}", &o[from.len()..]) } else { o.clone() });
            let conflict_of = conflict_of.filter(|o| crate::conflict::parse_conflict(&path).is_some_and(|c| c.original == *o));
            let info = FileInfo { path: path.clone(), version, modified_by: me, conflict_of, ..i.clone() };
            let stamp = if i.kind == FileKind::Dir { None } else { self.fs.lstat(&path)?.and_then(|m| restamped(r.stamp, &m)) };
            let new = LocalRecord { file: signer.sign(info), sequence: 0, invalid: false, stamp, tier: r.tier, dataless: false, want: false };
            pairs.push((new, crate::scan::tombstone(Some(signer), me, now_s, r.clone())));
        }
        let n = pairs.len() as u64;
        for chunk in pairs.chunks(500) {
            let mut batch = Batch::new(&*self.store, self.me)?;
            for (new, old) in chunk {
                batch.put_local(new.clone());
                batch.put_local(old.clone());
            }
            let ops = batch.finish()?;
            self.store.apply(ops)?;
        }
        self.follow_availability(&[])?;
        if pins {
            self.move_pins(from, to);
        }
        Ok(n)
    }

    /// Pins move with a rename the user made here: what was kept is kept under its new name. (The driver saves them.)
    pub fn move_pins(&mut self, from: &str, to: &str) {
        let moved: Vec<String> = self.pins.iter().filter(|p| ondemand::under(p, from)).cloned().collect();
        for p in moved {
            self.pins.remove(&p);
            self.pins.insert(format!("{to}{}", &p[from.len()..]));
        }
    }

    /// The path of an item identifier, if it names a live item.
    pub fn item_path(&mut self, id: &str) -> io::Result<Option<String>> {
        let items = self.items.get_or_insert_with(Items::default);
        items.set_not_members(&self.not_members);
        items.path_of(&*self.store, id)
    }

    /// The item at `path` (`""` is the root, a directory).
    pub fn item_at(&mut self, path: &str) -> io::Result<Option<Item>> {
        let items = self.items.get_or_insert_with(Items::default);
        items.set_not_members(&self.not_members);
        items.item_at(&*self.store, &self.pins, path)
    }

    /// The item with this identifier.
    pub fn item(&mut self, id: &str) -> io::Result<Option<Item>> {
        let items = self.items.get_or_insert_with(Items::default);
        items.set_not_members(&self.not_members);
        items.item(&*self.store, &self.pins, id)
    }

    /// A page of the children of the directory item `id` ([`Items::children`]).
    pub fn children(&mut self, id: &str, cursor: Option<&str>, limit: usize) -> io::Result<Option<Page>> {
        let items = self.items.get_or_insert_with(Items::default);
        items.set_not_members(&self.not_members);
        items.children(&*self.store, &self.pins, id, cursor, limit)
    }

    /// What changed since `anchor` ([`Items::changes`]).
    pub fn changes(&mut self, anchor: u64, limit: usize) -> io::Result<ChangePage> {
        let items = self.items.get_or_insert_with(Items::default);
        items.set_not_members(&self.not_members);
        items.changes(&*self.store, &self.pins, anchor, limit)
    }

    /// The anchor that stands for "now": [`Self::changes`] from it reports only what comes after.
    pub fn anchor(&self) -> io::Result<u64> {
        let m = self.store.meta()?;
        Ok(ondemand::anchor_of(m.index_id, m.sequence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_renamed_file_keeps_a_stamp_only_while_it_is_the_one_recorded() {
        let old = crate::index::Stamp { size: 5, mtime_ns: 10, ctime_ns: 20, inode: 7 };
        let meta = |size, mtime_ns, inode| crate::fs::Meta { kind: FileKind::File, size, mtime_ns, ctime_ns: 30, inode, exec: false, dataless: false, other: false };
        assert_eq!(restamped(Some(old), &meta(5, 10, 7)), Some(crate::index::Stamp { ctime_ns: 30, ..old }), "renamed: new ctime");
        assert_eq!(restamped(Some(old), &meta(6, 10, 7)), None, "written meanwhile");
        assert_eq!(restamped(Some(old), &meta(5, 11, 7)), None, "touched meanwhile");
        assert_eq!(restamped(Some(old), &meta(5, 10, 8)), None, "replaced meanwhile");
        assert_eq!(restamped(None, &meta(5, 10, 7)), None);
    }
}
