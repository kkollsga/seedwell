//! Ignored paths (SPEC §7G.2, *Not synced*): never indexed or announced, wherever they come from, never fetched or
//! counted as needed, and when new patterns ignore paths already in the index, left on disk but no longer announced.
//! The case behind these tests: a `.seedwellignore` with `/synced/` arrived on a device that had joined the folder,
//! and Claude Code then wrote `synced/` into it (through a junction); every file of it was indexed and announced, and
//! the other device, which ignores `synced/` too, waited for them forever.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use seedwell_sync::clock::Clock;
use seedwell_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
use seedwell_sync::fs::{Fs, MemFs, read_all, write_all};
use seedwell_sync::ignore::Ignore;
use seedwell_sync::index::{IndexStore, MemStore};
use seedwell_sync::pull::{ChunkRequest, ChunkSource, FetchError, PullReport};
use seedwell_sync::scan::ScanJob;
use seedwell_sync_proto::device::DeviceKey;

use crate::flow::{Key, learn_writers};

struct Utc;
impl Clock for Utc {
    fn now_ns(&self) -> i64 {
        1_792_938_725_000_000_000
    }
}

struct Dev {
    fs: MemFs,
    core: FolderCore,
}

fn key() -> Key {
    Key::rw([9; 32])
}

/// A device on an in-memory disk; `follows` says whether its core follows the folder's `.seedwellignore` (as the
/// driver's does), `local` are its own patterns.
fn dev_with(seed: u8, store: Box<dyn IndexStore>, fs: MemFs, follows: bool, local: &[&str]) -> Dev {
    let synced = read_all(&fs, ".seedwellignore").ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
    let local: Vec<String> = local.iter().map(|s| s.to_string()).collect();
    let ignore = if follows { Ignore::parse_all(&[&synced, &local.join("\n")]).0 } else { Ignore::parse_all(&[&local.join("\n")]).0 };
    dev_ignoring(seed, store, fs, ignore, follows.then_some(local))
}

/// A device whose core starts with `ignore`, and follows the folder's `.seedwellignore` with these own patterns.
fn dev_ignoring(seed: u8, store: Box<dyn IndexStore>, fs: MemFs, ignore: Ignore, follow: Option<Vec<String>>) -> Dev {
    if fs.lstat(".seedwell").unwrap().is_none() {
        fs.create_dir(".seedwell").unwrap();
    }
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let k = key();
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *k.public_key(),
        access: k.access(),
        case_insensitive: false,
        byte_exact_names: false,
        exec_bits: true,
        archive: true,
        on_demand: false,
    };
    let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
    let new_id = Arc::new(move || {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&ids.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        id
    });
    let mut core = FolderCore::new(cfg, store, Arc::new(fs.clone()), Some(dk), Arc::new(Utc), new_id, ignore).unwrap();
    if let Some(local) = follow {
        core.follow_ignore_file(&local).unwrap();
    }
    Dev { fs, core }
}

fn dev(seed: u8, follows: bool, local: &[&str]) -> Dev {
    dev_with(seed, Box::new(MemStore::new(u64::from(seed))), MemFs::new(), follows, local)
}

/// `from`'s whole index (and writers) to `to`.
fn send(from: &Dev, to: &mut Dev) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let entries = from.core.announce_since(0, usize::MAX).unwrap();
    to.core.apply_remote(from.core.me(), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
}

/// Serves chunks from other devices' disks, and remembers every path asked for.
struct Disks<'a> {
    disks: Vec<&'a MemFs>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl ChunkSource for Disks<'_> {
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        reqs.iter()
            .map(|r| {
                self.asked.lock().unwrap().push(r.path.clone());
                for fs in &self.disks {
                    if let Ok(fh) = fs.open_read(&r.path) {
                        let mut buf = vec![0; r.len as usize];
                        if fh.read_at(r.offset, &mut buf).ok() == Some(buf.len()) {
                            return Ok(buf);
                        }
                    }
                }
                Err(FetchError::Unavailable)
            })
            .collect()
    }
}

