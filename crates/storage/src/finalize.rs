//! Moving finished files from the `.incomplete` staging tree to their final paths, crash-safely.
//!
//! # Protocol (per file)
//! * Same volume: `rename(2)` — atomic, no journal record needed. After a crash the file is at
//!   either the source or the destination, and the resume logic just looks.
//! * Different volume (`EXDEV`, or `force_copy`): a `Started` record is appended and fsynced,
//!   then the data is copied to `<dst>.trs-part`, fsynced, `rename`d to `<dst>`, the parent dir is
//!   fsynced and the source removed. `<dst>` therefore only ever appears complete; a leftover
//!   `.trs-part` is discarded on resume. If both source and destination exist and `Started` was
//!   recorded, the copy finished and only the source removal is left.
//! * Destinations that exist without a `Started` record are foreign: we never overwrite them
//!   ([`StorageError::DestinationExists`]).
//!
//! # Journal
//! An append-only file: 17-byte header (`TRSJ`, version, file count, layout digest) followed by
//! 5-byte records `(file: u32 LE, state: u8)`; a torn trailing record is ignored. In memory it is
//! one byte per file. It is deleted when the move completes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::disk::{Location, TorrentHandle, TorrentShared};
use crate::error::{Result, StorageError, is_not_found};
use crate::fsutil;
use crate::layout::Layout;
use crate::oneshot::Pending;

const MAGIC: &[u8; 4] = b"TRSJ";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 17;
const RECORD_LEN: usize = 5;
const COPY_CHUNK: usize = 1 << 20;

const ST_NONE: u8 = 0;
const ST_STARTED: u8 = 1;
const ST_DONE: u8 = 2;

/// Fault-injection hook: called with a step number before every filesystem mutation; returning
/// `false` makes finalize stop with [`StorageError::Interrupted`], simulating a crash.
pub type FaultHook = Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// Options for [`TorrentHandle::finalize`].
#[derive(Clone, Default)]
pub struct FinalizeOptions {
    /// Always use copy+fsync+remove (testing the cross-volume path on one volume).
    pub force_copy: bool,
    /// Crash simulation for tests.
    pub fault: Option<FaultHook>,
}

/// What a finalize run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinalizeReport {
    /// Files moved with `rename`.
    pub renamed: u32,
    /// Files moved by copy.
    pub copied: u32,
    /// Files already in place (resume).
    pub already_done: u32,
    /// Zero-length files created at the destination.
    pub empty_created: u32,
}

/// Per-file move state (one byte per file), serialisable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Journal {
    states: Vec<u8>,
    digest: u64,
}

impl Journal {
    /// Empty journal for `count` files of the layout identified by `digest`.
    pub fn new(count: usize, digest: u64) -> Self {
        Self {
            states: vec![ST_NONE; count],
            digest,
        }
    }

    /// Whether file `i` is completely moved.
    pub fn is_done(&self, i: usize) -> bool {
        self.states.get(i) == Some(&ST_DONE)
    }

    /// Whether a cross-volume copy of file `i` was started.
    pub fn is_started(&self, i: usize) -> bool {
        self.states.get(i).is_some_and(|&s| s >= ST_STARTED)
    }

    /// Serialises the whole journal (header + one record per touched file).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = header(self.states.len(), self.digest).to_vec();
        for (i, &s) in self.states.iter().enumerate() {
            if s != ST_NONE {
                out.extend_from_slice(&(i as u32).to_le_bytes());
                out.push(s);
            }
        }
        out
    }

    /// Parses a journal file, replaying its records. Tolerates a torn trailing record.
    pub fn decode(bytes: &[u8], count: usize, digest: u64) -> Result<Self> {
        if bytes.len() < HEADER_LEN || bytes[..HEADER_LEN] != header(count, digest) {
            return Err(StorageError::CorruptJournal);
        }
        let mut j = Self::new(count, digest);
        for rec in bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>().0 {
            let idx = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
            let st = rec[4];
            if idx >= count || !(ST_STARTED..=ST_DONE).contains(&st) {
                return Err(StorageError::CorruptJournal);
            }
            j.states[idx] = j.states[idx].max(st);
        }
        Ok(j)
    }
}

fn header(count: usize, digest: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4] = VERSION;
    h[5..9].copy_from_slice(&(count as u32).to_le_bytes());
    h[9..].copy_from_slice(&digest.to_le_bytes());
    h
}

struct JournalFile {
    file: File,
    state: Journal,
}

