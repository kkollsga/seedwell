//! `RedbStore` against `MemStore` (the reference semantics), persistence, atomicity, schema and damage
//! handling, and an ignored scale run. Temp folders only.

use std::io;
use std::path::Path;
use std::time::Instant;

use proptest::prelude::*;
use trs_sync::index::{Batch, IndexStore, LocalRecord, MemStore, NeedRecord, Op, PeerState, Stamp, TierMemo};
use trs_sync::global::Need;
use trs_sync::store_redb::{Durability, Migration, OpenOptions, RedbStore, Schema};
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{ChunkRef, Counter, FileInfo, FileKind, IndexEntry, SignedFile, Version};
use trs_sync_proto::sign::Writer;

const PATHS: &[&str] = &["a", "a-b", "a/c", "a/c/d", "a/e", "a0", "ab", "b", "b/a", "c/a/b"];
const PREFIXES: &[&str] = &["", "a", "a/c", "a-b", "b", "c", "c/a", "zz", "a/", "a/c/d"];
const ME: ShortId = ShortId(5);
const DEVS: [ShortId; 3] = [ShortId(10), ShortId(20), ShortId(u64::MAX)];
/// Version counters name one of these.
const IDS: [ShortId; 4] = [ME, DEVS[0], DEVS[1], DEVS[2]];

fn hash(i: u8) -> [u8; 32] {
    [i; 32]
}

#[derive(Clone, Debug)]
struct FileSpec {
    path: usize,
    deleted: bool,
    counters: Vec<(usize, u64)>,
    chunks: Vec<(u8, u32)>,
    mtime: i64,
    by: usize,
}

fn signed(path: &str, deleted: bool, counters: &[(usize, u64)], chunks: &[(u8, u32)], mtime: i64, by: usize) -> SignedFile {
    let mut cs: Vec<Counter> = Vec::new();
    for &(i, v) in counters {
        let id = IDS[i];
        match cs.iter_mut().find(|c| c.id == id) {
            Some(c) => c.value = c.value.max(v + 1),
            None => cs.push(Counter { id, value: v + 1 }),
        }
    }
    cs.sort_by_key(|c| c.id);
    let chunks: Vec<ChunkRef> =
        if deleted { Vec::new() } else { chunks.iter().map(|&(h, len)| ChunkRef { len, hash: hash(h) }).collect() };
    let info = FileInfo {
        path: path.to_string(),
        kind: FileKind::File,
        size: chunks.iter().map(|c| u64::from(c.len)).sum(),
        mtime_ns: mtime,
        exec: false,
        deleted,
        version: Version::from_counters(cs).unwrap(),
        modified_by: IDS[by],
        file_id: [7; 16],
        symlink_target: String::new(),
        conflict_of: None,
        chunks,
    };
    SignedFile { info, sig: [9; 64] }
}

fn file(f: &FileSpec) -> SignedFile {
    signed(PATHS[f.path], f.deleted, &f.counters, &f.chunks, f.mtime, f.by)
}

#[derive(Clone, Debug)]
enum Act {
    PutLocal { f: FileSpec, invalid: bool, stamp: Option<(u64, i64)>, tier: bool },
    Restamp { path: usize, inode: u64 },
    PutRemote { dev: usize, f: FileSpec, seq: u64, invalid: bool },
    SetPeer { dev: usize, index_id: u64, max_seq: u64 },
    DropPeer { dev: usize },
    PutWriter { dev: usize, b: u8 },
    DropWriter { dev: usize },
    SetIndexId(u64),
}

#[derive(Clone, Debug)]
enum Step {
    Batch(Vec<Act>, bool),
    DelLocal(usize),
    Flush,
    Reopen,
}

