//! Regressions found by the simulator (`sim.rs`), each reduced to a few devices driven by hand.
//!

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use trs_sync::clock::Clock;
use trs_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
use trs_sync::fs::{FileHandle, Fs, Listing, MemFs, MemFsOptions, Meta, read_all, write_all};
use trs_sync::ignore::Ignore;
use trs_sync::index::MemStore;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError, PullReport};
use trs_sync_proto::device::DeviceKey;

use crate::flow::{Key, learn_writers};
use trs_sync_proto::ids::ShortId;

const S: i64 = 1_000_000_000;

struct Utc;
impl Clock for Utc {
    fn now_ns(&self) -> i64 {
        1_792_938_725 * S
    }
    fn local_offset_s(&self, _: i64) -> i32 {
        0
    }
}

struct Dev {
    fs: MemFs,
    core: FolderCore,
    name: &'static str,
}

fn rw() -> Key {
    Key::rw([7; 32])
}

/// A device on its own disk (`ci`: case-insensitive, like APFS by default).
fn dev(name: &'static str, seed: u8, key: &Key, ci: bool) -> Dev {
    let fs = MemFs::with_options(MemFsOptions { case_insensitive: ci, ..MemFsOptions::default() });
    fs.create_dir(".seedwell").unwrap();
    let view: Arc<dyn Fs> = Arc::new(fs.clone());
    dev_on(name, seed, key, fs, view, ci, u64::from(seed))
}

/// A device whose core sees the disk through `view`, with this index id.
fn dev_on(name: &'static str, seed: u8, key: &Key, fs: MemFs, view: Arc<dyn Fs>, ci: bool, index_id: u64) -> Dev {
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive: ci,
        byte_exact_names: false,
        archive: true,
    };
    let ids = Arc::new(AtomicU64::new((u64::from(seed) << 32) + (index_id << 16)));
    let new_id = Arc::new(move || {
        let n = ids.fetch_add(1, Ordering::SeqCst);
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.to_be_bytes());
        id
    });
    let core = FolderCore::new(
        cfg,
        Box::new(MemStore::new(index_id)),
        view,
        Some(dk.clone()),
        Arc::new(Utc),
        new_id,
        Ignore::defaults(),
    )
    .unwrap();
    Dev { fs, core, name }
}

fn short(d: &Dev) -> ShortId {
    d.core.me()
}

/// Serves chunks from other devices' disks, checking each against its hash like a peer would.
struct Disks<'a>(Vec<&'a MemFs>);

impl ChunkSource for Disks<'_> {
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        reqs.iter()
            .map(|r| {
                for fs in &self.0 {
                    if let Ok(fh) = fs.open_read(&r.path) {
                        let mut buf = vec![0; r.len as usize];
                        if fh.read_at(r.offset, &mut buf).ok() == Some(buf.len())
                            && <[u8; 32]>::from(Sha256::digest(&buf)) == r.hash
                        {
                            return Ok(buf);
                        }
                    }
                }
                Err(FetchError::Unavailable)
            })
            .collect()
    }
}

/// `from`'s whole index (and writers) to `to`.
fn send(from: &Dev, to: &mut Dev) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let entries = from.core.announce_since(0, usize::MAX).unwrap();
    to.core.apply_remote(short(from), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
}

/// Pull rounds until one does nothing.
fn pull(to: &mut Dev, from: &[&Dev]) -> PullReport {
    let source = Disks(from.iter().map(|d| &d.fs).collect());
    // Devices are named by their key seed (1 = A, 2 = B, ...), the same on every device.
    let name = |id: ShortId| {
        (1u8..=4)
            .find(|&s| DeviceKey::from_seed(&[s; 32]).device_id().short() == id)
            .map_or("?".to_string(), |s| char::from(b'A' + s - 1).to_string())
    };
    let mut total = PullReport::default();
    for _ in 0..20 {
        let r = to.core.pull(&source, &name, 64 << 20).unwrap();
        total.done += r.done;
        total.conflicts.extend(r.conflicts.clone());
        total.failed.extend(r.failed.clone());
        if r.done == 0 {
            total.rescan = r.rescan;
            total.waiting = r.waiting;
            return total;
        }
    }
    panic!("{}: pull rounds do not end", to.name);
}

