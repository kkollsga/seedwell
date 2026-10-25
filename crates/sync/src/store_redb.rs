//! A folder's index in a redb file: [`RedbStore`], the persistent [`IndexStore`].
//!
//! One file per folder (the driver uses `<state>/sync/<folder-id>.redb`). It behaves exactly like
//! [`MemStore`](crate::index::MemStore), the reference semantics; a property test in
//! `tests/it/redb_store.rs` applies the same op sequences to both and compares every read.
//!
//! # Tables
//!
//! | Table | Key | Value |
//! |---|---|---|
//! | `meta` | `"schema"`, `"counters"`, `"need_totals"`, `"local_totals"` | schema version (u32 LE); index id and sequence; needed paths and bytes; live local files and bytes |
//! | `local` | path | local record: the wire entry (`msg::encode_entry`), then the disk stamp and tier memo |
//! | `by_seq` | sequence | path |
//! | `remote` | (path, device) | the device's wire entry |
//! | `remote_by_device` | (device, path) | `()` (so dropping a device reads only its own rows) |
//! | `peers` | device | index id, highest stored sequence |
//! | `writers` | device | public key ‖ certificate (96 bytes) |
//! | `need` | path | need kind, holders, the current version's wire entry |
//! | `chunks` | (hash, path, offset) | `()`: where a valid local file holds a chunk |
//!
//! Records use the wire encoding of `trs-sync-proto` (varints, capped lengths), so the decoder that
//! checks what peers send also checks what comes off the disk. A record that does not decode is an
//! [`io::ErrorKind::InvalidData`] error naming its table and key; nothing panics on a damaged file
//! (redb's own panics on broken pages are caught and returned as errors too).
//!
//! # Durability
//!
//! Every [`apply`](IndexStore::apply) is one write transaction: after a crash the file holds each
//! batch whole or not at all, and the derived tables (`by_seq`, `chunks`, `need`, the totals, the
//! reverse device index) always match the records they come from.
//!
//! * [`Durability::Immediate`] (the default): when `apply` returns, the batch is on disk (redb
//!   fsyncs the commit). A crash or power loss loses nothing that was applied.
//! * [`Durability::Eventual`]: the batch is committed and visible to every read, but not fsynced. A
//!   crash rolls the file back to the last durable commit (an `Immediate` apply or
//!   [`flush`](IndexStore::flush)): every `Eventual` batch since then is lost, each one entirely.
//!   Meant for bursts whose loss is recomputed: a scan (the next scan finds the same changes), remote
//!   index batches (the peer's `max_seq` rolls back with its entries, so they are asked for again).
//!
//! **The puller's guarantee.** The puller keeps the records of finished steps pending, makes the
//! directories they touched durable (`Fs::sync_dir`), then records them in one batch (every 64
//! steps and at the end of a round). A record is never ahead of the disk. A crash before that
//! commit loses only records; the need still points at the version the file now holds (it rolled
//! back with it), and the scanner adopts the finished file as that version instead of making a new
//! one (`scan.rs`). With `Eventual`, a crash can lose several such batches; each is recovered by the
//! same adoption (a pulled delete comes back as a local delete of the same path, which is not a
//! conflict: concurrent tombstones have the same content).
//!
//! **Sequences handed to peers are never reused.** [`IndexStore::local_since`], which is what peers
//! are sent from, first makes every earlier `Eventual` batch durable. Otherwise a crash could give a
//! sequence a peer already stored to a different record, and the peer would never ask for it.
//! Anything else announced from the store must call [`IndexStore::flush`] first.
//!
//! Dropping the store flushes. After a crash redb repairs the file at the next open, walking it
//! once (time grows with the file).
//!
//! # Schema
//!
//! [`SCHEMA`] is version 1. An older file is copied to `<file>.v<N>.<n>.bak` and migrated in one
//! transaction ([`Migration`], the `trs-state` pattern); a file from a newer schema is refused and
//! left untouched.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, Table, TableDefinition, WriteTransaction,
};
use trs_sync_proto::codec::{DecodeError, Reader, put_ivarint, put_varint};
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{FileInfo, IndexEntry, MAX_CHUNK, MAX_COUNTERS, MAX_PATH, SignedFile};
use trs_sync_proto::msg::{MAX_CHUNKS, decode_entry, encode_entry};
use trs_sync_proto::sign::Writer;

use crate::global::Need;
use crate::index::{IndexStore, LocalRecord, Meta, NeedRecord, Op, PeerState, Stamp, TierMemo, counted, offsets};

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const LOCAL: TableDefinition<&str, &[u8]> = TableDefinition::new("local");
const BY_SEQ: TableDefinition<u64, &str> = TableDefinition::new("by_seq");
const REMOTE: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("remote");
const REMOTE_BY_DEVICE: TableDefinition<(u64, &str), ()> = TableDefinition::new("remote_by_device");
const PEERS: TableDefinition<u64, &[u8]> = TableDefinition::new("peers");
const WRITERS: TableDefinition<u64, &[u8]> = TableDefinition::new("writers");
const NEED: TableDefinition<&str, &[u8]> = TableDefinition::new("need");
const CHUNKS: TableDefinition<(&[u8; 32], &str, u64), ()> = TableDefinition::new("chunks");

