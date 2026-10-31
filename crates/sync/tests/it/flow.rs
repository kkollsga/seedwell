//! Two or three devices on in-memory filesystems, exchanging indexes by hand: the basic flows end to end
//! (the randomized simulator lives in `sim.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use trs_sync::clock::Clock;
use trs_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
use trs_sync::fs::{Fs, MemFs, MemFsOptions, read_all, write_all};
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
    dev_on(MemFs::new(), name, seed, key)
}

fn dev_on(fs: MemFs, name: &'static str, seed: u8, key: &Key) -> Dev {
    fs.create_dir(".seedwell").unwrap();
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive: trs_sync::path::probe_case_insensitive(&fs).unwrap(),
        byte_exact_names: false,
        exec_bits: trs_sync::path::probe_exec_bits(&fs).unwrap(),
        archive: true,
        on_demand: false,
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
fn on_a_disk_without_hard_links_a_replaced_version_is_archived_as_a_copy() {
    // exFAT and FAT drives: replacing a file used to fail at every try.
    let key = rw();
    let exfat = MemFs::with_options(MemFsOptions { hard_links: false, ..MemFsOptions::default() });
    let (mut a, mut b) = (dev_on(exfat, "A", 1, &key), dev("B", 2, &key));
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
    let old = format!(".seedwell/Archive/{}", archived.entries[0].name);
    assert_eq!(read_all(&a.fs, &old).unwrap(), b"one", "a copy of the old version, not a link to the new one");
}

#[test]
fn a_disk_without_permissions_keeps_each_files_exec_bit_as_synced() {
    // exFAT on a Mac reads every file executable, NTFS none: neither may re-version files or flip the bit for
    // the other devices.
    for fixed in [true, false] {
        let key = rw();
        let odd = MemFs::with_options(MemFsOptions { fixed_exec: Some(fixed), ..MemFsOptions::default() });
        let (mut a, mut b) = (dev("A", 1, &key), dev_on(odd, "B", 2, &key));
        write_all(&a.fs, "run.sh", b"#!/bin/sh").unwrap();
        a.fs.set_exec("run.sh", true).unwrap();
        write_all(&a.fs, "notes.txt", b"plain").unwrap();
        a.core.scan("").unwrap();
        send(&a, &mut b);
        let p = pull(&mut b, &[&a]);
        assert!(p.failed.is_empty(), "{p:?}");
        let r = b.core.scan("").unwrap();
        assert_eq!((r.changed, r.hashed_bytes), (0, 0), "nothing re-versioned (fixed exec {fixed})");

        // An edit on B keeps the bit each file had.
        write_all(&b.fs, "run.sh", b"#!/bin/sh\necho hi").unwrap();
        write_all(&b.fs, "notes.txt", b"plain, edited").unwrap();
        b.core.scan("").unwrap();
        send(&b, &mut a);
        let p = pull(&mut a, &[&b]);
        assert!(p.conflicts.is_empty() && p.failed.is_empty(), "{p:?}");
        assert_eq!(read_all(&a.fs, "notes.txt").unwrap(), b"plain, edited");
        assert!(a.fs.lstat("run.sh").unwrap().unwrap().exec, "still executable on A (fixed exec {fixed})");
        assert!(!a.fs.lstat("notes.txt").unwrap().unwrap().exec, "still not executable on A (fixed exec {fixed})");
    }
}

#[test]
fn a_name_the_disk_folds_onto_another_is_refused_not_written_over_it() {
    // A case-sensitive device has `strasse.txt` and `straße.txt`; a disk that folds `ß` to `ss` holds one of them.
    let key = rw();
    let folding = MemFs::with_options(MemFsOptions { case_insensitive: true, full_case_folding: true, ..MemFsOptions::default() });
    let mut a = dev("A", 1, &key);
    let mut b = dev_on(folding, "B", 2, &key);
    write_all(&a.fs, "strasse.txt", b"first").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    assert!(pull(&mut b, &[&a]).failed.is_empty());
    write_all(&a.fs, "stra\u{df}e.txt", b"second").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "strasse.txt").unwrap(), b"first", "not written over");
    assert_eq!(p.failed.len(), 1, "{p:?}");
    assert!(format!("{:?}", p.failed[0]).contains("strasse.txt"), "{p:?}");
}

