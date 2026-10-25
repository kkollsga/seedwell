//! The randomized simulator (Dropbox's CanopyCheck/Trinity idea): 2–4 devices, each a [`FolderCore`]
//! on its own [`MemFs`] and index store, driven from one seed through random user edits, scans, index
//! exchange over a network that partitions and heals, pulls, crashes, unmounts and injected I/O
//! failures. Invariants are checked after every step and at the end.
//!
//! * `SYNC_SIM_SEEDS=5000` runs more seeds (default 300, spread over the shard tests), `SYNC_SIM_STEPS`
//!   longer runs (default 200).
//! * A failure prints the seed and the action log. Replay it exactly with
//!   `SYNC_SIM_SEED=<seed> cargo nextest run -p trs-sync -E 'test(=sim::replay)' --no-capture`
//!   (plus `SYNC_SIM_MODE=<mode>` for the mode tests, `SYNC_SIM_TRACE=<path>` to log every device's
//!   view of one path after each step).
//! * Modes (each its own test): `race-after-rename`, `case-insensitive`, `resets`, `kinds`,
//!   `lossy-disk`, and `strict-history`, ignored for one known gap (see `Cfg`).
//!
//! # What "no data loss" means here
//!
//! The simulator models the *user's intent*, not the engine's versions. A file content becomes **known**
//! once a scan recorded it (the device's record matches the disk stamp right after a scan). It is
//! **discharged** only when a user, on a writer device, overwrites, appends to or deletes a file with
//! that content: the user saw it and replaced it, so later versions legitimately supersede it
//! everywhere. (A user's copy of a file makes its content wanted again.) (A read-only device's edits never propagate, so they discharge only its
//! own local contents.) Then:
//!
//! * **(2a) always:** every known content that is not discharged exists on some device, as a file in
//!   the tree, a conflict copy or a file under `.seedwell/Archive/`;
//! * **(2b) at the end:** every such content a writer made is still a *live* file in the converged tree.
//!   A version replacing it could only have come from someone who never saw it, so it must have become
//!   a conflict copy, not just an archived version. (Archived-only is what "a remote version replaced
//!   local content" looks like.)
//!
//! Crashes are the realistic case for APFS: the index store's commits are durable (redb fsyncs), the
//! disk keeps every completed operation, and the crash lands between any two operations (disk or
//! store) of a scan, pull or index delivery. A crash loses the in-memory state (a full index send in
//! progress) and every connection of the device.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use trs_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
use trs_sync::fs::{FileHandle, Fs, Listing, MemFs, MemFsOptions, Meta, read_all, write_all};
use trs_sync::ignore::Ignore;
use trs_sync::index::{IndexStore, LocalRecord, MemStore, Meta as IndexMeta, NeedRecord, Op, PeerState, offsets};
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError};
use trs_sync::scan::ScanError;
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::keys::{Access, FolderKey, FolderSecret};
use trs_sync_proto::model::{FileKind, IndexEntry, STATE_DIR};
use trs_sync_proto::sign::Writer;

const START_NS: i64 = 1_767_225_600_000_000_000;

// ---------------------------------------------------------------------------------------------
// Randomness

/// splitmix64: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x5EED_5EED)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64) / ((1u64 << 53) as f64) < p
    }
    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() { None } else { Some(v[self.below(v.len())].clone()) }
    }
    fn weighted(&mut self, w: &[u32]) -> usize {
        let total: u32 = w.iter().sum();
        let mut x = (self.next() % u64::from(total)) as u32;
        for (i, &wi) in w.iter().enumerate() {
            if x < wi {
                return i;
            }
            x -= wi;
        }
        w.len() - 1
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

/// A content fingerprint (not cryptographic; collisions between random contents are negligible).
fn h64(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ (data.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for c in data.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(0x0100_0000_01b3);
        h ^= h >> 29;
    }
    h
}

// ---------------------------------------------------------------------------------------------
// The device's view of its disk and store: crash points, unmounts, races, and what it touched.

/// Shared by a device's filesystem wrapper and store wrapper.
#[derive(Default)]
struct CtlState {
    /// Operations (disk or store) left before the crash; `None` = no crash armed.
    crash_in: Option<u32>,
    /// The process is dead: every operation fails until the reboot.
    crashed: bool,
    /// Things the core did that it must never do.
    violations: Vec<String>,
    /// A user edit to make in the middle of the puller's work.
    race: Option<Race>,
    /// The racing edits that happened.
    raced: Vec<RaceEvent>,
}

type Ctl = Arc<Mutex<CtlState>>;

/// When to edit: before `op` on a temp file (`before`), or right after it.
struct Race {
    before: bool,
    op: &'static str,
    data: Vec<u8>,
    atomic: bool,
}

struct RaceEvent {
    path: String,
    old: Option<Vec<u8>>,
}

fn tick(ctl: &Ctl, what: &str) -> io::Result<()> {
    let mut c = ctl.lock().unwrap();
    if c.crashed {
        return Err(io::Error::other(format!("crashed (at {what})")));
    }
    if let Some(n) = c.crash_in {
        if n == 0 {
            c.crashed = true;
            c.crash_in = None;
            return Err(io::Error::other(format!("crash (at {what})")));
        }
        c.crash_in = Some(n - 1);
    }
    Ok(())
}

fn is_tmp_name(name: &str) -> bool {
    name.starts_with(".seedwell.") && name.ends_with(".tmp")
}

fn last(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn parent_of(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// The path a puller temp file is for.
fn tmp_target(tmp: &str) -> Option<String> {
    let n = last(tmp);
    let inner = n.strip_prefix(".seedwell.")?.strip_suffix(".tmp")?;
    Some(join(parent_of(tmp), inner))
}

const MUTATING: &[&str] =
    &["open_rw", "create_dir", "remove_dir", "remove_file", "rename", "hard_link", "copy", "set_mtime", "set_exec", "symlink"];

/// The filesystem the core sees.
#[derive(Clone)]
struct SimFs {
    real: MemFs,
    /// `Some`: the folder's disk is unmounted, and this (empty) mount point is what is there.
    mount: Arc<Mutex<Option<MemFs>>>,
    ctl: Ctl,
}

impl SimFs {
    fn cur(&self) -> MemFs {
        self.mount.lock().unwrap().clone().unwrap_or_else(|| self.real.clone())
    }

    fn unmounted(&self) -> bool {
        self.mount.lock().unwrap().is_some()
    }

    /// Counts the operation towards a crash and records what it must not do.
    fn check(&self, op: &'static str, path: &str, dest: bool) -> io::Result<()> {
        tick(&self.ctl, op)?;
        let mut v = Vec::new();
        if MUTATING.contains(&op) && self.unmounted() {
            v.push(format!("{op} {path:?} while the folder is unmounted"));
        }
        if path == STATE_DIR {
            if op != "lstat" && op != "sync_dir" {
                v.push(format!("{op} on the folder marker itself"));
            }
        } else if let Some(rest) = path.strip_prefix(".seedwell/") {
            let archive = rest == "Archive" || rest.starts_with("Archive/");
            let ok = archive
                && (op == "lstat" || op == "create_dir" || op == "sync_dir" || (dest && (op == "rename" || op == "hard_link")));
            if !ok {
                v.push(format!("{op} {path:?} inside .seedwell"));
            }
        }
        if !v.is_empty() {
            self.ctl.lock().unwrap().violations.extend(v);
        }
        Ok(())
    }

    /// Makes the planned racing user edit if this is its moment.
    fn race(&self, before: bool, op: &str, tmp: &str, target: Option<&str>) {
        if !is_tmp_name(last(tmp)) {
            return;
        }
        let r = {
            let mut c = self.ctl.lock().unwrap();
            match &c.race {
                Some(r) if r.before == before && r.op == op => c.race.take(),
                _ => None,
            }
        };
        let Some(r) = r else { return };
        let Some(target) = target.map(str::to_string).or_else(|| tmp_target(tmp)) else { return };
        if let Ok(old) = user_write(&self.real, &target, &r.data, r.atomic) {
            self.ctl.lock().unwrap().raced.push(RaceEvent { path: target, old });
        }
    }
}

struct SimHandle {
    inner: Box<dyn FileHandle>,
    ctl: Ctl,
}

impl FileHandle for SimHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        tick(&self.ctl, "read_at")?;
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        tick(&self.ctl, "write_at")?;
        self.inner.write_at(offset, buf)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        tick(&self.ctl, "set_len")?;
        self.inner.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        tick(&self.ctl, "len")?;
        self.inner.len()
    }
    fn sync_all(&self) -> io::Result<()> {
        tick(&self.ctl, "sync_all")?;
        self.inner.sync_all()
    }
}

impl Fs for SimFs {
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        self.check("lstat", rel, false)?;
        self.cur().lstat(rel)
    }
    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        self.check("read_dir", rel, false)?;
        self.cur().read_dir(rel)
    }
    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.check("open_read", rel, false)?;
        Ok(Box::new(SimHandle { inner: self.cur().open_read(rel)?, ctl: self.ctl.clone() }))
    }
    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.check("open_rw", rel, false)?;
        self.race(true, "open_rw", rel, None);
        Ok(Box::new(SimHandle { inner: self.cur().open_rw(rel)?, ctl: self.ctl.clone() }))
    }
    fn create_dir(&self, rel: &str) -> io::Result<()> {
        self.check("create_dir", rel, false)?;
        self.cur().create_dir(rel)
    }
    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        self.check("remove_dir", rel, false)?;
        self.cur().remove_dir(rel)
    }
    fn remove_file(&self, rel: &str) -> io::Result<()> {
        self.check("remove_file", rel, false)?;
        self.cur().remove_file(rel)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.check("rename", from, false)?;
        self.check("rename", to, true)?;
        self.race(true, "rename", from, Some(to));
        self.cur().rename(from, to)?;
        self.race(false, "rename", from, Some(to));
        Ok(())
    }
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.check("hard_link", from, false)?;
        self.check("hard_link", to, true)?;
        self.cur().hard_link(from, to)
    }
    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        self.check("copy", from, false)?;
        self.check("copy", to, false)?;
        self.cur().copy(from, to)
    }
    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        self.check("set_mtime", rel, false)?;
        self.cur().set_mtime(rel, mtime_ns)
    }
    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        self.check("set_exec", rel, false)?;
        self.race(true, "set_exec", rel, None);
        self.cur().set_exec(rel, exec)
    }
    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        self.check("symlink", rel, false)?;
        self.cur().symlink(target, rel)
    }
    fn read_link(&self, rel: &str) -> io::Result<String> {
        self.check("read_link", rel, false)?;
        self.cur().read_link(rel)
    }
    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        self.check("sync_dir", rel, false)?;
        self.cur().sync_dir(rel)
    }
}