/// Everyone scans, tells everyone, and pulls, `rounds` times.
fn settle(devs: &mut [&mut Dev], rounds: usize) {
    for _ in 0..rounds {
        for d in devs.iter_mut() {
            d.core.scan("").unwrap();
        }
        for i in 0..devs.len() {
            for j in 0..devs.len() {
                if i != j {
                    let (a, b) = if i < j {
                        let (x, y) = devs.split_at_mut(j);
                        (&*x[i], &mut *y[0])
                    } else {
                        let (x, y) = devs.split_at_mut(i);
                        (&*y[0], &mut *x[j])
                    };
                    send(a, b);
                }
            }
        }
        for i in 0..devs.len() {
            let (before, rest) = devs.split_at_mut(i);
            let (me, after) = rest.split_first_mut().unwrap();
            let others: Vec<&Dev> = before.iter().chain(after.iter()).map(|d| &**d).collect();
            pull(me, &others);
        }
    }
}

/// Every file's content on a disk (outside `.seedwell`).
fn contents(fs: &MemFs) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        for e in fs.read_dir(&dir).unwrap().entries {
            let p = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if p == ".seedwell" {
                continue;
            }
            match fs.lstat(&p).unwrap().unwrap().kind {
                trs_sync_proto::model::FileKind::Dir => stack.push(p),
                trs_sync_proto::model::FileKind::File => out.push(read_all(fs, &p).unwrap()),
                _ => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Fixed

/// The puller had no marker check: with the disk unmounted it wrote new files into the empty mount
/// point, and an archive step there would create `.seedwell/`, after which a scan reads every file as
/// deleted and sends that to every device.
#[test]
fn a_pull_never_writes_into_a_folder_without_its_marker() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    write_all(&a.fs, "f", b"one").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    write_all(&a.fs, "g", b"new").unwrap();
    write_all(&a.fs, "f", b"two").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    b.fs.remove_dir(".seedwell").unwrap();
    let source = Disks(vec![&a.fs]);
    assert!(b.core.pull(&source, &|_| "A".into(), 1 << 20).is_err(), "folder missing");
    assert!(b.fs.lstat("g").unwrap().is_none(), "nothing written");
    assert!(b.fs.lstat(".seedwell").unwrap().is_none(), "the marker is never made by the puller");
}

/// A read-only device recorded its local delete again on every scan (a new sequence each time, sent
/// to every peer forever).
#[test]
fn a_read_only_device_records_a_local_delete_once() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key, false), dev("R", 3, &ro, false));
    write_all(&a.fs, "f", b"from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);

    r.fs.remove_file("f").unwrap();
    assert_eq!(r.core.scan("").unwrap().deleted, 1);
    let (_, seq) = r.core.index_state().unwrap();
    let again = r.core.scan("").unwrap();
    assert_eq!((again.changed, again.deleted), (0, 0));
    assert_eq!(r.core.index_state().unwrap().1, seq, "nothing new to announce");
}

/// ... and a remote delete of a path it had deleted locally then waited for a rescan forever (the
/// record still said "a file is here").
#[test]
fn a_read_only_local_delete_takes_the_remote_delete() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key, false), dev("R", 3, &ro, false));
    write_all(&a.fs, "f", b"from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    r.fs.remove_file("f").unwrap();
    r.core.scan("").unwrap();

    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    let p = pull(&mut r, &[&a]);
    assert!(p.rescan.is_empty(), "{p:?}");
    assert_eq!(r.core.need_totals().unwrap().0, 0);
}

/// A read-only device's own new file kept its first description: replaced by a symlink (another
/// kind), it looked changed on every scan.
#[test]
fn a_read_only_local_file_that_changes_kind_settles() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let mut r = dev("R", 3, &ro, false);
    write_all(&r.fs, "x", b"local").unwrap();
    r.core.scan("").unwrap();
    r.fs.remove_file("x").unwrap();
    r.fs.symlink("elsewhere", "x").unwrap();
    assert_eq!(r.core.scan("").unwrap().changed, 1);
    assert_eq!(r.core.scan("").unwrap().changed, 0, "unchanged now");
}