/// The ops a batch of these actions produces on `store`.
fn build<S: IndexStore>(store: &S, acts: &[Act]) -> Vec<Op> {
    let mut b = Batch::new(store, ME).unwrap();
    for a in acts {
        match a {
            Act::PutLocal { f, invalid, stamp, tier } => b.put_local(LocalRecord {
                file: file(f),
                sequence: 0,
                invalid: *invalid,
                stamp: stamp.map(|(size, mtime_ns)| Stamp { size, mtime_ns, ctime_ns: -mtime_ns, inode: size ^ 77 }),
                tier: tier.then_some(TierMemo { avg: 65536, chosen_at: 12345 }),
            }),
            Act::Restamp { path, inode } => {
                if let Some(r) = b.local(PATHS[*path]).unwrap() {
                    let stamp = Some(Stamp { size: 1, mtime_ns: 2, ctime_ns: 3, inode: *inode });
                    b.restamp_local(LocalRecord { stamp, ..r });
                }
            }
            Act::PutRemote { dev, f, seq, invalid } => {
                b.put_remote(DEVS[*dev], IndexEntry { sequence: *seq, invalid: *invalid, file: file(f) })
            }
            Act::SetPeer { dev, index_id, max_seq } => {
                b.set_peer(DEVS[*dev], PeerState { index_id: *index_id, max_seq: *max_seq })
            }
            Act::DropPeer { dev } => b.drop_peer(DEVS[*dev]).unwrap(),
            Act::PutWriter { dev, b: x } => b.put_writer(DEVS[*dev], Writer { device_pk: [*x; 32] }),
            Act::DropWriter { dev } => b.drop_writer(DEVS[*dev]),
            Act::SetIndexId(id) => b.set_index_id(*id),
        }
    }
    b.finish().unwrap()
}

/// Every read of the trait, compared.
fn assert_same(mem: &MemStore, db: &RedbStore) {
    assert_eq!(mem.meta().unwrap(), db.meta().unwrap(), "meta");
    let probe: Vec<&str> = PATHS.iter().copied().chain(["zz", ""]).collect();
    for p in &probe {
        assert_eq!(mem.local(p).unwrap(), db.local(p).unwrap(), "local({p:?})");
        assert_eq!(mem.remote(p).unwrap(), db.remote(p).unwrap(), "remote({p:?})");
        assert_eq!(mem.need(p).unwrap(), db.need(p).unwrap(), "need({p:?})");
    }
    let seq = mem.meta().unwrap().sequence;
    for after in 0..=seq + 1 {
        for limit in [0, 1, 2, 1000] {
            assert_eq!(
                mem.local_since(after, limit).unwrap(),
                db.local_since(after, limit).unwrap(),
                "local_since({after}, {limit})"
            );
        }
    }
    let afters: Vec<Option<&str>> = std::iter::once(None).chain(probe.iter().map(|p| Some(*p))).collect();
    for prefix in PREFIXES {
        for after in &afters {
            for limit in [0, 1, 2, 100] {
                assert_eq!(
                    mem.local_paths(prefix, *after, limit).unwrap(),
                    db.local_paths(prefix, *after, limit).unwrap(),
                    "local_paths({prefix:?}, {after:?}, {limit})"
                );
            }
        }
    }
    for d in IDS {
        for after in &afters {
            for limit in [1, 100] {
                assert_eq!(
                    mem.remote_paths(d, *after, limit).unwrap(),
                    db.remote_paths(d, *after, limit).unwrap(),
                    "remote_paths({d:?}, {after:?}, {limit})"
                );
            }
        }
        assert_eq!(mem.peer(d).unwrap(), db.peer(d).unwrap(), "peer({d:?})");
    }
    assert_eq!(format!("{:?}", mem.writers().unwrap()), format!("{:?}", db.writers().unwrap()), "writers");
    for after in &afters {
        for limit in [1, 100] {
            assert_eq!(mem.needs(*after, limit).unwrap(), db.needs(*after, limit).unwrap(), "needs({after:?}, {limit})");
        }
    }
    assert_eq!(mem.need_totals().unwrap(), db.need_totals().unwrap(), "need_totals");
    assert_eq!(mem.local_totals().unwrap(), db.local_totals().unwrap(), "local_totals");
    for h in 0..6 {
        for limit in [1, 100] {
            assert_eq!(
                mem.chunk_sources(&hash(h), limit).unwrap(),
                db.chunk_sources(&hash(h), limit).unwrap(),
                "chunk_sources({h}, {limit})"
            );
        }
    }
}

fn eventual() -> OpenOptions {
    OpenOptions { durability: Durability::Eventual, ..OpenOptions::default() }
}

