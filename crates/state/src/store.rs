//! `state.redb`: the transactional store (docs/ENGINE.md §7, SPEC §5A/§10).
//!
//! # Write path
//!
//! Producers never touch the database. Each call coalesces into an in-memory batch
//! (keyed, so memory is bounded by the number of distinct keys, not by the call rate) and a
//! writer thread commits it after a 2 s debounce, in one transaction. [`Store::flush_now`]
//! forces a commit and waits for it. The tracker's critical write (`completed_sent`) bypasses
//! the batch: `TimingHandle::save_durable` (the `TimingStore::save` of [`crate::timing::TimingHandle`]) commits its own transaction before it returns.
//!
//! A failed commit (disk full, I/O error) keeps the batch, merges newer calls into it and retries
//! after the next debounce; the error is visible through [`Store::last_write_error`] and
//! [`Store::flush_now`].
//!
//! # Reads
//!
//! Read methods first flush pending writes, so they always see the caller's own earlier writes.
//! They are meant for startup and for UI selections, not for per-packet paths.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use redb::{Database, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};

use crate::error::{DecodeError, StateError};
use crate::fsutil::{atomic_write, unique_stamped};
use crate::record::{Counters, ResumeData, TorrentRecord, decode_peers, encode_peers};
use crate::stats::{Day, StatsBucket, StatsScope, StatsSummary};

/// 20-byte info-hash (v1, or the truncated v2 hash).
pub type InfoHash = [u8; 20];

pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub(crate) const TORRENTS: TableDefinition<InfoHash, &[u8]> = TableDefinition::new("torrents");
pub(crate) const RESUME: TableDefinition<InfoHash, &[u8]> = TableDefinition::new("resume");
pub(crate) const COUNTERS: TableDefinition<InfoHash, &[u8]> = TableDefinition::new("counters");
pub(crate) const TIMING: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tracker_timing");
pub(crate) const STATS_DAILY: TableDefinition<(&str, u32), &[u8]> = TableDefinition::new("stats_daily");
pub(crate) const STATS_TOTALS: TableDefinition<&str, &[u8]> = TableDefinition::new("stats_totals");
pub(crate) const PEERS: TableDefinition<InfoHash, &[u8]> = TableDefinition::new("peers_cache");
pub(crate) const DHT: TableDefinition<&str, &[u8]> = TableDefinition::new("dht_table");
pub(crate) const ROOTS: TableDefinition<u32, &str> = TableDefinition::new("save_roots");

const SCHEMA_KEY: &str = "schema";
const DHT_KEY: &str = "table";
/// Most distinct save roots. Roots are interned folders; hundreds would already be odd.
pub const MAX_ROOTS: usize = 4096;
/// Largest DHT table blob (nodes are ~26 bytes; 1 MiB is ~40k nodes).
pub const MAX_DHT_BLOB: usize = 1 << 20;
/// Largest `.torrent` accepted.
pub const MAX_TORRENT_FILE: usize = 16 << 20;
/// Longest series a stats query will build.
pub const MAX_SERIES_DAYS: u32 = 3660;

// ------------------------------------------------------------------ schema & migrations

/// One forward migration step, run inside the upgrade transaction.
pub struct Migration {
    /// Version this step upgrades from.
    pub from: u32,
    /// Version it produces (`from + 1`).
    pub to: u32,
    /// Name shown in errors.
    pub name: &'static str,
    /// The work. Any error aborts the whole upgrade and leaves the database unchanged.
    pub run: fn(&WriteTransaction) -> Result<(), StateError>,
}

/// Schema description: the current version and the steps that lead to it.
pub struct Schema {
    /// Version this build writes and understands.
    pub current: u32,
    /// Steps `1 -> 2`, `2 -> 3`, ... covering every older version still in the field.
    pub migrations: &'static [Migration],
}

/// The schema of this build. Version 1 is the initial layout; add steps here when it changes.
///
/// Version 2 (SPEC §7A.4/§7A.6): torrent records gained `stage_root` and two flag bits. Version 1
/// records decode as they are (the new parts default), so the step rewrites nothing; bumping the
/// schema makes the upgrade take its backup (`backups/state.redb.v1.<stamp>`) and stops older builds
/// from opening a file whose records they would misread.
pub static SCHEMA: Schema = Schema {
    current: 2,
    migrations: &[Migration { from: 1, to: 2, name: "torrent records v2 (stage root)", run: |_| Ok(()) }],
};

/// How to open the store.
#[derive(Clone)]
pub struct OpenOptions {
    /// Write debounce (SPEC: 2 s).
    pub debounce: Duration,
    /// redb page cache. redb's default is 1 GiB, far too much for a menu bar app.
    pub cache_bytes: usize,
    /// Schema to enforce (tests substitute their own).
    pub schema: &'static Schema,
    /// Verify page checksums of the whole file at open (cost grows with the file; see README).
    pub verify: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions { debounce: Duration::from_secs(2), cache_bytes: 4 << 20, schema: &SCHEMA, verify: true }
    }
}

/// A damaged database file that was set aside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// Where the damaged file now lives.
    pub moved_to: PathBuf,
    /// What was wrong with it.
    pub reason: String,
}

/// A schema upgrade that ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migrated {
    /// Version before.
    pub from: u32,
    /// Version after.
    pub to: u32,
    /// Copy of the old file taken before the upgrade.
    pub backup: PathBuf,
}

/// What happened while opening.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// A new database was created (first launch, or after a recovery).
    pub created: bool,
    /// The old database was corrupt, moved aside; this one is empty. The engine should rebuild
    /// torrents from `torrents/*.torrent` ([`Store::list_torrent_files`]).
    pub recovered: Option<Recovery>,
    /// A schema migration ran.
    pub migrated: Option<Migrated>,
    /// The file was written by Seedwell 0.1 (redb 2) and was converted; this is the copy of the
    /// original taken first.
    pub converted: Option<PathBuf>,
    /// Schema version of the open database.
    pub schema_version: u32,
}

/// The opened store and its report.
pub struct Opened {
    /// The store.
    pub store: Store,
    /// What happened.
    pub report: OpenReport,
}

