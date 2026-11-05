//! Hashing an unhashed file as it is first fetched (SPEC §7G.11, *Listed first, hashed when first fetched*).
//!
//! An on-demand folder's scan records what it finds without reading it: an unhashed version (a size, no chunks). The
//! first device that needs its content asks the version's maker for it in pieces, in order. The maker's [`Stream`]
//! reads each piece once, runs it through the chunker and answers with the bytes and the chunks that ended in them; at
//! the end it checks that the file did not change while it was read (its stamp before the first byte, as recorded, and
//! after the last) and hands back the chunk list, which the maker signs into the version's hashed form. The receiver's
//! [`StreamCheck`] checks each chunk's hash as its bytes arrive and, at the end, that the chunks are the ones the
//! signed version lists.

use std::collections::{BTreeMap, HashMap};
use std::io;

use sha2::{Digest, Sha256};
use trs_sync_proto::model::{ChunkRef, FileInfo, FileKind, STATE_DIR, SignedFile, Version};

use crate::chunk::{self, Chunker, Tier};
use crate::fs::{Fs, Meta};
use crate::index::{LocalRecord, Stamp, TierMemo};
use crate::pull::{ChunkRequest, ChunkSource, FetchError, STREAM_PIECE, StreamAsk, StreamFail};
use trs_sync_proto::ids::ShortId;

fn stamp_of(m: &Meta) -> Stamp {
    Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }
}

/// Why a piece was not served.
#[derive(Debug)]
pub enum StreamError {
    /// No unhashed version of this device's own at that path (any more): hashed meanwhile, changed, deleted, or not
    /// this device's to hash.
    Gone,
    /// The file changed since it was recorded, or while it was read: nothing of it is recorded.
    Changed,
    /// Not where the read is (a piece out of order): start again from the first.
    OutOfOrder,
    /// The disk could not be read.
    Io(io::Error),
}

impl From<io::Error> for StreamError {
    fn from(e: io::Error) -> Self {
        StreamError::Io(e)
    }
}

/// One piece of a [`Stream`].
#[derive(Clone, Debug)]
pub struct Piece {
    /// The bytes.
    pub data: Vec<u8>,
    /// The chunks that ended inside the bytes read so far and were not handed out before.
    pub chunks: Vec<ChunkRef>,
    /// With the last piece: the whole file's chunks, the file unchanged from its record to its last byte.
    pub done: Option<Hashed>,
}

/// A file read to its end: what its hashed version is made of.
#[derive(Clone, Debug)]
pub struct Hashed {
    /// The version as recorded, with its chunks (to be signed by the maker).
    pub info: FileInfo,
    /// The stamp it had throughout.
    pub stamp: Stamp,
    /// Its tier.
    pub tier: TierMemo,
}

/// The maker's read of one unhashed file for one device: each byte read once, in order.
pub struct Stream {
    info: FileInfo,
    stamp: Stamp,
    tier: TierMemo,
    next: u64,
    chunker: Option<Chunker>,
    all: Vec<ChunkRef>,
    handed: usize,
    /// The last piece handed out (where, how long, its chunks and end), so an answer lost on the way (a slow link that
    /// timed out) can be asked for again: its bytes are read again, the rest is as it was.
    last: Option<(u64, u32, Vec<ChunkRef>, Option<Hashed>)>,
    /// The last piece was served again already: once only (a receiver asking it a third time starts again from 0).
    again: bool,
}

impl Stream {
    /// A read of `rec` (this device's own unhashed version, valid and held) from its first byte. The disk must hold
    /// the file as recorded (its stamp), else [`StreamError::Changed`] (the scan records what is there now).
    pub fn start(fs: &dyn Fs, rec: &LocalRecord) -> Result<Stream, StreamError> {
        let info = &rec.file.info;
        if rec.invalid || rec.dataless || !info.is_unhashed() {
            return Err(StreamError::Gone);
        }
        let now = fs.lstat(&info.path)?.map(|m| stamp_of(&m));
        if now.is_none() || now != rec.stamp {
            return Err(StreamError::Changed);
        }
        let (tier, chosen_at) = chunk::choose_tier(info.size, rec.tier.map(|t| (Tier { avg: t.avg }, t.chosen_at)));
        Ok(Stream {
            info: info.clone(),
            stamp: rec.stamp.expect("checked"),
            tier: TierMemo { avg: tier.avg, chosen_at },
            next: 0,
            chunker: Some(Chunker::new(tier)),
            all: Vec::new(),
            handed: 0,
            last: None,
            again: false,
        })
    }

    /// The version it reads.
    pub fn version(&self) -> &Version {
        &self.info.version
    }