#[test]
fn an_entry_with_a_path_this_version_refuses_is_skipped_not_the_batch() {
    // A later version may send names this one does not accept: the rest of the batch still arrives.
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    write_all(&a.fs, "good.txt", b"fine").unwrap();
    write_all(&a.fs, "odd.txt", b"odd").unwrap();
    a.core.scan("").unwrap();
    let mut entries = a.core.announce_since(0, usize::MAX).unwrap();
    let odd = entries.iter_mut().find(|e| e.file.info.path == "odd.txt").unwrap();
    let mut info = odd.file.info.clone();
    info.path = "../outside.txt".into();
    // Signed by a real writer: only the path is wrong.
    odd.file = trs_sync_proto::sign::sign_file(&DeviceKey::from_seed(&[1; 32]), &a.core.folder_id(), info);
    learn_writers(&mut b.core, &a.core.writers()).unwrap();
    let (index_id, _) = a.core.index_state().unwrap();
    let stored = b.core.apply_remote(short(&a), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
    assert_eq!(stored, 1, "the good entry is stored");
    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty(), "{p:?}");
    assert_eq!(read_all(&b.fs, "good.txt").unwrap(), b"fine");
    assert!(b.fs.lstat("../outside.txt").is_err() || b.fs.lstat("../outside.txt").unwrap().is_none());

    // A forged signature still refuses the whole batch.
    let mut entries = a.core.announce_since(0, usize::MAX).unwrap();
    entries[0].file.info.size += 1;
    let (index_id, _) = a.core.index_state().unwrap();
    assert!(b.core.apply_remote(short(&a), RemoteIndex { index_id, full: true, done: true, entries }).is_err());
}

#[test]
fn a_file_deleted_in_the_pass_through_folder_is_not_archived() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    let p = format!("{}/kglite/2026-10-26 08.05 from api/graph.bin", trs_sync::PASS_THROUGH);
    let mut dir = String::new();
    for part in ["Seedwell Pass-Through", "kglite", "2026-10-26 08.05 from api"] {
        dir = if dir.is_empty() { part.to_string() } else { format!("{dir}/{part}") };
        a.fs.create_dir(&dir).unwrap();
    }
    write_all(&a.fs, &p, b"a big file in transit").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, &p).unwrap(), b"a big file in transit");
    // B received it and deletes the copy: gone on A too, and no archive keeps it (the original stays with its sender).
    b.fs.remove_file(&p).unwrap();
    b.core.scan("").unwrap();
    send(&b, &mut a);
    pull(&mut a, &[&b]);
    assert!(a.fs.lstat(&p).unwrap().is_none(), "the delete reached A");
    assert!(a.fs.read_dir(".seedwell/Archive").map_or(true, |l| l.entries.is_empty()), "nothing archived");
    assert!(trs_sync::in_pass_through(trs_sync::PASS_THROUGH) && !trs_sync::in_pass_through("Seedwell Pass-Through 2/x"));
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