const SCHEMA_KEY: &str = "schema";
const COUNTERS_KEY: &str = "counters";
const TOTALS_KEY: &str = "need_totals";
const LOCAL_TOTALS_KEY: &str = "local_totals";

/// Most holders one need record may list.
const MAX_HOLDERS: usize = 1 << 16;

// ------------------------------------------------------------------ schema & migrations

/// One forward migration step, run inside the upgrade transaction.
pub struct Migration {
    /// Version this step upgrades from.
    pub from: u32,
    /// Version it produces (`from + 1`).
    pub to: u32,
    /// Name shown in errors.
    pub name: &'static str,
    /// The work. Any error aborts the whole upgrade and leaves the file unchanged.
    pub run: fn(&WriteTransaction) -> io::Result<()>,
}

/// Schema description: the current version and the steps that lead to it.
pub struct Schema {
    /// Version this build writes and understands.
    pub current: u32,
    /// Steps `1 -> 2`, `2 -> 3`, ... covering every older version still in the field.
    pub migrations: &'static [Migration],
}

/// The schema of this build. Version 1 is the initial layout; add steps here when it changes.
pub static SCHEMA: Schema = Schema { current: 1, migrations: &[] };

/// When a committed batch is on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// Fsynced before `apply` returns.
    #[default]
    Immediate,
    /// Committed and readable, but durable only at the next `Immediate` commit or flush.
    Eventual,
}

/// How to open a store.
#[derive(Clone, Copy)]
pub struct OpenOptions {
    /// redb page cache (its default is 1 GiB, far too much for one folder of a menu bar app).
    pub cache_bytes: usize,
    /// Durability of [`IndexStore::apply`] (see [`RedbStore::apply_with`] for a per-batch choice).
    pub durability: Durability,
    /// Verify the checksums of every page at open (cost grows with the file).
    pub verify: bool,
    /// Schema to enforce (tests substitute their own).
    pub schema: &'static Schema,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions { cache_bytes: 16 << 20, durability: Durability::Immediate, verify: false, schema: &SCHEMA }
    }
}

impl fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenOptions")
            .field("cache_bytes", &self.cache_bytes)
            .field("durability", &self.durability)
            .field("verify", &self.verify)
            .field("schema", &self.schema.current)
            .finish()
    }
}

/// A schema upgrade that ran at open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migrated {
    /// Version before.
    pub from: u32,
    /// Version after.
    pub to: u32,
    /// Copy of the file taken before the upgrade.
    pub backup: PathBuf,
}

/// What happened at open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// A new, empty index was created (with the caller's index id).
    pub created: bool,
    /// A schema migration ran.
    pub migrated: Option<Migrated>,
}

/// A folder's index in one redb file. See the module docs for the layout and the durability rules.
pub struct RedbStore {
    db: Database,
    path: PathBuf,
    durability: Durability,
    /// `Eventual` batches were committed since the last durable commit.
    unflushed: AtomicBool,
    report: OpenReport,
}

impl fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedbStore").field("path", &self.path).field("durability", &self.durability).finish()
    }
}

// ------------------------------------------------------------------ errors

/// A redb error as an io error (I/O errors pass through with their kind).
fn dbe(e: impl Into<redb::Error>) -> io::Error {
    match e.into() {
        redb::Error::Io(e) => e,
        redb::Error::Corrupted(m) => io::Error::new(io::ErrorKind::InvalidData, format!("the file is damaged: {m}")),
        redb::Error::UpgradeRequired(v) => {
            io::Error::new(io::ErrorKind::InvalidData, format!("redb file format {v} is not supported by this build"))
        }
        redb::Error::DatabaseAlreadyOpen => io::Error::other("the index is already open (by this or another process)"),
        other => io::Error::other(other.to_string()),
    }
}

/// A record that does not decode.
fn bad(table: &str, key: impl fmt::Debug, why: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("damaged record in table `{table}` at key {key:?}: {why}"))
}

fn refuse(what: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("cannot store this record: {what}"))
}

/// Runs `f`, turning a panic inside redb (it trusts the bytes of its pages) into an error, and
/// naming the file in any error.
fn guard<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let r = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(io::Error::new(io::ErrorKind::InvalidData, format!("the database engine failed on this file: {msg}")))
        }
    };
    r.map_err(|e| io::Error::new(e.kind(), format!("sync index {}: {e}", path.display())))
}

// ------------------------------------------------------------------ open

impl RedbStore {
    /// Opens the index at `path`, or creates it (with parent folders) holding `new_index_id`.
    pub fn open(path: impl AsRef<Path>, new_index_id: u64) -> io::Result<RedbStore> {
        Self::open_with(path, new_index_id, &OpenOptions::default())
    }