/// Pulls until nothing more happens; returns the report totals and every path whose chunks were asked for.
fn pull(to: &mut Dev, from: &[&Dev]) -> (PullReport, Vec<String>) {
    let source = Disks { disks: from.iter().map(|d| &d.fs).collect(), asked: Default::default() };
    let name = |_: seedwell_sync_proto::ids::ShortId| "other".to_string();
    let mut total = PullReport::default();
    loop {
        let r = to.core.pull(&source, &name, 64 << 20).unwrap();
        total.done += r.done;
        total.failed.extend(r.failed.clone());
        total.conflicts.extend(r.conflicts.clone());
        if r.done == 0 {
            total.waiting = r.waiting;
            total.unsupported = r.unsupported;
            break;
        }
    }
    let asked = source.asked.into_inner().unwrap();
    (total, asked)
}

/// The paths this device has records of, below `prefix`.
fn recorded(d: &Dev, prefix: &str) -> Vec<String> {
    d.core.store().local_paths(prefix, None, usize::MAX).unwrap()
}

/// What Claude Code wrote: a bucket file and a folder of skill files, below `synced/`.
fn write_synced(fs: &MemFs) {
    if fs.lstat("synced").unwrap().is_none() {
        fs.create_dir("synced").unwrap();
    }
    write_all(fs, "synced/.bucket-1", b"bucket").unwrap();
    fs.create_dir("synced/u1_u2").unwrap();
    for i in 0..5 {
        write_all(fs, &format!("synced/u1_u2/skill{i}.md"), format!("skill {i}").as_bytes()).unwrap();
    }
}

const RULES: &[u8] = b"// Claude Code's own state\n/synced/\n/_snapshots/\n.venv/\n__pycache__/\n*.pyc\n";

#[test]
fn rules_written_after_the_folder_started_apply_to_every_later_scan() {
    let mut a = dev(1, true, &[]);
    write_all(&a.fs, "real.md", b"a skill").unwrap();
    a.core.scan("").unwrap();
    // The rules arrive while the folder runs (here written; a pull is the other way, below).
    write_all(&a.fs, ".seedwellignore", RULES).unwrap();
    a.core.scan(".seedwellignore").unwrap();
    assert!(a.core.take_ignore_changed(), "a whole scan is due");
    assert!(!a.core.take_ignore_changed());
    // Then the directory appears: found by a watcher event for the directory, for one file in it, by a scan in steps
    // (as the first scan runs), and by a full scan.
    write_synced(&a.fs);
    a.fs.create_dir("tool").unwrap();
    a.fs.create_dir("tool/__pycache__").unwrap();
    write_all(&a.fs, "tool/__pycache__/m.pyc", b"bytecode").unwrap();
    write_all(&a.fs, "tool/run.py", b"print()").unwrap();
    a.core.scan("synced").unwrap();
    a.core.scan("synced/u1_u2").unwrap();
    a.core.scan("synced/u1_u2/skill0.md").unwrap();
    a.core.scan("tool/__pycache__/m.pyc").unwrap();
    let mut job = ScanJob::stepped("", std::time::Duration::from_millis(1), 1);
    while !a.core.scan_step(&mut job).unwrap() {
        let tasks = job.take_to_hash();
        let fs = a.core.fs();
        job.hashed(tasks.iter().map(|t| t.run(&*fs)).collect());
    }
    a.core.scan("").unwrap();
    assert_eq!(recorded(&a, "synced"), Vec::<String>::new());
    assert_eq!(recorded(&a, "tool"), ["tool", "tool/run.py"]);
    let announced: Vec<String> = a.core.announce_since(0, usize::MAX).unwrap().into_iter().map(|e| e.file.info.path).collect();
    assert!(announced.iter().all(|p| !p.starts_with("synced") && !p.contains("__pycache__")), "{announced:?}");
    // Changed again: the next scan step reads it.
    write_all(&a.fs, ".seedwellignore", b"/other/\n").unwrap();
    let mut job = ScanJob::new("other");
    a.core.scan_step(&mut job).unwrap();
    assert!(a.core.take_ignore_changed());
    a.core.scan("").unwrap();
    assert_eq!(recorded(&a, "synced").len(), 8, "no longer ignored: picked up by the next whole scan");
}