/// A device that joins with write access receives the folder before it is admitted (the owner's grant comes later):
/// an edit it makes meanwhile is its user's change as a writer. It is not kept as a read-only device's conflict copy
/// when another device's older edit arrives; once admitted it is this device's own version, which wins by its newer
/// mtime, and the other edit becomes the one conflict copy, named after the device that made it.
#[test]
fn an_edit_made_while_waiting_to_be_admitted_with_write_access_becomes_this_devices_version() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key.read_only().unwrap()));
    b.core.set_awaiting_write(true);
    write_all(&a.fs, "plan.txt", b"v1").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "plan.txt").unwrap(), b"v1");

    write_all(&a.fs, "plan.txt", b"from A").unwrap();
    a.core.scan("").unwrap();
    write_all(&b.fs, "plan.txt", b"from B, later").unwrap();
    b.core.scan("").unwrap();
    assert!(!b.core.signs(), "not admitted yet: B's edit is a local difference for now");
    send(&a, &mut b);
    let r = pull(&mut b, &[&a]);
    assert!(r.conflicts.is_empty() && r.waiting == 1, "it waits instead of keeping B's edit as a copy: {r:?}");
    assert_eq!(read_all(&b.fs, "plan.txt").unwrap(), b"from B, later");

    // Admitted with write access: B's key joins the writers; the driver clears the flag after applying it.
    let mut writers: Vec<[u8; 32]> = a.core.writers().iter().map(|w| w.device_pk).collect();
    writers.push(*DeviceKey::from_seed(&[2; 32]).public_key());
    writers.sort_unstable();
    b.core.set_membership(&writers, Access::Write).unwrap();
    b.core.set_awaiting_write(false);
    a.core.set_membership(&writers, Access::Write).unwrap();
    assert!(b.core.signs());
    b.core.scan("").unwrap();
    let mine = b.core.announce_since(0, usize::MAX).unwrap().into_iter().find(|e| e.file.info.path == "plan.txt").unwrap();
    assert!(!mine.invalid && mine.file.info.modified_by == short(&b) && mine.file.info.size == 13, "B's own version now: {mine:?}");

    for _ in 0..3 {
        send(&a, &mut b);
        send(&b, &mut a);
        pull(&mut a, &[&b]);
        pull(&mut b, &[&a]);
        a.core.scan("").unwrap();
        b.core.scan("").unwrap();
    }
    for d in [&a, &b] {
        assert_eq!(read_all(&d.fs, "plan.txt").unwrap(), b"from B, later", "the newer edit wins on {}", d.name);
        let copies: Vec<String> = d.fs.read_dir("").unwrap().entries.into_iter().map(|e| e.name).filter(|n| n.contains("(conflict, ")).collect();
        assert_eq!(copies.len(), 1, "one copy on {}: {copies:?}", d.name);
        assert!(!copies[0].contains("(conflict, B,"), "the copy is A's edit, not B's: {copies:?}");
        assert_eq!(read_all(&d.fs, &copies[0]).unwrap(), b"from A");
    }
}

/// A read-only device's own change that loses to an arriving version is kept as a conflict copy named after this
/// device (whose user made it), not after the device that wrote the version it was made over.
#[test]
fn a_read_only_devices_own_change_is_kept_under_its_own_name() {
    let key = rw();
    let (mut a, mut r) = (dev("A", 1, &key), dev("R", 3, &key.read_only().unwrap()));
    write_all(&a.fs, "f", b"v1").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    write_all(&r.fs, "f", b"changed on R").unwrap();
    r.core.scan("").unwrap();
    write_all(&a.fs, "f", b"v2 from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut r);
    let (ra, rr) = (short(&a), short(&r));
    let name = move |id: ShortId| if id == ra { "A".to_string() } else if id == rr { "R".to_string() } else { "?".to_string() };
    let report = r.core.pull(&Disks(vec![&a.fs]), &name, 64 << 20).unwrap();
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert!(report.conflicts[0].starts_with("f (conflict, R, "), "{:?}", report.conflicts);
    assert_eq!(read_all(&r.fs, &report.conflicts[0]).unwrap(), b"changed on R");
    assert_eq!(read_all(&r.fs, "f").unwrap(), b"v2 from A");
}

/// A FolderCore over a real folder on this system's disk.
fn core_on_disk(fs: trs_sync::fs::StdFs, seed: u8, key: &Key) -> FolderCore {
    fs.create_dir(".seedwell").unwrap();
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive: trs_sync::path::probe_case_insensitive(&fs).unwrap(),
        byte_exact_names: false,
        exec_bits: trs_sync::path::probe_exec_bits(&fs).unwrap(),
        archive: true,
        on_demand: false,
    };
    let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
    let new_id = Arc::new(move || {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&ids.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        id
    });
    FolderCore::new(cfg, Box::new(MemStore::new(u64::from(seed))), Arc::new(fs), Some(dk), Arc::new(Utc), new_id, Ignore::defaults()).unwrap()
}

