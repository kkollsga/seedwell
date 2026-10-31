//! On-demand folders (SPEC §7G.11), end to end on in-memory devices: what an on-demand member holds, fetches, keeps
//! and lets go of, the host's changes, item identifiers and the change feed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use trs_sync::clock::Clock;
use trs_sync::folder::{FolderConfig, FolderCore, HostError, RemoteIndex, Requested};
use trs_sync::fs::{Fs, MemFs, MemFsOptions, read_all, write_all};
use trs_sync::ignore::Ignore;
use trs_sync::index::MemStore;
use trs_sync::ondemand::{Deleted, Evicted, ItemState, ROOT_ID};
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError, PullReport};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{FileInfo, FileKind};
use trs_sync_proto::msg::HeldFile;

use crate::flow::{Key, learn_writers};

struct Utc(AtomicU64);
impl Clock for Utc {
    fn now_ns(&self) -> i64 {
        // Every call a second later, so versions made one after the other have distinct times.
        (1_792_938_725 + self.0.fetch_add(1, Ordering::SeqCst) as i64) * 1_000_000_000
    }
}

struct Dev {
    fs: MemFs,
    core: FolderCore,
    name: &'static str,
}

/// Serves chunks from other devices' disks: a device without a file there has nothing to give.
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

fn dev(name: &'static str, seed: u8, key: &Key, on_demand: bool) -> Dev {
    dev_on(name, seed, key, on_demand, false)
}

/// A device on a disk that folds case (as APFS's default) with `case_insensitive`.
fn dev_on(name: &'static str, seed: u8, key: &Key, on_demand: bool, case_insensitive: bool) -> Dev {
    let fs = MemFs::with_options(MemFsOptions { case_insensitive, ..MemFsOptions::default() });
    fs.create_dir(".seedwell").unwrap();
    let core = core_on(&fs, seed, key, on_demand, case_insensitive, Box::new(MemStore::new(u64::from(seed))));
    Dev { fs, core, name }
}

/// A device's core on `fs` and `store` (opened again on the same ones, as after a restart).
fn core_on(fs: &MemFs, seed: u8, key: &Key, on_demand: bool, case_insensitive: bool, store: Box<dyn trs_sync::index::IndexStore>) -> FolderCore {
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive,
        byte_exact_names: false,
        exec_bits: true,
        archive: true,
        on_demand,
    };
    let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
    let new_id = Arc::new(move || {
        let n = ids.fetch_add(1, Ordering::SeqCst);
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.to_be_bytes());
        id
    });
    FolderCore::new(cfg, store, Arc::new(fs.clone()), Some(dk), Arc::new(Utc(AtomicU64::new(0))), new_id, Ignore::defaults()).unwrap()
}

/// What `others`, asked now (as the service asks the devices that announce them held), confirm they hold of what `d`
/// could free at `path` (`unpinning`: a read-only member's delete).
fn asked(d: &mut Dev, path: &str, unpinning: bool, others: &[&Dev]) -> Vec<HeldFile> {
    let (candidates, _) = d.core.free_candidates(path, unpinning).unwrap();
    candidates.into_iter().filter(|f| others.iter().any(|o| o.core.holds(d.core.me(), std::slice::from_ref(f), &|_| false)[0])).collect()
}

/// `d` lets go of `path`, asking `others` first.
fn evict(d: &mut Dev, path: &str, others: &[&Dev]) -> Evicted {
    let confirmed = asked(d, path, false, others);
    d.core.evict(path, &confirmed).unwrap()
}

/// The host deleted `path` on `d` (a read-only member asks `others` first, as for an evict).
fn host_delete(d: &mut Dev, path: &str, others: &[&Dev]) -> Deleted {
    let confirmed = if d.core.frees_on_delete() { asked(d, path, true, others) } else { Vec::new() };
    d.core.host_delete(path, &confirmed).unwrap()
}

fn rw() -> Key {
    Key::rw([9; 32])
}

/// `from`'s whole index to `to`.
fn send(from: &Dev, to: &mut Dev) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let entries = from.core.announce_since(0, usize::MAX).unwrap();
    to.core.apply_remote(from.core.me(), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
}

/// Pull rounds until nothing moves, fetching from `from`'s disks.
fn pull(to: &mut Dev, from: &[&Dev]) -> PullReport {
    let source = Disks(from.iter().map(|d| &d.fs).collect());
    let names: Vec<(ShortId, &str)> = from.iter().map(|d| (d.core.me(), d.name)).collect();
    let name = move |id: ShortId| names.iter().find(|(s, _)| *s == id).map_or("?", |(_, n)| n).to_string();
    let mut total = PullReport::default();
    loop {
        let r = to.core.pull(&source, &name, 64 << 20).unwrap();
        total.done += r.done;
        total.fetched_bytes += r.fetched_bytes;
        total.conflicts.extend(r.conflicts.clone());
        total.failed.extend(r.failed.clone());
        if r.done == 0 {
            total.waiting = r.waiting;
            return total;
        }
    }
}

/// The host renamed `from` to `to` on `d`, which does not hold every file there: `holder` (a device that does, as the
/// service picks one) makes the rename for it, and its change reaches `d`. Returns how many records `holder` renamed.
fn rename_by(d: &mut Dev, from: &str, to: &str, holder: &mut Dev) -> u64 {
    assert!(matches!(d.core.host_rename(from, to), Err(HostError::NeedsHolder)), "{} holds every file at {from}", d.name);
    assert!(d.core.rename_holders(from).unwrap().contains(&holder.core.me()), "{} announces {from} held", holder.name);
    let seen = d.core.rename_seen(from).unwrap();
    let n = holder.core.rename_for_peer(from, to, &seen).unwrap();
    send(holder, d);
    let p = pull(d, &[&*holder]);
    assert!(p.failed.is_empty(), "{p:?}");
    d.core.move_pins(from, to);
    n
}

/// Both ways until neither changes.
fn sync(a: &mut Dev, b: &mut Dev) {
    for _ in 0..3 {
        send(a, b);
        pull(b, &[a]);
        send(b, a);
        pull(a, &[b]);
    }
}

/// A full device with a small tree.
fn full_with_files() -> Dev {
    let a = dev("A", 1, &rw(), false);
    a.fs.create_dir("docs").unwrap();
    a.fs.create_dir("docs/old").unwrap();
    a.fs.create_dir("media").unwrap();
    write_all(&a.fs, "docs/a.txt", b"alpha alpha alpha").unwrap();
    write_all(&a.fs, "docs/b.txt", b"bravo").unwrap();
    write_all(&a.fs, "docs/old/c.txt", b"charlie, older").unwrap();
    write_all(&a.fs, "media/big.bin", &vec![42u8; 300_000]).unwrap();
    write_all(&a.fs, "top.txt", b"at the top").unwrap();
    a
}

fn names(items: &[trs_sync::ondemand::Item]) -> Vec<&str> {
    items.iter().map(|i| i.name.as_str()).collect()
}

fn item(d: &mut Dev, path: &str) -> trs_sync::ondemand::Item {
    d.core.item_at(path).unwrap().unwrap_or_else(|| panic!("no item at {path}"))
}

#[test]
fn an_on_demand_member_lists_everything_without_contents() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert!(p.failed.is_empty(), "{p:?}");
    assert_eq!(p.fetched_bytes, 0, "no content fetched");
    assert_eq!(b.core.need_totals().unwrap(), (0, 0), "nothing left to pull: in sync, online only");
    // The tree is there (directories), the files are not.
    assert!(b.fs.lstat("docs/old").unwrap().is_some_and(|m| m.kind == FileKind::Dir));
    assert!(b.fs.lstat("docs/a.txt").unwrap().is_none());
    assert_eq!(b.core.local_totals().unwrap(), (0, 0), "no content held");

    let root = b.core.children(ROOT_ID, None, 100).unwrap().unwrap();
    assert_eq!(names(&root.items), ["docs", "media", "top.txt"], "children only, in order");
    assert_eq!(root.next, None);
    let docs = root.items.iter().find(|i| i.name == "docs").unwrap().clone();
    assert_eq!((docs.kind, docs.parent_id.as_str()), (FileKind::Dir, ROOT_ID));
    let kids = b.core.children(&docs.id, None, 100).unwrap().unwrap();
    assert_eq!(names(&kids.items), ["a.txt", "b.txt", "old"]);
    let a_txt = &kids.items[0];
    assert_eq!((a_txt.size, a_txt.state, a_txt.parent_id.as_str()), (17, ItemState::OnlineOnly, docs.id.as_str()));
    // Paging: one at a time, then the rest.
    let first = b.core.children(&docs.id, None, 1).unwrap().unwrap();
    assert_eq!(names(&first.items), ["a.txt"]);
    let rest = b.core.children(&docs.id, first.next.as_deref(), 10).unwrap().unwrap();
    assert_eq!(names(&rest.items), ["b.txt", "old"]);
    // An item by its identifier; an identifier of nothing.
    assert_eq!(b.core.item(&a_txt.id).unwrap().unwrap().path, "docs/a.txt");
    assert!(b.core.item("0123456789abcdef0123456789abcdef").unwrap().is_none());

    // Every item's parent is known, and the change feed lists each parent before its children (a host never meets
    // an item whose folder it does not know yet).
    let feed = b.core.changes(0, 1000).unwrap();
    let mut seen = vec![ROOT_ID.to_string()];
    for i in &feed.items {
        assert!(seen.contains(&i.parent_id), "{} comes before its parent {}", i.path, i.parent_id);
        seen.push(i.id.clone());
    }

    // B announces the files invalid: no device asks it for content it does not have.
    let entries = b.core.announce_since(0, usize::MAX).unwrap();
    let file = entries.iter().find(|e| e.file.info.path == "docs/a.txt").unwrap();
    assert!(file.invalid);
    assert!(!entries.iter().find(|e| e.file.info.path == "docs").unwrap().invalid, "a directory has no content to lack");
}

#[test]
fn a_fetch_gets_exactly_one_files_content_and_a_cancelled_one_nothing() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    assert_eq!(b.core.request("docs/a.txt").unwrap(), Requested::Fetching(1));
    assert_eq!(item(&mut b, "docs/a.txt").state, ItemState::Downloading);
    assert_eq!(b.core.need_totals().unwrap(), (1, 17), "the one file is needed now");
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.fetched_bytes, 17, "exactly that file's content: {p:?}");
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha");
    assert_eq!(item(&mut b, "docs/a.txt").state, ItemState::Downloaded);
    assert_eq!(item(&mut b, "docs/b.txt").state, ItemState::OnlineOnly);
    assert_eq!(b.core.request("docs/a.txt").unwrap(), Requested::Here);
    // Downloaded content is announced valid again: B serves it from now on.
    let e = b.core.announce_since(0, usize::MAX).unwrap().into_iter().find(|e| e.file.info.path == "docs/a.txt").unwrap();
    assert!(!e.invalid);
    // A scan of the cache finds nothing new.
    let r = b.core.scan("").unwrap();
    assert_eq!((r.changed, r.deleted), (0, 0));

    // Asked for, then taken back before it came: nothing fetched.
    b.core.request("media/big.bin").unwrap();
    b.core.cancel_request("media/big.bin").unwrap();
    assert_eq!(pull(&mut b, &[&a]).fetched_bytes, 0);
    assert_eq!(item(&mut b, "media/big.bin").state, ItemState::OnlineOnly);

    // A whole folder at once (Download Now on a folder).
    assert_eq!(b.core.request("docs").unwrap(), Requested::Fetching(2));
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "docs/old/c.txt").unwrap(), b"charlie, older");
}