/// The index store: a [`MemStore`] whose commits are durable the moment `apply` returns (redb), and
/// which is dead while the device is crashed.
struct SimStore {
    inner: Arc<Mutex<MemStore>>,
    ctl: Ctl,
}

impl SimStore {
    fn alive(&self) -> io::Result<()> {
        if self.ctl.lock().unwrap().crashed { Err(io::Error::other("crashed (store)")) } else { Ok(()) }
    }
    fn s(&self) -> std::sync::MutexGuard<'_, MemStore> {
        self.inner.lock().unwrap()
    }
}

impl IndexStore for SimStore {
    fn meta(&self) -> io::Result<IndexMeta> {
        self.alive()?;
        self.s().meta()
    }
    fn local(&self, path: &str) -> io::Result<Option<LocalRecord>> {
        self.alive()?;
        self.s().local(path)
    }
    fn local_since(&self, after: u64, limit: usize) -> io::Result<Vec<LocalRecord>> {
        self.alive()?;
        self.s().local_since(after, limit)
    }
    fn local_paths(&self, prefix: &str, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        self.alive()?;
        self.s().local_paths(prefix, after, limit)
    }
    fn remote(&self, path: &str) -> io::Result<Vec<(ShortId, IndexEntry)>> {
        self.alive()?;
        self.s().remote(path)
    }
    fn remote_paths(&self, device: ShortId, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        self.alive()?;
        self.s().remote_paths(device, after, limit)
    }
    fn peer(&self, device: ShortId) -> io::Result<Option<PeerState>> {
        self.alive()?;
        self.s().peer(device)
    }
    fn writers(&self) -> io::Result<Vec<(ShortId, Writer)>> {
        self.alive()?;
        self.s().writers()
    }
    fn need(&self, path: &str) -> io::Result<Option<NeedRecord>> {
        self.alive()?;
        self.s().need(path)
    }
    fn needs(&self, after: Option<&str>, limit: usize) -> io::Result<Vec<(String, NeedRecord)>> {
        self.alive()?;
        self.s().needs(after, limit)
    }
    fn need_totals(&self) -> io::Result<(u64, u64)> {
        self.alive()?;
        self.s().need_totals()
    }
    fn local_totals(&self) -> io::Result<(u64, u64)> {
        self.alive()?;
        self.s().local_totals()
    }
    fn chunk_sources(&self, hash: &[u8; 32], limit: usize) -> io::Result<Vec<(String, u64)>> {
        self.alive()?;
        self.s().chunk_sources(hash, limit)
    }
    fn apply(&mut self, ops: Vec<Op>) -> io::Result<()> {
        tick(&self.ctl, "commit")?;
        self.s().apply(ops)
    }
}

struct SimClock {
    base: Arc<AtomicI64>,
    skew: i64,
}

impl trs_sync::clock::Clock for SimClock {
    fn now_ns(&self) -> i64 {
        self.base.load(Ordering::SeqCst) + self.skew
    }
    fn local_offset_s(&self, _: i64) -> i32 {
        0
    }
}

// ---------------------------------------------------------------------------------------------
// User edits (straight on the disk, never through the core's wrapper)

/// Writes a file like an app does: in place, or atomically (a temp file renamed over it). Returns the
/// content it replaced.
fn user_write(fs: &MemFs, path: &str, data: &[u8], atomic: bool) -> io::Result<Option<Vec<u8>>> {
    let old = match fs.lstat(path)? {
        Some(m) if m.kind == FileKind::File => Some(read_all(fs, path)?),
        Some(_) => return Err(io::Error::other("not a file")),
        None => None,
    };
    if atomic {
        let tmp = join(parent_of(path), &format!(".{}.sv~", last(path)));
        write_all(fs, &tmp, data)?;
        fs.rename(&tmp, path)?;
    } else {
        write_all(fs, path, data)?;
    }
    Ok(old)
}

// ---------------------------------------------------------------------------------------------
// Devices and the network

struct Device {
    name: String,
    writer: bool,
    short: ShortId,
    key: Arc<DeviceKey>,
    cert: Option<[u8; 64]>,
    folder_pk: [u8; 32],
    access: Access,
    real: MemFs,
    fs: SimFs,
    ctl: Ctl,
    store: Arc<Mutex<MemStore>>,
    core: FolderCore,
    clock: Arc<SimClock>,
    ids: Arc<AtomicU64>,
    ci: bool,
}

#[allow(clippy::too_many_arguments)]
fn open_core(
    key: &Arc<DeviceKey>,
    cert: Option<[u8; 64]>,
    folder_pk: [u8; 32],
    access: Access,
    store: &Arc<Mutex<MemStore>>,
    fs: &SimFs,
    ctl: &Ctl,
    clock: &Arc<SimClock>,
    ids: &Arc<AtomicU64>,
    case_insensitive: bool,
) -> FolderCore {
    let cfg = FolderConfig {
        me: key.device_id(),
        folder_pk,
        access,
        case_insensitive,
        byte_exact_names: false,
        archive: true,
    };
    let ids = ids.clone();
    let new_id = Arc::new(move || {
        let n = ids.fetch_add(1, Ordering::SeqCst);
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.to_be_bytes());
        id
    });
    FolderCore::new(
        cfg,
        Box::new(SimStore { inner: store.clone(), ctl: ctl.clone() }),
        Arc::new(fs.clone()),
        cert.map(|c| (key.clone(), c)),
        clock.clone(),
        new_id,
        Ignore::defaults(),
    )
    .expect("open the folder")
}

impl Device {
    fn reopen(&mut self) {
        {
            let mut c = self.ctl.lock().unwrap();
            c.crashed = false;
            c.crash_in = None;
            c.race = None;
        }
        self.core = open_core(
            &self.key,
            self.cert,
            self.folder_pk,
            self.access,
            &self.store,
            &self.fs,
            &self.ctl,
            &self.clock,
            &self.ids,
            self.ci,
        );
    }

    fn crashed(&self) -> bool {
        self.ctl.lock().unwrap().crashed
    }

    fn unmounted(&self) -> bool {
        self.fs.unmounted()
    }
}

/// One index message on a link.
#[derive(Clone)]
struct Msg {
    index_id: u64,
    full: bool,
    done: bool,
    entries: Vec<IndexEntry>,
    writers: Vec<Writer>,
}

/// Serves chunks from the disks of the devices this one can reach now, as a peer answering a request
/// would: it reads its own file at that path and offset.
struct Source {
    peers: Vec<(ShortId, MemFs)>,
}