#[test]
fn rules_pulled_in_apply_to_what_is_written_here_afterwards() {
    // The owner has the rules and nothing they ignore; the PC joins, gets the rules, then the files appear on it.
    let mut mac = dev(1, true, &[]);
    write_all(&mac.fs, ".seedwellignore", RULES).unwrap();
    write_all(&mac.fs, "real.md", b"a skill").unwrap();
    mac.core.scan("").unwrap();
    let mut pc = dev(2, true, &[]);
    send(&mac, &mut pc);
    pull(&mut pc, &[&mac]);
    assert_eq!(read_all(&pc.fs, ".seedwellignore").unwrap(), RULES);
    assert!(pc.core.take_ignore_changed(), "read as it arrived");
    write_synced(&pc.fs);
    pc.core.scan("synced/u1_u2").unwrap();
    pc.core.scan("").unwrap();
    assert_eq!(recorded(&pc, "synced"), Vec::<String>::new());
    assert!(pc.core.announce_since(0, usize::MAX).unwrap().iter().all(|e| !e.file.info.path.starts_with("synced")));
}

/// A device whose patterns ignore a path does not need what others announce there: nothing is fetched, counted or
/// waited for, and a change, a delete or a conflict there leaves its own file alone.
#[test]
fn what_others_announce_at_a_path_this_device_ignores_is_never_needed() {
    let mut a = dev(1, true, &[]);
    a.fs.create_dir("cache").unwrap();
    a.fs.create_dir("cache/sub").unwrap();
    write_all(&a.fs, "cache/big.bin", &[7; 100_000]).unwrap();
    write_all(&a.fs, "cache/sub/x.txt", b"x").unwrap();
    write_all(&a.fs, "doc.md", b"doc").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev(2, true, &["cache/"]);
    b.fs.create_dir("cache").unwrap();
    write_all(&b.fs, "cache/big.bin", b"B's own, ignored here").unwrap();
    b.core.scan("").unwrap();
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap(), (1, 3), "doc.md only");
    let (r, asked) = pull(&mut b, &[&a]);
    assert!(r.failed.is_empty() && r.waiting == 0, "{r:?}");
    assert!(asked.iter().all(|p| !p.starts_with("cache")), "{asked:?}");
    assert_eq!(read_all(&b.fs, "doc.md").unwrap(), b"doc");
    assert!(b.fs.lstat("cache/sub").unwrap().is_none());
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));

    // A changes one, deletes the other, then the whole directory: nothing here moves.
    write_all(&a.fs, "cache/big.bin", &[8; 100_001]).unwrap();
    a.fs.remove_file("cache/sub/x.txt").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));
    let (r, asked) = pull(&mut b, &[&a]);
    assert!(asked.is_empty() && r.conflicts.is_empty() && r.failed.is_empty(), "{r:?} {asked:?}");
    a.fs.remove_file("cache/big.bin").unwrap();
    a.fs.remove_dir("cache/sub").unwrap();
    a.fs.remove_dir("cache").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "cache/big.bin").unwrap(), b"B's own, ignored here");
    assert!(b.core.store().local_paths("", None, usize::MAX).unwrap().iter().all(|p| !p.starts_with("cache")));
}

/// The owner indexed files below `synced/` before its rules applied (an earlier build); the joining device follows the
/// rules. They come first in its pull, and what they ignore is never fetched, even in the same index batch.
#[test]
fn rules_pulled_in_are_taken_first_so_what_they_ignore_is_never_fetched() {
    let mut old = dev(1, false, &[]);
    write_all(&old.fs, ".seedwellignore", RULES).unwrap();
    write_synced(&old.fs);
    write_all(&old.fs, "real.md", b"a skill").unwrap();
    old.core.scan("").unwrap();
    assert_eq!(recorded(&old, "synced").len(), 8);
    let mut pc = dev(2, true, &[]);
    send(&old, &mut pc);
    let (r, asked) = pull(&mut pc, &[&old]);
    assert!(r.failed.is_empty() && r.waiting == 0, "{r:?}");
    assert!(asked.iter().all(|p| !p.starts_with("synced")), "{asked:?}");
    assert!(pc.fs.lstat("synced").unwrap().is_none());
    assert_eq!(read_all(&pc.fs, "real.md").unwrap(), b"a skill");
    assert_eq!(pc.core.need_totals().unwrap(), (0, 0));
}