#[test]
fn a_pinned_folder_stays_downloaded_through_changes_and_unpinning_keeps_what_is_here() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    pull(&mut b, &[&a]);

    assert_eq!(b.core.pin("docs").unwrap(), 3);
    pull(&mut b, &[&a]);
    for p in ["docs/a.txt", "docs/b.txt", "docs/old/c.txt"] {
        assert!(b.fs.lstat(p).unwrap().is_some(), "{p} downloaded");
        assert!(item(&mut b, p).pinned);
    }
    assert!(b.fs.lstat("top.txt").unwrap().is_none(), "outside the pin: online only");

    // Changes on A: a new file and an edit below the pin, an edit outside it.
    write_all(&a.fs, "docs/new.txt", b"new below the pin").unwrap();
    write_all(&a.fs, "docs/b.txt", b"bravo, edited").unwrap();
    write_all(&a.fs, "top.txt", b"at the top, edited").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "docs/new.txt").unwrap(), b"new below the pin");
    assert_eq!(read_all(&b.fs, "docs/b.txt").unwrap(), b"bravo, edited");
    let top = item(&mut b, "top.txt");
    assert_eq!((top.state, top.size), (ItemState::OnlineOnly, 18), "outside the pin: the new version, online only");

    // Unpinned: what is here stays; new files below arrive online only again.
    b.core.unpin("docs").unwrap();
    assert!(b.fs.lstat("docs/new.txt").unwrap().is_some());
    write_all(&a.fs, "docs/later.txt", b"after the unpin").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(item(&mut b, "docs/later.txt").state, ItemState::OnlineOnly);
}

#[test]
fn evicting_frees_the_space_and_a_fetch_brings_it_back_but_an_only_copy_is_kept() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    // A knows B holds docs/a.txt now.
    send(&b, &mut a);

    let e = evict(&mut b, "docs", &[&a]);
    assert_eq!((e.files, e.bytes, e.kept), (3, 17 + 5 + 14, 0));
    assert!(b.fs.lstat("docs/a.txt").unwrap().is_none());
    assert_eq!(item(&mut b, "docs/a.txt").state, ItemState::OnlineOnly);
    assert_eq!(b.core.local_totals().unwrap(), (0, 0));
    // A hears B does not serve them any more: a device that needs them asks A only.
    send(&b, &mut a);
    let c = {
        let mut c = dev("C", 3, &rw(), false);
        send(&b, &mut c);
        send(&a, &mut c);
        c
    };
    let n = c.core.store().need("docs/a.txt").unwrap().unwrap();
    assert_eq!(n.holders, vec![a.core.me()], "only A holds it");

    // Back again.
    b.core.request("docs/a.txt").unwrap();
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.fetched_bytes, 17);
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha");

    // A file only B has (made here, no other device holds it yet) is kept, and so is a pinned one.
    write_all(&b.fs, "mine.txt", b"only on B").unwrap();
    b.core.host_written("mine.txt").unwrap();
    assert_eq!(item(&mut b, "mine.txt").state, ItemState::ModifiedHere);
    b.core.pin("docs/a.txt").unwrap();
    let e = evict(&mut b, "", &[&a]);
    assert_eq!(e.kept, 2, "{e:?}");
    assert!(b.fs.lstat("mine.txt").unwrap().is_some() && b.fs.lstat("docs/a.txt").unwrap().is_some());
    // Once A has it, it can go.
    sync(&mut a, &mut b);
    assert_eq!(item(&mut b, "mine.txt").state, ItemState::Downloaded);
    assert_eq!(evict(&mut b, "mine.txt", &[&a]).files, 1);
}

#[test]
fn a_file_changed_elsewhere_stays_online_only_if_it_was_and_is_updated_if_it_was_here() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    b.core.request("docs/a.txt").unwrap();
    pull(&mut b, &[&a]);
    let before = (item(&mut b, "docs/a.txt"), item(&mut b, "docs/b.txt"));

    write_all(&a.fs, "docs/a.txt", b"alpha, changed on A").unwrap();
    write_all(&a.fs, "docs/b.txt", b"bravo, changed on A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.fetched_bytes, 19, "only the downloaded file's new content: {p:?}");
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha, changed on A");
    let (ia, ib) = (item(&mut b, "docs/a.txt"), item(&mut b, "docs/b.txt"));
    assert_eq!((ia.state, ib.state), (ItemState::Downloaded, ItemState::OnlineOnly));
    assert_ne!(ia.content_version, before.0.content_version);
    assert_ne!(ib.content_version, before.1.content_version, "the new version's content, though not here");
    assert_ne!(ib.metadata_version, before.1.metadata_version);
    assert_eq!(ib.size, 19);
}

#[test]
fn the_hosts_changes_become_versions_on_the_other_devices() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);

    // A new folder and file, an edit of a downloaded file, a delete of an online-only one and of a folder.
    b.fs.create_dir("new").unwrap();
    b.core.host_written("new").unwrap();
    write_all(&b.fs, "new/n.txt", b"made on B").unwrap();
    b.core.host_written("new/n.txt").unwrap();
    b.core.request("docs/a.txt").unwrap();
    pull(&mut b, &[&a]);
    write_all(&b.fs, "docs/a.txt", b"alpha, edited on B").unwrap();
    b.core.host_written("docs/a.txt").unwrap();
    assert_eq!(host_delete(&mut b, "top.txt", &[]), Deleted::Deleted { items: 1, failed: vec![] });
    assert_eq!(host_delete(&mut b, "docs/old", &[]), Deleted::Deleted { items: 2, failed: vec![] });
    assert!(b.core.item_at("docs/old/c.txt").unwrap().is_none());

    sync(&mut a, &mut b);
    assert_eq!(read_all(&a.fs, "new/n.txt").unwrap(), b"made on B");
    assert_eq!(read_all(&a.fs, "docs/a.txt").unwrap(), b"alpha, edited on B");
    assert!(a.fs.lstat("top.txt").unwrap().is_none());
    assert!(a.fs.lstat("docs/old").unwrap().is_none());
    assert_eq!(a.core.need_totals().unwrap(), (0, 0));
    // Nothing is a conflict, and B, though it never had top.txt's content, deleted the version A had.
    assert!(a.core.conflicts().unwrap().is_empty());
}

#[test]
fn concurrent_edits_make_a_conflict_copy_as_in_a_full_folder() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs/b.txt").unwrap();
    pull(&mut b, &[&a]);

    // Both edit before hearing of the other; A's is newer (its clock is a few seconds on).
    write_all(&b.fs, "docs/b.txt", b"bravo from B").unwrap();
    b.core.host_written("docs/b.txt").unwrap();
    write_all(&a.fs, "docs/b.txt", b"bravo from A, later").unwrap();
    for _ in 0..5 {
        a.core.scan("").unwrap();
    }
    a.core.scan("").unwrap();
    sync(&mut a, &mut b);
    let ca = a.core.conflicts().unwrap();
    let cb = b.core.conflicts().unwrap();
    assert_eq!(ca.len(), 1, "{ca:?}");
    assert_eq!(ca, cb, "the same conflict copy on both");
    assert_eq!(read_all(&a.fs, "docs/b.txt").unwrap(), read_all(&b.fs, "docs/b.txt").unwrap());
}

#[test]
fn a_rename_keeps_the_items_identifier_and_moves_online_only_files_without_their_content() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs/a.txt").unwrap();
    pull(&mut b, &[&a]);
    let ids: Vec<String> = ["docs", "docs/a.txt", "docs/b.txt", "docs/old/c.txt"].iter().map(|p| item(&mut b, p).id).collect();
    let anchor = b.core.anchor().unwrap();

    // B holds only one of the files there: A, which holds them all, makes the rename, and B takes it.
    assert_eq!(rename_by(&mut b, "docs", "papers", &mut a), 5, "the folder, its subfolder and three files");
    for (old, id) in ["docs", "docs/a.txt", "docs/b.txt", "docs/old/c.txt"].iter().zip(&ids) {
        let new = old.replacen("docs", "papers", 1);
        assert_eq!(&item(&mut b, &new).id, id, "{old} → {new} keeps its identifier");
        assert_eq!(b.core.item(id).unwrap().unwrap().path, new);
        assert!(b.core.item_at(old).unwrap().is_none(), "{old} is gone");
    }
    // The file B held moved with its content (nothing fetched); the others stay online only.
    assert_eq!(item(&mut b, "papers/a.txt").state, ItemState::Downloaded);
    assert_eq!(item(&mut b, "papers/b.txt").state, ItemState::OnlineOnly);
    assert_eq!(read_all(&b.fs, "papers/a.txt").unwrap(), b"alpha alpha alpha");
    // The host hears of changed items, no deletes (the items live on under new names).
    let ch = b.core.changes(anchor, 1000).unwrap();
    for id in &ids {
        assert!(ch.items.iter().any(|i| &i.id == id), "{id} reported changed");
    }
    assert!(ch.deleted.iter().all(|d| !ids.contains(d)), "no renamed item reported deleted: {:?}", ch.deleted);
    // The renamed versions are A's, held there: B can fetch the moved online-only file.
    assert_eq!(read_all(&a.fs, "papers/b.txt").unwrap(), b"bravo");
    assert!(a.fs.lstat("docs").unwrap().is_none());
    b.core.request("papers/b.txt").unwrap();
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "papers/b.txt").unwrap(), b"bravo");
}

#[test]
fn a_device_that_holds_every_file_renames_them_itself() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    let id = item(&mut b, "docs/a.txt").id;
    assert_eq!(b.core.host_rename("docs", "papers").unwrap(), 5);
    assert_eq!(item(&mut b, "papers/a.txt").id, id);
    assert_eq!(item(&mut b, "papers/a.txt").state, ItemState::ModifiedHere, "its own version until another device takes it");
    sync(&mut a, &mut b);
    assert_eq!(read_all(&a.fs, "papers/old/c.txt").unwrap(), b"charlie, older");
    assert!(a.fs.lstat("docs").unwrap().is_none());
    assert_eq!(item(&mut b, "papers/a.txt").state, ItemState::Downloaded);
    // A file changed on the disk since it was recorded is not held as recorded: the rename goes to a holder.
    write_all(&b.fs, "papers/b.txt", b"bravo, edited behind the record").unwrap();
    assert!(matches!(b.core.host_rename("papers", "docs"), Err(HostError::NeedsHolder)));
    assert!(b.core.item_at("papers/b.txt").unwrap().is_some(), "nothing renamed");
}