fn file_spec() -> impl Strategy<Value = FileSpec> {
    (
        0..PATHS.len(),
        prop::bool::weighted(0.2),
        prop::collection::vec((0..IDS.len(), 0u64..3), 0..3),
        prop::collection::vec((0u8..5, 1u32..2000), 0..4),
        -3i64..3,
        0..IDS.len(),
    )
        .prop_map(|(path, deleted, counters, chunks, mtime, by)| FileSpec { path, deleted, counters, chunks, mtime, by })
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        5 => (file_spec(), prop::bool::weighted(0.2), prop::option::of((0u64..5, -2i64..2)), any::<bool>())
            .prop_map(|(f, invalid, stamp, tier)| Act::PutLocal { f, invalid, stamp, tier }),
        2 => (0..PATHS.len(), 0u64..3).prop_map(|(path, inode)| Act::Restamp { path, inode }),
        6 => (0..DEVS.len(), file_spec(), 0u64..50, prop::bool::weighted(0.2))
            .prop_map(|(dev, f, seq, invalid)| Act::PutRemote { dev, f, seq, invalid }),
        1 => (0..DEVS.len(), 0u64..3, 0u64..100).prop_map(|(dev, index_id, max_seq)| Act::SetPeer { dev, index_id, max_seq }),
        1 => (0..DEVS.len()).prop_map(|dev| Act::DropPeer { dev }),
        1 => (0..DEVS.len(), any::<u8>()).prop_map(|(dev, b)| Act::PutWriter { dev, b }),
        1 => (0..DEVS.len()).prop_map(|dev| Act::DropWriter { dev }),
        1 => (0u64..4).prop_map(Act::SetIndexId),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => (prop::collection::vec(act(), 1..6), any::<bool>()).prop_map(|(a, e)| Step::Batch(a, e)),
        1 => (0..PATHS.len()).prop_map(Step::DelLocal),
        1 => Just(Step::Flush),
        1 => Just(Step::Reopen),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// The same op sequences on both stores give the same answers to every read; the batches built on
    /// each (which read the store) are the same too.
    #[test]
    fn redb_matches_memstore(steps in prop::collection::vec(step(), 1..25)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync").join("f.redb");
        let mut mem = MemStore::new(42);
        let mut db = Some(RedbStore::open_with(&path, 42, &eventual()).unwrap());
        for s in &steps {
            let st = db.as_mut().unwrap();
            match s {
                Step::Batch(acts, ev) => {
                    let ops = build(&mem, acts);
                    let ops2 = build(&*st, acts);
                    assert_eq!(format!("{ops:?}"), format!("{ops2:?}"), "batches differ");
                    mem.apply(ops).unwrap();
                    st.apply_with(ops2, if *ev { Durability::Eventual } else { Durability::Immediate }).unwrap();
                }
                Step::DelLocal(p) => {
                    let ops = vec![Op::DelLocal(PATHS[*p].to_string())];
                    mem.apply(ops.clone()).unwrap();
                    st.apply(ops).unwrap();
                }
                Step::Flush => st.flush().unwrap(),
                Step::Reopen => {
                    drop(db.take());
                    let again = RedbStore::open_with(&path, 999, &eventual()).unwrap();
                    assert!(!again.report().created);
                    db = Some(again);
                }
            }
            assert_same(&mem, db.as_ref().unwrap());
        }
    }
}