/// Needs stored with other patterns (the rules changed while the folder was closed; or an index of an earlier build,
/// whose needs never left ignored paths out) are worked out again when the folder opens.
#[test]
fn needs_stored_with_other_patterns_are_worked_out_again_when_the_folder_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.redb");
    let mut pc = dev(1, false, &[]);
    write_synced(&pc.fs);
    write_all(&pc.fs, "real.md", b"a skill").unwrap();
    pc.core.scan("").unwrap();
    let fs = MemFs::new();
    let store = || Box::new(seedwell_sync::store_redb::RedbStore::open(&path, 5).unwrap());
    let mut mac = dev_with(2, store(), fs.clone(), true, &[]);
    send(&pc, &mut mac);
    assert_eq!(mac.core.need_totals().unwrap().0, 9, "all of it, without rules");
    drop(mac);
    // The rules arrive while it is closed (an index from before 0.1.17 is the same case: no patterns noted).
    write_all(&fs, ".seedwellignore", RULES).unwrap();
    let mac = dev_with(2, store(), fs.clone(), true, &[]);
    assert_eq!(mac.core.need_totals().unwrap(), (1, 7), "real.md only");
    drop(mac);
    // And back.
    fs.remove_file(".seedwellignore").unwrap();
    let mac = dev_with(2, store(), fs, true, &[]);
    assert_eq!(mac.core.need_totals().unwrap().0, 9);
}

/// Paths already in this device's index when its patterns start to ignore them (the PC's 228 files): they stay on disk
/// and in its index, but are announced again as invalid, so other devices stop counting them, and the folder's file
/// count leaves them out. Nothing is deleted anywhere. Ignored no longer, they are announced as before.
#[test]
fn newly_ignored_files_stay_on_disk_and_are_no_longer_offered() {
    let mut pc = dev(1, true, &[]);
    write_all(&pc.fs, "real.md", b"a skill").unwrap();
    write_synced(&pc.fs);
    write_all(&pc.fs, "synced/gone.md", b"deleted before the rules came").unwrap();
    pc.core.scan("").unwrap();
    pc.fs.remove_file("synced/gone.md").unwrap();
    pc.core.scan("").unwrap();
    assert_eq!(pc.core.local_totals().unwrap().0, 7);
    // One device took the files already, another has not yet; neither ignores them.
    let mut took = dev(2, false, &[]);
    send(&pc, &mut took);
    pull(&mut took, &[&pc]);
    assert_eq!(took.core.need_totals().unwrap(), (0, 0));
    let mut late = dev(3, false, &[]);
    let (_, seq) = pc.core.index_state().unwrap();

    write_all(&pc.fs, ".seedwellignore", RULES).unwrap();
    pc.core.scan("").unwrap();
    let again = pc.core.announce_since(seq, usize::MAX).unwrap();
    let synced: Vec<_> = again.iter().filter(|e| e.file.info.path.starts_with("synced")).collect();
    assert_eq!(synced.len(), 8, "announced again: {again:?}");
    assert!(synced.iter().all(|e| e.invalid && !e.file.info.deleted));
    let all = pc.core.announce_since(0, usize::MAX).unwrap();
    let gone = all.iter().find(|e| e.file.info.path == "synced/gone.md").unwrap();
    assert!(gone.file.info.deleted && !gone.invalid, "its own delete goes out as it was");
    assert!(pc.fs.lstat("synced/u1_u2/skill0.md").unwrap().is_some(), "left on disk");
    assert_eq!(recorded(&pc, "synced").len(), 9, "and in the index");
    assert_eq!(pc.core.local_totals().unwrap(), (2, 7 + RULES.len() as u64), "real.md and the rules");

    send(&pc, &mut took);
    send(&pc, &mut late);
    assert_eq!(took.core.need_totals().unwrap(), (1, RULES.len() as u64), "the rules only");
    assert_eq!(late.core.need_totals().unwrap(), (2, 7 + RULES.len() as u64), "the rules and real.md");
    let (_, asked) = pull(&mut took, &[&pc]);
    assert_eq!(asked, [".seedwellignore"]);
    assert!(took.fs.lstat("synced/u1_u2/skill0.md").unwrap().is_some(), "nothing deleted where they arrived before");
    let (r, asked) = pull(&mut late, &[&pc]);
    assert!(r.failed.is_empty() && r.waiting == 0, "{r:?}");
    assert!(asked.iter().all(|p| !p.starts_with("synced")), "{asked:?}");
    assert!(late.fs.lstat("synced").unwrap().is_none());
    assert_eq!(late.core.need_totals().unwrap(), (0, 0));

    // Ignored no longer: announced as before, and a skill changed meanwhile is a new version.
    write_all(&pc.fs, "synced/u1_u2/skill1.md", b"edited while ignored").unwrap();
    pc.fs.remove_file(".seedwellignore").unwrap();
    pc.core.scan("").unwrap();
    assert_eq!(pc.core.local_totals().unwrap().0, 7);
    let all = pc.core.announce_since(0, usize::MAX).unwrap();
    assert!(all.iter().filter(|e| e.file.info.path.starts_with("synced")).all(|e| !e.invalid));
    send(&pc, &mut late);
    pull(&mut late, &[&pc]);
    assert_eq!(read_all(&late.fs, "synced/u1_u2/skill1.md").unwrap(), b"edited while ignored");
    assert_eq!(read_all(&late.fs, "synced/u1_u2/skill2.md").unwrap(), b"skill 2");
    assert_eq!(late.core.need_totals().unwrap(), (0, 0));
}

