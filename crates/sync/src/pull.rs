//! The puller: makes the disk match the current versions, safely (SPEC §7G.2).
//!
//! For every step of the [`plan`](crate::plan):
//! - **check first:** the path must still be what our record says (same stamp), or absent when we
//!   have nothing. Anything else is a local change the scanner has not seen yet: the step is skipped
//!   and the path handed back for a scan, so it becomes a conflict instead of being overwritten;
//! - **build aside:** a file is built in `.seedwell.<name>.tmp` next to it, from a clone of the old
//!   version, local chunks and peers, every chunk checked against its SHA-256, then fsynced;
//! - **check again, then swap:** right before the rename the target is checked once more;
//! - **keep what is replaced:** the old version goes to the archive (a hard link, or a rename for a
//!   delete), a losing concurrent version becomes a conflict copy.
//!
//! **Recording.** A finished step's record waits: every 64 steps, at the end of the round, and
//! before a step on a path with a waiting record, the directories the steps changed are made durable
//! ([`Fs::sync_dir`]), then the records are committed in one batch. So a record is never ahead of
//! the disk (a crash could otherwise undo a rename the index already holds, and the next scan would
//! read the missing file as the user deleting it). A crash before the commit loses only records, and
//! the scanner recognises a finished-but-unrecorded file as the version it was built from
//! (`scan.rs`). One fsync per directory per batch, not per file.
//!
//! **The known window.** Between the check right before the swap and the rename itself, a save by
//! the user (an app's own atomic rename onto the file) is replaced by the pulled version; an in-place
//! write lands in the archived old version, which shares the file. The window is a few system calls
//! long. Closing it needs `renamex_np(RENAME_SWAP)` and a check of what was swapped out (unsafe FFI),
//! which is not worth it now.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;

use sha2::{Digest, Sha256};
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{Causality, ChunkRef, FileInfo, FileKind, STATE_DIR, SignedFile, Version};

use crate::chunk;
use crate::clock::{self, Clock};
use crate::conflict;
use crate::fs::{FileHandle, Fs, Meta};
use crate::global::Need;
use crate::index::{Batch, IndexStore, LocalRecord, NeedRecord, Stamp, TierMemo, offsets};
use crate::plan::{self, Step};
use crate::scan::Signer;

/// One chunk to fetch from peers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRequest {
    /// The path (wire form) in the version being taken.
    pub path: String,
    /// Where the chunk starts in that version.
    pub offset: u64,
    /// Its length.
    pub len: u32,
    /// Its SHA-256.
    pub hash: [u8; 32],
    /// Devices that hold the version.
    pub holders: Vec<ShortId>,
}

/// Why a chunk could not be fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// No connected device has it now: try again when one connects.
    Unavailable,
    /// A device sent data that does not match the hash (it is told, and asked no more for this file).
    Corrupt(ShortId),
    /// The transfer failed (timeout, closed connection).
    Failed(String),
}

/// Fetches chunks from peers. Blocking; an implementation fetches the requests of one call in parallel.
pub trait ChunkSource: Sync {
    /// One result per request, in order.
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>>;
}

/// What a pull round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PullReport {
    /// Steps finished.
    pub done: u64,
    /// Bytes fetched from peers.
    pub fetched_bytes: u64,
    /// Bytes copied from local files instead of fetched.
    pub reused_bytes: u64,
    /// Paths that changed on disk since they were scanned: scan them, then pull again.
    pub rescan: Vec<String>,
    /// Steps waiting for a device that has the data.
    pub waiting: u64,
    /// Steps that failed, and why (retried later).
    pub failed: Vec<(String, String)>,
    /// Conflict copies made (their paths).
    pub conflicts: Vec<String>,
}

/// Everything a pull needs.
pub struct Puller<'a> {
    /// The folder on disk.
    pub fs: &'a dyn Fs,
    /// This device.
    pub me: ShortId,
    /// Signs this device's versions (conflict copies). `None` on a read-only device.
    pub signer: Option<&'a dyn Signer>,
    /// Peers.
    pub source: &'a dyn ChunkSource,
    /// Time.
    pub clock: &'a dyn Clock,
    /// Names devices for conflict copies (`None` = the first group of the device id is used).
    pub device_name: &'a dyn Fn(ShortId) -> String,
    /// The folder is case-insensitive: refuse a new name that differs only in case from an existing one.
    pub case_insensitive: bool,
    /// Archive replaced and deleted versions.
    pub archive: bool,
    /// Most bytes asked of the source in one call.
    pub window: u64,
    /// Records waiting for their directories to be durable (start with `Default::default()`).
    pub(crate) round: RefCell<Round>,
}

/// Records of finished steps not committed yet, and the directories to make durable first.
#[derive(Default)]
pub(crate) struct Round {
    records: Vec<LocalRecord>,
    paths: BTreeSet<String>,
    dirs: BTreeSet<String>,
}

/// Steps whose records may wait before they are committed in one batch.
const COMMIT_EVERY: usize = 64;

/// A step that did not run, and why.
enum Skip {
    Rescan,
    Waiting,
    Failed(String),
}

impl From<io::Error> for Skip {
    fn from(e: io::Error) -> Self {
        Skip::Failed(e.to_string())
    }
}

fn stamp_of(m: &Meta) -> Stamp {
    Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }
}