/// A few batches touching every table.
fn sample_batches() -> Vec<Vec<Act>> {
    let f = |path, deleted, counters: Vec<(usize, u64)>, chunks: Vec<(u8, u32)>| FileSpec {
        path,
        deleted,
        counters,
        chunks,
        mtime: 1,
        by: 0,
    };
    vec![
        vec![
            Act::PutLocal { f: f(0, false, vec![(0, 1)], vec![(1, 100), (2, 50)]), invalid: false, stamp: Some((150, 9)), tier: true },
            Act::PutLocal { f: f(2, false, vec![(0, 1)], vec![(1, 100)]), invalid: false, stamp: Some((100, 9)), tier: false },
            Act::PutLocal { f: f(3, true, vec![(0, 2)], vec![]), invalid: false, stamp: None, tier: false },
        ],
        vec![
            Act::PutRemote { dev: 0, f: f(0, false, vec![(0, 1), (1, 1)], vec![(3, 10)]), seq: 4, invalid: false },
            Act::PutRemote { dev: 1, f: f(5, false, vec![(2, 1)], vec![(4, 10)]), seq: 7, invalid: false },
            Act::PutRemote { dev: 2, f: f(5, false, vec![(2, 1)], vec![(4, 10)]), seq: 1, invalid: true },
            Act::SetPeer { dev: 0, index_id: 3, max_seq: 4 },
            Act::SetPeer { dev: 1, index_id: 5, max_seq: 7 },
            Act::PutWriter { dev: 1, b: 3 },
        ],
        vec![Act::Restamp { path: 2, inode: 55 }, Act::DropPeer { dev: 2 }],
    ]
}

#[test]
fn reopen_after_drop_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.redb");
    let mut mem = MemStore::new(7);
    {
        let mut db = RedbStore::open(&path, 7).unwrap();
        assert!(db.report().created);
        for (i, acts) in sample_batches().iter().enumerate() {
            let ops = build(&mem, acts);
            mem.apply(ops.clone()).unwrap();
            // The last batch only eventually: dropping the store must still keep it.
            db.apply_with(ops, if i == 2 { Durability::Eventual } else { Durability::Immediate }).unwrap();
        }
        assert_same(&mem, &db);
    }
    let db = RedbStore::open(&path, 8).unwrap();
    assert!(!db.report().created);
    assert_eq!(db.meta().unwrap().index_id, 7, "the index id is the creating one");
    assert!(db.need_totals().unwrap().0 > 0, "something is needed");
    assert_same(&mem, &db);
}

#[test]
fn a_reader_sees_every_commit_and_keeps_compaction_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = RedbStore::open(dir.path().join("f.redb"), 1).unwrap();
    let reader = db.reader();
    assert_eq!(reader.local("a").unwrap(), None);
    let rec = |p: &str, seq| LocalRecord { file: signed(p, false, &[(0, 1)], &[(1, 5)], 0, 0), sequence: seq, invalid: false, stamp: None, tier: None };
    db.apply(vec![Op::PutLocal(rec("a", 1))]).unwrap();
    // An eventual commit is not on disk yet, but it is committed: readers see it.
    db.apply_with(vec![Op::PutLocal(rec("b", 2))], Durability::Eventual).unwrap();
    assert_eq!(reader.local("a").unwrap(), db.local("a").unwrap());
    assert_eq!(reader.local("b").unwrap().map(|r| r.sequence), Some(2));
    // Compaction needs the file to itself.
    assert!(db.compact().is_err());
    drop(reader);
    db.compact().unwrap();
}

#[test]
fn a_failed_apply_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut mem = MemStore::new(1);
    let mut db = RedbStore::open(dir.path().join("f.redb"), 1).unwrap();
    let first = build(&mem, &sample_batches()[0]);
    mem.apply(first.clone()).unwrap();
    db.apply(first).unwrap();

    // Valid changes, then a record the wire format cannot hold (a path over MAX_PATH), then more.
    let mut ops = build(&mem, &sample_batches()[1]);
    let long = signed(&"x".repeat(5000), false, &[(0, 1)], &[(1, 5)], 0, 0);
    ops.insert(3, Op::PutLocal(LocalRecord { file: long, sequence: 99, invalid: false, stamp: None, tier: None }));
    ops.push(Op::SetNeed("a".into(), Some(NeedRecord { need: Need::Take, file: signed("a", false, &[], &[], 0, 0), holders: vec![] })));
    let err = db.apply(ops).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("5000") || err.to_string().contains("bytes"), "{err}");
    assert_same(&mem, &db);

    // The store keeps working.
    let next = build(&mem, &sample_batches()[1]);
    mem.apply(next.clone()).unwrap();
    db.apply(next).unwrap();
    assert_same(&mem, &db);
}