#[test]
fn the_change_feed_reports_exactly_what_changed() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    // Everything from the start: every item, nothing deleted.
    let all = b.core.changes(0, 1000).unwrap();
    assert_eq!(all.items.len(), 8, "{:?}", names(&all.items));
    assert!(all.deleted.is_empty() && !all.more);
    let anchor = b.core.anchor().unwrap();
    assert_eq!(b.core.changes(anchor, 1000).unwrap().items, vec![], "nothing after now");
    let gone_id = item(&mut b, "docs/b.txt").id;

    write_all(&a.fs, "docs/a.txt", b"alpha, again").unwrap();
    a.fs.remove_file("docs/b.txt").unwrap();
    write_all(&a.fs, "fresh.txt", b"fresh").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    let ch = b.core.changes(anchor, 1000).unwrap();
    let mut changed: Vec<&str> = ch.items.iter().map(|i| i.path.as_str()).collect();
    changed.sort();
    // docs: b.txt was deleted from it.
    assert_eq!(changed, ["docs", "docs/a.txt", "fresh.txt"]);
    assert_eq!(ch.deleted, vec![gone_id]);
    assert_eq!(b.core.changes(ch.anchor, 1000).unwrap().items, vec![]);
    // An anchor of another index (one made anew since), or from the future, has expired.
    let other = trs_sync::ondemand::anchor_of(0xbeef, 3);
    assert_eq!(b.core.changes(other, 10).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert!(b.core.changes(ch.anchor + 1000, 10).is_err());
    // In pages.
    let p1 = b.core.changes(anchor, 1).unwrap();
    assert!(p1.more);
    let p2 = b.core.changes(p1.anchor, 1000).unwrap();
    let mut paged: Vec<&str> = p1.items.iter().chain(&p2.items).map(|i| i.path.as_str()).collect();
    paged.sort();
    paged.dedup();
    assert_eq!(paged, ["docs", "docs/a.txt", "fresh.txt"], "the same, in pages (a folder may be reported in each)");
    assert_eq!(p1.deleted.len() + p2.deleted.len(), 1);
}

#[test]
fn an_on_demand_device_never_holds_up_a_full_one() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    // C, full, hears of the folder from B first: B lists every file but can give none, so C waits for A, never for B.
    let mut c = dev("C", 3, &rw(), false);
    send(&b, &mut c);
    let p = pull(&mut c, &[&b]);
    assert_eq!(p.fetched_bytes, 0);
    assert!(c.fs.lstat("docs/a.txt").unwrap().is_none());
    assert_eq!(c.core.need_totals().unwrap().0, 0, "B's online-only files are no versions for C to take");
    send(&a, &mut c);
    let p = pull(&mut c, &[&a, &b]);
    assert!(p.failed.is_empty() && p.waiting == 0, "{p:?}");
    assert_eq!(read_all(&c.fs, "media/big.bin").unwrap().len(), 300_000);
    assert_eq!(c.core.need_totals().unwrap(), (0, 0));
}

#[test]
fn switching_a_folder_to_on_demand_and_back_loses_nothing() {
    let key = rw();
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &key, false);
    sync(&mut a, &mut b);
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha");

    // Full → on demand: everything stays downloaded; it can be let go now.
    b.core.set_on_demand(true).unwrap();
    assert_eq!(item(&mut b, "docs/a.txt").state, ItemState::Downloaded);
    let e = evict(&mut b, "", &[&a]);
    assert_eq!(e.files, 5);
    assert_eq!(b.core.scan("").unwrap().deleted, 0, "missing content is no delete");
    sync(&mut a, &mut b);
    assert_eq!(read_all(&a.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha", "A lost nothing");

    // On demand → full: every file is fetched again.
    b.core.set_on_demand(false).unwrap();
    pull(&mut b, &[&a]);
    for p in ["docs/a.txt", "docs/b.txt", "docs/old/c.txt", "media/big.bin", "top.txt"] {
        assert_eq!(read_all(&b.fs, p).unwrap(), read_all(&a.fs, p).unwrap(), "{p}");
    }
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));
    assert_eq!(b.core.local_totals().unwrap(), a.core.local_totals().unwrap());
    let r = b.core.scan("").unwrap();
    assert_eq!((r.changed, r.deleted), (0, 0));
}

#[test]
fn content_missing_from_the_cache_is_online_only_never_a_delete_and_a_read_only_member_cannot_edit() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&a]);
    // Something removed the file from the cache behind Seedwell's back.
    b.fs.remove_file("top.txt").unwrap();
    let r = b.core.scan("").unwrap();
    assert_eq!(r.deleted, 0);
    assert_eq!(item(&mut b, "top.txt").state, ItemState::OnlineOnly);
    sync(&mut a, &mut b);
    assert!(a.fs.lstat("top.txt").unwrap().is_some(), "A still has it");

    // A read-only member lists and fetches, but the host's changes are refused.
    let ro = rw().read_only().unwrap();
    let mut r = dev("R", 4, &ro, true);
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    assert_eq!(item(&mut r, "top.txt").state, ItemState::OnlineOnly);
    r.core.request("top.txt").unwrap();
    pull(&mut r, &[&a]);
    assert_eq!(read_all(&r.fs, "top.txt").unwrap(), b"at the top");
    assert!(matches!(r.core.host_rename("top.txt", "t.txt"), Err(HostError::ReadOnly)));
}

/// Every item of a feed comes after its parent (known before the feed, or reported earlier in it).
fn parents_first(d: &mut Dev, feed: &[trs_sync::ondemand::Item], known: &[String]) {
    let mut seen: Vec<String> = known.to_vec();
    seen.push(ROOT_ID.into());
    for i in feed {
        assert!(seen.contains(&i.parent_id), "{} comes before its parent {}", i.path, i.parent_id);
        seen.push(i.id.clone());
    }
    let _ = d;
}

#[test]
fn a_file_is_evicted_only_while_another_device_holds_it() {
    // Two on-demand devices hold a file B made; both let go at once, before hearing of the other. Of the two, the
    // higher device id never relies on the lower: one copy stays.
    let mut a = dev("A", 1, &rw(), true);
    let mut b = dev("B", 2, &rw(), true);
    write_all(&b.fs, "x.txt", b"the only copy, soon").unwrap();
    b.core.host_written("x.txt").unwrap();
    sync(&mut a, &mut b);
    a.core.request("x.txt").unwrap();
    pull(&mut a, &[&b]);
    sync(&mut a, &mut b);
    // Both ask the other before either lets go, then both free what was confirmed.
    let ca = asked(&mut a, "x.txt", false, &[&b]);
    let cb = asked(&mut b, "x.txt", false, &[&a]);
    let (ea, eb) = (a.core.evict("x.txt", &ca).unwrap(), b.core.evict("x.txt", &cb).unwrap());
    let (lo, hi) = if a.core.me().0 < b.core.me().0 { (&ea, &eb) } else { (&eb, &ea) };
    assert_eq!((lo.files, hi.files, hi.kept), (1, 0, 1), "{ea:?} {eb:?}");
    assert_eq!(hi.kept_why[0].1, trs_sync::ondemand::NOT_HELD_ELSEWHERE);
    assert!(read_all(&a.fs, "x.txt").is_ok() || read_all(&b.fs, "x.txt").is_ok(), "a copy is left");
    sync(&mut a, &mut b);
    let (holder, other) = if a.fs.lstat("x.txt").unwrap().is_some() { (&mut a, &mut b) } else { (&mut b, &mut a) };
    assert_eq!(item(holder, "x.txt").state, ItemState::ModifiedHere, "the only copy is here now");
    assert_eq!(evict(holder, "x.txt", &[&*other]).kept, 1);

    // A device with the version online only is no holder either: its rename is made by the holder, which still may not
    // let go.
    rename_by(other, "x.txt", "y.txt", holder);
    sync(holder, other);
    assert!(other.core.store().local("y.txt").unwrap().unwrap().dataless);
    assert_eq!(read_all(&holder.fs, "y.txt").unwrap(), b"the only copy, soon");
    assert_eq!(evict(holder, "y.txt", &[&*other]).kept, 1, "online only there: kept here");
}

#[test]
fn a_file_is_evicted_only_while_a_device_that_has_it_is_connected() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);
    // A holds it, but is not connected (or its announcement is from an earlier session): kept, and why.
    let e = evict(&mut b, "top.txt", &[]);
    assert_eq!((e.files, e.kept), (0, 1));
    assert_eq!(e.kept_why, vec![("top.txt".to_string(), trs_sync::ondemand::NOT_HELD_ELSEWHERE.to_string())]);
    // A full device holding it, connected: always enough.
    assert_eq!(evict(&mut b, "top.txt", &[&a]).files, 1);
}

#[test]
fn a_rename_on_another_on_demand_device_keeps_the_only_copy() {
    // B made x.txt (the only copy); A, on demand too, has it online only and renames it: B, which holds it, makes it.
    let mut a = dev("A", 1, &rw(), true);
    let mut b = dev("B", 2, &rw(), true);
    write_all(&b.fs, "x.txt", b"made on B, only here").unwrap();
    b.core.host_written("x.txt").unwrap();
    sync(&mut a, &mut b);
    assert_eq!(item(&mut b, "x.txt").state, ItemState::ModifiedHere);
    rename_by(&mut a, "x.txt", "y.txt", &mut b);
    sync(&mut a, &mut b);
    // B moved its copy to the new name: still there, nothing archived, nothing lost.
    assert_eq!(read_all(&b.fs, "y.txt").unwrap(), b"made on B, only here");
    assert!(b.fs.lstat("x.txt").unwrap().is_none());
    assert_ne!(item(&mut b, "y.txt").state, ItemState::OnlineOnly);
    assert!(b.fs.lstat(".seedwell/Archive").unwrap().is_none(), "nothing went to the archive");
    assert_eq!(item(&mut a, "y.txt").state, ItemState::OnlineOnly);
    a.core.request("y.txt").unwrap();
    pull(&mut a, &[&b]);
    assert_eq!(read_all(&a.fs, "y.txt").unwrap(), b"made on B, only here");
}
#[test]
fn a_new_name_that_arrives_before_the_old_ones_delete_still_gets_the_content() {
    // B holds x.txt; A renames it. B hears of y.txt first (the delete of x.txt comes in a later batch): online only for
    // now, then the delete moves B's copy there.
    let mut a = dev("A", 1, &rw(), false);
    let mut b = dev("B", 2, &rw(), true);
    write_all(&a.fs, "x.txt", b"made on A").unwrap();
    a.core.scan("").unwrap();
    sync(&mut a, &mut b);
    b.core.request("x.txt").unwrap();
    pull(&mut b, &[&a]);
    a.fs.rename("x.txt", "y.txt").unwrap();
    a.core.scan("").unwrap();
    let (index_id, _) = a.core.index_state().unwrap();
    let all = a.core.announce_since(0, usize::MAX).unwrap();
    let (first, later): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.file.info.path != "x.txt");
    learn_writers(&mut b.core, &a.core.writers()).unwrap();
    b.core.apply_remote(a.core.me(), RemoteIndex { index_id, full: false, done: false, entries: first }).unwrap();
    pull(&mut b, &[]);
    assert!(b.core.store().local("y.txt").unwrap().unwrap().dataless);
    b.core.apply_remote(a.core.me(), RemoteIndex { index_id, full: false, done: false, entries: later }).unwrap();
    let p = pull(&mut b, &[]);
    assert_eq!(p.fetched_bytes, 0, "moved, not fetched: {p:?}");
    assert_eq!(read_all(&b.fs, "y.txt").unwrap(), b"made on A", "the delete moved the copy to the new name");
    assert!(b.fs.lstat(".seedwell/Archive").unwrap().is_none());
}
#[test]
fn a_downloaded_file_renamed_on_a_full_device_stays_downloaded() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs/a.txt").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);
    a.fs.rename("docs/a.txt", "docs/z.txt").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let p = pull(&mut b, &[&a]);
    assert_eq!(p.fetched_bytes, 0, "moved, not fetched: {p:?}");
    assert_eq!(read_all(&b.fs, "docs/z.txt").unwrap(), b"alpha alpha alpha");
    assert_eq!(item(&mut b, "docs/z.txt").state, ItemState::Downloaded);
    assert!(b.fs.lstat(".seedwell/Archive").unwrap().is_none(), "nothing went to the archive");
}