/// The PC's index as an earlier build left it: files below `synced/` indexed although the rules ignore them. Opened by
/// this build, they are announced again as not offered, without a scan; opened again, nothing more is announced.
#[test]
fn files_an_earlier_build_indexed_are_no_longer_offered_once_the_folder_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.redb");
    let store = || Box::new(seedwell_sync::store_redb::RedbStore::open(&path, 5).unwrap());
    let fs = MemFs::new();
    let mut pc = dev_with(1, store(), fs.clone(), false, &[]);
    write_all(&fs, ".seedwellignore", RULES).unwrap();
    write_all(&fs, "real.md", b"a skill").unwrap();
    write_synced(&fs);
    pc.core.scan("").unwrap();
    assert_eq!(recorded(&pc, "synced").len(), 8, "indexed: the rules never applied");
    let (_, seq) = pc.core.index_state().unwrap();
    drop(pc);
    let pc = dev_with(1, store(), fs.clone(), true, &[]);
    let again = pc.core.announce_since(seq, usize::MAX).unwrap();
    assert_eq!(again.len(), 8, "{again:?}");
    assert!(again.iter().all(|e| e.file.info.path.starts_with("synced") && e.invalid));
    assert_eq!(pc.core.local_totals().unwrap(), (2, 7 + RULES.len() as u64));
    assert!(fs.lstat("synced/u1_u2/skill4.md").unwrap().is_some());
    drop(pc);
    let pc = dev_with(1, store(), fs, true, &[]);
    assert_eq!(pc.core.index_state().unwrap().1, seq + 8, "nothing announced again");
    assert_eq!(pc.core.local_totals().unwrap(), (2, 7 + RULES.len() as u64));
}

/// A later build's built-in list ignores more (Windows Explorer's `Thumbs.db` and `desktop.ini`): an index an earlier
/// build left, with those indexed and needed, is worked out again when the folder opens, as for changed rules.
#[test]
fn a_changed_built_in_list_is_applied_to_an_index_an_earlier_build_left() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.redb");
    let store = || Box::new(seedwell_sync::store_redb::RedbStore::open(&path, 5).unwrap());
    let earlier = || Ignore::from_identity("");
    assert!(!earlier().is_ignored("Thumbs.db", false));
    let fs = MemFs::new();
    let mut pc = dev_ignoring(1, store(), fs.clone(), earlier(), None);
    fs.create_dir("pics").unwrap();
    write_all(&fs, "pics/Thumbs.db", b"thumbnails").unwrap();
    write_all(&fs, "pics/desktop.ini", b"[.ShellClassInfo]").unwrap();
    write_all(&fs, "pics/a.jpg", b"jpeg").unwrap();
    pc.core.scan("").unwrap();
    // Another device of the earlier build announces its own.
    let mut other = dev_ignoring(2, Box::new(MemStore::new(2)), MemFs::new(), earlier(), None);
    other.fs.create_dir("docs").unwrap();
    write_all(&other.fs, "docs/Thumbs.db", b"its thumbnails").unwrap();
    other.core.scan("").unwrap();
    send(&other, &mut pc);
    assert_eq!(pc.core.need_totals().unwrap().0, 2, "docs and its Thumbs.db");
    assert_eq!(pc.core.local_totals().unwrap().0, 3);
    let (_, seq) = pc.core.index_state().unwrap();
    drop(pc);

    let pc = dev_ignoring(1, store(), fs.clone(), Ignore::defaults(), None);
    let again = pc.core.announce_since(seq, usize::MAX).unwrap();
    let mut paths: Vec<(&str, bool)> = again.iter().map(|e| (e.file.info.path.as_str(), e.invalid)).collect();
    paths.sort();
    assert_eq!(paths, [("pics/Thumbs.db", true), ("pics/desktop.ini", true)]);
    assert_eq!(pc.core.local_totals().unwrap(), (1, 4), "a.jpg");
    assert_eq!(pc.core.need_totals().unwrap().0, 1, "docs only");
    assert!(fs.lstat("pics/Thumbs.db").unwrap().is_some(), "left on disk");
    drop(pc);
    let pc = dev_ignoring(1, store(), fs, Ignore::defaults(), None);
    assert_eq!(pc.core.index_state().unwrap().1, seq + 2, "once");
}