    /// Opens with explicit options.
    ///
    /// * A file from a newer schema is refused (and not touched).
    /// * An older one is copied to `<file>.v<N>.<n>.bak` and migrated in one transaction.
    /// * A new file is built under `<file>.new` and renamed into place, so a crash during creation
    ///   never leaves a half-initialised index.
    pub fn open_with(path: impl AsRef<Path>, new_index_id: u64, opts: &OpenOptions) -> io::Result<RedbStore> {
        let path = path.as_ref();
        guard(path, || {
            let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            fs::create_dir_all(dir)?;
            let mut created = false;
            if fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true) {
                create_atomically(dir, path, new_index_id, opts)?;
                created = true;
            }
            let (db, migrated) = prepare(open_database(path, opts)?, path, new_index_id, opts)?;
            Ok(RedbStore {
                db,
                path: path.to_path_buf(),
                durability: opts.durability,
                unflushed: AtomicBool::new(false),
                report: OpenReport { created, migrated },
            })
        })
    }

    /// What happened at open.
    pub fn report(&self) -> &OpenReport {
        &self.report
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The durability [`IndexStore::apply`] uses from now on.
    pub fn set_durability(&mut self, d: Durability) {
        self.durability = d;
    }

    /// Applies the ops in one transaction with this durability (all or nothing).
    pub fn apply_with(&mut self, ops: Vec<Op>, d: Durability) -> io::Result<()> {
        guard(&self.path, || {
            let mut txn = self.db.begin_write().map_err(dbe)?;
            if d == Durability::Eventual {
                txn.set_durability(redb::Durability::None).map_err(dbe)?;
            }
            let res = (|| {
                let mut w = Writing::open(&txn)?;
                for op in ops {
                    w.op(op)?;
                }
                w.finish()
            })();
            match res {
                Ok(()) => txn.commit().map_err(dbe)?,
                Err(e) => {
                    let _ = txn.abort();
                    return Err(e);
                }
            }
            // An Immediate commit also makes every earlier Eventual one durable.
            self.unflushed.store(d == Durability::Eventual, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Makes everything durable, then rewrites the file without its free pages (after dropping a peer,
    /// or a large scan). Slow on a large file; `Ok(false)` when there was nothing to reclaim.
    pub fn compact(&mut self) -> io::Result<bool> {
        self.flush_shared()?;
        guard(&self.path, || self.db.compact().map_err(dbe))
    }

    fn flush_shared(&self) -> io::Result<()> {
        if !self.unflushed.load(Ordering::SeqCst) {
            return Ok(());
        }
        guard(&self.path, || {
            // An empty Immediate commit persists the Eventual ones before it.
            self.db.begin_write().map_err(dbe)?.commit().map_err(dbe)?;
            self.unflushed.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn rtx(&self) -> io::Result<ReadTransaction> {
        self.db.begin_read().map_err(dbe)
    }
}

impl Drop for RedbStore {
    fn drop(&mut self) {
        let _ = self.flush_shared();
    }
}

fn open_database(path: &Path, opts: &OpenOptions) -> io::Result<Database> {
    let mut db = redb::Builder::new().set_cache_size(opts.cache_bytes).create(path).map_err(dbe)?;
    if opts.verify {
        // Walks every page and checks its checksum, so damage shows now rather than on some later read.
        // `false` means redb repaired something (an unclean shutdown), which is not an error.
        db.check_integrity().map_err(dbe)?;
    }
    Ok(db)
}

fn create_atomically(dir: &Path, path: &Path, index_id: u64, opts: &OpenOptions) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    {
        prepare(open_database(&tmp, &OpenOptions { verify: false, ..*opts })?, &tmp, index_id, opts)?;
    }
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    // Make the rename durable.
    fs::File::open(dir)?.sync_all()
}

fn stored_version(rtx: &ReadTransaction) -> io::Result<Option<u32>> {
    let t = match rtx.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            if rtx.list_tables().map_err(dbe)?.next().is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a Seedwell sync index (no meta table)"));
            }
            return Ok(None);
        }
        Err(e) => return Err(dbe(e)),
    };
    let Some(v) = t.get(SCHEMA_KEY).map_err(dbe)? else {
        return Err(bad("meta", SCHEMA_KEY, "missing"));
    };
    let arr: [u8; 4] = v.value().try_into().map_err(|_| bad("meta", SCHEMA_KEY, "wrong size"))?;
    Ok(Some(u32::from_le_bytes(arr)))
}

fn tables_missing(rtx: &ReadTransaction) -> bool {
    rtx.open_table(META).is_err()
        || rtx.open_table(LOCAL).is_err()
        || rtx.open_table(BY_SEQ).is_err()
        || rtx.open_table(REMOTE).is_err()
        || rtx.open_table(REMOTE_BY_DEVICE).is_err()
        || rtx.open_table(PEERS).is_err()
        || rtx.open_table(WRITERS).is_err()
        || rtx.open_table(NEED).is_err()
        || rtx.open_table(CHUNKS).is_err()
}

fn create_tables(w: &WriteTransaction) -> io::Result<()> {
    w.open_table(META).map_err(dbe)?;
    w.open_table(LOCAL).map_err(dbe)?;
    w.open_table(BY_SEQ).map_err(dbe)?;
    w.open_table(REMOTE).map_err(dbe)?;
    w.open_table(REMOTE_BY_DEVICE).map_err(dbe)?;
    w.open_table(PEERS).map_err(dbe)?;
    w.open_table(WRITERS).map_err(dbe)?;
    w.open_table(NEED).map_err(dbe)?;
    w.open_table(CHUNKS).map_err(dbe)?;
    Ok(())
}

fn write_version(w: &WriteTransaction, v: u32) -> io::Result<()> {
    w.open_table(META).map_err(dbe)?.insert(SCHEMA_KEY, v.to_le_bytes().as_slice()).map_err(dbe)?;
    Ok(())
}

/// Brings an open file to the current schema: initialises an empty one, migrates an older one.
fn prepare(
    mut db: Database,
    path: &Path,
    index_id: u64,
    opts: &OpenOptions,
) -> io::Result<(Database, Option<Migrated>)> {
    let current = opts.schema.current;
    let rtx = db.begin_read().map_err(dbe)?;
    let stored = stored_version(&rtx)?;
    if stored == Some(current) && !tables_missing(&rtx) {
        let has_totals = rtx.open_table(META).map_err(dbe)?.get(LOCAL_TOTALS_KEY).map_err(dbe)?.is_some();
        drop(rtx);
        if !has_totals {
            // Written before the local totals were kept: count them once.
            let w = db.begin_write().map_err(dbe)?;
            count_local_totals(&w)?;
            w.commit().map_err(dbe)?;
        }
        return Ok((db, None));
    }
    drop(rtx);
    if let Some(v) = stored.filter(|v| *v > current) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "written by a newer Seedwell (index schema {v}; this build reads up to {current}); \
                 update Seedwell to open it"
            ),
        ));
    }
    let w = db.begin_write().map_err(dbe)?;
    let migrated = match stored {
        None => {
            create_tables(&w)?;
            write_version(&w, current)?;
            let mut meta = w.open_table(META).map_err(dbe)?;
            meta.insert(COUNTERS_KEY, enc_meta(&Meta { index_id, sequence: 0 }).as_slice()).map_err(dbe)?;
            meta.insert(TOTALS_KEY, enc_pair(0, 0).as_slice()).map_err(dbe)?;
            meta.insert(LOCAL_TOTALS_KEY, enc_pair(0, 0).as_slice()).map_err(dbe)?;
            None
        }
        Some(v) if v < current => {
            // Close first so the copy is of a cleanly closed file, then copy, then reopen.
            drop(w);
            drop(db);
            let backup = back_up(path, v)?;
            db = open_database(path, opts)?;
            let w = db.begin_write().map_err(dbe)?;
            let mut at = v;
            while at < current {
                let step = opts.schema.migrations.iter().find(|m| m.from == at).ok_or_else(|| {
                    io::Error::other(format!("no migration from index schema {at} (backup at {})", backup.display()))
                })?;
                (step.run)(&w).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("migration \"{}\" failed: {e} (backup at {})", step.name, backup.display()),
                    )
                })?;
                at = step.to;
            }
            create_tables(&w)?;
            write_version(&w, current)?;
            count_local_totals(&w)?;
            w.commit().map_err(dbe)?;
            return Ok((db, Some(Migrated { from: v, to: current, backup })));
        }
        Some(_) => {
            create_tables(&w)?;
            None
        }
    };
    w.commit().map_err(dbe)?;
    Ok((db, migrated))
}