impl JournalFile {
    fn open(path: &Path, count: usize, digest: u64) -> Result<Self> {
        let existing = match fs::read(path) {
            Ok(b) if b.len() >= HEADER_LEN => Some(b),
            Ok(_) => None, // crashed while writing the header: nothing was moved yet
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(StorageError::io(e)),
        };
        match existing {
            Some(bytes) => {
                let state = Journal::decode(&bytes, count, digest)?;
                let file = OpenOptions::new()
                    .append(true)
                    .open(path)
                    .map_err(StorageError::io)?;
                Ok(Self { file, state })
            }
            None => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
                    .map_err(StorageError::io)?;
                file.write_all(&header(count, digest))
                    .map_err(StorageError::io)?;
                file.sync_all().map_err(StorageError::io)?;
                sync_parent(path).map_err(StorageError::io)?;
                Ok(Self {
                    file,
                    state: Journal::new(count, digest),
                })
            }
        }
    }

    fn record(&mut self, i: usize, st: u8, sync: bool) -> Result<()> {
        let mut rec = [0u8; RECORD_LEN];
        rec[..4].copy_from_slice(&(i as u32).to_le_bytes());
        rec[4] = st;
        // One write(2) on an O_APPEND fd: the record is either there or not.
        self.file.write_all(&rec).map_err(StorageError::io)?;
        if sync {
            self.file.sync_data().map_err(StorageError::io)?;
        }
        if let Some(s) = self.state.states.get_mut(i) {
            *s = (*s).max(st);
        }
        Ok(())
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => File::open(p)?.sync_all(),
        _ => Ok(()),
    }
}

fn exists(p: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

struct Ctx<'a> {
    opts: &'a FinalizeOptions,
    step: u32,
}

impl Ctx<'_> {
    fn step(&mut self) -> Result<()> {
        let n = self.step;
        self.step += 1;
        match &self.opts.fault {
            Some(f) if !f(n) => Err(StorageError::Interrupted),
            _ => Ok(()),
        }
    }
}

fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_os_string();
    s.push(".trs-part");
    PathBuf::from(s)
}

fn copy_file(src: &Path, part: &Path, ctx: &mut Ctx<'_>) -> Result<()> {
    let mut input = File::open(src).map_err(StorageError::io)?;
    let mtime = input.metadata().and_then(|m| m.modified()).ok();
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(part)
        .map_err(StorageError::io)?;
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        ctx.step()?;
        let n = input.read(&mut buf).map_err(StorageError::io)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(StorageError::io)?;
    }
    // Extended attributes (Finder tags and comments), ACLs and mode go with the data.
    let _ = crate::fsutil::copy_metadata(&input, &out);
    // Keep the mtime so stored fingerprints stay valid after a cross-volume move.
    if let Some(m) = mtime {
        let _ = out.set_modified(m);
    }
    out.sync_all().map_err(StorageError::io)?;
    Ok(())
}

enum Moved {
    Renamed,
    Copied,
    AlreadyDone,
    EmptyCreated,
}

fn move_one(
    t: &TorrentShared,
    layout: &Layout,
    i: u32,
    j: &mut JournalFile,
    ctx: &mut Ctx<'_>,
) -> Result<Moved> {
    let (src, _) = t.resolve(layout, i, Location::Staging);
    let (dst, base) = t.resolve(layout, i, Location::Final);
    let io = |e: io::Error| StorageError::from_io(i, e);
    let src_exists = exists(&src).map_err(io)?;
    let dst_exists = exists(&dst).map_err(io)?;
    let len = layout.file_len(i as usize);

    match (src_exists, dst_exists) {
        (false, true) => return Ok(Moved::AlreadyDone),
        (false, false) => {
            if len != 0 {
                return Err(StorageError::MissingData { file: i });
            }
        }
        (true, true) => {
            if j.state.is_started(i as usize)
                && fs::metadata(&dst).map_err(io)?.len() == fs::metadata(&src).map_err(io)?.len()
            {
                // Copy finished before the crash; only the source removal is left.
                ctx.step()?;
                fs::remove_file(&src).map_err(io)?;
                return Ok(Moved::Copied);
            }
            return Err(StorageError::DestinationExists { file: i });
        }
        (true, false) => {}
    }

    // Parent directories of the destination: never rebuild a vanished root/volume.
    if let Some(b) = &base
        && !b.is_dir() {
            return Err(StorageError::MissingRoot);
        }
    if let Some(parent) = dst.parent() {
        ctx.step()?;
        fsutil::create_dirs_guarded(parent).map_err(|e| {
            if is_not_found(&e) {
                StorageError::MissingRoot
            } else {
                io(e)
            }
        })?;
    }

    if !src_exists {
        // Zero-length file that was never created during the download.
        ctx.step()?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dst)
            .map_err(io)?;
        return Ok(Moved::EmptyCreated);
    }

    if !ctx.opts.force_copy {
        ctx.step()?;
        match fs::rename(&src, &dst) {
            Ok(()) => return Ok(Moved::Renamed),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
            Err(e) => return Err(io(e)),
        }
    }

    // Cross-volume: copy + fsync + atomic publish + remove.
    ctx.step()?;
    j.record(i as usize, ST_STARTED, true)?;
    let part = part_path(&dst);
    match fs::remove_file(&part) {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(io(e)),
    }
    copy_file(&src, &part, ctx)?;
    ctx.step()?;
    fs::rename(&part, &dst).map_err(io)?;
    sync_parent(&dst).map_err(io)?;
    ctx.step()?;
    fs::remove_file(&src).map_err(io)?;
    Ok(Moved::Copied)
}