// ------------------------------------------------------------------ batch

enum Op {
    Put(Vec<u8>),
    Del,
}

#[derive(Default)]
struct CounterOp {
    /// Replace the stored row instead of adding to it (torrent removed).
    reset: bool,
    add: Counters,
    at_least: Counters,
}

#[derive(Default)]
struct Batch {
    torrents: HashMap<InfoHash, Op>,
    resume: HashMap<InfoHash, Op>,
    peers: HashMap<InfoHash, Op>,
    counters: HashMap<InfoHash, CounterOp>,
    roots: BTreeMap<u32, String>,
    stats: HashMap<(String, u32), StatsBucket>,
    dht: Option<Vec<u8>>,
    purge_timing: HashSet<InfoHash>,
    delete_files: HashSet<InfoHash>,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.torrents.is_empty()
            && self.resume.is_empty()
            && self.peers.is_empty()
            && self.counters.is_empty()
            && self.roots.is_empty()
            && self.stats.is_empty()
            && self.dht.is_none()
            && self.purge_timing.is_empty()
            && self.delete_files.is_empty()
    }

    /// Fold `newer` (calls made after `self` was taken) into `self` after a failed commit.
    fn merge(mut self, newer: Batch) -> Batch {
        self.torrents.extend(newer.torrents);
        self.resume.extend(newer.resume);
        self.peers.extend(newer.peers);
        for (k, n) in newer.counters {
            match self.counters.get_mut(&k) {
                Some(o) if !n.reset => {
                    o.add.add(&n.add);
                    o.at_least = max_counters(o.at_least, n.at_least);
                }
                _ => {
                    self.counters.insert(k, n);
                }
            }
        }
        self.roots.extend(newer.roots);
        for (k, v) in newer.stats {
            self.stats.entry(k).or_default().add(&v);
        }
        if newer.dht.is_some() {
            self.dht = newer.dht;
        }
        self.purge_timing.extend(newer.purge_timing);
        self.delete_files.extend(newer.delete_files);
        self
    }
}

fn max_counters(a: Counters, b: Counters) -> Counters {
    Counters {
        downloaded: a.downloaded.max(b.downloaded),
        uploaded: a.uploaded.max(b.uploaded),
        seed_secs: a.seed_secs.max(b.seed_secs),
    }
}

// ------------------------------------------------------------------ shared state

struct State {
    batch: Batch,
    /// Count of calls ever accepted.
    seq: u64,
    /// Highest `seq` known to be in the database.
    committed: u64,
    first_dirty: Option<Instant>,
    flush_requested: bool,
    shutdown: bool,
    writer_alive: bool,
    fail_count: u64,
    last_error: Option<String>,
    roots_by_path: HashMap<String, u32>,
    roots_by_id: HashMap<u32, String>,
    next_root: u32,
    /// `.torrent` files written since the writer last took a batch. A commit that removed one of
    /// these torrents must not delete its file: it belongs to the re-add.
    rewritten: HashSet<InfoHash>,
    /// Open [`WriteGroup`]s: the debounced commit waits until there are none.
    groups: u32,
}

pub(crate) struct Shared {
    pub(crate) db: Database,
    dir: PathBuf,
    debounce: Duration,
    state: Mutex<State>,
    wake: Condvar,
    done: Condvar,
    /// Fault injection for tests: number of upcoming commits that fail.
    fail_next: AtomicU32,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means a thread panicked while holding it; the data is still a valid
        // batch, so keep going rather than turning one bug into a permanent outage.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn torrents_dir(&self) -> PathBuf {
        self.dir.join("torrents")
    }
}

/// The state database. Cheap to share by reference; all methods take `&self`.
pub struct Store {
    pub(crate) shared: Arc<Shared>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// Writes that commit together; see [`Store::group`].
#[must_use = "the group ends when the guard is dropped"]
pub struct WriteGroup<'a>(&'a Store);

impl Drop for WriteGroup<'_> {
    fn drop(&mut self) {
        let mut st = self.0.shared.lock();
        st.groups -= 1;
        if st.groups == 0 {
            self.0.shared.wake.notify_one();
        }
    }
}

/// Torrent records read at startup.
#[derive(Debug, Default)]
pub struct LoadedTorrents {
    /// Valid records.
    pub records: Vec<(InfoHash, TorrentRecord)>,
    /// Records that failed to decode. They are left in place and reported, never deleted.
    pub skipped: Vec<(InfoHash, DecodeError)>,
}

/// Lower-case hex of an info-hash (used for file names).
pub fn hex20(id: &InfoHash) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ open

pub(crate) enum OpenFail {
    /// The file is damaged; moving it aside and starting fresh is safe.
    Corrupt(String),
    /// The file uses an older redb file format (Seedwell 0.1); convert it ([`crate::legacy`]).
    OldFormat(u8),
    /// Anything else: report and leave the file alone.
    Fatal(StateError),
}

impl From<redb::Error> for OpenFail {
    fn from(e: redb::Error) -> Self {
        match e {
            redb::Error::Corrupted(m) => OpenFail::Corrupt(m),
            redb::Error::UpgradeRequired(v) => OpenFail::OldFormat(v),
            redb::Error::Io(io)
                if matches!(io.kind(), io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof) =>
            {
                OpenFail::Corrupt(io.to_string())
            }
            other => OpenFail::Fatal(StateError::Db(Box::new(other))),
        }
    }
}

impl From<StateError> for OpenFail {
    fn from(e: StateError) -> Self {
        match e {
            StateError::Db(inner) => OpenFail::from(*inner),
            other => OpenFail::Fatal(other),
        }
    }
}

pub(crate) fn io_err(context: &'static str) -> impl FnOnce(io::Error) -> StateError {
    move |source| StateError::Io { context, source }
}

/// Run `f`, turning a panic into an error. redb (2.6 for the conversion, and 4) contains `assert!`s and unchecked slice
/// indexing on structures read from the file, so a damaged file can panic instead of returning
/// `Corrupted`. With `panic = "unwind"` this converts it; with `panic = "abort"` (the release
/// profile) the opening-marker protocol in [`Store::open_with`] takes over.
pub(crate) fn guarded<T>(f: impl FnOnce() -> Result<T, OpenFail>) -> Result<T, OpenFail> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(OpenFail::Corrupt(format!("the database engine failed on this file: {msg}")))
        }
    }
}