/// Counts the live local files into `local_totals` (a file written before they were kept).
fn count_local_totals(w: &WriteTransaction) -> io::Result<()> {
    let (mut files, mut bytes) = (0u64, 0u64);
    {
        let t = w.open_table(LOCAL).map_err(dbe)?;
        for row in t.iter().map_err(dbe)? {
            let (k, v) = row.map_err(dbe)?;
            let rec = dec_local(v.value()).map_err(|e| bad("local", k.value(), e))?;
            if let Some(size) = counted(&rec) {
                files += 1;
                bytes += size;
            }
        }
    }
    w.open_table(META).map_err(dbe)?.insert(LOCAL_TOTALS_KEY, enc_pair(files, bytes).as_slice()).map_err(dbe)?;
    Ok(())
}

/// Copies the file next to itself before a migration.
fn back_up(path: &Path, version: u32) -> io::Result<PathBuf> {
    let base = path.as_os_str().to_string_lossy().into_owned();
    let backup = (0u32..)
        .map(|n| PathBuf::from(format!("{base}.v{version}.{n}.bak")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| PathBuf::from(format!("{base}.v{version}.bak")));
    fs::copy(path, &backup)?;
    fs::File::open(&backup)?.sync_all()?;
    Ok(backup)
}

// ------------------------------------------------------------------ record codec

fn done(r: &Reader<'_>) -> Result<(), DecodeError> {
    if r.remaining() == 0 { Ok(()) } else { Err(DecodeError::Invalid("trailing bytes")) }
}

/// The caps the wire decoder enforces, checked before writing: a record that could not be read back
/// is refused instead.
fn check_file(f: &FileInfo) -> io::Result<()> {
    let long = |s: &str| s.len() > MAX_PATH;
    if long(&f.path) || long(&f.symlink_target) || f.conflict_of.as_deref().is_some_and(long) {
        return Err(refuse(format_args!("a path of {:?}… is over {MAX_PATH} bytes", f.path.get(..64).unwrap_or(""))));
    }
    if f.chunks.len() > MAX_CHUNKS || f.chunks.iter().any(|c| c.len > MAX_CHUNK) {
        return Err(refuse(format_args!("{:?} has more or larger chunks than the wire allows", f.path)));
    }
    if f.version.counters().len() > MAX_COUNTERS {
        return Err(refuse(format_args!("{:?} has a version of more than {MAX_COUNTERS} devices", f.path)));
    }
    Ok(())
}

fn enc_meta(m: &Meta) -> Vec<u8> {
    enc_pair(m.index_id, m.sequence)
}

fn enc_pair(a: u64, b: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    put_varint(&mut out, a);
    put_varint(&mut out, b);
    out
}

fn dec_pair(b: &[u8]) -> Result<(u64, u64), DecodeError> {
    let mut r = Reader::new(b);
    let v = (r.varint("first")?, r.varint("second")?);
    done(&r)?;
    Ok(v)
}

fn enc_local(rec: &LocalRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(160 + 36 * rec.file.info.chunks.len());
    encode_entry(&mut out, rec.sequence, rec.invalid, &rec.file);
    match rec.stamp {
        Some(s) => {
            out.push(1);
            put_varint(&mut out, s.size);
            put_ivarint(&mut out, s.mtime_ns);
            put_ivarint(&mut out, s.ctime_ns);
            put_varint(&mut out, s.inode);
        }
        None => out.push(0),
    }
    match rec.tier {
        Some(t) => {
            out.push(1);
            put_varint(&mut out, u64::from(t.avg));
            put_varint(&mut out, t.chosen_at);
        }
        None => out.push(0),
    }
    out
}

fn dec_local(b: &[u8]) -> Result<LocalRecord, DecodeError> {
    let mut r = Reader::new(b);
    let IndexEntry { sequence, invalid, file } = decode_entry(&mut r)?;
    let stamp = if r.bool("has stamp")? {
        Some(Stamp {
            size: r.varint("stamp size")?,
            mtime_ns: r.ivarint("stamp mtime")?,
            ctime_ns: r.ivarint("stamp ctime")?,
            inode: r.varint("stamp inode")?,
        })
    } else {
        None
    };
    let tier =
        if r.bool("has tier")? { Some(TierMemo { avg: r.u32("tier")?, chosen_at: r.varint("tier size")? }) } else { None };
    done(&r)?;
    Ok(LocalRecord { file, sequence, invalid, stamp, tier })
}

fn enc_entry(e: &IndexEntry) -> Vec<u8> {
    let mut out = Vec::with_capacity(160 + 36 * e.file.info.chunks.len());
    encode_entry(&mut out, e.sequence, e.invalid, &e.file);
    out
}

fn dec_entry(b: &[u8]) -> Result<IndexEntry, DecodeError> {
    let mut r = Reader::new(b);
    let e = decode_entry(&mut r)?;
    done(&r)?;
    Ok(e)
}

fn need_byte(n: Need) -> u8 {
    match n {
        Need::Nothing => 0,
        Need::Take => 1,
        Need::TakeOverConflict => 2,
        Need::Repair => 3,
        Need::Absorb => 4,
    }
}

fn enc_need(n: &NeedRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(170 + 8 * n.holders.len() + 36 * n.file.info.chunks.len());
    out.push(need_byte(n.need));
    put_varint(&mut out, n.holders.len() as u64);
    for h in &n.holders {
        out.extend_from_slice(&h.0.to_be_bytes());
    }
    encode_entry(&mut out, 0, false, &n.file);
    out
}

fn dec_need(b: &[u8]) -> Result<NeedRecord, DecodeError> {
    let mut r = Reader::new(b);
    let need = match r.u8("need")? {
        0 => Need::Nothing,
        1 => Need::Take,
        2 => Need::TakeOverConflict,
        3 => Need::Repair,
        4 => Need::Absorb,
        _ => return Err(DecodeError::Invalid("need")),
    };
    let holders = r.list(MAX_HOLDERS, 8, "holders", |r| Ok(ShortId(u64::from_be_bytes(r.array("holder")?))))?;
    let file: SignedFile = decode_entry(&mut r)?.file;
    done(&r)?;
    Ok(NeedRecord { need, file, holders })
}

fn enc_writer(w: &Writer) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&w.device_pk);
    out.extend_from_slice(&w.cert);
    out
}

