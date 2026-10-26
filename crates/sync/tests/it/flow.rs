//! Two or three devices on in-memory filesystems, exchanging indexes by hand: the basic flows end to end
//! (the randomized simulator lives in `sim.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use trs_sync::clock::Clock;
use trs_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
use trs_sync::fs::{Fs, MemFs, read_all, write_all};
use trs_sync::ignore::Ignore;
use trs_sync::index::MemStore;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::ShortId;
use trs_sync::folder::RemoteError;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::keys::{Access, FolderSecret};
use trs_sync_proto::sign::Writer;

/// What a device holds of the test folder: its public key and its access. In the driver both come from the
/// folder's membership; here a test hands them out.
#[derive(Clone, Debug)]
pub(crate) struct Key {
    pk: [u8; 32],
    access: Access,
}

impl Key {
    /// Read & write access to the folder made from `secret`.
    pub(crate) fn rw(secret: [u8; 32]) -> Key {
        Key { pk: *FolderSecret::from_bytes(secret).public_key(), access: Access::Write }
    }

    /// The same folder, read only.
    pub(crate) fn read_only(&self) -> Option<Key> {
        Some(Key { access: Access::Read, ..self.clone() })
    }

    pub(crate) fn public_key(&self) -> &[u8; 32] {
        &self.pk
    }

    pub(crate) fn access(&self) -> Access {
        self.access
    }

    #[allow(dead_code)]
    pub(crate) fn folder_id(&self) -> FolderId {
        FolderId::of_public_key(&self.pk)
    }
}

/// Takes in the writers another device knows, as the driver does when membership arrives.
pub(crate) fn learn_writers(core: &mut FolderCore, writers: &[Writer]) -> Result<bool, RemoteError> {
    let mut keys: Vec<[u8; 32]> = core.writers().iter().map(|w| w.device_pk).collect();
    keys.extend(writers.iter().map(|w| w.device_pk));
    keys.sort_unstable();
    keys.dedup();
    let access = core.access();
    core.set_membership(&keys, access)
}

struct Utc;
impl Clock for Utc {
    fn now_ns(&self) -> i64 {
        1_792_938_725_000_000_000
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

/// Serves chunks straight from other devices' disks (what a peer would answer to a Request).
struct Disks<'a>(Vec<&'a MemFs>);

impl ChunkSource for Disks<'_> {
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        reqs.iter()
            .map(|r| {
                for fs in &self.0 {
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

fn dev(name: &'static str, seed: u8, key: &Key) -> Dev {
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive: false,
        byte_exact_names: false,
        archive: true,
    };
    let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
    let new_id = Arc::new(move || {
        let n = ids.fetch_add(1, Ordering::SeqCst);
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.to_be_bytes());
        id
    });
    let core = FolderCore::new(
        cfg,
        Box::new(MemStore::new(u64::from(seed))),
        Arc::new(fs.clone()),
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

/// `from`'s whole index (and writers) to `to`.
fn send(from: &Dev, to: &mut Dev) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let entries = from.core.announce_since(0, usize::MAX).unwrap();
    to.core.apply_remote(short(from), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
}

fn pull(to: &mut Dev, from: &[&Dev]) -> trs_sync::pull::PullReport {
    let source = Disks(from.iter().map(|d| &d.fs).collect());
    let names: Vec<(ShortId, &str)> = from.iter().map(|d| (short(d), d.name)).collect();
    let name = move |id: ShortId| names.iter().find(|(s, _)| *s == id).map_or("?", |(_, n)| n).to_string();
    let mut total = trs_sync::pull::PullReport::default();
    loop {
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
}

fn rw() -> Key {
    Key::rw([7; 32])
}

#[test]
fn a_new_file_reaches_the_other_device() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    a.fs.create_dir("docs").unwrap();
    write_all(&a.fs, "docs/hello.txt", b"hello world").unwrap();
    let r = a.core.scan("").unwrap();
    assert_eq!(r.changed, 2, "the directory and the file");

    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap().0, 2);
    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty(), "{:?}", p.failed);
    assert_eq!(read_all(&b.fs, "docs/hello.txt").unwrap(), b"hello world");
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));

    // B scanning finds nothing new: the pulled file is recorded with its stamp.
    let r = b.core.scan("").unwrap();
    assert_eq!((r.changed, r.deleted, r.hashed_bytes), (0, 0, 0));
}