fn parent(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

fn name(p: &str) -> &str {
    p.rfind('/').map_or(p, |i| &p[i + 1..])
}

/// `.seedwell.<name>.tmp` beside `p`; a hash stands in for names too long to fit.
pub fn temp_path(p: &str) -> String {
    let n = name(p);
    let tmp = if n.len() + 14 <= 255 {
        format!(".seedwell.{n}.tmp")
    } else {
        let h = Sha256::digest(n.as_bytes());
        let hex: String = h[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!(".seedwell.{hex}.tmp")
    };
    match parent(p) {
        "" => tmp,
        dir => format!("{dir}/{tmp}"),
    }
}

/// A temp file of ours (`.seedwell.<name>.tmp`).
fn is_temp(name: &str) -> bool {
    name.starts_with(".seedwell.") && name.ends_with(".tmp")
}

/// Says the record that something is on disk? A read-only device's own change keeps the version it
/// had received (invalid), so there the stamp tells: none for its local delete (nothing there, like a
/// tombstone), one for its own file, also over a received delete.
pub(crate) fn holds(rec: &LocalRecord) -> bool {
    if rec.invalid { rec.stamp.is_some() } else { !rec.file.info.deleted }
}

/// Does the disk at `rel` still hold what `rec` says (or nothing, when we have no record)?
fn matches_record(meta: Option<&Meta>, rec: Option<&LocalRecord>) -> bool {
    let live = rec.filter(|r| holds(r));
    match (meta, live) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(m), Some(r)) => match (m.kind, r.file.info.kind) {
            (FileKind::Dir, FileKind::Dir) => true,
            (a, b) if a != b => false,
            _ => r.stamp == Some(stamp_of(m)),
        },
    }
}