static V2: Schema = Schema {
    current: 2,
    migrations: &[Migration {
        from: 1,
        to: 2,
        name: "test step",
        run: |w| {
            let t: redb::TableDefinition<&str, u64> = redb::TableDefinition::new("migrated");
            w.open_table(t).map_err(|e| io::Error::other(e.to_string()))?.insert("yes", 1).map_err(|e| io::Error::other(e.to_string()))?;
            Ok(())
        },
    }],
};

#[test]
fn older_schemas_migrate_with_a_backup_and_newer_ones_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.redb");
    let mut mem = MemStore::new(3);
    {
        let mut db = RedbStore::open(&path, 3).unwrap();
        let ops = build(&mem, &sample_batches()[0]);
        mem.apply(ops.clone()).unwrap();
        db.apply(ops).unwrap();
    }
    let opts = OpenOptions { schema: &V2, ..OpenOptions::default() };
    let db = RedbStore::open_with(&path, 3, &opts).unwrap();
    let m = db.report().migrated.clone().expect("migrated");
    assert_eq!((m.from, m.to), (1, 2));
    assert!(m.backup.exists(), "backup at {}", m.backup.display());
    assert!(m.backup.file_name().unwrap().to_string_lossy().starts_with("f.redb.v1."));
    assert_same(&mem, &db);
    drop(db);
    assert!(RedbStore::open_with(&path, 3, &opts).unwrap().report().migrated.is_none(), "only once");

    let err = RedbStore::open(&path, 3).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let msg = err.to_string();
    assert!(msg.contains("newer") && msg.contains("schema 2") && msg.contains("f.redb"), "{msg}");
    // Refusing does not touch the file.
    assert_same(&mem, &RedbStore::open_with(&path, 3, &opts).unwrap());
}

/// Writes raw bytes into a table of a closed index file.
fn poke(path: &Path, table: &str, key: &str, value: &[u8]) {
    let db = redb::Database::open(path).unwrap();
    let w = db.begin_write().unwrap();
    {
        let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new(table);
        w.open_table(def).unwrap().insert(key, value).unwrap();
    }
    w.commit().unwrap();
}

#[test]
fn a_damaged_record_names_its_table_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.redb");
    let mut mem = MemStore::new(3);
    {
        let mut db = RedbStore::open(&path, 3).unwrap();
        let ops = build(&mem, &sample_batches()[0]);
        mem.apply(ops.clone()).unwrap();
        db.apply(ops).unwrap();
    }
    poke(&path, "local", "a/c", &[1, 0, 200]);
    poke(&path, "need", "zz", &[9]);
    let db = RedbStore::open(&path, 3).unwrap();
    let err = db.local("a/c").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let msg = err.to_string();
    assert!(msg.contains("table `local`") && msg.contains("\"a/c\""), "{msg}");
    let msg = db.needs(None, 10).unwrap_err().to_string();
    assert!(msg.contains("table `need`") && msg.contains("\"zz\""), "{msg}");
    // The rest still reads.
    assert_eq!(db.local("a").unwrap(), mem.local("a").unwrap());
    assert_eq!(db.local_paths("", None, 10).unwrap(), mem.local_paths("", None, 10).unwrap());

    // A record under the wrong key.
    drop(db);
    let mut other = Vec::new();
    trs_sync_proto::msg::encode_entry(&mut other, 1, false, &signed("b", false, &[], &[], 0, 0));
    other.extend_from_slice(&[0, 0]);
    poke(&path, "local", "a", &other);
    let msg = RedbStore::open(&path, 3).unwrap().local("a").unwrap_err().to_string();
    assert!(msg.contains("the record is for \"b\""), "{msg}");
}

#[test]
fn garbage_files_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    for (i, bytes) in [vec![0u8; 10], vec![0xAB; 64 << 10], b"redb\x1a\x0a\xa9\x0d\x0a".repeat(1000)].into_iter().enumerate() {
        let path = dir.path().join(format!("g{i}.redb"));
        std::fs::write(&path, &bytes).unwrap();
        let err = RedbStore::open(&path, 1).unwrap_err();
        assert!(err.to_string().contains(&format!("g{i}.redb")), "{err}");
    }
    // A redb file of something else.
    let path = dir.path().join("other.redb");
    {
        let db = redb::Database::create(&path).unwrap();
        let w = db.begin_write().unwrap();
        w.open_table(redb::TableDefinition::<u64, u64>::new("x")).unwrap().insert(1, 2).unwrap();
        w.commit().unwrap();
    }
    let msg = RedbStore::open(&path, 1).unwrap_err().to_string();
    assert!(msg.contains("not a Seedwell sync index"), "{msg}");
}