/// A read-only device's own file at a path it had received as deleted: a newer delete of that path
/// waited for a rescan forever; and once the user deleted that file, a change arriving waited forever
/// too. The local file is the user's: it stays, under the newer delete.
#[test]
fn a_read_only_local_file_over_a_received_delete() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key, false), dev("R", 3, &ro, false));
    write_all(&a.fs, "f", b"one").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    assert!(r.fs.lstat("f").unwrap().is_none());

    write_all(&r.fs, "f", b"R's own").unwrap();
    r.core.scan("").unwrap();
    // A newer delete: A makes the file again and deletes it.
    write_all(&a.fs, "f", b"two").unwrap();
    a.core.scan("").unwrap();
    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    let p = pull(&mut r, &[&a]);
    assert!(p.rescan.is_empty(), "{p:?}");
    assert_eq!(r.core.need_totals().unwrap().0, 0);
    assert_eq!(read_all(&r.fs, "f").unwrap(), b"R's own", "the local file stays");

    r.fs.remove_file("f").unwrap();
    r.core.scan("").unwrap();
    write_all(&a.fs, "f", b"three").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    let p = pull(&mut r, &[&a]);
    assert!(p.rescan.is_empty(), "{p:?}");
    assert_eq!(read_all(&r.fs, "f").unwrap(), b"three");
}

/// A writer moves a file onto a path where a read-only device has its own file over a received
/// delete: the planner paired the move into a rename onto that file, which waited for a rescan
/// forever. Now it is a fetch, and the local file becomes a conflict copy.
#[test]
fn a_move_onto_a_read_only_local_file_keeps_that_file() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key, false), dev("R", 3, &ro, false));
    write_all(&a.fs, "s", b"moved content").unwrap();
    write_all(&a.fs, "t", b"old t").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    a.fs.remove_file("t").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    write_all(&r.fs, "t", b"R's own t").unwrap();
    r.core.scan("").unwrap();

    a.fs.rename("s", "t").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    let p = pull(&mut r, &[&a]);
    assert!(p.rescan.is_empty(), "{p:?}");
    assert_eq!(read_all(&r.fs, "t").unwrap(), b"moved content");
    assert!(contents(&r.fs).contains(&b"R's own t".to_vec()), "kept as a conflict copy");
    assert!(r.fs.lstat("s").unwrap().is_none());
}

/// Two devices delete one path concurrently. The one that took the other's delete dropped its own
/// delete's version, so the version that delete had replaced (still on a third device) beat the
/// remaining delete ("a change beats a delete") and the deleted file came back everywhere.
#[test]
fn concurrent_deletes_keep_both_histories() {
    let key = rw();
    let (mut a, mut b, mut c) = (dev("A", 1, &key, false), dev("B", 2, &key, false), dev("C", 3, &key, false));
    write_all(&a.fs, "f", b"X, A's file").unwrap();
    a.fs.set_mtime("f", 1_000 * S).unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "f", b"Y, B's file").unwrap();
    b.fs.set_mtime("f", 2_000 * S).unwrap();
    b.core.scan("").unwrap();
    b.fs.remove_file("f").unwrap();
    b.core.scan("").unwrap();
    send(&a, &mut c);
    pull(&mut c, &[&a]);
    assert_eq!(read_all(&c.fs, "f").unwrap(), b"X, A's file");

    // A's file beats B's concurrent delete; then A deletes it too. B's delete wins over A's (newer
    // mtime), and A takes it.
    send(&b, &mut a);
    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    pull(&mut a, &[&b]);

    settle(&mut [&mut a, &mut b, &mut c], 3);
    for d in [&a, &b, &c] {
        assert!(d.fs.lstat("f").unwrap().is_none(), "{} has the deleted file back", d.name);
    }
}