impl Puller<'_> {
    /// One round over everything the folder needs. Call again while it makes progress (`done > 0`).
    pub fn pull(&self, store: &mut dyn IndexStore) -> io::Result<PullReport> {
        // Without the marker the folder is not there (an unmounted disk): writing would fill the empty
        // mount point, and the archive would create a marker there that makes the next scan read every
        // file as deleted. Same rule as the scanner.
        self.marker()?;
        let mut needs = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = store.needs(after.as_deref(), 1000)?;
            let Some((last, _)) = page.last().cloned() else { break };
            needs.extend(page);
            after = Some(last);
        }
        let steps = plan::plan(&needs, &|p| store.local(p).ok().flatten());
        let needs: BTreeMap<String, NeedRecord> = needs.into_iter().collect();
        let mut report = PullReport::default();
        for step in steps {
            let (path, other) = match &step {
                Step::MkDir(p) | Step::Fetch(p) | Step::Delete(p) | Step::Absorb(p) => (p.clone(), None),
                Step::Rename { from, to } => (to.clone(), Some(from.as_str())),
            };
            // A step reads the records of its paths: those must not be waiting.
            if self.pending(&path) || other.is_some_and(|f| self.pending(f)) {
                self.commit(store)?;
            }
            match self.run(store, &needs, &step, &mut report) {
                Ok(()) => report.done += 1,
                Err(Skip::Rescan) => report.rescan.push(path),
                Err(Skip::Waiting) => report.waiting += 1,
                Err(Skip::Failed(why)) => report.failed.push((path, why)),
            }
            if self.round.borrow().records.len() >= COMMIT_EVERY {
                self.commit(store)?;
            }
        }
        self.commit(store)?;
        Ok(report)
    }

    /// The versions other devices announce at `p` (valid ones), for [`crate::scan::adopt`].
    fn others(&self, store: &dyn IndexStore, p: &str) -> io::Result<Vec<Version>> {
        Ok(store.remote(p)?.into_iter().filter(|(_, e)| !e.invalid).map(|(_, e)| e.file.info.version).collect())
    }

    fn pending(&self, p: &str) -> bool {
        self.round.borrow().paths.contains(p)
    }

    /// A directory whose entries this round changed.
    fn changed_dir(&self, p: &str) {
        self.round.borrow_mut().dirs.insert(parent(p).to_string());
    }

    /// Makes the changed directories durable, then commits the waiting records in one batch. A
    /// directory gone since (removed by a later step) is skipped: its parent is synced too.
    fn commit(&self, store: &mut dyn IndexStore) -> io::Result<()> {
        let (records, dirs) = {
            let mut r = self.round.borrow_mut();
            r.paths.clear();
            (std::mem::take(&mut r.records), std::mem::take(&mut r.dirs))
        };
        for d in &dirs {
            match self.fs.sync_dir(d) {
                Ok(()) => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {}
                Err(e) => return Err(e),
            }
        }
        if records.is_empty() {
            return Ok(());
        }
        let mut batch = Batch::new(&*store, self.me)?;
        for rec in records {
            batch.put_local(rec);
        }
        let ops = batch.finish()?;
        store.apply(ops)
    }

    /// Queues a record (committed by [`Self::commit`]). `moved` says the step changed the disk at
    /// that path (its directory then has to be durable first).
    fn queue(&self, store: &mut dyn IndexStore, rec: LocalRecord, moved: bool) -> io::Result<()> {
        let p = rec.file.info.path.clone();
        if self.pending(&p) {
            self.commit(store)?;
        }
        if moved {
            self.changed_dir(&p);
        }
        let mut r = self.round.borrow_mut();
        r.paths.insert(p);
        r.records.push(rec);
        Ok(())
    }

    fn run(
        &self,
        store: &mut dyn IndexStore,
        needs: &BTreeMap<String, NeedRecord>,
        step: &Step,
        report: &mut PullReport,
    ) -> Result<(), Skip> {
        // The need as the store has it now: an earlier step of this round (a conflict copy made at a
        // name another device's copy also has, say) can have changed it since the round was planned.
        let need_of = |store: &dyn IndexStore, p: &str| -> Result<NeedRecord, Skip> {
            let planned = needs.get(p);
            match store.need(p)? {
                Some(now)
                    if planned.is_some_and(|w| {
                        w.need == now.need && w.file.info.version == now.file.info.version && w.file.info.same_content(&now.file.info)
                    }) =>
                {
                    Ok(now)
                }
                _ => Err(Skip::Failed(format!("{p}: no longer needed"))),
            }
        };
        match step {
            Step::MkDir(p) => {
                let n = need_of(&*store, p)?;
                self.mkdir(store, p, n, report)
            }
            Step::Fetch(p) => {
                let n = need_of(&*store, p)?;
                self.fetch(store, p, n, needs, report)
            }
            Step::Delete(p) => {
                let n = need_of(&*store, p)?;
                self.delete(store, p, n)
            }
            Step::Rename { from, to } => {
                let (nf, nt) = (need_of(&*store, from)?, need_of(&*store, to)?);
                self.rename(store, from, nf, to, nt)
            }
            Step::Absorb(p) => self.absorb(store, p),
        }
    }

    /// Records a new version of what we hold at `p` that has seen the deletes that lost to it
    /// ([`Need::Absorb`]). Nothing changes on disk.
    fn absorb(&self, store: &mut dyn IndexStore, p: &str) -> Result<(), Skip> {
        let Some(signer) = self.signer else { return Err(Skip::Failed("a read-only device cannot sign".into())) };
        let Some(l) = store.local(p)?.filter(|l| holds(l) && !l.invalid) else {
            return Err(Skip::Failed(format!("{p}: no longer ours")));
        };
        // The disk must still hold it: a change not scanned yet is a new version, absorbed after.
        if !matches_record(self.lstat_at(p, Some(&l))?.as_ref(), Some(&l)) {
            return Err(Skip::Rescan);
        }
        // The deletes that lost, and the same content made concurrently elsewhere (the same change:
        // merged, so the new version does not lose to or tie with one of them, and absorbing ends).
        let ours = &l.file.info;
        let mut version = ours.version.clone();
        for (_, e) in store.remote(p)? {
            let theirs = &e.file.info;
            if !e.invalid
                && (theirs.deleted || theirs.same_content(ours))
                && theirs.version.compare(&ours.version) == Causality::Concurrent
            {
                version = version.merge(&theirs.version);
            }
        }
        let version = version.bump(self.me, (self.clock.now_ns() / 1_000_000_000).max(0) as u64);
        let file = signer.sign(FileInfo { version, modified_by: self.me, ..l.file.info.clone() });
        self.queue(store, LocalRecord { file, sequence: 0, ..l }, false)?;
        Ok(())
    }

    /// Records `file` as ours at its path, with the disk's current stamp.
    fn record(&self, store: &mut dyn IndexStore, file: SignedFile, tier: Option<TierMemo>) -> Result<(), Skip> {
        self.record_placed(store, file, tier, None)
    }

    /// [`Self::record`] for a file this step just renamed into place, `placed` being its meta from
    /// before the rename. If the path no longer holds that file (another inode, size or mtime), the user
    /// changed it in between: the stamp is left out, so the next scan reads the change instead of
    /// taking it for the version recorded here.
    fn record_placed(
        &self,
        store: &mut dyn IndexStore,
        file: SignedFile,
        tier: Option<TierMemo>,
        placed: Option<&Meta>,
    ) -> Result<(), Skip> {
        let meta = self.fs.lstat(&file.info.path)?;
        let moved = |m: &Meta| placed.is_some_and(|p| (p.inode, p.size, p.mtime_ns) != (m.inode, m.size, m.mtime_ns));
        let stamp = if file.info.deleted { None } else { meta.as_ref().filter(|m| !moved(m)).map(stamp_of) };
        if self.pending(&file.info.path) {
            self.commit(store)?;
        }
        let others = self.others(store, &file.info.path)?;
        let file = crate::scan::adopt(self.signer, self.me, store.local(&file.info.path)?.as_ref(), file, &others);
        self.queue(store, LocalRecord { file, sequence: 0, invalid: false, stamp, tier }, true)?;
        Ok(())
    }

    /// Creates the parents of `p` that are missing.
    fn ensure_parents(&self, p: &str) -> io::Result<()> {
        let dir = parent(p);
        if dir.is_empty() {
            return Ok(());
        }
        let mut at = String::new();
        for c in dir.split('/') {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(c);
            match self.fs.lstat(&at)? {
                Some(m) if m.kind == FileKind::Dir => {}
                Some(_) => return Err(io::Error::other(format!("{at} is not a directory"))),
                None => {
                    self.fs.create_dir(&at)?;
                    self.changed_dir(&at);
                }
            }
        }
        Ok(())
    }

    /// On a case-insensitive folder, a new name must not clash with an existing name that differs
    /// only in case (it would silently become that file). Syncthing refuses it the same way.
    fn case_clash(&self, p: &str) -> io::Result<Option<String>> {
        if !self.case_insensitive {
            return Ok(None);
        }
        let dir = parent(p);
        if !dir.is_empty() && self.fs.lstat(dir)?.is_none() {
            return Ok(None);
        }
        let want = crate::path::key(name(p), true);
        let listing = self.fs.read_dir(dir)?;
        Ok(listing
            .entries
            .into_iter()
            .map(|e| e.name)
            .find(|n| n != name(p) && crate::path::key(&crate::path::wire_name(n), true) == want))
    }

    /// The folder marker is there (else `NotFound`, *Folder missing*).
    fn marker(&self) -> io::Result<()> {
        match self.fs.lstat(STATE_DIR)? {
            Some(m) if m.kind == FileKind::Dir => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "folder missing: no .seedwell marker")),
        }
    }

    /// Moves the current file at `p` into the archive (a delete) or links it there (a replace).
    fn archive_current(&self, p: &str, replace: bool) -> io::Result<()> {
        if !self.archive {
            if !replace {
                self.fs.remove_file(p)?;
            }
            return Ok(());
        }
        // `ensure_parents` below must never create the marker itself (the disk may have gone mid-pull).
        self.marker()?;
        let stamp = clock::compact_stamp(self.clock.now_ns());
        let mut to = conflict::archive_path(p, &stamp);
        let mut n = 2;
        while self.fs.lstat(&to)?.is_some() {
            to = conflict::archive_path(p, &format!("{stamp}-{n}"));
            n += 1;
        }
        self.ensure_parents(&to)?;
        if replace { self.fs.hard_link(p, &to)? } else { self.fs.rename(p, &to)? }
        // The archived version must be as durable as the record that replaces it.
        self.changed_dir(&to);
        self.changed_dir(p);
        Ok(())
    }

    /// `lstat`, where we hold nothing at `p`: on a case-insensitive disk it also finds a file under
    /// another spelling (`a` for `A`, after a case-only rename), which is not at `p` for us. The listing
    /// tells; it is read only in that case, so the common paths cost nothing more.
    fn lstat_at(&self, p: &str, local: Option<&LocalRecord>) -> io::Result<Option<Meta>> {
        let m = self.fs.lstat(p)?;
        if !self.case_insensitive || m.is_none() || local.is_some_and(holds) {
            return Ok(m);
        }
        let listing = self.fs.read_dir(parent(p))?;
        Ok(m.filter(|_| listing.entries.iter().any(|e| crate::path::wire_name(&e.name) == name(p))))
    }

    fn mkdir(&self, store: &mut dyn IndexStore, p: &str, n: NeedRecord, report: &mut PullReport) -> Result<(), Skip> {
        let local = store.local(p)?;
        let meta = self.lstat_at(p, local.as_ref())?;
        // A read-only device's own change here (a file where it had received a directory, say) is
        // expected to differ from the record; its stamp still has to match the disk.
        let ours_changed = local.as_ref().is_some_and(|l| l.invalid && l.stamp.is_some());
        let fine = if ours_changed {
            meta.as_ref().is_none_or(|m| m.kind == FileKind::Dir || local.as_ref().and_then(|l| l.stamp) == Some(stamp_of(m)))
        } else {
            // A directory already there that we never recorded is fine: it is what we want.
            matches_record(meta.as_ref(), local.as_ref()) || meta.as_ref().is_some_and(|m| m.kind == FileKind::Dir)
        };
        if !fine {
            return Err(Skip::Rescan);
        }
        match meta {
            Some(m) if m.kind == FileKind::Dir => {}
            Some(m) => {
                // A file (or link) we hold is replaced by a directory. One made concurrently with the
                // directory lost the conflict (a directory beats a file), and a read-only device's
                // own file is its user's: they become conflict copies. One the directory has seen
                // goes to the archive.
                let conflict = matches!(n.need, Need::TakeOverConflict | Need::Repair) || ours_changed;
                match local.as_ref().filter(|l| conflict && holds(l)) {
                    Some(l) => {
                        let copy = self.make_conflict_copy(store, p, l, &m)?;
                        report.conflicts.push(copy);
                    }
                    None => self.archive_current(p, false)?,
                }
                self.fs.create_dir(p)?;
            }
            None => {
                if let Some(other) = self.case_clash(p)? {
                    return Err(Skip::Failed(format!("name differs only in case from \"{other}\"")));
                }
                if self.under_own_change(store, p)? {
                    return self.skip_under_own_change(store, n);
                }
                self.ensure_parents(p)?;
                self.fs.create_dir(p)?;
            }
        }
        self.record(store, n.file, None)
    }

    /// Is a parent of `p` a read-only device's own change that is not a directory (a file where it
    /// had received a directory)? Then nothing can be put below it.
    fn under_own_change(&self, store: &dyn IndexStore, p: &str) -> io::Result<bool> {
        let mut at = parent(p);
        while !at.is_empty() {
            if self.fs.lstat(at)?.is_some_and(|m| m.kind != FileKind::Dir) && store.local(at)?.is_some_and(|r| r.invalid) {
                return Ok(true);
            }
            at = parent(at);
        }
        Ok(false)
    }

    /// Records `n` as not here, by the user's own change above it (invalid, no stamp, like a local
    /// delete), so the need settles until that change is undone.
    fn skip_under_own_change(&self, store: &mut dyn IndexStore, n: NeedRecord) -> Result<(), Skip> {
        self.queue(store, LocalRecord { file: n.file, sequence: 0, invalid: true, stamp: None, tier: None }, false)?;
        Ok(())
    }

    fn delete(&self, store: &mut dyn IndexStore, p: &str, n: NeedRecord) -> Result<(), Skip> {
        let local = store.local(p)?;
        let meta = self.lstat_at(p, local.as_ref())?;
        // A read-only device's own change here is its user's and stays, as a change arriving would keep
        // it as a conflict copy: the delete is recorded under it (still invalid, never served). Else a
        // local file over a received delete waits for a rescan forever.
        if let Some(l) = local.as_ref().filter(|l| l.invalid && l.stamp.is_some()) {
            if !meta.as_ref().is_some_and(|m| m.kind == FileKind::Dir || Some(stamp_of(m)) == l.stamp) {
                return Err(Skip::Rescan);
            }
            self.queue(store, LocalRecord { file: n.file, sequence: 0, invalid: true, stamp: l.stamp, tier: l.tier }, false)?;
            return Ok(());
        }
        if !matches_record(meta.as_ref(), local.as_ref()) {
            return Err(Skip::Rescan);
        }
        if let Some(m) = meta {
            if m.kind == FileKind::Dir {
                // Only an empty directory goes; one with files we do not sync (ignored, unknown) stays.
                let listing = self.fs.read_dir(p)?;
                let left: Vec<&str> = listing
                    .entries
                    .iter()
                    .map(|e| e.name.as_str())
                    .filter(|n| !is_temp(n))
                    .collect();
                if !left.is_empty() || !listing.skipped.is_empty() {
                    // On a read-only device, its own files keep the directory: it is recorded as a local
                    // change (the received delete, invalid, stamped), so the need settles. Only when
                    // every file left is its own (a synced one may still be on its way out).
                    if self.signer.is_none() && listing.skipped.is_empty() {
                        self.commit(store)?;
                        let mut own = true;
                        for name in &left {
                            own &= store.local(&format!("{p}/{name}"))?.is_none_or(|r| r.invalid);
                        }
                        if own {
                            let rec = LocalRecord { file: n.file, sequence: 0, invalid: true, stamp: Some(stamp_of(&m)), tier: None };
                            self.queue(store, rec, false)?;
                            return Ok(());
                        }
                    }
                    return Err(Skip::Failed("the directory holds files that are not synced".into()));
                }
                for n in &listing.entries {
                    self.fs.remove_file(&format!("{p}/{}", n.name))?;
                }
                self.fs.remove_dir(p)?;
            } else {
                self.archive_current(p, false)?;
            }
        }
        self.record(store, n.file, None)
    }

    fn rename(
        &self,
        store: &mut dyn IndexStore,
        from: &str,
        from_need: NeedRecord,
        to: &str,
        to_need: NeedRecord,
    ) -> Result<(), Skip> {
        let (lf, lt) = (store.local(from)?, store.local(to)?);
        // A case-only rename (`a` → `A`) on a case-insensitive disk: `lstat(to)` finds `from`.
        let (mf, mt) = (self.lstat_at(from, lf.as_ref())?, self.lstat_at(to, lt.as_ref())?);
        if !matches_record(mf.as_ref(), lf.as_ref()) || !matches_record(mt.as_ref(), lt.as_ref()) || mt.is_some() {
            return Err(Skip::Rescan);
        }
        if let Some(other) = self.case_clash(to)? {
            // A case-only rename (`a` → `A`) is the one clash that is fine: it is the same file.
            if other != name(from) || parent(from) != parent(to) {
                // `to` is refused, as a fetch of it would be; the delete of `from` it stood in for
                // still happens (else it waits as long as the clash stays).
                self.delete(store, from, from_need)?;
                return Err(Skip::Failed(format!("name differs only in case from \"{other}\"")));
            }
        }
        self.ensure_parents(to)?;
        self.fs.rename(from, to)?;
        let tier = lf.and_then(|l| l.tier);
        self.record_placed(store, to_need.file, tier, mf.as_ref())?;
        self.record(store, from_need.file, None)
    }

    fn fetch(
        &self,
        store: &mut dyn IndexStore,
        p: &str,
        n: NeedRecord,
        needs: &BTreeMap<String, NeedRecord>,
        report: &mut PullReport,
    ) -> Result<(), Skip> {
        let want = &n.file.info;
        let local = store.local(p)?;
        let meta = self.lstat_at(p, local.as_ref())?;
        // A read-only device's own change (invalid record) is expected to differ: it becomes a
        // conflict copy below. Anything else that differs from our record is a change not yet scanned.
        let ours_changed = local.as_ref().is_some_and(|l| l.invalid);
        if !ours_changed && !matches_record(meta.as_ref(), local.as_ref()) {
            return Err(Skip::Rescan);
        }
        // The same content as what we hold (a merged or absorbed version of it): only the record
        // changes, and the mtime if it moved.
        if let (Some(l), Some(m)) = (&local, &meta)
            && !ours_changed
            && holds(l)
            && l.file.info.same_content(want)
        {
            let mtime = if want.kind == FileKind::File && m.mtime_ns != want.mtime_ns {
                self.fs.set_mtime(p, want.mtime_ns)?;
                want.mtime_ns
            } else {
                m.mtime_ns
            };
            // Not the file we checked any more (a save in between): leave the stamp out, the scan reads it.
            let stamp = self.fs.lstat(p)?.filter(|n| (n.inode, n.size, n.mtime_ns) == (m.inode, m.size, mtime)).as_ref().map(stamp_of);
            let others = self.others(store, p)?;
            let file = crate::scan::adopt(self.signer, self.me, Some(l), n.file, &others);
            self.queue(store, LocalRecord { file, sequence: 0, invalid: false, stamp, tier: l.tier }, false)?;
            return Ok(());
        }
        if meta.as_ref().is_some_and(|m| m.kind == FileKind::Dir) {
            // A directory is replaced by a file: only once it is empty (its deletes come later in the
            // round, so this step comes again). Our own temp files there do not count (else a child's
            // interrupted download keeps it forever).
            let listing = self.fs.read_dir(p)?;
            if listing.entries.iter().any(|e| !is_temp(&e.name)) || !listing.skipped.is_empty() {
                // Files that stay (made concurrently with the file that replaced the directory, or
                // not synced): a directory with live content beats a file, as in a conflict. Else
                // they are deleted later in this round.
                let Some(dir) = local.as_ref().filter(|l| ours_changed || l.file.info.kind == FileKind::Dir) else {
                    return Err(Skip::Failed("waiting for the directory to empty".into()));
                };
                // (A read-only device's own directory is its local change whatever is in it.)
                if ours_changed || !self.subtree_goes(p, needs)? {
                    return self.keep_dir(store, p, dir, meta.as_ref().expect("a directory"), &n, report);
                }
                return Err(Skip::Failed("waiting for the directory to empty".into()));
            }
            for e in &listing.entries {
                self.fs.remove_file(&format!("{p}/{}", e.name))?;
            }
            self.fs.remove_dir(p)?;
        }
        if meta.is_none()
            && let Some(other) = self.case_clash(p)?
        {
            return Err(Skip::Failed(format!("name differs only in case from \"{other}\"")));
        }
        if meta.is_none() && self.under_own_change(store, p)? {
            return self.skip_under_own_change(store, n);
        }
        self.ensure_parents(p)?;
        let tmp = temp_path(p);

        let tier = match want.kind {
            FileKind::Symlink => {
                if self.fs.lstat(&tmp)?.is_some() {
                    self.fs.remove_file(&tmp)?;
                }
                self.fs.symlink(&want.symlink_target, &tmp)?;
                None
            }
            FileKind::File => Some(self.build(store, p, &tmp, local.as_ref(), &n, report)?),
            FileKind::Dir => return Err(Skip::Failed("a directory is not fetched".into())),
        };

        // Check again right before the swap: the user may have saved the file meanwhile.
        let now = self.lstat_at(p, local.as_ref())?;
        let still = if ours_changed {
            local.as_ref().and_then(|l| l.stamp) == now.as_ref().map(stamp_of)
        } else {
            matches_record(now.as_ref(), local.as_ref()) || now.as_ref().is_some_and(|m| m.kind == FileKind::Dir)
        };
        if !still {
            return Err(Skip::Rescan);
        }

        // What we hold now loses: keep it as a conflict copy if it is real content that differs.
        let conflict = matches!(n.need, Need::TakeOverConflict | Need::Repair) || ours_changed;
        if let (Some(l), Some(m)) = (&local, &now)
            && m.kind != FileKind::Dir
        {
            let differs = ours_changed || !l.file.info.same_content(want);
            if conflict && differs && !(l.file.info.deleted && !ours_changed) {
                let copy = self.make_conflict_copy(store, p, l, m)?;
                report.conflicts.push(copy);
            } else {
                self.archive_current(p, true)?;
            }
        }
        let placed = self.fs.lstat(&tmp)?;
        self.fs.rename(&tmp, p)?;
        self.record_placed(store, n.file, tier, placed.as_ref())
    }

    /// Is everything under the directory `p` on its way out in this round (a delete is needed for
    /// each path below it)?
    fn subtree_goes(&self, p: &str, needs: &BTreeMap<String, NeedRecord>) -> io::Result<bool> {
        let mut stack = vec![p.to_string()];
        while let Some(dir) = stack.pop() {
            let listing = self.fs.read_dir(&dir)?;
            if !listing.skipped.is_empty() {
                return Ok(false);
            }
            for e in listing.entries.into_iter().filter(|e| !is_temp(&e.name)) {
                let c = format!("{dir}/{}", crate::path::wire_name(&e.name));
                if !needs.get(&c).is_some_and(|n| n.file.info.deleted) {
                    return Ok(false);
                }
                if self.fs.lstat(&c)?.is_some_and(|m| m.kind == FileKind::Dir) {
                    stack.push(c);
                }
            }
        }
        Ok(true)
    }

    /// A file replaced the directory `p` elsewhere, but the directory has content here that stays.
    /// The directory wins: the file comes in as a conflict copy beside it (the same copy any device
    /// in this position makes), and a new version of the directory that has seen the file is
    /// recorded, so the file's device takes the directory back. A read-only device cannot sign: it
    /// keeps its directory as a local change when the content is all its own, else it waits.
    fn keep_dir(
        &self,
        store: &mut dyn IndexStore,
        p: &str,
        dir: &LocalRecord,
        dir_meta: &Meta,
        n: &NeedRecord,
        report: &mut PullReport,
    ) -> Result<(), Skip> {
        let want = &n.file.info;
        let Some(signer) = self.signer else {
            self.commit(store)?;
            let mut own = true;
            for e in self.fs.read_dir(p)?.entries.iter().filter(|e| !is_temp(&e.name)) {
                own &= store.local(&format!("{p}/{}", e.name))?.is_none_or(|r| r.invalid);
            }
            if !own {
                return Err(Skip::Failed("waiting for the directory to empty".into()));
            }
            let rec = LocalRecord { file: n.file.clone(), sequence: 0, invalid: true, stamp: Some(stamp_of(dir_meta)), tier: None };
            self.queue(store, rec, false)?;
            return Ok(());
        };
        if want.kind == FileKind::File {
            if let (copy, true) = self.copy_name(store, p, want, want.mtime_ns)? {
                let tmp = temp_path(&copy);
                let tier = self.build(store, p, &tmp, None, n, report)?;
                let placed = self.fs.lstat(&tmp)?;
                if self.fs.lstat(&copy)?.is_some() {
                    return Err(Skip::Rescan);
                }
                self.fs.rename(&tmp, &copy)?;
                let file = signer.sign(self.copy_info(&copy, p, want));
                let meta = self.fs.lstat(&copy)?;
                let moved = |m: &Meta| placed.as_ref().is_some_and(|q| (q.inode, q.size, q.mtime_ns) != (m.inode, m.size, m.mtime_ns));
                let stamp = meta.as_ref().filter(|m| !moved(m)).map(stamp_of);
                self.queue(store, LocalRecord { file, sequence: 0, invalid: false, stamp, tier: Some(tier) }, true)?;
                report.conflicts.push(copy);
            }
        } else {
            // A symlink: nothing to fetch; the scanner would make it anew anyway. It is kept as a
            // conflict copy too.
            if let (copy, true) = self.copy_name(store, p, want, want.mtime_ns)? {
                self.fs.symlink(&want.symlink_target, &copy)?;
                let file = signer.sign(self.copy_info(&copy, p, want));
                let stamp = self.fs.lstat(&copy)?.as_ref().map(stamp_of);
                self.queue(store, LocalRecord { file, sequence: 0, invalid: false, stamp, tier: None }, true)?;
                report.conflicts.push(copy);
            }
        }
        let version = dir.file.info.version.merge(&want.version).bump(self.me, (self.clock.now_ns() / 1_000_000_000).max(0) as u64);
        let file = signer.sign(FileInfo { version, modified_by: self.me, ..dir.file.info.clone() });
        self.queue(store, LocalRecord { file, sequence: 0, ..dir.clone() }, false)?;
        Ok(())
    }

    /// The conflict copy of `loser` at `p`, as every device that keeps it names it, and whether it is
    /// still to be made (false: it is here already, made here or synced from a device that made it).
    fn copy_name(&self, store: &mut dyn IndexStore, p: &str, loser: &FileInfo, mtime_ns: i64) -> Result<(String, bool), Skip> {
        let who = (self.device_name)(loser.modified_by);
        let stamp = clock::readable_stamp(self.clock, mtime_ns);
        let mut n = 1;
        loop {
            let copy = conflict::conflict_path(p, &who, &stamp, n);
            n += 1;
            if self.pending(&copy) {
                self.commit(store)?;
            }
            let rec = store.local(&copy)?;
            match self.fs.lstat(&copy)? {
                // A name we hold nothing live at (a tombstone there is fine: a delete of this very
                // copy, which then wins).
                None if rec.as_ref().is_none_or(|r| !holds(r)) => return Ok((copy, true)),
                Some(m)
                    if rec.as_ref().is_some_and(|r| {
                        !r.invalid
                            && r.file.info.same_content(loser)
                            && r.file.info.version == loser.version
                            && r.stamp == Some(stamp_of(&m))
                    }) =>
                {
                    return Ok((copy, false));
                }
                _ => {}
            }
        }
    }

    /// The entry of the conflict copy at `copy` of `loser` at `p`. Every device that keeps this loser
    /// makes the same one: the loser's own version vector, a file id derived from the loser's, the
    /// loser's mtime. Two such copies are Equal with the same content (no conflict), and a delete of
    /// the copy anywhere dominates them all.
    fn copy_info(&self, copy: &str, p: &str, loser: &FileInfo) -> FileInfo {
        let id: [u8; 32] = Sha256::new().chain_update(b"conflict").chain_update(loser.file_id).finalize().into();
        FileInfo {
            path: copy.to_string(),
            version: loser.version.clone(),
            modified_by: self.me,
            file_id: id[..16].try_into().expect("16 bytes"),
            conflict_of: Some(p.to_string()),
            ..loser.clone()
        }
    }

    /// Builds the file `want` in `tmp`: a clone of our old version when we have one, then every chunk
    /// that is not already in place from local files or peers. Returns the tier memo for the record.
    fn build(
        &self,
        store: &dyn IndexStore,
        p: &str,
        tmp: &str,
        local: Option<&LocalRecord>,
        n: &NeedRecord,
        report: &mut PullReport,
    ) -> Result<TierMemo, Skip> {
        let want = &n.file.info;
        // What is in the temp file to start with: a resumed temp file is trusted only chunk by chunk
        // (each is hashed before it is kept); a fresh clone of our old version holds its chunks.
        let mut in_place: BTreeMap<u64, [u8; 32]> = BTreeMap::new();
        let left = self.fs.lstat(tmp)?;
        // A leftover that is not a file (a symlink this step was making when it stopped) would make
        // `open_rw` fail on every round: start over.
        match left.as_ref().map(|m| m.kind) {
            Some(FileKind::Symlink) => self.fs.remove_file(tmp)?,
            Some(FileKind::Dir) => self.fs.remove_dir(tmp)?,
            _ => {}
        }
        let resumed = left.is_some_and(|m| m.kind == FileKind::File);
        if !resumed
            && let Some(l) = local.filter(|l| !l.invalid && !l.file.info.deleted && l.file.info.kind == FileKind::File)
        {
            self.fs.copy(p, tmp)?;
            for (off, c) in offsets(&l.file.info.chunks) {
                in_place.insert(off, c.hash);
            }
        }
        let fh = self.fs.open_rw(tmp)?;
        let mut missing: Vec<(u64, ChunkRef)> = Vec::new();
        for (off, c) in offsets(&want.chunks) {
            if in_place.get(&off) == Some(&c.hash) {
                continue;
            }
            if resumed && chunk_at(&*fh, off, c)? {
                continue;
            }
            if self.copy_local(store, &*fh, off, c)? {
                report.reused_bytes += u64::from(c.len);
                continue;
            }
            missing.push((off, *c));
        }

        // Peers, in windows of at most `window` bytes.
        let mut i = 0;
        let mut waiting = false;
        while i < missing.len() {
            let mut j = i;
            let mut bytes = 0u64;
            while j < missing.len() && (j == i || bytes + u64::from(missing[j].1.len) <= self.window) {
                bytes += u64::from(missing[j].1.len);
                j += 1;
            }
            let reqs: Vec<ChunkRequest> = missing[i..j]
                .iter()
                .map(|(off, c)| ChunkRequest {
                    path: p.to_string(),
                    offset: *off,
                    len: c.len,
                    hash: c.hash,
                    holders: n.holders.clone(),
                })
                .collect();
            let results = self.source.fetch(&reqs);
            for ((off, c), res) in missing[i..j].iter().zip(results) {
                match res {
                    Ok(data) if data.len() == c.len as usize && hash(&data) == c.hash => {
                        fh.write_at(*off, &data)?;
                        report.fetched_bytes += data.len() as u64;
                    }
                    Ok(_) => return Err(Skip::Failed("a device sent data that does not match its hash".into())),
                    Err(FetchError::Unavailable) => waiting = true,
                    Err(FetchError::Corrupt(_)) => {
                        return Err(Skip::Failed("a device sent data that does not match its hash".into()));
                    }
                    Err(FetchError::Failed(why)) => return Err(Skip::Failed(why)),
                }
            }
            i = j;
        }
        if waiting {
            // What arrived stays in the temp file; the rest comes when a device that has it connects.
            fh.sync_all()?;
            return Err(Skip::Waiting);
        }
        fh.set_len(want.size)?;
        fh.sync_all()?;
        drop(fh);
        self.fs.set_mtime(tmp, want.mtime_ns)?;
        self.fs.set_exec(tmp, want.exec)?;
        let prev = local.and_then(|l| l.tier).map(|t| (chunk::Tier { avg: t.avg }, t.chosen_at));
        let (tier, chosen_at) = chunk::choose_tier(want.size, prev);
        Ok(TierMemo { avg: tier.avg, chosen_at })
    }

    /// Copies a chunk from any local file that has it (checked against the hash: the file may have
    /// changed since it was scanned). True when it was found.
    fn copy_local(&self, store: &dyn IndexStore, to: &dyn FileHandle, off: u64, c: &ChunkRef) -> io::Result<bool> {
        for (src, src_off) in store.chunk_sources(&c.hash, 4)? {
            let Ok(fh) = self.fs.open_read(&src) else { continue };
            let mut buf = vec![0u8; c.len as usize];
            if read_full(&*fh, src_off, &mut buf)? && hash(&buf) == c.hash {
                to.write_at(off, &buf)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Moves our losing content at `p` to a conflict copy and records the copy as a new file.
    fn make_conflict_copy(
        &self,
        store: &mut dyn IndexStore,
        p: &str,
        ours: &LocalRecord,
        meta: &Meta,
    ) -> Result<String, Skip> {
        let loser = &ours.file.info;
        let (copy, fresh) = self.copy_name(store, p, loser, meta.mtime_ns)?;
        if !fresh {
            // The copy is here already (another device that kept the same loser made it, and it
            // synced): the loser is kept, `p` is only replaced.
            self.archive_current(p, true)?;
            return Ok(copy);
        }
        self.fs.rename(p, &copy)?;
        // The copy is a new file: the scanner would find it too, but recording it now keeps its
        // chunks known for reuse and announces it at once.
        let now = self.fs.lstat(&copy)?.ok_or_else(|| Skip::Failed("the conflict copy vanished".into()))?;
        let info = self.copy_info(&copy, p, loser);
        let rec = match (self.signer, ours.invalid) {
            // A read-only device's local change, now known only by its stamp: rescan it rather than
            // trust a record that described other content.
            (_, true) => None,
            (Some(signer), false) => Some(LocalRecord {
                file: signer.sign(info),
                sequence: 0,
                invalid: false,
                stamp: Some(stamp_of(&now)),
                tier: ours.tier,
            }),
            (None, false) => Some(LocalRecord {
                file: SignedFile { info: FileInfo { version: Version::new(), ..info }, sig: [0; 64] },
                sequence: 0,
                invalid: true,
                stamp: Some(stamp_of(&now)),
                tier: ours.tier,
            }),
        };
        if let Some(rec) = rec {
            self.queue(store, rec, true)?;
        } else {
            self.changed_dir(&copy);
        }
        Ok(copy)
    }
}

fn hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Reads exactly `buf.len()` bytes at `off`; false when the file is shorter.
fn read_full(fh: &dyn FileHandle, mut off: u64, mut buf: &mut [u8]) -> io::Result<bool> {
    while !buf.is_empty() {
        let n = fh.read_at(off, buf)?;
        if n == 0 {
            return Ok(false);
        }
        off += n as u64;
        buf = &mut buf[n..];
    }
    Ok(true)
}

/// The temp file already holds this chunk at this offset (a resumed download).
fn chunk_at(fh: &dyn FileHandle, off: u64, c: &ChunkRef) -> io::Result<bool> {
    if fh.len()? < off + u64::from(c.len) {
        return Ok(false);
    }
    let mut buf = vec![0u8; c.len as usize];
    Ok(read_full(fh, off, &mut buf)? && hash(&buf) == c.hash)
}