impl ChunkSource for Source {
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        reqs.iter()
            .map(|r| {
                let mut bad = None;
                for h in &r.holders {
                    let Some((_, fs)) = self.peers.iter().find(|(s, _)| s == h) else { continue };
                    let Ok(fh) = fs.open_read(&r.path) else { continue };
                    let mut buf = vec![0; r.len as usize];
                    if fh.read_at(r.offset, &mut buf).ok() != Some(buf.len()) {
                        continue;
                    }
                    if <[u8; 32]>::from(Sha256::digest(&buf)) == r.hash {
                        return Ok(buf);
                    }
                    bad.get_or_insert(*h);
                }
                match bad {
                    Some(h) => Err(FetchError::Corrupt(h)),
                    None => Err(FetchError::Unavailable),
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The simulation

#[derive(Clone, Debug)]
struct Cfg {
    devices: usize,
    read_only: bool,
    crash: f64,
    faults: f64,
    race: f64,
    race_after_rename: bool,
    /// Fail also on the known gap: the content a delete removed came back in a version concurrent
    /// with the delete that is no change of content (merged or absorbed elsewhere).
    strict_history: bool,
    /// Case-insensitive disks (APFS default), with case-only renames.
    case_insensitive: bool,
    /// Files and directories share names, so a path changes kind.
    kinds: bool,
    /// Devices lose their index database now and then (a new index id; everything is rescanned).
    resets: bool,
    /// A crash also loses what the disk had not made durable: namespace changes (create, rename,
    /// delete) since the last checkpoint, and unsynced data. The index store's commits stay.
    lossy_disk: bool,
    unmount: bool,
    steps: usize,
}

impl Cfg {
    fn from_seed(rng: &mut Rng, steps: usize, mode: &str) -> Cfg {
        let devices = 2 + rng.below(3);
        let mut c = Cfg {
            devices,
            read_only: devices >= 3 && rng.chance(0.5),
            crash: if rng.chance(0.6) { 0.08 } else { 0.0 },
            faults: if rng.chance(0.5) { 0.2 } else { 0.0 },
            race: if rng.chance(0.5) { 0.25 } else { 0.0 },
            race_after_rename: false,
            strict_history: false,
            case_insensitive: false,
            kinds: false,
            resets: false,
            lossy_disk: false,
            unmount: rng.chance(0.3),
            steps,
        };
        // `SYNC_SIM_STRICT=1`: the known gaps fail too, in every mode.
        c.strict_history = std::env::var("SYNC_SIM_STRICT").is_ok();
        match mode {
            "" => {}
            "race-after-rename" => {
                c.race = 0.4;
                c.race_after_rename = true;
            }
            "strict-history" => c.strict_history = true,
            "case-insensitive" => c.case_insensitive = true,
            "kinds" => c.kinds = true,
            "resets" => c.resets = true,
            "lossy-disk" => {
                c.lossy_disk = true;
                c.crash = 0.1;
            }
            "calm" => {
                c.crash = 0.0;
                c.faults = 0.0;
                c.race = 0.0;
                c.unmount = false;
            }
            m => panic!("unknown SYNC_SIM_MODE {m}"),
        }
        c
    }
}

#[derive(Clone, Debug)]
struct Known {
    /// A writer recorded it (else only a read-only device's local change).
    writer: bool,
    discharged: bool,
    first: usize,
    size: usize,
    at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    Dir,
    File { hash: u64, size: u64, exec: bool },
    Link(String),
}

struct Sim {
    rng: Rng,
    cfg: Cfg,
    base: Arc<AtomicI64>,
    devs: Vec<Device>,
    up: Vec<Vec<bool>>,
    q: Vec<Vec<VecDeque<Msg>>>,
    cursor: Vec<Vec<u64>>,
    log: Vec<String>,
    step: usize,
    known: BTreeMap<u64, Known>,
    /// Contents a user on a writer replaced or removed.
    replaced: BTreeSet<u64>,
    /// Every tombstone version any device recorded, per path.
    tombs: BTreeMap<String, Vec<trs_sync_proto::model::Version>>,
    /// Those another device received (announced at least once).
    announced: BTreeSet<(String, Vec<(u64, u64)>)>,
    /// (origin device, origin is a writer, path, content, exec): a user removed that content from that
    /// path; it must not come back there.
    removed: BTreeSet<(usize, bool, String, u64, bool)>,
    /// (device, path, content, exec) a user wrote: the same change made on another device concurrently
    /// legitimately beats a delete.
    written: BTreeSet<(usize, String, u64, bool)>,
    /// Devices whose index was reset (`resets` mode): a delete they made before is forgotten, by design.
    was_reset: BTreeSet<usize>,
    cache: BTreeMap<(usize, u64, i64, i64, u64), u64>,
    /// Faults armed for the current operation (cleared after it).
    faults: Vec<(usize, String)>,
    /// Contents a user action is removing (discharged once it is done).
    pending_discharge: Vec<u64>,
}

type R<T = ()> = Result<T, String>;

fn e<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |err| format!("{what}: {err}")
}

const DIR_NAMES: &[&str] = &["d1", "d2", "d3"];
const ANY_NAMES: &[&str] = &["d1", "d2", "f1", "f2", "g.txt", "F1"];
const FILE_NAMES: &[&str] = &["f1", "f2", "f3", "f4", "g.txt", "h.bin", "F1", "G.txt"];

impl Sim {
    fn new(seed: u64, steps: usize, mode: &str) -> Sim {
        let mut rng = Rng::new(seed);
        let cfg = Cfg::from_seed(&mut rng, steps, mode);
        let base = Arc::new(AtomicI64::new(START_NS));
        let secret: [u8; 32] = rng.bytes(32).try_into().unwrap();
        let rw = FolderKey::ReadWrite(FolderSecret::from_bytes(secret));
        let ro = rw.read_only().unwrap();
        let mut devs = Vec::new();
        for i in 0..cfg.devices {
            let key = if cfg.read_only && i == cfg.devices - 1 { &ro } else { &rw };
            let seed32: [u8; 32] = rng.bytes(32).try_into().unwrap();
            let dk = Arc::new(DeviceKey::from_seed(&seed32));
            // Clocks disagree by up to ±2 s.
            let skew = (rng.below(4_000) as i64 - 2_000) * 1_000_000;
            let b = base.clone();
            let real = MemFs::with_options(MemFsOptions {
                case_insensitive: cfg.case_insensitive,
                normalization_insensitive: false,
                clock: Arc::new(move || b.fetch_add(1_000, Ordering::SeqCst) + skew),
            });
            real.create_dir(STATE_DIR).unwrap();
            real.snapshot();
            let ctl: Ctl = Arc::default();
            let fs = SimFs { real: real.clone(), mount: Arc::default(), ctl: ctl.clone() };
            let store = Arc::new(Mutex::new(MemStore::new(rng.next())));
            let clock = Arc::new(SimClock { base: base.clone(), skew });
            let ids = Arc::new(AtomicU64::new((i as u64 + 1) << 40));
            let cert = key.writer_cert(dk.public_key());
            let core = open_core(
                &dk,
                cert,
                *key.public_key(),
                key.access(),
                &store,
                &fs,
                &ctl,
                &clock,
                &ids,
                cfg.case_insensitive,
            );
            devs.push(Device {
                name: format!("D{i}"),
                writer: key.access() == Access::Write,
                short: core.me(),
                key: dk,
                cert,
                folder_pk: *key.public_key(),
                access: key.access(),
                real,
                fs,
                ctl,
                store,
                core,
                clock,
                ids,
                ci: cfg.case_insensitive,
            });
        }
        let n = devs.len();
        Sim {
            rng,
            cfg,
            base,
            devs,
            up: vec![vec![false; n]; n],
            q: vec![vec![VecDeque::new(); n]; n],
            cursor: vec![vec![0; n]; n],
            log: Vec::new(),
            step: 0,
            known: BTreeMap::new(),
            tombs: BTreeMap::new(),
            announced: BTreeSet::new(),
            replaced: BTreeSet::new(),
            was_reset: BTreeSet::new(),
            written: BTreeSet::new(),
            removed: BTreeSet::new(),
            cache: BTreeMap::new(),
            faults: Vec::new(),
            pending_discharge: Vec::new(),
        }
    }

    fn note(&mut self, s: String) {
        self.log.push(format!("{:4} {s}", self.step));
    }

    // --- the network ------------------------------------------------------------------------

    /// The opening of a connection from `i` to `j`: `j` says what it holds of `i`'s index, `i` sends
    /// what is newer (or everything, for an index `j` does not hold).
    fn join(&mut self, i: usize, j: usize) -> R {
        let ps = self.devs[j].core.peer_state(self.devs[i].short).map_err(e("peer_state"))?;
        let (index_id, seq) = self.devs[i].core.index_state().map_err(e("index_state"))?;
        let writers = self.devs[i].core.writers();
        self.q[i][j].clear();
        if ps.index_id == index_id {
            let entries = self.devs[i].core.announce_since(ps.max_seq, usize::MAX).map_err(e("announce"))?;
            if !entries.is_empty() {
                self.q[i][j].push_back(Msg { index_id, full: false, done: false, entries, writers });
            }
        } else {
            let entries = self.devs[i].core.announce_since(0, usize::MAX).map_err(e("announce"))?;
            let per = 1 + self.rng.below(8);
            let batches: Vec<Vec<IndexEntry>> =
                if entries.is_empty() { vec![Vec::new()] } else { entries.chunks(per).map(<[_]>::to_vec).collect() };
            let nb = batches.len();
            for (k, b) in batches.into_iter().enumerate() {
                self.q[i][j].push_back(Msg { index_id, full: true, done: k + 1 == nb, entries: b, writers: writers.clone() });
            }
        }
        self.cursor[i][j] = seq;
        Ok(())
    }

    fn connect(&mut self, i: usize, j: usize) -> R {
        self.up[i][j] = true;
        self.up[j][i] = true;
        self.join(i, j)?;
        self.join(j, i)
    }

    fn disconnect(&mut self, i: usize, j: usize) {
        self.up[i][j] = false;
        self.up[j][i] = false;
        self.q[i][j].clear();
        self.q[j][i].clear();
    }

    /// `i` sends `j` its changes since the last send (at most `limit` records).
    fn send(&mut self, i: usize, j: usize, limit: usize) -> R<usize> {
        let from = self.cursor[i][j];
        let recs = self.devs[i].core.store().local_since(from, limit).map_err(e("local_since"))?;
        let Some(top) = recs.last().map(|r| r.sequence) else { return Ok(0) };
        let entries = self.devs[i].core.announce_since(from, limit).map_err(e("announce"))?;
        self.cursor[i][j] = top;
        let n = entries.len();
        if n > 0 {
            let (index_id, _) = self.devs[i].core.index_state().map_err(e("index_state"))?;
            let writers = self.devs[i].core.writers();
            self.q[i][j].push_back(Msg { index_id, full: false, done: false, entries, writers });
        }
        Ok(n)
    }

    /// Applies the next message on the link `i` → `j` (twice when `dup`).
    fn deliver(&mut self, i: usize, j: usize, dup: bool) -> R {
        let Some(msg) = self.q[i][j].pop_front() else { return Ok(()) };
        let peer = self.devs[i].short;
        let from = self.devs[i].name.clone();
        self.arm_crash(j);
        let times = if dup { 2 } else { 1 };
        for _ in 0..times {
            let d = &mut self.devs[j];
            let r = d.core.add_writers(&msg.writers).and_then(|_| {
                d.core.apply_remote(
                    peer,
                    RemoteIndex { index_id: msg.index_id, full: msg.full, done: msg.done, entries: msg.entries.clone() },
                )
            });
            if let Err(err) = r {
                if !d.crashed() {
                    return Err(format!("{} refused an index from {from}: {err}", d.name));
                }
                break;
            }
        }
        self.after_op(j)
    }

    // --- crashes, faults ---------------------------------------------------------------------

    fn arm_crash(&mut self, d: usize) {
        if self.cfg.crash > 0.0 && self.rng.chance(self.cfg.crash) {
            let n = self.rng.below(40) as u32;
            self.devs[d].ctl.lock().unwrap().crash_in = Some(n);
        }
    }

    fn arm_faults(&mut self, d: usize) {
        if self.cfg.faults == 0.0 || !self.rng.chance(self.cfg.faults) {
            return;
        }
        let mut paths: Vec<String> = self.disk_paths(d).into_iter().map(|(p, _)| p).collect();
        paths.push(STATE_DIR.to_string());
        if let Ok(needs) = self.devs[d].core.store().needs(None, 100) {
            for (p, _) in needs {
                paths.push(trs_sync::pull::temp_path(&p));
                paths.push(p);
            }
        }
        for _ in 0..1 + self.rng.below(2) {
            let Some(p) = self.rng.pick(&paths) else { return };
            let n = 1 + self.rng.below(3) as u32;
            let kind = [io::ErrorKind::Other, io::ErrorKind::StorageFull, io::ErrorKind::PermissionDenied][self.rng.below(3)];
            self.devs[d].real.fail_next(&p, n, kind);
            self.note(format!("  fault {} {p:?} x{n} {kind:?}", self.devs[d].name));
            self.faults.push((d, p));
        }
    }

    /// After a core operation on `d`: clear faults, account racing edits, reboot after a crash.
    fn after_op(&mut self, d: usize) -> R {
        for (fd, p) in std::mem::take(&mut self.faults) {
            self.devs[fd].real.fail_next(&p, 0, io::ErrorKind::Other);
        }
        let raced = std::mem::take(&mut self.devs[d].ctl.lock().unwrap().raced);
        for r in raced {
            self.note(format!("  race: user wrote {:?} on {} mid-pull", r.path, self.devs[d].name));
            if let Some(old) = r.old {
                self.discharge(d, h64(&old));
            }
        }
        {
            let mut c = self.devs[d].ctl.lock().unwrap();
            c.race = None;
            c.crash_in = None;
        }
        if self.devs[d].crashed() {
            self.note(format!("  CRASH {}", self.devs[d].name));
            if self.cfg.lossy_disk {
                self.devs[d].real.restore();
                // What the crash took from the disk is the crash's loss, not the engine's.
                let mut everywhere = BTreeSet::new();
                for k in 0..self.devs.len() {
                    for n in self.tree(k, true).values() {
                        if let Node::File { hash, .. } = n {
                            everywhere.insert(*hash);
                        }
                    }
                }
                for (h, k) in self.known.iter_mut() {
                    if !everywhere.contains(h) {
                        k.discharged = true;
                    }
                }
            }
            self.reboot(d)?;
        }
        Ok(())
    }

    fn reboot(&mut self, d: usize) -> R {
        self.devs[d].reopen();
        for k in 0..self.devs.len() {
            if k == d {
                continue;
            }
            self.q[d][k].clear();
            self.q[k][d].clear();
            if self.up[d][k] {
                self.connect(d, k)?;
            }
        }
        Ok(())
    }

    // --- core operations ---------------------------------------------------------------------

    fn scan(&mut self, d: usize, scope: &str) -> R {
        let unmounted = self.devs[d].unmounted();
        let before = self.devs[d].store.lock().unwrap().meta().unwrap();
        self.arm_crash(d);
        self.arm_faults(d);
        let had_faults = !self.faults.is_empty();
        let r = self.devs[d].core.scan(scope);
        let crashed = self.devs[d].crashed();
        let after = self.devs[d].store.lock().unwrap().meta().unwrap();
        self.after_op(d)?;
        let name = self.devs[d].name.clone();
        if unmounted {
            if after != before {
                return Err(format!("{name}: a scan of an unmounted folder changed the index"));
            }
            return match r {
                Err(ScanError::FolderMissing) => Ok(()),
                _ if crashed || had_faults => Ok(()),
                Ok(_) => Err(format!("{name}: a scan of an unmounted folder ran")),
                Err(ScanError::Io(err)) => Err(format!("{name}: scan of an unmounted folder: {err}")),
            };
        }
        match r {
            Ok(rep) => {
                self.note(format!(
                    "  scan {name} {scope:?}: changed {} deleted {} busy {} issues {}",
                    rep.changed,
                    rep.deleted,
                    rep.busy.len(),
                    rep.issues.len()
                ));
                self.observe(d)
            }
            Err(_) if crashed || had_faults => Ok(()),
            Err(ScanError::FolderMissing) => Err(format!("{name}: folder missing, but it is there")),
            Err(ScanError::Io(err)) => Err(format!("{name}: scan failed: {err}")),
        }
    }

    fn pull(&mut self, d: usize, race: Option<Race>) -> R<trs_sync::pull::PullReport> {
        let peers: Vec<(ShortId, MemFs)> = (0..self.devs.len())
            .filter(|&k| k != d && self.up[d][k] && !self.devs[k].unmounted())
            .map(|k| (self.devs[k].short, self.devs[k].real.clone()))
            .collect();
        let names: Vec<(ShortId, String)> = self.devs.iter().map(|x| (x.short, x.name.clone())).collect();
        let name = move |id: ShortId| names.iter().find(|(s, _)| *s == id).map_or("?".into(), |(_, n)| n.clone());
        let window = self.rng.pick(&[64u64 << 10, 1 << 20, 64 << 20]).unwrap();
        if let Some(r) = race {
            self.note(format!("  race armed: {} {} atomic={}", if r.before { "before" } else { "after" }, r.op, r.atomic));
            self.devs[d].ctl.lock().unwrap().race = Some(r);
        }
        let unmounted = self.devs[d].unmounted();
        let had_faults = !self.faults.is_empty();
        let r = self.devs[d].core.pull(&Source { peers }, &name, window);
        let crashed = self.devs[d].crashed();
        self.after_op(d)?;
        let dn = self.devs[d].name.clone();
        match r {
            Ok(rep) => {
                self.note(format!(
                    "  pull {dn}: done {} waiting {} rescan {:?} failed {:?} conflicts {:?}",
                    rep.done, rep.waiting, rep.rescan, rep.failed, rep.conflicts
                ));
                Ok(rep)
            }
            Err(_) if crashed || unmounted || had_faults => Ok(Default::default()),
            Err(err) => Err(format!("{dn}: pull failed: {err}")),
        }
    }

    // --- what is on the disks ----------------------------------------------------------------

    /// Every path on `d`'s disk (not the state dir, not temp files), with its meta.
    fn disk_paths(&self, d: usize) -> Vec<(String, Meta)> {
        walk(&self.devs[d].real, false)
    }

    fn content_hash(&mut self, d: usize, p: &str, m: &Meta) -> u64 {
        let k = (d, m.inode, m.mtime_ns, m.ctime_ns, m.size);
        if let Some(h) = self.cache.get(&k) {
            return *h;
        }
        let h = h64(&read_all(&self.devs[d].real, p).unwrap_or_default());
        self.cache.insert(k, h);
        h
    }

    fn tree(&mut self, d: usize, with_state: bool) -> BTreeMap<String, Node> {
        let mut out = BTreeMap::new();
        for (p, m) in walk(&self.devs[d].real, with_state) {
            let node = match m.kind {
                FileKind::Dir => Node::Dir,
                FileKind::File => Node::File { hash: self.content_hash(d, &p, &m), size: m.size, exec: m.exec },
                FileKind::Symlink => Node::Link(self.devs[d].real.read_link(&p).unwrap_or_default()),
            };
            out.insert(p, node);
        }
        out
    }

    /// Remembers the tombstones `d` holds (for telling the known design gap from other resurrections).
    fn collect_tombs(&mut self, d: usize) {
        let st = self.devs[d].store.lock().unwrap();
        for p in st.local_paths("", None, usize::MAX).unwrap() {
            let r = st.local(&p).unwrap().unwrap();
            for (_, e) in st.remote(&p).unwrap() {
                if e.file.info.deleted {
                    self.announced.insert((p.clone(), key_of(&e.file.info.version)));
                }
            }
            if r.file.info.deleted {
                let v = self.tombs.entry(p).or_default();
                if !v.contains(&r.file.info.version) {
                    v.push(r.file.info.version.clone());
                }
            }
        }
    }

    /// After a scan: every file whose record matches the disk is known to the system.
    fn observe(&mut self, d: usize) -> R {
        self.collect_tombs(d);
        for (p, m) in self.disk_paths(d) {
            if m.kind != FileKind::File || m.size == 0 {
                continue;
            }
            let Some(rec) = self.devs[d].store.lock().unwrap().local(&p).unwrap() else { continue };
            if rec.file.info.deleted || rec.stamp != Some(stamp(&m)) {
                continue;
            }
            let h = self.content_hash(d, &p, &m);
            let writer = self.devs[d].writer && !rec.invalid;
            let (step, at) = (self.step, format!("{}:{p}", self.devs[d].name));
            let discharged = self.replaced.contains(&h);
            let k = self.known.entry(h).or_insert(Known { writer, discharged, first: step, size: m.size as usize, at });
            k.writer |= writer;
        }
        Ok(())
    }

    /// A user on `d` replaced or removed content `h`. (Even when another copy is still there: that copy
    /// may be a stale replica that sync will replace legitimately. The price: losing one of two
    /// deliberate copies goes unnoticed once the other was replaced.)
    fn discharge(&mut self, d: usize, h: u64) {
        let writer = self.devs[d].writer;
        if writer {
            // The engine may have recorded it already (a scan that crashed after its commit) though
            // `observe` has not seen it yet: once known, it starts out discharged.
            self.replaced.insert(h);
        }
        if let Some(k) = self.known.get_mut(&h)
            && (writer || !k.writer)
        {
            k.discharged = true;
        }
    }

    // --- user actions ------------------------------------------------------------------------

    fn content(&mut self, d: usize) -> Vec<u8> {
        let size = match self.rng.weighted(&[70, 20, 8, 2]) {
            0 => self.rng.below(2 << 10),
            1 => 2048 + self.rng.below(38 << 10),
            2 => (40 << 10) + self.rng.below(80 << 10),
            _ => (120 << 10) + self.rng.below(180 << 10),
        };
        let mut data = Vec::with_capacity(size);
        // Sometimes built from pieces of files already there, so chunks are shared.
        if size > 20 << 10 && self.rng.chance(0.4) {
            let files: Vec<String> =
                self.disk_paths(d).into_iter().filter(|(_, m)| m.kind == FileKind::File && m.size > 0).map(|(p, _)| p).collect();
            while data.len() < size {
                match self.rng.pick(&files) {
                    Some(f) if self.rng.chance(0.6) => {
                        let src = read_all(&self.devs[d].real, &f).unwrap_or_default();
                        let a = self.rng.below(src.len());
                        let b = (a + 1 + self.rng.below(src.len() - a)).min(src.len());
                        data.extend_from_slice(&src[a..b]);
                    }
                    _ => {
                        let n = 1 + self.rng.below(32 << 10);
                        data.extend(self.rng.bytes(n));
                    }
                }
            }
            data.truncate(size);
            return data;
        }
        self.rng.bytes(size)
    }

    fn user(&mut self, d: usize) -> R {
        let paths = self.disk_paths(d);
        let files: Vec<String> = paths.iter().filter(|(_, m)| m.kind == FileKind::File).map(|(p, _)| p.clone()).collect();
        let links: Vec<String> = paths.iter().filter(|(_, m)| m.kind == FileKind::Symlink).map(|(p, _)| p.clone()).collect();
        let dirs: Vec<String> = paths.iter().filter(|(_, m)| m.kind == FileKind::Dir).map(|(p, _)| p.clone()).collect();
        let mut parents = vec![String::new(), String::new()];
        parents.extend(dirs.iter().filter(|p| p.matches('/').count() < 2).cloned());
        let fs = self.devs[d].real.clone();
        let dn = self.devs[d].name.clone();
        let exists = |p: &str| fs.lstat(p).ok().flatten();
        let names = if self.cfg.kinds { ANY_NAMES } else { FILE_NAMES };
        let new_file_path = |rng: &mut Rng| join(&rng.pick(&parents).unwrap(), rng.pick(names).unwrap());

        let replace = if self.cfg.kinds { 6 } else { 0 };
        let action = self.rng.weighted(&[30, 8, 6, 5, 10, 8, 2, 3, 8, 3, 4, 3, 2, replace]);
        match action {
            // write (create or overwrite)
            0 => {
                let p = if self.rng.chance(0.5) && !files.is_empty() {
                    self.rng.pick(&files).unwrap()
                } else {
                    new_file_path(&mut self.rng)
                };
                if exists(&p).is_some_and(|m| m.kind != FileKind::File) {
                    return Ok(());
                }
                let data = self.content(d);
                let atomic = self.rng.chance(0.5);
                let old = exists(&p).map(|_| h64(&read_all(&fs, &p).unwrap()));
                if user_write(&fs, &p, &data, atomic).is_ok() {
                    self.note(format!("{dn} write {p:?} {} bytes atomic={atomic}", data.len()));
                    self.wrote(d, &p);
                    if let Some(o) = old {
                        self.discharge(d, o);
                    }
                }
            }
            // append
            1 => {
                let Some(p) = self.rng.pick(&files) else { return Ok(()) };
                let old = read_all(&fs, &p).map_err(e("read"))?;
                let max = if self.rng.chance(0.8) { 1 << 10 } else { 64 << 10 };
                let n = 1 + self.rng.below(max);
                let add = self.rng.bytes(n);
                let fh = fs.open_rw(&p).map_err(e("open"))?;
                fh.write_at(old.len() as u64, &add).map_err(e("append"))?;
                drop(fh);
                self.note(format!("{dn} append {p:?} +{n}"));
                self.wrote(d, &p);
                self.discharge(d, h64(&old));
            }
            // overwrite a range in the middle (in place)
            2 => {
                let Some(p) = self.rng.pick(&files) else { return Ok(()) };
                let old = read_all(&fs, &p).map_err(e("read"))?;
                if old.is_empty() {
                    return Ok(());
                }
                let at = self.rng.below(old.len());
                let n = 1 + self.rng.below(4096);
                let patch = self.rng.bytes(n);
                let fh = fs.open_rw(&p).map_err(e("open"))?;
                fh.write_at(at as u64, &patch).map_err(e("patch"))?;
                drop(fh);
                self.note(format!("{dn} patch {p:?} @{at} {n} bytes"));
                self.wrote(d, &p);
                self.discharge(d, h64(&old));
            }
            // copy
            3 => {
                let Some(from) = self.rng.pick(&files) else { return Ok(()) };
                let to = new_file_path(&mut self.rng);
                if exists(&to).is_some() {
                    return Ok(());
                }
                let data = read_all(&fs, &from).map_err(e("read"))?;
                write_all(&fs, &to, &data).map_err(e("copy"))?;
                self.note(format!("{dn} copy {from:?} -> {to:?}"));
                self.wrote(d, &to);
                // A new copy on a writer: the content is wanted again. (A read-only device's copy is
                // its own local file; it cannot make a writer's content wanted again.)
                if self.devs[d].writer {
                    self.replaced.remove(&h64(&data));
                    if let Some(k) = self.known.get_mut(&h64(&data)) {
                        k.discharged = false;
                    }
                }
            }
            // delete a file or link
            4 => {
                let pool: Vec<String> = files.iter().chain(links.iter()).cloned().collect();
                let Some(p) = self.rng.pick(&pool) else { return Ok(()) };
                self.removing(d, &p);
                fs.remove_file(&p).map_err(e("delete"))?;
                self.note(format!("{dn} delete {p:?}"));
                self.discharge_removed(d);
            }
            // move a file, sometimes over another file
            5 | 6 => {
                let Some(from) = self.rng.pick(&files) else { return Ok(()) };
                let to = if action == 6 {
                    // a case-only rename
                    let n = last(&from);
                    let flipped: String =
                        n.chars().map(|c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect();
                    join(parent_of(&from), &flipped)
                } else {
                    new_file_path(&mut self.rng)
                };
                if to == from || exists(&to).is_some_and(|m| m.kind != FileKind::File) {
                    return Ok(());
                }
                self.removing(d, &from);
                if exists(&to).is_some() {
                    self.removing(d, &to);
                }
                fs.rename(&from, &to).map_err(e("rename"))?;
                self.note(format!("{dn} move {from:?} -> {to:?}"));
                self.wrote(d, &to);
                self.discharge_removed(d);
            }
            // move a directory
            7 => {
                let Some(from) = self.rng.pick(&dirs) else { return Ok(()) };
                let to = join(&self.rng.pick(&parents).unwrap(), self.rng.pick(if self.cfg.kinds { ANY_NAMES } else { DIR_NAMES }).unwrap());
                if to == from || to.starts_with(&format!("{from}/")) || exists(&to).is_some() || to.matches('/').count() > 2 {
                    return Ok(());
                }
                let inside: Vec<String> =
                    paths.iter().filter(|(p, _)| p.starts_with(&format!("{from}/"))).map(|(p, _)| p.clone()).collect();
                for p in &inside {
                    self.removing(d, p);
                }
                fs.rename(&from, &to).map_err(e("rename dir"))?;
                self.note(format!("{dn} move dir {from:?} -> {to:?}"));
                for p in &inside {
                    self.wrote(d, &format!("{to}{}", &p[from.len()..]));
                }
                self.discharge_removed(d);
            }
            // mkdir
            8 => {
                let p = join(&self.rng.pick(&parents).unwrap(), self.rng.pick(if self.cfg.kinds { ANY_NAMES } else { DIR_NAMES }).unwrap());
                if exists(&p).is_some() {
                    return Ok(());
                }
                fs.create_dir(&p).map_err(e("mkdir"))?;
                self.note(format!("{dn} mkdir {p:?}"));
            }
            // rm -r
            9 => {
                let Some(dir) = self.rng.pick(&dirs) else { return Ok(()) };
                let mut inside: Vec<(String, Meta)> =
                    walk_all(&fs, &dir).into_iter().collect::<Vec<_>>();
                inside.sort_by(|a, b| b.0.cmp(&a.0));
                for (p, m) in &inside {
                    if m.kind != FileKind::Dir && !is_tmp_name(last(p)) {
                        self.removing(d, p);
                    }
                }
                for (p, m) in &inside {
                    if m.kind == FileKind::Dir { fs.remove_dir(p) } else { fs.remove_file(p) }.map_err(e("rm -r"))?;
                }
                fs.remove_dir(&dir).map_err(e("rmdir"))?;
                self.note(format!("{dn} rm -r {dir:?} ({} inside)", inside.len()));
                self.discharge_removed(d);
            }
            // chmod +x / -x
            10 => {
                let Some(p) = self.rng.pick(&files) else { return Ok(()) };
                let m = exists(&p).unwrap();
                fs.set_exec(&p, !m.exec).map_err(e("chmod"))?;
                self.note(format!("{dn} chmod {p:?} exec={}", !m.exec));
                self.wrote(d, &p);
            }
            // touch
            11 => {
                let Some(p) = self.rng.pick(&files) else { return Ok(()) };
                let now = self.base.load(Ordering::SeqCst) + self.devs[d].clock.skew;
                fs.set_mtime(&p, now).map_err(e("touch"))?;
                self.note(format!("{dn} touch {p:?}"));
            }
            // a file becomes a directory, or a directory a file
            13 => {
                let pool: Vec<String> = files.iter().chain(dirs.iter()).cloned().collect();
                let Some(p) = self.rng.pick(&pool) else { return Ok(()) };
                let m = exists(&p).unwrap();
                if m.kind == FileKind::Dir {
                    let mut inside = walk_all(&fs, &p);
                    inside.sort_by(|a, b| b.0.cmp(&a.0));
                    for (q, qm) in &inside {
                        if qm.kind != FileKind::Dir && !is_tmp_name(last(q)) {
                            self.removing(d, q);
                        }
                    }
                    for (q, qm) in &inside {
                        if qm.kind == FileKind::Dir { fs.remove_dir(q) } else { fs.remove_file(q) }.map_err(e("rm -r"))?;
                    }
                    fs.remove_dir(&p).map_err(e("rmdir"))?;
                    let data = self.content(d);
                    write_all(&fs, &p, &data).map_err(e("write"))?;
                    self.note(format!("{dn} replace dir {p:?} with a file of {} bytes", data.len()));
                    self.wrote(d, &p);
                } else {
                    self.removing(d, &p);
                    fs.remove_file(&p).map_err(e("delete"))?;
                    fs.create_dir(&p).map_err(e("mkdir"))?;
                    self.note(format!("{dn} replace file {p:?} with a directory"));
                }
                self.discharge_removed(d);
            }
            // symlink
            _ => {
                let p = new_file_path(&mut self.rng);
                if exists(&p).is_some() {
                    return Ok(());
                }
                let target = self.rng.pick(&["f1", "../d1/g.txt", "/etc/passwd", "d2"]).unwrap();
                fs.symlink(target, &p).map_err(e("symlink"))?;
                self.note(format!("{dn} symlink {p:?} -> {target:?}"));
            }
        }
        Ok(())
    }

    /// The user is about to remove `p` from `d`: remember its content there.
    fn removing(&mut self, d: usize, p: &str) {
        let fs = self.devs[d].real.clone();
        if let Ok(Some(m)) = fs.lstat(p)
            && m.kind == FileKind::File
        {
            let h = self.content_hash(d, p, &m);
            self.removed.insert((d, self.devs[d].writer, p.to_string(), h, m.exec));
            self.pending_discharge.push(h);
        }
    }

    fn discharge_removed(&mut self, d: usize) {
        for h in std::mem::take(&mut self.pending_discharge) {
            self.discharge(d, h);
        }
    }

    /// The user put content at `p` on `d`: that content there is no resurrection.
    fn wrote(&mut self, d: usize, p: &str) {
        let fs = self.devs[d].real.clone();
        if let Ok(Some(m)) = fs.lstat(p)
            && m.kind == FileKind::File
        {
            let h = self.content_hash(d, p, &m);
            self.removed.retain(|(_, _, rp, rh, rx)| !(rp == p && *rh == h && *rx == m.exec));
            self.written.insert((d, p.to_string(), h, m.exec));
        }
    }

    // --- invariants --------------------------------------------------------------------------

    fn check_step(&mut self, full: bool) -> R {
        for d in 0..self.devs.len() {
            let v = std::mem::take(&mut self.devs[d].ctl.lock().unwrap().violations);
            if let Some(x) = v.first() {
                return Err(format!("{}: {x}", self.devs[d].name));
            }
        }
        for d in 0..self.devs.len() {
            self.collect_tombs(d);
        }
        // (2a) nothing known and wanted is gone from every device.
        let mut everywhere = BTreeSet::new();
        for d in 0..self.devs.len() {
            for n in self.tree(d, true).values() {
                if let Node::File { hash, .. } = n {
                    everywhere.insert(*hash);
                }
            }
        }
        for (h, k) in &self.known {
            if !k.discharged && !everywhere.contains(h) {
                return Err(format!("DATA LOSS: content {h:016x} ({} bytes, first known at step {} as {}) is on no device", k.size, k.first, k.at));
            }
        }
        // The index tells the truth: a record whose stamp matches the disk describes the disk.
        for d in 0..self.devs.len() {
            self.check_records(d, full)?;
        }
        Ok(())
    }

    fn check_records(&mut self, d: usize, full: bool) -> R {
        let store = self.devs[d].store.lock().unwrap().clone();
        let fs = self.devs[d].real.clone();
        for p in store.local_paths("", None, usize::MAX).unwrap() {
            let rec = store.local(&p).unwrap().unwrap();
            let info = &rec.file.info;
            if info.deleted || info.kind != FileKind::File || rec.invalid {
                continue;
            }
            let Ok(Some(m)) = fs.lstat(&p) else { continue };
            if rec.stamp != Some(stamp(&m)) {
                continue;
            }
            let bad = if m.size != info.size || m.exec != info.exec {
                true
            } else if full {
                let data = read_all(&fs, &p).unwrap_or_default();
                offsets(&info.chunks).any(|(off, c)| {
                    let end = off as usize + c.len as usize;
                    end > data.len() || <[u8; 32]>::from(Sha256::digest(&data[off as usize..end])) != c.hash
                })
            } else {
                false
            };
            if bad {
                return Err(format!(
                    "{}: the record of {p:?} says {} bytes exec={} but the disk (same stamp) holds {} bytes exec={}{}",
                    self.devs[d].name,
                    info.size,
                    info.exec,
                    m.size,
                    m.exec,
                    if m.size == info.size { ", other content" } else { "" }
                ));
            }
        }
        Ok(())
    }

    // --- the run -----------------------------------------------------------------------------

    fn run(&mut self) -> R {
        let n = self.devs.len();
        for i in 0..n {
            for j in i + 1..n {
                self.connect(i, j)?;
            }
        }
        for s in 0..self.cfg.steps {
            self.step = s;
            let dt = if self.rng.chance(0.05) { 120_000_000_000 } else { self.rng.below(3_000_000_000) as i64 };
            self.base.fetch_add(dt, Ordering::SeqCst);
            self.one_step()?;
            if self.cfg.lossy_disk && self.rng.chance(0.3) {
                let d = self.rng.below(self.devs.len());
                self.devs[d].real.snapshot();
            }
            self.trace();
            self.check_step(s % 10 == 0)?;
        }
        self.step = self.cfg.steps;
        self.note("--- heal and settle".into());
        self.settle()
    }

    fn one_step(&mut self) -> R {
        let n = self.devs.len();
        let d = self.rng.below(n);
        let resets = if self.cfg.resets { 2 } else { 0 };
        match self.rng.weighted(&[35, 15, 15, 17, 12, 2, 2, 1, resets]) {
            0 => {
                if !self.devs[d].unmounted() {
                    self.user(d)?;
                    // A crash in lossy-disk mode loses what the engine had not made durable, not the
                    // user's own edits (an app saving a document syncs it).
                    if self.cfg.lossy_disk {
                        self.devs[d].real.snapshot();
                    }
                }
            }
            1 => {
                let scope = if self.rng.chance(0.7) {
                    String::new()
                } else {
                    let mut ps: Vec<String> = self.disk_paths(d).into_iter().map(|(p, _)| p).collect();
                    ps.extend(self.devs[d].store.lock().unwrap().local_paths("", None, usize::MAX).unwrap());
                    self.rng.pick(&ps).unwrap_or_default()
                };
                self.note(format!("scan {} {scope:?}", self.devs[d].name));
                self.scan(d, &scope)?;
            }
            2 => {
                let j = self.rng.below(n);
                if j != d && self.up[d][j] {
                    let limit = if self.rng.chance(0.3) { 1 + self.rng.below(4) } else { usize::MAX };
                    let k = self.send(d, j, limit)?;
                    if k > 0 {
                        self.note(format!("send {} -> {} {k} entries", self.devs[d].name, self.devs[j].name));
                    }
                }
            }
            3 => {
                let links: Vec<(usize, usize)> = (0..n)
                    .flat_map(|i| (0..n).map(move |j| (i, j)))
                    .filter(|&(i, j)| self.up[i][j] && !self.q[i][j].is_empty())
                    .collect();
                if let Some((i, j)) = self.rng.pick(&links) {
                    let m = &self.q[i][j][0];
                    // Duplicates of changes only: a repeated batch of a full send starts that full
                    // send over and drops the batches before it (TCP never repeats one).
                    let dup = !m.full && self.rng.chance(0.05);
                    let what = format!(
                        "deliver {} -> {} ({} entries{}{}{})",
                        self.devs[i].name,
                        self.devs[j].name,
                        m.entries.len(),
                        if m.full { ", full" } else { "" },
                        if m.done { ", done" } else { "" },
                        if dup { ", twice" } else { "" }
                    );
                    self.note(what);
                    self.deliver(i, j, dup)?;
                }
            }
            4 => {
                let race = (self.cfg.race > 0.0 && self.rng.chance(self.cfg.race)).then(|| {
                    let data = self.content(d);
                    let (before, op) = if self.cfg.race_after_rename && self.rng.chance(0.5) {
                        (false, "rename")
                    } else {
                        (true, if self.rng.chance(0.5) { "set_exec" } else { "open_rw" })
                    };
                    Race { before, op, data, atomic: self.rng.chance(0.5) }
                });
                self.note(format!("pull {}", self.devs[d].name));
                self.arm_crash(d);
                self.arm_faults(d);
                let rep = self.pull(d, race)?;
                if !rep.rescan.is_empty() && self.rng.chance(0.7) {
                    for p in rep.rescan {
                        self.scan(d, &p)?;
                    }
                }
            }
            5 => {
                // The network changes: a random split, one link, or everything heals.
                match self.rng.below(3) {
                    0 => {
                        let side: Vec<bool> = (0..n).map(|_| self.rng.chance(0.5)).collect();
                        self.note(format!("partition {side:?}"));
                        for i in 0..n {
                            for j in i + 1..n {
                                if side[i] != side[j] {
                                    self.disconnect(i, j);
                                }
                            }
                        }
                    }
                    1 => {
                        let j = self.rng.below(n);
                        if j != d {
                            if self.up[d][j] {
                                self.note(format!("link {}-{} down", self.devs[d].name, self.devs[j].name));
                                self.disconnect(d, j);
                            } else {
                                self.note(format!("link {}-{} up", self.devs[d].name, self.devs[j].name));
                                self.connect(d, j)?;
                            }
                        }
                    }
                    _ => {
                        self.note("heal".into());
                        self.heal()?;
                    }
                }
            }
            6 => {
                if self.cfg.crash > 0.0 {
                    self.note(format!("restart {}", self.devs[d].name));
                    self.reboot(d)?;
                }
            }
            8 => {
                // The index database is lost (a new index id): the device rescans from nothing.
                let id = self.rng.next();
                *self.devs[d].store.lock().unwrap() = MemStore::new(id);
                self.note(format!("RESET the index of {}", self.devs[d].name));
                self.was_reset.insert(d);
                self.reboot(d)?;
            }
            _ => {
                if self.cfg.unmount {
                    let m = self.devs[d].fs.mount.clone();
                    let mut g = m.lock().unwrap();
                    if g.is_some() {
                        *g = None;
                        drop(g);
                        self.note(format!("remount {}", self.devs[d].name));
                    } else {
                        // What is there while the disk is away: an empty directory.
                        *g = Some(MemFs::new());
                        drop(g);
                        self.note(format!("unmount {}", self.devs[d].name));
                    }
                }
            }
        }
        Ok(())
    }

    fn heal(&mut self) -> R {
        let n = self.devs.len();
        for i in 0..n {
            for j in i + 1..n {
                if !self.up[i][j] {
                    self.connect(i, j)?;
                }
            }
        }
        Ok(())
    }

    /// Heals everything, then scan/send/pull rounds until nothing moves; then the end invariants.
    fn settle(&mut self) -> R {
        let n = self.devs.len();
        self.cfg.crash = 0.0;
        self.cfg.faults = 0.0;
        for d in 0..n {
            *self.devs[d].fs.mount.lock().unwrap() = None;
        }
        self.heal()?;
        let mut quiet = false;
        for round in 0..50 {
            self.step = self.cfg.steps + round;
            let mut moved = false;
            for d in 0..n {
                let seq = self.devs[d].store.lock().unwrap().meta().unwrap().sequence;
                let r = self.devs[d].core.scan("").map_err(|x| format!("{}: final scan: {x:?}", self.devs[d].name))?;
                if r.changed + r.deleted > 0 || !r.busy.is_empty() {
                    let what: Vec<String> = self.devs[d].store.lock().unwrap().local_since(seq, 5).unwrap().into_iter().map(|r| r.file.info.path).collect();
                    self.note(format!("  scan {}: changed {} deleted {} {what:?}", self.devs[d].name, r.changed, r.deleted));
                    moved = true;
                }
                self.observe(d)?;
            }
            for i in 0..n {
                for j in 0..n {
                    if i != j && self.send(i, j, usize::MAX)? > 0 {
                        moved = true;
                    }
                }
            }
            for i in 0..n {
                for j in 0..n {
                    while !self.q[i][j].is_empty() {
                        self.deliver(i, j, false)?;
                    }
                }
            }
            for d in 0..n {
                let mut rounds = 0;
                loop {
                    let rep = self.pull(d, None)?;
                    for p in &rep.rescan {
                        self.devs[d].core.scan(p).map_err(|x| format!("rescan: {x:?}"))?;
                        moved = true;
                    }
                    if rep.done == 0 {
                        break;
                    }
                    moved = true;
                    rounds += 1;
                    if rounds > 50 {
                        let needs = self.devs[d].store.lock().unwrap().needs(None, 5).unwrap();
                        let mut s = format!("{}: pull rounds do not terminate; needs {:?}", self.devs[d].name, needs.iter().map(|(p, n)| (p.as_str(), n.need)).collect::<Vec<_>>());
                        for (p, _) in needs.iter().take(3) {
                            s += &format!("\n  {p:?}{}", self.explain(p));
                        }
                        return Err(s);
                    }
                }
            }
            self.trace();
            self.check_step(false)?;
            let needs = self.untolerated_needs();
            if !moved && needs == 0 {
                quiet = true;
                break;
            }
        }
        if !quiet {
            let mut s = String::from("no convergence after 50 rounds");
            for d in &self.devs {
                let needs = d.store.lock().unwrap().needs(None, 20).unwrap();
                s += &format!("\n  {} needs {:?}", d.name, needs.iter().map(|(p, n)| (p.as_str(), n.need)).collect::<Vec<_>>());
            }
            let paths: BTreeSet<String> = self
                .devs
                .iter()
                .flat_map(|d| d.store.lock().unwrap().needs(None, 3).unwrap().into_iter().map(|(p, _)| p))
                .collect();
            for p in paths.iter().take(3) {
                let disk: Vec<String> = self
                    .devs
                    .iter()
                    .map(|d| match d.real.lstat(p).ok().flatten() {
                        Some(m) => format!("{}:{:?} {}b x{} mtime {} ino {}", d.name, m.kind, m.size, m.exec as u8, m.mtime_ns - START_NS, m.inode),
                        None => format!("{}:-", d.name),
                    })
                    .collect();
                s += &format!("\n  {p:?}: disk {}{}", disk.join(", "), self.explain(p));
            }
            return Err(s);
        }
        self.check_end()
    }

    /// Every device's view of one path, for failure messages.
    fn explain(&self, p: &str) -> String {
        let mut s = String::new();
        let ver = |f: &trs_sync_proto::model::SignedFile| {
            let v: Vec<String> = f.info.version.counters().iter().map(|c| format!("{}:{}", self.name_of(c.id), c.value)).collect();
            format!(
                "[{}]{} by {} size {} exec {} mtime {}",
                v.join(" "),
                if f.info.deleted { " DELETED" } else { "" },
                self.name_of(f.info.modified_by),
                f.info.size,
                f.info.exec,
                f.info.mtime_ns - START_NS
            )
        };
        for d in &self.devs {
            let st = d.store.lock().unwrap();
            let l = st.local(p).unwrap();
            s += &format!("\n    {} local: {}", d.name, l.as_ref().map_or("-".into(), |r| format!("{}{} seq {}", ver(&r.file), if r.invalid { " INVALID" } else { "" }, r.sequence)));
            for (dev, e) in st.remote(p).unwrap() {
                s += &format!("\n       remote {}: {}{}", self.name_of(dev), ver(&e.file), if e.invalid { " INVALID" } else { "" });
            }
            if let Some(n) = st.need(p).unwrap() {
                s += &format!("\n       need {:?}", n.need);
            }
        }
        s
    }

    /// With `SYNC_SIM_TRACE=<path>`, logs every device's view of that path after each step.
    fn trace(&mut self) {
        if let Ok(p) = std::env::var("SYNC_SIM_TRACE") {
            let x = self.explain(&p);
            let disk: Vec<String> = self.devs.iter().map(|d| match d.real.lstat(&p).ok().flatten() {
                Some(m) => format!("{}:{}b x{}", d.name, m.size, m.exec as u8),
                None => format!("{}:-", d.name),
            }).collect();
            self.note(format!("  trace {p:?} disk {}{x}", disk.join(" ")));
        }
    }

    fn name_of(&self, id: ShortId) -> String {
        self.devs.iter().find(|d| d.short == id).map_or(format!("{id:?}"), |d| d.name.clone())
    }

    /// Needs that stay by design: on case-insensitive disks, a name that differs only in case from one
    /// on the disk is refused.
    fn tolerated(&self, d: usize, p: &str, _n: &NeedRecord) -> bool {
        let dev = &self.devs[d];
        // A name that differs only in case from one on the disk is refused, by design.
        if self.cfg.case_insensitive {
            let clash = |q: &str| {
                dev.real.read_dir(parent_of(q)).is_ok_and(|l| {
                    l.entries.iter().any(|e| e.name != last(q) && e.name.eq_ignore_ascii_case(last(q)))
                })
            };
            let mut pre = String::new();
            if p.split('/').any(|c| {
                pre = join(&pre, c);
                clash(&pre)
            }) {
                return true;
            }
        }
        false
    }

    fn untolerated_needs(&self) -> usize {
        (0..self.devs.len())
            .map(|d| {
                let needs = self.devs[d].store.lock().unwrap().needs(None, usize::MAX).unwrap();
                needs.iter().filter(|(p, n)| !self.tolerated(d, p, n)).count()
            })
            .sum()
    }

    fn check_end(&mut self) -> R {
        self.check_step(true)?;
        let n = self.devs.len();
        let writers: Vec<usize> = (0..n).filter(|&d| self.devs[d].writer).collect();
        let mut trees: Vec<BTreeMap<String, Node>> = (0..n).map(|d| self.tree(d, false)).collect();
        let live: BTreeSet<u64> = writers
            .iter()
            .flat_map(|&w| trees[w].values())
            .filter_map(|x| match x {
                Node::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect();
        if self.cfg.case_insensitive {
            // Names that differ only in case are refused with an issue, by design: the devices keep
            // their own spelling there, and those paths are left out of the comparison.
            let mut spellings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for t in &trees {
                for p in t.keys() {
                    let mut pre = String::new();
                    for c in p.split('/') {
                        pre = join(&pre, c);
                        spellings.entry(pre.to_ascii_lowercase()).or_default().insert(pre.clone());
                    }
                }
            }
            let clash = |p: &str| {
                let mut pre = String::new();
                p.split('/').any(|c| {
                    pre = join(&pre, c);
                    spellings.get(&pre.to_ascii_lowercase()).is_some_and(|s| s.len() > 1)
                })
            };
            for t in &mut trees {
                t.retain(|p, _| !clash(p));
            }
        }
        // (1) convergence
        let w0 = writers[0];
        for &w in &writers[1..] {
            if trees[w] != trees[w0] {
                return Err(format!(
                    "the writers differ: {} vs {}: {}",
                    self.devs[w0].name,
                    self.devs[w].name,
                    diff(&trees[w0], &trees[w])
                ));
            }
        }
        for d in 0..n {
            if self.devs[d].writer {
                continue;
            }
            let store = self.devs[d].store.lock().unwrap().clone();
            let local_change = |p: &str| {
                store.local(p).unwrap().is_some_and(|r| r.invalid)
                    || store.need(p).unwrap().is_some_and(|n| self.tolerated(d, p, &n))
            };
            let all: BTreeSet<&String> = trees[w0].keys().chain(trees[d].keys()).collect();
            for p in all {
                if local_change(p) {
                    continue;
                }
                if trees[w0].get(p) != trees[d].get(p) {
                    return Err(format!(
                        "read-only {} differs at {p:?}: writers {:?}, it has {:?}",
                        self.devs[d].name,
                        trees[w0].get(p),
                        trees[d].get(p)
                    ));
                }
            }
        }
        // (2b) what a writer made and nobody replaced is still a live file.
        for (h, k) in &self.known {
            if k.writer && !k.discharged && !live.contains(h) {
                return Err(format!(
                    "LOST FROM VIEW: content {h:016x} ({} bytes, known at step {} as {}) was replaced without a conflict copy; it is only in an archive",
                    k.size, k.first, k.at
                ));
            }
        }
        // (3) no delete comes back. Not checked for conflict copies: two devices that keep the same
        // losing version make the same copy with two concurrent versions, and deleting one of them
        // loses to the other ("a change beats a delete"), see regress::a_deleted_conflict_copy_*.
        for (origin, writer, p, h, x) in &self.removed.clone() {
            // A lost index forgets the deletes it had (or had not yet) recorded: "a reset database
            // sends everything", and what it still has on disk comes back.
            if self.was_reset.contains(origin) {
                continue;
            }
            if trs_sync::conflict::looks_like_conflict(p) {
                continue;
            }
            let devices: Vec<usize> = if *writer { (0..n).collect() } else { vec![*origin] };
            for d in devices {
                if self.was_reset.contains(&d) {
                    continue; // it re-offers what its lost index no longer says it deleted
                }
                if let Some(Node::File { hash, exec, .. }) = trees[d].get(p)
                    && hash == h
                    && exec == x
                {
                    // The same content can come back legitimately: another device's user made the
                    // same change (a chmod on both), not having seen the delete, and a change beats a
                    // delete. Nothing else brings it back.
                    if self.written.iter().any(|(wd, wp, wh, wx)| wd != origin && wp == p && wh == h && wx == x) {
                        continue;
                    }
                    // The known gap: a version concurrent with the delete but holding the content the
                    // delete removed (merged or absorbed elsewhere, not a change of content) beat it,
                    // and then absorbed it. Telling them apart needs the tombstone to name what it
                    // deleted. (A delete that has *not* been seen by the current version, one that
                    // dominates it, is a dropped history: always a failure.)
                    use trs_sync_proto::model::Causality;
                    let cur = self.devs[d].store.lock().unwrap().local(p).unwrap().map(|r| r.file.info.version);
                    // The other known gap: the deleter took a change over its delete before the delete
                    // ever reached another device, so no holder of the change could absorb it, and a
                    // version it had replaced came back.
                    let unannounced = cur.as_ref().is_some_and(|v| {
                        let ts = self.tombs.get(p).map(Vec::as_slice).unwrap_or_default();
                        let dropping: Vec<_> = ts.iter().filter(|t| t.compare(v) == Causality::Greater).collect();
                        !dropping.is_empty() && dropping.iter().all(|t| !self.announced.contains(&(p.clone(), key_of(t))))
                    });
                    if unannounced && !self.cfg.strict_history {
                        self.note(format!("known gap: {p:?} came back (its delete was never announced)"));
                        continue;
                    }
                    let concurrent = cur.is_some_and(|v| {
                        let ts = self.tombs.get(p).map(Vec::as_slice).unwrap_or_default();
                        !ts.iter().any(|t| t.compare(&v) == Causality::Greater)
                            && ts.iter().any(|t| matches!(v.compare(t), Causality::Greater | Causality::Concurrent))
                    });
                    if concurrent && !self.cfg.strict_history {
                        self.note(format!("known gap: {p:?} came back (same content, concurrent with the delete)"));
                        continue;
                    }
                    return Err(format!(
                        "RESURRECTED: {p:?} came back on {} with the content {} removed{}",
                        self.devs[d].name,
                        self.devs[*origin].name,
                        self.explain(p)
                    ));
                }
            }
        }
        Ok(())
    }
}

fn key_of(v: &trs_sync_proto::model::Version) -> Vec<(u64, u64)> {
    v.counters().iter().map(|c| (c.id.0, c.value)).collect()
}

fn stamp(m: &Meta) -> trs_sync::index::Stamp {
    trs_sync::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }
}

fn diff(a: &BTreeMap<String, Node>, b: &BTreeMap<String, Node>) -> String {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .take(6)
        .map(|k| format!("{k:?}: {:?} / {:?}", a.get(k), b.get(k)))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Every path in the tree (depth first), skipping temp files, and `.seedwell` unless `with_state`.
fn walk(fs: &MemFs, with_state: bool) -> Vec<(String, Meta)> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let Ok(listing) = fs.read_dir(&dir) else { continue };
        for ent in listing.entries {
            if is_tmp_name(&ent.name) || (dir.is_empty() && ent.name == STATE_DIR && !with_state) {
                continue;
            }
            let p = join(&dir, &ent.name);
            let Ok(Some(m)) = fs.lstat(&p) else { continue };
            if m.kind == FileKind::Dir {
                stack.push(p.clone());
            }
            out.push((p, m));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Everything under `dir`, temp files included.
fn walk_all(fs: &MemFs, dir: &str) -> Vec<(String, Meta)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_string()];
    while let Some(d) = stack.pop() {
        let Ok(listing) = fs.read_dir(&d) else { continue };
        for ent in listing.entries {
            let p = join(&d, &ent.name);
            let Ok(Some(m)) = fs.lstat(&p) else { continue };
            if m.kind == FileKind::Dir {
                stack.push(p.clone());
            }
            out.push((p, m));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Entry points

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Runs one seed; on failure, the seed, the configuration and the action log.
fn run_seed(seed: u64, steps: usize, mode: &str) -> Result<(), String> {
    let mut sim = Sim::new(seed, steps, mode);
    let cfg = sim.cfg.clone();
    let r = catch_unwind(AssertUnwindSafe(|| sim.run()));
    let err = match r {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(x)) => x,
        Err(p) => format!("PANIC: {}", p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()),
    };
    let tail = sim.log.len().saturating_sub(400);
    Err(format!(
        "seed {seed} (mode {mode:?}, {steps} steps) failed: {err}\nconfig {cfg:?}\nreplay: SYNC_SIM_SEED={seed}{} cargo nextest run -p trs-sync -E 'test(=sim::replay)' --no-capture\nlog (last {} lines):\n{}",
        if mode.is_empty() { String::new() } else { format!(" SYNC_SIM_MODE={mode}") },
        sim.log.len() - tail,
        sim.log[tail..].join("\n")
    ))
}

const SHARDS: u64 = 8;

fn shard(k: u64, mode: &str, default_seeds: u64) {
    let seeds = env_u64("SYNC_SIM_SEEDS", default_seeds);
    let steps = env_u64("SYNC_SIM_STEPS", 200) as usize;
    let mut failures = Vec::new();
    let mut ran = 0;
    for seed in (0..seeds).filter(|s| s % SHARDS == k) {
        ran += 1;
        if let Err(x) = run_seed(seed, steps, mode) {
            failures.push(x);
        }
    }
    if !failures.is_empty() {
        let lines: Vec<&str> = failures.iter().map(|f| f.lines().next().unwrap_or("")).collect();
        panic!("{} of {ran} seeds failed:\n{}\n\nthe first in full:\n{}", failures.len(), lines.join("\n"), failures[0]);
    }
}

#[test]
fn sim_0() {
    shard(0, "", 300);
}
#[test]
fn sim_1() {
    shard(1, "", 300);
}
#[test]
fn sim_2() {
    shard(2, "", 300);
}
#[test]
fn sim_3() {
    shard(3, "", 300);
}
#[test]
fn sim_4() {
    shard(4, "", 300);
}
#[test]
fn sim_5() {
    shard(5, "", 300);
}
#[test]
fn sim_6() {
    shard(6, "", 300);
}
#[test]
fn sim_7() {
    shard(7, "", 300);
}

/// The known gap fails too (about 0.25 % of seeds).
#[test]
#[ignore = "known gap: a merged or absorbed version (no change of content) concurrent with a delete beats it; tombstones would have to name what they deleted"]
fn sim_strict_history() {
    for k in 0..SHARDS {
        shard(k, "strict-history", 400);
    }
}

/// User edits right after the puller renamed its temp file into place, before it records it.
#[test]
fn sim_race_after_rename() {
    for k in 0..SHARDS {
        shard(k, "race-after-rename", 40);
    }
}

/// Case-insensitive disks, kind changes, lost index databases: each on its own, 40 seeds by default.
#[test]
fn sim_case_insensitive() {
    for k in 0..SHARDS {
        shard(k, "case-insensitive", 40);
    }
}

#[test]
fn sim_kinds() {
    for k in 0..SHARDS {
        shard(k, "kinds", 40);
    }
}

#[test]
fn sim_resets() {
    for k in 0..SHARDS {
        shard(k, "resets", 40);
    }
}

/// Crashes that also lose the disk's namespace changes the engine had not made durable, while the
/// store keeps its commits: the store must never be ahead of the disk.
#[test]
fn sim_lossy_disk() {
    for k in 0..SHARDS {
        shard(k, "lossy-disk", 40);
    }
}

/// Replays `SYNC_SIM_SEED` (with `SYNC_SIM_MODE`, `SYNC_SIM_STEPS`) and prints its log.
#[test]
fn replay() {
    let Ok(seed) = std::env::var("SYNC_SIM_SEED") else { return };
    let seed: u64 = seed.parse().expect("SYNC_SIM_SEED is a number");
    let mode = std::env::var("SYNC_SIM_MODE").unwrap_or_default();
    let steps = env_u64("SYNC_SIM_STEPS", 200) as usize;
    let mut sim = Sim::new(seed, steps, &mode);
    let cfg = sim.cfg.clone();
    let r = sim.run();
    println!("config {cfg:?}\n{}", sim.log.join("\n"));
    if let Err(x) = r {
        panic!("seed {seed}: {x}");
    }
}