/// A copy of the file taken while the store is open is what a crash of the process would leave.
#[test]
fn a_crash_keeps_durable_batches_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.redb");
    let mut mem = MemStore::new(3);
    let mut db = RedbStore::open(&path, 3).unwrap();
    let batches = sample_batches();
    let ops = build(&mem, &batches[0]);
    mem.apply(ops.clone()).unwrap();
    db.apply(ops).unwrap();
    let durable = mem.clone();

    let ops = build(&mem, &batches[1]);
    mem.apply(ops.clone()).unwrap();
    db.apply_with(ops, Durability::Eventual).unwrap();
    let crash1 = dir.path().join("crash1.redb");
    std::fs::copy(&path, &crash1).unwrap();
    let c = RedbStore::open(&crash1, 0).unwrap();
    // Either the eventual batch is there whole, or it is not there at all.
    let have = c.peer(DEVS[0]).unwrap().is_some();
    assert_same(if have { &mem } else { &durable }, &c);
    eprintln!("eventual batch survived a process crash: {have}");

    // Reading what peers are sent from makes it durable.
    let _ = db.local_since(0, 1).unwrap();
    let crash2 = dir.path().join("crash2.redb");
    std::fs::copy(&path, &crash2).unwrap();
    assert_same(&mem, &RedbStore::open(&crash2, 0).unwrap());
}

fn rss_mib() -> Option<f64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse::<f64>().ok().map(|kib| kib / 1024.0)
}