#[test]
fn a_case_only_rename_works_on_a_disk_that_folds_case() {
    let mut b = dev_on("B", 2, &rw(), true, true);
    write_all(&b.fs, "Notes.txt", b"notes").unwrap();
    b.core.host_written("Notes.txt").unwrap();
    let id = item(&mut b, "Notes.txt").id;
    assert_eq!(b.core.host_rename("Notes.txt", "notes.txt").unwrap(), 1);
    assert_eq!(item(&mut b, "notes.txt").id, id);
    assert!(b.core.item_at("Notes.txt").unwrap().is_none());
    assert_eq!(b.fs.read_dir("").unwrap().entries.iter().filter(|e| e.name != ".seedwell").map(|e| e.name.as_str()).collect::<Vec<_>>(), ["notes.txt"]);
    // Another file at the new name is still refused.
    write_all(&b.fs, "other.txt", b"other").unwrap();
    b.core.host_written("other.txt").unwrap();
    assert!(matches!(b.core.host_rename("notes.txt", "OTHER.txt"), Err(HostError::Exists(_))));
}

#[test]
fn identifiers_do_not_depend_on_the_order_records_are_read_in() {
    // B and C rename the same file at once (to y.txt and z.txt): two live paths with one file id everywhere. Which
    // of them keeps the file id's identifier is the same on both, and after a restart (identifiers read anew).
    let mut a = full_with_files();
    write_all(&a.fs, "x.txt", b"renamed twice").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    let mut c = dev("C", 3, &rw(), true);
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    let x = item(&mut b, "x.txt").id;
    // Both hold it (a device renames only what it holds).
    for d in [&mut b, &mut c] {
        d.core.request("x.txt").unwrap();
        pull(d, &[&a]);
    }
    c.core.host_rename("x.txt", "z.txt").unwrap();
    b.core.host_rename("x.txt", "y.txt").unwrap();
    // B reads its own rename before hearing of C's.
    assert_eq!(item(&mut b, "y.txt").id, x);
    let anchor = b.core.anchor().unwrap();
    sync(&mut b, &mut c);
    let ids = |d: &mut Dev| (item(d, "y.txt").id, item(d, "z.txt").id);
    let (on_b, on_c) = (ids(&mut b), ids(&mut c));
    assert_eq!(on_b, on_c, "the same identifiers on both");
    assert_eq!(on_b.0, x, "the first path keeps the file id's identifier (the versions are concurrent)");
    assert_eq!(on_b.1, "p:z.txt");
    let mut fresh = trs_sync::ondemand::Items::default();
    let pins = Default::default();
    assert_eq!(fresh.item_at(c.core.store(), &pins, "y.txt").unwrap().unwrap().id, x, "the same after a restart");
    // C's feed tells its host that the identifier moved: x's item is at y.txt now, z.txt is a new one.
    let _ = anchor;
    let feed = c.core.changes(0, 1000).unwrap();
    assert!(feed.items.iter().any(|i| i.id == x && i.path == "y.txt"), "{:?}", feed.items);
    assert!(feed.items.iter().any(|i| i.id == "p:z.txt"));
}

#[test]
fn the_feed_reports_downloading_and_kept_items_and_parents_first() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    let anchor = b.core.anchor().unwrap();
    b.core.request("docs/a.txt").unwrap();
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "docs/a.txt" && i.state == ItemState::Downloading), "{:?}", ch.items);
    pull(&mut b, &[&a]);
    let anchor = b.core.changes(anchor, 1000).unwrap().anchor;
    // Pinning a folder: every item below it shows it, the downloaded one too. (A file pinned first keeps its earlier
    // sequence: its folders now come after it in the index.)
    b.core.pin("docs/old/c.txt").unwrap();
    let anchor = b.core.changes(anchor, 1000).unwrap().anchor;
    b.core.pin("docs").unwrap();
    let ch = b.core.changes(anchor, 1000).unwrap();
    for p in ["docs", "docs/a.txt", "docs/b.txt"] {
        assert!(ch.items.iter().any(|i| i.path == p && i.pinned), "{p} reported kept: {:?}", ch.items);
    }
    // A folder changed after its files (its record has the later sequence) still comes first.
    let known: Vec<String> = Vec::new();
    let all = b.core.changes(0, 1000).unwrap();
    parents_first(&mut b, &all.items, &known);
    let mut pages = Vec::new();
    let mut at = 0;
    loop {
        let p = b.core.changes(at, 2).unwrap();
        pages.extend(p.items);
        at = p.anchor;
        if !p.more {
            break;
        }
    }
    parents_first(&mut b, &pages, &known);
}

#[test]
fn a_kept_file_missing_from_the_cache_is_fetched_again() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.pin("top.txt").unwrap();
    pull(&mut b, &[&a]);
    b.fs.remove_file("top.txt").unwrap();
    b.core.scan("").unwrap();
    assert_eq!(item(&mut b, "top.txt").state, ItemState::Downloading);
    pull(&mut b, &[&a]);
    assert_eq!(read_all(&b.fs, "top.txt").unwrap(), b"at the top");
}

#[test]
fn a_file_that_cannot_be_removed_stays_and_the_others_go() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);

    // Evict: a.txt is open elsewhere (the disk reads before the removal fail too: when the holders are asked, and when
    // the files are picked again).
    b.fs.fail_next("docs/a.txt", 3, std::io::ErrorKind::PermissionDenied);
    let e = evict(&mut b, "docs", &[&a]);
    assert_eq!((e.files, e.failed.len()), (2, 1), "{e:?}");
    assert_eq!(e.failed[0].0, "docs/a.txt");
    assert_eq!(item(&mut b, "docs/a.txt").state, ItemState::Downloaded);
    assert_eq!(item(&mut b, "docs/b.txt").state, ItemState::OnlineOnly);
    assert_eq!(b.core.scan("").unwrap().changed, 0, "every record says what the disk has");

    // Delete: the same; the folder above it stays.
    b.fs.fail_next("docs/a.txt", 1, std::io::ErrorKind::PermissionDenied);
    let Deleted::Deleted { items, failed } = host_delete(&mut b, "docs", &[]) else { panic!("a writer deletes") };
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(items, 3, "b.txt, old/c.txt and old");
    assert!(b.core.item_at("docs/a.txt").unwrap().is_some() && b.core.item_at("docs").unwrap().is_some());
    assert!(b.core.item_at("docs/b.txt").unwrap().is_none());
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha");
}

#[test]
fn a_read_only_members_delete_frees_the_content_and_sends_nothing() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let ro = rw().read_only().unwrap();
    let mut r = dev("R", 4, &ro, true);
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    r.core.pin("docs").unwrap();
    r.core.request("top.txt").unwrap();
    pull(&mut r, &[&a]);
    let before = r.core.announce_since(0, usize::MAX).unwrap().len();
    let (_, seq) = r.core.index_state().unwrap();

    let Deleted::Freed(e) = host_delete(&mut r, "top.txt", &[&a]) else { panic!("a read-only member frees") };
    assert_eq!((e.files, e.bytes), (1, 10));
    assert_eq!(item(&mut r, "top.txt").state, ItemState::OnlineOnly, "still listed, online only");
    // A pinned folder: unpinned, then freed.
    let Deleted::Freed(e) = host_delete(&mut r, "docs", &[&a]) else { panic!() };
    assert_eq!(e.files, 3);
    assert!(r.core.pins().is_empty());
    assert!(!item(&mut r, "docs/a.txt").pinned);
    pull(&mut r, &[&a]);
    assert_eq!(item(&mut r, "docs/a.txt").state, ItemState::OnlineOnly, "not fetched again");
    // Nothing deleted for the others: R announces no delete, and can fetch them again.
    let news = r.core.announce_since(seq, usize::MAX).unwrap();
    assert!(news.iter().all(|e| !e.file.info.deleted), "{news:?}");
    assert!(before > 0);
    r.core.request("top.txt").unwrap();
    pull(&mut r, &[&a]);
    assert_eq!(read_all(&r.fs, "top.txt").unwrap(), b"at the top");
    // Writes stay refused.
    assert!(matches!(r.core.host_check_write("new.txt", false), Err(HostError::ReadOnly)));
    assert!(matches!(r.core.host_rename("top.txt", "t.txt"), Err(HostError::ReadOnly)));
}

#[test]
fn the_hosts_changes_are_checked_before_anything_is_touched() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    assert!(matches!(b.core.host_check_write("docs", false), Err(HostError::Exists(_))), "a file over a folder");
    assert!(matches!(b.core.host_check_write("top.txt", true), Err(HostError::Exists(_))), "a folder over a file");
    assert!(matches!(b.core.host_check_write("top.txt/x", false), Err(HostError::Exists(_))), "below a file");
    assert!(matches!(b.core.host_check_write(".seedwell/x", false), Err(HostError::BadName(_))));
    assert!(b.core.host_check_write("docs/new.txt", false).is_ok());
    assert!(matches!(b.core.host_check_rename("nope", "x"), Err(HostError::NotFound(_))));
    assert!(matches!(b.core.host_check_rename("docs", "top.txt"), Err(HostError::Exists(_))));
    assert!(matches!(b.core.host_check_rename("docs", "docs/in"), Err(HostError::BadName(_))));
    assert_eq!(b.core.host_check_rename("docs", "papers").unwrap().len(), 5);
}