fn dec_writer(b: &[u8]) -> Result<Writer, DecodeError> {
    let mut r = Reader::new(b);
    let w = Writer { device_pk: r.array("device key")?, cert: r.array("certificate")? };
    done(&r)?;
    Ok(w)
}

/// The chunk-map rows a local record contributes: only a valid, present file serves chunks.
fn chunk_rows(rec: &LocalRecord) -> BTreeSet<([u8; 32], u64)> {
    if rec.invalid || rec.file.info.deleted {
        return BTreeSet::new();
    }
    offsets(&rec.file.info.chunks).map(|(off, c)| (c.hash, off)).collect()
}

// ------------------------------------------------------------------ write path

/// The tables of one write transaction.
struct Writing<'t> {
    meta: Table<'t, &'static str, &'static [u8]>,
    local: Table<'t, &'static str, &'static [u8]>,
    by_seq: Table<'t, u64, &'static str>,
    remote: Table<'t, (&'static str, u64), &'static [u8]>,
    by_device: Table<'t, (u64, &'static str), ()>,
    peers: Table<'t, u64, &'static [u8]>,
    writers: Table<'t, u64, &'static [u8]>,
    need: Table<'t, &'static str, &'static [u8]>,
    chunks: Table<'t, (&'static [u8; 32], &'static str, u64), ()>,
    /// Need totals, read at the first need change, written back at the end.
    totals: Option<(u64, u64)>,
    /// Local totals, the same way.
    local_totals: Option<(u64, u64)>,
}