/// 200k local records and two peers with 200k entries each.
/// `tools/sysguard/heavy.sh cargo test --release -p trs-sync --test it redb_scale -- --ignored --nocapture`
#[test]
#[ignore = "scale run, about a minute in release"]
fn redb_scale() {
    const N: usize = 200_000;
    const BATCH: usize = 1000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scale.redb");
    let mut db = RedbStore::open(&path, 1).unwrap();
    // REDB_SCALE_IMMEDIATE=1 commits every bulk batch durably instead.
    let bulk = if std::env::var_os("REDB_SCALE_IMMEDIATE").is_some() { Durability::Immediate } else { Durability::Eventual };
    let rec = |i: usize, by: ShortId| {
        let chunks = (0..4u32)
            .map(|c| {
                let mut h = [0u8; 32];
                h[..8].copy_from_slice(&(i as u64).to_be_bytes());
                h[8] = c as u8;
                ChunkRef { len: 65536 + c, hash: h }
            })
            .collect::<Vec<_>>();
        let info = FileInfo {
            path: format!("photos/{:03}/IMG_{i:06}.jpg", i / 1000),
            kind: FileKind::File,
            size: chunks.iter().map(|c| u64::from(c.len)).sum(),
            mtime_ns: 1_700_000_000_000_000_000 + i as i64,
            exec: false,
            deleted: false,
            version: Version::from_counters(vec![Counter { id: by, value: 1_700_000_000 + i as u64 }]).unwrap(),
            modified_by: by,
            file_id: [1; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks,
        };
        SignedFile { info, sig: [2; 64] }
    };
    let t = Instant::now();
    for start in (0..N).step_by(BATCH) {
        let mut b = Batch::new(&db, ME).unwrap();
        for i in start..start + BATCH {
            let stamp = Some(Stamp { size: 262_150, mtime_ns: i as i64, ctime_ns: i as i64, inode: i as u64 });
            b.put_local(LocalRecord { file: rec(i, ME), sequence: 0, invalid: false, stamp, tier: None });
        }
        let ops = b.finish().unwrap();
        db.apply_with(ops, bulk).unwrap();
    }
    db.flush().unwrap();
    let local_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    for (k, dev) in [DEVS[0], DEVS[1]].into_iter().enumerate() {
        for start in (0..N).step_by(BATCH) {
            let mut b = Batch::new(&db, ME).unwrap();
            for i in start..start + BATCH {
                b.put_remote(dev, IndexEntry { sequence: i as u64 + 1, invalid: false, file: rec(i, ME) });
            }
            b.set_peer(dev, PeerState { index_id: k as u64, max_seq: (start + BATCH) as u64 });
            let ops = b.finish().unwrap();
            db.apply_with(ops, bulk).unwrap();
        }
    }
    db.flush().unwrap();
    let remote_s = t.elapsed().as_secs_f64();

    // One Immediate batch of 100 changes (a puller step is one path; a scan batch about this).
    let t = Instant::now();
    let mut b = Batch::new(&db, ME).unwrap();
    for i in 0..100 {
        let r = b.local(&rec(i, ME).info.path).unwrap().unwrap();
        b.put_local(LocalRecord { stamp: None, ..r });
    }
    let ops = b.finish().unwrap();
    db.apply_with(ops, Durability::Immediate).unwrap();
    let immediate_ms = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let page = db.local_since(N as u64 / 2, 1000).unwrap();
    let since_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(page.len(), 1000);
    let t = Instant::now();
    let paths = db.local_paths("photos/150", None, 1000).unwrap();
    let paths_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(paths.len(), 1000);
    let t = Instant::now();
    let rp = db.remote_paths(DEVS[1], Some("photos/100"), 1000).unwrap();
    let rp_ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(rp.len(), 1000);
    let t = Instant::now();
    let mut found = 0;
    for i in (0..N).step_by(N / 1000) {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&(i as u64).to_be_bytes());
        found += db.chunk_sources(&h, 4).unwrap().len();
    }
    let chunk_us = t.elapsed().as_secs_f64() * 1e6 / 1000.0;
    assert_eq!(found, 1000);
    assert_eq!(db.need_totals().unwrap(), (0, 0));
    let rss = rss_mib();
    let size = std::fs::metadata(&path).unwrap().len() as f64 / 1048576.0;
    drop(db);
    let t = Instant::now();
    let db = RedbStore::open(&path, 1).unwrap();
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    drop(db);
    let t = Instant::now();
    let db = RedbStore::open_with(&path, 1, &OpenOptions { verify: true, ..OpenOptions::default() }).unwrap();
    let verify_ms = t.elapsed().as_secs_f64() * 1e3;
    // Dropping a peer: 200k paths re-evaluated.
    let t = Instant::now();
    let mut db = db;
    let mut b = Batch::new(&db, ME).unwrap();
    b.drop_peer(DEVS[0]).unwrap();
    let ops = b.finish().unwrap();
    db.apply(ops).unwrap();
    let drop_s = t.elapsed().as_secs_f64();
    assert!(db.remote_paths(DEVS[0], None, 1).unwrap().is_empty());
    let t = Instant::now();
    db.compact().unwrap();
    let compact_s = t.elapsed().as_secs_f64();
    let compacted = std::fs::metadata(&path).unwrap().len() as f64 / 1048576.0;

    eprintln!("redb_scale: {N} local + 2 x {N} remote, batches of {BATCH}, {bulk:?}");
    eprintln!("  insert local   {local_s:.2} s  ({:.0} records/s)", N as f64 / local_s);
    eprintln!("  insert remote  {remote_s:.2} s  ({:.0} entries/s)", 2.0 * N as f64 / remote_s);
    eprintln!("  immediate batch of 100 local changes  {immediate_ms:.1} ms");
    eprintln!("  file size      {size:.1} MiB");
    eprintln!("  local_since page of 1000      {since_ms:.2} ms");
    eprintln!("  local_paths page of 1000      {paths_ms:.2} ms");
    eprintln!("  remote_paths page of 1000     {rp_ms:.2} ms");
    eprintln!("  chunk_sources lookup          {chunk_us:.1} us");
    eprintln!("  open {open_ms:.1} ms, open with verify {verify_ms:.0} ms");
    eprintln!("  drop a peer of {N} entries   {drop_s:.2} s");
    eprintln!("  compact after the drop  {compact_s:.2} s, {compacted:.1} MiB");
    if let Some(r) = rss {
        eprintln!("  RSS after inserts  {r:.0} MiB (16 MiB page cache)");
    }
}