fn pins(d: &Dev) -> Vec<&str> {
    d.core.pins().iter().map(String::as_str).collect()
}

#[test]
fn freeing_a_file_below_a_pinned_folder_splits_the_pin() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.pin("").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);

    // Remove Download of a file kept through the whole folder's pin: every folder on the way is unpinned, their
    // other children keep their own pins, and only the file goes.
    let anchor = b.core.anchor().unwrap();
    let e = evict(&mut b, "docs/old/c.txt", &[&a]);
    assert_eq!((e.files, e.kept), (1, 0), "{e:?}");
    assert_eq!(pins(&b), ["docs/a.txt", "docs/b.txt", "media", "top.txt"]);
    assert_eq!(item(&mut b, "docs/old/c.txt").state, ItemState::OnlineOnly);
    for p in ["", "docs", "docs/old", "docs/old/c.txt"] {
        assert!(!item(&mut b, p).pinned, "{p} no longer kept");
    }
    for p in ["docs/a.txt", "docs/b.txt", "media", "media/big.bin", "top.txt"] {
        assert!(item(&mut b, p).pinned, "{p} still kept");
    }
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "docs" && !i.pinned), "the host hears of it: {:?}", ch.items);
    pull(&mut b, &[&a]);
    assert!(b.fs.lstat("docs/old/c.txt").unwrap().is_none(), "not fetched again");
    // A file added later to a split folder is not kept any more; one in a folder still pinned is.
    write_all(&a.fs, "docs/later.txt", b"later").unwrap();
    write_all(&a.fs, "media/more.bin", b"more").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(item(&mut b, "docs/later.txt").state, ItemState::OnlineOnly);
    assert_eq!(read_all(&b.fs, "media/more.bin").unwrap(), b"more");

    // A file pinned itself stays (the user unpins it first), and its pins with it.
    let e = evict(&mut b, "docs/a.txt", &[&a]);
    assert_eq!((e.files, e.kept), (0, 1));
    assert!(pins(&b).contains(&"docs/a.txt"));
    // The only copy cannot go: the pin above it is not split for nothing.
    b.core.pin("docs").unwrap();
    write_all(&b.fs, "docs/mine.txt", b"only on B").unwrap();
    b.core.host_written("docs/mine.txt").unwrap();
    let before = b.core.pins().clone();
    assert_eq!(evict(&mut b, "docs/mine.txt", &[&a]).kept, 1);
    assert_eq!(b.core.pins(), &before);
}

#[test]
fn a_read_only_members_delete_below_a_pinned_folder_splits_the_pin() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let ro = rw().read_only().unwrap();
    let mut r = dev("R", 4, &ro, true);
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    r.core.pin("docs").unwrap();
    pull(&mut r, &[&a]);
    let Deleted::Freed(e) = host_delete(&mut r, "docs/b.txt", &[&a]) else { panic!("a read-only member frees") };
    assert_eq!(e.files, 1);
    assert_eq!(pins(&r), ["docs/a.txt", "docs/old"]);
    assert_eq!(item(&mut r, "docs/b.txt").state, ItemState::OnlineOnly);
    assert_eq!(read_all(&r.fs, "docs/old/c.txt").unwrap(), b"charlie, older", "the rest stays");
}

#[test]
fn a_path_made_anew_retires_its_old_identifier() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    let old = item(&mut b, "top.txt").id;
    let anchor = b.core.anchor().unwrap();
    // Deleted and made anew on A (another file id), and B takes only the new one.
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    write_all(&a.fs, "top.txt", b"a new file").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    let new = item(&mut b, "top.txt").id;
    assert_ne!(new, old);
    assert!(b.core.item(&old).unwrap().is_none(), "the old identifier names nothing");
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.deleted.contains(&old), "{ch:?}");
    assert!(ch.items.iter().any(|i| i.id == new));
}

#[test]
fn an_edit_of_a_version_that_moved_on_is_a_conflict_copy() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs/b.txt").unwrap();
    pull(&mut b, &[&a]);
    let v1 = item(&mut b, "docs/b.txt").content_version;
    // A's v2 arrives before the host reports the user's edit of v1.
    write_all(&a.fs, "docs/b.txt", b"bravo v2 from A").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    let to = b.core.host_target("docs/b.txt", Some(&v1), "Beta").unwrap();
    assert_ne!(to, "docs/b.txt");
    assert!(trs_sync::conflict::looks_like_conflict(&to), "{to}");
    assert!(to.contains("Beta"), "{to}");
    write_all(&b.fs, &to, b"bravo, the user's edit of v1").unwrap();
    b.core.host_written_copy(&to, "docs/b.txt").unwrap();
    assert_eq!(read_all(&b.fs, "docs/b.txt").unwrap(), b"bravo v2 from A", "v2 stays");
    assert_eq!(item(&mut b, &to).conflict_of.as_deref(), Some("docs/b.txt"));
    // Based on the current version: written in place. A new file where one exists now: a conflict copy too.
    let v2 = item(&mut b, "docs/b.txt").content_version;
    assert_eq!(b.core.host_target("docs/b.txt", Some(&v2), "Beta").unwrap(), "docs/b.txt");
    assert_ne!(b.core.host_target("top.txt", None, "Beta").unwrap(), "top.txt");
    assert_eq!(b.core.host_target("fresh.txt", None, "Beta").unwrap(), "fresh.txt");
    sync(&mut a, &mut b);
    assert_eq!(read_all(&a.fs, &to).unwrap(), b"bravo, the user's edit of v1");
}

#[test]
fn deleting_a_folder_keeps_what_the_host_never_saw_in_the_archive() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    // Files the index does not know: ignored, a temp, a whole folder of them.
    write_all(&b.fs, "docs/.DS_Store", b"finder").unwrap();
    write_all(&b.fs, "docs/old/.seedwell.x.tmp", b"temp").unwrap();
    b.fs.create_dir("docs/node_modules").unwrap();
    write_all(&b.fs, "docs/node_modules/m.js", b"module").unwrap();
    let Deleted::Deleted { failed, .. } = host_delete(&mut b, "docs", &[]) else { panic!() };
    assert!(failed.is_empty(), "{failed:?}");
    assert!(b.fs.lstat("docs").unwrap().is_none());
    let mut kept = Vec::new();
    let mut stack = vec![".seedwell/Archive".to_string()];
    while let Some(d) = stack.pop() {
        for e in b.fs.read_dir(&d).unwrap().entries {
            let p = format!("{d}/{}", e.name);
            match b.fs.lstat(&p).unwrap().unwrap().kind {
                FileKind::Dir => stack.push(p),
                _ => kept.push(read_all(&b.fs, &p).unwrap()),
            }
        }
    }
    kept.sort();
    assert_eq!(kept, [b"finder".to_vec(), b"module".to_vec(), b"temp".to_vec()]);
}

#[test]
fn pins_follow_renames_and_deletes_and_a_folder_with_nothing_to_free_splits_none() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.pin("docs/old").unwrap();
    pull(&mut b, &[&a]);
    rename_by(&mut b, "docs", "papers", &mut a);
    assert_eq!(pins(&b), ["papers/old"]);
    assert!(item(&mut b, "papers/old/c.txt").pinned);
    b.core.pin("top.txt").unwrap();
    host_delete(&mut b, "top.txt", &[]);
    assert_eq!(pins(&b), ["papers/old"]);
    // Nothing below media can go (online only): the pin above it stays whole.
    b.core.pin("").unwrap();
    let e = evict(&mut b, "media", &[&a]);
    assert_eq!(e.files, 0);
    assert!(pins(&b).contains(&""), "{:?}", pins(&b));
}

#[test]
fn pins_and_requests_reach_the_host_without_sending_peers_anything() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    let (_, seq) = b.core.index_state().unwrap();
    let anchor = b.core.anchor().unwrap();
    b.core.pin("docs").unwrap();
    b.core.request("top.txt").unwrap();
    assert!(b.core.announce_since(seq, usize::MAX).unwrap().is_empty(), "nothing new for peers");
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "docs/a.txt" && i.pinned));
    assert!(ch.items.iter().any(|i| i.path == "top.txt" && i.state == ItemState::Downloading));
    // Once fetched, the new content is sent.
    pull(&mut b, &[&a]);
    assert!(b.core.announce_since(seq, usize::MAX).unwrap().iter().any(|e| e.file.info.path == "top.txt" && !e.invalid));
}


/// `from`'s index to `to` the way a session sends it: in batches of 1000, by sequence, the cursor the last entry sent.
/// `cut`: the session ends after that many batches (the next one starts over, as a full send not finished does).
fn send_paged(from: &Dev, to: &mut Dev, cut: Option<usize>) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let mut sent = 0;
    let mut batches = 0;
    loop {
        let batch = from.core.announce_since(sent, 1000).unwrap();
        let last = batch.len() < 1000;
        sent = batch.last().map_or(sent, |e| e.sequence);
        to.core.apply_remote(from.core.me(), RemoteIndex { index_id, full: true, done: last, entries: batch }).unwrap();
        batches += 1;
        if last || cut == Some(batches) {
            return;
        }
    }
}

#[test]
fn pins_and_requests_never_hide_versions_from_a_peer_that_pages() {
    // A full device with more files than one batch; B on demand takes them all, then pins and asks for some.
    let mut a = dev("A", 1, &rw(), false);
    for i in 0..1500 {
        write_all(&a.fs, &format!("f{i:04}.txt"), format!("file {i}").as_bytes()).unwrap();
    }
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    b.core.pin("f0100.txt").unwrap();
    b.core.request("f1400.txt").unwrap();
    b.core.pin("").unwrap();
    // C hears of the folder from B alone, in pages, and once with the session cut after the first page.
    for cut in [None, Some(1)] {
        let mut c = dev("C", 3, &rw(), false);
        send_paged(&b, &mut c, cut);
        if cut.is_some() {
            send_paged(&b, &mut c, None);
        }
        let known = (0..1500).filter(|i| !c.core.store().remote(&format!("f{i:04}.txt")).unwrap().is_empty()).count();
        assert_eq!(known, 1500, "every version reaches C (cut: {cut:?})");
    }
}

#[test]
fn the_feed_log_and_retired_identifiers_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("b.redb");
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let store = Box::new(trs_sync::store_redb::RedbStore::open(&db, 2).unwrap());
    let mut b = Dev { core: core_on(&fs, 2, &rw(), true, false, store), fs, name: "B" };
    sync(&mut a, &mut b);
    let old = item(&mut b, "top.txt").id;
    let anchor = b.core.anchor().unwrap();
    b.core.pin("docs").unwrap();
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    write_all(&a.fs, "top.txt", b"a new file").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    // Opened again (a restart; a new handle): the feed still has both.
    let Dev { fs, core, .. } = b;
    drop(core);
    let store = Box::new(trs_sync::store_redb::RedbStore::open(&db, 2).unwrap());
    let mut b = Dev { core: core_on(&fs, 2, &rw(), true, false, store), fs, name: "B" };
    b.core.set_pins(["docs".to_string()]);
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "docs/a.txt" && i.pinned), "{:?}", ch.items);
    assert!(ch.deleted.contains(&old), "{ch:?}");
}