    /// Where the next piece starts.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Reads the `len` bytes at `offset` (where the last piece ended; fewer only at the end of the file). The last
    /// piece checks the file's stamp again: changed while it was read, it is [`StreamError::Changed`] and nothing is
    /// handed back to record.
    ///
    /// The last piece again (its answer was lost: the receiver asks the same offset and length once more) is read
    /// again and handed back with the same chunks and end, the file's stamp checked again first.
    pub fn piece(&mut self, fs: &dyn Fs, offset: u64, len: u32) -> Result<Piece, StreamError> {
        let size = self.info.size;
        if let Some((at, n, chunks, done)) = self.last.clone().filter(|(at, n, ..)| *at == offset && *n == len && offset != self.next) {
            if std::mem::replace(&mut self.again, true) {
                return Err(StreamError::OutOfOrder);
            }
            if fs.lstat(&self.info.path)?.map(|m| stamp_of(&m)) != Some(self.stamp) {
                return Err(StreamError::Changed);
            }
            let data = self.read(fs, at, u64::from(n).min(size - at) as usize)?;
            return Ok(Piece { data, chunks, done });
        }
        if offset != self.next || offset >= size || len == 0 {
            return Err(StreamError::OutOfOrder);
        }
        let want = u64::from(len).min(size - offset) as usize;
        let data = self.read(fs, offset, want)?;
        let chunker = self.chunker.as_mut().ok_or(StreamError::OutOfOrder)?;
        chunker.feed(&data, &mut self.all);
        self.next += want as u64;
        let mut done = None;
        if self.next == size {
            self.all.extend(self.chunker.take().and_then(Chunker::finish));
            let after = fs.lstat(&self.info.path)?.map(|m| stamp_of(&m));
            if after != Some(self.stamp) {
                return Err(StreamError::Changed);
            }
            done = Some(Hashed { info: FileInfo { chunks: self.all.clone(), ..self.info.clone() }, stamp: self.stamp, tier: self.tier });
        }
        let chunks = self.all[self.handed..].to_vec();
        self.handed = self.all.len();
        self.last = Some((offset, len, chunks.clone(), done.clone()));
        self.again = false;
        Ok(Piece { data, chunks, done })
    }

    /// Every piece was handed out (the last one may still be asked for again).
    pub fn ended(&self) -> bool {
        self.next == self.info.size
    }

    /// `want` bytes at `offset`; fewer on the disk than recorded is a change.
    fn read(&self, fs: &dyn Fs, offset: u64, want: usize) -> Result<Vec<u8>, StreamError> {
        let mut data = vec![0u8; want];
        let fh = fs.open_read(&self.info.path)?;
        let mut got = 0;
        while got < want {
            match fh.read_at(offset + got as u64, &mut data[got..])? {
                // Shorter than recorded: it changed.
                0 => return Err(StreamError::Changed),
                n => got += n,
            }
        }
        Ok(data)
    }
}

/// A receiver's stream of one file cut off in the middle (the maker went away for a while, an answer did not come in
/// time): kept between pull rounds so the next one goes on from where it stopped, while the maker still keeps its
/// read ([`STREAM_IDLE`] there). The temp file holds the bytes fed.
#[derive(Clone)]
pub struct Resume {
    /// The unhashed version streamed.
    pub version: Version,
    /// The check, as far as the bytes came.
    pub check: StreamCheck,
}

/// How long a maker keeps a read nobody asks a piece of, and a receiver its [`Resume`].
pub const STREAM_IDLE: std::time::Duration = std::time::Duration::from_secs(600);

/// The streams of one folder between pull rounds (in [`crate::folder::FolderCore`]). A pull never streams with the
/// folder's core held (a piece can take minutes on a slow link, and the hosts' calls and the scan wait for the core):
/// a round that needs an unhashed file's content notes a job and goes on; whoever drives the folder runs the jobs
/// with the core let go ([`stream_file`]) and hands the end back ([`crate::folder::FolderCore::stream_ended`]); the
/// next round places a finished file and records it.
#[derive(Default)]
pub struct Streams {
    /// Wanted by the last rounds, by path.
    pub(crate) jobs: BTreeMap<String, Wanted>,
    /// Taken ([`crate::folder::FolderCore::take_jobs`]) and not ended yet, by path: the version, and how many (a job
    /// taken again before the one before it ended, as a race allows, runs after it). A round leaves those paths alone
    /// (their temp files are being written).
    pub(crate) running: HashMap<String, (Version, u32)>,
    /// The peers' chunks of hashed files fetched apart so far, by path ([`ChunkJob`]).
    pub(crate) chunks: HashMap<String, Chunks>,
    /// Cut off in the middle, by path, and when (the clock's ns): dropped after [`STREAM_IDLE`], or when another
    /// version is wanted there.
    pub(crate) resume: HashMap<String, (Resume, i64)>,
    /// Finished, to place, by path.
    pub(crate) prepared: HashMap<String, Prepared>,
    /// Failed, by path: the version and why. The next round that wants that version says so, once.
    pub(crate) failed: HashMap<String, (Version, String)>,
}