impl<'t> Writing<'t> {
    fn open(txn: &'t WriteTransaction) -> io::Result<Self> {
        Ok(Writing {
            meta: txn.open_table(META).map_err(dbe)?,
            local: txn.open_table(LOCAL).map_err(dbe)?,
            by_seq: txn.open_table(BY_SEQ).map_err(dbe)?,
            remote: txn.open_table(REMOTE).map_err(dbe)?,
            by_device: txn.open_table(REMOTE_BY_DEVICE).map_err(dbe)?,
            peers: txn.open_table(PEERS).map_err(dbe)?,
            writers: txn.open_table(WRITERS).map_err(dbe)?,
            need: txn.open_table(NEED).map_err(dbe)?,
            chunks: txn.open_table(CHUNKS).map_err(dbe)?,
            totals: None,
            local_totals: None,
        })
    }

    fn op(&mut self, op: Op) -> io::Result<()> {
        match op {
            Op::SetMeta(m) => {
                self.meta.insert(COUNTERS_KEY, enc_meta(&m).as_slice()).map_err(dbe)?;
            }
            Op::PutLocal(rec) => self.put_local(&rec)?,
            Op::DelLocal(path) => {
                if let Some(old) = self.old_local(&path)? {
                    self.count(Some(&old), None)?;
                    self.by_seq.remove(old.sequence).map_err(dbe)?;
                    for (h, off) in chunk_rows(&old) {
                        self.chunks.remove((&h, path.as_str(), off)).map_err(dbe)?;
                    }
                    self.local.remove(path.as_str()).map_err(dbe)?;
                }
            }
            Op::PutRemote(d, e) => {
                check_file(&e.file.info)?;
                let path = e.file.info.path.as_str();
                self.remote.insert((path, d.0), enc_entry(&e).as_slice()).map_err(dbe)?;
                self.by_device.insert((d.0, path), ()).map_err(dbe)?;
            }
            Op::SetPeer(d, s) => {
                self.peers.insert(d.0, enc_pair(s.index_id, s.max_seq).as_slice()).map_err(dbe)?;
            }
            Op::DropPeer(d) => {
                let paths = device_paths(&self.by_device, d.0, None, usize::MAX)?;
                for p in &paths {
                    self.remote.remove((p.as_str(), d.0)).map_err(dbe)?;
                    self.by_device.remove((d.0, p.as_str())).map_err(dbe)?;
                }
                self.peers.remove(d.0).map_err(dbe)?;
            }
            Op::PutWriter(d, w) => {
                self.writers.insert(d.0, enc_writer(&w).as_slice()).map_err(dbe)?;
            }
            Op::SetNeed(p, n) => self.set_need(&p, n.as_ref())?,
        }
        Ok(())
    }

    fn old_local(&self, path: &str) -> io::Result<Option<LocalRecord>> {
        match self.local.get(path).map_err(dbe)? {
            Some(v) => dec_local(v.value()).map(Some).map_err(|e| bad("local", path, e)),
            None => Ok(None),
        }
    }

    fn put_local(&mut self, rec: &LocalRecord) -> io::Result<()> {
        check_file(&rec.file.info)?;
        let path = rec.file.info.path.as_str();
        // Only the rows that change are touched (a restamp changes none).
        let new_rows = chunk_rows(rec);
        let old = self.old_local(path)?;
        self.count(old.as_ref(), Some(rec))?;
        let old_rows = match old {
            Some(old) => {
                self.by_seq.remove(old.sequence).map_err(dbe)?;
                chunk_rows(&old)
            }
            None => BTreeSet::new(),
        };
        for (h, off) in old_rows.difference(&new_rows) {
            self.chunks.remove((h, path, *off)).map_err(dbe)?;
        }
        for (h, off) in new_rows.difference(&old_rows) {
            self.chunks.insert((h, path, *off), ()).map_err(dbe)?;
        }
        self.by_seq.insert(rec.sequence, path).map_err(dbe)?;
        self.local.insert(path, enc_local(rec).as_slice()).map_err(dbe)?;
        Ok(())
    }

    /// Moves the local totals from what `old` added to what `new` adds.
    fn count(&mut self, old: Option<&LocalRecord>, new: Option<&LocalRecord>) -> io::Result<()> {
        let (mut files, mut bytes) = match self.local_totals {
            Some(t) => t,
            None => read_pair(&self.meta, LOCAL_TOTALS_KEY)?,
        };
        if let Some(size) = old.and_then(counted) {
            files = files.wrapping_sub(1);
            bytes = bytes.wrapping_sub(size);
        }
        if let Some(size) = new.and_then(counted) {
            files = files.wrapping_add(1);
            bytes = bytes.wrapping_add(size);
        }
        self.local_totals = Some((files, bytes));
        Ok(())
    }

