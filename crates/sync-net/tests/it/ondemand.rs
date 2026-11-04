//! On-demand folders over the network (SPEC §7G.11): a member that lists everything, fetches what is asked for, keeps
//! what is pinned, lets go of what is evicted, and whose host's changes reach the other device. Temp folders and
//! loopback only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync::ondemand::{ItemState, ROOT_ID};
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::status::FolderState;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

struct Device {
    h: SyncHandle,
    _state: tempfile::TempDir,
}

async fn device(name: &str) -> Device {
    device_with(name, trs_sync_proto::msg::FEATURES).await
}

/// The feature bits of Seedwell 0.1.11 (it knows none of the on-demand messages, and ignores a Join for a folder it
/// shares already).
const FEATURES_0_1_11: u64 = trs_sync_proto::msg::FEATURE_CHAT | trs_sync_proto::msg::FEATURE_DIRECT;

/// [`device`] offering `features` (an earlier build's, to play one).
async fn device_with(name: &str, features: u64) -> Device {
    let state = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    opts.features = features;
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

fn addr(d: &Device) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), d.h.port())
}

async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The value of a future that is ready at once (a call that does not wait on anything).
fn futures_now<T>(f: impl std::future::Future<Output = Result<T, trs_sync_net::SyncError>>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f)).unwrap()
}

/// Copies `from` into a new `to` as a move to another volume does (each file's time kept).
fn copy_as_a_move_does(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type().unwrap().is_dir() {
            copy_as_a_move_does(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
            let t = std::fs::metadata(&src).unwrap().modified().unwrap();
            std::fs::File::options().write(true).open(&dst).unwrap().set_modified(t).unwrap();
        }
    }
}

/// Marks `to` as a partial copy, as a move to another volume does while it copies.
fn mark(to: &std::path::Path) {
    std::fs::create_dir_all(to.join(".seedwell")).unwrap();
    std::fs::write(to.join(".seedwell/partial-copy"), b"").unwrap();
}