/// What a round wants done with the core let go.
pub enum Wanted {
    /// A stream of this unhashed version.
    Stream(SignedFile),
    /// These chunks of a hashed file from peers.
    Chunks(ChunkJob),
}

/// Work to do with the folder's core let go ([`run_job`]).
pub enum Job {
    /// A stream of an unhashed file (boxed: its check holds a hasher's state).
    Stream(Box<StreamJob>),
    /// A hashed file's build.
    Chunks(Box<ChunkJob>),
}

/// What an earlier job of a hashed file left ([`ChunkJob::known`]): the temp file as it was then (file id, size,
/// mtime), and the chunks still missing.
pub type Known = ((u64, u64, i64), Vec<(u64, ChunkRef)>);

impl Job {
    /// The path it is for.
    pub fn path(&self) -> &str {
        match self {
            Job::Stream(j) => &j.path,
            Job::Chunks(j) => &j.path,
        }
    }
}

/// How a [`Job`] ended.
pub enum JobEnd {
    /// A stream's.
    Stream(StreamEnd),
    /// A fetch of chunks'.
    Chunks(ChunksEnd),
}

/// A hashed file to build in its temp file with the folder's core let go (SPEC §7G.2): every disk read and network wait
/// of it (the clone of our old version, chunks found in other local files, the check of a temp file left over, the
/// peers' chunks) is the job's; the round that notes it does none, and a later round places the file.
#[derive(Clone, Debug)]
pub struct ChunkJob {
    /// The path (wire form).
    pub path: String,
    /// Its temp file ([`crate::pull::temp_path`]).
    pub tmp: String,
    /// The version to build.
    pub version: Version,
    /// Its chunks.
    pub chunks: Vec<ChunkRef>,
    /// Our version at the path to start from (its stamp as recorded and its chunks), when there is one: cloned only if
    /// the file has that stamp before and after the copy (a save during the copy would mix its bytes in).
    pub old: Option<(Stamp, Vec<ChunkRef>)>,
    /// What an earlier job left (the temp file as it was then, and the chunks still missing), trusted only while the
    /// temp file is still so.
    pub known: Option<Known>,
    /// The devices that hold the version.
    pub holders: Vec<ShortId>,
    /// Our record at the path when the job was noted (its version and stamp): the file is placed only while it is
    /// still so.
    pub local: Option<(Version, Option<Stamp>)>,
}

/// How far a job came ([`build_chunks`]): the chunks still to come (none: the temp file is whole), the temp file as
/// the job left it (file id, size, mtime), and our record as the job started from it; a later round trusts the temp
/// file only while both are still so.
#[derive(Clone, Debug)]
pub struct Chunks {
    /// The version.
    pub version: Version,
    /// The temp file as left.
    pub tmp: (u64, u64, i64),
    /// Still to come.
    pub remaining: Vec<(u64, ChunkRef)>,
    /// Our record at the path as the job started ([`ChunkJob::local`]).
    pub local: Option<(Version, Option<Stamp>)>,
}

/// How a fetch of chunks ended.
pub enum ChunksEnd {
    /// As far as it came ([`Chunks::remaining`] empty: all of it).
    Fetched(Chunks),
    /// A device sent what does not match its hash, or the fetch failed: what came is in the temp file, which the next
    /// job checks chunk by chunk again.
    Failed(String),
    /// The temp file is gone (or not a file): the next job starts over.
    Gone,
}

fn stamp_meta(m: &Meta) -> (u64, u64, i64) {
    (m.inode, m.size, m.mtime_ns)
}