/// The machine-specific lines of a folder of skills, marked deletable, and one that is not.
const DELETABLE_RULES: &[u8] = b"(?d)__pycache__/\n(?d)*.pyc\n(?d).venv/\nnode_modules/\n";

/// Files in a folder's archive.
fn archived(fs: &MemFs) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![".seedwell/Archive".to_string()];
    while let Some(d) = stack.pop() {
        let Ok(l) = fs.read_dir(&d) else { continue };
        for e in l.entries {
            let p = format!("{d}/{}", e.name);
            if fs.lstat(&p).unwrap().is_some_and(|m| m.kind == seedwell_sync_proto::model::FileKind::Dir) {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// A folder another device deleted, where all that is left here is deletable ((?d) lines, OS metadata): it goes, and
/// what was in it goes to the archive, as a deleted file goes.
#[test]
fn a_deleted_folder_takes_deletable_ignored_files_with_it() {
    let mut a = dev(1, true, &[]);
    write_all(&a.fs, ".seedwellignore", DELETABLE_RULES).unwrap();
    a.fs.create_dir("skill").unwrap();
    write_all(&a.fs, "skill/run.py", b"print()").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev(2, true, &[]);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    // Made on B only: caches, a virtual environment, Finder's and Explorer's files.
    for d in ["skill/__pycache__", "skill/.venv", "skill/.venv/bin"] {
        b.fs.create_dir(d).unwrap();
    }
    for f in ["skill/__pycache__/run.cpython-313.pyc", "skill/.venv/bin/python", "skill/.DS_Store", "skill/Thumbs.db", "skill/x.pyc"] {
        write_all(&b.fs, f, b"junk").unwrap();
    }
    b.core.scan("").unwrap();
    assert_eq!(b.core.store().local_paths("skill", None, usize::MAX).unwrap(), ["skill", "skill/run.py"]);

    a.fs.remove_file("skill/run.py").unwrap();
    a.fs.remove_dir("skill").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let (r, _) = pull(&mut b, &[&a]);
    assert!(r.failed.is_empty() && r.unsupported.is_empty(), "{r:?}");
    assert!(b.fs.lstat("skill").unwrap().is_none(), "gone, junk and all");
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));
    let kept = archived(&b.fs);
    assert_eq!(kept.len(), 6, "run.py and the five ignored files, in the archive: {kept:?}");
}

/// Ignored files that are not deletable keep the folder: said once, as an issue, not tried again and not waited for,
/// until something there changes; once they are gone the folder goes too.
#[test]
fn ignored_files_that_are_not_deletable_keep_the_folder_and_say_so_once() {
    let mut a = dev(1, true, &[]);
    write_all(&a.fs, ".seedwellignore", DELETABLE_RULES).unwrap();
    a.fs.create_dir("tool").unwrap();
    write_all(&a.fs, "tool/index.js", b"js").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev(2, true, &[]);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    b.fs.create_dir("tool/node_modules").unwrap();
    write_all(&b.fs, "tool/node_modules/m.js", b"module").unwrap();
    write_all(&b.fs, "tool/.DS_Store", b"finder").unwrap();
    b.core.scan("").unwrap();

    a.fs.remove_file("tool/index.js").unwrap();
    a.fs.remove_dir("tool").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let (r, _) = pull(&mut b, &[&a]);
    assert!(r.failed.is_empty() && r.waiting == 0, "{r:?}");
    assert_eq!(r.unsupported.len(), 1, "{r:?}");
    assert_eq!(r.unsupported[0].path, "tool");
    assert!(r.unsupported[0].why.contains("tool/node_modules") && r.unsupported[0].why.contains("(?d)"), "{}", r.unsupported[0].why);
    assert_eq!(b.core.need_totals().unwrap(), (0, 0), "not waited for");
    assert_eq!(read_all(&b.fs, "tool/node_modules/m.js").unwrap(), b"module");
    // Not tried again by the next pulls, nor after a scan elsewhere.
    b.core.scan("other").unwrap();
    assert!(!b.core.take_retry_due());
    let (r, _) = pull(&mut b, &[&a]);
    assert_eq!(r.done, 0);
    // The modules go: the scan there lets the step be tried again, and the folder goes (Finder's file with it).
    b.fs.remove_file("tool/node_modules/m.js").unwrap();
    b.fs.remove_dir("tool/node_modules").unwrap();
    b.core.scan("tool/node_modules").unwrap();
    assert!(b.core.take_retry_due());
    assert_eq!(b.core.need_totals().unwrap(), (0, 0), "not waited for until the pull has tried it again");
    let (r, _) = pull(&mut b, &[&a]);
    assert!(r.unsupported.is_empty() && r.failed.is_empty(), "{r:?}");
    assert!(b.fs.lstat("tool").unwrap().is_none());
    assert_eq!(b.core.store().need_totals().unwrap(), (0, 0));
}

/// The first line that matches decides, also below a deletable folder: a plain `*.pyc` before `(?d)__pycache__/`
/// keeps a `.pyc` in it, so the folder stays.
#[test]
fn a_file_below_a_deletable_folder_that_an_earlier_plain_line_ignores_keeps_it() {
    let mut a = dev(1, true, &[]);
    write_all(&a.fs, ".seedwellignore", b"*.pyc\n(?d)__pycache__/\n").unwrap();
    a.fs.create_dir("s").unwrap();
    write_all(&a.fs, "s/run.py", b"print()").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev(2, true, &[]);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    b.fs.create_dir("s/__pycache__").unwrap();
    write_all(&b.fs, "s/__pycache__/run.pyc", b"bytecode").unwrap();
    a.fs.remove_file("s/run.py").unwrap();
    a.fs.remove_dir("s").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let (r, _) = pull(&mut b, &[&a]);
    assert_eq!(r.unsupported.iter().map(|u| u.path.as_str()).collect::<Vec<_>>(), ["s"]);
    assert!(r.unsupported[0].why.contains("s/__pycache__/run.pyc"), "{}", r.unsupported[0].why);
    assert!(b.fs.lstat("s/__pycache__/run.pyc").unwrap().is_some());
}

/// This device's own patterns apply when they change, after the folder's `.seedwellignore`, as a changed file does.
#[test]
fn own_patterns_apply_when_they_change() {
    let mut a = dev(1, true, &[]);
    a.fs.create_dir("cache").unwrap();
    write_all(&a.fs, "cache/a.bin", b"a").unwrap();
    write_all(&a.fs, "doc.md", b"doc").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev(2, true, &[]);
    write_all(&b.fs, ".seedwellignore", b"/synced/\n").unwrap();
    b.core.scan("").unwrap();
    b.fs.create_dir("mine").unwrap();
    write_all(&b.fs, "mine/x.txt", b"B's").unwrap();
    b.core.scan("").unwrap();
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap().0, 3, "cache, cache/a.bin, doc.md");
    let (_, seq) = b.core.index_state().unwrap();

    b.core.set_local_ignore(&["cache/".into(), "mine/".into()]).unwrap();
    assert!(b.core.take_ignore_changed(), "a full scan is due");
    assert_eq!(b.core.need_totals().unwrap(), (1, 3), "doc.md only");
    let again = b.core.announce_since(seq, usize::MAX).unwrap();
    assert!(again.len() == 2 && again.iter().all(|e| e.file.info.path.starts_with("mine") && e.invalid), "{again:?}");
    // With the file's patterns still: synced/ stays ignored.
    write_synced(&b.fs);
    b.core.scan("").unwrap();
    assert!(recorded(&b, "synced").is_empty());
    assert_eq!(b.core.local_totals().unwrap().0, 1, ".seedwellignore");

    // And gone again.
    b.core.set_local_ignore(&[]).unwrap();
    assert!(b.core.take_ignore_changed());
    assert_eq!(b.core.need_totals().unwrap().0, 3);
    assert!(b.core.announce_since(0, usize::MAX).unwrap().iter().filter(|e| e.file.info.path.starts_with("mine")).all(|e| !e.invalid));
    // A core that does not follow the file takes them alone.
    let mut c = dev(3, false, &[]);
    c.core.set_local_ignore(&["cache/".into()]).unwrap();
    send(&a, &mut c);
    assert_eq!(c.core.need_totals().unwrap().0, 1);
}