/// A peer reset its database while its full index send to us was cut off. The new index's full send
/// went on top of the old partial one: the old entries stayed, and so did the old highest sequence,
/// so the new index's next changes (lower sequences) were skipped as already held.
#[test]
fn a_full_send_of_a_new_index_starts_over() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    for i in 0..6 {
        write_all(&a.fs, &format!("old{i}"), b"x").unwrap();
    }
    a.core.scan("").unwrap();
    learn_writers(&mut b.core, &a.core.writers()).unwrap();
    let entries = a.core.announce_since(0, usize::MAX).unwrap();
    b.core.apply_remote(short(&a), RemoteIndex { index_id: 1, full: true, done: false, entries }).unwrap();

    // A loses its database (a new index id) and the connection; it comes back with one file left.
    for i in 1..6 {
        a.fs.remove_file(&format!("old{i}")).unwrap();
    }
    let view: Arc<dyn Fs> = Arc::new(a.fs.clone());
    let mut a2 = dev_on("A", 1, &key, a.fs.clone(), view, false, 99);
    a2.core.scan("").unwrap();
    let entries = a2.core.announce_since(0, usize::MAX).unwrap();
    b.core.apply_remote(short(&a2), RemoteIndex { index_id: 99, full: true, done: true, entries }).unwrap();
    let (_, top) = a2.core.index_state().unwrap();
    write_all(&a.fs, "new", b"y").unwrap();
    a2.core.scan("").unwrap();
    let entries = a2.core.announce_since(top, usize::MAX).unwrap();
    b.core.apply_remote(short(&a2), RemoteIndex { index_id: 99, full: false, done: false, entries }).unwrap();

    assert!(!b.core.store().remote("new").unwrap().is_empty(), "the change after the full send arrived");
    assert!(b.core.store().remote("old3").unwrap().is_empty(), "nothing of the old index is left");
}

/// A temp file left as a symlink (a symlink being fetched when the step stopped) made every later
/// fetch of a file at that path fail.
#[test]
fn a_leftover_symlink_temp_does_not_block_a_file() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    b.fs.symlink("somewhere", ".seedwell.f.tmp").unwrap();
    write_all(&a.fs, "f", b"content").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty(), "{p:?}");
    assert_eq!(read_all(&b.fs, "f").unwrap(), b"content");
}

/// A case-only rename on a case-insensitive disk: `lstat` finds the old spelling too, so the scanner
/// never recorded it gone (two live records for one file), and the puller took the new name for an
/// existing file and refused the rename.
#[test]
fn a_case_only_rename_on_a_case_insensitive_disk_syncs() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, true), dev("B", 2, &key, true));
    write_all(&a.fs, "report.txt", b"the report").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    a.fs.rename("report.txt", "Report.txt").unwrap();
    // As the watcher would: the old name, then the new one.
    a.core.scan("report.txt").unwrap();
    a.core.scan("Report.txt").unwrap();
    assert!(a.core.store().local("report.txt").unwrap().unwrap().file.info.deleted, "the old spelling is gone");
    assert!(!a.core.store().local("Report.txt").unwrap().unwrap().file.info.deleted);
    assert_eq!(a.core.scan("").unwrap().changed + a.core.scan("").unwrap().deleted, 0, "settled");

    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty() && p.rescan.is_empty(), "{p:?}");
    let names: Vec<String> = b.fs.read_dir("").unwrap().entries.into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"Report.txt".to_string()) && !names.contains(&"report.txt".to_string()), "{names:?}");
    assert_eq!(b.core.need_totals().unwrap().0, 0);
}

/// The disk the core sees, with a user edit right after the puller renamed its temp file into place.
struct EditAfterSwap {
    inner: MemFs,
    armed: Mutex<Option<(String, Vec<u8>)>>,
}

impl Fs for EditAfterSwap {
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        self.inner.lstat(rel)
    }
    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        self.inner.read_dir(rel)
    }
    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.inner.open_read(rel)
    }
    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.inner.open_rw(rel)
    }
    fn create_dir(&self, rel: &str) -> io::Result<()> {
        self.inner.create_dir(rel)
    }
    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        self.inner.remove_dir(rel)
    }
    fn remove_file(&self, rel: &str) -> io::Result<()> {
        self.inner.remove_file(rel)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)?;
        let armed = self.armed.lock().unwrap().take_if(|(p, _)| p == to);
        if let Some((p, data)) = armed {
            write_all(&self.inner, &p, &data)?;
        }
        Ok(())
    }
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.hard_link(from, to)
    }
    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.copy(from, to)
    }
    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        self.inner.set_mtime(rel, mtime_ns)
    }
    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        self.inner.set_exec(rel, exec)
    }
    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        self.inner.symlink(target, rel)
    }
    fn read_link(&self, rel: &str) -> io::Result<String> {
        self.inner.read_link(rel)
    }
    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        self.inner.sync_dir(rel)
    }
}