fn read_full(fh: &dyn crate::fs::FileHandle, mut off: u64, mut buf: &mut [u8]) -> io::Result<bool> {
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

/// Builds `job`'s file in its temp file (SPEC §7G.2) with the folder's core let go: a temp file an earlier job left
/// as it was, or one left over checked chunk by chunk, or our old version cloned (only if it is unchanged from before
/// to after the copy); then each chunk still missing from another local file that has it (`index`, a read-only view:
/// every chunk checked against its hash), and the rest from peers, in windows of at most `window` bytes, every chunk
/// checked and written as it comes (a host shows how far a fetch is). Blocking, and holding nothing of the folder's.
/// `stop` is asked between windows; `fetched` counts the bytes that came from peers.
pub fn build_chunks(
    fs: &dyn Fs,
    source: &dyn ChunkSource,
    index: &dyn crate::index::IndexStore,
    job: ChunkJob,
    stop: &dyn Fn() -> bool,
    window: u64,
    fetched: &mut u64,
) -> io::Result<ChunksEnd> {
    let left = fs.lstat(&job.tmp)?;
    let missing: Vec<(u64, ChunkRef)> = match job.known {
        Some((at, still)) if left.as_ref().is_some_and(|m| m.kind == FileKind::File && stamp_meta(m) == at) => still,
        _ => {
            // A leftover that is not a file (a symlink a step was making when it stopped) would fail every time.
            match left.as_ref().map(|m| m.kind) {
                Some(FileKind::Symlink) => fs.remove_file(&job.tmp)?,
                Some(FileKind::Dir) => fs.remove_dir(&job.tmp)?,
                _ => {}
            }
            let resumed = left.is_some_and(|m| m.kind == FileKind::File);
            let mut in_place: HashMap<u64, [u8; 32]> = HashMap::new();
            if !resumed && let Some((stamp, chunks)) = &job.old {
                let before = fs.lstat(&job.path)?.map(|m| stamp_of(&m));
                if before == Some(*stamp) {
                    fs.copy(&job.path, &job.tmp)?;
                    // Saved to during the copy: what was copied may be part old and part new. Not trusted.
                    if fs.lstat(&job.path)?.map(|m| stamp_of(&m)) == Some(*stamp) {
                        in_place.extend(crate::index::offsets(chunks).map(|(off, c)| (off, c.hash)));
                    } else {
                        fs.remove_file(&job.tmp)?;
                    }
                }
            }
            let fh = fs.open_rw(&job.tmp)?;
            let mut missing = Vec::new();
            for (off, c) in crate::index::offsets(&job.chunks) {
                if in_place.get(&off) == Some(&c.hash) {
                    continue;
                }
                let mut buf = vec![0u8; c.len as usize];
                if resumed && fh.len()? >= off + u64::from(c.len) && read_full(&*fh, off, &mut buf)? && crate::pull::hash(&buf) == c.hash {
                    continue;
                }
                // From another local file that has the chunk (checked: that file may have changed since it was scanned).
                let mut found = false;
                for (src, src_off) in index.chunk_sources(&c.hash, 4)? {
                    let Ok(from) = fs.open_read(&src) else { continue };
                    if read_full(&*from, src_off, &mut buf)? && crate::pull::hash(&buf) == c.hash {
                        fh.write_at(off, &buf)?;
                        found = true;
                        break;
                    }
                }
                if !found {
                    missing.push((off, *c));
                }
            }
            fh.sync_barrier()?;
            missing
        }
    };
    if !fs.lstat(&job.tmp)?.is_some_and(|m| m.kind == FileKind::File) {
        return Ok(ChunksEnd::Gone);
    }
    let fh = fs.open_rw(&job.tmp)?;
    let mut remaining: Vec<(u64, ChunkRef)> = Vec::new();
    let mut failed: Option<String> = None;
    let mut i = 0;
    while i < missing.len() {
        if failed.is_some() || stop() || job.holders.is_empty() {
            remaining.extend_from_slice(&missing[i..]);
            break;
        }
        let mut j = i;
        let mut bytes = 0u64;
        while j < missing.len() && (j == i || bytes + u64::from(missing[j].1.len) <= window) {
            bytes += u64::from(missing[j].1.len);
            j += 1;
        }
        let win = &missing[i..j];
        let reqs: Vec<ChunkRequest> = win
            .iter()
            .map(|(off, c)| ChunkRequest { path: job.path.clone(), offset: *off, len: c.len, hash: c.hash, holders: job.holders.clone() })
            .collect();
        source.fetch_each(&reqs, &mut |k, res| {
            let (off, c) = &win[k];
            if failed.is_some() {
                remaining.push((*off, *c));
                return;
            }
            match res {
                Ok(data) if data.len() == c.len as usize && crate::pull::hash(&data) == c.hash => match fh.write_at(*off, &data) {
                    Ok(()) => *fetched += data.len() as u64,
                    Err(e) => {
                        remaining.push((*off, *c));
                        failed = Some(e.to_string());
                    }
                },
                Ok(_) | Err(FetchError::Corrupt(_)) => {
                    remaining.push((*off, *c));
                    failed = Some("a device sent data that does not match its hash".into());
                }
                Err(FetchError::Unavailable) => remaining.push((*off, *c)),
                Err(FetchError::Failed(why)) => {
                    remaining.push((*off, *c));
                    failed = Some(why);
                }
            }
        });
        i = j;
    }
    // What came is behind a barrier before the next round trusts it.
    fh.sync_barrier()?;
    drop(fh);
    if let Some(why) = failed {
        return Ok(ChunksEnd::Failed(why));
    }
    let Some(m) = fs.lstat(&job.tmp)? else { return Ok(ChunksEnd::Gone) };
    Ok(ChunksEnd::Fetched(Chunks { version: job.version, tmp: stamp_meta(&m), remaining, local: job.local }))
}

/// Runs a [`Job`] with the folder's core let go: a stream ([`stream_file`]) or a hashed file's build ([`build_chunks`],
/// `index` a read-only view of the folder's index, peers asked in windows of `window` bytes).
pub fn run_job(fs: &dyn Fs, source: &dyn ChunkSource, index: &dyn crate::index::IndexStore, job: Job, stop: &dyn Fn() -> bool, window: u64, fetched: &mut u64) -> io::Result<JobEnd> {
    Ok(match job {
        Job::Stream(j) => JobEnd::Stream(stream_file(fs, source, *j, stop, fetched)?),
        Job::Chunks(j) => JobEnd::Chunks(build_chunks(fs, source, index, *j, stop, window, fetched)?),
    })
}

/// Locks the jobs' state ([`Streams`]), poisoned or not (it holds no invariant a panic could break halfway).
pub fn lock(m: &std::sync::Mutex<Streams>) -> std::sync::MutexGuard<'_, Streams> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Streams {
    /// How a job for `path` ended ([`crate::folder::FolderCore::take_jobs`]), `now_ns` the time now. A stream: done,
    /// it is placed and recorded by the next pull round (if that version is still wanted there); cut off, the next one
    /// goes on from there; failed, the next round says why. A fetch of chunks: the next round asks for what is still
    /// missing, or places the file; failed, it says why. Needs no core: a round never holds up a job's end.
    pub fn job_ended(&mut self, path: &str, end: JobEnd, now_ns: i64) {
        let version = self.running.get(path).map(|(v, _)| v.clone());
        self.cancelled(path);
        let failed = |why: String| version.clone().map(|v| (path.to_string(), (v, why)));
        match end {
            JobEnd::Stream(end) => {
                self.resume.remove(path);
                match end {
                    StreamEnd::Done(p) => {
                        self.prepared.insert(path.to_string(), p);
                    }
                    StreamEnd::Waiting(r) => {
                        if let Some(r) = r {
                            self.resume.insert(path.to_string(), (r, now_ns));
                        }
                    }
                    StreamEnd::Failed(why) => {
                        self.failed.extend(failed(why));
                    }
                }
            }
            JobEnd::Chunks(end) => match end {
                ChunksEnd::Fetched(c) => {
                    self.chunks.insert(path.to_string(), c);
                }
                ChunksEnd::Failed(why) => {
                    self.chunks.remove(path);
                    self.failed.extend(failed(why));
                }
                ChunksEnd::Gone => {
                    self.chunks.remove(path);
                }
            },
        }
    }
}