#[test]
fn a_holder_answers_for_what_it_holds_and_ranks_above_the_asker() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let (mut b, mut c) = (dev("B", 2, &rw(), true), dev("C", 3, &rw(), true));
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&a]);
    c.core.request("top.txt").unwrap();
    pull(&mut c, &[&a]);
    let rec = b.core.store().local("top.txt").unwrap().unwrap().file.info;
    let q = |f: &FileInfo| [HeldFile { path: f.path.clone(), version: f.version.clone(), content: f.content_digest() }];
    // A full holder: always.
    assert_eq!(a.core.holds(b.core.me(), &q(&rec), &|_| false), [true]);
    // On demand both: only the higher answers yes, to the lower.
    let (lo, hi) = if b.core.me().0 < c.core.me().0 { (&b, &c) } else { (&c, &b) };
    assert_eq!(hi.core.holds(lo.core.me(), &q(&rec), &|_| false), [true]);
    assert_eq!(lo.core.holds(hi.core.me(), &q(&rec), &|_| false), [false]);
    // Online only there, or another content: no.
    let docs = b.core.store().local("docs/a.txt").unwrap().unwrap().file.info;
    assert_eq!(hi.core.holds(lo.core.me(), &q(&docs), &|_| false), [false]);
    let mut other = rec.clone();
    other.chunks[0].hash[0] ^= 1;
    assert_eq!(a.core.holds(b.core.me(), &q(&other), &|_| false), [false]);
}

#[test]
fn an_edit_whose_content_did_not_change_since_is_no_conflict() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs/b.txt").unwrap();
    pull(&mut b, &[&a]);
    let base = item(&mut b, "docs/b.txt").content_version;
    // A new version, the same content (only its time moved).
    a.fs.set_mtime("docs/b.txt", 1_700_000_000_000_000_000).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    assert_eq!(b.core.host_target("docs/b.txt", Some(&base), "Beta").unwrap(), "docs/b.txt");
    // A rename keeps the content: the same base still fits under the new name.
    b.core.host_rename("docs/b.txt", "docs/bb.txt").unwrap();
    assert_eq!(b.core.host_target("docs/bb.txt", Some(&base), "Beta").unwrap(), "docs/bb.txt");
}


#[test]
fn only_a_writer_whose_folder_and_file_are_there_says_it_holds() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    let rec = a.core.store().local("top.txt").unwrap().unwrap().file.info;
    let q = [HeldFile { path: rec.path.clone(), version: rec.version.clone(), content: rec.content_digest() }];
    assert_eq!(a.core.holds(b.core.me(), &q, &|_| false), [true]);
    // A read-only member: its own later change is never sent, so its copy is no copy for others.
    let mut r = dev("R", 4, &rw().read_only().unwrap(), false);
    send(&a, &mut r);
    pull(&mut r, &[&a]);
    assert!(r.fs.lstat("top.txt").unwrap().is_some());
    assert_eq!(r.core.holds(b.core.me(), &q, &|_| false), [false]);
    // The file changed on disk since it was recorded (not scanned yet): no.
    write_all(&a.fs, "top.txt", b"changed, not scanned").unwrap();
    assert_eq!(a.core.holds(b.core.me(), &q, &|_| false), [false]);
    // The folder's marker is gone (moved to the Trash, a disk unmounted): no.
    let mut c = full_with_files();
    c.core.scan("").unwrap();
    let rec = c.core.store().local("docs/b.txt").unwrap().unwrap().file.info;
    let q = [HeldFile { path: rec.path.clone(), version: rec.version.clone(), content: rec.content_digest() }];
    assert_eq!(c.core.holds(b.core.me(), &q, &|_| false), [true]);
    // Ignored here now: not this device's to keep (it may go any time, unsynced): no.
    c.core.set_ignore(Ignore::parse_all(&["docs/b.txt"]).0);
    assert_eq!(c.core.holds(b.core.me(), &q, &|_| false), [false]);
    c.core.set_ignore(Ignore::defaults());
    assert_eq!(c.core.holds(b.core.me(), &q, &|_| false), [true]);
    c.fs.remove_dir(".seedwell").unwrap();
    assert_eq!(c.core.holds(b.core.me(), &q, &|_| false), [false]);
}

#[test]
fn a_device_freeing_a_file_says_it_does_not_hold_it_until_it_is_done() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let (mut b, mut c) = (dev("B", 2, &rw(), true), dev("C", 3, &rw(), true));
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    for d in [&mut b, &mut c] {
        d.core.request("top.txt").unwrap();
        pull(d, &[&a]);
    }
    let (lo, hi) = if b.core.me().0 < c.core.me().0 { (&b, &c) } else { (&c, &b) };
    let rec = hi.core.store().local("top.txt").unwrap().unwrap().file.info;
    let q = [HeldFile { path: rec.path.clone(), version: rec.version.clone(), content: rec.content_digest() }];
    assert_eq!(hi.core.holds(lo.core.me(), &q, &|_| false), [true]);
    // The higher one is freeing it (it asked): until it is done, it holds nothing for anyone.
    assert_eq!(hi.core.holds(lo.core.me(), &q, &|p| p == "top.txt"), [false]);
}

#[test]
fn a_rename_of_a_file_changed_meanwhile_on_the_holder_is_refused_there() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    assert!(matches!(b.core.host_rename("top.txt", "moved.txt"), Err(HostError::NeedsHolder)));
    let seen = b.core.rename_seen("top.txt").unwrap();
    // A changed it on its disk, not recorded yet: it does not hold what it recorded, so it renames nothing.
    write_all(&a.fs, "top.txt", b"A's edit").unwrap();
    assert!(matches!(a.core.rename_for_peer("top.txt", "moved.txt", &seen), Err(HostError::NeedsHolder)));
    // Recorded: not the version B saw.
    a.core.scan("").unwrap();
    assert!(matches!(a.core.rename_for_peer("top.txt", "moved.txt", &seen), Err(HostError::Changed(_))));
    // A deleted it: nothing to rename.
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    assert!(matches!(a.core.rename_for_peer("top.txt", "moved.txt", &seen), Err(HostError::NotFound(_))));
    assert!(a.fs.lstat("moved.txt").unwrap().is_none() && a.core.store().local("moved.txt").unwrap().is_none());
    // A read-only member records nothing of its own.
    let mut r = dev("R", 4, &rw().read_only().unwrap(), false);
    sync(&mut a, &mut r);
    assert!(matches!(r.core.rename_for_peer("docs", "papers", &b.core.rename_seen("docs").unwrap()), Err(HostError::ReadOnly)));
    sync(&mut a, &mut b);
    assert!(b.core.item_at("moved.txt").unwrap().is_none());
    assert!(a.core.unavailable().unwrap().is_empty() && b.core.unavailable().unwrap().is_empty());
}
#[test]
fn the_host_hears_of_each_change_of_an_items_availability() {
    // A (full) holds top.txt, C (full) too; B has it online only. C leaves the folder: A still has it. A leaves too:
    // no device has it now, so it is unavailable (a report: nothing is deleted), until a device announces it held.
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut c = dev("C", 3, &rw(), false);
    sync(&mut a, &mut c);
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    sync(&mut c, &mut b);
    let anchor = b.core.anchor().unwrap();
    b.core.forget_peer(c.core.me()).unwrap();
    assert_eq!(item(&mut b, "top.txt").state, ItemState::OnlineOnly);
    b.core.forget_peer(a.core.me()).unwrap();
    assert_eq!(item(&mut b, "top.txt").state, ItemState::Unavailable);
    assert!(b.core.unavailable().unwrap().contains(&("top.txt".to_string(), 10)));
    assert_eq!(b.core.need_totals().unwrap(), (0, 0), "counted apart, not needed");
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "top.txt" && i.state == ItemState::Unavailable), "{:?}", ch.items);
    // C comes back and announces it held: fetchable, and the host hears of that too.
    let anchor = ch.anchor;
    send(&c, &mut b);
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "top.txt" && i.state == ItemState::OnlineOnly), "{:?}", ch.items);
    assert!(b.core.unavailable().unwrap().iter().all(|(p, _)| p != "top.txt"));
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&c]);
    assert_eq!(read_all(&b.fs, "top.txt").unwrap(), b"at the top");
}
#[test]
fn a_rename_made_for_another_device_met_by_one_with_an_old_version_deletes_nothing() {
    // A edits top.txt; B (on demand) hears of it, then renames top.txt (online only there): A makes the rename. E
    // slept through A's edit and meets the new name before the old one's delete.
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    let mut e = dev("E", 5, &rw(), false);
    sync(&mut a, &mut b);
    sync(&mut a, &mut e);
    write_all(&a.fs, "top.txt", b"K1, newer").unwrap();
    a.core.scan("").unwrap();
    sync(&mut a, &mut b);
    rename_by(&mut b, "top.txt", "moved.txt", &mut a);
    let (index_id, _) = a.core.index_state().unwrap();
    let all = a.core.announce_since(0, usize::MAX).unwrap();
    let (first, later): (Vec<_>, Vec<_>) = all.into_iter().partition(|x| x.file.info.path != "top.txt");
    learn_writers(&mut e.core, &a.core.writers()).unwrap();
    e.core.apply_remote(a.core.me(), RemoteIndex { index_id, full: false, done: false, entries: first }).unwrap();
    assert!(e.core.store().need("moved.txt").unwrap().is_some(), "E needs the new name");
    pull(&mut e, &[&a]);
    e.core.apply_remote(a.core.me(), RemoteIndex { index_id, full: false, done: false, entries: later }).unwrap();
    for _ in 0..3 {
        sync(&mut a, &mut b);
        sync(&mut a, &mut e);
        sync(&mut b, &mut e);
    }
    for d in [&a, &e] {
        assert_eq!(read_all(&d.fs, "moved.txt").unwrap(), b"K1, newer", "{}", d.name);
        assert!(d.fs.lstat("top.txt").unwrap().is_none(), "{}", d.name);
    }
    assert_eq!(item(&mut b, "moved.txt").state, ItemState::OnlineOnly);
}
#[test]
fn a_rename_onto_a_name_made_meanwhile_ends_in_a_conflict_copy() {
    // Review 8, #1: B (on demand) renames old.txt, which it has online only, to new.txt, while A makes a new.txt of
    // its own. A, which has new.txt now, refuses; C (full, not heard of A's new.txt) makes the rename. Both versions of
    // new.txt are held by the devices that made them: the loser becomes a conflict copy, nothing is lost.
    let mut a = dev("A", 1, &rw(), false);
    write_all(&a.fs, "old.txt", b"the old bytes").unwrap();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    let mut c = dev("C", 3, &rw(), false);
    sync(&mut a, &mut b);
    sync(&mut a, &mut c);
    sync(&mut b, &mut c);
    write_all(&a.fs, "new.txt", b"A's new file").unwrap();
    a.core.scan("").unwrap();
    assert!(matches!(b.core.host_rename("old.txt", "new.txt"), Err(HostError::NeedsHolder)));
    let seen = b.core.rename_seen("old.txt").unwrap();
    assert!(matches!(a.core.rename_for_peer("old.txt", "new.txt", &seen), Err(HostError::Exists(_))));
    rename_by(&mut b, "old.txt", "new.txt", &mut c);
    for _ in 0..3 {
        sync(&mut a, &mut b);
        sync(&mut a, &mut c);
        sync(&mut b, &mut c);
    }
    for d in [&a, &c] {
        let mut contents: Vec<Vec<u8>> = d
            .fs
            .read_dir("")
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.name.starts_with("new"))
            .map(|e| read_all(&d.fs, &e.name).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, [b"A's new file".to_vec(), b"the old bytes".to_vec()], "{}: both, one as a conflict copy", d.name);
        assert!(d.fs.lstat("old.txt").unwrap().is_none(), "{}", d.name);
    }
    assert_eq!(a.core.conflicts().unwrap().len(), 1);
    for d in [&a, &b, &c] {
        assert!(d.core.unavailable().unwrap().is_empty(), "{}", d.name);
    }
}

