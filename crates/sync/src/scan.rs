//! The scanner: turns what is on disk into local records.
//!
//! Two passes over the scope (a path, a directory, or the whole folder):
//! 1. **walk** the disk: a path whose stamp (size, mtime, ctime, inode) matches its record is skipped
//!    without reading it; anything else is hashed and, if the content differs, becomes a new version;
//! 2. **check the index**: every record under the scope that is not on disk any more becomes a
//!    tombstone.
//!
//! Safety first (SPEC §7G.2): without the folder marker nothing is scanned (an unmounted disk must not
//! read as "everything deleted"); a file that changes while it is hashed is skipped and retried; a path
//! that cannot be read (permissions, a cloud placeholder) is never turned into a delete.

use std::io;

use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{FileInfo, FileKind, STATE_DIR, SignedFile, Version};

use crate::chunk::{self, Tier};
use crate::fs::{Fs, Meta};
use crate::ignore::Ignore;
use crate::global::Need;
use crate::index::{Batch, IndexStore, LocalRecord, NeedRecord, Stamp, TierMemo};
use crate::path;

/// Makes and signs this device's new versions. A read-only device has none: its local changes are
/// recorded as invalid and never announced.
pub trait Signer: Send + Sync {
    /// Signs a version this device made.
    fn sign(&self, info: FileInfo) -> SignedFile;
}

/// The version to record when this device takes `file` in place of its own record `ours`; `others`
/// are the versions other devices announce at that path.
///
/// Taking `file` as it is drops our version's history. That matters when ours had replaced a version
/// some device still announces and `file` has not seen it: that version is then concurrent with
/// `file` and can come back ("a change beats a delete", or by winning the conflict). Then, and for two
/// concurrent deletes (always the same content), a writer records the merge of both vectors, signed
/// as its own, with `file`'s content; no bump, it is no change of ours. Merging on every take was
/// measured in the simulator and is worse: a device that edits or deletes `file` before it hears of
/// the merge is concurrent with it, and its delete loses. A delete that lost to a change is absorbed
/// by the change's holder instead ([`Need::Absorb`]). A read-only device cannot sign: it takes `file`
/// as it is.
///
/// [`Need::Absorb`]: crate::global::Need::Absorb
pub fn adopt(
    signer: Option<&dyn Signer>,
    me: ShortId,
    ours: Option<&LocalRecord>,
    file: SignedFile,
    others: &[Version],
) -> SignedFile {
    let (Some(signer), Some(ours)) = (signer, ours) else { return file };
    let (mine, theirs) = (&ours.file.info.version, &file.info.version);
    if ours.invalid || theirs.dominates(mine) {
        return file;
    }
    let both_deleted = file.info.deleted && ours.file.info.deleted;
    let load_bearing = others.iter().any(|o| mine.dominates(o) && mine != o && !theirs.dominates(o));
    if !both_deleted && !load_bearing {
        return file;
    }
    let version = theirs.merge(mine);
    signer.sign(FileInfo { version, modified_by: me, ..file.info })
}

/// Something about a path the user should know (shown under Issues).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    /// The path (wire form, or lossy for names that are not UTF-8).
    pub path: String,
    /// Why it is not synced.
    pub kind: IssueKind,
}

/// Why a path is not synced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueKind {
    /// The name is not valid UTF-8.
    NotUtf8,
    /// The name is not in Unicode NFC and this filesystem stores names byte for byte (Linux).
    NotNfc,
    /// A cloud placeholder whose data is not on this device; reading it would download it.
    Dataless,
    /// A socket, device or fifo.
    Unsupported,
    /// The name cannot be synced (too long, reserved, a control character).
    BadName,
    /// Reading failed.
    Io(String),
}

/// What a scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// New versions recorded (changes and new paths).
    pub changed: u64,
    /// Tombstones recorded.
    pub deleted: u64,
    /// Bytes read for hashing.
    pub hashed_bytes: u64,
    /// Files that changed while being hashed: scan them again later.
    pub busy: Vec<String>,
    /// Paths that are not synced, and why.
    pub issues: Vec<Issue>,
}

/// Why a scan did not run.
#[derive(Debug)]
pub enum ScanError {
    /// The folder marker (`.seedwell/`) is missing: the folder is not there (an unmounted disk) or was
    /// deleted. Nothing was changed.
    FolderMissing,
    /// The index could not be read or written.
    Io(io::Error),
}

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        ScanError::Io(e)
    }
}