impl Streams {
    /// A job for `path` taken and not run after all (the runner dropped it for a later one of the same path).
    pub fn cancelled(&mut self, path: &str) {
        if let Some((_, n)) = self.running.get_mut(path) {
            *n -= 1;
            if *n == 0 {
                self.running.remove(path);
            }
        }
    }

    /// A job for `path` of `version` is taken: until it ends, a round leaves the path alone.
    pub(crate) fn taken(&mut self, path: &str, version: &Version) {
        let e = self.running.entry(path.to_string()).or_insert((version.clone(), 0));
        e.0 = version.clone();
        e.1 += 1;
    }
}

/// A stream to run: the path, the unhashed version wanted, and where an earlier one stopped.
#[derive(Clone)]
pub struct StreamJob {
    /// The path (wire form).
    pub path: String,
    /// The unhashed version.
    pub want: SignedFile,
    /// Where an earlier stream of it stopped (its temp file holds that much).
    pub resume: Option<Resume>,
}

/// A stream that came to its end: its temp file ([`crate::pull::stream_temp_path`]) holds the bytes, every chunk
/// checked; `file` is the version put together with the chunks that came and the maker's signature (checked against
/// the folder's writers when it is placed).
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The hashed version.
    pub file: SignedFile,
    /// The temp file as it was left (file id, mtime): placed only if it is still that file.
    pub tmp: (u64, i64),
}

