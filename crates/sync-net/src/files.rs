//! Files in chat messages (SPEC §7G.7 *Files*): any file, any size, in a channel or a private conversation. The
//! sender serves the original in place; nothing is copied and nothing downloads by itself.
//!
//! * **Sending.** The file is read once, off the service's locks ([`prepare`]): its SHA-256 and its piece table (the
//!   SHA-256 of each [`PIECE`] of it, [`trs_sync_proto::attach`]). The message is posted when that is done, naming the
//!   file, its size and the table's hash; until then the sender's conversation shows it as being prepared. The share
//!   (the file's path, size, modification time and hashes) is kept in the room's container, sealed.
//! * **Serving.** A piece table is served from the sealed copy kept here. A piece is read from the file where it is,
//!   and only when the file has the size and modification time it had when it was sent and the piece still hashes to
//!   its entry in the table; otherwise the share is over and the answer is *gone* ([`BlobCode::Gone`]), for good. A
//!   device that downloaded the file serves its copy the same way while it is unchanged (answering *missing* once it
//!   is not), so a member can fetch it from whoever has it.
//! * **Downloading.** Only when the user asks, to a folder (a free name in it, [`unique_path`]) or to a file chosen
//!   with Save As. The piece table is fetched first and checked against its hash; then each piece, a few at a time,
//!   checked against its entry, written at its place in a partial file next to the destination
//!   (`<name>.seedwell-part`) and recorded. A bad piece is fetched again alone; a peer that sends three is passed over.
//!   Memory stays flat: at most [`WINDOW`] pieces are in flight. The record of verified pieces is saved every
//!   [`SAVE_EVERY`] (after the partial file is flushed), so a download resumes after a restart from what it had. When
//!   nobody that has the file is connected the download pauses, and resumes when one connects. When done, the file
//!   gets the system's mark for downloaded files ([`trs_platform::fs::mark_downloaded`]) and its name.
//! * **Kept** in the room's container (`<channel id>.files/`, `dm-<device>.files/`): `state` (the shares and
//!   downloads, sealed JSON) and one sealed piece table per file (`<table hash>`). Deleted with the room, with the
//!   partial files of unfinished downloads; finished downloads are the user's files and stay.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use trs_sync_proto::attach::{PIECE, piece_count, piece_span, safe_name, table_len};
use trs_sync_proto::members::hex;
use trs_sync_proto::msg::{BlobCode, BlobData, BlobWant};

use crate::at_rest::Sealer;
use crate::blobs::{hash_from_hex, sha256};
use crate::room::{Named, Room, file_of_table, named};
use crate::shared::{Ctx, PeerHandle, lock};

/// The associated data of the sealed files of a room's files.
pub(crate) const AAD: &[u8] = b"SWFL";
/// What a partial download's name ends with.
pub const PART_SUFFIX: &str = ".seedwell-part";
/// Pieces asked for at once from one peer.
pub const WINDOW: usize = 4;
/// How long one piece may take.
const PIECE_TIME: Duration = Duration::from_secs(30);
/// How often the record of verified pieces is saved while a download runs.
const SAVE_EVERY: Duration = Duration::from_millis(500);
/// Bad pieces after which a peer is passed over.
const BAD_PIECES: u32 = 3;
/// The largest piece table this version holds (a file of 8 TiB).
const MAX_TABLE: u64 = 256 << 20;

/// **Tests:** what the serving side does to the pieces it sends.
#[derive(Debug, Default)]
pub struct Hooks {
    /// The next this many pieces served are changed after they were checked (as a peer that sends bad bytes).
    pub corrupt: AtomicU32,
    /// Each piece served waits this long first.
    pub delay_ms: AtomicU64,
    /// Pieces served so far.
    pub served: AtomicU64,
}

/// What [`prepare`] found: what a message names a file by, and what its share keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// Bytes.
    pub size: u64,
    /// Its modification time (Unix seconds, nanoseconds) when it was read.
    pub mtime: (i64, u32),
    /// SHA-256 of the whole file.
    pub hash: [u8; 32],
    /// SHA-256 of each piece, one after another.
    pub table: Vec<u8>,
    /// SHA-256 of `table`.
    pub table_hash: [u8; 32],
}

/// A file's size and modification time.
fn stamp(path: &Path) -> io::Result<(u64, (i64, u32))> {
    let m = std::fs::metadata(path)?;
    if m.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "it is a folder"));
    }
    Ok((m.len(), trs_platform::fs::unix_time(m.modified()?)))
}

/// Reads a file once to name it: its hash and piece table. `done` counts the bytes read (for "Preparing… 40 %");
/// `cancel` stops it. Fails when the file changes while it is read. Blocking: call it off the async threads.
pub fn prepare(path: &Path, done: &AtomicU64, cancel: &AtomicBool) -> io::Result<Prepared> {
    let (size, mtime) = stamp(path)?;
    if table_len(size) > MAX_TABLE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the file is larger than this version can send (8 TiB)"));
    }
    let mut f = File::open(path)?;
    let mut whole = ring::digest::Context::new(&ring::digest::SHA256);
    let mut table = Vec::with_capacity(table_len(size) as usize);
    let mut buf = vec![0u8; PIECE as usize];
    for i in 0..piece_count(size) {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        let (_, len) = piece_span(size, i);
        let b = &mut buf[..len as usize];
        f.read_exact(b).map_err(|e| if e.kind() == io::ErrorKind::UnexpectedEof { changed() } else { e })?;
        whole.update(b);
        table.extend_from_slice(&sha256(b));
        done.fetch_add(len, Ordering::Relaxed);
    }
    if stamp(path)? != (size, mtime) {
        return Err(changed());
    }
    let hash: [u8; 32] = whole.finish().as_ref().try_into().expect("32 bytes");
    let table_hash = sha256(&table);
    Ok(Prepared { size, mtime, hash, table, table_hash })
}

fn changed() -> io::Error {
    io::Error::other("the file changed while it was being read; try again")
}