/// Everything a scan needs.
pub struct Scanner<'a> {
    /// The folder on disk.
    pub fs: &'a dyn Fs,
    /// This device.
    pub me: ShortId,
    /// Signs new versions (`None` on a read-only device).
    pub signer: Option<&'a dyn Signer>,
    /// What not to sync.
    pub ignore: &'a Ignore,
    /// Names on disk compare byte for byte (Linux), so a name that is not NFC cannot be synced.
    pub byte_exact_names: bool,
    /// The disk finds names whatever their case (APFS default), so `lstat` alone does not tell that a
    /// name is gone after a case-only rename.
    pub case_insensitive: bool,
    /// The disk keeps the executable bit (see [`crate::folder::FolderConfig::exec_bits`]).
    pub exec_bits: bool,
    /// Now, Unix seconds (for version counters).
    pub now_s: u64,
    /// Random bytes for new file ids.
    pub new_file_id: &'a dyn Fn() -> [u8; 16],
    /// Records are committed in batches of this many changes.
    pub batch_size: usize,
}

/// Temp files of the puller (`.seedwell.<name>.tmp`) and the state directory are never scanned.
fn is_ours(name: &str) -> bool {
    name == STATE_DIR || (name.starts_with(".seedwell.") && name.ends_with(".tmp"))
}

/// Is the temp file `name` in `dir` the one a pull would resume (a path it would be the temp of still
/// needs something)? A wanted temp is kept for that; any other is left over.
fn temp_is_wanted(store: &dyn IndexStore, dir: &str, name: &str) -> io::Result<bool> {
    let tmp = join(dir, name);
    let wanted = |p: &str, n: &NeedRecord| n.need != Need::Nothing && crate::pull::temp_path(p) == tmp;
    // The usual name: `.seedwell.<name>.tmp` next to the path. (`.seedwell.tmp` itself has no name inside.)
    let Some(inner) = name.get(".seedwell.".len()..name.len().saturating_sub(".tmp".len())).filter(|n| !n.is_empty()) else {
        return Ok(false);
    };
    let path = join(dir, inner);
    if store.need(&path)?.is_some_and(|n| wanted(&path, &n)) {
        return Ok(true);
    }
    // A long name has a hashed temp name: look through the needs in `dir` (rare).
    if !(inner.len() == 32 && inner.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Ok(false);
    }
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut after = (!dir.is_empty()).then(|| dir.to_string());
    loop {
        let page = store.needs(after.as_deref(), 1000)?;
        let Some((last, _)) = page.last() else { return Ok(false) };
        for (p, n) in &page {
            if p.starts_with(&prefix) && wanted(p, n) {
                return Ok(true);
            }
        }
        // Paths below `dir` sort before `<dir>0` ('0' follows '/').
        if !prefix.is_empty() && last.as_str() > format!("{dir}0").as_str() {
            return Ok(false);
        }
        after = Some(last.clone());
    }
}