/// How a stream ended.
pub enum StreamEnd {
    /// All of it came.
    Done(Prepared),
    /// The maker is away or slow, or the stream was stopped: what came is kept, to go on from.
    Waiting(Option<Resume>),
    /// It failed (the file changed on its maker, a wrong hash): nothing of it is kept.
    Failed(String),
}

/// Runs a stream of `job` from its maker into its temp file (SPEC §7G.11): each piece in order, every chunk checked
/// against its hash as its bytes arrive, written as it comes (a host shows how far it is). Blocking, and holding
/// nothing of the folder's: run it with the folder's core let go. `stop` is asked before each piece (the folder stops);
/// `fetched` counts the bytes that came.
pub fn stream_file(fs: &dyn Fs, source: &dyn ChunkSource, job: StreamJob, stop: &dyn Fn() -> bool, fetched: &mut u64) -> io::Result<StreamEnd> {
    let want = &job.want.info;
    let tmp = crate::pull::stream_temp_path(&job.path);
    let resumed = job.resume.filter(|r| r.version == want.version);
    let (fh, mut check) = match resumed {
        Some(r) if fs.lstat(&tmp)?.is_some_and(|m| m.kind == FileKind::File && m.size >= r.check.fed()) => (fs.open_rw(&tmp)?, r.check),
        _ => {
            match fs.lstat(&tmp)?.map(|m| m.kind) {
                Some(FileKind::Dir) => fs.remove_dir(&tmp)?,
                Some(_) => fs.remove_file(&tmp)?,
                None => {}
            }
            (fs.open_rw(&tmp)?, StreamCheck::new())
        }
    };
    let keep = |check: StreamCheck| (check.fed() > 0).then(|| Resume { version: want.version.clone(), check });
    let mut sig: Option<[u8; 64]> = None;
    let mut restarted = false;
    while check.fed() < want.size {
        let off = check.fed();
        if stop() {
            fh.sync_barrier()?;
            return Ok(StreamEnd::Waiting(keep(check)));
        }
        let ask = StreamAsk {
            path: job.path.clone(),
            version: want.version.clone(),
            offset: off,
            len: u32::try_from(want.size - off).unwrap_or(u32::MAX).min(STREAM_PIECE),
            maker: want.modified_by,
        };
        let got = match source.stream(&ask) {
            Ok(g) => g,
            // Kept for a later round: the maker serves the piece whose answer was lost once more.
            Err(StreamFail::Unavailable) => {
                fh.sync_barrier()?;
                return Ok(StreamEnd::Waiting(keep(check)));
            }
            // The maker's read is gone: once more from the first byte, now.
            Err(StreamFail::Restart) if off > 0 && !restarted => {
                restarted = true;
                fh.set_len(0)?;
                check = StreamCheck::new();
                continue;
            }
            Err(StreamFail::Restart) => return Ok(StreamEnd::Waiting(None)),
            Err(StreamFail::Changed) => {
                return Ok(StreamEnd::Failed("it changed on the device that has it while it was read; it is fetched again once its new version is here".into()));
            }
            Err(StreamFail::Failed(why)) => return Ok(StreamEnd::Failed(why)),
        };
        let short = got.data.len() as u64 != u64::from(ask.len);
        if short || !check.feed(&got.data, &got.chunks) {
            return Ok(StreamEnd::Failed("a device sent data that does not match its hash".into()));
        }
        fh.write_at(off, &got.data)?;
        *fetched += got.data.len() as u64;
        if got.sig.is_some() {
            sig = got.sig;
            break;
        }
    }
    let (Some(sig), Some(chunks)) = (sig, check.finish(want.size)) else {
        return Ok(StreamEnd::Failed("the device that has it ended the file without its signature".into()));
    };
    fh.set_len(want.size)?;
    fh.sync_barrier()?;
    drop(fh);
    let Some(m) = fs.lstat(&tmp)? else { return Ok(StreamEnd::Waiting(None)) };
    Ok(StreamEnd::Done(Prepared { file: SignedFile { info: FileInfo { chunks, ..want.clone() }, sig }, tmp: (m.inode, m.mtime_ns) }))
}