    fn set_need(&mut self, path: &str, n: Option<&NeedRecord>) -> io::Result<()> {
        let (mut count, mut bytes) = match self.totals {
            Some(t) => t,
            None => read_totals(&self.meta)?,
        };
        let old = match self.need.get(path).map_err(dbe)? {
            Some(v) => Some(dec_need(v.value()).map_err(|e| bad("need", path, e))?.file.info.size),
            None => None,
        };
        if let Some(size) = old {
            count = count.wrapping_sub(1);
            bytes = bytes.wrapping_sub(size);
        }
        match n {
            Some(n) => {
                check_file(&n.file.info)?;
                if n.holders.len() > MAX_HOLDERS {
                    return Err(refuse(format_args!("{path:?} has more than {MAX_HOLDERS} holders")));
                }
                self.need.insert(path, enc_need(n).as_slice()).map_err(dbe)?;
                count = count.wrapping_add(1);
                bytes = bytes.wrapping_add(n.file.info.size);
            }
            None => {
                if old.is_some() {
                    self.need.remove(path).map_err(dbe)?;
                }
            }
        }
        self.totals = Some((count, bytes));
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some((count, bytes)) = self.totals {
            self.meta.insert(TOTALS_KEY, enc_pair(count, bytes).as_slice()).map_err(dbe)?;
        }
        if let Some((files, bytes)) = self.local_totals {
            self.meta.insert(LOCAL_TOTALS_KEY, enc_pair(files, bytes).as_slice()).map_err(dbe)?;
        }
        Ok(())
    }
}

fn read_totals(meta: &impl ReadableTable<&'static str, &'static [u8]>) -> io::Result<(u64, u64)> {
    read_pair(meta, TOTALS_KEY)
}

fn read_pair(meta: &impl ReadableTable<&'static str, &'static [u8]>, key: &str) -> io::Result<(u64, u64)> {
    match meta.get(key).map_err(dbe)? {
        Some(v) => dec_pair(v.value()).map_err(|e| bad("meta", key, e)),
        None => Err(bad("meta", key, "missing")),
    }
}

