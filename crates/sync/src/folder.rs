//! One synced folder: the index, the scanner, the puller and the rules for what peers send, together.
//!
//! [`FolderCore`] is what the driver (and the simulator) talk to. It owns no threads and no sockets:
//! every method does its work and returns. The driver serialises calls per folder.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::{DeviceId, FolderId, ShortId};
use trs_sync_proto::keys::Access;
use trs_sync_proto::model::{FileInfo, FileKind, IndexEntry, STATE_DIR, SignedFile, validate_path};
use trs_sync_proto::sign::{BadWriter, Writer, WriterTable, sign_file};

use crate::archive::{self, Archived, Retention};
use crate::clock::Clock;
use crate::fs::Fs;
use crate::ignore::Ignore;
use crate::index::{Batch, IndexStore, PeerState};
use crate::pull::{ChunkSource, PullReport, Puller};
use crate::scan::{ScanError, ScanReport, Scanner, Signer};

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
    /// The folder's signing public key (identifies it; checks writer certificates).
    pub folder_pk: [u8; 32],
    /// What this device may do.
    pub access: Access,
    /// The folder is case-insensitive (default APFS): refuse names that clash by case only.
    pub case_insensitive: bool,
    /// Names compare byte for byte on this filesystem (Linux): names not in NFC are not synced.
    pub byte_exact_names: bool,
    /// Archive replaced and deleted versions.
    pub archive: bool,
}

/// Why a peer's index was refused. The peer is misbehaving or out of date; the driver closes the
/// connection with this reason.
#[derive(Debug)]
pub enum RemoteError {
    /// An entry is not signed by a writer of this folder.
    BadSignature(String),
    /// An entry's path is not acceptable.
    BadPath(String),
    /// A writer certificate does not check out.
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
            RemoteError::BadPath(p) => write!(f, "{p:?}: not an acceptable path"),
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
    signer: Option<DeviceSigner>,
    clock: Arc<dyn Clock>,
    new_file_id: Arc<dyn Fn() -> [u8; 16] + Send + Sync>,
    ignore: Ignore,
    /// Peers whose full send is in progress, with the index id it is of (their old entries were
    /// dropped at its start).
    receiving_full: BTreeMap<ShortId, u64>,
}

impl FolderCore {
    /// Opens a folder. A writer passes its device key and certificate; it is added to the writer table.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: FolderConfig,
        store: Box<dyn IndexStore>,
        fs: Arc<dyn Fs>,
        device_key: Option<(Arc<DeviceKey>, [u8; 64])>,
        clock: Arc<dyn Clock>,
        new_file_id: Arc<dyn Fn() -> [u8; 16] + Send + Sync>,
        ignore: Ignore,
    ) -> Result<Self, RemoteError> {
        let mut writers = WriterTable::new(cfg.folder_pk);
        for (_, w) in store.writers()? {
            // Stored writers were checked when they arrived; a bad one now means a damaged store.
            writers.insert(w).map_err(RemoteError::BadWriter)?;
        }
        let me = cfg.me.short();
        let mut core = FolderCore {
            signer: None,
            cfg,
            me,
            store,
            fs,
            writers,
            clock,
            new_file_id,
            ignore,
            receiving_full: BTreeMap::new(),
        };
        if let Some((key, cert)) = device_key.filter(|_| core.cfg.access == Access::Write) {
            core.add_writers(&[Writer { device_pk: *key.public_key(), cert }])?;
            core.signer = Some(DeviceSigner::new(key, core.writers.folder_id()));
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
        let new_id = self.new_file_id.clone();
        let scanner = Scanner {
            fs: &*self.fs,
            me: self.me,
            signer: self.signer.as_ref().map(|s| s as &dyn Signer),
            ignore: &self.ignore,
            byte_exact_names: self.cfg.byte_exact_names,
            case_insensitive: self.cfg.case_insensitive,
            now_s: (self.clock.now_ns() / 1_000_000_000).max(0) as u64,
            new_file_id: &*new_id,
            batch_size: 1000,
        };
        scanner.scan(&mut *self.store, scope)
    }

    /// One pull round. `device_name` names devices in conflict copies.
    pub fn pull(&mut self, source: &dyn ChunkSource, device_name: &dyn Fn(ShortId) -> String, window: u64) -> io::Result<PullReport> {
        let puller = Puller {
            fs: &*self.fs,
            me: self.me,
            signer: self.signer.as_ref().map(|s| s as &dyn Signer),
            source,
            clock: &*self.clock,
            device_name,
            case_insensitive: self.cfg.case_insensitive,
            archive: self.cfg.archive,
            window,
            round: Default::default(),
        };
        puller.pull(&mut *self.store)
    }

    /// Our entries with a sequence above `after`, at most `limit`, for a peer. A read-only device's
    /// own new files (never signed) are left out; its changed copies go out marked invalid.
    pub fn announce_since(&self, after: u64, limit: usize) -> io::Result<Vec<IndexEntry>> {
        Ok(self
            .store
            .local_since(after, limit)?
            .into_iter()
            .filter(|r| !(r.invalid && r.file.info.version.is_empty()))
            .map(|r| IndexEntry { sequence: r.sequence, invalid: r.invalid, file: r.file })
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

    /// Every writer we know (sent to peers, so they can check what we pass on).
    pub fn writers(&self) -> Vec<Writer> {
        self.writers.iter().copied().collect()
    }

    /// Adds writers a peer told us about (each certificate checked). True when any was new.
    pub fn add_writers(&mut self, ws: &[Writer]) -> Result<bool, RemoteError> {
        let mut batch = Batch::new(&*self.store, self.me)?;
        let mut new = false;
        for w in ws {
            if self.writers.insert(*w).map_err(RemoteError::BadWriter)? {
                batch.put_writer(DeviceId::of_public_key(&w.device_pk).short(), *w);
                new = true;
            }
        }
        if new {
            let ops = batch.finish()?;
            self.store.apply(ops)?;
        }
        Ok(new)
    }

    /// A peer's index batch. Every entry is checked (path, signature) before any is stored: one bad entry
    /// refuses the whole batch.
    pub fn apply_remote(&mut self, peer: ShortId, idx: RemoteIndex) -> Result<usize, RemoteError> {
        for e in &idx.entries {
            let p = &e.file.info.path;
            if validate_path(p).is_err() || crate::path::check_wire_path(p).is_err() {
                return Err(RemoteError::BadPath(p.clone()));
            }
            if !self.writers.verify(&e.file) {
                return Err(RemoteError::BadSignature(p.clone()));
            }
        }
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
            self.receiving_full.insert(peer, idx.index_id);
            state = PeerState { index_id: 0, max_seq: 0 };
        } else if !idx.full && known.index_id != idx.index_id {
            // Changes against an index we do not hold: ignore them (the peer sends a full index next).
            return Ok(0);
        }
        let mut stored = 0;
        let mut max_seq = state.max_seq;
        for e in idx.entries {
            if !idx.full && e.sequence <= known.max_seq {
                continue;
            }
            max_seq = max_seq.max(e.sequence);
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
        Ok(stored)
    }

    /// A peer left the folder (or its key was rotated out): forget what it announced.
    pub fn forget_peer(&mut self, peer: ShortId) -> io::Result<()> {
        let mut batch = Batch::new(&*self.store, self.me)?;
        batch.drop_peer(peer)?;
        let ops = batch.finish()?;
        self.receiving_full.remove(&peer);
        self.store.apply(ops)
    }

    /// How much is still needed: (paths, bytes).
    pub fn need_totals(&self) -> io::Result<(u64, u64)> {
        self.store.need_totals()
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