/// The puller recorded a file with the stamp it read after renaming it into place: a save by the user
/// in between was taken for the pulled version, never scanned, never synced.
#[test]
fn a_save_right_after_the_swap_is_not_taken_for_the_pulled_version() {
    let key = rw();
    let mut a = dev("A", 1, &key, false);
    let disk = MemFs::new();
    disk.create_dir(".seedwell").unwrap();
    let view = Arc::new(EditAfterSwap { inner: disk.clone(), armed: Mutex::new(None) });
    let mut b = dev_on("B", 2, &key, disk, view.clone(), false, 2);
    write_all(&a.fs, "f", b"v1").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    write_all(&a.fs, "f", b"v2 from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    *view.armed.lock().unwrap() = Some(("f".into(), b"B's save, right after the swap".to_vec()));
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "f").unwrap(), b"B's save, right after the swap");
    assert_eq!(b.core.scan("").unwrap().changed, 1, "the save is a change");
    assert_eq!(b.core.store().local("f").unwrap().unwrap().file.info.size, 30);
}

/// A conflict copy of a conflict copy put its suffix inside the first one's parentheses (`.28)` taken
/// for an extension).
#[test]
fn a_conflict_copy_of_a_conflict_copy_reads_well() {
    let p = trs_sync::conflict::conflict_path("f (conflict, A, 2026-01-01 00.28)", "B", "2026-01-01 00.30", 1);
    assert_eq!(p, "f (conflict, A, 2026-01-01 00.28) (conflict, B, 2026-01-01 00.30)");
    assert_eq!(trs_sync::conflict::conflict_path("a.txt", "B", "t", 1), "a (conflict, B, t).txt");
}

// ---------------------------------------------------------------------------------------------
// Design gaps, closed by rule changes

/// A device that took a change over its own concurrent delete recorded the change's version as it
/// was, dropping the delete's history. A version that delete had replaced, still on a device that had
/// not caught up, was then concurrent with the change and could win: the deleted content came back.
/// Here the deleter takes the change before anyone heard of its delete, so no holder of the change can
/// absorb it. Merging it on the deleter's side fixes this case but measured about 8x more resurrections
/// overall (a device that edits or deletes the change before it hears of the merge is concurrent with
/// it); a real fix needs the tombstone to name the content it deleted.
#[test]
#[ignore = "known gap: a delete the deleter never announced before taking a change over it cannot be absorbed"]
fn a_change_taken_over_a_delete_keeps_the_delete_history() {
    let key = rw();
    let (mut a, mut b, mut c) = (dev("A", 1, &key, false), dev("B", 2, &key, false), dev("C", 3, &key, false));
    write_all(&a.fs, "f", b"X, deleted by A").unwrap();
    a.fs.set_mtime("f", 3_000 * S).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut c);
    pull(&mut c, &[&a]);
    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "f", b"Y, made by B").unwrap();
    b.fs.set_mtime("f", 2_000 * S).unwrap();
    b.core.scan("").unwrap();
    send(&b, &mut a);
    pull(&mut a, &[&b]);
    assert_eq!(read_all(&a.fs, "f").unwrap(), b"Y, made by B", "a change beats a delete");

    settle(&mut [&mut a, &mut b, &mut c], 3);
    for d in [&a, &b, &c] {
        assert!(!contents(&d.fs).contains(&b"X, deleted by A".to_vec()), "{} has A's deleted file back", d.name);
    }
}