#[test]
fn a_full_device_that_meets_a_rename_and_an_edit_of_the_old_name_together_keeps_both() {
    // Review 8, #2: B (on demand) renames top.txt (online only there): A makes it. Meanwhile C edits top.txt, smaller.
    // F (full) learns of both in one round: whatever order it takes them in, the renamed bytes come from A, which
    // holds them, and the edit from C. Nothing is unavailable anywhere.
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    let mut c = dev("C", 3, &rw(), false);
    let mut f = dev("F", 6, &rw(), false);
    for d in [&mut b, &mut c, &mut f] {
        sync(&mut a, d);
    }
    rename_by(&mut b, "top.txt", "moved.txt", &mut a);
    write_all(&c.fs, "top.txt", b"C's").unwrap();
    c.core.scan("").unwrap();
    send(&a, &mut f);
    send(&c, &mut f);
    let p = pull(&mut f, &[&a, &c]);
    assert!(p.failed.is_empty(), "{p:?}");
    assert_eq!(read_all(&f.fs, "moved.txt").unwrap(), b"at the top");
    assert_eq!(read_all(&f.fs, "top.txt").unwrap(), b"C's", "the edit survives the rename's delete");
    for _ in 0..3 {
        sync(&mut a, &mut b);
        sync(&mut a, &mut c);
        sync(&mut a, &mut f);
        sync(&mut c, &mut f);
        sync(&mut b, &mut c);
    }
    for d in [&a, &c, &f] {
        assert_eq!(read_all(&d.fs, "moved.txt").unwrap(), b"at the top", "{}", d.name);
        assert_eq!(read_all(&d.fs, "top.txt").unwrap(), b"C's", "{}", d.name);
    }
    for d in [&a, &b, &c, &f] {
        assert!(d.core.unavailable().unwrap().is_empty(), "{}", d.name);
    }
}

#[test]
fn a_rename_by_a_device_that_missed_a_delete_is_refused_and_loses_nothing() {
    // Review 8, the offline variant: B (on demand) was away while A deleted top.txt. Back, before hearing of it, B
    // renames top.txt: A has nothing there to rename, and B has no bytes to record a version of.
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    assert!(matches!(b.core.host_rename("top.txt", "moved.txt"), Err(HostError::NeedsHolder)));
    let seen = b.core.rename_seen("top.txt").unwrap();
    assert!(matches!(a.core.rename_for_peer("top.txt", "moved.txt", &seen), Err(HostError::NotFound(_))));
    // No holder at all (A gone): B's rename has no device to go to.
    b.core.forget_peer(a.core.me()).unwrap();
    assert!(b.core.rename_holders("top.txt").unwrap().is_empty());
    assert!(matches!(b.core.host_rename("top.txt", "moved.txt"), Err(HostError::NeedsHolder)));
    assert!(b.core.item_at("moved.txt").unwrap().is_none());
}
#[test]
fn content_at_an_online_only_path_is_archived_unless_it_is_the_records() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    // The record's own content (a fetch cut off before it was recorded): held.
    write_all(&b.fs, "top.txt", b"at the top").unwrap();
    b.core.scan("").unwrap();
    assert_eq!(item(&mut b, "top.txt").state, ItemState::Downloaded);
    // Anything else, put there behind the host's back: archived, never a version.
    write_all(&b.fs, "docs/b.txt", b"stray").unwrap();
    let r = b.core.scan("").unwrap();
    assert_eq!(r.changed, 0, "{r:?}");
    assert_eq!(item(&mut b, "docs/b.txt").state, ItemState::OnlineOnly);
    assert!(b.fs.lstat("docs/b.txt").unwrap().is_none());
    assert!(b.core.announce_since(0, usize::MAX).unwrap().iter().all(|e| e.file.info.path != "docs/b.txt" || e.file.info.modified_by != b.core.me()));
    // The host's own write there is a version, as before.
    write_all(&b.fs, "docs/b.txt", b"the user's").unwrap();
    b.core.host_written("docs/b.txt").unwrap();
    assert_eq!(item(&mut b, "docs/b.txt").state, ItemState::ModifiedHere);
}

#[test]
fn a_full_folder_keeps_no_feed_rows_and_a_switched_one_is_readable_by_0_1_11() {
    let dir = tempfile::tempdir().unwrap();
    // Full: a path made anew elsewhere and taken in one go notes no retired file id.
    let db = dir.path().join("a.redb");
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let mut a = Dev { core: core_on(&fs, 1, &rw(), false, false, Box::new(trs_sync::store_redb::RedbStore::open(&db, 1).unwrap())), fs, name: "A" };
    let mut z = dev("Z", 5, &rw(), false);
    write_all(&z.fs, "f.txt", b"one").unwrap();
    z.core.scan("").unwrap();
    sync(&mut z, &mut a);
    z.fs.remove_file("f.txt").unwrap();
    z.core.scan("").unwrap();
    write_all(&z.fs, "f.txt", b"two").unwrap();
    z.core.scan("").unwrap();
    send(&z, &mut a);
    pull(&mut a, &[&z]);
    assert_eq!(read_all(&a.fs, "f.txt").unwrap(), b"two");
    assert!(a.core.store().retired(0, u64::MAX).unwrap().is_empty());

    // On demand, then full again: every record is written as 0.1.11 reads it.
    let mut src = full_with_files();
    src.core.scan("").unwrap();
    let db = dir.path().join("b.redb");
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let mut b = Dev { core: core_on(&fs, 2, &rw(), true, false, Box::new(trs_sync::store_redb::RedbStore::open(&db, 2).unwrap())), fs, name: "B" };
    sync(&mut src, &mut b);
    b.core.request("docs/a.txt").unwrap();
    pull(&mut b, &[&src]);
    b.core.set_on_demand(false).unwrap();
    let Dev { fs, core, .. } = b;
    drop(core);
    let store = trs_sync::store_redb::RedbStore::open(&db, 2).unwrap();
    let rows = trs_sync::store_redb::raw_local_rows(&store).unwrap();
    assert!(!rows.is_empty());
    for (p, bytes) in &rows {
        assert!(trs_sync::store_redb::in_released_format(bytes), "{p} is not in the released format");
    }
    // And it fetches what it had online only, as a full folder.
    let mut b = Dev { core: core_on(&fs, 2, &rw(), false, false, Box::new(store)), fs, name: "B" };
    send(&src, &mut b);
    pull(&mut b, &[&src]);
    assert_eq!(read_all(&b.fs, "media/big.bin").unwrap().len(), 300_000);
}

#[test]
fn a_full_folders_index_never_has_the_on_demand_tables() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let db = dir.path().join("f.redb");
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let mut f = Dev { core: core_on(&fs, 3, &rw(), false, false, Box::new(trs_sync::store_redb::RedbStore::open(&db, 3).unwrap())), fs, name: "F" };
    sync(&mut a, &mut f);
    write_all(&f.fs, "docs/b.txt", b"changed on F").unwrap();
    f.core.scan("").unwrap();
    sync(&mut a, &mut f);
    drop(f);
    let store = trs_sync::store_redb::RedbStore::open(&db, 3).unwrap();
    let names = trs_sync::store_redb::table_names(&store).unwrap();
    assert!(names.contains(&"local".to_string()), "{names:?}");
    for t in ["shown", "shown_at", "retired", "unavailable"] {
        assert!(!names.contains(&t.to_string()), "{t} in {names:?}");
    }
}

#[test]
fn an_anchor_from_before_the_kept_feed_has_expired() {
    use trs_sync::index::{IndexStore, Op};
    let mut store = MemStore::new(7);
    store.set_feeds_host(true);
    store.apply(vec![Op::SetMeta(trs_sync::index::Meta { index_id: 7, sequence: 100 }), Op::PruneRetired(50)]).unwrap();
    let mut items = trs_sync::ondemand::Items::default();
    let pins = Default::default();
    let old = trs_sync::ondemand::anchor_of(7, 40);
    assert_eq!(items.changes(&store, &pins, old, 10).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    assert!(items.changes(&store, &pins, trs_sync::ondemand::anchor_of(7, 60), 10).is_ok());
    assert!(items.changes(&store, &pins, 0, 10).is_ok(), "from the start");
}


#[test]
fn a_folder_the_host_makes_records_only_itself() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.fs.create_dir("nd").unwrap();
    write_all(&b.fs, "nd/stray.txt", b"stray").unwrap();
    b.core.host_made_dir("nd").unwrap();
    assert_eq!(item(&mut b, "nd").kind, FileKind::Dir);
    assert!(b.core.item_at("nd/stray.txt").unwrap().is_none(), "not taken as the host's");
}

#[test]
fn a_file_asked_for_and_deleted_elsewhere_before_it_came_goes_here_too() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("top.txt").unwrap();
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[]);
    assert!(b.core.item_at("top.txt").unwrap().is_none());
    assert_eq!(b.core.need_totals().unwrap(), (0, 0));
}

#[test]
fn a_users_file_named_like_the_old_partial_copy_mark_syncs_like_any() {
    let mut a = full_with_files();
    write_all(&a.fs, ".seedwell-partial-copy", b"a user's file of that name").unwrap();
    a.core.scan("").unwrap();
    let mut f = dev("F", 6, &rw(), false);
    sync(&mut a, &mut f);
    assert_eq!(read_all(&f.fs, ".seedwell-partial-copy").unwrap(), b"a user's file of that name");
}