fn open_database(path: &Path, opts: &OpenOptions) -> Result<Database, OpenFail> {
    guarded(|| {
        let mut db = redb::Builder::new()
            .set_cache_size(opts.cache_bytes)
            .create(path)
            .map_err(|e| OpenFail::from(redb::Error::from(e)))?;
        if opts.verify {
            // Walks every page and checks checksums: surfaces latent damage now, inside the
            // guarded section, instead of as a surprise on some later read.
            db.check_integrity().map_err(|e| OpenFail::from(redb::Error::from(e)))?;
        }
        Ok(db)
    })
}

fn tables_missing(rtx: &ReadTransaction) -> bool {
    rtx.open_table(TORRENTS).is_err()
        || rtx.open_table(RESUME).is_err()
        || rtx.open_table(COUNTERS).is_err()
        || rtx.open_table(TIMING).is_err()
        || rtx.open_table(STATS_DAILY).is_err()
        || rtx.open_table(STATS_TOTALS).is_err()
        || rtx.open_table(PEERS).is_err()
        || rtx.open_table(DHT).is_err()
        || rtx.open_table(ROOTS).is_err()
}

fn create_tables(w: &WriteTransaction) -> Result<(), StateError> {
    w.open_table(TORRENTS)?;
    w.open_table(RESUME)?;
    w.open_table(COUNTERS)?;
    w.open_table(TIMING)?;
    w.open_table(STATS_DAILY)?;
    w.open_table(STATS_TOTALS)?;
    w.open_table(PEERS)?;
    w.open_table(DHT)?;
    w.open_table(ROOTS)?;
    w.open_table(META)?;
    Ok(())
}

fn read_schema_version(rtx: &ReadTransaction) -> Result<Option<u32>, OpenFail> {
    let t = match rtx.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(redb::Error::from(e).into()),
    };
    let Some(v) = t.get(SCHEMA_KEY).map_err(redb::Error::from)? else {
        return Err(OpenFail::Corrupt("meta table has no schema version".into()));
    };
    let b = v.value();
    let arr: [u8; 4] = b.try_into().map_err(|_| OpenFail::Corrupt("schema version record has the wrong size".into()))?;
    Ok(Some(u32::from_le_bytes(arr)))
}

struct Prepared {
    db: Database,
    created: bool,
    migrated: Option<Migrated>,
    version: u32,
}

/// Stored schema version of an existing database, or `None` if it has no meta table yet.
fn stored_version(db: &Database) -> Result<Option<u32>, OpenFail> {
    guarded(|| {
        let rtx = db.begin_read().map_err(redb::Error::from)?;
        read_schema_version(&rtx)
    })
}

fn write_version(w: &WriteTransaction, v: u32) -> Result<(), OpenFail> {
    w.open_table(META)
        .map_err(redb::Error::from)?
        .insert(SCHEMA_KEY, v.to_le_bytes().as_slice())
        .map_err(redb::Error::from)?;
    Ok(())
}

fn open_and_prepare(dir: &Path, path: &Path, opts: &OpenOptions) -> Result<Prepared, OpenFail> {
    let mut db = open_database(path, opts)?;
    let current = opts.schema.current;
    let mut created = false;
    let mut migrated = None;
    match stored_version(&db)? {
        None => {
            guarded(|| {
                let w = db.begin_write().map_err(redb::Error::from)?;
                create_tables(&w)?;
                write_version(&w, current)?;
                w.commit().map_err(redb::Error::from)?;
                Ok(())
            })?;
            created = true;
        }
        Some(v) if v > current => {
            return Err(OpenFail::Fatal(StateError::NewerSchema { found: v, supported: current, path: path.to_path_buf() }));
        }
        Some(v) if v < current => {
            // Close first so the copy is of a cleanly closed file, then copy, then reopen.
            drop(db);
            let backups = dir.join("backups");
            fs::create_dir_all(&backups).map_err(io_err("create backups directory"))?;
            let backup = unique_stamped(&backups, &format!("state.redb.v{v}."), "");
            fs::copy(path, &backup).map_err(io_err("back up the database before migrating"))?;
            if let Ok(f) = fs::File::open(&backup) {
                let _ = f.sync_all();
            }
            db = open_database(path, opts)?;
            let backup2 = backup.clone();
            guarded(|| {
                let w = db.begin_write().map_err(redb::Error::from)?;
                let mut at = v;
                while at < current {
                    let step = opts
                        .schema
                        .migrations
                        .iter()
                        .find(|m| m.from == at)
                        .ok_or(OpenFail::Fatal(StateError::NoMigration { from: at }))?;
                    (step.run)(&w).map_err(|e| {
                        OpenFail::Fatal(StateError::Migration { name: step.name, reason: e.to_string(), backup: backup2.clone() })
                    })?;
                    at = step.to;
                }
                create_tables(&w)?;
                write_version(&w, current)?;
                w.commit().map_err(redb::Error::from)?;
                Ok(())
            })?;
            migrated = Some(Migrated { from: v, to: current, backup });
        }
        Some(_) => {
            guarded(|| {
                let rtx = db.begin_read().map_err(redb::Error::from)?;
                let missing = tables_missing(&rtx);
                drop(rtx);
                if missing {
                    let w = db.begin_write().map_err(redb::Error::from)?;
                    create_tables(&w)?;
                    w.commit().map_err(redb::Error::from)?;
                }
                Ok(())
            })?;
        }
    }
    Ok(Prepared { db, created, migrated, version: current })
}