/// Names another system allows but this disk cannot hold (on Windows `?` and `:`, a reserved device
/// name, and a folder with a trailing dot, which Windows would make under another name) are issues on
/// their paths and are not tried again; the good file arrives and the folder settles. Where the disk
/// can hold them, they arrive like any other.
#[test]
fn names_the_disk_cannot_hold_are_issues_and_the_rest_arrives() {
    let key = rw();
    let mut a = dev("A", 1, &key);
    a.fs.create_dir("dir.").unwrap();
    let files = [("dir./x.txt", "in a folder with a trailing dot"), ("why?.txt", "question"), ("a:b.txt", "colon"), ("con.txt", "device"), ("good.txt", "good")];
    for (p, body) in files {
        write_all(&a.fs, p, body.as_bytes()).unwrap();
    }
    a.core.scan("").unwrap();

    let t = tempfile::tempdir().unwrap();
    let disk = trs_sync::fs::StdFs::new(t.path());
    let mut b = core_on_disk(disk.clone(), 2, &key);
    learn_writers(&mut b, &a.core.writers()).unwrap();
    let (index_id, _) = a.core.index_state().unwrap();
    let entries = a.core.announce_since(0, usize::MAX).unwrap();
    b.apply_remote(short(&a), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();

    let source = Disks(vec![&a.fs]);
    let name = |_: ShortId| "A".to_string();
    let first = b.pull(&source, &name, 64 << 20).unwrap();
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    assert_eq!(read_all(&disk, "good.txt").unwrap(), b"good");
    let bad = ["a:b.txt", "con.txt", "dir.", "dir./x.txt", "why?.txt"];
    if disk.name_problem("why?.txt").is_some() {
        let mut got: Vec<&str> = first.unsupported.iter().map(|u| u.path.as_str()).collect();
        got.sort_unstable();
        assert_eq!(got, bad);
        assert!(first.unsupported.iter().all(|u| u.why == trs_platform::fs::NAME_NOT_ALLOWED), "{:?}", first.unsupported);
        assert_eq!(b.need_totals().unwrap(), (0, 0), "the folder settles: nothing left to wait for");
        let mut on_disk: Vec<String> = std::fs::read_dir(t.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        on_disk.sort_unstable();
        assert_eq!(on_disk, [".seedwell", "good.txt"], "nothing made under another name (no \"dir\")");
        // Not tried again while their versions stay the same: reported, nothing done.
        let again = b.pull(&source, &name, 64 << 20).unwrap();
        assert_eq!((again.done, again.failed.len(), again.unsupported.len()), (0, 0, bad.len()));
    } else {
        assert!(first.unsupported.is_empty(), "{:?}", first.unsupported);
        for (p, body) in files {
            assert_eq!(read_all(&disk, p).unwrap(), body.as_bytes(), "{p}");
        }
        assert_eq!(b.need_totals().unwrap(), (0, 0));
    }
    // The next scan finds nothing new either way.
    let r = b.scan("").unwrap();
    assert_eq!((r.changed, r.deleted), (0, 0));
}

/// A pull's temp file is kept while its path still needs pulling (the next pull resumes from it) and
/// removed by the next scan once the path needs nothing (deleted on the other side meanwhile), a
/// long name's hashed temp too; the one still wanted is finished by the pull.
#[test]
fn a_temp_file_nobody_will_finish_is_removed_by_the_next_scan() {
    let key = rw();
    let (mut a, mut b) = (dev("A", 1, &key), dev("B", 2, &key));
    let long = format!("{}.txt", "l".repeat(250));
    let paths = ["gone.txt", long.as_str(), "kept.txt"];
    for p in paths {
        write_all(&a.fs, p, b"data").unwrap();
    }
    a.core.scan("").unwrap();
    send(&a, &mut b);
    // B's pull stopped half way: a partial temp of each (as a crash or a quit leaves them).
    let temps: Vec<String> = paths.iter().map(|p| trs_sync::pull::temp_path(p)).collect();
    assert!(temps[1].len() < 60, "the long name has a hashed temp name: {}", temps[1]);
    for t in &temps {
        write_all(&b.fs, t, b"da").unwrap();
    }
    b.core.scan("").unwrap();
    assert!(temps.iter().all(|t| b.fs.lstat(t).unwrap().is_some()), "all still wanted: kept for resuming");

    // A deletes two of them before B fetched them.
    a.fs.remove_file("gone.txt").unwrap();
    a.fs.remove_file(&long).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    b.core.scan("").unwrap();
    assert!(b.fs.lstat(&temps[0]).unwrap().is_none(), "gone.txt's temp is removed");
    assert!(b.fs.lstat(&temps[1]).unwrap().is_none(), "the long name's temp is removed");
    assert!(b.fs.lstat(&temps[2]).unwrap().is_some(), "kept.txt's temp is still wanted");

    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty(), "{:?}", p.failed);
    assert_eq!(read_all(&b.fs, "kept.txt").unwrap(), b"data");
    assert!(b.fs.lstat(&temps[2]).unwrap().is_none());

    // A file named like the temps' frame alone holds no path: the scan does not trip over it.
    write_all(&b.fs, ".seedwell.tmp", b"x").unwrap();
    b.core.scan("").unwrap();
}