#[test]
fn the_holders_of_a_rename_are_the_devices_with_every_file_full_ones_first() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    // C's id sorts below A's, so the order is the rule's, not the ids'.
    let mut c = (3u8..).map(|s| dev("C", s, &rw(), true)).find(|c| c.core.me() < a.core.me()).unwrap();
    let mut d = dev("D", 100, &rw(), true);
    sync(&mut a, &mut c);
    sync(&mut a, &mut d);
    c.core.request("docs").unwrap();
    pull(&mut c, &[&a]);
    d.core.request("docs/a.txt").unwrap();
    pull(&mut d, &[&a]);
    sync(&mut a, &mut b);
    sync(&mut c, &mut b);
    sync(&mut d, &mut b);
    // A and C hold everything below docs; D only one file. The full device first.
    assert_eq!(b.core.rename_holders("docs").unwrap(), [a.core.me(), c.core.me()]);
    let mut on_demand = vec![c.core.me(), d.core.me()];
    on_demand.sort();
    assert_eq!(b.core.rename_holders("docs/a.txt").unwrap(), [vec![a.core.me()], on_demand].concat());
}

#[test]
fn the_files_only_a_device_holds_are_counted_for_its_removal_question() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let only = |d: &Dev| trs_sync::ondemand::files_only_here(d.core.store(), &Default::default()).unwrap();
    assert_eq!(only(&a), 5, "no other device yet");
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    // B has them online only: A is still the only one with each.
    assert_eq!((only(&a), only(&b)), (5, 0));
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);
    assert_eq!((only(&a), only(&b)), (2, 0), "B holds the three below docs now");
    // A file B made: only B has it, until A (full) takes it.
    write_all(&b.fs, "mine.txt", b"made on B").unwrap();
    b.core.host_written("mine.txt").unwrap();
    assert_eq!(only(&b), 1);
    sync(&mut a, &mut b);
    assert_eq!((only(&a), only(&b)), (2, 0));
    // A version held elsewhere only at an older version does not count as held there.
    write_all(&a.fs, "docs/a.txt", b"A's newer").unwrap();
    a.core.scan("").unwrap();
    assert_eq!(only(&a), 3);
}

#[test]
fn a_device_no_longer_a_member_is_no_source_or_holder_and_which_version_is_current_never_changes() {
    // A and C (full) hold everything; B is on demand. R's edit of top.txt beat A's concurrent delete; F (full) has
    // heard of both and not fetched R's edit yet.
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut c = dev("C", 3, &rw(), false);
    let mut b = dev("B", 2, &rw(), true);
    let mut r = dev("R", 4, &rw(), false);
    let mut f = dev("F", 6, &rw(), false);
    for d in [&mut c, &mut b, &mut r, &mut f] {
        sync(&mut a, d);
    }
    sync(&mut c, &mut b);
    write_all(&r.fs, "top.txt", b"R's edit").unwrap();
    r.core.scan("").unwrap();
    a.fs.remove_file("top.txt").unwrap();
    a.core.scan("").unwrap();
    send(&r, &mut f);
    send(&a, &mut f);
    let need = f.core.store().need("top.txt").unwrap().unwrap();
    assert!(!need.file.info.deleted, "R's edit beats the delete: {need:?}");
    let members = |ds: &[&Dev]| ds.iter().map(|d| d.core.me()).collect::<std::collections::BTreeSet<_>>();
    let mut full = vec![a.core.me(), c.core.me()];
    full.sort();
    assert_eq!(b.core.rename_holders("docs").unwrap(), full);
    let records = |d: &Dev| d.core.store().local_paths("", None, usize::MAX).unwrap().into_iter().map(|p| d.core.store().local(&p).unwrap()).collect::<Vec<_>>();
    let before = records(&f);

    // C is removed: no holder; A, a member that is merely offline, still is.
    let now = members(&[&a, &b, &f, &r]);
    assert_eq!(b.core.set_members(&now).unwrap(), [c.core.me()]);
    assert_eq!(b.core.rename_holders("docs").unwrap(), [a.core.me()], "C is no holder");
    let rec = b.core.store().local("docs/a.txt").unwrap().unwrap().file.info;
    let q = HeldFile { content: rec.content_digest(), version: rec.version.clone(), path: rec.path.clone() };
    assert_eq!(b.core.holders_of(&q).unwrap(), [a.core.me()], "only A is asked");
    assert_eq!(trs_sync::ondemand::files_only_here(a.core.store(), b.core.not_members()).unwrap(), 0, "B and the rest");

    // R is removed too: on F, R's edit is still the current version (needed, from nobody now: unavailable), and
    // top.txt stays on F as it was. Nothing on F changes.
    let now = members(&[&a, &b, &f]);
    assert_eq!(f.core.set_members(&now).unwrap(), [r.core.me()]);
    assert_eq!(f.core.store().need("top.txt").unwrap().map(|n| n.file.info.version), Some(need.file.info.version), "R's edit is still current");
    // (A full folder reads no records for it: its next pull reports what only they hold.)
    assert_eq!(pull(&mut f, &[&a, &c]).done, 0, "nothing to do");
    assert_eq!(f.core.unavailable().unwrap(), [("top.txt".to_string(), 8)]);
    assert_eq!(records(&f), before, "no record changed");
    assert_eq!(read_all(&f.fs, "top.txt").unwrap(), b"at the top", "never deleted");
    // R let back in: its edit can be fetched again.
    assert!(f.core.set_members(&members(&[&a, &b, &f, &r])).unwrap().is_empty());
    assert!(f.core.unavailable().unwrap().is_empty());
    pull(&mut f, &[&r]);
    assert_eq!(read_all(&f.fs, "top.txt").unwrap(), b"R's edit");
}

#[test]
fn a_rename_into_a_folder_named_in_another_case_records_the_folder_as_it_is_named() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev_on("B", 2, &rw(), true, true);
    sync(&mut a, &mut b);
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&a]);
    assert_eq!(b.core.rename_target("DOCS/Old/t.txt").unwrap(), "docs/old/t.txt");
    assert_eq!(b.core.host_rename("top.txt", "DOCS/Old/t.txt").unwrap(), 1);
    assert!(b.core.item_at("docs/old/t.txt").unwrap().is_some());
    assert!(b.core.store().local("DOCS").unwrap().is_none() && b.core.store().local("DOCS/Old/t.txt").unwrap().is_none(), "no second spelling");
    // A folder only on disk (not recorded yet) is found there.
    b.fs.create_dir("Made").unwrap();
    assert_eq!(b.core.rename_target("made/x.txt").unwrap(), "Made/x.txt");
    // A disk that keeps case: as asked.
    assert_eq!(a.core.rename_target("DOCS/x.txt").unwrap(), "DOCS/x.txt");
}

#[test]
fn a_file_replaced_by_another_is_checked_whole_first_and_a_renamed_conflict_copy_is_an_ordinary_file() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("docs").unwrap();
    pull(&mut b, &[&a]);
    // b.txt changed on disk since it was recorded: refused, and a.txt (the one to go) is untouched.
    write_all(&b.fs, "docs/b.txt", b"changed behind the record").unwrap();
    assert!(matches!(b.core.host_replace("docs/b.txt", "docs/a.txt"), Err(HostError::NeedsHolder)));
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"alpha alpha alpha");
    assert!(b.core.item_at("docs/a.txt").unwrap().is_some());
    assert!(b.fs.lstat(".seedwell/Archive").unwrap().is_none(), "nothing archived");
    // Recorded: b.txt takes a.txt's place; a.txt's content is in the archive.
    b.core.host_written("docs/b.txt").unwrap();
    assert_eq!(b.core.host_replace("docs/b.txt", "docs/a.txt").unwrap(), 1);
    assert_eq!(read_all(&b.fs, "docs/a.txt").unwrap(), b"changed behind the record");
    assert!(b.core.item_at("docs/b.txt").unwrap().is_none());
    assert!(b.fs.read_dir(".seedwell/Archive/docs").unwrap().entries.iter().any(|e| e.name.starts_with("a~")));
    sync(&mut a, &mut b);
    assert_eq!(read_all(&a.fs, "docs/a.txt").unwrap(), b"changed behind the record");

    // A conflict copy moved with its folder is still one (of the moved original); renamed to any other name it is an
    // ordinary file, on every device, so resolving can never act on it.
    write_all(&b.fs, "docs/old/c.txt", b"charlie from B").unwrap();
    b.core.host_written("docs/old/c.txt").unwrap();
    write_all(&a.fs, "docs/old/c.txt", b"charlie from A, later").unwrap();
    for _ in 0..5 {
        a.core.scan("").unwrap();
    }
    sync(&mut a, &mut b);
    assert_eq!(a.core.conflicts().unwrap().len(), 1);
    rename_any(&mut b, "docs", "papers", &mut a);
    sync(&mut a, &mut b);
    let c = a.core.conflicts().unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].1, "papers/old/c.txt");
    assert_eq!(b.core.conflicts().unwrap(), c);
    rename_any(&mut b, &c[0].0, "papers/old/kept.txt", &mut a);
    sync(&mut a, &mut b);
    assert!(a.core.conflicts().unwrap().is_empty() && b.core.conflicts().unwrap().is_empty());
}

/// [`rename_by`], or made on `d` when it holds every file there.
fn rename_any(d: &mut Dev, from: &str, to: &str, holder: &mut Dev) {
    match d.core.host_rename(from, to) {
        Err(HostError::NeedsHolder) => {
            let seen = d.core.rename_seen(from).unwrap();
            holder.core.rename_for_peer(from, to, &seen).unwrap();
        }
        r => {
            r.unwrap();
        }
    }
}

#[test]
fn a_file_whose_other_copy_is_on_a_removed_device_is_only_here() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    b.core.request("top.txt").unwrap();
    pull(&mut b, &[&a]);
    sync(&mut a, &mut b);
    assert_eq!(item(&mut b, "top.txt").state, ItemState::Downloaded);
    b.core.set_members(&[b.core.me()].into_iter().collect()).unwrap();
    assert_eq!(item(&mut b, "top.txt").state, ItemState::ModifiedHere, "A's copy is no copy now");
}

/// A folder whose item was deleted, or moved out of it, is reported changed too (what is in it changed: the host's
/// version of it moves), not only the item.
#[test]
fn the_folder_above_a_deleted_or_moved_item_is_reported() {
    let mut a = full_with_files();
    a.core.scan("").unwrap();
    let mut b = dev("B", 2, &rw(), true);
    sync(&mut a, &mut b);
    let anchor = b.core.anchor().unwrap();
    a.fs.remove_file("docs/b.txt").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    let ch = b.core.changes(anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "docs"), "{:?}", names(&ch.items));
    // Moved out of docs/old into media: docs/old is reported (and media, with the item).
    let anchor = b.core.anchor().unwrap();
    a.fs.rename("docs/old/c.txt", "media/c.txt").unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &[&a]);
    let ch = b.core.changes(anchor, 1000).unwrap();
    let paths: Vec<&str> = ch.items.iter().map(|i| i.path.as_str()).collect();
    assert!(paths.contains(&"docs/old") && paths.contains(&"media/c.txt"), "{paths:?}");
}