fn read(p: &std::path::Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

/// A full device sharing a small tree, and an on-demand device that joined it.
async fn pair() -> (Device, Device, tempfile::TempDir, tempfile::TempDir, FolderId) {
    pair_as(Role::ReadWrite).await
}

/// [`pair`], the on-demand device with `role`.
async fn pair_as(role: Role) -> (Device, Device, tempfile::TempDir, tempfile::TempDir, FolderId) {
    pair_with(role, trs_sync_proto::msg::FEATURES).await
}

/// [`pair_as`], the full device offering `features`.
async fn pair_with(role: Role, features: u64) -> (Device, Device, tempfile::TempDir, tempfile::TempDir, FolderId) {
    let (a, b) = (device_with("Alpha", features).await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::create_dir_all(fa.path().join("docs/old")).unwrap();
    std::fs::write(fa.path().join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(fa.path().join("docs/old/c.txt"), b"charlie").unwrap();
    std::fs::write(fa.path().join("big.bin"), vec![5u8; 2_000_000]).unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.h.create_invite(&id, role, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Shared")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B lists A's files", || b.h.od_item_at(&id, "docs/old/c.txt").ok().flatten().is_some() && b.h.od_item_at(&id, "big.bin").ok().flatten().is_some()).await;
    (a, b, fa, fb, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_on_demand_member_lists_fetches_keeps_and_lets_go() {
    let (a, b, fa, fb, id) = pair().await;
    // Everything listed, nothing downloaded, and the host's place is not touched.
    let root = b.h.od_children(&id, ROOT_ID, None, 100).unwrap().unwrap();
    let names: Vec<&str> = root.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["big.bin", "docs"]);
    assert!(root.items.iter().all(|i| i.state == ItemState::OnlineOnly || i.kind == trs_sync_proto::model::FileKind::Dir));
    assert!(!fb.path().join("Shared").exists(), "the place is the host's");
    assert!(!b.h.od_cache_path(&id, "big.bin").unwrap().exists());
    let st = &b.h.snapshot().folders[0];
    assert_eq!((st.path.as_str(), st.bytes), (fb.path().join("Shared").to_str().unwrap(), 0));

    // A fetch brings exactly the file asked for.
    let at = b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap().path;
    assert_eq!(read(&at).as_deref(), Some(&b"alpha"[..]));
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::Downloaded);
    assert!(!b.h.od_cache_path(&id, "big.bin").unwrap().exists());
    let p = b.h.od_progress(&id, "docs/a.txt").unwrap();
    assert!(p.here && p.done == 5);

    // Pinned: a folder's files come, and so does one added there later.
    b.h.od_pin(&id, "docs", true).await.unwrap();
    until(30, "the pinned folder is here", || b.h.od_cache_path(&id, "docs/old/c.txt").is_ok_and(|p| p.exists())).await;
    std::fs::write(fa.path().join("docs/new.txt"), b"new").unwrap();
    until(30, "a new file below the pin comes", || b.h.od_cache_path(&id, "docs/new.txt").is_ok_and(|p| read(&p).as_deref() == Some(b"new"))).await;

    // Evicted: the content goes, the item stays; A no longer counts B as holding it.
    b.h.od_pin(&id, "docs", false).await.unwrap();
    let e = b.h.od_evict(&id, "docs").await.unwrap();
    assert_eq!(e.files, 3, "{e:?}");
    assert!(!b.h.od_cache_path(&id, "docs/a.txt").unwrap().exists());
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::OnlineOnly);

    // The host's changes reach A: a new folder and file, a rename of an online-only file (made by A, which has it), a
    // delete.
    let anchor = b.h.od_anchor(&id).unwrap();
    let big = b.h.od_item_at(&id, "big.bin").unwrap().unwrap().id;
    let old: Vec<String> = ["docs/old", "docs/old/c.txt"].iter().map(|p| b.h.od_item_at(&id, p).unwrap().unwrap().id).collect();
    let src = fb.path().join("from-host.txt");
    std::fs::write(&src, b"written by the host").unwrap();
    b.h.od_write(&id, "made/here.txt", &src, None).await.unwrap();
    b.h.od_rename(&id, "big.bin", "media/big.bin").await.unwrap();
    // The call returns once the renamed item is here, under its identifier.
    let moved = b.h.od_item_at(&id, "media/big.bin").unwrap().unwrap();
    assert_eq!(moved.id, big);
    assert!(b.h.od_item_at(&id, "big.bin").unwrap().is_none());
    b.h.od_delete(&id, "docs/old").await.unwrap();
    until(30, "A has B's file", || read(&fa.path().join("made/here.txt")).as_deref() == Some(&b"written by the host"[..])).await;
    until(30, "A took the rename", || read(&fa.path().join("media/big.bin")).map(|v| v.len()) == Some(2_000_000) && !fa.path().join("big.bin").exists()).await;
    until(30, "A deleted docs/old", || !fa.path().join("docs/old").exists()).await;
    let ch = b.h.od_changes(&id, anchor, 1000).unwrap();
    assert!(ch.items.iter().any(|i| i.path == "made/here.txt") && ch.items.iter().any(|i| i.path == "media/big.bin"));
    assert!(old.iter().all(|o| ch.deleted.contains(o)), "docs/old and docs/old/c.txt: {:?}", ch.deleted);
    assert!(!ch.deleted.contains(&big), "the renamed item lives on: {:?}", ch.deleted);

    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_switches_to_on_demand_and_back_without_losing_anything() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("one.txt"), b"one").unwrap();
    std::fs::write(fa.path().join("two.txt"), vec![2u8; 100_000]).unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let full = fb.path().join("Shared");
    b.h.join_folder(&link, full.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B has both files", || read(&full.join("two.txt")).map(|v| v.len()) == Some(100_000) && full.join("one.txt").exists()).await;

    // Full → on demand: the files move into the cache, downloaded; then one is let go.
    b.h.set_on_demand(&id, true, None).await.unwrap();
    assert!(!full.exists(), "the folder's files are the cache now");
    assert_eq!(b.h.od_item_at(&id, "one.txt").unwrap().unwrap().state, ItemState::Downloaded);
    // A holds the file: once the session that was linked when the folder closed has started anew, B may let it go.
    until(30, "B lets two.txt go", || futures_now(b.h.od_evict(&id, "two.txt")).files == 1).await;
    assert_eq!(b.h.od_item_at(&id, "two.txt").unwrap().unwrap().state, ItemState::OnlineOnly);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(read(&fa.path().join("two.txt")).map(|v| v.len()), Some(100_000), "A lost nothing");

    // On demand → full, at a new place: everything is there again.
    let back = fb.path().join("Back");
    b.h.set_on_demand(&id, false, Some(back.clone())).await.unwrap();
    until(30, "B has both files again", || read(&back.join("two.txt")).map(|v| v.len()) == Some(100_000) && read(&back.join("one.txt")).as_deref() == Some(b"one")).await;
    assert!(b.h.od_item_at(&id, "one.txt").is_err(), "not on demand any more");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_an_on_demand_folder_keeps_what_is_held_here() {
    let (a, b, _fa, fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    let src = fb.path().join("from-host.txt");
    std::fs::write(&src, b"only on B").unwrap();
    b.h.od_write(&id, "mine/only.txt", &src, None).await.unwrap();
    let cache = b.h.od_cache_path(&id, "").unwrap();
    // Something already at the place (the host's leftovers): the files go next to it.
    let place = fb.path().join("Shared");
    std::fs::create_dir_all(&place).unwrap();
    std::fs::write(place.join("leftover"), b"x").unwrap();

    let at = b.h.remove_folder(&id).await.unwrap();
    assert_eq!(at, fb.path().join("Shared 2"));
    assert_eq!(read(&at.join("mine/only.txt")).as_deref(), Some(&b"only on B"[..]), "the only copy is kept");
    assert_eq!(read(&at.join("docs/a.txt")).as_deref(), Some(&b"alpha"[..]));
    assert!(!at.join("big.bin").exists(), "online-only files were never here");
    assert!(!cache.exists());
    assert!(read(&place.join("leftover")).is_some());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_to_full_that_fails_changes_nothing() {
    let (a, b, _fa, fb, id) = pair().await;
    b.h.od_pin(&id, "docs/a.txt", true).await.unwrap();
    until(30, "the pinned file is here", || b.h.od_cache_path(&id, "docs/a.txt").is_ok_and(|p| p.exists())).await;
    // The new place is below a file: the move fails.
    std::fs::write(fb.path().join("a-file"), b"x").unwrap();
    assert!(b.h.set_on_demand(&id, false, Some(fb.path().join("a-file/Shared"))).await.is_err());
    // Still on demand, still pinned, and nothing else is fetched.
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().pinned);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(b.h.od_item_at(&id, "big.bin").unwrap().unwrap().state, ItemState::OnlineOnly);
    assert!(!b.h.od_cache_path(&id, "big.bin").unwrap().exists());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_change_leaves_the_cache_as_it_was_and_a_read_only_delete_frees() {
    let (a, b, _fa, fb, id) = pair_as(Role::ReadOnly).await;
    b.h.od_pin(&id, "docs", true).await.unwrap();
    until(30, "the pinned folder is here", || b.h.od_cache_path(&id, "docs/old/c.txt").is_ok_and(|p| p.exists())).await;
    let cached = b.h.od_cache_path(&id, "docs/a.txt").unwrap();
    let src = fb.path().join("edit.txt");
    std::fs::write(&src, b"not allowed").unwrap();
    assert!(b.h.od_write(&id, "docs/a.txt", &src, None).await.is_err());
    assert_eq!(read(&cached).as_deref(), Some(&b"alpha"[..]), "the cache keeps the received version");
    assert!(b.h.od_write(&id, "new/x.txt", &src, None).await.is_err());
    assert!(!b.h.od_cache_path(&id, "new").unwrap().exists(), "no folder made for a refused write");
    assert!(b.h.od_rename(&id, "nope.txt", "other/x.txt").await.is_err());
    assert!(b.h.od_item_at(&id, "other").unwrap().is_none());

    // Delete on a read-only member: the content is freed, the items stay, the pin is gone (and saved).
    let d = b.h.od_delete(&id, "docs").await.unwrap();
    assert!(matches!(&d, trs_sync::ondemand::Deleted::Freed(e) if e.files == 2), "{d:?}");
    assert!(!cached.exists());
    let i = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap();
    assert_eq!((i.state, i.pinned), (ItemState::OnlineOnly, false));
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_is_freed_only_while_a_device_that_has_it_is_connected() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.od_fetch(&id, "docs/old/c.txt", Duration::from_secs(30)).await.unwrap();
    // A has it and is connected, its index current: B may let go.
    until(30, "A is connected", || !b.h.snapshot().devices.is_empty()).await;
    let e = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!(e.files, 1, "{e:?}");
    // A is gone: what it announced holds nothing now.
    a.h.shutdown().await;
    until(30, "B saw A go", || b.h.snapshot().devices.is_empty()).await;
    let e = b.h.od_evict(&id, "docs/old/c.txt").await.unwrap();
    assert_eq!((e.files, e.kept), (0, 1));
    assert_eq!(e.kept_why[0].1, trs_sync::ondemand::NOT_HELD_ELSEWHERE);
    assert!(b.h.od_cache_path(&id, "docs/old/c.txt").unwrap().exists());
    b.h.shutdown().await;
}

/// A device that runs on `state` (to start one again on the same state, as after a crash).
async fn device_at(name: &str, state: &std::path::Path) -> SyncHandle {
    device_retrying(name, state, Duration::from_secs(3600)).await
}

/// [`device_at`], trying a folder held back by an unfinished move again every `every`.
async fn device_retrying(name: &str, state: &std::path::Path, every: Duration) -> SyncHandle {
    let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
    let mut opts = Options::new(state.to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    opts.held_retry = every;
    SyncHandle::start(opts).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_or_switch_cut_off_is_finished_at_the_next_start() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
    let journal = |id: &FolderId| state.path().join("sync").join(format!("{id}.journal"));
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let mut h = device_at("Beta", state.path()).await;
    let on_demand = async |h: &SyncHandle| -> (FolderId, std::path::PathBuf) {
        let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
        h.set_on_demand(&id, true, None).await.unwrap();
        (id, h.od_cache_path(&id, "").unwrap())
    };

    // A removal cut off after its journal was written, before anything moved: the files end at the place.
    let (id, cache) = on_demand(&h).await;
    h.shutdown().await;
    let j = format!(r#"{{"kind":"remove","from":{},"to":{},"place":{},"copied":false}}"#, text(&cache), text(&docs), text(&docs));
    std::fs::write(journal(&id), j).unwrap();
    h = device_at("Beta", state.path()).await;
    assert!(h.snapshot().folders.is_empty(), "the removal is finished");
    assert_eq!(read(&docs.join("a.txt")).as_deref(), Some(&b"held here"[..]), "the files are at the place");
    assert!(!cache.exists() && !journal(&id).exists());

    // A switch to full whose copy was whole, cut off before the original (the cache) went: finished, the copy kept,
    // the original gone, full at the place.
    let (id, cache) = on_demand(&h).await;
    h.shutdown().await;
    copy_as_a_move_does(&cache, &docs);
    let j = format!(r#"{{"kind":"switch","on":false,"from":{},"to":{},"place":{},"copied":true}}"#, text(&cache), text(&docs), text(&docs));
    std::fs::write(journal(&id), j).unwrap();
    h = device_at("Beta", state.path()).await;
    let f = h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap();
    assert!(!f.on_demand && f.path == docs.display().to_string(), "{f:?}");
    assert!(!cache.exists(), "the original went");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(read(&docs.join("a.txt")).as_deref(), Some(&b"held here"[..]), "the copy kept, nothing read as deleted");
    assert!(!journal(&id).exists());

    // A switch to on demand whose copy was cut off halfway: undone (the partial copy goes), full as it was.
    let full_id: FolderId = id;
    h.shutdown().await;
    let cache = state.path().join("sync").join(format!("{full_id}.cache"));
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("half"), b"partial").unwrap();
    mark(&cache);
    let j = format!(r#"{{"kind":"switch","on":true,"from":{},"to":{},"place":{},"copied":false}}"#, text(&docs), text(&cache), text(&docs));
    std::fs::write(journal(&full_id), j).unwrap();
    h = device_at("Beta", state.path()).await;
    let f = h.snapshot().folders.into_iter().find(|f| f.id == full_id.to_string()).unwrap();
    assert!(!f.on_demand, "{f:?}");
    assert!(!cache.exists(), "the partial copy went");
    assert_eq!(read(&docs.join("a.txt")).as_deref(), Some(&b"held here"[..]));

    // The same switch, moved on one volume and not saved: finished, on demand.
    h.shutdown().await;
    std::fs::rename(&docs, &cache).unwrap();
    std::fs::write(journal(&full_id), format!(r#"{{"kind":"switch","on":true,"from":{},"to":{},"place":{},"copied":false}}"#, text(&docs), text(&cache), text(&docs))).unwrap();
    h = device_at("Beta", state.path()).await;
    let f = h.snapshot().folders.into_iter().find(|f| f.id == full_id.to_string()).unwrap();
    assert!(f.on_demand, "{f:?}");
    assert_eq!(h.od_item_at(&full_id, "a.txt").unwrap().unwrap().state, ItemState::ModifiedHere, "held, not lost");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_or_switch_done_but_not_saved_never_touches_what_is_at_the_old_place_later() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (docs, moved) = (work.path().join("Docs"), work.path().join("Moved"));
    let journal = |id: &FolderId| state.path().join("sync").join(format!("{id}.journal"));
    // The configuration cannot be saved (something is in the way of its new copy).
    let in_the_way = state.path().join("sync.toml.tmp");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"the folder's").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    std::fs::create_dir_all(&in_the_way).unwrap();
    assert!(h.move_folder(&id, moved.clone()).await.is_err(), "not saved");
    assert_eq!(read(&moved.join("a.txt")).as_deref(), Some(&b"the folder's"[..]));
    assert!(std::fs::read_to_string(journal(&id)).unwrap().contains(r#""done":true"#), "the move is noted over");
    h.shutdown().await;
    // Something new at the old place, then a start: the move is saved, and the new folder there is the user's.
    std::fs::remove_dir(&in_the_way).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("user.txt"), b"made later").unwrap();
    let h = device_at("Beta", state.path()).await;
    let f = h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap();
    assert_eq!(f.path, moved.display().to_string());
    assert_eq!(read(&docs.join("user.txt")).as_deref(), Some(&b"made later"[..]), "never touched");
    assert!(!journal(&id).exists());

    // A switch to on demand, the same way.
    std::fs::create_dir_all(&in_the_way).unwrap();
    assert!(h.set_on_demand(&id, true, None).await.is_err(), "not saved");
    h.shutdown().await;
    std::fs::remove_dir(&in_the_way).unwrap();
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(moved.join("user.txt"), b"made later too").unwrap();
    let h = device_at("Beta", state.path()).await;
    let f = h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap();
    assert!(f.on_demand, "{f:?}");
    assert_eq!(read(&moved.join("user.txt")).as_deref(), Some(&b"made later too"[..]), "never touched");
    assert_eq!(read(&h.od_cache_path(&id, "a.txt").unwrap()).as_deref(), Some(&b"the folder's"[..]));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_place_never_overlaps_another_folder_and_an_old_cache_is_never_read_in() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (x, y) = (work.path().join("X"), work.path().join("Y"));
    for d in [&x, &y] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("f.txt"), b"f").unwrap();
    }
    let h = device_at("Beta", state.path()).await;
    let xid: FolderId = h.add_folder(x.clone()).await.unwrap().id.parse().unwrap();
    h.add_folder(y.clone()).await.unwrap();
    let e = h.move_folder(&xid, y.join("inside")).await.unwrap_err();
    assert!(e.to_string().contains("overlaps"), "{e}");
    h.set_on_demand(&xid, true, None).await.unwrap();
    let e = h.set_on_demand(&xid, false, Some(y.join("inside"))).await.unwrap_err();
    assert!(e.to_string().contains("overlaps"), "{e}");
    // Its own place is fine.
    h.set_on_demand(&xid, false, None).await.unwrap();
    assert_eq!(read(&x.join("f.txt")).as_deref(), Some(&b"f"[..]));
    h.shutdown().await;

    // Joining on demand where an earlier cache of the folder is: refused, saying where it is; nothing read in.
    let a = device("Alpha").await;
    let fa = tempfile::tempdir().unwrap();
    std::fs::write(fa.path().join("a.txt"), b"a").unwrap();
    let id: FolderId = a.h.add_folder(fa.path().to_path_buf()).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let b = device("Beta").await;
    let cache = b._state.path().join("sync").join(format!("{id}.cache"));
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("old.txt"), b"from before").unwrap();
    let e = b.h.join_folder_on_demand(&link, work.path().join("Shared")).await.unwrap_err();
    assert!(e.to_string().contains(&cache.display().to_string()), "{e}");
    assert!(b.h.snapshot().folders.is_empty());
    assert_eq!(read(&cache.join("old.txt")).as_deref(), Some(&b"from before"[..]));
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_on_demand_devices_freeing_at_once_keep_a_copy() {
    let (a, b, _fa, fb, id) = pair().await;
    let c = device("Gamma").await;
    let fc = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    c.h.join_folder_on_demand(&link, fc.path().join("Shared")).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    // B makes a file; C downloads it; then A (full, the third copy) goes away.
    let src = fb.path().join("x.txt");
    std::fs::write(&src, b"two copies").unwrap();
    b.h.od_write(&id, "x.txt", &src, None).await.unwrap();
    until(30, "C lists it", || c.h.od_item_at(&id, "x.txt").ok().flatten().is_some()).await;
    c.h.od_fetch(&id, "x.txt", Duration::from_secs(30)).await.unwrap();
    a.h.shutdown().await;
    until(30, "B and C connected, A gone", || {
        let (sb, sc) = (b.h.snapshot().devices, c.h.snapshot().devices);
        sb.len() == 1 && sc.len() == 1 && sb[0].name == "Gamma"
    })
    .await;
    // Both let go at once, each asking the other: a device freeing a file holds nothing for others meanwhile, and the
    // higher id never relies on the lower, so at least one keeps it.
    let (eb, ec) = tokio::join!(b.h.od_evict(&id, "x.txt"), c.h.od_evict(&id, "x.txt"));
    let (eb, ec) = (eb.unwrap(), ec.unwrap());
    assert!(eb.files + ec.files <= 1, "{eb:?} {ec:?}");
    let held = [&b, &c].iter().filter(|d| d.h.od_cache_path(&id, "x.txt").unwrap().exists()).count();
    assert!(held >= 1, "a copy left");
    // One after the other: one frees (relying on the other), the other keeps the last copy.
    let (eb, ec) = (b.h.od_evict(&id, "x.txt").await.unwrap(), c.h.od_evict(&id, "x.txt").await.unwrap());
    let held = [&b, &c].iter().filter(|d| d.h.od_cache_path(&id, "x.txt").unwrap().exists()).count();
    assert_eq!(held, 1, "one copy left: {eb:?} {ec:?}");
    b.h.shutdown().await;
    c.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_that_removed_the_folder_or_does_not_answer_holds_nothing() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.od_fetch(&id, "docs/old/c.txt", Duration::from_secs(30)).await.unwrap();
    // A second folder keeps the session up.
    let other = tempfile::tempdir().unwrap();
    let id2: FolderId = a.h.add_folder(other.path().to_path_buf()).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id2, Role::ReadWrite, 1, 1).unwrap().link;
    let other_b = tempfile::tempdir().unwrap();
    b.h.join_folder(&link, other_b.path().join("Other")).await.unwrap();
    until(30, "both folders shared", || b.h.snapshot().devices.first().is_some_and(|d| d.folders.len() == 2)).await;

    // An earlier Seedwell does not answer: kept.
    a.h.test_silent_ondemand(true);
    b.h.test_hold_wait(Duration::from_millis(500));
    let e = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!((e.files, e.kept), (0, 1), "{e:?}");
    assert_eq!(e.kept_why[0].1, trs_sync::ondemand::NOT_HELD_ELSEWHERE);
    a.h.test_silent_ondemand(false);
    assert_eq!(b.h.od_evict(&id, "docs/a.txt").await.unwrap().files, 1, "it answers now");

    // A removes the folder (its files to the Trash), still connected: what it announced holds nothing.
    a.h.remove_folder(&id).await.unwrap();
    assert!(!b.h.snapshot().devices.is_empty(), "still connected");
    let e = b.h.od_evict(&id, "docs/old/c.txt").await.unwrap();
    assert_eq!((e.files, e.kept), (0, 1), "{e:?}");
    assert!(b.h.od_cache_path(&id, "docs/old/c.txt").unwrap().exists());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_on_demand_folder_is_not_removed_and_its_files_are_found_when_it_is_not_running() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    h.set_on_demand(&id, true, None).await.unwrap();
    let cache = h.od_cache_path(&id, "").unwrap();
    // Busy (a scan holds it): the removal is refused, nothing moves, the folder goes on.
    h.test_stop_wait(Duration::from_millis(300));
    let hold = h.test_hold_folder(&id).unwrap();
    let e = h.remove_folder(&id).await.unwrap_err();
    assert!(e.to_string().contains("busy") && e.to_string().contains(&cache.display().to_string()), "{e}");
    assert!(cache.join("a.txt").exists() && !docs.exists());
    drop(hold);
    until(30, "it runs again", || h.od_item_at(&id, "a.txt").ok().flatten().is_some()).await;
    // Where its files are, even when it is not running.
    h.shutdown().await;
    assert_eq!(h.folder_files(&id), Some(cache.clone()));
    assert!(h.od_cache_path(&id, "").is_err(), "not running");
}


#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn holders_are_asked_together_and_a_paused_one_holds_nothing() {
    let (a, b, _fa, _fb, id) = pair().await;
    let c = device("Gamma").await;
    let fc = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    c.h.join_folder(&link, fc.path().join("Shared")).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    until(30, "C has the files", || fc.path().join("Shared/docs/a.txt").exists()).await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    until(30, "B knows both hold it", || b.h.snapshot().devices.len() == 2).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Neither answers: both questions are waited for together, 500 ms in all, and nothing is left waiting.
    a.h.test_silent_ondemand(true);
    c.h.test_silent_ondemand(true);
    b.h.test_hold_wait(Duration::from_millis(500));
    let t = Instant::now();
    let e = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!(e.files, 0);
    assert!(t.elapsed() < Duration::from_millis(900), "{:?}", t.elapsed());
    assert_eq!(b.h.test_holds_waiting(), 0);
    // A paused holder holds nothing for others.
    a.h.test_silent_ondemand(false);
    c.h.test_silent_ondemand(false);
    a.h.set_paused(&id, true).await.unwrap();
    c.h.set_paused(&id, true).await.unwrap();
    assert_eq!(b.h.od_evict(&id, "docs/a.txt").await.unwrap().files, 0);
    c.h.set_paused(&id, false).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(b.h.od_evict(&id, "docs/a.txt").await.unwrap().files, 1, "C answers again");
    for d in [a, b, c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_on_demand_folder_is_renamed_in_place_and_never_moved() {
    let (a, b, _fa, fb, id) = pair().await;
    let st = b.h.rename_folder(&id, "Papers").await.unwrap();
    assert_eq!((st.name.as_str(), st.path.as_str()), ("Papers", fb.path().join("Papers").to_str().unwrap()));
    assert!(b.h.od_cache_path(&id, "").unwrap().exists(), "the files stay in the cache");
    let e = b.h.move_folder(&id, fb.path().join("Elsewhere")).await.unwrap_err();
    assert!(e.to_string().contains("on-demand"), "{e}");
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().is_some(), "it runs on");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_or_removal_that_cannot_be_noted_changes_nothing() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    h.set_on_demand(&id, true, None).await.unwrap();
    // The journal cannot be written (something is in its way).
    let journal = state.path().join("sync").join(format!("{id}.journal"));
    std::fs::create_dir_all(journal.join("in the way")).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    assert!(h.set_on_demand(&id, false, None).await.is_err());
    assert!(docs.is_dir(), "the host's empty place is still there");
    assert!(h.od_item_at(&id, "a.txt").unwrap().is_some(), "still on demand, running");
    assert!(h.remove_folder(&id).await.is_err());
    assert!(h.od_item_at(&id, "a.txt").unwrap().is_some(), "still there, running");
    h.shutdown().await;
}

/// Back to full at its own place, kept in place: until the host lets go of it the place is Seedwell's own sync root,
/// which Windows' cloud-storage check (any Cloud Files sync root) calls cloud storage, and the switch was refused
/// (0.1.18, the Windows QA). What counts is the folder it is in. A place the Mac's check calls cloud storage stands in
/// for the registered root here (on Windows nothing is registered there: a plain folder).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_kept_in_place_goes_back_to_full_at_its_own_place() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    let place = work.path().join("Library").join("CloudStorage");
    h.set_on_demand(&id, true, Some(place.clone())).await.unwrap();
    std::fs::create_dir_all(&place).unwrap();
    h.od_set_in_place(&id, true).await.unwrap();
    until(30, "it runs kept in place", || h.od_item_at(&id, "a.txt").ok().flatten().is_some()).await;
    let full = h.set_on_demand(&id, false, None).await.unwrap();
    assert!(!full.on_demand && full.path == place.display().to_string(), "{full:?}");
    assert_eq!(read(&place.join("a.txt")).as_deref(), Some(&b"held here"[..]));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_cut_off_after_its_copy_was_whole_is_finished_at_the_next_start() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (docs, moved) = (work.path().join("Docs"), work.path().join("Moved"));
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    until(30, "scanned", || h.snapshot().folders[0].files == 1).await;
    h.shutdown().await;
    // A copy to another volume, whole and noted; the original not set aside yet.
    std::fs::create_dir_all(&moved).unwrap();
    for e in std::fs::read_dir(&docs).unwrap() {
        let e = e.unwrap();
        let to = moved.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
            for f in std::fs::read_dir(e.path()).unwrap() {
                let f = f.unwrap();
                if f.file_type().unwrap().is_file() {
                    std::fs::copy(f.path(), to.join(f.file_name())).unwrap();
                }
            }
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
    let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
    let j = format!(r#"{{"kind":"move","from":{},"to":{},"copied":true}}"#, text(&docs), text(&moved));
    std::fs::write(state.path().join("sync").join(format!("{id}.journal")), j).unwrap();
    let h = device_at("Beta", state.path()).await;
    assert_eq!(h.snapshot().folders[0].path, moved.display().to_string());
    assert!(!docs.exists(), "the original went");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(read(&moved.join("a.txt")).as_deref(), Some(&b"held here"[..]));
    h.shutdown().await;
}


#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_busy_with_a_long_pull_still_answers_and_a_dropped_free_ends() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.od_fetch(&id, "docs/old/c.txt", Duration::from_secs(30)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    // A's core is held for 3 s (a long pull); it answers from its index all the same, at once.
    b.h.test_hold_wait(Duration::from_secs(1));
    a.h.test_lock_core(&id, Duration::from_secs(3));
    let e = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!(e.files, 1, "{e:?}");
    // A free whose future is dropped while it asks ends all the same: B answers for its files again.
    a.h.test_silent_ondemand(true);
    b.h.test_hold_wait(Duration::from_secs(5));
    assert!(tokio::time::timeout(Duration::from_millis(300), b.h.od_evict(&id, "docs/old/c.txt")).await.is_err());
    assert_eq!(b.h.test_freeing(&id), 0);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_folder_where_one_is_records_nothing_found_in_it() {
    let (a, b, _fa, _fb, id) = pair().await;
    // Something put content at an online-only path behind the host's back; then the host makes the folder that holds
    // it (it is there already).
    std::fs::write(b.h.od_cache_path(&id, "docs/a.txt").unwrap(), b"stray").unwrap();
    b.h.od_mkdir(&id, "docs").await.unwrap();
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::OnlineOnly, "not taken as the host's");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_writers_word_that_it_holds_a_file_lets_one_go() {
    let (a, b, _fa, _fb, id) = pair().await;
    // R reads only, holds every file, and says it holds anything asked about.
    let r = device("Reader").await;
    let fr = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadOnly, 1, 1).unwrap().link;
    r.h.join_folder(&link, fr.path().join("Shared")).await.unwrap();
    r.h.add_address(&id, addr(&a)).unwrap();
    until(30, "R has the files", || read(&fr.path().join("Shared/docs/a.txt")).as_deref() == Some(&b"alpha"[..])).await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.add_address(&id, addr(&r)).unwrap();
    a.h.shutdown().await;
    until(30, "B connected to R only", || b.h.snapshot().devices.len() == 1).await;
    // (R's index reaches B.)
    tokio::time::sleep(Duration::from_secs(1)).await;
    r.h.test_yes_to_holds(true);
    b.h.test_hold_wait(Duration::from_secs(2));
    let e = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!((e.files, e.kept), (0, 1), "a read-only member's copy is no copy to rely on: {e:?}");
    assert!(b.h.od_cache_path(&id, "docs/a.txt").unwrap().exists());
    r.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflict_on_demand_is_resolved_through_the_core_with_both_versions_here() {
    let (a, b, fa, fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    let base = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    std::fs::write(fa.path().join("docs/a.txt"), b"A's edit").unwrap();
    let _ = a.h.rescan(&id);
    until(30, "B has A's edit", || {
        b.h.od_item_at(&id, "docs/a.txt").ok().flatten().is_some_and(|i| i.content_version != base)
            && b.h.od_cache_path(&id, "docs/a.txt").is_ok_and(|p| read(&p).as_deref() == Some(&b"A's edit"[..]))
    })
    .await;
    let src = fb.path().join("edit.txt");
    std::fs::write(&src, b"B's edit").unwrap();
    let copy = b.h.od_write(&id, "docs/a.txt", &src, Some(&base)).await.unwrap();
    assert_ne!(copy, "docs/a.txt", "a conflict copy");
    until(30, "A has the copy", || fa.path().join(&copy).exists()).await;

    // The original online only here: refused, nothing changes.
    assert_eq!(b.h.od_evict(&id, "docs/a.txt").await.unwrap().files, 1);
    let e = b.h.resolve_conflict(&id, &copy, trs_sync_net::Keep::Copy).await.unwrap_err();
    assert!(e.to_string().contains("Download both versions first"), "{e}");
    assert!(b.h.od_item_at(&id, &copy).unwrap().is_some());

    // Both here: the copy wins, everywhere; what went is kept in the archive.
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.resolve_conflict(&id, &copy, trs_sync_net::Keep::Copy).await.unwrap();
    assert!(b.h.od_item_at(&id, &copy).unwrap().is_none());
    assert_eq!(read(&b.h.od_cache_path(&id, "docs/a.txt").unwrap()).as_deref(), Some(&b"B's edit"[..]));
    until(30, "A took it", || read(&fa.path().join("docs/a.txt")).as_deref() == Some(&b"B's edit"[..]) && !fa.path().join(&copy).exists()).await;
    let archive = b.h.od_cache_path(&id, "").unwrap().join(".seedwell/Archive");
    assert!(std::fs::read_dir(archive.join("docs")).unwrap().next().is_some(), "A's version kept in the archive here");
    // No conflict is left listed, here or on A (the renamed copy is an ordinary file), so nothing can archive it.
    until(30, "no conflict listed on A", || futures_now(a.h.conflicts(&id)).is_empty()).await;
    assert!(b.h.conflicts(&id).await.unwrap().is_empty());

    // Another conflict, both kept: the copy becomes an ordinary file everywhere.
    let base = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    std::fs::write(fa.path().join("docs/a.txt"), b"A's second").unwrap();
    let _ = a.h.rescan(&id);
    until(30, "B has A's second", || b.h.od_cache_path(&id, "docs/a.txt").is_ok_and(|p| read(&p).as_deref() == Some(&b"A's second"[..]))).await;
    std::fs::write(&src, b"B's second").unwrap();
    let copy = b.h.od_write(&id, "docs/a.txt", &src, Some(&base)).await.unwrap();
    until(30, "A has the copy", || fa.path().join(&copy).exists()).await;
    b.h.resolve_conflict(&id, &copy, trs_sync_net::Keep::Both).await.unwrap();
    until(30, "A took it, no conflict listed", || !fa.path().join(&copy).exists() && futures_now(a.h.conflicts(&id)).is_empty()).await;
    assert!(b.h.conflicts(&id).await.unwrap().is_empty());
    let kept: Vec<String> = std::fs::read_dir(fa.path().join("docs")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains("a (")).collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    // A resolve of the old copy name finds nothing to act on: the kept file stays.
    a.h.resolve_conflict(&id, &copy, trs_sync_net::Keep::Original).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(read(&fa.path().join("docs").join(&kept[0])).as_deref(), Some(&b"B's second"[..]));
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_made_while_a_folder_was_closed_here_still_arrive_from_new_and_earlier_builds() {
    for features in [trs_sync_proto::msg::FEATURES, FEATURES_0_1_11] {
        let owner = device_with("Owner", features).await;
        let (files, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(files.path().join("a.txt"), b"a").unwrap();
        let id: FolderId = owner.h.add_folder(files.path().to_path_buf()).await.unwrap().id.parse().unwrap();
        let link = owner.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
        let joiner = device("Joiner").await;
        let (place, moved) = (work.path().join("Joined"), work.path().join("Moved"));
        joiner.h.join_folder(&link, place.clone()).await.unwrap();
        joiner.h.add_address(&id, addr(&owner)).unwrap();
        until(30, "the joiner has a.txt", || place.join("a.txt").exists()).await;
        // The joiner moves the folder; while it is closed, the owner makes two changes.
        joiner.h.test_reopen_wait(Duration::from_secs(3));
        let h = joiner.h.clone();
        let moving = tokio::spawn(async move { h.move_folder(&id, moved.clone()).await });
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::fs::write(files.path().join("b.txt"), b"made while it was closed").unwrap();
        let _ = owner.h.rescan(&id);
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::fs::write(files.path().join("c.txt"), b"this one too").unwrap();
        let _ = owner.h.rescan(&id);
        moving.await.unwrap().unwrap();
        let moved = work.path().join("Moved");
        until(30, "both changes arrived", || read(&moved.join("b.txt")).is_some() && read(&moved.join("c.txt")).is_some()).await;
        // And later ones too.
        std::fs::write(files.path().join("d.txt"), b"later").unwrap();
        let _ = owner.h.rescan(&id);
        until(30, "a later change arrived", || read(&moved.join("d.txt")).is_some()).await;
        owner.h.shutdown().await;
        joiner.h.shutdown().await;
    }
}

/// A full folder of `owner`'s, joined (full) by each of `names`: their handles and places, the owner's folder, its id.
async fn full_group(owner: &Device, names: &[&str], work: &std::path::Path) -> (Vec<Device>, Vec<std::path::PathBuf>, std::path::PathBuf, FolderId) {
    let files = work.join("Owner's");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::write(files.join("a.txt"), b"a").unwrap();
    let id: FolderId = owner.h.add_folder(files.clone()).await.unwrap().id.parse().unwrap();
    let (mut ds, mut places) = (Vec::new(), Vec::new());
    for n in names {
        let link = owner.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
        let d = device(n).await;
        let place = work.join(n);
        d.h.join_folder(&link, place.clone()).await.unwrap();
        d.h.add_address(&id, addr(owner)).unwrap();
        until(30, "a member has a.txt", || place.join("a.txt").exists()).await;
        ds.push(d);
        places.push(place);
    }
    (ds, places, files, id)
}

/// Writes `name` in `dir` and has `h` scan it.
fn change(h: &SyncHandle, id: &FolderId, dir: &std::path::Path, name: &str) {
    std::fs::write(dir.join(name), name.as_bytes()).unwrap();
    let _ = h.rescan(id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_on_both_sides_never_stalls_the_folder_nor_skips_a_change() {
    // Once with the device that pauses first being the one dialled (the other dials it again), once with it dialling.
    for first_dials in [false, true] {
        let work = tempfile::tempdir().unwrap();
        let owner = device("Alpha").await;
        let (ds, places, files, id) = full_group(&owner, &["Beta"], work.path()).await;
        let (joiner, jp) = (&ds[0], &places[0]);
        // (The joiner dials the owner.)
        let ((a, ap), (b, bp)) = if first_dials { ((joiner, jp), (&owner, &files)) } else { ((&owner, &files), (joiner, jp)) };
        // A pauses; B changes x (A drops it); B pauses; A resumes; B resumes and changes y.
        a.h.set_paused(&id, true).await.unwrap();
        change(&b.h, &id, bp, "x.txt");
        tokio::time::sleep(Duration::from_millis(800)).await;
        b.h.set_paused(&id, true).await.unwrap();
        a.h.set_paused(&id, false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        b.h.set_paused(&id, false).await.unwrap();
        change(&b.h, &id, bp, "y.txt");
        until(30, "x and y at A", || ap.join("x.txt").exists() && ap.join("y.txt").exists()).await;
        // Paused and resumed within a round trip, twice, with changes meanwhile: nothing stalls, nothing is skipped.
        for n in 0..2 {
            a.h.set_paused(&id, true).await.unwrap();
            change(&b.h, &id, bp, &format!("p{n}.txt"));
            a.h.set_paused(&id, false).await.unwrap();
        }
        a.h.set_paused(&id, true).await.unwrap();
        a.h.set_paused(&id, false).await.unwrap();
        change(&b.h, &id, bp, "z.txt");
        until(30, "every change at A", || ["p0.txt", "p1.txt", "z.txt"].iter().all(|n| ap.join(n).exists())).await;
        // And A's changes reach B.
        change(&a.h, &id, ap, "from-a.txt");
        until(30, "A's change at B", || bp.join("from-a.txt").exists()).await;
        owner.h.shutdown().await;
        joiner.h.shutdown().await;
    }
}

/// `d` started again on its state (a new session with every peer), offering `features`.
async fn again(d: Device, name: &str, features: u64) -> Device {
    d.h.shutdown().await;
    let state = d._state;
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    opts.features = features;
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_closed_while_a_session_is_set_up_links_once_it_opens() {
    for features in [trs_sync_proto::msg::FEATURES, FEATURES_0_1_11] {
        // Review 12, #1: A's core is held while B dials (A's answer waits on it); A pauses meanwhile, B writes x, A
        // resumes, B writes y. Both arrive.
        let work = tempfile::tempdir().unwrap();
        let a = device("Alpha").await;
        let (mut ds, places, files, id) = full_group(&a, &["Beta"], work.path()).await;
        let bp = places[0].clone();
        let b = ds.remove(0);
        b.h.shutdown().await;
        a.h.test_lock_core(&id, Duration::from_secs(6));
        let b = again(b, "Beta", features).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        a.h.set_paused(&id, true).await.unwrap();
        change(&b.h, &id, &bp, "x.txt");
        tokio::time::sleep(Duration::from_secs(7)).await;
        a.h.set_paused(&id, false).await.unwrap();
        change(&b.h, &id, &bp, "y.txt");
        until(40, "x and y at A", || files.join("x.txt").exists() && files.join("y.txt").exists()).await;
        a.h.shutdown().await;
        b.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_closed_when_a_peer_dials_links_once_it_opens() {
    for features in [trs_sync_proto::msg::FEATURES, FEATURES_0_1_11] {
        // Review 12, #2: A's folder is paused when B dials (B offers it, A leaves it out); B writes x; A resumes: x
        // arrives, and so do later changes both ways.
        let work = tempfile::tempdir().unwrap();
        let a = device("Alpha").await;
        let (mut ds, places, files, id) = full_group(&a, &["Beta"], work.path()).await;
        let bp = places[0].clone();
        // A second folder they share keeps the session up while the first is closed at A.
        let other = work.path().join("Other");
        std::fs::create_dir_all(&other).unwrap();
        let oid: FolderId = a.h.add_folder(other.clone()).await.unwrap().id.parse().unwrap();
        let link = a.h.create_invite(&oid, Role::ReadWrite, 1, 1).unwrap().link;
        ds[0].h.join_folder(&link, work.path().join("Beta's other")).await.unwrap();
        until(30, "B in both folders", || ds[0].h.snapshot().folders.iter().all(|f| f.role.is_some())).await;
        a.h.set_paused(&id, true).await.unwrap();
        let b = again(ds.remove(0), "Beta", features).await;
        until(30, "B connected again", || b.h.snapshot().devices.len() == 1).await;
        change(&b.h, &id, &bp, "x.txt");
        tokio::time::sleep(Duration::from_secs(1)).await;
        a.h.set_paused(&id, false).await.unwrap();
        until(30, "x at A", || files.join("x.txt").exists()).await;
        change(&a.h, &id, &files, "from-a.txt");
        until(30, "A's change at B", || bp.join("from-a.txt").exists()).await;
        a.h.shutdown().await;
        b.h.shutdown().await;
    }
}

/// The owner's folder F (with a.txt) and a second folder G they both have, which keeps sessions up while F is closed;
/// The owner's folder F (with a.txt) and a second folder G they both have, which keeps sessions up while F is closed;
/// the owner, the joiner, the owner's and the joiner's place of F, F's and G's ids. `g_first`: G's id sorts before F's
/// (each device takes them in that order), or after.
async fn two_folders(work: &std::path::Path, g_first: bool) -> (Device, Device, std::path::PathBuf, std::path::PathBuf, FolderId, FolderId) {
    two_folders_with(work, g_first, trs_sync_proto::msg::FEATURES, trs_sync_proto::msg::FEATURES).await
}

/// [`two_folders`], the owner and the joiner offering these features. (The joiner dials the owner.)
async fn two_folders_with(
    work: &std::path::Path,
    g_first: bool,
    owner: u64,
    joiner: u64,
) -> (Device, Device, std::path::PathBuf, std::path::PathBuf, FolderId, FolderId) {
    let a = device_with("Alpha", owner).await;
    let files = work.join("Owner's");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::write(files.join("a.txt"), b"a").unwrap();
    let id: FolderId = a.h.add_folder(files.clone()).await.unwrap().id.parse().unwrap();
    // G with an id on the asked side of F's (ids are random: one that is not goes again).
    let gid = loop {
        let other = work.join(format!("Other {}", rand_tag()));
        std::fs::create_dir_all(&other).unwrap();
        let gid: FolderId = a.h.add_folder(other).await.unwrap().id.parse().unwrap();
        if (gid.0 < id.0) == g_first {
            break gid;
        }
        a.h.remove_folder(&gid).await.unwrap();
    };
    let b = device_with("Beta", joiner).await;
    let place = work.join("Beta's");
    for (fid, at) in [(id, place.clone()), (gid, work.join("Beta's other"))] {
        let link = a.h.create_invite(&fid, Role::ReadWrite, 1, 1).unwrap().link;
        b.h.join_folder(&link, at).await.unwrap();
        b.h.add_address(&fid, addr(&a)).unwrap();
    }
    until(30, "B in both folders, with a.txt", || place.join("a.txt").exists() && b.h.snapshot().folders.iter().all(|f| f.role.is_some())).await;
    (a, b, files, place, id, gid)
}

fn rand_tag() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_opens_while_a_session_is_set_up_links_on_it() {
    // Both orders of the two folders (each device takes them in id order: F before G, or after).
    for g_first in [true, false] {
        // Review 13, the listener: A's F is paused when B dials; A's answer waits 6 s on G's core; F resumes 2 s into
        // that. Then B's change of F reaches A, and A's reaches B.
        let work = tempfile::tempdir().unwrap();
        let (a, b, af, bf, id, gid) = two_folders(work.path(), g_first).await;
        a.h.set_paused(&id, true).await.unwrap();
        b.h.shutdown().await;
        let b = Device { h: device_at("Beta", b._state.path()).await, _state: b._state };
        let _ = &b;
        a.h.test_lock_core(&gid, Duration::from_secs(6));
        tokio::time::sleep(Duration::from_secs(2)).await;
        a.h.set_paused(&id, false).await.unwrap();
        change(&b.h, &id, &bf, "x.txt");
        until(40, "x at A", || af.join("x.txt").exists()).await;
        change(&a.h, &id, &af, "from-a.txt");
        until(30, "A's change at B", || bf.join("from-a.txt").exists()).await;
        a.h.shutdown().await;
        b.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_opens_while_its_session_is_dialled_links_on_it() {
    // Both orders of the two folders (each device takes them in id order: F before G, or after).
    for g_first in [true, false] {
        // Review 13, the dialer: B's F is paused; A starts again, B dials it, B's Join waits 6 s on G's core; F resumes
        // at B 2 s into that.
        let work = tempfile::tempdir().unwrap();
        let (a, b, af, bf, id, gid) = two_folders(work.path(), g_first).await;
        b.h.set_paused(&id, true).await.unwrap();
        let a = again(a, "Alpha", trs_sync_proto::msg::FEATURES).await;
        b.h.test_lock_core(&gid, Duration::from_secs(6));
        tokio::time::sleep(Duration::from_secs(2)).await;
        b.h.set_paused(&id, false).await.unwrap();
        change(&a.h, &id, &af, "y.txt");
        until(40, "y at B", || bf.join("y.txt").exists()).await;
        change(&b.h, &id, &bf, "from-b.txt");
        until(30, "B's change at A", || af.join("from-b.txt").exists()).await;
        a.h.shutdown().await;
        b.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_opened_while_connected_links_without_starting_the_session_anew() {
    // Review 15, #4: a folder opened while connected is offered once per session (a second offer would read as the
    // folder opened again and start the session anew).
    let work = tempfile::tempdir().unwrap();
    let (a, b, _af, _bf, _id, _gid) = two_folders(work.path(), true).await;
    let sessions = (a.h.test_sessions(), b.h.test_sessions());
    let h = work.path().join("Third");
    std::fs::create_dir_all(&h).unwrap();
    std::fs::write(h.join("h.txt"), b"h").unwrap();
    let hid: FolderId = a.h.add_folder(h.clone()).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&hid, Role::ReadWrite, 1, 1).unwrap().link;
    let bh = work.path().join("Beta's third");
    b.h.join_folder(&link, bh.clone()).await.unwrap();
    // Opened again and again meanwhile (each opening offers it where it is not in our Join yet).
    for _ in 0..3 {
        a.h.set_paused(&hid, false).await.unwrap();
        b.h.set_paused(&hid, false).await.unwrap();
    }
    until(30, "the third folder at B", || bh.join("h.txt").exists()).await;
    change(&b.h, &hid, &bh, "from-b.txt");
    until(30, "back at A", || h.join("from-b.txt").exists()).await;
    assert_eq!((a.h.test_sessions(), b.h.test_sessions()), sessions, "no session started anew");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_left_out_of_our_answer_is_never_linked() {
    // Review 14, #1: B dials A; A's answer reaches F only after G (whose core is held 6 s), and F is paused by then:
    // F is not in A's Join. A's session waits 4 s more before it is up; F resumes meanwhile. F must not be linked
    // on that session (B never linked it): the session starts anew, and B's change of F reaches A.
    // One session only while it is set up: B dials once, A not at all (no other attempt can make up for it).
    let work = tempfile::tempdir().unwrap();
    let (a, b, af, bf, id, gid) = two_folders(work.path(), true).await;
    a.h.test_dials_left(Some(0));
    b.h.shutdown().await;
    a.h.test_lock_core(&gid, Duration::from_secs(6));
    a.h.test_setup_wait(Duration::from_secs(4));
    let b = {
        let state = b._state;
        let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
        let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
        opts.default_device_name = "Beta".into();
        opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        opts.lan_discovery = false;
        opts.dials = Some(1);
        Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    a.h.set_paused(&id, true).await.unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    a.h.set_paused(&id, false).await.unwrap();
    a.h.test_setup_wait(Duration::ZERO);
    tokio::time::sleep(Duration::from_secs(4)).await;
    // That session is up. B may dial again (to start a session anew, if one ends).
    b.h.test_dials_left(None);
    change(&b.h, &id, &bf, "x.txt");
    until(40, "x at A", || af.join("x.txt").exists()).await;
    change(&a.h, &id, &af, "from-a.txt");
    until(30, "A's change at B", || bf.join("from-a.txt").exists()).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_folder_an_earlier_build_named_links_once_it_opens() {
    // Final review, #1: X's F is held back at start (a move cut off, its disk away), so not open and not stopped
    // either; P (0.1.11) dials X and names F. Once X's disk is back and F opens, P's changes of F reach X.
    let work = tempfile::tempdir().unwrap();
    let (p, x, pf, xf, id, _gid) = two_folders_with(work.path(), true, FEATURES_0_1_11, trs_sync_proto::msg::FEATURES).await;
    x.h.shutdown().await;
    let to = work.path().join("Moved");
    mark(&to);
    let unmounted = work.path().join("unmounted");
    std::fs::rename(&xf, &unmounted).unwrap();
    let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
    std::fs::write(x._state.path().join("sync").join(format!("{id}.journal")), format!(r#"{{"kind":"move","from":{},"to":{},"copied":false}}"#, text(&xf), text(&to))).unwrap();
    // X starts again (on a new port) and dials nobody: P dials it.
    let x = {
        let state = x._state;
        let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
        let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
        opts.default_device_name = "Beta".into();
        opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        opts.lan_discovery = false;
        opts.dials = Some(0);
        opts.held_retry = Duration::from_secs(3600);
        Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
    };
    assert!(x.h.snapshot().folders.iter().any(|f| f.state == FolderState::Missing), "F held back");
    p.h.add_address(&id, addr(&x)).unwrap();
    until(30, "P connected to X", || p.h.snapshot().devices.len() == 1).await;
    // The disk is back: the move is undone and F opens.
    std::fs::rename(&unmounted, &xf).unwrap();
    x.h.volumes_changed().await;
    until(30, "F open at X", || x.h.snapshot().folders.iter().all(|f| f.state != FolderState::Missing)).await;
    change(&p.h, &id, &pf, "x.txt");
    until(40, "P's change at X", || xf.join("x.txt").exists()).await;
    p.h.shutdown().await;
    x.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_an_earlier_build_offers_later_is_answered() {
    // Review 14, #2: P plays 0.1.11 and owns F and G. F is paused at P when A dials: A's Join names F, P's answer
    // does not. P resumes F (an earlier build offers nothing then) and moves it: its Join offers F, and waits for
    // ours. A answers; then A's change of F reaches P.
    let work = tempfile::tempdir().unwrap();
    let (p, a, pf, af, id, _gid) = two_folders_with(work.path(), true, FEATURES_0_1_11, trs_sync_proto::msg::FEATURES).await;
    p.h.set_paused(&id, true).await.unwrap();
    // A dials P (P starts again while A is away: its own dial fails and waits).
    a.h.shutdown().await;
    let p = again(p, "Alpha", FEATURES_0_1_11).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let a = again(a, "Beta", trs_sync_proto::msg::FEATURES).await;
    // (P listens on a new port now.)
    a.h.add_address(&id, addr(&p)).unwrap();
    until(30, "A connected", || a.h.snapshot().devices.len() == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sessions = a.h.test_sessions();
    p.h.set_paused(&id, false).await.unwrap();
    let moved = work.path().join("P's moved");
    p.h.move_folder(&id, moved.clone()).await.unwrap();
    let _ = pf;
    change(&a.h, &id, &af, "x.txt");
    until(40, "x at P", || moved.join("x.txt").exists()).await;
    // Its answer to our answer is no reopening: the session never started anew.
    assert_eq!(a.h.test_sessions(), sessions, "a session started anew");
    p.h.shutdown().await;
    a.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_an_earlier_build_opens_again_is_taken_from_the_start_of_its_gap() {
    // Review 13, #2: P plays 0.1.11 and moves F (closed 3 s; its Join comes on the same session after). What A
    // changed meanwhile reaches P.
    let work = tempfile::tempdir().unwrap();
    let a = device("Alpha").await;
    let files = work.path().join("Owner's");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::write(files.join("a.txt"), b"a").unwrap();
    let id: FolderId = a.h.add_folder(files.clone()).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let p = device_with("Old", FEATURES_0_1_11).await;
    let (place, moved) = (work.path().join("P"), work.path().join("P moved"));
    p.h.join_folder(&link, place.clone()).await.unwrap();
    p.h.add_address(&id, addr(&a)).unwrap();
    until(30, "P has a.txt", || place.join("a.txt").exists()).await;
    p.h.test_reopen_wait(Duration::from_secs(3));
    let h = p.h.clone();
    let moving = tokio::spawn(async move { h.move_folder(&id, moved).await });
    tokio::time::sleep(Duration::from_millis(800)).await;
    change(&a.h, &id, &files, "x.txt");
    moving.await.unwrap().unwrap();
    let moved = work.path().join("P moved");
    until(30, "x at P", || moved.join("x.txt").exists()).await;
    a.h.shutdown().await;
    p.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_with_two_peers_takes_both_ones_changes_after() {
    let work = tempfile::tempdir().unwrap();
    let a = device("Alpha").await;
    let (ds, places, files, id) = full_group(&a, &["Beta", "Gamma"], work.path()).await;
    a.h.set_paused(&id, true).await.unwrap();
    change(&ds[0].h, &id, &places[0], "from-b.txt");
    change(&ds[1].h, &id, &places[1], "from-c.txt");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!files.join("from-b.txt").exists() && !files.join("from-c.txt").exists(), "paused: nothing taken");
    a.h.set_paused(&id, false).await.unwrap();
    until(30, "both changes at A", || files.join("from-b.txt").exists() && files.join("from-c.txt").exists()).await;
    a.h.shutdown().await;
    for d in ds {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_sent_between_resume_and_the_new_session_arrives_by_the_new_one() {
    let work = tempfile::tempdir().unwrap();
    let a = device("Alpha").await;
    let (ds, places, files, id) = full_group(&a, &["Beta"], work.path()).await;
    let (b, bp) = (&ds[0], &places[0]);
    a.h.set_paused(&id, true).await.unwrap();
    change(&b.h, &id, bp, "x.txt");
    // Resumed, but the session starts anew only 3 s later: what B sends meanwhile on the old one is not taken.
    a.h.test_reopen_wait(Duration::from_secs(3));
    // The call returns once the folder is resumed: the sessions start anew in the background.
    let t = Instant::now();
    a.h.set_paused(&id, false).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(1), "resuming waited on the network: {:?}", t.elapsed());
    tokio::time::sleep(Duration::from_millis(500)).await;
    change(&b.h, &id, bp, "y.txt");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!files.join("x.txt").exists() && !files.join("y.txt").exists(), "nothing of the stale session");
    until(30, "both at A by the new session", || files.join("x.txt").exists() && files.join("y.txt").exists()).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_earlier_build_is_never_asked_what_it_cannot_answer() {
    let (a, b, _fa, _fb, id) = pair_with(Role::ReadWrite, FEATURES_0_1_11).await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    b.h.test_hold_wait(Duration::from_secs(5));
    let t = Instant::now();
    // A is the only holder, and does not know the questions: refused at once, not after waiting.
    let e = b.h.od_rename(&id, "big.bin", "moved.bin").await.unwrap_err();
    assert!(e.to_string().contains(trs_sync::folder::RENAME_NEEDS_HOLDER), "{e}");
    let ev = b.h.od_evict(&id, "docs/a.txt").await.unwrap();
    assert_eq!((ev.files, ev.kept), (0, 1), "{ev:?}");
    assert!(t.elapsed() < Duration::from_secs(3), "nothing waited for an answer: {:?}", t.elapsed());
    assert_eq!(b.h.test_renames_waiting() + b.h.test_holds_waiting(), 0);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_the_holder_cannot_start_in_time_is_refused_and_never_happens() {
    let (a, b, fa, _fb, id) = pair().await;
    b.h.test_hold_wait(Duration::from_secs(1));
    // A's core is held 4 s (a long pull): A says it is busy within the second, and never renames later.
    a.h.test_lock_core(&id, Duration::from_secs(4));
    let e = b.h.od_rename(&id, "big.bin", "moved.bin").await.unwrap_err();
    assert!(e.to_string().contains("busy"), "{e}");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(fa.path().join("big.bin").exists() && !fa.path().join("moved.bin").exists(), "never renamed later");
    assert!(b.h.od_item_at(&id, "big.bin").unwrap().is_some() && b.h.od_item_at(&id, "moved.bin").unwrap().is_none());
    // Held for less than the time to live: renamed, and B reports it.
    a.h.test_lock_core(&id, Duration::from_millis(300));
    assert_eq!(b.h.od_rename(&id, "big.bin", "moved.bin").await.unwrap(), 1);
    assert!(fa.path().join("moved.bin").exists() && !fa.path().join("big.bin").exists());
    assert!(b.h.od_item_at(&id, "moved.bin").unwrap().is_some());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_without_the_files_needs_a_device_that_has_them() {
    let (a, b, fa, _fb, id) = pair().await;
    let renames = |h: &SyncHandle| h.test_renames_waiting();
    // A is an earlier Seedwell (it leaves the request unanswered): refused, nothing renamed.
    a.h.test_silent_ondemand(true);
    b.h.test_hold_wait(Duration::from_secs(1));
    let e = b.h.od_rename(&id, "big.bin", "docs/moved.bin").await.unwrap_err();
    assert!(e.to_string().contains("did not answer"), "an outcome not known is no refusal: {e}");
    assert_eq!(renames(&b.h), 0, "the request is forgotten");
    a.h.test_silent_ondemand(false);
    assert!(b.h.od_item_at(&id, "big.bin").unwrap().is_some());
    // A away: refused at once.
    let a_state = a._state;
    a.h.shutdown().await;
    until(30, "B sees A gone", || b.h.snapshot().devices.is_empty()).await;
    let e = b.h.od_rename(&id, "big.bin", "docs/moved.bin").await.unwrap_err();
    assert!(e.to_string().contains(trs_sync::folder::RENAME_NEEDS_HOLDER), "{e}");
    assert!(b.h.od_item_at(&id, "big.bin").unwrap().is_some() && b.h.od_item_at(&id, "docs/moved.bin").unwrap().is_none());
    // A back: it makes the rename, as its own change, and the call ends when the result is here.
    let secrets = Arc::new(FileSecrets::new(a_state.path().join("secrets")));
    let mut opts = Options::new(a_state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = "Alpha".into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    let a = SyncHandle::start(opts).await.unwrap();
    b.h.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port())).unwrap();
    until(30, "B connected to A again", || b.h.snapshot().devices.len() == 1).await;
    let big = b.h.od_item_at(&id, "big.bin").unwrap().unwrap().id;
    assert_eq!(b.h.od_rename(&id, "big.bin", "docs/moved.bin").await.unwrap(), 1);
    assert_eq!(b.h.od_item_at(&id, "docs/moved.bin").unwrap().unwrap().id, big);
    assert_eq!(read(&fa.path().join("docs/moved.bin")).map(|v| v.len()), Some(2_000_000));
    assert!(!fa.path().join("big.bin").exists());
    assert!(b.h.unavailable(&id).unwrap().is_empty());
    a.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_freeing_a_file_says_no_and_its_free_ends_with_the_work_not_the_caller() {
    let (a, b, _fa, fb, id) = pair().await;
    let c = device("Gamma").await;
    let fc = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    c.h.join_folder_on_demand(&link, fc.path().join("Shared")).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    let src = fb.path().join("x.txt");
    std::fs::write(&src, b"two copies").unwrap();
    b.h.od_write(&id, "x.txt", &src, None).await.unwrap();
    until(30, "C lists it", || c.h.od_item_at(&id, "x.txt").ok().flatten().is_some()).await;
    c.h.od_fetch(&id, "x.txt", Duration::from_secs(30)).await.unwrap();
    a.h.shutdown().await;
    until(30, "B and C connected, A gone", || b.h.snapshot().devices.len() == 1 && c.h.snapshot().devices.len() == 1).await;
    // The higher is freeing x.txt (it asked the lower, who does not answer); asked meanwhile by the lower, it says no.
    // The higher one: its answer to the lower would be yes, but for its own free in progress.
    let (lo, hi) = if b.h.device_id().short().0 > c.h.device_id().short().0 { (&c, &b) } else { (&b, &c) };
    lo.h.test_silent_ondemand(true);
    hi.h.test_hold_wait(Duration::from_secs(3));
    let freeing = { let h = hi.h.clone(); tokio::spawn(async move { h.od_evict(&id, "x.txt").await }) };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hi.h.test_freeing(&id), 1);
    lo.h.test_hold_wait(Duration::from_secs(1));
    let e = lo.h.od_evict(&id, "x.txt").await.unwrap();
    assert_eq!(e.files, 0, "the other is freeing it: no: {e:?}");
    let _ = freeing.await;
    assert_eq!(hi.h.test_freeing(&id), 0);
    // A caller dropped while the free waits for the core (after asking): the free still runs, and ends with it.
    hi.h.test_hold_wait(Duration::from_secs(1));
    let (h2, id2) = (hi.h.clone(), id);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        h2.test_lock_core(&id2, Duration::from_secs(2));
    });
    assert!(tokio::time::timeout(Duration::from_millis(1500), hi.h.od_evict(&id, "x.txt")).await.is_err());
    assert_eq!(hi.h.test_freeing(&id), 1, "still freeing: the blocking work has not run yet");
    until(10, "the free ended with its work", || hi.h.test_freeing(&id) == 0).await;
    b.h.shutdown().await;
    c.h.shutdown().await;
}


#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_whose_original_is_not_there_waits_stopped_never_at_the_partial_copy() {
    for by_timer in [false, true] {
        let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (docs, moved) = (work.path().join("Docs"), work.path().join("Moved"));
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), b"held here").unwrap();
        std::fs::write(docs.join("b.txt"), b"not copied yet").unwrap();
        let h = device_at("Beta", state.path()).await;
        let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
        until(30, "scanned", || h.snapshot().folders[0].files == 2).await;
        h.shutdown().await;
        // Cut off while copying to another volume; then the original's disk is not mounted at the start.
        std::fs::create_dir_all(&moved).unwrap();
        mark(&moved);
        std::fs::copy(docs.join("a.txt"), moved.join("a.txt")).unwrap();
        let unmounted = work.path().join("unmounted");
        std::fs::rename(&docs, &unmounted).unwrap();
        let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
        let journal = state.path().join("sync").join(format!("{id}.journal"));
        std::fs::write(&journal, format!(r#"{{"kind":"move","from":{},"to":{},"copied":false}}"#, text(&docs), text(&moved))).unwrap();
        let every = if by_timer { Duration::from_millis(300) } else { Duration::from_secs(3600) };
        let h = device_retrying("Beta", state.path(), every).await;
        // Held back: not opened at either end, and shown as missing, with why.
        let f = h.snapshot().folders[0].clone();
        assert_eq!((f.state.clone(), f.path.as_str()), (FolderState::Missing, docs.to_str().unwrap()));
        assert!(f.error.as_deref().is_some_and(|e| e.starts_with("Waiting for the disk with its original files")), "{f:?}");
        assert!(journal.exists() && moved.join(".seedwell/partial-copy").exists());
        h.volumes_changed().await;
        assert_eq!(h.snapshot().folders[0].state, FolderState::Missing, "still not there");
        // The disk is back: undone without a restart (the app says a disk was mounted; a daemon tries on its timer),
        // the folder where it was, whole.
        std::fs::rename(&unmounted, &docs).unwrap();
        if !by_timer {
            h.volumes_changed().await;
        }
        until(10, "the folder runs again", || h.snapshot().folders.first().is_some_and(|f| f.state != FolderState::Missing)).await;
        assert_eq!(h.snapshot().folders[0].path, docs.display().to_string());
        assert!(!moved.exists() && !journal.exists());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(read(&docs.join("b.txt")).as_deref(), Some(&b"not copied yet"[..]));
        h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_device_holds_nothing_for_the_others_and_an_offline_one_still_does() {
    let owner = device("Owner").await;
    let (files, joiner_state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(files.path().join("a.txt"), b"a").unwrap();
    std::fs::write(files.path().join("b.txt"), b"b").unwrap();
    let place = work.path().join("Joined");
    let (j, id) = joined_full(&owner, files.path(), joiner_state.path(), &place).await;
    let only = |h: &SyncHandle| h.only_here(&id).unwrap();
    until(30, "the owner knows the joiner holds them", || only(&owner.h) == 0).await;
    let joiner = j.device_id();
    // Offline: still a holder.
    j.shutdown().await;
    until(30, "the joiner gone", || owner.h.snapshot().devices.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(only(&owner.h), 0, "an offline member still holds them");
    // Removed: no holder any more; the owner's files and versions as they were.
    let before = owner.h.snapshot().folders[0].files;
    owner.h.remove_device(&id, &joiner).unwrap();
    until(30, "the removed device no holder", || only(&owner.h) == 2).await;
    assert_eq!(owner.h.snapshot().folders[0].files, before);
    assert_eq!(read(&files.path().join("a.txt")).as_deref(), Some(&b"a"[..]));
    owner.h.shutdown().await;
}

/// A full folder joined by `name` from `owner`, at `place`, with the owner's files there; the joining device runs on
/// `state` (to start it again on it). Returns the joiner, the folder id.
async fn joined_full(owner: &Device, files: &std::path::Path, state: &std::path::Path, place: &std::path::Path) -> (SyncHandle, FolderId) {
    let st = owner.h.add_folder(files.to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = owner.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let h = device_at("Alpha", state).await;
    h.join_folder(&link, place.to_path_buf()).await.unwrap();
    h.add_address(&id, addr(owner)).unwrap();
    until(30, "the joiner has the files", || place.join("b.txt").exists()).await;
    (h, id)
}

/// The joiner's folder held back: a move cut off while copying to another volume, its original's disk unmounted.
async fn held_back(h: SyncHandle, id: &FolderId, state: &std::path::Path, place: &std::path::Path, to: &std::path::Path) -> (SyncHandle, std::path::PathBuf) {
    h.shutdown().await;
    std::fs::create_dir_all(to).unwrap();
    mark(to);
    let unmounted = place.with_file_name("unmounted");
    std::fs::rename(place, &unmounted).unwrap();
    let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
    std::fs::write(state.join("sync").join(format!("{id}.journal")), format!(r#"{{"kind":"move","from":{},"to":{},"copied":false}}"#, text(place), text(to))).unwrap();
    let h = device_at("Alpha", state).await;
    assert_eq!(h.snapshot().folders[0].state, FolderState::Missing);
    (h, unmounted)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_folder_is_never_joined_again_changed_or_resurrected_and_its_removal_deletes_nothing() {
    let owner = device("Owner").await;
    let (files, work, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(files.path().join("a.txt"), b"a").unwrap();
    std::fs::write(files.path().join("b.txt"), b"b").unwrap();
    let (place, to) = (work.path().join("Docs"), work.path().join("Moved"));
    let (h, id) = joined_full(&owner, files.path(), state.path(), &place).await;
    let (h, unmounted) = held_back(h, &id, state.path(), &place, &to).await;
    // Joined again at another place: refused (it would reuse the index there, and the scan delete everything).
    let link = owner.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let e = h.join_folder(&link, work.path().join("Docs 2")).await.unwrap_err();
    assert!(e.to_string().contains("already synced"), "{e}");
    // Moved, renamed, switched: refused while it waits.
    assert!(h.move_folder(&id, work.path().join("Else")).await.unwrap_err().to_string().contains("waiting for the disk"));
    assert!(h.rename_folder(&id, "Papers").await.is_err());
    assert!(h.set_on_demand(&id, true, None).await.is_err());
    // Removed: forgotten, its journal gone, nothing on disk touched, never started again by a retry.
    let at = h.remove_folder(&id).await.unwrap();
    assert_eq!(at, place);
    assert!(h.snapshot().folders.is_empty());
    assert!(!state.path().join("sync").join(format!("{id}.journal")).exists());
    assert!(to.join(".seedwell/partial-copy").exists() && read(&unmounted.join("a.txt")).as_deref() == Some(&b"a"[..]));
    std::fs::rename(&unmounted, &place).unwrap();
    h.volumes_changed().await;
    assert!(h.snapshot().folders.is_empty(), "not started again");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(read(&files.path().join("a.txt")).as_deref(), Some(&b"a"[..]), "nothing deleted at the owner");
    assert_eq!(read(&files.path().join("b.txt")).as_deref(), Some(&b"b"[..]));
    h.shutdown().await;
    owner.h.shutdown().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_that_fails_for_another_reason_is_shown_in_error_and_not_retried() {
    use std::os::unix::fs::PermissionsExt;
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (docs, moved) = (work.path().join("Docs"), work.path().join("Moved"));
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = device_at("Beta", state.path()).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    h.shutdown().await;
    // A partial copy that cannot be removed (a folder in it that cannot be written), the original there.
    std::fs::create_dir_all(moved.join("sub")).unwrap();
    mark(&moved);
    std::fs::write(moved.join("sub/x"), b"x").unwrap();
    std::fs::set_permissions(moved.join("sub"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let text = |p: &std::path::Path| format!("{:?}", p.display().to_string());
    let journal = state.path().join("sync").join(format!("{id}.journal"));
    std::fs::write(&journal, format!(r#"{{"kind":"move","from":{},"to":{},"copied":false}}"#, text(&docs), text(&moved))).unwrap();
    let h = device_retrying("Beta", state.path(), Duration::from_millis(200)).await;
    let f = h.snapshot().folders[0].clone();
    assert_eq!(f.state, FolderState::Error, "{f:?}");
    assert!(f.error.as_deref().is_some_and(|e| e.contains("could not be finished")), "{f:?}");
    // Fixed by hand meanwhile: the timer does not retry it (no copy-and-discard loop); the next start does.
    std::fs::set_permissions(moved.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.snapshot().folders[0].state, FolderState::Error);
    assert!(journal.exists());
    h.shutdown().await;
    let h = device_at("Beta", state.path()).await;
    assert_ne!(h.snapshot().folders[0].state, FolderState::Error);
    assert!(!moved.exists() && !journal.exists());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_unavailable_list_never_waits_for_a_folders_core() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.test_lock_core(&id, Duration::from_secs(2));
    let t = Instant::now();
    assert!(b.h.unavailable(&id).unwrap().is_empty());
    assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// Detaches a disk image mounted for a test, also when the test fails.
#[cfg(target_os = "macos")]
struct Mounted(std::path::PathBuf);

#[cfg(target_os = "macos")]
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil").arg("detach").arg(&self.0).arg("-force").output();
    }
}

/// A case-sensitive APFS volume under `work` (a sparse image, mounted there), or `None` where this system cannot make
/// one (a sandbox without hdiutil).
#[cfg(target_os = "macos")]
fn case_sensitive_volume(work: &std::path::Path) -> Option<Mounted> {
    volume(work, "Case-sensitive APFS", "cs")
}

/// A volume of the file system `fs` (as `hdiutil create -fs` names it) under `work`, mounted at `work/<name>`.
#[cfg(target_os = "macos")]
fn volume(work: &std::path::Path, fs: &str, name: &str) -> Option<Mounted> {
    let (img, at) = (work.join(format!("{name}-image")), work.join(name));
    let Ok(made) = std::process::Command::new("hdiutil")
        .args(["create", "-size", "64m", "-fs", fs, "-type", "SPARSE", "-volname", "SeedwellTest"])
        .arg(&img)
        .output()
    else {
        eprintln!("SKIPPED (no disk image here): hdiutil cannot run");
        return None;
    };
    if !made.status.success() {
        eprintln!("SKIPPED (no disk image here): hdiutil create failed: {}", String::from_utf8_lossy(&made.stderr));
        return None;
    }
    std::fs::create_dir_all(&at).ok()?;
    let attached = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-mountpoint"])
        .arg(&at)
        .arg(img.with_extension("sparseimage"))
        .output()
        .ok()?;
    if !attached.status.success() {
        eprintln!("SKIPPED (no disk image here): hdiutil attach failed: {}", String::from_utf8_lossy(&attached.stderr));
        return None;
    }
    Some(Mounted(at))
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_onto_a_volume_that_folds_case_never_merges_two_files_into_one() {
    // Review 14, #7 (data loss on master): a folder on a case-sensitive volume holding Notes.txt and notes.txt, moved
    // to this Mac's case-insensitive volume, would copy one over the other and delete the original.
    let work = tempfile::tempdir().unwrap();
    let Some(cs) = case_sensitive_volume(work.path()) else { return };
    let src = cs.0.join("Notes folder");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Notes.txt"), b"upper").unwrap();
    std::fs::write(src.join("notes.txt"), b"lower").unwrap();
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let id: FolderId = a.h.add_folder(src.clone()).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let bp = cs.0.join("Beta's");
    b.h.join_folder(&link, bp.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B has both", || bp.join("Notes.txt").exists() && bp.join("notes.txt").exists()).await;

    // Refused, naming both; nothing deleted, nothing left at the target.
    let to = work.path().join("Moved");
    let e = a.h.move_folder(&id, to.clone()).await.unwrap_err().to_string();
    assert!(e.contains("Notes.txt") && e.contains("notes.txt"), "{e}");
    assert_eq!(read(&src.join("Notes.txt")).as_deref(), Some(&b"upper"[..]));
    assert_eq!(read(&src.join("notes.txt")).as_deref(), Some(&b"lower"[..]));
    assert!(!to.exists());
    // And the folder syncs on where it is.
    change(&a.h, &id, &src, "after.txt");
    until(30, "a change after it reaches B", || bp.join("after.txt").exists()).await;

    // The other way (onto the case-sensitive volume) is fine.
    let plain = work.path().join("Plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("a.txt"), b"a").unwrap();
    let pid: FolderId = a.h.add_folder(plain.clone()).await.unwrap().id.parse().unwrap();
    let there = cs.0.join("Plain moved");
    a.h.move_folder(&pid, there.clone()).await.unwrap();
    assert_eq!(read(&there.join("a.txt")).as_deref(), Some(&b"a"[..]));
    assert!(!plain.exists());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_onto_a_volume_that_stores_names_in_another_form_works() {
    // Review 15, #1: HFS+ keeps names in NFD; a folder with "Måke.txt" (NFC) moved there is copied whole and moves.
    let work = tempfile::tempdir().unwrap();
    let Some(hfs) = volume(work.path(), "HFS+", "hfs") else { return };
    let src = work.path().join("Forms");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("M\u{e5}ke.txt"), b"form").unwrap();
    let a = device("Alpha").await;
    let id: FolderId = a.h.add_folder(src.clone()).await.unwrap().id.parse().unwrap();
    let to = hfs.0.join("Forms moved");
    a.h.move_folder(&id, to.clone()).await.unwrap();
    let names: Vec<String> = std::fs::read_dir(&to).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(std::fs::read(to.join(&names[0])).unwrap(), b"form");
    assert!(!src.exists());
    a.h.shutdown().await;
}

/// What the host was asked (the Mac's File Provider: its domain goes before a folder is removed or switched to full).
#[derive(Default)]
struct Hook {
    calls: std::sync::Mutex<Vec<String>>,
    refuse: std::sync::atomic::AtomicBool,
}

impl trs_sync_net::OnDemandHost for Hook {
    fn release(&self, folder: &str, name: &str, place: &std::path::Path) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("release {folder} {name} {}", place.display()));
        if self.refuse.load(std::sync::atomic::Ordering::Relaxed) { Err("the Finder said no".into()) } else { Ok(()) }
    }
    fn settled(&self, folder: &str) {
        self.calls.lock().unwrap().push(format!("settled {folder}"));
    }
}

/// Whoever asks (the app, the control socket, an agent), an on-demand folder is let go of by its host before it is
/// removed or switched to full, after the checks that need no host; a host that cannot refuses it, and nothing
/// changes. Once over, the host hears so. With no host (a headless daemon), it goes on as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_host_lets_go_of_a_folder_before_it_is_removed_or_switched() {
    let (a, b, _fa, fb, id) = pair().await;
    let hook = Arc::new(Hook::default());
    b.h.set_on_demand_host(Some(hook.clone()));
    let place = fb.path().join("Shared");
    // A switch the core refuses by itself (its place is not free) never asks the host.
    std::fs::create_dir_all(fb.path().join("Taken")).unwrap();
    std::fs::write(fb.path().join("Taken/x"), b"x").unwrap();
    assert!(b.h.set_on_demand(&id, false, Some(fb.path().join("Taken"))).await.is_err());
    assert!(hook.calls.lock().unwrap().is_empty(), "checked first");
    // The host cannot: nothing changes.
    hook.refuse.store(true, std::sync::atomic::Ordering::Relaxed);
    let e = b.h.remove_folder(&id).await.unwrap_err().to_string();
    assert!(e.contains("the Finder said no"), "{e}");
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().is_some(), "still on demand");
    assert!(b.h.set_on_demand(&id, false, None).await.is_err());
    assert_eq!(hook.calls.lock().unwrap().len(), 2);
    // It can: released first, settled after.
    hook.refuse.store(false, std::sync::atomic::Ordering::Relaxed);
    hook.calls.lock().unwrap().clear();
    let back = fb.path().join("Back");
    b.h.set_on_demand(&id, false, Some(back.clone())).await.unwrap();
    let calls = hook.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with(&format!("release {id} Shared {}", place.display())), "{calls:?}");
    assert_eq!(calls[1], format!("settled {id}"));
    // A full folder is removed without asking.
    hook.calls.lock().unwrap().clear();
    b.h.remove_folder(&id).await.unwrap();
    assert!(hook.calls.lock().unwrap().is_empty());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A folder's version for the host changes whenever anything below it does, and a delete of the folder carrying the
/// version the host saw is refused when something below changed since (on another device): nothing is deleted then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_changed_below_since_it_was_seen_is_not_deleted() {
    let (a, b, fa, _fb, id) = pair().await;
    let seen = b.h.od_newest_below(&id, "docs").unwrap();
    let before = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    // Another device changes a file below.
    std::fs::write(fa.path().join("docs/a.txt"), b"alpha, changed").unwrap();
    a.h.rescan(&id).unwrap();
    until(30, "B has the new version", || b.h.od_item_at(&id, "docs/a.txt").ok().flatten().is_some_and(|i| i.content_version != before)).await;
    let now = b.h.od_newest_below(&id, "docs").unwrap();
    assert!(now > seen, "the folder's version moved");
    let e = b.h.od_delete_seen(&id, "docs", Some(trs_sync_net::DeleteSeen::Folder(seen))).await.unwrap_err();
    assert!(matches!(e, trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::Changed, _)), "{e}");
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().is_some(), "nothing deleted");
    // Seen as it is now: deleted.
    b.h.od_delete_seen(&id, "docs", Some(trs_sync_net::DeleteSeen::Folder(now))).await.unwrap();
    assert!(b.h.od_item_at(&id, "docs").unwrap().is_none());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// Whether a folder is configured (and on demand) is read from the configuration, whether it runs or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_is_configured_until_it_is_removed() {
    let (a, b, _fa, _fb, id) = pair().await;
    assert_eq!(b.h.configured(&id), Some(true));
    b.h.remove_folder(&id).await.unwrap();
    assert_eq!(b.h.configured(&id), None);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A folder of this device added on demand is never opened full first: its files go into the cache before it opens
/// (so no scan holds it busy), and when they cannot, nothing is added and nothing is left behind.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_added_on_demand_is_added_whole_or_not_at_all() {
    use std::os::unix::fs::PermissionsExt;
    let a = device("Alpha").await;
    let work = tempfile::tempdir().unwrap();
    let locked = work.path().join("Locked");
    let docs = locked.join("Docs");
    std::fs::create_dir_all(docs.join("sub")).unwrap();
    std::fs::write(docs.join("sub/a.txt"), b"mine").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = a.h.add_folder_on_demand(docs.clone()).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_err());
    assert!(a.h.snapshot().folders.is_empty(), "nothing added");
    assert_eq!(read(&docs.join("sub/a.txt")).as_deref(), Some(&b"mine"[..]));
    assert!(!docs.join(".seedwell").exists(), "nothing left behind");
    // Tried again: on demand at once, every file here.
    let st = a.h.add_folder_on_demand(docs.clone()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    assert_eq!(a.h.configured(&id), Some(true));
    assert!(!docs.exists(), "the files are the cache now");
    until(10, "listed", || a.h.od_item_at(&id, "sub/a.txt").ok().flatten().is_some_and(|i| i.state != ItemState::OnlineOnly)).await;
    a.h.shutdown().await;
}

/// Only another device's change below a folder refuses its delete: this device's own (a file deleted first, as
/// `rm -rf` does, a temporary folder made, filled and deleted) never does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn this_devices_own_changes_below_a_folder_do_not_refuse_its_delete() {
    let (a, b, _fa, fb, id) = pair().await;
    let seen = b.h.od_newest_below(&id, "docs").unwrap();
    b.h.od_delete(&id, "docs/a.txt").await.unwrap();
    b.h.od_mkdir(&id, "docs/tmp").await.unwrap();
    let src = fb.path().join("t.bin");
    std::fs::write(&src, b"temp").unwrap();
    b.h.od_write(&id, "docs/tmp/t.bin", &src, None).await.unwrap();
    b.h.od_delete(&id, "docs/tmp").await.unwrap();
    assert!(b.h.od_newest_below(&id, "docs").unwrap() > seen, "the folder changed, by this device");
    b.h.od_delete_seen(&id, "docs", Some(trs_sync_net::DeleteSeen::Folder(seen))).await.unwrap();
    assert!(b.h.od_item_at(&id, "docs").unwrap().is_none());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A file's delete carries the content version the user saw, checked on the folder's core with the delete: a newer
/// version from another device is not deleted unseen, and nothing can land between the check and the delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_since_it_was_seen_is_not_deleted() {
    let (a, b, fa, _fb, id) = pair().await;
    let before = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    std::fs::write(fa.path().join("docs/a.txt"), b"alpha, changed").unwrap();
    a.h.rescan(&id).unwrap();
    until(30, "B has the new version", || b.h.od_item_at(&id, "docs/a.txt").ok().flatten().is_some_and(|i| i.content_version != before)).await;
    let e = b.h.od_delete_seen(&id, "docs/a.txt", Some(trs_sync_net::DeleteSeen::File(before))).await.unwrap_err();
    assert!(matches!(e, trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::Changed, _)), "{e}");
    let now = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    b.h.od_delete_seen(&id, "docs/a.txt", Some(trs_sync_net::DeleteSeen::File(now))).await.unwrap();
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().is_none());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// The host's stand-in for Cloud Files filling a placeholder (`CfExecute` TRANSFER_DATA): the fetched content copied
/// into the file at the folder's place, then the core told (as the Windows host does after the last transfer).
async fn fill_placeholder(h: &SyncHandle, id: &FolderId, place: &std::path::Path, path: &str) -> String {
    let cache = h.od_fetch(id, path, Duration::from_secs(30)).await.unwrap().path;
    h.od_hand(id, path);
    let to = place.join(path);
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(&cache, &to).unwrap();
    let cv = h.od_item_at(id, path).unwrap().unwrap().content_version;
    h.od_hydrated(id, path, &cv).await.unwrap();
    cv
}

/// A folder kept in place (Windows' Cloud Files): a file's content is stored once, in its placeholder. Fetched, it
/// moves from the cache into the placeholder; another device is served from there; freed, it goes; fetched again, it
/// comes back; the user's edit in the placeholder is told from an earlier version, reported and filled in again; and
/// switched to full, the folder takes everything back without losing a byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_kept_in_place_stores_each_file_once_in_its_placeholder() {
    let (a, b, fa, fb, id) = pair().await;
    // A third device, on demand too, joins while the owner is there.
    let c = device("Gamma").await;
    let fc = tempfile::tempdir().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    c.h.join_folder_on_demand(&link, fc.path().join("Shared")).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    until(30, "C lists the files", || c.h.od_item_at(&id, "docs/a.txt").ok().flatten().is_some()).await;

    // The host keeps B's content in place: its sync root is the folder's place.
    let place = fb.path().join("Shared");
    std::fs::create_dir_all(&place).unwrap();
    assert!(!b.h.od_in_place(&id).unwrap());
    b.h.od_set_in_place(&id, true).await.unwrap();
    assert!(b.h.od_in_place(&id).unwrap());
    until(30, "B runs again", || b.h.od_item_at(&id, "docs/a.txt").is_ok()).await;

    // Fetched and filled: the placeholder holds it, the cache does not.
    let cv = fill_placeholder(&b.h, &id, &place, "docs/a.txt").await;
    let cached = b.h.od_cache_path(&id, "docs/a.txt").unwrap();
    assert!(!cached.exists(), "stored once");
    assert!(b.h.od_held_in_place(&id, "docs/a.txt"));
    assert_eq!(read(&place.join("docs/a.txt")).as_deref(), Some(&b"alpha"[..]));
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::Downloaded);
    assert_eq!(b.h.od_local_change(&id, "docs/a.txt").await.unwrap(), trs_sync_net::LocalChange::Unchanged);
    // Told again: nothing more to do.
    b.h.od_hydrated(&id, "docs/a.txt", &cv).await.unwrap();

    // Served from the placeholder: with A gone, B is the only device that has it.
    a.h.shutdown().await;
    until(30, "B and C connected, A gone", || b.h.snapshot().devices.len() == 1 && c.h.snapshot().devices.len() == 1).await;
    let at = c.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap().path;
    assert_eq!(read(&at).as_deref(), Some(&b"alpha"[..]));
    assert!(!cached.exists(), "B served it from its placeholder");
    c.h.shutdown().await;

    // A back: B may free it. The note stays (the host dehydrates the placeholder: here it goes).
    let ha = device_at("Alpha", a._state.path()).await;
    b.h.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ha.port())).unwrap();
    until(60, "B frees it", || futures_now(b.h.od_evict(&id, "docs/a.txt")).files == 1).await;
    assert!(!b.h.od_held_in_place(&id, "docs/a.txt"));
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::OnlineOnly);
    assert_eq!(b.h.od_local_change(&id, "docs/a.txt").await.unwrap(), trs_sync_net::LocalChange::Stale, "let go of: the host may dehydrate it");
    std::fs::remove_file(place.join("docs/a.txt")).unwrap();

    // Fetched again, filled again.
    fill_placeholder(&b.h, &id, &place, "docs/a.txt").await;
    assert!(b.h.od_held_in_place(&id, "docs/a.txt") && !cached.exists());

    // The user edits the placeholder: a change, from the version it held, reported and filled in again.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(place.join("docs/a.txt"), b"alpha, edited on B").unwrap();
    let base = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().content_version;
    let change = b.h.od_local_change(&id, "docs/a.txt").await.unwrap();
    assert_eq!(change, trs_sync_net::LocalChange::Modified { base: Some(base.clone()) });
    b.h.od_write(&id, "docs/a.txt", &place.join("docs/a.txt"), Some(&base)).await.unwrap();
    let edited = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap();
    assert!(edited.content_version != base && edited.state == ItemState::ModifiedHere, "{edited:?}");
    assert!(cached.exists(), "the change is the cache's until the placeholder is filled with it");
    b.h.od_hydrated(&id, "docs/a.txt", &edited.content_version).await.unwrap();
    assert!(!cached.exists() && b.h.od_held_in_place(&id, "docs/a.txt"));
    until(30, "A has B's edit", || read(&fa.path().join("docs/a.txt")).as_deref() == Some(&b"alpha, edited on B"[..])).await;

    // A newer version from A: B fetches it (a downloaded file stays current) into its cache; the placeholder holds an
    // earlier one, unchanged since: stale, for the host to fill anew.
    std::fs::write(fa.path().join("docs/a.txt"), b"alpha, then A again").unwrap();
    ha.rescan(&id).unwrap();
    // (The file is in the cache a moment before its version is recorded: until then the placeholder still holds the
    // current content.)
    until(30, "B has A's version, recorded", || {
        b.h.od_cache_path(&id, "docs/a.txt").is_ok_and(|p| read(&p).as_deref() == Some(&b"alpha, then A again"[..]))
            && b.h.od_item_at(&id, "docs/a.txt").ok().flatten().is_some_and(|i| i.content_version != edited.content_version)
    })
    .await;
    assert!(!b.h.od_held_in_place(&id, "docs/a.txt"));
    assert_eq!(b.h.od_local_change(&id, "docs/a.txt").await.unwrap(), trs_sync_net::LocalChange::Stale);
    std::fs::remove_file(place.join("docs/a.txt")).unwrap();
    fill_placeholder(&b.h, &id, &place, "docs/a.txt").await;

    // The user renames it: the host reports it before the placeholder moves; the content stays where it is, once.
    b.h.od_rename(&id, "docs/a.txt", "docs/b.txt").await.unwrap();
    assert!(b.h.od_held_in_place(&id, "docs/b.txt") && !b.h.od_cache_path(&id, "docs/b.txt").unwrap().exists());
    std::fs::rename(place.join("docs/a.txt"), place.join("docs/b.txt")).unwrap();
    assert_eq!(b.h.od_local_change(&id, "docs/b.txt").await.unwrap(), trs_sync_net::LocalChange::Unchanged);
    until(30, "A took the rename", || read(&fa.path().join("docs/b.txt")).as_deref() == Some(&b"alpha, then A again"[..])).await;

    // Switched to full: the content comes back from the placeholders (the host let go of them), and the place is the
    // folder's.
    b.h.set_on_demand(&id, false, None).await.unwrap();
    until(30, "B is full at its place", || read(&place.join("docs/b.txt")).as_deref() == Some(&b"alpha, then A again"[..]) && place.join("big.bin").exists()).await;
    assert!(!place.join("docs/a.txt").exists());
    assert!(b.h.od_in_place(&id).is_err(), "not on demand any more");
    ha.shutdown().await;
    b.h.shutdown().await;
}

/// The host's change-feed anchor is kept with the folder and goes with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_anchor_is_kept_with_the_folder() {
    let (a, b, _fa, _fb, id) = pair().await;
    assert_eq!(b.h.od_host_anchor(&id).unwrap(), None);
    let now = b.h.od_anchor(&id).unwrap();
    b.h.od_set_host_anchor(&id, now).unwrap();
    assert_eq!(b.h.od_host_anchor(&id).unwrap(), Some(now));
    b.h.od_set_host_anchor(&id, u64::MAX).unwrap();
    assert_eq!(b.h.od_host_anchor(&id).unwrap(), Some(u64::MAX), "all 64 bits");
    // Only an on-demand folder's.
    let full: FolderId = a.h.snapshot().folders[0].id.parse().unwrap();
    assert!(a.h.od_set_host_anchor(&full, 1).is_err());
    b.h.remove_folder(&id).await.unwrap();
    assert!(b.h.od_host_anchor(&id).is_err(), "gone with the folder");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

fn not_here_yet(e: &trs_sync_net::SyncError) -> bool {
    matches!(e, trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::NotHereYet, _))
}

/// A fetch says itself when it cannot get further (SPEC §7G.11): a file only an offline device has fails at once past
/// the grace, its request taken back; one asked for while sessions may be coming up waits, and comes when the device
/// that has it connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_only_an_offline_device_can_serve_fails_at_once_and_one_waits_while_it_connects() {
    let (a, b, _fa, _fb, id) = pair().await;
    let Device { h: ha, _state: state_a } = a;
    ha.shutdown().await;
    until(30, "B sees A gone", || b.h.transports().is_empty()).await;
    b.h.test_fetch_times(Duration::from_millis(300), Duration::from_secs(30));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let t = Instant::now();
    let e = b.h.od_fetch_wait(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap_err();
    assert!(not_here_yet(&e), "{e}");
    assert!(t.elapsed() < Duration::from_secs(3), "at once, not after the wait: {:?}", t.elapsed());
    until(10, "the request is taken back", || b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state == ItemState::OnlineOnly).await;
    // Asked again (as a file manager does): the same at once.
    let t = Instant::now();
    assert!(not_here_yet(&b.h.od_fetch_wait(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap_err()));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());

    // Within the grace (just started, the network changed, a session began) it waits, and A coming up brings it.
    b.h.test_fetch_times(Duration::from_secs(30), Duration::from_secs(30));
    let (bh, fid) = (b.h.clone(), id);
    let fetch = tokio::spawn(async move { bh.od_fetch_wait(&fid, "docs/a.txt", Duration::from_secs(60)).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    if fetch.is_finished() {
        panic!("waits while devices may be coming up: {:?}", fetch.await.unwrap().map(|_| ()));
    }
    let ha = crate::chat::start_at(state_a.path(), "Alpha", |_| {}).await;
    b.h.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ha.port())).unwrap();
    match fetch.await.unwrap().unwrap() {
        trs_sync_net::service::FetchWait::Here(f) => assert_eq!(read(&f.path).as_deref(), Some(&b"alpha"[..])),
        other => panic!("not here: {other:?}"),
    }
    ha.shutdown().await;
    b.h.shutdown().await;
}

/// A fetch from a connected device that has the file answers how far it is and from whom while it waits, until it is
/// here; with no grace at all, the connected device alone keeps it going. While its request is still on its way to
/// the folder's core it comes from nobody named yet; once the request stands, from the device that has it. (0.1.18:
/// on Windows' CI the request outlasted the 100 ms wait, its commit a flush on a slow disk, and the test expected a
/// name at once. Here a pull round holds the core first, so every system sees the request wait.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_from_a_connected_device_says_it_is_coming_until_it_is_here() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.test_fetch_times(Duration::ZERO, Duration::from_secs(30));
    // Each round of the pull starts late, so the fetch is seen on its way.
    b.h.test_fetch_wait(Duration::from_millis(300));
    // A round for another file holds the folder's core (for its 300 ms and more) when the fetch is asked for.
    b.h.od_request(&id, "docs/a.txt").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (mut unnamed, mut named) = (0, 0);
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < end, "not here after 60 s");
        match b.h.od_fetch_wait(&id, "big.bin", Duration::from_millis(100)).await.unwrap() {
            trs_sync_net::service::FetchWait::Here(f) => {
                assert_eq!(read(&f.path).map(|d| d.len()), Some(2_000_000));
                break;
            }
            trs_sync_net::service::FetchWait::Coming { from, .. } if from.is_empty() => {
                assert_eq!(named, 0, "once named, always named while it comes");
                unnamed += 1;
            }
            trs_sync_net::service::FetchWait::Coming { from, .. } => {
                assert_eq!(from, ["Alpha"]);
                named += 1;
            }
        }
    }
    assert!(named > 0, "seen on its way, from Alpha (unnamed first: {unnamed})");
    assert_eq!(b.h.od_item_at(&id, "big.bin").unwrap().unwrap().state, ItemState::Downloaded);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A fetch the connected device that has the file sends nothing of gives up after the stall, its request taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_that_gets_none_of_the_file_gives_up() {
    let (a, b, _fa, _fb, id) = pair().await;
    b.h.test_fetch_times(Duration::ZERO, Duration::from_millis(500));
    // The pull holds back longer than the stall: nothing comes.
    b.h.test_fetch_wait(Duration::from_secs(3));
    let t = Instant::now();
    let e = b.h.od_fetch_wait(&id, "big.bin", Duration::from_secs(30)).await.unwrap_err();
    assert!(not_here_yet(&e), "{e}");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    b.h.test_fetch_wait(Duration::ZERO);
    // (The round under way may still bring it before the request is taken back: either way it is no longer on its way.)
    until(30, "the request is taken back", || b.h.od_item_at(&id, "big.bin").unwrap().unwrap().state != ItemState::Downloading).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// `len` bytes no two chunks of which are alike (each chunk is fetched, none found here already).
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// An on-demand device B joined to A's folder holding `slow.bin` (`len` bytes of noise), B's download limited to
/// `down_kib` KiB/s on the LAN (as `SEEDWELL_QA_LIMIT_LAN` does in a QA run).
async fn slow_pair(len: usize, down_kib: u32) -> (Device, Device, tempfile::TempDir, tempfile::TempDir, FolderId, Vec<u8>) {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let data = noise(len, 58);
    std::fs::write(fa.path().join("slow.bin"), &data).unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    b.h.test_limit_lan(true);
    b.h.set_settings(trs_sync_net::config::Settings { down_kib, ..b.h.settings() }).unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    b.h.join_folder_on_demand(&link, fb.path().join("Shared")).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B lists slow.bin", || b.h.od_item_at(&id, "slow.bin").ok().flatten().is_some()).await;
    (a, b, fa, fb, id, data)
}

/// A fetch over a slow link (Windows build 58: 3 MB at 500 KB/s): each wait returns within its time while the pull
/// downloads the file, and how far the file is rises as it comes, chunk by chunk. (Build 58: the second wait took
/// the folder's core, which the pull holds for the whole file, so it returned only with the file; and the partial file
/// grew only once the whole window had come.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_fetch_answers_each_wait_in_time_and_shows_how_far_it_is() {
    const LEN: usize = 1_500_000;
    // 256 KiB/s: about 6 s for the file.
    let (a, b, _fa, _fb, id, data) = slow_pair(LEN, 256).await;
    let wait = Duration::from_millis(250);
    let mut done = Vec::new();
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < end, "not here after 60 s: {done:?}");
        let t = Instant::now();
        let got = b.h.od_fetch_wait(&id, "slow.bin", wait).await.unwrap();
        let took = t.elapsed();
        // A little slack for a slow test machine; build 58 took the rest of the file (seconds).
        assert!(took < wait + Duration::from_millis(1500), "a wait of {wait:?} took {took:?} (done so far {done:?})");
        let p = b.h.od_progress(&id, "slow.bin").unwrap();
        assert_eq!(p.total, LEN as u64);
        match got {
            trs_sync_net::service::FetchWait::Here(f) => {
                assert!(p.here && p.done == LEN as u64, "{p:?}");
                assert!(read(&f.path) == Some(data.clone()), "the file is whole and the same");
                break;
            }
            trs_sync_net::service::FetchWait::Coming { .. } => done.push(p.done),
        }
    }
    assert!(done.windows(2).all(|w| w[0] <= w[1]), "never goes back: {done:?}");
    let partial: std::collections::BTreeSet<u64> = done.iter().copied().filter(|&d| d > 0 && d < LEN as u64).collect();
    assert!(partial.len() >= 2, "how far it is rises through partial values: {done:?}");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// The pull's last step: the whole copy renamed into place, its record committed only at the end of the round (a
/// flush, long on a slow disk). Meanwhile how far it is says the whole file, never the 0 of a partial copy that is
/// gone (0.1.18: the slow fetch's last value fell to 0 on Windows' CI).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_renamed_into_place_but_not_recorded_yet_counts_as_whole() {
    let (a, b, _fa, _fb, id) = pair().await;
    assert_eq!(b.h.od_progress(&id, "big.bin").unwrap().done, 0, "nothing of it here");
    let cached = b.h.od_cache_path(&id, "big.bin").unwrap();
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, vec![5u8; 2_000_000]).unwrap();
    let p = b.h.od_progress(&id, "big.bin").unwrap();
    assert_eq!((p.done, p.total), (2_000_000, 2_000_000), "{p:?}");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A fetch whose request finds the folder's core busy (a pull round that downloads, minutes on a slow link) still
/// answers within its wait, and waits on the request without giving up; the request goes in once the core is free. A
/// cancel meanwhile keeps it from going in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fetch_while_the_core_is_busy_answers_in_time_and_asks_once_it_is_free() {
    let (a, b, _fa, _fb, id) = pair().await;
    // No grace and a short stall: a request still waiting for the core must not be taken for a fetch that cannot get
    // further.
    b.h.test_fetch_times(Duration::ZERO, Duration::from_millis(500));
    let wait = Duration::from_millis(300);
    let coming = |r: &trs_sync_net::service::FetchWait| matches!(r, trs_sync_net::service::FetchWait::Coming { .. });

    // Cancelled while its request waits for the core: never asked for.
    b.h.test_lock_core(&id, Duration::from_secs(2));
    let t = Instant::now();
    assert!(coming(&b.h.od_fetch_wait(&id, "docs/a.txt", wait).await.unwrap()));
    assert!(t.elapsed() < wait + Duration::from_millis(1500), "{:?}", t.elapsed());
    b.h.od_cancel(&id, "docs/a.txt").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().state, ItemState::OnlineOnly, "not asked for after the cancel");

    // The core busy for 3 s: each wait answers in time, none gives up, and the file comes once the core is free.
    b.h.test_lock_core(&id, Duration::from_secs(3));
    let end = Instant::now() + Duration::from_secs(30);
    let mut waits = 0;
    loop {
        assert!(Instant::now() < end, "not here after 30 s");
        let t = Instant::now();
        let r = b.h.od_fetch_wait(&id, "big.bin", wait).await.unwrap();
        assert!(t.elapsed() < wait + Duration::from_millis(1500), "{:?}", t.elapsed());
        waits += 1;
        if !coming(&r) {
            break;
        }
    }
    assert!(waits >= 3, "waited while the core was busy: {waits}");
    assert_eq!(b.h.od_item_at(&id, "big.bin").unwrap().unwrap().state, ItemState::Downloaded);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// Watches `path` every 2 ms on its own thread until dropped: whether it was ever missing meanwhile.
struct Watch {
    stop: Arc<std::sync::atomic::AtomicBool>,
    missed: Arc<std::sync::atomic::AtomicBool>,
}

impl Watch {
    fn new(path: std::path::PathBuf) -> Watch {
        let (stop, missed) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let (s, m) = (stop.clone(), missed.clone());
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut was = true;
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                let is = path.exists();
                if is != was {
                    eprintln!("DIAG watch {} at {} ms", if is { "back" } else { "missing" }, t0.elapsed().as_millis());
                    was = is;
                }
                if !is {
                    m.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Watch { stop, missed }
    }

    fn missed(&self) -> bool {
        self.missed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A save by replace as Windows' host reports it (SPEC §7G.11): the document's delete (NOTIFY_DELETE), the program's
/// temp file (refused: a name the folder does not sync), then, once the host's watcher found the new file closed (it
/// looks every half second), the new file at the document's name. The item keeps its identifier, and the full device
/// sees one edit: its file is never missing meanwhile (the delete waited to be announced) and no conflict copy is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_by_replace_reaches_the_other_device_as_one_edit() {
    let (a, b, fa, fb, id) = pair().await;
    let doc = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap();
    let watch = Watch::new(fa.path().join("docs/a.txt"));
    b.h.od_delete(&id, "docs/a.txt").await.unwrap();
    assert!(b.h.od_replace_hold(&id).is_some(), "announcing waits for a save");
    let src = fb.path().join("saved.txt");
    std::fs::write(&src, b"alpha, saved by replace").unwrap();
    let temp = b.h.od_write(&id, "docs/~WRD0001.tmp", &src, None).await;
    assert!(matches!(temp, Err(trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::Ignored, _))), "{temp:?}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(b.h.od_write(&id, "docs/a.txt", &src, None).await.unwrap(), "docs/a.txt");
    let now = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap();
    assert_eq!(now.id, doc.id, "the same item");
    assert!(b.h.od_replace_hold(&id).is_none());
    until(30, "A has the saved version", || read(&fa.path().join("docs/a.txt")).as_deref() == Some(&b"alpha, saved by replace"[..])).await;
    assert!(!watch.missed(), "A's file was never deleted meanwhile");
    assert!(b.h.od_item_at(&id, "docs/~WRD0001.tmp").unwrap().is_none());
    assert!(!fa.path().join("docs/~WRD0001.tmp").exists());
    let names: Vec<String> = std::fs::read_dir(fa.path().join("docs")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("conflict")), "{names:?}");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// Word's own save, as the hosts report it: the document renamed aside to `~WRL0001.tmp` (it leaves the folder; its
/// content is here, so the core takes it out at once), the new document at its name (the same item, changed), then the
/// Mac's delete of the item it excluded, as it saw it (letting go of that file: the saved document stays). The full
/// device sees one edit. A delete with nothing after it reaches the other device once the window is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn words_save_and_a_plain_delete_reach_the_other_device_as_they_should() {
    let (a, b, fa, fb, id) = pair().await;
    b.h.od_fetch(&id, "docs/a.txt", Duration::from_secs(30)).await.unwrap();
    let doc = b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap();
    let watch = Watch::new(fa.path().join("docs/a.txt"));
    let aside = b.h.od_rename(&id, "docs/a.txt", "docs/~WRL0001.tmp").await;
    assert!(matches!(&aside, Err(trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::Ignored, _))), "{aside:?}");
    assert!(b.h.od_item_at(&id, "docs/a.txt").unwrap().is_none(), "it left the folder");
    // An online-only file renamed so: nothing changes until its content is here.
    let far = b.h.od_rename(&id, "docs/old/c.txt", "docs/old/~c.tmp").await;
    assert!(matches!(&far, Err(trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::IgnoredNotHere, _))), "{far:?}");
    assert!(b.h.od_item_at(&id, "docs/old/c.txt").unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(600)).await;
    let src = fb.path().join("saved.txt");
    std::fs::write(&src, b"alpha, saved by Word").unwrap();
    assert_eq!(b.h.od_write(&id, "docs/a.txt", &src, None).await.unwrap(), "docs/a.txt");
    assert_eq!(b.h.od_item_at(&id, "docs/a.txt").unwrap().unwrap().id, doc.id);
    let gone = b.h.od_delete_seen(&id, "docs/a.txt", Some(trs_sync_net::DeleteSeen::File(doc.content_version.clone()))).await.unwrap();
    assert_eq!(gone, trs_sync::ondemand::Deleted::Deleted { items: 0, failed: vec![] }, "the file that left, let go of");
    until(30, "A has the saved version", || read(&fa.path().join("docs/a.txt")).as_deref() == Some(&b"alpha, saved by Word"[..])).await;
    assert!(!watch.missed(), "A's file was never deleted meanwhile");
    drop(watch);

    // A delete with nothing after it: announced once the window is over.
    let t0 = Instant::now();
    b.h.od_delete(&id, "big.bin").await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(fa.path().join("big.bin").exists(), "not yet: it may still be a save");
    until(30, "A deleted big.bin", || !fa.path().join("big.bin").exists()).await;
    assert!(t0.elapsed() >= trs_sync::ondemand::REPLACE_WINDOW, "{:?}", t0.elapsed());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A delete made while a session is already announcing (a slow link: it was woken by an earlier change and is still at
/// its batches) waits for the save as well: the full device still sees one edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_made_while_a_session_announces_still_waits_for_the_save() {
    let (a, b, fa, fb, id) = pair().await;
    b.h.test_announce_pause(&id, Duration::from_millis(500));
    let watch = Watch::new(fa.path().join("docs/a.txt"));
    // A change wakes B's session, which goes on to announce after its pause.
    b.h.od_mkdir(&id, "made").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.h.od_delete(&id, "docs/a.txt").await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let src = fb.path().join("saved.txt");
    std::fs::write(&src, b"alpha, saved while announcing").unwrap();
    assert_eq!(b.h.od_write(&id, "docs/a.txt", &src, None).await.unwrap(), "docs/a.txt");
    until(30, "A has the saved version", || read(&fa.path().join("docs/a.txt")).as_deref() == Some(&b"alpha, saved while announcing"[..])).await;
    until(30, "A has the new folder", || fa.path().join("made").is_dir()).await;
    assert!(!watch.missed(), "A's file was never deleted meanwhile");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A big document saved by replace: the new file takes a while to read in (hashed on the folder's core), and the delete
/// keeps waiting all that time. It stops waiting only once the new record is stored, so the full device never sees
/// the file missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_big_document_saved_by_replace_reaches_the_other_device_as_one_edit() {
    let (a, b, fa, fb, id) = pair().await;
    let doc = b.h.od_item_at(&id, "big.bin").unwrap().unwrap();
    let watch = Watch::new(fa.path().join("big.bin"));
    b.h.od_delete(&id, "big.bin").await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let src = fb.path().join("saved.bin");
    let big: Vec<u8> = (0..48_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(&src, &big).unwrap();
    assert_eq!(b.h.od_write(&id, "big.bin", &src, None).await.unwrap(), "big.bin");
    assert_eq!(b.h.od_item_at(&id, "big.bin").unwrap().unwrap().id, doc.id, "the same item");
    until(60, "A has the saved version", || std::fs::metadata(fa.path().join("big.bin")).is_ok_and(|m| m.len() == big.len() as u64)).await;
    assert!(!watch.missed(), "A's file was never deleted meanwhile");
    a.h.shutdown().await;
    b.h.shutdown().await;
}
