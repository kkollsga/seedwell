//! Watch folders (SPEC 7A.1): pick up `.torrent` files once their size is stable, add or ask,
//! then trash, archive or leave the file.
use crate::common;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use trs_engine::config::{AfterAdd, DetectionConfig, WatchConfig, WatchMode};
use trs_engine::*;

struct Rig {
    n: SNode,
    watch: PathBuf,
    trash: PathBuf,
    lib: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

async fn rig(mode: WatchMode, after: impl FnOnce(&Path, &Path) -> AfterAdd, category: Option<Category>) -> Rig {
    rig_pre(mode, after, category, |_| {}).await
}

/// `pre` runs on the watch folder before the engine starts (files that were already there).
async fn rig_pre(
    mode: WatchMode,
    after: impl FnOnce(&Path, &Path) -> AfterAdd,
    category: Option<Category>,
    pre: impl FnOnce(&Path),
) -> Rig {
    let dirs: Vec<tempfile::TempDir> = (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let p = |i: usize| dirs[i].path().to_path_buf();
    let (watch, trash, lib, dl, state, inc) = (p(0), p(1), p(2), p(3), p(4), p(5));
    let after = after(&watch, &trash);
    pre(&watch);
    // The baseline is "now" when the engine starts; files written before must be older than it.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let (lib2, w2, t2) = (lib.clone(), watch.clone(), trash.clone());
    let n = snode(&state, &dl, None, move |c| {
        c.detect = Some(DetectionConfig::default());
        c.category_locations[Category::Book.index()].finished = Some(lib2.join("Books"));
        c.category_locations[Category::Other.index()].finished = Some(lib2.join("Other"));
        c.incomplete_dir = Some(inc.join(".incomplete"));
        c.use_staging = true;
        c.trash_dir = Some(t2);
        c.watch = vec![WatchConfig { path: w2, mode, after_add: after, category, rule: None, since: None }];
    })
    .await;
    Rig { n, watch, trash, lib, _dirs: dirs }
}

fn book(seed: u64, name: &str) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name,
        piece_len: 32 * 1024,
        files: &[(name, 120_000)],
        private: false,
        announce: None,
        seed,
    })
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_mode_adds_a_settled_file_and_moves_the_torrent_to_the_trash() {
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Trash, None).await;
    let t = book(501, "Jane Austen - Emma.epub");
    let mut ev = r.n.h.subscribe();
    // A browser download in progress: written in pieces, then renamed to its final name.
    let part = r.watch.join("Jane Austen - Emma.epub.torrent.crdownload");
    std::fs::write(&part, &t.bytes[..20]).unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(r.n.row(&t.id).await.is_none(), "a partial download is never picked up");
    std::fs::write(&part, &t.bytes).unwrap();
    let fin = r.watch.join("Jane Austen - Emma.epub.torrent");
    std::fs::rename(&part, &fin).unwrap();
    let e = wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { .. })).await;
    let EngineEvent::AutoAdded { id, name, category, destination, origin, .. } = e else { unreachable!() };
    assert_eq!((id, name.as_str(), category), (t.id, "Jane Austen - Emma.epub", Category::Book));
    assert!(destination.is_some_and(|d| d.starts_with(&r.lib)), "the notification can say where it goes");
    assert_eq!(origin, AddOrigin::Watch(fin.clone()));
    assert!(r.n.row(&t.id).await.is_some());
    wait_for("trashed", 10, || async { (!fin.exists()).then_some(()) }).await;
    assert_eq!(names(&r.trash), ["Jane Austen - Emma.epub.torrent"]);
    // The original is kept internally.
    let stored = r.n.store.load_torrent_file(&t.id).unwrap().unwrap();
    assert_eq!(&stored[..], &t.bytes[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trash_never_overwrites_and_archive_and_leave_work() {
    // Trash with a name that is taken already.
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Trash, None).await;
    std::fs::write(r.trash.join("a.torrent"), b"old").unwrap();
    let t = book(502, "Isaac Asimov - Foundation.epub");
    std::fs::write(r.watch.join("a.torrent"), &t.bytes).unwrap();
    wait_for("trashed", 15, || async { (!r.watch.join("a.torrent").exists()).then_some(()) }).await;
    assert_eq!(names(&r.trash), ["a (2).torrent", "a.torrent"]);
    assert_eq!(std::fs::read(r.trash.join("a.torrent")).unwrap(), b"old");

    // Archive.
    let r = rig(WatchMode::Auto, |_, trash| AfterAdd::Archive(trash.join("done")), None).await;
    let t = book(503, "Frank Herbert - Dune.epub");
    std::fs::write(r.watch.join("dune.torrent"), &t.bytes).unwrap();
    wait_for("archived", 15, || async { r.trash.join("done/dune.torrent").exists().then_some(()) }).await;
    assert!(!r.watch.join("dune.torrent").exists());

    // Leave: the file stays, and is not added again on the next scans.
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Leave, None).await;
    let t = book(504, "Mary Shelley - Frankenstein.epub");
    let mut ev = r.n.h.subscribe();
    std::fs::write(r.watch.join("f.torrent"), &t.bytes).unwrap();
    wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { .. })).await;
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(r.watch.join("f.torrent").exists());
    assert!(!matches!(ev.try_recv(), Ok(EngineEvent::Duplicate { .. })), "settled files are looked at once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_mode_emits_a_request_and_the_app_finishes_the_add() {
    let r = rig(WatchMode::Ask, |_, _| AfterAdd::Trash, None).await;
    let t = book(505, "Jules Verne - Nemo.epub");
    let mut ev = r.n.h.subscribe();
    let f = r.watch.join("nemo.torrent");
    std::fs::write(&f, &t.bytes).unwrap();
    let e = wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AddRequest(_))).await;
    let EngineEvent::AddRequest(req) = e else { unreachable!() };
    assert_eq!(req.reason, AskReason::FolderMode);
    assert_eq!(req.path, f);
    assert_eq!(req.preview.id, t.id);
    assert_eq!(req.preview.category, Category::Book);
    assert!(req.preview.confidence >= 0.8);
    // The add panel is the chooser: it carries the structure and says why it was opened.
    assert!(req.preview.choice.needs_choice);
    assert!(req.preview.choice.reasons.contains(&ChoiceReason::FolderAsk), "{:?}", req.preview.choice.reasons);
    assert!(f.exists(), "nothing happens to the file until the app decides");
    assert!(r.n.row(&t.id).await.is_none());
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(!matches!(ev.try_recv(), Ok(EngineEvent::AddRequest(_))), "asked once");

    // The app confirms (here with another category): added, and the file handled as configured.
    let added = r
        .n
        .h
        .add_torrent_path(&f, AddOptions { category: Some(Category::Other), paused: true, ..Default::default() }, None)
        .await
        .unwrap();
    assert_eq!(added.id, t.id);
    assert_eq!(r.n.row(&t.id).await.unwrap().category, Some(Category::Other));
    assert!(!f.exists(), "the folder's after-add action ran (trash)");
    assert_eq!(names(&r.trash), ["nemo.torrent"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsure_category_asks_even_in_auto_mode() {
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Leave, None).await;
    let t = make_torrent(&TorrentSpec {
        name: "stuff",
        piece_len: 32 * 1024,
        files: &[("a.bin", 50_000), ("b.bin", 50_000)],
        private: false,
        announce: None,
        seed: 506,
    });
    let mut ev = r.n.h.subscribe();
    std::fs::write(r.watch.join("stuff.torrent"), &t.bytes).unwrap();
    let e = wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AddRequest(_))).await;
    let EngineEvent::AddRequest(req) = e else { unreachable!() };
    assert_eq!(req.reason, AskReason::LowConfidence);
    assert!(req.preview.uncertain && req.preview.category == Category::Other);
    assert!(r.n.row(&t.id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_category_skips_the_question_and_duplicates_are_reported() {
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Leave, Some(Category::Book)).await;
    let t = make_torrent(&TorrentSpec {
        name: "stuff",
        piece_len: 32 * 1024,
        files: &[("a.bin", 50_000), ("b.bin", 50_000)],
        private: false,
        announce: None,
        seed: 507,
    });
    let mut ev = r.n.h.subscribe();
    std::fs::write(r.watch.join("one.torrent"), &t.bytes).unwrap();
    let e = wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { .. })).await;
    let EngineEvent::AutoAdded { category, .. } = e else { unreachable!() };
    assert_eq!(category, Category::Book, "the folder says books");
    // The same torrent under another name: ignored, with a notification.
    std::fs::write(r.watch.join("two.torrent"), &t.bytes).unwrap();
    wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::Duplicate { .. })).await;
    assert_eq!(r.n.h.snapshot().await.unwrap().rows.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_is_reported_once_and_other_files_are_ignored() {
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Trash, None).await;
    let mut ev = r.n.h.subscribe();
    std::fs::write(r.watch.join("bad.torrent"), b"this is not bencode").unwrap();
    let e = wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::WatchError { .. })).await;
    let EngineEvent::WatchError { path, .. } = e else { unreachable!() };
    assert_eq!(path, r.watch.join("bad.torrent"));
    assert!(path.exists(), "a bad file is left for the user");
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(!matches!(ev.try_recv(), Ok(EngineEvent::WatchError { .. })), "reported once");
    // Not a .torrent: ignored.
    std::fs::write(r.watch.join("notes.txt"), b"hi").unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(r.watch.join("notes.txt").exists());
}