/// Two devices that hold the same losing version made the same conflict copy, each with its own new
/// version; one deleting its copy (the user resolved the conflict) before hearing of the other's lost
/// to that one. Copies are deterministic now: the loser's version vector and a derived file id.
#[test]
fn a_deleted_conflict_copy_stays_deleted() {
    let key = rw();
    let (mut a, mut b, mut c) = (dev("A", 1, &key, false), dev("B", 2, &key, false), dev("C", 3, &key, false));
    write_all(&a.fs, "f", b"base").unwrap();
    a.core.scan("").unwrap();
    settle(&mut [&mut a, &mut b, &mut c], 1);
    write_all(&a.fs, "f", b"A's edit, loses").unwrap();
    a.fs.set_mtime("f", 1_000 * S).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut c);
    pull(&mut c, &[&a]);
    write_all(&b.fs, "f", b"B's edit, wins").unwrap();
    b.fs.set_mtime("f", 2_000 * S).unwrap();
    b.core.scan("").unwrap();
    send(&b, &mut a);
    let pa = pull(&mut a, &[&b]);
    send(&b, &mut c);
    let pc = pull(&mut c, &[&b]);
    assert_eq!((pa.conflicts.len(), pc.conflicts.len()), (1, 1));
    assert_eq!(pa.conflicts, pc.conflicts, "the same copy on both");
    let copy = pa.conflicts[0].clone();
    a.fs.remove_file(&copy).unwrap();
    a.core.scan("").unwrap();

    settle(&mut [&mut a, &mut b, &mut c], 3);
    for d in [&a, &b, &c] {
        assert!(d.fs.lstat(&copy).unwrap().is_none(), "{} has the deleted copy back", d.name);
    }
}

/// A read-only device's own file inside a directory a writer deletes: the directory could not go (it
/// is not empty) nor come back (the file is never announced), so the delete stayed needed forever.
/// The directory is recorded as a local change now.
#[test]
fn a_read_only_device_with_its_own_file_in_a_deleted_directory_settles() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key, false), dev("R", 3, &ro, false));
    a.fs.create_dir("d").unwrap();
    write_all(&a.fs, "d/x", b"from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    write_all(&r.fs, "d/mine", b"R's own").unwrap();
    r.core.scan("").unwrap();
    a.fs.remove_file("d/x").unwrap();
    a.fs.remove_dir("d").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    assert_eq!(read_all(&r.fs, "d/mine").unwrap(), b"R's own");
    assert_eq!(r.core.need_totals().unwrap().0, 0, "nothing left to do");
}

/// A file made on one device and a directory with files at the same path on another: a file beat a
/// directory in the conflict (directories have no mtime), the directory could not become a file while
/// its files were current, nor its files be written under a file. A directory beats a file now, and
/// the file becomes a conflict copy beside it.
#[test]
fn a_file_and_a_directory_at_one_path_converge() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    write_all(&a.fs, "p", b"a file").unwrap();
    b.fs.create_dir("p").unwrap();
    write_all(&b.fs, "p/x", b"a file in a directory").unwrap();
    settle(&mut [&mut a, &mut b], 4);
    assert_eq!(a.core.need_totals().unwrap().0 + b.core.need_totals().unwrap().0, 0);
    for d in [&a, &b] {
        assert!(contents(&d.fs).contains(&b"a file".to_vec()) && contents(&d.fs).contains(&b"a file in a directory".to_vec()));
    }
}

/// The puller committed a step as soon as its rename returned, without making the rename durable (no
/// directory fsync). A crash could then leave the store ahead of the disk, and the next scan read the
/// missing file as the user deleting it, a delete that went to every device. Now the directories are
/// synced before the records of a batch of steps are committed.
#[test]
fn a_crash_that_loses_a_pulled_file_deletes_nothing_elsewhere() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    write_all(&a.fs, "f", b"A's file").unwrap();
    a.core.scan("").unwrap();
    b.fs.snapshot();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    // Crash: the store kept its commits, the disk lost the rename.
    b.fs.restore();
    b.core.scan("").unwrap();
    send(&b, &mut a);
    pull(&mut a, &[&b]);
    assert_eq!(read_all(&a.fs, "f").ok().as_deref(), Some(&b"A's file"[..]), "a crash on B deleted A's file");
}