fn stamp_of(m: &Meta) -> Stamp {
    Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// A directory's version has no time: its mtime changes with every file added inside, which is not a
/// change of the directory itself.
fn same_for_sync(a: &FileInfo, b: &FileInfo) -> bool {
    a.same_content(b)
}

impl Scanner<'_> {
    /// Scans `scope` (a wire path, `""` = the whole folder) into `store`.
    pub fn scan(&self, store: &mut dyn IndexStore, scope: &str) -> Result<ScanReport, ScanError> {
        match self.fs.lstat(STATE_DIR)? {
            Some(m) if m.kind == FileKind::Dir => {}
            _ => return Err(ScanError::FolderMissing),
        }
        let mut report = ScanReport::default();
        let mut batch = Batch::new(&*store, self.me)?;

        // Pass 1: the disk. An explicit stack keeps deep trees off the call stack.
        let mut stack = vec![scope.to_string()];
        // A scope that is not on disk is handled by pass 2 (it was deleted).
        while let Some(rel) = stack.pop() {
            let Some(meta) = (match self.fs.lstat(&rel) {
                Ok(m) => m,
                Err(e) => {
                    report.issues.push(Issue { path: rel.clone(), kind: IssueKind::Io(e.to_string()) });
                    continue;
                }
            }) else {
                continue;
            };
            if !rel.is_empty() {
                if self.ignore.is_ignored(&rel, meta.kind == FileKind::Dir) {
                    continue;
                }
                // The scope itself was found by `lstat`, which on a case-insensitive disk also finds
                // it under another spelling: then it is not there (pass 2), and that spelling is
                // scanned under its own name.
                if rel == scope && self.case_insensitive && !self.listed(&rel, &mut None) {
                    continue;
                }
                self.visit(&mut batch, &rel, &meta, &mut report)?;
            }
            if meta.kind == FileKind::Dir && !meta.other {
                let listing = match self.fs.read_dir(&rel) {
                    Ok(l) => l,
                    Err(e) => {
                        report.issues.push(Issue { path: rel.clone(), kind: IssueKind::Io(e.to_string()) });
                        continue;
                    }
                };
                for bad in listing.skipped {
                    report.issues.push(Issue { path: join(&rel, &bad), kind: IssueKind::NotUtf8 });
                }
                let mut names: Vec<String> = listing.entries.into_iter().map(|e| e.name).collect();
                // Reverse order on the stack = path order when popped (stable, deterministic).
                names.sort_unstable_by(|a, b| b.cmp(a));
                for name in names {
                    if is_ours(&name) {
                        if name != STATE_DIR && !temp_is_wanted(&*store, &rel, &name)? {
                            // A pull's temp file whose path needs nothing any more (deleted or renamed
                            // on the other side meanwhile): no pull will finish it.
                            let _ = self.fs.remove_file(&join(&rel, &name));
                        }
                        continue;
                    }
                    if self.byte_exact_names && !path::is_nfc(&name) {
                        report.issues.push(Issue { path: join(&rel, &name), kind: IssueKind::NotNfc });
                        continue;
                    }
                    stack.push(join(&rel, &path::wire_name(&name)));
                }
            }
            if batch.len() >= self.batch_size {
                let ops = batch.finish()?;
                store.apply(ops)?;
                batch = Batch::new(&*store, self.me)?;
            }
        }
        let ops = batch.finish()?;
        store.apply(ops)?;

        // Pass 2: the index. Records under the scope that are gone from disk become tombstones.
        let mut after: Option<String> = None;
        loop {
            let page = store.local_paths(scope, after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { break };
            let mut batch = Batch::new(&*store, self.me)?;
            let mut listing = None;
            for p in &page {
                let Some(rec) = batch.local(p)? else { continue };
                // A read-only device's local delete keeps the received version (invalid, no stamp):
                // already recorded, so not recorded again (a new sequence on every scan, forever). Its
                // local file over a received delete (invalid, stamped) is a change that can go, though.
                let recorded = if rec.invalid { rec.stamp.is_none() } else { rec.file.info.deleted };
                if recorded || self.ignore.is_ignored(p, rec.file.info.kind == FileKind::Dir) {
                    continue;
                }
                // Still there (pass 1 handled it), or not readable now: never a delete.
                let gone = match self.fs.lstat(p) {
                    Ok(None) => true,
                    Ok(Some(_)) if self.case_insensitive => !self.listed(p, &mut listing),
                    _ => false,
                };
                if gone {
                    self.record_delete(&mut batch, rec);
                    report.deleted += 1;
                }
            }
            let ops = batch.finish()?;
            store.apply(ops)?;
            after = Some(last);
        }
        Ok(report)
    }

    /// Is `p` in its directory's listing under exactly this name (wire form)? `cache` keeps the last
    /// directory read. A listing that cannot be read says yes: never a delete on an error.
    fn listed(&self, p: &str, cache: &mut Option<(String, Vec<String>)>) -> bool {
        let (dir, name) = match p.rfind('/') {
            Some(i) => (&p[..i], &p[i + 1..]),
            None => ("", p),
        };
        if cache.as_ref().is_none_or(|(d, _)| d != dir) {
            let Ok(l) = self.fs.read_dir(dir) else { return true };
            *cache = Some((dir.to_string(), l.entries.into_iter().map(|e| path::wire_name(&e.name)).collect()));
        }
        cache.as_ref().is_some_and(|(_, names)| names.iter().any(|n| n == name))
    }

    /// One path from the walk.
    fn visit<S: IndexStore + ?Sized>(
        &self,
        batch: &mut Batch<'_, S>,
        rel: &str,
        meta: &Meta,
        report: &mut ScanReport,
    ) -> io::Result<()> {
        if trs_sync_proto::model::validate_path(rel).is_err() {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::BadName });
            return Ok(());
        }
        if meta.other {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Unsupported });
            return Ok(());
        }
        if meta.dataless {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Dataless });
            return Ok(());
        }
        let old = batch.local(rel)?;
        let stamp = stamp_of(meta);
        if let Some(o) = &old {
            let unchanged = if o.invalid {
                // A read-only device's own change keeps the received version in its record (another
                // kind or size than the disk), so only the stamp says whether the disk moved since (any
                // stamp, for a directory). No stamp: it was deleted here, and is back now.
                o.stamp.is_some_and(|s| s == stamp || meta.kind == FileKind::Dir)
            } else {
                !o.file.info.deleted && o.file.info.kind == meta.kind && match meta.kind {
                    FileKind::File => o.stamp == Some(stamp) && (!self.exec_bits || o.file.info.exec == meta.exec),
                    // Directories have no content; symlinks are re-read (cheap) below.
                    FileKind::Dir => true,
                    FileKind::Symlink => o.stamp == Some(stamp),
                }
            };
            if unchanged {
                return Ok(());
            }
        }

        let tier_memo = old.as_ref().and_then(|o| o.tier);
        let (chunks, size, tier, target) = match meta.kind {
            FileKind::File => {
                let (tier, chosen_at) =
                    chunk::choose_tier(meta.size, tier_memo.map(|t| (Tier { avg: t.avg }, t.chosen_at)));
                let fh = match self.fs.open_read(rel) {
                    Ok(f) => f,
                    Err(e) => {
                        report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                        return Ok(());
                    }
                };
                let chunks = match chunk::chunk_file(&*fh, meta.size, tier, 1 << 20) {
                    Ok(c) => c,
                    Err(e) => {
                        report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                        return Ok(());
                    }
                };
                report.hashed_bytes += meta.size;
                // Changed while we read it: the hash is of no particular version. Try again later.
                match self.fs.lstat(rel)? {
                    Some(after) if stamp_of(&after) == stamp => {}
                    _ => {
                        report.busy.push(rel.to_string());
                        return Ok(());
                    }
                }
                (chunks, meta.size, Some(TierMemo { avg: tier.avg, chosen_at }), String::new())
            }
            FileKind::Symlink => match self.fs.read_link(rel) {
                Ok(t) => (Vec::new(), 0, None, t),
                Err(e) => {
                    report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                    return Ok(());
                }
            },
            FileKind::Dir => (Vec::new(), 0, None, String::new()),
        };

        let base = old.as_ref().map(|o| &o.file.info);
        let mut info = FileInfo {
            path: rel.to_string(),
            kind: meta.kind,
            size,
            mtime_ns: if meta.kind == FileKind::Dir { 0 } else { meta.mtime_ns },
            // Where the disk does not keep the bit, the version we have says it.
            exec: meta.kind == FileKind::File && if self.exec_bits { meta.exec } else { base.is_some_and(|b| !b.deleted && b.exec) },
            deleted: false,
            version: Version::new(),
            modified_by: self.me,
            file_id: base.filter(|b| !b.deleted).map_or_else(|| (self.new_file_id)(), |b| b.file_id),
            symlink_target: target,
            conflict_of: base.and_then(|b| b.conflict_of.clone()),
            chunks,
        };

        // The same content as what we already have: only the stamp moved (a touch, a copy back).
        if let Some(o) = &old
            && !o.file.info.deleted
            && same_for_sync(&o.file.info, &info)
        {
            batch.restamp_local(LocalRecord { stamp: Some(stamp), tier, ..o.clone() });
            return Ok(());
        }
        // The same content as the version this device is about to take (a crash between the puller
        // writing it and recording it): take that version instead of making a concurrent one.
        if let Some(n) = batch.need(rel)?
            && !n.file.info.deleted
            && same_for_sync(&n.file.info, &info)
        {
            let others: Vec<Version> = batch.remote_versions(rel)?;
            let file = adopt(self.signer, self.me, old.as_ref(), n.file, &others);
            batch.put_local(LocalRecord { file, sequence: 0, invalid: false, stamp: Some(stamp), tier });
            report.changed += 1;
            return Ok(());
        }

        match self.signer {
            Some(signer) => {
                info.version = base.map_or_else(Version::new, |b| b.version.clone()).bump(self.me, self.now_s);
                let file = signer.sign(info);
                batch.put_local(LocalRecord { file, sequence: 0, invalid: false, stamp: Some(stamp), tier });
            }
            None => {
                // Read-only: keep the version we received (if any) and mark our copy as differing. A
                // file only this device has (no version) is described as it is now.
                let file = match &old {
                    Some(o) if !o.file.info.version.is_empty() => o.file.clone(),
                    _ => SignedFile { info, sig: [0; 64] },
                };
                batch.put_local(LocalRecord { file, sequence: 0, invalid: true, stamp: Some(stamp), tier });
            }
        }
        report.changed += 1;
        Ok(())
    }

    /// A record whose path is gone from disk.
    fn record_delete<S: IndexStore + ?Sized>(&self, batch: &mut Batch<'_, S>, rec: LocalRecord) {
        match self.signer {
            Some(signer) => {
                let o = &rec.file.info;
                let info = FileInfo {
                    path: o.path.clone(),
                    kind: o.kind,
                    size: 0,
                    mtime_ns: o.mtime_ns,
                    exec: false,
                    deleted: true,
                    version: o.version.bump(self.me, self.now_s),
                    modified_by: self.me,
                    file_id: o.file_id,
                    symlink_target: String::new(),
                    conflict_of: None,
                    chunks: Vec::new(),
                };
                batch.put_local(LocalRecord { file: signer.sign(info), sequence: 0, invalid: false, stamp: None, tier: None });
            }
            // Read-only: a local delete is a local change like any other.
            None => batch.put_local(LocalRecord { invalid: true, stamp: None, ..rec }),
        }
    }
}