#[test]
fn edits_and_deletes_flow_both_ways_and_the_archive_keeps_the_old_version() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "f", b"one").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    write_all(&b.fs, "f", b"two, longer").unwrap();
    b.core.scan("").unwrap();
    send(&b, &mut a);
    let p = pull(&mut a, &[&b]);
    assert!(p.conflicts.is_empty() && p.failed.is_empty(), "{p:?}");
    assert_eq!(read_all(&a.fs, "f").unwrap(), b"two, longer");
    let archived = a.fs.read_dir(".seedwell/Archive").unwrap();
    assert_eq!(archived.entries.len(), 1, "the replaced version is archived");

    a.fs.remove_file("f").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert!(b.fs.lstat("f").unwrap().is_none(), "the delete reached B");
    assert_eq!(b.fs.read_dir(".seedwell/Archive").unwrap().entries.len(), 1, "and B archived its copy");
}

#[test]
fn concurrent_edits_keep_both_versions() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "doc.txt", b"base").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    write_all(&a.fs, "doc.txt", b"edit on A").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "doc.txt", b"edit on B, later").unwrap();
    b.core.scan("").unwrap();

    send(&a, &mut b);
    send(&b, &mut a);
    let pa = pull(&mut a, &[&b]);
    let pb = pull(&mut b, &[&a]);
    // Both computed the same winner; exactly one side made a conflict copy, and then it syncs.
    assert_eq!(pa.conflicts.len() + pb.conflicts.len(), 1, "{pa:?} {pb:?}");
    send(&a, &mut b);
    send(&b, &mut a);
    pull(&mut a, &[&b]);
    pull(&mut b, &[&a]);

    let la = a.fs.read_dir("").unwrap().entries.into_iter().map(|e| e.name).collect::<std::collections::BTreeSet<_>>();
    let lb = b.fs.read_dir("").unwrap().entries.into_iter().map(|e| e.name).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(la, lb, "both devices hold the same files");
    let copy = la.iter().find(|n| n.contains("(conflict, ")).expect("a conflict copy");
    let mut contents = vec![read_all(&a.fs, "doc.txt").unwrap(), read_all(&a.fs, copy).unwrap()];
    contents.sort();
    assert_eq!(contents, vec![b"edit on A".to_vec(), b"edit on B, later".to_vec()], "no edit was lost");
    assert_eq!(read_all(&a.fs, "doc.txt").unwrap(), read_all(&b.fs, "doc.txt").unwrap());
}

#[test]
fn a_change_beats_a_concurrent_delete() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "keep", b"v1").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    a.fs.remove_file("keep").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "keep", b"v2 from B").unwrap();
    b.core.scan("").unwrap();
    send(&a, &mut b);
    send(&b, &mut a);
    pull(&mut a, &[&b]);
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&a.fs, "keep").unwrap(), b"v2 from B", "the file came back on A");
    assert_eq!(read_all(&b.fs, "keep").unwrap(), b"v2 from B");
}

#[test]
fn a_rename_moves_the_file_without_fetching() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "old.bin", &vec![42u8; 300_000]).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    a.fs.rename("old.bin", "new.bin").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    // No source at all: the content must come from B's own disk.
    let p = pull(&mut b, &[]);
    assert!(p.failed.is_empty() && p.waiting == 0, "{p:?}");
    assert!(b.fs.lstat("old.bin").unwrap().is_none());
    assert_eq!(read_all(&b.fs, "new.bin").unwrap().len(), 300_000);
}

#[test]
fn a_local_edit_the_scanner_has_not_seen_is_never_overwritten() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "f", b"v1").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    write_all(&a.fs, "f", b"v2 from A").unwrap();
    a.core.scan("").unwrap();
    // B saves the file and the watcher has not fired yet.
    write_all(&b.fs, "f", b"unsaved work on B").unwrap();
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.rescan, vec!["f".to_string()], "handed back for a scan");
    assert_eq!(read_all(&b.fs, "f").unwrap(), b"unsaved work on B");

    // After the scan it is a conflict, and both versions survive.
    b.core.scan("").unwrap();
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.conflicts.len() + usize::from(read_all(&b.fs, "f").unwrap() == b"unsaved work on B"), 1);
}