/// Changing the configured folders at runtime starts and stops watching.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_watch_list_follows_the_configuration() {
    let r = rig(WatchMode::Auto, |_, _| AfterAdd::Leave, None).await;
    let other = tempfile::tempdir().unwrap();
    let o = other.path().to_path_buf();
    r.n.h.update_config(move |c| {
        c.watch = vec![WatchConfig { path: o, mode: WatchMode::Auto, after_add: AfterAdd::Leave, category: None, rule: None, since: None }];
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = book(508, "H G Wells - Time Machine.epub");
    std::fs::write(r.watch.join("old.torrent"), &t.bytes).unwrap();
    tokio::time::sleep(Duration::from_millis(5000)).await;
    assert!(r.n.row(&t.id).await.is_none(), "the old folder is no longer watched");
    let mut ev = r.n.h.subscribe();
    std::fs::write(other.path().join("new.torrent"), &t.bytes).unwrap();
    wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { .. })).await;
}

/// SPEC 7A.1: a folder that joins the watch list does not mass-add the `.torrent` files that are
/// already in it (real `~/Downloads` folders are full of them, some from private trackers).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_files_are_backlog_and_only_new_ones_are_picked_up() {
    let old1 = book(601, "Old One.epub");
    let old2 = book(602, "Old Two.epub");
    let (o1, o2) = (old1.bytes.clone(), old2.bytes.clone());
    let r = rig_pre(WatchMode::Auto, |_, _| AfterAdd::Leave, None, move |w| {
        std::fs::write(w.join("old-one.torrent"), &o1).unwrap();
        std::fs::write(w.join("old-two.torrent"), &o2).unwrap();
        // Not a torrent: never listed.
        std::fs::write(w.join("notes.txt"), b"hi").unwrap();
    })
    .await;
    let mut ev = r.n.h.subscribe();
    // Several scans pass (one per 2 s, plus the minute safety net does not matter here).
    tokio::time::sleep(Duration::from_millis(6500)).await;
    assert!(r.n.row(&old1.id).await.is_none() && r.n.row(&old2.id).await.is_none(), "backlog is not added");
    assert!(ev.try_recv().is_err(), "and nothing is announced about it");
    // The app can list the backlog (and offer to add it once).
    let backlog = r.n.h.watch_backlog(&r.watch).await;
    assert_eq!(names_of(&backlog), ["old-one.torrent", "old-two.torrent"]);
    assert!(r.n.h.watch_backlog(&r.lib).await.is_empty(), "an unwatched folder has no backlog");

    // A file that arrives after the baseline is picked up as usual.
    let fresh = book(603, "Fresh.epub");
    std::fs::write(r.watch.join("fresh.torrent"), &fresh.bytes).unwrap();
    wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { id, .. } if *id == fresh.id)).await;
    assert!(r.n.row(&old1.id).await.is_none());
    // The backlog is still only the old ones.
    let backlog = r.n.h.watch_backlog(&r.watch).await;
    assert_eq!(names_of(&backlog), ["old-one.torrent", "old-two.torrent"]);

    // The user accepts the offer for one of them: it is added like a dropped file.
    r.n.h.add_torrent_path(&backlog[0], AddOptions::default(), Some(AfterAdd::Leave)).await.unwrap();
    assert!(r.n.row(&old1.id).await.is_some());
}

/// A file that is replaced (deleted and written again, as a browser does) after the baseline is
/// news even though an old file of that name sat in the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backlog_name_that_is_replaced_later_is_news() {
    let t = book(604, "Edited.epub");
    let bytes = t.bytes.clone();
    let r = rig_pre(WatchMode::Auto, |_, _| AfterAdd::Leave, None, move |w| {
        std::fs::write(w.join("edited.torrent"), b"not a torrent yet").unwrap();
        let _ = &bytes;
    })
    .await;
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let mut ev = r.n.h.subscribe();
    assert_eq!(names_of(&r.n.h.watch_backlog(&r.watch).await), ["edited.torrent"]);
    std::fs::remove_file(r.watch.join("edited.torrent")).unwrap();
    std::fs::write(r.watch.join("edited.torrent"), &t.bytes).unwrap();
    wait_event(&mut ev, 15, |e| matches!(e, EngineEvent::AutoAdded { id, .. } if *id == t.id)).await;
    assert!(r.n.h.watch_backlog(&r.watch).await.is_empty());
}

fn names_of(v: &[PathBuf]) -> Vec<String> {
    v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
}