/// Where the hashed form of a path's unhashed version is noted: in the folder's state directory.
fn note_path(p: &str) -> String {
    let h = Sha256::digest(p.as_bytes());
    let hex: String = h[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{STATE_DIR}/hashed/{hex}")
}

/// Notes the hashed form of an unhashed version (SPEC §7G.11): on its maker, what it signed at the end of a stream (its
/// file unchanged throughout), so its scans know the version's chunks even when no receiver's index has come (a
/// receiver that stopped before recording it never announces it); on a receiver, what it is about to place, so a
/// stop between the rename and the commit is recognised by its bytes. Durable before it returns.
pub fn note_hashed(fs: &dyn Fs, file: &SignedFile) -> io::Result<()> {
    let dir = format!("{STATE_DIR}/hashed");
    if fs.lstat(&dir)?.is_none() {
        fs.create_dir(&dir)?;
    }
    let mut b = Vec::new();
    trs_sync_proto::msg::encode_entry(&mut b, 0, false, file);
    let fh = fs.open_rw(&note_path(&file.info.path))?;
    fh.set_len(0)?;
    fh.write_at(0, &b)?;
    fh.sync_all()
}

/// The hashed form noted for the unhashed version `v` ([`note_hashed`]): the same version of the same file, with
/// its chunks; `None` when none is noted (or the note is of another version, or unreadable).
pub fn noted_hashed(fs: &dyn Fs, v: &FileInfo) -> Option<SignedFile> {
    if !v.is_unhashed() {
        return None;
    }
    let b = crate::fs::read_all(fs, &note_path(&v.path)).ok()?;
    let e = trs_sync_proto::msg::decode_entry(&mut trs_sync_proto::codec::Reader::new(&b)).ok()?;
    let f = e.file;
    (!f.info.is_unhashed() && f.info.path == v.path && f.info.same_content(v)).then_some(f)
}

/// Forgets a path's note ([`note_hashed`]), if there is one.
pub fn forget_noted(fs: &dyn Fs, p: &str) {
    let _ = fs.remove_file(&note_path(p));
}

/// The receiver's check of a streamed file: every chunk's hash over its bytes as they arrive, and at the end that the
/// chunks cover the file exactly.
#[derive(Clone, Default)]
pub struct StreamCheck {
    /// Bytes fed so far.
    pos: u64,
    /// Where the chunk being hashed starts.
    start: u64,
    sha: Sha256,
    chunks: Vec<ChunkRef>,
}

impl StreamCheck {
    /// A check from the first byte.
    pub fn new() -> Self {
        StreamCheck::default()
    }

    /// The next piece's bytes and the chunks that ended in them (or in the bytes before): false when a chunk does not
    /// hash to what it says, or ends outside the bytes sent so far.
    pub fn feed(&mut self, mut data: &[u8], ends: &[ChunkRef]) -> bool {
        for c in ends {
            let end = self.start + u64::from(c.len);
            if c.len == 0 || end < self.pos || end - self.pos > data.len() as u64 {
                return false;
            }
            let take = (end - self.pos) as usize;
            self.sha.update(&data[..take]);
            data = &data[take..];
            self.pos = end;
            let hash: [u8; 32] = self.sha.finalize_reset().into();
            if hash != c.hash {
                return false;
            }
            self.start = end;
            self.chunks.push(*c);
        }
        self.sha.update(data);
        self.pos += data.len() as u64;
        true
    }

    /// Bytes fed so far.
    pub fn fed(&self) -> u64 {
        self.pos
    }

    /// The chunks of a file of `size` bytes, once every byte was fed and is in a checked chunk.
    pub fn finish(self, size: u64) -> Option<Vec<ChunkRef>> {
        (self.pos == size && self.start == size).then_some(self.chunks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{MemFs, write_all};
    use trs_sync_proto::ids::ShortId;
    use trs_sync_proto::model::{FileKind, SignedFile};

    fn data(n: usize) -> Vec<u8> {
        let mut s = 7u64;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                (s >> 33) as u8
            })
            .collect()
    }

    fn record(fs: &MemFs, path: &str) -> LocalRecord {
        let m = fs.lstat(path).unwrap().unwrap();
        let info = FileInfo {
            path: path.into(),
            kind: FileKind::File,
            size: m.size,
            mtime_ns: m.mtime_ns,
            exec: false,
            deleted: false,
            version: Version::new().bump(ShortId(1), 5),
            modified_by: ShortId(1),
            file_id: [1; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks: Vec::new(),
        };
        let stamp = Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode };
        LocalRecord { file: SignedFile { info, sig: [0; 64] }, sequence: 1, invalid: false, stamp: Some(stamp), tier: None, dataless: false, want: false }
    }

    /// The failure: a streamed file's chunks differ from the ones a scan would record (another cut, a lost chunk at a
    /// piece's edge), so the receiver's check or a later chunk by chunk download of the hashed version fails.
    #[test]
    fn a_stream_in_pieces_hashes_the_file_as_a_scan_would_and_the_receiver_checks_every_chunk() {
        let fs = MemFs::new();
        let bytes = data(5_000_000);
        write_all(&fs, "f.bin", &bytes).unwrap();
        let rec = record(&fs, "f.bin");
        let mut s = Stream::start(&fs, &rec).unwrap();
        let mut check = StreamCheck::new();
        let (mut off, mut hashed) = (0u64, None);
        while hashed.is_none() {
            let p = s.piece(&fs, off, 777_777).unwrap();
            assert!(check.feed(&p.data, &p.chunks), "every chunk checks at {off}");
            off += p.data.len() as u64;
            hashed = p.done;
        }
        let hashed = hashed.unwrap();
        let scanned = chunk::chunk_bytes(&bytes, Tier::for_size(bytes.len() as u64));
        assert_eq!(hashed.info.chunks, scanned);
        assert_eq!(check.finish(bytes.len() as u64), Some(scanned));
        assert!(hashed.info.same_content(&rec.file.info), "the same version, now with its hashes");
    }

    /// The failure: a file written to while it is streamed is recorded with chunks of no version (part old, part new).
    #[test]
    fn a_file_changed_while_it_is_streamed_hands_back_nothing_to_record() {
        let fs = MemFs::new();
        write_all(&fs, "f.bin", &data(3_000_000)).unwrap();
        let rec = record(&fs, "f.bin");
        let mut s = Stream::start(&fs, &rec).unwrap();
        s.piece(&fs, 0, 1 << 20).unwrap();
        // Rewritten in place with the same size: only its stamp says so.
        fs.open_rw("f.bin").unwrap().write_at(2 << 20, &[9; 1000]).unwrap();
        let mut off = 1 << 20;
        let end = loop {
            match s.piece(&fs, off, 1 << 20) {
                Ok(p) => {
                    assert!(p.done.is_none(), "no hashes of mixed content");
                    off += p.data.len() as u64;
                }
                Err(e) => break e,
            }
        };
        assert!(matches!(end, StreamError::Changed), "{end:?}");
        // Changed since its record: not even started.
        assert!(matches!(Stream::start(&fs, &rec), Err(StreamError::Changed)));
    }

    /// The failure: a piece asked out of order (a second request racing the first, an earlier piece) is hashed into
    /// the chunker, which then cuts chunks of bytes in the wrong order. Only the last piece may be asked again (its
    /// answer was lost), once: the same bytes and chunks, the chunker untouched.
    #[test]
    fn a_piece_out_of_order_is_refused_and_only_the_last_is_served_again() {
        let fs = MemFs::new();
        write_all(&fs, "f.bin", &data(100_000)).unwrap();
        let mut s = Stream::start(&fs, &record(&fs, "f.bin")).unwrap();
        assert!(matches!(s.piece(&fs, 10, 100), Err(StreamError::OutOfOrder)));
        s.piece(&fs, 0, 100).unwrap();
        let second = s.piece(&fs, 100, 60_000).unwrap();
        let again = s.piece(&fs, 100, 60_000).unwrap();
        assert_eq!((again.data, again.chunks), (second.data, second.chunks));
        // The failure: a receiver (or anyone) asks the same piece over and over, and the maker reads it every time.
        assert!(matches!(s.piece(&fs, 100, 60_000), Err(StreamError::OutOfOrder)), "served again once only");
        assert!(matches!(s.piece(&fs, 0, 100), Err(StreamError::OutOfOrder)), "not an earlier one");
        assert!(matches!(s.piece(&fs, 100, 100), Err(StreamError::OutOfOrder)), "nor the last with another length");
        assert_eq!(s.next(), 60_100);
        let last = s.piece(&fs, 60_100, 39_900).unwrap();
        assert!(last.done.is_some());
        assert!(s.piece(&fs, 60_100, 39_900).unwrap().done.is_some(), "the end again, with the file's hashes");
        assert!(matches!(s.piece(&fs, 60_100, 39_900), Err(StreamError::OutOfOrder)), "once");
        assert!(s.ended());
    }

    /// The failure: a receiver takes bytes that do not hash to the chunks sent with them, or a chunk list that leaves
    /// bytes out.
    #[test]
    fn the_receiver_refuses_a_wrong_hash_and_an_incomplete_list() {
        let bytes = data(300_000);
        let chunks = chunk::chunk_bytes(&bytes, Tier::for_size(300_000));
        let mut bad = chunks.clone();
        bad[0].hash[0] ^= 1;
        assert!(!StreamCheck::new().feed(&bytes, &bad));
        let mut check = StreamCheck::new();
        assert!(check.feed(&bytes, &chunks[..chunks.len() - 1]));
        assert_eq!(check.finish(300_000), None, "the last chunk never came");
        let mut long = StreamCheck::new();
        assert!(!long.feed(&bytes[..10], &chunks), "a chunk ending past the bytes sent");
    }
}