#[test]
fn a_missing_marker_stops_the_scan_and_deletes_nothing() {
    let key = rw();
    let mut a = dev("A", 1, &key);
    write_all(&a.fs, "f", b"x").unwrap();
    a.core.scan("").unwrap();
    a.fs.remove_file("f").unwrap();
    a.fs.remove_dir(".seedwell").unwrap();
    assert!(matches!(a.core.scan(""), Err(trs_sync::scan::ScanError::FolderMissing)));
    assert!(!a.core.store().local("f").unwrap().unwrap().file.info.deleted);
}

#[test]
fn a_read_only_device_receives_but_never_sends_its_changes() {
    let key = rw();
    let ro = key.read_only().unwrap();
    let (mut a, mut r) = (dev("A", 1, &key), dev("R", 3, &ro));
    assert_eq!(r.core.access(), Access::Read);
    write_all(&a.fs, "f", b"from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    assert_eq!(read_all(&r.fs, "f").unwrap(), b"from A");

    write_all(&r.fs, "f", b"local change on R").unwrap();
    write_all(&r.fs, "new", b"local file on R").unwrap();
    r.core.scan("").unwrap();
    let out = r.core.announce_since(0, usize::MAX).unwrap();
    assert!(out.iter().all(|e| e.file.info.path != "new"), "a local-only file is never announced");
    let f = out.iter().find(|e| e.file.info.path == "f").unwrap();
    assert!(f.invalid, "a changed copy is announced as not servable");
    // A takes nothing from R.
    send(&r, &mut a);
    assert_eq!(a.core.need_totals().unwrap().0, 0);
    assert_eq!(read_all(&a.fs, "f").unwrap(), b"from A");
}

/// A device that joins into a folder with files and scans them before it may write (a member is admitted, then told
/// it may write): once it may, the files only it had become its own and are sent; one with the same content as the
/// folder's merges; one that differs is kept on both sides as a conflict copy.
#[test]
fn files_scanned_before_a_device_may_write_are_sent_once_it_may() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key.read_only().unwrap()));
    write_all(&a.fs, "plan.txt", b"the folder's").unwrap();
    write_all(&a.fs, "same.txt", b"same").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "plan.txt", b"left here").unwrap();
    write_all(&b.fs, "same.txt", b"same").unwrap();
    write_all(&b.fs, "only-here.txt", b"mine").unwrap();
    b.core.scan("").unwrap();
    assert!(b.core.announce_since(0, usize::MAX).unwrap().iter().all(|e| e.file.info.path != "only-here.txt"), "not while it may only read");

    // Admitted as read & write: B's key joins the writers and it may write.
    let mut writers: Vec<[u8; 32]> = a.core.writers().iter().map(|w| w.device_pk).collect();
    writers.push(*DeviceKey::from_seed(&[2; 32]).public_key());
    writers.sort_unstable();
    b.core.set_membership(&writers, Access::Write).unwrap();
    a.core.set_membership(&writers, Access::Write).unwrap();
    let out = b.core.announce_since(0, usize::MAX).unwrap();
    assert!(out.iter().any(|e| e.file.info.path == "only-here.txt" && !e.invalid), "now its own: {:?}", out.iter().map(|e| &e.file.info.path).collect::<Vec<_>>());

    for _ in 0..3 {
        send(&a, &mut b);
        pull(&mut b, &[&a]);
        b.core.scan("").unwrap();
        send(&b, &mut a);
        pull(&mut a, &[&b]);
        a.core.scan("").unwrap();
    }
    assert_eq!(read_all(&a.fs, "only-here.txt").unwrap(), b"mine", "the file only B had reached A");
    let names = |fs: &MemFs| {
        let mut v: Vec<String> = fs.read_dir("").unwrap().entries.into_iter().map(|e| e.name).filter(|n| !n.starts_with('.')).collect();
        v.sort();
        v
    };
    assert_eq!(names(&a.fs), names(&b.fs));
    assert!(!names(&a.fs).iter().any(|n| n.starts_with("same (conflict")), "the same content merges: {:?}", names(&a.fs));
    assert_eq!(names(&a.fs).iter().filter(|n| n.starts_with("plan (conflict")).count(), 1, "one copy of the difference: {:?}", names(&a.fs));
}