/// A file served from here: one sent from here (`own`), or a downloaded copy.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Share {
    hash: String,
    table: String,
    path: PathBuf,
    size: u64,
    mtime_s: i64,
    mtime_ns: u32,
    own: bool,
    /// Why it is served no more (sent from here: shown on the sender's card).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gone: Option<String>,
}

impl Share {
    fn matches(&self) -> bool {
        stamp(&self.path).is_ok_and(|s| s == (self.size, (self.mtime_s, self.mtime_ns)))
    }
}

/// Where a download stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DlState {
    Active,
    Paused,
    Done,
    Gone,
    Failed,
}

/// A download, as saved.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DlRecord {
    hash: String,
    /// Its piece table's hash.
    table: String,
    name: String,
    size: u64,
    /// Where it goes when done.
    to: PathBuf,
    /// Save As: the user chose this file (and to replace one there).
    replace: bool,
    /// The partial file.
    part: PathBuf,
    /// The verified pieces, a bitmap in hex.
    have: String,
    state: DlState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    problem: Option<String>,
}

/// A download on this device.
struct Download {
    rec: DlRecord,
    have: Vec<u8>,
    /// Bytes verified.
    done: u64,
    /// A task runs it; set to stop it.
    running: Option<Arc<AtomicBool>>,
    /// Bytes per second, measured over the last second or so.
    rate: u64,
    window: (Instant, u64),
    saved: Instant,
    /// A cancel or the end of a run is deleting or putting in place its partial file, with the state let go: nobody
    /// else starts, cancels or ends it meanwhile (a new run to the same place has the same partial file).
    busy: bool,
}

impl Download {
    fn has(&self, i: u64) -> bool {
        self.have.get((i / 8) as usize).is_some_and(|b| b & (1 << (i % 8)) != 0)
    }
}

/// A send whose files are being read.
struct Preparing {
    id: u64,
    names: Vec<String>,
    total: u64,
    done: Arc<AtomicU64>,
    problem: Option<String>,
}

/// A send being prepared, for the sender's conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparingInfo {
    /// What dismisses it ([`crate::service::SyncHandle::chat_dismiss_preparing`]).
    pub id: u64,
    /// The files' names.
    pub names: Vec<String>,
    /// Bytes read so far, of `total`.
    pub done: u64,
    /// Bytes of the files together.
    pub total: u64,
    /// Why it was not sent (it then waits to be dismissed).
    pub problem: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    shares: Vec<Share>,
    #[serde(default)]
    downloads: Vec<DlRecord>,
}

#[derive(Default)]
struct State {
    shares: HashMap<[u8; 32], Share>,
    downloads: HashMap<[u8; 32], Download>,
    preparing: Vec<Preparing>,
    next_id: u64,
    /// The piece table used last (serving reads it for every piece).
    table: Option<([u8; 32], Arc<Vec<u8>>)>,
    /// The room is gone: nothing starts any more.
    closed: bool,
    /// Saves made of it so far ([`Files::snapshot`]): the turn of the next one.
    saves: u64,
}

/// A save of a room's files: their state as it was when it was made (sealed), and its turn.
struct Snapshot {
    turn: u64,
    bytes: Vec<u8>,
}

/// The writes of a room's files' state.
#[derive(Default)]
struct Saving {
    /// The turn of the newest state on disk.
    written: u64,
    /// The room is gone: nothing is written any more (its folder is deleted).
    closed: bool,
}

/// A file attachment as this device has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileView {
    /// `available`, `downloading`, `paused`, `downloaded`, `gone`, `failed`, `shared` or `unshared`.
    pub state: &'static str,
    /// Bytes downloaded.
    pub done: u64,
    /// Bytes per second, while downloading.
    pub rate: u64,
    /// Where it was downloaded to, or (sent from here) where it is shared from.
    pub path: Option<PathBuf>,
    /// Why it failed, or is no longer available or shared.
    pub problem: Option<String>,
}

/// Where a download goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadTo {
    /// Into a folder, under the file's name or a free one like it.
    Folder(PathBuf),
    /// To this file (Save As: the user agreed to replace one there).
    File(PathBuf),
}