/// Paths of `device` in the reverse index, after `after`, at most `limit`.
fn device_paths(
    t: &impl ReadableTable<(u64, &'static str), ()>,
    device: u64,
    after: Option<&str>,
    limit: usize,
) -> io::Result<Vec<String>> {
    let lower = match after {
        Some(a) => Bound::Excluded((device, a)),
        None => Bound::Included((device, "")),
    };
    let mut out = Vec::new();
    for row in t.range((lower, Bound::Unbounded)).map_err(dbe)? {
        if out.len() >= limit {
            break;
        }
        let (k, _) = row.map_err(dbe)?;
        let (d, p) = k.value();
        if d != device {
            break;
        }
        out.push(p.to_string());
    }
    Ok(out)
}

// ------------------------------------------------------------------ reads

impl IndexStore for RedbStore {
    fn meta(&self) -> io::Result<Meta> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(META).map_err(dbe)?;
            let v = t.get(COUNTERS_KEY).map_err(dbe)?.ok_or_else(|| bad("meta", COUNTERS_KEY, "missing"))?;
            let (index_id, sequence) = dec_pair(v.value()).map_err(|e| bad("meta", COUNTERS_KEY, e))?;
            Ok(Meta { index_id, sequence })
        })
    }

    fn local(&self, path: &str) -> io::Result<Option<LocalRecord>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(LOCAL).map_err(dbe)?;
            get_local(&t, path)
        })
    }

    fn local_since(&self, after: u64, limit: usize) -> io::Result<Vec<LocalRecord>> {
        // What peers are sent from: make it durable first, so no sequence is ever handed out twice.
        self.flush_shared()?;
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let seqs = rtx.open_table(BY_SEQ).map_err(dbe)?;
            let local = rtx.open_table(LOCAL).map_err(dbe)?;
            let mut out = Vec::new();
            for row in seqs.range((Bound::Excluded(after), Bound::Unbounded)).map_err(dbe)?.take(limit) {
                let (_, p) = row.map_err(dbe)?;
                if let Some(rec) = get_local(&local, p.value())? {
                    out.push(rec);
                }
            }
            Ok(out)
        })
    }

    fn local_paths(&self, prefix: &str, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(LOCAL).map_err(dbe)?;
            let mut out = Vec::new();
            if limit == 0 {
                return Ok(out);
            }
            if prefix.is_empty() {
                return range_keys(&t, (after.map_or(Bound::Unbounded, Bound::Excluded), Bound::Unbounded), limit, out);
            }
            // The directory itself sorts first; the paths below it are exactly the range
            // [prefix/, prefix0) ('0' follows '/'), and other names (`a-b`) may sit in between.
            if after.is_none_or(|a| prefix > a) && t.get(prefix).map_err(dbe)?.is_some() {
                out.push(prefix.to_string());
            }
            let below = format!("{prefix}/");
            let end = format!("{prefix}0");
            let lower = match after {
                Some(a) if a >= end.as_str() => return Ok(out),
                Some(a) if a >= below.as_str() => Bound::Excluded(a),
                _ => Bound::Included(below.as_str()),
            };
            range_keys(&t, (lower, Bound::Excluded(end.as_str())), limit, out)
        })
    }

    fn remote(&self, path: &str) -> io::Result<Vec<(ShortId, IndexEntry)>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(REMOTE).map_err(dbe)?;
            let mut out = Vec::new();
            for row in t.range((path, 0u64)..=(path, u64::MAX)).map_err(dbe)? {
                let (k, v) = row.map_err(dbe)?;
                let (_, d) = k.value();
                let e = dec_entry(v.value()).map_err(|e| bad("remote", (path, d), e))?;
                out.push((ShortId(d), e));
            }
            Ok(out)
        })
    }

    fn remote_paths(&self, device: ShortId, after: Option<&str>, limit: usize) -> io::Result<Vec<String>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(REMOTE_BY_DEVICE).map_err(dbe)?;
            device_paths(&t, device.0, after, limit)
        })
    }

    fn peer(&self, device: ShortId) -> io::Result<Option<PeerState>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(PEERS).map_err(dbe)?;
            match t.get(device.0).map_err(dbe)? {
                Some(v) => {
                    let (index_id, max_seq) = dec_pair(v.value()).map_err(|e| bad("peers", device.0, e))?;
                    Ok(Some(PeerState { index_id, max_seq }))
                }
                None => Ok(None),
            }
        })
    }

    fn writers(&self) -> io::Result<Vec<(ShortId, Writer)>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(WRITERS).map_err(dbe)?;
            let mut out = Vec::new();
            for row in t.iter().map_err(dbe)? {
                let (k, v) = row.map_err(dbe)?;
                let d = k.value();
                out.push((ShortId(d), dec_writer(v.value()).map_err(|e| bad("writers", d, e))?));
            }
            Ok(out)
        })
    }

    fn need(&self, path: &str) -> io::Result<Option<NeedRecord>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(NEED).map_err(dbe)?;
            match t.get(path).map_err(dbe)? {
                Some(v) => dec_need(v.value()).map(Some).map_err(|e| bad("need", path, e)),
                None => Ok(None),
            }
        })
    }

    fn needs(&self, after: Option<&str>, limit: usize) -> io::Result<Vec<(String, NeedRecord)>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(NEED).map_err(dbe)?;
            let lower = after.map_or(Bound::Unbounded, Bound::Excluded);
            let mut out = Vec::new();
            for row in t.range::<&str>((lower, Bound::Unbounded)).map_err(dbe)?.take(limit) {
                let (k, v) = row.map_err(dbe)?;
                let p = k.value();
                out.push((p.to_string(), dec_need(v.value()).map_err(|e| bad("need", p, e))?));
            }
            Ok(out)
        })
    }

    fn need_totals(&self) -> io::Result<(u64, u64)> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            read_totals(&rtx.open_table(META).map_err(dbe)?)
        })
    }

    fn local_totals(&self) -> io::Result<(u64, u64)> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            read_pair(&rtx.open_table(META).map_err(dbe)?, LOCAL_TOTALS_KEY)
        })
    }

    fn chunk_sources(&self, hash: &[u8; 32], limit: usize) -> io::Result<Vec<(String, u64)>> {
        guard(&self.path, || {
            let rtx = self.rtx()?;
            let t = rtx.open_table(CHUNKS).map_err(dbe)?;
            let mut out = Vec::new();
            for row in t.range((Bound::Included((hash, "", 0u64)), Bound::Unbounded)).map_err(dbe)? {
                if out.len() >= limit {
                    break;
                }
                let (k, _) = row.map_err(dbe)?;
                let (h, p, off) = k.value();
                if h != hash {
                    break;
                }
                out.push((p.to_string(), off));
            }
            Ok(out)
        })
    }

    fn apply(&mut self, ops: Vec<Op>) -> io::Result<()> {
        let d = self.durability;
        self.apply_with(ops, d)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_shared()
    }
}

fn get_local(t: &impl ReadableTable<&'static str, &'static [u8]>, path: &str) -> io::Result<Option<LocalRecord>> {
    let Some(v) = t.get(path).map_err(dbe)? else { return Ok(None) };
    let rec = dec_local(v.value()).map_err(|e| bad("local", path, e))?;
    if rec.file.info.path != path {
        return Err(bad("local", path, format_args!("the record is for {:?}", rec.file.info.path)));
    }
    Ok(Some(rec))
}

fn range_keys<'a>(
    t: &impl ReadableTable<&'static str, &'static [u8]>,
    bounds: (Bound<&'a str>, Bound<&'a str>),
    limit: usize,
    mut out: Vec<String>,
) -> io::Result<Vec<String>> {
    for row in t.range::<&str>(bounds).map_err(dbe)? {
        if out.len() >= limit {
            break;
        }
        let (k, _) = row.map_err(dbe)?;
        out.push(k.value().to_string());
    }
    Ok(out)
}