/// Create an empty, fully initialised database at `path` via a temp file and an atomic rename.
fn init_atomically(dir: &Path, path: &Path, opts: &OpenOptions) -> Result<(), StateError> {
    let tmp = dir.join("state.redb.new");
    let _ = fs::remove_file(&tmp);
    let mut init_opts = opts.clone();
    init_opts.verify = false; // nothing to verify in an empty file
    match open_and_prepare(dir, &tmp, &init_opts) {
        Ok(p) => drop(p.db),
        Err(OpenFail::Fatal(e)) => return Err(e),
        Err(OpenFail::Corrupt(m)) => return Err(StateError::Corrupt(m)),
        Err(OpenFail::OldFormat(v)) => return Err(StateError::Legacy(format!("a new database has file format {v}"))),
    }
    fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(io_err("sync the new database"))?;
    fs::rename(&tmp, path).map_err(io_err("move the new database into place"))?;
    crate::fsutil::sync_dir(dir).map_err(io_err("sync the data directory"))
}

impl Store {
    /// Open (or create) `dir/state.redb` with default options.
    pub fn open(dir: impl AsRef<Path>) -> Result<Opened, StateError> {
        Self::open_with(dir, &OpenOptions::default())
    }

    /// Open with explicit options.
    ///
    /// * A database written by a newer schema is refused with [`StateError::NewerSchema`] and not touched.
    /// * An older one is copied to `backups/` and migrated in a single transaction.
    /// * One written by Seedwell 0.1 (redb 2) is copied to `backups/` and converted first.
    /// * A damaged one is moved to `state.redb.corrupt-<time>` and a fresh one is created; the
    ///   report says so. Other failures (permissions, already open by another process) never
    ///   move anything.
    pub fn open_with(dir: impl AsRef<Path>, opts: &OpenOptions) -> Result<Opened, StateError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir.join("torrents")).map_err(io_err("create the data directory"))?;
        let path = dir.join("state.redb");
        let marker = dir.join("state.redb.opening");
        let mut recovered = None;

        // Crash-loop protection: if opening the file aborted the process (a panic with
        // `panic = "abort"`, or a kill during a long repair), the marker survives. One such
        // crash is tolerated (retry); two in a row mean this file kills us, so set it aside.
        let attempts = fs::read_to_string(&marker).ok().and_then(|t| t.trim().parse::<u32>().ok()).unwrap_or(0);
        if attempts >= 2 && path.exists() {
            let moved_to = unique_stamped(dir, "state.redb.corrupt-", "");
            fs::rename(&path, &moved_to).map_err(io_err("set aside a database that crashed the previous launches"))?;
            recovered = Some(Recovery {
                moved_to,
                reason: format!("opening it crashed the previous {attempts} launches"),
            });
            let _ = fs::remove_file(&marker);
        } else if path.exists() {
            atomic_write(&marker, (attempts + 1).to_string().as_bytes()).map_err(io_err("write the opening marker"))?;
        }

        let mut fresh = false;
        let mut converted = None;
        let prepared = loop {
            // A brand-new database is built under a temporary name and renamed into place, so a
            // crash during the very first run can never leave a half-initialised `state.redb`.
            if fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true) {
                init_atomically(dir, &path, opts)?;
                fresh = true;
            }
            match open_and_prepare(dir, &path, opts) {
                Ok(p) => {
                    let _ = fs::remove_file(&marker);
                    break p;
                }
                Err(OpenFail::Fatal(e)) => {
                    let _ = fs::remove_file(&marker);
                    return Err(e);
                }
                Err(OpenFail::OldFormat(_)) if converted.is_none() => match crate::legacy::convert(dir, &path, opts) {
                    Ok(backup) => converted = Some(backup),
                    Err(OpenFail::Corrupt(reason)) => {
                        // Damaged as well as old: set it aside like any damaged file (the backup
                        // taken before the attempt stays too).
                        let _ = fs::remove_file(&marker);
                        let moved_to = unique_stamped(dir, "state.redb.corrupt-", "");
                        fs::rename(&path, &moved_to).map_err(|e| {
                            StateError::Corrupt(format!("{reason}; and the damaged file could not be moved aside: {e}"))
                        })?;
                        recovered = Some(Recovery { moved_to, reason });
                    }
                    Err(OpenFail::Fatal(e)) => {
                        let _ = fs::remove_file(&marker);
                        return Err(e);
                    }
                    Err(OpenFail::OldFormat(v)) => {
                        let _ = fs::remove_file(&marker);
                        return Err(StateError::Legacy(format!("unexpected file format {v} during conversion")));
                    }
                },
                Err(OpenFail::OldFormat(v)) => {
                    let _ = fs::remove_file(&marker);
                    return Err(StateError::Legacy(format!("still file format {v} after conversion")));
                }
                Err(OpenFail::Corrupt(reason)) => {
                    let _ = fs::remove_file(&marker);
                    if recovered.is_some() {
                        return Err(StateError::Corrupt(reason));
                    }
                    let moved_to = unique_stamped(dir, "state.redb.corrupt-", "");
                    fs::rename(&path, &moved_to).map_err(|e| {
                        StateError::Corrupt(format!("{reason}; and the damaged file could not be moved aside: {e}"))
                    })?;
                    recovered = Some(Recovery { moved_to, reason });
                }
            }
        };
        let Prepared { db, created, migrated, version } = prepared;

        let mut roots_by_path = HashMap::new();
        let mut roots_by_id = HashMap::new();
        let mut next_root = 1;
        {
            let rtx = db.begin_read().map_err(redb::Error::from)?;
            let t = rtx.open_table(ROOTS).map_err(redb::Error::from)?;
            for row in t.iter().map_err(redb::Error::from)? {
                let (k, v) = row.map_err(redb::Error::from)?;
                let (id, p) = (k.value(), v.value().to_string());
                next_root = next_root.max(id.saturating_add(1));
                roots_by_path.insert(p.clone(), id);
                roots_by_id.insert(id, p);
            }
        }

        let shared = Arc::new(Shared {
            db,
            dir: dir.to_path_buf(),
            debounce: opts.debounce,
            state: Mutex::new(State {
                batch: Batch::default(),
                seq: 0,
                committed: 0,
                first_dirty: None,
                flush_requested: false,
                shutdown: false,
                writer_alive: true,
                fail_count: 0,
                last_error: None,
                roots_by_path,
                roots_by_id,
                next_root,
                rewritten: HashSet::new(),
                groups: 0,
            }),
            wake: Condvar::new(),
            done: Condvar::new(),
            fail_next: AtomicU32::new(0),
        });
        let s2 = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("trs-state-writer".into())
            .spawn(move || writer_loop(&s2))
            .map_err(io_err("start the writer thread"))?;
        let report = OpenReport {
            created: created || fresh,
            recovered,
            migrated,
            converted,
            schema_version: version,
        };
        Ok(Opened { store: Store { shared, writer: Mutex::new(Some(handle)) }, report })
    }

    /// Full integrity check of a database file that is not open (slow; for diagnostics and tests).
    pub fn check_file(path: impl AsRef<Path>) -> Result<bool, StateError> {
        let mut db = Database::open(path.as_ref()).map_err(redb::Error::from)?;
        Ok(db.check_integrity().map_err(redb::Error::from)?)
    }

    /// The data directory.
    pub fn dir(&self) -> &Path {
        &self.shared.dir
    }

    // -------------------------------------------------------------- plumbing

    fn submit(&self, f: impl FnOnce(&mut State)) -> Result<(), StateError> {
        let mut st = self.shared.lock();
        if st.shutdown || !st.writer_alive {
            return Err(StateError::Closed);
        }
        f(&mut st);
        st.seq += 1;
        if st.first_dirty.is_none() {
            st.first_dirty = Some(Instant::now());
            self.shared.wake.notify_one();
        }
        Ok(())
    }

    /// Group the writes made while the returned guard lives: the debounced commit waits for the
    /// guard, so they reach the database in one transaction (writes that belong together, such as
    /// a torrent's counters and the statistics of the same transfer, never land apart in a crash).
    /// An explicit flush still commits at once: [`Store::flush_now`] and every read, so a group
    /// that reads is split there. Keep groups short; the debounce resumes when the last one ends.
    pub fn group(&self) -> WriteGroup<'_> {
        self.shared.lock().groups += 1;
        WriteGroup(self)
    }

    /// Commit everything accepted so far and wait for it (SPEC: "explicit flush").
    pub fn flush_now(&self) -> Result<(), StateError> {
        let mut st = self.shared.lock();
        let target = st.seq;
        if st.committed >= target {
            return Ok(());
        }
        let fails = st.fail_count;
        st.flush_requested = true;
        self.shared.wake.notify_one();
        loop {
            if st.committed >= target {
                return Ok(());
            }
            if st.fail_count != fails {
                return Err(StateError::WriteFailed(st.last_error.clone().unwrap_or_default()));
            }
            if !st.writer_alive {
                return Err(StateError::Closed);
            }
            st = self.shared.done.wait_timeout(st, Duration::from_millis(500)).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Test hook: make the next `n` background commits fail as if the disk were full.
    #[doc(hidden)]
    pub fn __inject_commit_failures(&self, n: u32) {
        self.shared.fail_next.store(n, Ordering::SeqCst);
    }

    /// The last background commit failure, if the most recent attempt failed.
    pub fn last_write_error(&self) -> Option<String> {
        self.shared.lock().last_error.clone()
    }

    /// Calls accepted but not yet committed (diagnostics).
    pub fn pending_calls(&self) -> u64 {
        let st = self.shared.lock();
        st.seq - st.committed
    }

    fn read(&self) -> Result<ReadTransaction, StateError> {
        let _ = self.flush_now(); // best effort: a failing disk must not blind the reads
        Ok(self.shared.db.begin_read().map_err(redb::Error::from)?)
    }

    /// Flush, stop the writer and release the file. Returns the final flush result.
    pub fn close(self) -> Result<(), StateError> {
        let r = self.flush_now();
        drop(self);
        r
    }

    // -------------------------------------------------------------- torrents

    /// Insert or replace a torrent record. The `.torrent` file should be written first with [`Store::put_torrent_file`].
    pub fn put_torrent(&self, id: InfoHash, rec: &TorrentRecord) -> Result<(), StateError> {
        let bytes = rec.encode();
        self.submit(|st| {
            // A re-add right after a removal must not lose its file or timing rows.
            st.batch.delete_files.remove(&id);
            st.batch.purge_timing.remove(&id);
            st.batch.torrents.insert(id, Op::Put(bytes));
        })
    }

    /// Remove a torrent and everything keyed by it: record, resume data, counters, cached peers,
    /// tracker timing, and the stored `.torrent` file (after the commit). Statistics history stays.
    pub fn remove_torrent(&self, id: InfoHash) -> Result<(), StateError> {
        self.submit(|st| {
            let b = &mut st.batch;
            b.torrents.insert(id, Op::Del);
            b.resume.insert(id, Op::Del);
            b.peers.insert(id, Op::Del);
            b.counters.insert(id, CounterOp { reset: true, ..CounterOp::default() });
            b.purge_timing.insert(id);
            b.delete_files.insert(id);
            st.rewritten.remove(&id);
        })
    }

    /// Load every torrent record. Records that fail to decode are reported in `skipped`.
    pub fn load_torrents(&self) -> Result<LoadedTorrents, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(TORRENTS).map_err(redb::Error::from)?;
        let mut out = LoadedTorrents::default();
        for row in t.iter().map_err(redb::Error::from)? {
            let (k, v) = row.map_err(redb::Error::from)?;
            match TorrentRecord::decode(v.value()) {
                Ok(r) => out.records.push((k.value(), r)),
                Err(e) => out.skipped.push((k.value(), e)),
            }
        }
        Ok(out)
    }

    /// One torrent record.
    pub fn get_torrent(&self, id: InfoHash) -> Result<Option<TorrentRecord>, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(TORRENTS).map_err(redb::Error::from)?;
        let Some(v) = t.get(id).map_err(redb::Error::from)? else { return Ok(None) };
        TorrentRecord::decode(v.value()).map(Some).map_err(|e| StateError::Corrupt(e.to_string()))
    }

    // -------------------------------------------------------------- .torrent files

    /// Path of the stored `.torrent` for `id`.
    pub fn torrent_file_path(&self, id: &InfoHash) -> PathBuf {
        self.shared.torrents_dir().join(format!("{}.torrent", hex20(id)))
    }

    /// Store the original `.torrent` bytes atomically (immediately, not batched).
    pub fn put_torrent_file(&self, id: &InfoHash, bytes: &[u8]) -> Result<(), StateError> {
        if bytes.len() > MAX_TORRENT_FILE {
            return Err(StateError::TooLarge { what: ".torrent file", size: bytes.len(), max: MAX_TORRENT_FILE });
        }
        {
            // Claim the file before writing it; the writer deletes removed files under the same lock.
            let mut st = self.shared.lock();
            st.batch.delete_files.remove(id);
            st.rewritten.insert(*id);
        }
        atomic_write(&self.torrent_file_path(id), bytes).map_err(io_err("write the .torrent file"))
    }

    /// Read the stored `.torrent` bytes.
    pub fn read_torrent_file(&self, id: &InfoHash) -> Result<Vec<u8>, StateError> {
        let p = self.torrent_file_path(id);
        let len = fs::metadata(&p).map_err(io_err("stat the .torrent file"))?.len();
        if len > MAX_TORRENT_FILE as u64 {
            return Err(StateError::TooLarge { what: ".torrent file", size: len as usize, max: MAX_TORRENT_FILE });
        }
        fs::read(&p).map_err(io_err("read the .torrent file"))
    }

    /// Info-hashes of the `.torrent` files on disk, for rebuilding after a database recovery.
    pub fn list_torrent_files(&self) -> Result<Vec<InfoHash>, StateError> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.shared.torrents_dir()).map_err(io_err("list torrents/"))?.flatten() {
            let name = e.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".torrent")) else { continue };
            if stem.len() != 40 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let mut id = [0u8; 20];
            for (i, slot) in id.iter_mut().enumerate() {
                *slot = u8::from_str_radix(&stem[i * 2..i * 2 + 2], 16).unwrap_or(0);
            }
            out.push(id);
        }
        out.sort_unstable();
        Ok(out)
    }

    // -------------------------------------------------------------- resume

    /// Store resume data for an incomplete torrent.
    pub fn put_resume(&self, id: InfoHash, data: &ResumeData) -> Result<(), StateError> {
        let bytes = data.encode().map_err(|e| StateError::Corrupt(e.to_string()))?;
        self.submit(|st| {
            st.batch.resume.insert(id, Op::Put(bytes));
        })
    }

    /// Drop resume data (the torrent completed).
    pub fn remove_resume(&self, id: InfoHash) -> Result<(), StateError> {
        self.submit(|st| {
            st.batch.resume.insert(id, Op::Del);
        })
    }

    /// Read resume data. A record that fails to decode is an error the caller answers with a recheck.
    pub fn get_resume(&self, id: InfoHash) -> Result<Option<ResumeData>, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(RESUME).map_err(redb::Error::from)?;
        let Some(v) = t.get(id).map_err(redb::Error::from)? else { return Ok(None) };
        ResumeData::decode(v.value()).map(Some).map_err(|e| StateError::Corrupt(e.to_string()))
    }

    // -------------------------------------------------------------- counters

    /// Add to a torrent's lifetime counters. They can only grow.
    pub fn add_counters(&self, id: InfoHash, delta: Counters) -> Result<(), StateError> {
        self.submit(|st| {
            let e = st.batch.counters.entry(id).or_default();
            e.add.add(&delta);
        })
    }

    /// Raise the stored counters to at least `value` (field-wise max). Convenient for an engine
    /// that keeps absolute lifetime counters in memory. Do not mix with [`Store::add_counters`]
    /// for the same torrent within one debounce window.
    pub fn set_counters_at_least(&self, id: InfoHash, value: Counters) -> Result<(), StateError> {
        self.submit(|st| {
            let e = st.batch.counters.entry(id).or_default();
            e.at_least = max_counters(e.at_least, value);
        })
    }

    /// One torrent's lifetime counters (zero if none).
    pub fn get_counters(&self, id: InfoHash) -> Result<Counters, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(COUNTERS).map_err(redb::Error::from)?;
        let Some(v) = t.get(id).map_err(redb::Error::from)? else { return Ok(Counters::default()) };
        Counters::decode(v.value()).map_err(|e| StateError::Corrupt(e.to_string()))
    }

    /// All counters (startup).
    pub fn load_counters(&self) -> Result<Vec<(InfoHash, Counters)>, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(COUNTERS).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(redb::Error::from)? {
            let (k, v) = row.map_err(redb::Error::from)?;
            if let Ok(c) = Counters::decode(v.value()) {
                out.push((k.value(), c));
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------- roots

    /// Intern a save-root folder and return its small id (stable across restarts).
    pub fn intern_root(&self, path: &str) -> Result<u32, StateError> {
        let mut st = self.shared.lock();
        if st.shutdown || !st.writer_alive {
            return Err(StateError::Closed);
        }
        if let Some(id) = st.roots_by_path.get(path) {
            return Ok(*id);
        }
        if st.roots_by_path.len() >= MAX_ROOTS {
            return Err(StateError::TooLarge { what: "number of save roots", size: st.roots_by_path.len(), max: MAX_ROOTS });
        }
        if path.len() > crate::record::MAX_REL_PATH {
            return Err(StateError::TooLarge { what: "save root path", size: path.len(), max: crate::record::MAX_REL_PATH });
        }
        let id = st.next_root;
        st.next_root += 1;
        st.roots_by_path.insert(path.to_string(), id);
        st.roots_by_id.insert(id, path.to_string());
        st.batch.roots.insert(id, path.to_string());
        st.seq += 1;
        if st.first_dirty.is_none() {
            st.first_dirty = Some(Instant::now());
            self.shared.wake.notify_one();
        }
        Ok(id)
    }

    /// The path of an interned root.
    pub fn root_path(&self, id: u32) -> Option<String> {
        self.shared.lock().roots_by_id.get(&id).cloned()
    }

    // -------------------------------------------------------------- peers & dht

    /// Remember a bounded list of good peers for a quick restart (at most 256 are kept).
    pub fn put_peers(&self, id: InfoHash, peers: &[SocketAddr]) -> Result<(), StateError> {
        let bytes = encode_peers(peers);
        self.submit(|st| {
            st.batch.peers.insert(id, Op::Put(bytes));
        })
    }

    /// Cached peers of a torrent.
    pub fn get_peers(&self, id: InfoHash) -> Result<Vec<SocketAddr>, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(PEERS).map_err(redb::Error::from)?;
        let Some(v) = t.get(id).map_err(redb::Error::from)? else { return Ok(Vec::new()) };
        // A damaged cache entry is only a cache miss.
        Ok(decode_peers(v.value()).unwrap_or_default())
    }

    /// Store the DHT routing table (an opaque blob owned by the DHT crate, at most 1 MiB).
    pub fn put_dht_table(&self, blob: &[u8]) -> Result<(), StateError> {
        if blob.len() > MAX_DHT_BLOB {
            return Err(StateError::TooLarge { what: "DHT table", size: blob.len(), max: MAX_DHT_BLOB });
        }
        let v = blob.to_vec();
        self.submit(|st| {
            st.batch.dht = Some(v);
        })
    }

    /// The stored DHT table.
    pub fn get_dht_table(&self) -> Result<Option<Vec<u8>>, StateError> {
        let rtx = self.read()?;
        let t = rtx.open_table(DHT).map_err(redb::Error::from)?;
        Ok(t.get(DHT_KEY).map_err(redb::Error::from)?.map(|v| v.value().to_vec()))
    }

    // -------------------------------------------------------------- statistics

    /// Add `delta` to the bucket of `scope` on `day` (and to the lifetime total). Never subtracts.
    pub fn record_stats(&self, scope: &StatsScope, day: Day, delta: &StatsBucket) -> Result<(), StateError> {
        let Some(key) = scope.key() else {
            return Err(StateError::Invalid("`All` is a sum and cannot be written to"));
        };
        if delta.is_zero() {
            return Ok(());
        }
        let key = key.to_string();
        self.submit(|st| {
            st.batch.stats.entry((key, day.0)).or_default().add(delta);
        })
    }

    fn sum_range(
        rtx: &ReadTransaction,
        scope: &StatsScope,
        from: Day,
        to: Day,
    ) -> Result<BTreeMap<u32, StatsBucket>, StateError> {
        let t = rtx.open_table(STATS_DAILY).map_err(redb::Error::from)?;
        let keys: Vec<String> = match scope.key() {
            Some(k) => vec![k.to_string()],
            None => Self::scope_keys(rtx)?,
        };
        let mut out: BTreeMap<u32, StatsBucket> = BTreeMap::new();
        if from > to {
            return Ok(out);
        }
        for k in &keys {
            let range = t.range((k.as_str(), from.0)..=(k.as_str(), to.0)).map_err(redb::Error::from)?;
            for row in range {
                let (kk, v) = row.map_err(redb::Error::from)?;
                if let Ok(b) = StatsBucket::decode(v.value()) {
                    out.entry(kk.value().1).or_default().add(&b);
                }
            }
        }
        Ok(out)
    }

    fn scope_keys(rtx: &ReadTransaction) -> Result<Vec<String>, StateError> {
        let t = rtx.open_table(STATS_TOTALS).map_err(redb::Error::from)?;
        let mut keys = Vec::new();
        for row in t.iter().map_err(redb::Error::from)? {
            keys.push(row.map_err(redb::Error::from)?.0.value().to_string());
        }
        Ok(keys)
    }

    /// Bucket keys that have history (profile ids, `@public`, `@other-private`), including deleted profiles.
    pub fn stats_scopes(&self) -> Result<Vec<String>, StateError> {
        let rtx = self.read()?;
        Self::scope_keys(&rtx)
    }

    /// Daily buckets for the last `days` days ending at `today`, oldest first, one entry per day
    /// (zero buckets for quiet days). At most [`MAX_SERIES_DAYS`].
    pub fn stats_series(&self, scope: &StatsScope, today: Day, days: u32) -> Result<Vec<(Day, StatsBucket)>, StateError> {
        let days = days.min(MAX_SERIES_DAYS);
        if days == 0 {
            return Ok(Vec::new());
        }
        let from = today.minus(days - 1);
        let rtx = self.read()?;
        let map = Self::sum_range(&rtx, scope, from, today)?;
        Ok((from.0..=today.0).map(|d| (Day(d), map.get(&d).copied().unwrap_or_default())).collect())
    }

    /// Lifetime totals.
    pub fn stats_total(&self, scope: &StatsScope) -> Result<StatsBucket, StateError> {
        let rtx = self.read()?;
        Self::total(&rtx, scope)
    }

    fn total(rtx: &ReadTransaction, scope: &StatsScope) -> Result<StatsBucket, StateError> {
        let t = rtx.open_table(STATS_TOTALS).map_err(redb::Error::from)?;
        let mut sum = StatsBucket::default();
        match scope.key() {
            Some(k) => {
                if let Some(v) = t.get(k).map_err(redb::Error::from)?
                    && let Ok(b) = StatsBucket::decode(v.value())
                {
                    sum = b;
                }
            }
            None => {
                for row in t.iter().map_err(redb::Error::from)? {
                    let (_, v) = row.map_err(redb::Error::from)?;
                    if let Ok(b) = StatsBucket::decode(v.value()) {
                        sum.add(&b);
                    }
                }
            }
        }
        Ok(sum)
    }

    /// Today, this week (Monday on), this month, all time.
    pub fn stats_summary(&self, scope: &StatsScope, today: Day) -> Result<StatsSummary, StateError> {
        let rtx = self.read()?;
        let (ws, ms) = (today.week_start(), today.month_start());
        let map = Self::sum_range(&rtx, scope, ws.min(ms), today)?;
        let sum = |from: Day| {
            let mut s = StatsBucket::default();
            for (_, b) in map.range(from.0..=today.0) {
                s.add(b);
            }
            s
        };
        Ok(StatsSummary {
            today: map.get(&today.0).copied().unwrap_or_default(),
            this_week: sum(ws),
            this_month: sum(ms),
            all_time: Self::total(&rtx, scope)?,
        })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        {
            let mut st = self.shared.lock();
            st.shutdown = true;
            self.shared.wake.notify_all();
        }
        let handle = self.writer.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

// ------------------------------------------------------------------ writer thread

fn apply(w: &WriteTransaction, b: &Batch) -> Result<(), StateError> {
    {
        let mut t = w.open_table(TORRENTS)?;
        for (id, op) in &b.torrents {
            match op {
                Op::Put(v) => t.insert(*id, v.as_slice())?,
                Op::Del => t.remove(*id)?,
            };
        }
        let mut t = w.open_table(RESUME)?;
        for (id, op) in &b.resume {
            match op {
                Op::Put(v) => t.insert(*id, v.as_slice())?,
                Op::Del => t.remove(*id)?,
            };
        }
        let mut t = w.open_table(PEERS)?;
        for (id, op) in &b.peers {
            match op {
                Op::Put(v) => t.insert(*id, v.as_slice())?,
                Op::Del => t.remove(*id)?,
            };
        }
    }
    {
        let mut t = w.open_table(COUNTERS)?;
        for (id, op) in &b.counters {
            let mut cur = if op.reset {
                Counters::default()
            } else {
                let existing = t.get(*id)?.map(|v| Counters::decode(v.value()));
                // A damaged row is replaced rather than blocking every future write.
                existing.and_then(Result::ok).unwrap_or_default()
            };
            cur = max_counters(cur, op.at_least);
            cur.add(&op.add);
            if op.reset && cur == Counters::default() {
                t.remove(*id)?;
            } else {
                t.insert(*id, cur.encode().as_slice())?;
            }
        }
    }
    if !b.roots.is_empty() {
        let mut t = w.open_table(ROOTS)?;
        for (id, p) in &b.roots {
            t.insert(*id, p.as_str())?;
        }
    }
    if !b.stats.is_empty() {
        let mut daily = w.open_table(STATS_DAILY)?;
        let mut totals = w.open_table(STATS_TOTALS)?;
        let mut per_profile: HashMap<&str, StatsBucket> = HashMap::new();
        for ((profile, day), delta) in &b.stats {
            let mut cur = daily
                .get((profile.as_str(), *day))?
                .and_then(|v| StatsBucket::decode(v.value()).ok())
                .unwrap_or_default();
            cur.add(delta);
            daily.insert((profile.as_str(), *day), cur.encode().as_slice())?;
            per_profile.entry(profile.as_str()).or_default().add(delta);
        }
        for (profile, delta) in per_profile {
            let mut cur = totals
                .get(profile)?
                .and_then(|v| StatsBucket::decode(v.value()).ok())
                .unwrap_or_default();
            cur.add(&delta);
            totals.insert(profile, cur.encode().as_slice())?;
        }
    }
    if let Some(blob) = &b.dht {
        w.open_table(DHT)?.insert(DHT_KEY, blob.as_slice())?;
    }
    if !b.purge_timing.is_empty() {
        let mut t = w.open_table(TIMING)?;
        for id in &b.purge_timing {
            let mut doomed: Vec<Vec<u8>> = Vec::new();
            for row in t.range(id.as_slice()..)? {
                let (k, _) = row?;
                if !k.value().starts_with(id) {
                    break;
                }
                doomed.push(k.value().to_vec());
            }
            for k in doomed {
                t.remove(k.as_slice())?;
            }
        }
    }
    Ok(())
}

fn commit(shared: &Shared, b: &Batch) -> Result<(), StateError> {
    if shared.fail_next.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
        return Err(StateError::Io { context: "commit", source: io::Error::other("injected commit failure") });
    }
    let w = shared.db.begin_write()?;
    apply(&w, b)?;
    w.commit()?;
    Ok(())
}

struct AliveGuard<'a>(&'a Shared);
impl Drop for AliveGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.writer_alive = false;
        self.0.done.notify_all();
    }
}