/// Removes empty directories below `dir` (and `dir` itself if it ends up empty).
fn prune_empty(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for e in rd.flatten() {
        let p = e.path();
        let ft = e.file_type();
        if ft.as_ref().is_ok_and(|t| t.is_dir()) && prune_empty(&p) {
            continue;
        }
        if ft.is_ok_and(|t| t.is_file()) && crate::fsutil::is_finder_litter(&e.file_name()) {
            continue;
        }
        empty = false;
    }
    empty && crate::fsutil::remove_dir_if_empty(dir)
}

fn run_finalize(
    t: &Arc<TorrentShared>,
    layout: &Arc<Layout>,
    journal_path: &Path,
    opts: &FinalizeOptions,
) -> Result<FinalizeReport> {
    let mut report = FinalizeReport::default();
    let staging = t.staging_root();
    if t.location() == Location::Final || staging.is_none() {
        // Nothing staged (already finalized, or no staging configured). Idempotent.
        let _ = fs::remove_file(journal_path);
        t.set_location_final();
        return Ok(report);
    }
    let mut j = JournalFile::open(journal_path, layout.file_count(), layout.digest())?;
    let mut ctx = Ctx { opts, step: 0 };
    // Descriptors point into the staging tree; release them before moving anything.
    t.disk.fds.invalidate_torrent(t.id);
    for i in 0..layout.file_count() as u32 {
        if layout.file_is_padding(i as usize) {
            continue;
        }
        if j.state.is_done(i as usize) {
            report.already_done += 1;
            continue;
        }
        ctx.step()?;
        match move_one(t, layout, i, &mut j, &mut ctx)? {
            Moved::Renamed => report.renamed += 1,
            Moved::Copied => report.copied += 1,
            Moved::AlreadyDone => report.already_done += 1,
            Moved::EmptyCreated => report.empty_created += 1,
        }
        ctx.step()?;
        j.record(i as usize, ST_DONE, false)?;
    }
    ctx.step()?;
    t.set_location_final();
    // Journal first (it may live inside the staging tree), then the now-empty staging tree. A
    // crash in between is harmless: a rerun finds every file at its destination.
    match fs::remove_file(journal_path) {
        Ok(()) => {}
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(StorageError::io(e)),
    }
    let _ = sync_parent(journal_path);
    if let Some(s) = staging {
        prune_empty(&s);
    }
    Ok(report)
}

impl TorrentHandle {
    /// Moves every file from the staging tree to its final path (SPEC §7A.3) and then switches
    /// the torrent to [`Location::Final`].
    ///
    /// Safe to call again after a crash with the same `journal_path`: it resumes where the
    /// interrupted run stopped. Never overwrites existing destination files. Switch the torrent to
    /// [`Mode::Seed`](crate::Mode) afterwards.
    pub fn finalize(
        &self,
        journal_path: PathBuf,
        opts: FinalizeOptions,
    ) -> Result<Pending<FinalizeReport>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || run_finalize(&t, &layout, &journal_path, &opts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_codec_and_torn_tail() {
        let mut j = Journal::new(4, 77);
        j.states[1] = ST_STARTED;
        j.states[3] = ST_DONE;
        let mut bytes = j.encode();
        assert_eq!(Journal::decode(&bytes, 4, 77).unwrap(), j);
        bytes.extend_from_slice(&[1, 0]); // torn record
        assert_eq!(Journal::decode(&bytes, 4, 77).unwrap(), j);
        assert!(Journal::decode(&bytes, 5, 77).is_err());
        assert!(Journal::decode(&bytes, 4, 78).is_err());
        assert!(Journal::decode(&bytes[..5], 4, 77).is_err());
        let mut bad = j.encode();
        bad.extend_from_slice(&[9, 0, 0, 0, 2]); // index out of range
        assert!(Journal::decode(&bad, 4, 77).is_err());
    }
}