/// A room's files on this device.
///
/// What the app's calls read ([`Files::view`], [`Files::preparing_list`]) is never locked across a disk write: a
/// save copies the state under its lock ([`Files::snapshot`]) and writes it after it let go, under a lock of its own
/// ([`Files::write`]), where an older copy never replaces a newer one. A disk may take long to save (Windows' Defender
/// reads each new file before the write is done).
pub(crate) struct Files {
    dir: PathBuf,
    seal: Sealer,
    state: Mutex<State>,
    /// Taken for a save's whole write, never by a read.
    saving: Mutex<Saving>,
    /// **Tests:** what [`Files::test_gap`] runs.
    #[cfg(test)]
    gap: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Files {
    /// The folder of a room's files (`base`: `<channel id>` or `dm-<device>`).
    pub(crate) fn dir_of(state_dir: &Path, base: &str) -> PathBuf {
        state_dir.join(format!("{base}.files"))
    }

    /// Opens a room's files: what was shared and downloaded (a record that does not open is dropped).
    pub(crate) fn open(dir: PathBuf, seal: Sealer) -> Files {
        let mut st = State::default();
        let saved: Saved = std::fs::read(dir.join("state")).ok().and_then(|b| seal.open(&b)).and_then(|p| serde_json::from_slice(&p).ok()).unwrap_or_default();
        for s in saved.shares {
            if let Some(h) = hash_from_hex(&s.hash) {
                st.shares.insert(h, s);
            }
        }
        let now = Instant::now();
        for rec in saved.downloads {
            let Some(h) = hash_from_hex(&rec.hash) else { continue };
            let have = bitmap_from_hex(&rec.have, piece_count(rec.size));
            let done = (0..piece_count(rec.size)).filter(|i| have.get((i / 8) as usize).is_some_and(|b| b & (1 << (i % 8)) != 0)).map(|i| piece_span(rec.size, i).1).sum();
            st.downloads.insert(h, Download { rec, have, done, running: None, rate: 0, window: (now, 0), saved: now, busy: false });
        }
        Files {
            dir,
            seal,
            state: Mutex::new(st),
            saving: Mutex::new(Saving::default()),
            #[cfg(test)]
            gap: Mutex::new(None),
        }
    }

    /// The state as it is now, to save: a copy made under its lock, written after it is let go ([`Self::write`]).
    fn snapshot(&self, st: &mut State) -> io::Result<Snapshot> {
        let saved = Saved {
            shares: st.shares.values().cloned().collect(),
            downloads: st.downloads.values().map(|d| DlRecord { have: hex(&d.have), ..d.rec.clone() }).collect(),
        };
        let plain = serde_json::to_vec(&saved).map_err(io::Error::other)?;
        st.saves += 1;
        Ok(Snapshot { turn: st.saves, bytes: self.seal.seal(&plain) })
    }

    /// Writes a copy of the state atomically, unless a newer one is on disk already (the file never goes back to an
    /// older state) or the room is gone. The writes take turns.
    fn write(&self, s: Snapshot) -> io::Result<()> {
        let mut saving = lock(&self.saving);
        if saving.closed || s.turn <= saving.written {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(".state.tmp");
        std::fs::write(&tmp, &s.bytes)?;
        crate::shared::slow_disk();
        std::fs::rename(&tmp, self.dir.join("state"))?;
        saving.written = s.turn;
        Ok(())
    }

    /// Saves the state: copied under its lock (`st`, let go here), then written.
    fn save(&self, mut st: std::sync::MutexGuard<'_, State>) -> io::Result<()> {
        let snapshot = self.snapshot(&mut st);
        drop(st);
        self.write(snapshot?)
    }

    /// Keeps a file's piece table, sealed.
    fn put_table(&self, hash: &[u8; 32], table: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!(".{}.tmp", hex(hash)));
        std::fs::write(&tmp, self.seal.seal(table))?;
        crate::shared::slow_disk();
        std::fs::rename(&tmp, self.dir.join(hex(hash)))
    }

    /// **Tests:** called once where a cancel or the end of a run has let the state go before it touches the partial
    /// file (to interleave another call there).
    #[cfg(test)]
    fn test_gap(&self) {
        let f = lock(&self.gap).take();
        if let Some(f) = f {
            f();
        }
    }

    #[cfg(not(test))]
    fn test_gap(&self) {}

    /// A piece table kept here, checked against its hash.
    fn table(&self, hash: &[u8; 32]) -> Option<Arc<Vec<u8>>> {
        if let Some((h, t)) = lock(&self.state).table.as_ref()
            && h == hash
        {
            return Some(t.clone());
        }
        let plain = self.seal.open(&std::fs::read(self.dir.join(hex(hash))).ok()?)?;
        if sha256(&plain) != *hash {
            return None;
        }
        let t = Arc::new(plain);
        lock(&self.state).table = Some((*hash, t.clone()));
        Some(t)
    }

    /// Serves `path` as the file `p` names (sent from here when `own`).
    pub(crate) fn share(&self, p: &Prepared, path: &Path, own: bool) -> io::Result<()> {
        self.put_table(&p.table_hash, &p.table)?;
        let mut st = lock(&self.state);
        let s = Share { hash: hex(&p.hash), table: hex(&p.table_hash), path: path.to_path_buf(), size: p.size, mtime_s: p.mtime.0, mtime_ns: p.mtime.1, own, gone: None };
        st.shares.insert(p.hash, s);
        self.save(st)
    }

    /// Stops serving a file sent from here. False when it was not shared.
    pub(crate) fn stop_sharing(&self, hash: &[u8; 32]) -> io::Result<bool> {
        let mut st = lock(&self.state);
        let Some(s) = st.shares.get_mut(hash).filter(|s| s.own) else { return Ok(false) };
        s.gone = Some("you stopped sharing it".into());
        self.save(st)?;
        Ok(true)
    }

    /// How a file stands here; `mine`: its message was sent from this device. The app calls it on its UI thread
    /// (each file card shown): it looks at the file with the state let go, and never saves.
    pub(crate) fn view(&self, hash: &[u8; 32], mine: bool) -> FileView {
        let h = *hash;
        let own = lock(&self.state).shares.get(&h).filter(|s| s.own).cloned();
        if let Some(s) = own {
            if s.gone.is_none() && !s.matches() {
                // Checked when shown, so the sender sees it at once (serving finds it too, and saves it). Kept in
                // memory only: a view finds it again from the file after a restart.
                let why = if s.path.exists() { "the file changed since it was sent" } else { "the file was moved or deleted" };
                if let Some(x) = lock(&self.state).shares.get_mut(&h) {
                    x.gone.get_or_insert_with(|| why.into());
                }
                return FileView { state: "unshared", path: Some(s.path), problem: Some(why.into()), ..FileView::default() };
            }
            return match s.gone {
                Some(why) => FileView { state: "unshared", path: Some(s.path), problem: Some(why), ..FileView::default() },
                None => FileView { state: "shared", path: Some(s.path), ..FileView::default() },
            };
        }
        let dl = lock(&self.state).downloads.get(&h).map(|d| {
            let view = |state| FileView { state, done: d.done, rate: if state == "downloading" { d.rate } else { 0 }, path: None, problem: d.rec.problem.clone() };
            match d.rec.state {
                DlState::Done => Err((d.rec.to.clone(), d.rec.size)),
                DlState::Active if d.running.is_some() => Ok(view("downloading")),
                DlState::Active | DlState::Paused => Ok(view("paused")),
                DlState::Gone => Ok(view("gone")),
                DlState::Failed => Ok(view("failed")),
            }
        });
        match dl {
            Some(Ok(v)) => return v,
            Some(Err((to, size))) if to.is_file() => return FileView { state: "downloaded", done: size, path: Some(to), ..FileView::default() },
            Some(Err(_)) => return FileView { state: "available", ..FileView::default() },
            None => {}
        }
        if mine {
            return FileView { state: "unshared", problem: Some("it is no longer shared from this device".into()), ..FileView::default() };
        }
        FileView { state: "available", ..FileView::default() }
    }

    /// Stops a download and deletes its partial file: the file is *available* again. False when there was none.
    /// The partial file is deleted with the state let go, the download held [`Download::busy`] meanwhile, so a new
    /// run to the same place starts only after it is gone (and its own partial file is never deleted). False also
    /// while the download is being finished.
    pub(crate) fn cancel(&self, hash: &[u8; 32]) -> io::Result<bool> {
        let part = {
            let mut st = lock(&self.state);
            let Some(d) = st.downloads.get_mut(hash) else { return Ok(false) };
            if d.rec.state == DlState::Done || d.busy {
                return Ok(false);
            }
            if let Some(stop) = &d.running {
                stop.store(true, Ordering::SeqCst);
            }
            d.busy = true;
            d.rec.part.clone()
        };
        self.test_gap();
        let _ = std::fs::remove_file(part);
        let mut st = lock(&self.state);
        st.downloads.remove(hash);
        self.save(st)?;
        Ok(true)
    }

    /// The room is gone: every download stops and its partial file goes (the folder itself is the caller's).
    pub(crate) fn close(&self) {
        let parts: Vec<PathBuf> = {
            let mut st = lock(&self.state);
            st.closed = true;
            for d in st.downloads.values() {
                if let Some(stop) = &d.running {
                    stop.store(true, Ordering::SeqCst);
                }
            }
            st.downloads.drain().filter(|(_, d)| d.rec.state != DlState::Done).map(|(_, d)| d.rec.part).collect()
        };
        // A save under way ends first, and none comes after: the folder is deleted next.
        lock(&self.saving).closed = true;
        for p in parts {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The service stops: every download stops where it is (and resumes when it starts again).
    pub(crate) fn halt(&self) {
        for d in lock(&self.state).downloads.values() {
            if let Some(stop) = &d.running {
                stop.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Starts showing a send whose files are read: its id, and the counter of bytes read.
    pub(crate) fn preparing(&self, names: Vec<String>, total: u64) -> (u64, Arc<AtomicU64>) {
        let mut st = lock(&self.state);
        st.next_id += 1;
        let (id, done) = (st.next_id, Arc::new(AtomicU64::new(0)));
        st.preparing.push(Preparing { id, names, total, done: done.clone(), problem: None });
        (id, done)
    }

    /// A send was prepared and posted (`problem` none: it goes), or failed and waits to be dismissed.
    pub(crate) fn prepared(&self, id: u64, problem: Option<String>) {
        let mut st = lock(&self.state);
        match problem {
            None => st.preparing.retain(|p| p.id != id),
            Some(why) => {
                if let Some(p) = st.preparing.iter_mut().find(|p| p.id == id) {
                    p.problem = Some(why);
                }
            }
        }
    }

    /// Sends being prepared, oldest first.
    pub(crate) fn preparing_list(&self) -> Vec<PreparingInfo> {
        lock(&self.state)
            .preparing
            .iter()
            .map(|p| PreparingInfo { id: p.id, names: p.names.clone(), done: p.done.load(Ordering::Relaxed).min(p.total), total: p.total, problem: p.problem.clone() })
            .collect()
    }

    /// Forgets a send that failed.
    pub(crate) fn dismiss(&self, id: u64) -> bool {
        let mut st = lock(&self.state);
        let before = st.preparing.len();
        st.preparing.retain(|p| p.id != id || p.problem.is_none());
        st.preparing.len() != before
    }

    /// The first `n` pieces a download lacks.
    fn missing(&self, hash: &[u8; 32], n: usize) -> Vec<u64> {
        let st = lock(&self.state);
        let Some(d) = st.downloads.get(hash) else { return Vec::new() };
        (0..piece_count(d.rec.size)).filter(|i| !d.has(*i)).take(n).collect()
    }

    /// A piece was written and checked: recorded (saved every [`SAVE_EVERY`], after the partial file is flushed: what
    /// is saved names only pieces written before the copy was made).
    fn got(&self, hash: &[u8; 32], i: u64, len: u64, file: &File) {
        let mut st = lock(&self.state);
        let Some(d) = st.downloads.get_mut(hash) else { return };
        if d.has(i) {
            return;
        }
        if let Some(b) = d.have.get_mut((i / 8) as usize) {
            *b |= 1 << (i % 8);
        }
        d.done += len;
        d.window.1 += len;
        let since = d.window.0.elapsed();
        if since >= Duration::from_secs(1) {
            d.rate = (d.window.1 as f64 / since.as_secs_f64()) as u64;
            d.window = (Instant::now(), 0);
        }
        if d.saved.elapsed() < SAVE_EVERY {
            return;
        }
        d.saved = Instant::now();
        let snapshot = self.snapshot(&mut st);
        drop(st);
        let _ = file.sync_data();
        if let Ok(s) = snapshot {
            let _ = self.write(s);
        }
    }
}

fn bitmap_from_hex(s: &str, pieces: u64) -> Vec<u8> {
    let n = pieces.div_ceil(8) as usize;
    let mut out: Vec<u8> = (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
    out.resize(n, 0);
    out
}

/// A free path in `dir` for a file called `name` (already safe): `name`, else `name (1)`, `name (2)`… before its
/// extension (`archive (1).tar.gz`), never one that is there or whose partial file is.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let free = |p: &Path| std::fs::symlink_metadata(p).is_err() && std::fs::symlink_metadata(part_of(p)).is_err();
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    let (stem, ext) = split_ext(name);
    for n in 1..10_000u32 {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if free(&p) {
            return p;
        }
    }
    dir.join(format!("{stem} ({}){ext}", crate::service::random_id().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// `report.pdf` → (`report`, `.pdf`); `a.tar.gz` → (`a`, `.tar.gz`); `README` → (`README`, ``).
fn split_ext(name: &str) -> (&str, &str) {
    let at = match name.rfind('.') {
        Some(0) | None => return (name, ""),
        Some(i) => i,
    };
    let stem = &name[..at];
    match stem.rfind('.') {
        Some(j) if j > 0 && stem[j..].eq_ignore_ascii_case(".tar") => (&name[..j], &name[j..]),
        _ => (stem, &name[at..]),
    }
}

/// The partial file of a download to `to`.
fn part_of(to: &Path) -> PathBuf {
    let mut s = to.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// File names whose opening runs code: apps, installers, scripts and programs, on any system. Opening one asks first.
const CODE: &[&str] = &[
    "app", "command", "tool", "terminal", "sh", "bash", "zsh", "csh", "ksh", "fish", "py", "pyc", "pl", "rb", "php", "js", "mjs", "jar", "scpt", "scptd",
    "applescript", "workflow", "action", "pkg", "mpkg", "dmg", "prefpane", "kext", "plugin", "bundle", "osax", "dylib", "so", "exe", "msi", "msix",
    "appx", "bat", "cmd", "com", "ps1", "psm1", "vbs", "vbe", "wsf", "wsh", "scr", "lnk", "reg", "cpl", "dll", "hta", "appimage", "deb", "rpm", "run",
    "bin", "desktop", "inetloc", "webloc", "fileloc", "url",
];

/// Whether opening this file runs code (an app or a script, by its name; a folder such as an app bundle; or a file
/// marked executable): the host then asks first.
pub fn opens_as_code(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if CODE.contains(&ext.as_str()) {
        return true;
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => true,
        Ok(m) => trs_platform::fs::unix_mode(&m).is_some_and(|mode| mode & 0o111 != 0),
        Err(_) => false,
    }
}

/// A member asks for a file's piece table or a piece of it (`w.hash` names the table or the file).
pub(crate) fn serve(ctx: &Ctx, room: &dyn Room, w: &BlobWant) -> BlobData {
    let no = |code| BlobData { id: w.id, code, total: 0, data: Vec::new() };
    let files = room.files();
    // Whose answer is "gone" (the sender's), or "missing" (a copy that changed: someone else may have it).
    let over = |s: &Share| if s.own { BlobCode::Gone } else { BlobCode::Missing };
    let end = |hash: &[u8; 32], why: &str| {
        let mut st = lock(&files.state);
        match st.shares.get(hash).map(|s| s.own) {
            Some(true) => {
                if let Some(s) = st.shares.get_mut(hash) {
                    s.gone.get_or_insert_with(|| why.to_string());
                }
            }
            Some(false) => {
                st.shares.remove(hash);
            }
            None => {}
        }
        let _ = files.save(st);
    };
    let (n, table_asked) = match file_of_table(ctx, room, &w.hash) {
        Some(n) => (n, true),
        None => match named(ctx, room, &w.hash) {
            Some(n) if n.r.is_file() => (n, false),
            _ => return no(BlobCode::Refused),
        },
    };
    let Some(share) = lock(&files.state).shares.get(&n.r.hash).cloned() else { return no(BlobCode::Missing) };
    if share.gone.is_some() || hash_from_hex(&share.table) != Some(n.r.pieces) || share.size != n.r.size {
        return no(over(&share));
    }
    if !share.matches() {
        end(&n.r.hash, if share.path.exists() { "the file changed since it was sent" } else { "the file was moved or deleted" });
        return no(over(&share));
    }
    let Some(table) = files.table(&n.r.pieces) else { return no(BlobCode::Missing) };
    if table_asked {
        let total = table.len() as u64;
        if w.offset > total {
            return no(BlobCode::Refused);
        }
        let start = w.offset as usize;
        let end = (start + w.len as usize).min(table.len());
        return BlobData { id: w.id, code: BlobCode::Ok, total, data: table[start..end].to_vec() };
    }
    let size = n.r.size;
    let i = w.offset / PIECE;
    if !w.offset.is_multiple_of(PIECE) || i >= piece_count(size) || u64::from(w.len) != piece_span(size, i).1 {
        return no(BlobCode::Refused);
    }
    let mut data = vec![0u8; w.len as usize];
    let read = File::open(&share.path).and_then(|f| trs_platform::fs::read_exact_at(&f, &mut data, w.offset));
    if read.is_err() {
        end(&n.r.hash, "the file can no longer be read");
        return no(over(&share));
    }
    let want = table.get((i * 32) as usize..(i * 32 + 32) as usize);
    if want != Some(&sha256(&data)[..]) {
        end(&n.r.hash, "the file changed since it was sent");
        return no(over(&share));
    }
    let hooks = &ctx.file_hooks;
    let delay = hooks.delay_ms.load(Ordering::Relaxed);
    if delay > 0 {
        std::thread::sleep(Duration::from_millis(delay));
    }
    if hooks.corrupt.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
        && let Some(b) = data.first_mut()
    {
        *b ^= 0xff;
    }
    hooks.served.fetch_add(1, Ordering::Relaxed);
    BlobData { id: w.id, code: BlobCode::Ok, total: size, data }
}

/// Starts (or resumes, or retries) downloading a file of a room to `to`.
pub(crate) fn download(ctx: &Arc<Ctx>, room: &Arc<dyn Room>, n: &Named, to: DownloadTo) -> Result<(), String> {
    let files = room.files();
    let h = n.r.hash;
    if table_len(n.r.size) > MAX_TABLE {
        return Err("the file is larger than this version can download (8 TiB)".into());
    }
    {
        let mut st = lock(&files.state);
        if st.closed {
            return Err("the conversation is gone".into());
        }
        if st.shares.get(&h).is_some_and(|s| s.own) {
            return Err(concat!("this file was sent from this ", trs_platform::computer!()).into());
        }
        match st.downloads.get_mut(&h) {
            // Being cancelled or finished: what it ends as shows on its card.
            Some(d) if d.busy => return Ok(()),
            Some(d) if d.rec.state == DlState::Done && d.rec.to.is_file() => return Ok(()),
            Some(d) if d.rec.state != DlState::Done => {
                // Paused, gone or failed: tried again from where it was.
                d.rec.state = DlState::Active;
                d.rec.problem = None;
                let _ = files.save(st);
                resume(ctx, room);
                return Ok(());
            }
            Some(_) => {
                st.downloads.remove(&h);
            }
            None => {}
        }
    }
    let name = safe_name(&n.r.name);
    let (to, replace) = match to {
        DownloadTo::Folder(dir) => {
            std::fs::create_dir_all(&dir).map_err(|e| format!("the folder {} cannot be made: {e}", trs_platform::dirs::shown(&dir)))?;
            (unique_path(&dir, &name), false)
        }
        DownloadTo::File(p) => {
            let dir = p.parent().filter(|d| d.is_dir()).ok_or_else(|| format!("there is no folder {}", p.parent().map(trs_platform::dirs::shown).unwrap_or_default()))?;
            let file = p.file_name().map(|f| safe_name(&f.to_string_lossy())).ok_or("not a file name")?;
            (dir.join(file), true)
        }
    };
    let part = part_of(&to);
    let dir = to.parent().unwrap_or(Path::new("."));
    if let Ok(free) = trs_platform::fs::available_space(dir)
        && free < n.r.size
    {
        return Err(format!("there is not enough space: it needs {}, {} is free", size_text(n.r.size), size_text(free)));
    }
    let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&part).map_err(|e| format!("{} cannot be written: {e}", trs_platform::dirs::shown(&part)))?;
    drop(f);
    let now = Instant::now();
    let rec = DlRecord { hash: hex(&h), table: hex(&n.r.pieces), name, size: n.r.size, to, replace, part, have: String::new(), state: DlState::Active, problem: None };
    {
        let mut st = lock(&files.state);
        let have = vec![0u8; piece_count(n.r.size).div_ceil(8) as usize];
        st.downloads.insert(h, Download { rec, have, done: 0, running: None, rate: 0, window: (now, 0), saved: now, busy: false });
        files.save(st).map_err(|e| e.to_string())?;
    }
    resume(ctx, room);
    Ok(())
}

/// A size for people (the app's words).
fn size_text(n: u64) -> String {
    trs_media::size_text(n)
}

/// Starts the room's downloads that should run and do not (a member connected, the service started, one was asked).
pub(crate) fn resume(ctx: &Arc<Ctx>, room: &Arc<dyn Room>) {
    let mut starts = Vec::new();
    {
        let mut st = lock(&room.files().state);
        if st.closed {
            return;
        }
        for (h, d) in st.downloads.iter_mut() {
            if matches!(d.rec.state, DlState::Active | DlState::Paused) && d.running.is_none() {
                let stop = Arc::new(AtomicBool::new(false));
                d.running = Some(stop.clone());
                d.rec.state = DlState::Active;
                d.window = (Instant::now(), 0);
                starts.push((*h, stop));
            }
        }
    }
    for (h, stop) in starts {
        let (ctx, room) = (ctx.clone(), room.clone());
        tokio::spawn(async move {
            let end = pull(&ctx, &room, h, &stop).await;
            finish(room.files(), h, &stop, end);
        });
    }
}

/// How a run of a download ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Done,
    Stopped,
    /// Nobody connected has it now.
    Paused,
    /// Its sender said it is gone.
    Gone,
    Failed(String),
}

/// How a peer's part of a run ended.
enum PeerEnd {
    Done,
    Stopped,
    Gone,
    Away,
    Failed(String),
}

async fn pull(ctx: &Arc<Ctx>, room: &Arc<dyn Room>, h: [u8; 32], stop: &AtomicBool) -> End {
    let Some(n) = named(ctx, &**room, &h) else { return End::Failed("its message is no longer here".into()) };
    let files = room.files();
    let Some(part) = lock(&files.state).downloads.get(&h).map(|d| d.rec.part.clone()) else { return End::Stopped };
    let file = match std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&part) {
        Ok(f) => f,
        Err(e) => return End::Failed(format!("{} cannot be written: {e}", trs_platform::dirs::shown(&part))),
    };
    if file.metadata().map(|m| m.len()).unwrap_or(0) != n.r.size
        && let Err(e) = file.set_len(n.r.size)
    {
        return End::Failed(format!("{} cannot be written: {e}", trs_platform::dirs::shown(&part)));
    }
    if n.r.size == 0 && sha256(b"") == n.r.pieces {
        return match files.put_table(&n.r.pieces, b"") {
            Ok(()) => End::Done,
            Err(e) => End::Failed(format!("the download could not be recorded: {e}")),
        };
    }
    let mut peers = room.peers(ctx);
    // The sender first: it has the file unless it is gone.
    peers.sort_by_key(|p| p.device_pk != n.author);
    let mut gone = false;
    for p in peers {
        if stop.load(Ordering::SeqCst) {
            return End::Stopped;
        }
        let is_author = p.device_pk == n.author;
        let table = match files.table(&n.r.pieces) {
            Some(t) => t,
            None => match crate::blobs::fetch_coded(&p, &**room, &n.r.pieces, table_len(n.r.size)).await {
                Ok(t) if sha256(&t) == n.r.pieces => {
                    if let Err(e) = files.put_table(&n.r.pieces, &t) {
                        return End::Failed(format!("the download could not be recorded: {e}"));
                    }
                    Arc::new(t)
                }
                Err(Some(BlobCode::Gone)) => {
                    gone |= is_author;
                    continue;
                }
                _ => continue,
            },
        };
        match pieces(&p, &**room, &n, &table, &file, stop).await {
            PeerEnd::Done => return End::Done,
            PeerEnd::Stopped => return End::Stopped,
            PeerEnd::Gone => gone |= is_author,
            PeerEnd::Away => {}
            PeerEnd::Failed(e) => return End::Failed(e),
        }
    }
    if files.missing(&h, 1).is_empty() && files.table(&n.r.pieces).is_some() {
        return End::Done;
    }
    if gone { End::Gone } else { End::Paused }
}

/// The pieces the download lacks, from one peer, [`WINDOW`] at a time.
async fn pieces(p: &PeerHandle, room: &dyn Room, n: &Named, table: &[u8], file: &File, stop: &AtomicBool) -> PeerEnd {
    let (h, size) = (n.r.hash, n.r.size);
    let files = room.files();
    let mut bad = 0u32;
    loop {
        if stop.load(Ordering::SeqCst) {
            return PeerEnd::Stopped;
        }
        let batch = files.missing(&h, WINDOW);
        if batch.is_empty() {
            return PeerEnd::Done;
        }
        let mut asks = Vec::with_capacity(batch.len());
        for i in batch {
            let (offset, len) = piece_span(size, i);
            match p.ask_blob(BlobWant { id: 0, scope: room.scope(), hash: h, offset, len: len as u32 }).await {
                Some((id, rx)) => asks.push((i, id, rx)),
                None => break,
            }
        }
        if asks.is_empty() {
            return PeerEnd::Away;
        }
        let mut asks = asks.into_iter();
        while let Some((i, id, rx)) = asks.next() {
            let forget_rest = |rest: std::vec::IntoIter<(u64, u32, tokio::sync::oneshot::Receiver<BlobData>)>| {
                for (_, id, _) in rest {
                    p.forget_blob(id);
                }
            };
            let d = match tokio::time::timeout(PIECE_TIME, rx).await {
                Ok(Ok(d)) => d,
                _ => {
                    p.forget_blob(id);
                    forget_rest(asks);
                    return PeerEnd::Away;
                }
            };
            match d.code {
                BlobCode::Ok => {}
                BlobCode::Gone => {
                    forget_rest(asks);
                    return PeerEnd::Gone;
                }
                _ => {
                    forget_rest(asks);
                    return PeerEnd::Away;
                }
            }
            let (offset, len) = piece_span(size, i);
            let want = table.get((i * 32) as usize..(i * 32 + 32) as usize);
            if d.total != size || d.data.len() as u64 != len || want != Some(&sha256(&d.data)[..]) {
                // Asked again (from this peer while it has sent fewer than three bad pieces).
                bad += 1;
                if bad >= BAD_PIECES {
                    forget_rest(asks);
                    return PeerEnd::Away;
                }
                continue;
            }
            if let Err(e) = trs_platform::fs::write_all_at(file, &d.data, offset) {
                forget_rest(asks);
                return PeerEnd::Failed(format!("the download could not be written: {e}"));
            }
            files.got(&h, i, len, file);
        }
    }
}

/// Records how a run ended; a finished download gets its name, the system's mark, and is served from here. The disk
/// work (the file put in place, a partial file deleted) is done before the state is locked to record it: the app's
/// calls read it.
fn finish(files: &Files, h: [u8; 32], stop: &Arc<AtomicBool>, end: End) {
    let ours = |st: &State| st.downloads.get(&h).filter(|d| !d.busy && d.running.as_ref().is_some_and(|r| Arc::ptr_eq(r, stop))).map(|d| d.rec.clone());
    // Held busy while its partial file is put in place or deleted: no cancel, and so no new run to the same place
    // (whose partial file has the same path), comes in between.
    let rec = {
        let mut st = lock(&files.state);
        let Some(rec) = ours(&st) else { return };
        let touches = matches!(end, End::Done | End::Gone);
        if let Some(d) = st.downloads.get_mut(&h) {
            d.busy = touches;
        }
        rec
    };
    files.test_gap();
    let placed = match &end {
        End::Done => Some(place(&rec).map(|to| {
            let stamped = stamp(&to);
            (to, stamped)
        })),
        End::Gone => {
            let _ = std::fs::remove_file(&rec.part);
            None
        }
        _ => None,
    };
    let mut st = lock(&files.state);
    // The room closed meanwhile (its downloads are gone).
    let Some(d) = st.downloads.get_mut(&h).filter(|d| d.running.as_ref().is_some_and(|r| Arc::ptr_eq(r, stop))) else { return };
    d.busy = false;
    d.running = None;
    d.rate = 0;
    match (end, placed) {
        (End::Stopped, _) | (End::Done, None) => {}
        (End::Paused, _) => d.rec.state = DlState::Paused,
        (End::Gone, _) => {
            d.rec.state = DlState::Gone;
            d.rec.problem = None;
            d.have.iter_mut().for_each(|b| *b = 0);
            d.done = 0;
        }
        (End::Failed(why), _) => {
            d.rec.state = DlState::Failed;
            d.rec.problem = Some(why);
        }
        (End::Done, Some(Ok((to, stamped)))) => {
            d.rec.to = to.clone();
            d.rec.state = DlState::Done;
            d.done = d.rec.size;
            let table = d.rec.table.clone();
            if let Ok((size, mtime)) = stamped {
                st.shares.insert(h, Share { hash: hex(&h), table, path: to, size, mtime_s: mtime.0, mtime_ns: mtime.1, own: false, gone: None });
            }
        }
        (End::Done, Some(Err(e))) => {
            d.rec.state = DlState::Failed;
            d.rec.problem = Some(format!("the finished file could not be put in place: {e}"));
        }
    }
    let _ = files.save(st);
}

/// Puts a finished partial file in place: marked as downloaded, under its name (a free one like it when another file
/// took that name meanwhile, unless the user chose to replace it).
fn place(rec: &DlRecord) -> io::Result<PathBuf> {
    if let Ok(f) = File::open(&rec.part) {
        let _ = f.sync_all();
    }
    let _ = trs_platform::fs::mark_downloaded(&rec.part);
    if rec.replace {
        std::fs::rename(&rec.part, &rec.to)?;
        return Ok(rec.to.clone());
    }
    let mut to = rec.to.clone();
    for _ in 0..8 {
        match trs_platform::fs::rename_no_replace(&rec.part, &to) {
            Ok(()) => return Ok(to),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let dir = to.parent().unwrap_or(Path::new(".")).to_path_buf();
                to = unique_path(&dir, &rec.name);
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_in(dir: &Path) -> Arc<Files> {
        Arc::new(Files::open(dir.join("x.files"), Sealer::new([3; 32], AAD)))
    }

    /// A download of `h` to `to` (Save As), run by the flag returned, its partial file holding `what`.
    fn running(files: &Files, h: [u8; 32], to: &Path, what: &[u8]) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let part = part_of(to);
        std::fs::write(&part, what).unwrap();
        let rec = DlRecord {
            hash: hex(&h),
            table: hex(&[0; 32]),
            name: "report.pdf".into(),
            size: what.len() as u64,
            to: to.to_path_buf(),
            replace: true,
            part,
            have: String::new(),
            state: DlState::Active,
            problem: None,
        };
        let now = Instant::now();
        lock(&files.state).downloads.insert(h, Download { rec, have: vec![0], done: 0, running: Some(stop.clone()), rate: 0, window: (now, 0), saved: now, busy: false });
        stop
    }

    /// The user asks for the file again, to the same place: a new run with a new partial file, when `download` would
    /// start one (no download of it is there; one that is there is tried again, or left alone while it is busy).
    fn ask_again(files: &Files, h: [u8; 32], to: &Path) -> bool {
        if lock(&files.state).downloads.contains_key(&h) {
            return false;
        }
        running(files, h, to, b"new run");
        true
    }

    /// The failure: a run ends (the file is done, or its sender said it is gone) and, before its partial file is put
    /// in place or deleted, the user cancels and asks for the file again to the same place (the same partial file):
    /// the old run's end deletes the new run's partial file, or puts it in place as the finished file.
    #[test]
    fn the_end_of_a_run_never_touches_the_partial_file_of_a_new_run_to_the_same_place() {
        for end in [End::Gone, End::Done] {
            let dir = tempfile::tempdir().unwrap();
            let files = files_in(dir.path());
            let (h, to) = ([5; 32], dir.path().join("report.pdf"));
            let stop = running(&files, h, &to, b"old run");
            let again = Arc::new(AtomicBool::new(false));
            let (f, t, a) = (files.clone(), to.clone(), again.clone());
            *lock(&files.gap) = Some(Box::new(move || {
                if f.cancel(&h).unwrap() && ask_again(&f, h, &t) {
                    a.store(true, Ordering::SeqCst);
                }
            }));
            let gone = end == End::Gone;
            finish(&files, h, &stop, end);
            if again.load(Ordering::SeqCst) {
                assert_eq!(std::fs::read(part_of(&to)).ok().as_deref(), Some(&b"new run"[..]), "the new run's partial file is kept");
            } else if gone {
                assert!(!part_of(&to).exists(), "the old run's partial file is deleted");
            } else {
                assert_eq!(std::fs::read(&to).unwrap(), b"old run", "the old run's file is put in place");
            }
        }
    }

    /// The failure: the user cancels a download (its partial file already deleted by hand) and asks for it again to
    /// the same place before the cancel deleted the partial file: the cancel deletes the new run's.
    #[test]
    fn a_cancel_never_deletes_the_partial_file_of_a_new_run_to_the_same_place() {
        let dir = tempfile::tempdir().unwrap();
        let files = files_in(dir.path());
        let (h, to) = ([6; 32], dir.path().join("report.pdf"));
        running(&files, h, &to, b"old run");
        std::fs::remove_file(part_of(&to)).unwrap();
        let (f, t) = (files.clone(), to.clone());
        *lock(&files.gap) = Some(Box::new(move || {
            ask_again(&f, h, &t);
        }));
        assert!(files.cancel(&h).unwrap());
        if lock(&files.state).downloads.contains_key(&h) {
            assert_eq!(std::fs::read(part_of(&to)).ok().as_deref(), Some(&b"new run"[..]), "the new run's partial file is kept");
        }
        // Once the cancel is done, the file can be asked for again.
        assert!(ask_again(&files, h, &to) || lock(&files.state).downloads.contains_key(&h));
        assert_eq!(std::fs::read(part_of(&to)).unwrap(), b"new run");
    }

    #[test]
    fn a_file_is_named_by_its_hash_and_piece_table() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data.bin");
        let bytes: Vec<u8> = (0..(2 * PIECE + 10)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        let done = AtomicU64::new(0);
        let r = prepare(&p, &done, &AtomicBool::new(false)).unwrap();
        assert_eq!((r.size, r.hash, done.load(Ordering::Relaxed)), (bytes.len() as u64, sha256(&bytes), bytes.len() as u64));
        assert_eq!(r.table.len(), 96);
        assert_eq!(&r.table[32..64], &sha256(&bytes[PIECE as usize..2 * PIECE as usize])[..]);
        assert_eq!(r.table_hash, sha256(&r.table));
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        let e = prepare(&empty, &done, &AtomicBool::new(false)).unwrap();
        assert_eq!((e.size, e.table.len(), e.hash), (0, 0, sha256(b"")));
        assert!(prepare(dir.path(), &done, &AtomicBool::new(false)).is_err(), "a folder is not a file");
        assert!(prepare(&p, &done, &AtomicBool::new(true)).is_err(), "stopped");
    }

    #[test]
    fn a_free_name_is_found_beside_the_taken_ones() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(unique_path(d, "report.pdf"), d.join("report.pdf"));
        std::fs::write(d.join("report.pdf"), b"").unwrap();
        assert_eq!(unique_path(d, "report.pdf"), d.join("report (1).pdf"));
        std::fs::write(d.join("report (1).pdf.seedwell-part"), b"").unwrap();
        assert_eq!(unique_path(d, "report.pdf"), d.join("report (2).pdf"), "a partial file holds its name");
        std::fs::write(d.join("a.tar.gz"), b"").unwrap();
        assert_eq!(unique_path(d, "a.tar.gz"), d.join("a (1).tar.gz"));
        std::fs::write(d.join("README"), b"").unwrap();
        assert_eq!(unique_path(d, "README"), d.join("README (1)"));
        assert_eq!(split_ext(".profile"), (".profile", ""));
    }

    #[test]
    fn apps_and_scripts_are_code_and_documents_are_not() {
        let dir = tempfile::tempdir().unwrap();
        for (name, code) in [("Setup.app", true), ("run.sh", true), ("x.EXE", true), ("tool.command", true), ("report.pdf", false), ("photo.jpg", false)] {
            let p = dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            assert_eq!(opens_as_code(&p), code, "{name}");
        }
        let bundle = dir.path().join("Thing");
        std::fs::create_dir(&bundle).unwrap();
        assert!(opens_as_code(&bundle), "a folder (a bundle) asks first");
        #[cfg(unix)]
        {
            let p = dir.path().join("plain");
            std::fs::write(&p, b"x").unwrap();
            assert!(!opens_as_code(&p));
            trs_platform::fs::set_exec(&p, true).unwrap();
            assert!(opens_as_code(&p), "marked executable");
        }
    }
}
