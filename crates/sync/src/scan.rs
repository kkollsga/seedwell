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
use std::time::{Duration, Instant};

use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{ChunkRef, FileInfo, FileKind, STATE_DIR, SignedFile, Version};

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
    /// An on-demand folder (SPEC §7G.11): the disk is the content cache, so a file missing there is a file no
    /// longer held here (online only from now on), never a delete. Deletes come from the host
    /// ([`crate::folder::FolderCore::host_delete`]). It says which paths are kept on this device (pinned): one of
    /// those missing from the cache is fetched again.
    pub on_demand: Option<&'a dyn Fn(&str) -> bool>,
    /// On demand: the content found is the host's change (`host_written`), so it becomes this device's version even
    /// where the record is online only. Otherwise content found at an online-only path that is not the record's goes
    /// to the archive and never becomes a version (in an on-demand folder edits come from the host only).
    pub host_write: bool,
}

impl Scanner<'_> {
    /// Moves the file at `rel` into the folder's archive, as a replaced file is kept.
    fn archive_stray(&self, rel: &str) -> io::Result<()> {
        let stamp = crate::clock::compact_stamp(i64::try_from(self.now_s).unwrap_or(i64::MAX).saturating_mul(1_000_000_000));
        let mut to = crate::conflict::archive_path(rel, &stamp);
        let mut n = 2;
        while self.fs.lstat(&to)?.is_some() {
            to = crate::conflict::archive_path(rel, &format!("{stamp}-{n}"));
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
        self.fs.rename(rel, &to)
    }
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

/// A scan in steps (SPEC §7G.2, *Scanning in steps*): each [`Scanner::step`] commits what it found and returns, so
/// whoever drives it can let go of the folder between steps (the app, a peer asking for the index and the file provider
/// never wait for a whole first scan of a big folder). A step ends after a batch of changes, after [`ScanJob::stepped`]'s time budget,
/// or with big files to hash: those are read off the folder's lock ([`ScanJob::take_to_hash`], [`HashTask::run`]) and
/// recorded by the next step, as long as they did not change meanwhile.
#[derive(Debug)]
pub struct ScanJob {
    scope: String,
    report: ScanReport,
    /// Pass 1's paths still to visit (popped from the end, so in path order).
    stack: Vec<String>,
    /// Pass 1 is still on (pass 2 follows).
    walking: bool,
    /// Pass 2: the last path looked at.
    after: Option<String>,
    done: bool,
    /// A step ends after about this long (`None`: after a full batch only).
    budget: Option<Duration>,
    /// Files of at least this many bytes are hashed apart (`None`: in the step).
    hash_apart_from: Option<u64>,
    to_hash: Vec<HashTask>,
    hashed: Vec<Hashed>,
}

/// Files hashed apart in one go at most: this many, or this many bytes (a step ends when either is reached).
const APART_FILES: usize = 256;
const APART_BYTES: u64 = 1 << 30;

impl ScanJob {
    /// A scan of `scope` whose steps each run to the end of a batch.
    pub fn new(scope: &str) -> Self {
        ScanJob {
            scope: scope.to_string(),
            report: ScanReport::default(),
            stack: vec![scope.to_string()],
            walking: true,
            after: None,
            done: false,
            budget: None,
            hash_apart_from: None,
            to_hash: Vec::new(),
            hashed: Vec::new(),
        }
    }

    /// A scan of `scope` whose steps end after about `budget`, with files of `hash_apart_from` bytes and more left to
    /// be hashed apart.
    pub fn stepped(scope: &str, budget: Duration, hash_apart_from: u64) -> Self {
        ScanJob { budget: Some(budget), hash_apart_from: Some(hash_apart_from), ..ScanJob::new(scope) }
    }

    /// The files the next step needs hashed (each with [`HashTask::run`], then [`Self::hashed`]).
    pub fn take_to_hash(&mut self) -> Vec<HashTask> {
        std::mem::take(&mut self.to_hash)
    }

    /// The files of [`Self::take_to_hash`], hashed: the next step records them.
    pub fn hashed(&mut self, done: Vec<Hashed>) {
        self.hashed.extend(done);
    }

    /// What the scan did so far.
    pub fn report(&self) -> &ScanReport {
        &self.report
    }

    /// What the scan did.
    pub fn into_report(self) -> ScanReport {
        self.report
    }

    /// Nothing more to do.
    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// A file a step found changed and left to be hashed apart: what it was then.
#[derive(Clone, Debug)]
pub struct HashTask {
    rel: String,
    stamp: Stamp,
    size: u64,
    tier: Tier,
    chosen_at: u64,
}

/// A [`HashTask`] done.
#[derive(Debug)]
pub struct Hashed {
    rel: String,
    stamp: Stamp,
    tier: Tier,
    chosen_at: u64,
    /// The chunks; `Ok(None)` when the file changed while it was read; or why it could not be read.
    chunks: Result<Option<Vec<ChunkRef>>, String>,
}

impl HashTask {
    /// The file's size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Reads and hashes the file (no lock needed: only the disk is read).
    pub fn run(&self, fs: &dyn Fs) -> Hashed {
        let read = fs.open_read(&self.rel).and_then(|fh| chunk::chunk_file(&*fh, self.size, self.tier, 1 << 20));
        let chunks = match read {
            // Changed while we read it: the hash is of no particular version.
            Ok(c) => Ok(matches!(fs.lstat(&self.rel), Ok(Some(after)) if stamp_of(&after) == self.stamp).then_some(c)),
            Err(e) => Err(e.to_string()),
        };
        Hashed { rel: self.rel.clone(), stamp: self.stamp, tier: self.tier, chosen_at: self.chosen_at, chunks }
    }
}

impl Scanner<'_> {
    /// Scans `scope` (a wire path, `""` = the whole folder) into `store`.
    pub fn scan(&self, store: &mut dyn IndexStore, scope: &str) -> Result<ScanReport, ScanError> {
        let mut job = ScanJob::new(scope);
        while !self.step(store, &mut job)? {
            let tasks = job.take_to_hash();
            job.hashed(tasks.iter().map(|t| t.run(self.fs)).collect());
        }
        Ok(job.into_report())
    }

    /// One step of `job`: true when the scan is done. Every step commits what it recorded.
    pub fn step(&self, store: &mut dyn IndexStore, job: &mut ScanJob) -> Result<bool, ScanError> {
        if job.done {
            return Ok(true);
        }
        // Looked at in every step: a disk unmounted between two must not read as "everything deleted".
        match self.fs.lstat(STATE_DIR)? {
            Some(m) if m.kind == FileKind::Dir => {}
            _ => return Err(ScanError::FolderMissing),
        }
        let started = Instant::now();
        let budget = job.budget;
        let out_of_time = || budget.is_some_and(|b| started.elapsed() >= b);
        if job.walking {
            self.walk(store, job, &out_of_time)?;
            if job.stack.is_empty() && job.to_hash.is_empty() && job.hashed.is_empty() {
                job.walking = false;
            }
            return Ok(false);
        }

        // Pass 2: the index. Records under the scope that are gone from disk become tombstones.
        let page = store.local_paths(&job.scope, job.after.as_deref(), 1000)?;
        if page.is_empty() {
            job.done = true;
            return Ok(true);
        }
        let mut batch = Batch::new(&*store, self.me)?;
        let mut listing = None;
        for p in &page {
            job.after = Some(p.clone());
            if let Some(rec) = batch.local(p)? {
                // A read-only device's local delete keeps the received version (invalid, no stamp):
                // already recorded, so not recorded again (a new sequence on every scan, forever). Its
                // local file over a received delete (invalid, stamped) is a change that can go, though.
                let recorded = rec.dataless || if rec.invalid { rec.stamp.is_none() } else { rec.file.info.deleted };
                if !recorded && !self.ignore.is_ignored(p, rec.file.info.kind == FileKind::Dir) {
                    // Still there (pass 1 handled it), or not readable now: never a delete.
                    let gone = match self.fs.lstat(p) {
                        Ok(None) => true,
                        Ok(Some(_)) if self.case_insensitive => !self.listed(p, &mut listing),
                        _ => false,
                    };
                    if let (true, Some(pinned)) = (gone, self.on_demand) {
                        // Gone from the cache: the content is no longer here. A file becomes online only (one kept
                        // on this device is fetched again); a directory or link holds no content and stays as it is
                        // (the puller makes it again).
                        if rec.file.info.kind == FileKind::File && !rec.file.info.deleted {
                            let want = pinned(p);
                            batch.put_local(LocalRecord { invalid: true, stamp: None, tier: None, dataless: true, want, ..rec });
                        }
                    } else if gone {
                        self.record_delete(&mut batch, rec);
                        job.report.deleted += 1;
                    }
                }
            }
            if out_of_time() {
                break;
            }
        }
        let ops = batch.finish()?;
        store.apply(ops)?;
        Ok(false)
    }

    /// Pass 1, the disk, up to the end of a batch (or of the step's time, or with files to hash apart). An explicit
    /// stack keeps deep trees off the call stack.
    fn walk(&self, store: &mut dyn IndexStore, job: &mut ScanJob, out_of_time: &dyn Fn() -> bool) -> Result<(), ScanError> {
        let mut batch = Batch::new(&*store, self.me)?;
        let report = &mut job.report;
        // Files hashed apart since the last step first, each as it is now.
        for h in std::mem::take(&mut job.hashed) {
            let rel = h.rel.clone();
            match self.fs.lstat(&rel) {
                // Gone meanwhile: pass 2 says so.
                Ok(None) => {}
                Ok(Some(meta)) => {
                    self.visit(&mut batch, &rel, &meta, report, job.hash_apart_from, Some(h))?;
                }
                Err(e) => report.issues.push(Issue { path: rel, kind: IssueKind::Io(e.to_string()) }),
            }
        }
        let mut apart: u64 = 0;
        let scope = job.scope.as_str();
        // A scope that is not on disk is handled by pass 2 (it was deleted).
        while let Some(rel) = job.stack.pop() {
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
                if let Some(task) = self.visit(&mut batch, &rel, &meta, report, job.hash_apart_from, None)? {
                    apart += task.size;
                    job.to_hash.push(task);
                }
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
                        // (The state directory and the partial-copy mark are skipped, never removed.)
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
                    job.stack.push(join(&rel, &path::wire_name(&name)));
                }
            }
            if batch.len() >= self.batch_size || job.to_hash.len() >= APART_FILES || apart >= APART_BYTES || out_of_time() {
                break;
            }
        }
        let ops = batch.finish()?;
        store.apply(ops)?;
        Ok(())
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
        hash_apart_from: Option<u64>,
        pre: Option<Hashed>,
    ) -> io::Result<Option<HashTask>> {
        if trs_sync_proto::model::validate_path(rel).is_err() {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::BadName });
            return Ok(None);
        }
        if meta.other {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Unsupported });
            return Ok(None);
        }
        if meta.dataless {
            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Dataless });
            return Ok(None);
        }
        let old = batch.local(rel)?;
        let stamp = stamp_of(meta);
        if let Some(o) = &old {
            let unchanged = if o.dataless {
                // Online only, and now there is a file: fetched (a crash before it was recorded), or the host's.
                false
            } else if o.invalid {
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
                return Ok(None);
            }
        }

        let tier_memo = old.as_ref().and_then(|o| o.tier);
        let (chunks, size, tier, target) = match meta.kind {
            FileKind::File => {
                let (tier, chosen_at) =
                    chunk::choose_tier(meta.size, tier_memo.map(|t| (Tier { avg: t.avg }, t.chosen_at)));
                let (chunks, tier, chosen_at) = match pre.filter(|h| h.rel == rel) {
                    // Hashed apart: what was read is this version only when the file has not moved since.
                    Some(h) => match h.chunks {
                        Ok(Some(c)) if h.stamp == stamp => (c, h.tier, h.chosen_at),
                        Err(e) if h.stamp == stamp => {
                            report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e) });
                            return Ok(None);
                        }
                        _ => {
                            report.hashed_bytes += meta.size;
                            report.busy.push(rel.to_string());
                            return Ok(None);
                        }
                    },
                    None if hash_apart_from.is_some_and(|min| meta.size >= min) => {
                        return Ok(Some(HashTask { rel: rel.to_string(), stamp, size: meta.size, tier, chosen_at }));
                    }
                    None => {
                        let fh = match self.fs.open_read(rel) {
                            Ok(f) => f,
                            Err(e) => {
                                report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                                return Ok(None);
                            }
                        };
                        let chunks = match chunk::chunk_file(&*fh, meta.size, tier, 1 << 20) {
                            Ok(c) => c,
                            Err(e) => {
                                report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                                return Ok(None);
                            }
                        };
                        // Changed while we read it: the hash is of no particular version. Try again later.
                        match self.fs.lstat(rel)? {
                            Some(after) if stamp_of(&after) == stamp => {}
                            _ => {
                                report.hashed_bytes += meta.size;
                                report.busy.push(rel.to_string());
                                return Ok(None);
                            }
                        }
                        (chunks, tier, chosen_at)
                    }
                };
                report.hashed_bytes += meta.size;
                (chunks, meta.size, Some(TierMemo { avg: tier.avg, chosen_at }), String::new())
            }
            FileKind::Symlink => match self.fs.read_link(rel) {
                Ok(t) => (Vec::new(), 0, None, t),
                Err(e) => {
                    report.issues.push(Issue { path: rel.to_string(), kind: IssueKind::Io(e.to_string()) });
                    return Ok(None);
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
            if o.dataless {
                // The content of an online-only version arrived: this device holds it now (announced valid again).
                batch.put_local(LocalRecord { invalid: false, stamp: Some(stamp), tier, dataless: false, want: false, ..o.clone() });
                report.changed += 1;
            } else {
                batch.restamp_local(LocalRecord { stamp: Some(stamp), tier, ..o.clone() });
            }
            return Ok(None);
        }
        // The same content as the version this device is about to take (a crash between the puller
        // writing it and recording it): take that version instead of making a concurrent one.
        if let Some(n) = batch.need(rel)?
            && !n.file.info.deleted
            && same_for_sync(&n.file.info, &info)
        {
            let others: Vec<Version> = batch.remote_versions(rel)?;
            let file = adopt(self.signer, self.me, old.as_ref(), n.file, &others);
            batch.put_local(LocalRecord { file, sequence: 0, invalid: false, stamp: Some(stamp), tier, dataless: false, want: false });
            report.changed += 1;
            return Ok(None);
        }

        if self.on_demand.is_some() && !self.host_write && old.as_ref().is_some_and(|o| o.dataless) {
            // Something put content at an online-only path behind the host's back: it is kept, never a version that
            // could beat another device's newer one.
            self.archive_stray(rel)?;
            return Ok(None);
        }

        match self.signer {
            Some(signer) => {
                info.version = base.map_or_else(Version::new, |b| b.version.clone()).bump(self.me, self.now_s);
                let file = signer.sign(info);
                batch.put_local(LocalRecord { file, sequence: 0, invalid: false, stamp: Some(stamp), tier, dataless: false, want: false });
            }
            None => {
                // Read-only: keep the version we received (if any) and mark our copy as differing. A
                // file only this device has (no version) is described as it is now.
                let file = match &old {
                    Some(o) if !o.file.info.version.is_empty() => o.file.clone(),
                    _ => SignedFile { info, sig: [0; 64] },
                };
                batch.put_local(LocalRecord { file, sequence: 0, invalid: true, stamp: Some(stamp), tier, dataless: false, want: false });
            }
        }
        report.changed += 1;
        Ok(None)
    }

    /// A record whose path is gone from disk.
    fn record_delete<S: IndexStore + ?Sized>(&self, batch: &mut Batch<'_, S>, rec: LocalRecord) {
        batch.put_local(tombstone(self.signer, self.me, self.now_s, rec));
    }
}

/// The record of a delete of `rec`'s path by this device: a signed tombstone that has seen `rec`'s version, or on a
/// read-only device a local change (the version received, marked invalid, nothing on disk).
pub(crate) fn tombstone(signer: Option<&dyn Signer>, me: ShortId, now_s: u64, rec: LocalRecord) -> LocalRecord {
    match signer {
        Some(signer) => {
            let o = &rec.file.info;
            let info = FileInfo {
                path: o.path.clone(),
                kind: o.kind,
                size: 0,
                mtime_ns: o.mtime_ns,
                exec: false,
                deleted: true,
                version: o.version.bump(me, now_s),
                modified_by: me,
                file_id: o.file_id,
                symlink_target: String::new(),
                conflict_of: None,
                chunks: Vec::new(),
            };
            LocalRecord { file: signer.sign(info), sequence: 0, invalid: false, stamp: None, tier: None, dataless: false, want: false }
        }
        // Read-only: a local delete is a local change like any other.
        None => LocalRecord { invalid: true, stamp: None, dataless: false, want: false, ..rec },
    }
}