fn writer_loop(shared: &Shared) {
    let _alive = AliveGuard(shared);
    loop {
        let (batch, taken) = {
            let mut st = shared.lock();
            loop {
                let have = !st.batch.is_empty();
                if st.shutdown {
                    if have {
                        break;
                    }
                    return;
                }
                if st.flush_requested {
                    if have {
                        break;
                    }
                    st.flush_requested = false;
                    st.committed = st.seq;
                    shared.done.notify_all();
                    continue;
                }
                let deadline = if have { st.first_dirty.map(|t| t + shared.debounce) } else { None };
                match deadline {
                    Some(dl) => {
                        let now = Instant::now();
                        if now >= dl && st.groups == 0 {
                            break;
                        }
                        // Due while a group of writes is open: it commits as one when the group ends.
                        st = if now >= dl {
                            shared.wake.wait(st).unwrap_or_else(|p| p.into_inner())
                        } else {
                            shared.wake.wait_timeout(st, dl - now).unwrap_or_else(|p| p.into_inner()).0
                        };
                    }
                    None => st = shared.wake.wait(st).unwrap_or_else(|p| p.into_inner()),
                }
            }
            st.flush_requested = false;
            st.first_dirty = None;
            st.rewritten.clear();
            (std::mem::take(&mut st.batch), st.seq)
        };

        let result = commit(shared, &batch);

        let mut st = shared.lock();
        match result {
            Ok(()) => {
                st.committed = taken;
                st.last_error = None;
                if !st.batch.is_empty() && st.first_dirty.is_none() {
                    st.first_dirty = Some(Instant::now());
                }
                // Only delete files of torrents that were not re-added since the batch was taken.
                // Under the lock, so a concurrent `put_torrent_file` is either seen here or writes
                // after the delete.
                for id in &batch.delete_files {
                    if !st.rewritten.contains(id) && !st.batch.torrents.contains_key(id) {
                        let _ = fs::remove_file(shared.torrents_dir().join(format!("{}.torrent", hex20(id))));
                    }
                }
                drop(st);
                shared.done.notify_all();
            }
            Err(e) => {
                let mut batch = batch;
                // Files re-added while this commit ran stay, as on success.
                for id in &st.rewritten {
                    batch.delete_files.remove(id);
                }
                let newer = std::mem::take(&mut st.batch);
                st.batch = batch.merge(newer);
                st.fail_count += 1;
                st.last_error = Some(e.to_string());
                st.first_dirty = Some(Instant::now());
                let stop = st.shutdown;
                shared.done.notify_all();
                if stop {
                    return;
                }
            }
        }
    }
}