/// The other order: the change's holder hears of the concurrent delete before the deleter takes the
/// change, and absorbs it (a new version of the same content that has seen the delete).
#[test]
fn a_delete_that_lost_is_absorbed_by_the_holder_of_the_change() {
    let key = rw();
    let (mut a, mut b, mut c) = (dev("A", 1, &key, false), dev("B", 2, &key, false), dev("C", 3, &key, false));
    write_all(&a.fs, "f", b"X, deleted by A").unwrap();
    a.fs.set_mtime("f", 3_000 * S).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut c);
    pull(&mut c, &[&a]);
    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "f", b"Y, made by B").unwrap();
    b.fs.set_mtime("f", 2_000 * S).unwrap();
    b.core.scan("").unwrap();
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap().0, 1, "B absorbs A's delete");
    pull(&mut b, &[]);
    assert_eq!(b.core.need_totals().unwrap().0, 0);
    let v = b.core.store().local("f").unwrap().unwrap().file.info.version;
    let t = a.core.store().local("f").unwrap().unwrap().file.info.version;
    assert!(v.dominates(&t), "B's version has seen A's delete");

    settle(&mut [&mut a, &mut b, &mut c], 3);
    for d in [&a, &b, &c] {
        assert_eq!(read_all(&d.fs, "f").unwrap(), b"Y, made by B", "{}", d.name);
        assert!(!contents(&d.fs).contains(&b"X, deleted by A".to_vec()), "{} has A's deleted file back", d.name);
    }
}

/// A file replaced a directory on one device (its version has seen the directory) while another
/// device added files inside: neither could finish. A directory with live content beats the file that
/// replaced it: the file comes in as a conflict copy beside it, and the directory comes back.
#[test]
fn a_directory_with_new_files_beats_the_file_that_replaced_it() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    a.fs.create_dir("p").unwrap();
    a.core.scan("").unwrap();
    settle(&mut [&mut a, &mut b], 1);
    a.fs.remove_dir("p").unwrap();
    write_all(&a.fs, "p", b"a file now").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "p/new", b"made inside").unwrap();
    b.core.scan("").unwrap();
    settle(&mut [&mut a, &mut b], 4);
    for d in [&a, &b] {
        assert_eq!(d.core.need_totals().unwrap().0, 0, "{}", d.name);
        assert_eq!(read_all(&d.fs, "p/new").unwrap(), b"made inside", "{}", d.name);
        assert!(contents(&d.fs).contains(&b"a file now".to_vec()), "{} keeps the file", d.name);
    }
}

/// `local_totals`, `conflicts` and `prune_archive`.
#[test]
fn the_folder_reports_totals_conflicts_and_prunes_its_archive() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key, false), dev("B", 2, &key, false));
    write_all(&a.fs, "one", b"12345").unwrap();
    write_all(&a.fs, "two", b"123").unwrap();
    a.fs.create_dir("d").unwrap();
    a.core.scan("").unwrap();
    assert_eq!(a.core.local_totals().unwrap(), (2, 8), "files only");
    settle(&mut [&mut a, &mut b], 1);

    write_all(&a.fs, "one", b"A's edit").unwrap();
    a.fs.set_mtime("one", 1_000 * S).unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "one", b"B's edit, newer").unwrap();
    b.fs.set_mtime("one", 2_000 * S).unwrap();
    b.core.scan("").unwrap();
    settle(&mut [&mut a, &mut b], 2);
    let c = a.core.conflicts().unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1, "one");
    assert_eq!(b.core.conflicts().unwrap(), c, "the copy synced");

    // B archives the version of `two` that A's edit replaces. Pruning by age: young, kept; old, gone.
    write_all(&a.fs, "two", b"A changed two").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(b.fs.read_dir(".seedwell/Archive").unwrap().entries.len(), 1);
    let r = trs_sync::archive::Retention { max_age_s: 30 * 86_400, max_percent: 0 };
    assert_eq!(b.core.prune_archive(Utc.now_ns(), r).unwrap(), 0);
    assert_eq!(b.core.prune_archive(Utc.now_ns() + 31 * 86_400 * S, r).unwrap(), 1);
    assert!(b.fs.read_dir(".seedwell/Archive").unwrap().entries.is_empty());
}
