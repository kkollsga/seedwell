//! Seedwell's own torrents next to folder sync (SPEC §7A.5, §7G.11 *Modes*): a folder's files are never moved from under
//! a torrent, and the torrents are told when a folder's files moved (they may be back). A fake [`LocalTorrents`] stands
//! in for the engine; the control crate's tests cover the engine's answer.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use trs_sync_net::LocalTorrents;
use trs_sync_net::service::BoxFuture;
use trs_sync_proto::ids::FolderId;

use crate::chat::start_at;
use crate::utp::read;

/// The engine as folder sync sees it: each question gets the next answer (none left: no torrent), and the moves it is
/// told of are counted.
#[derive(Default)]
struct Torrents {
    answers: Mutex<VecDeque<Vec<String>>>,
    asked: AtomicUsize,
    moved: AtomicUsize,
}

impl Torrents {
    fn answering(answers: &[&[&str]]) -> Arc<Torrents> {
        let t = Arc::new(Torrents::default());
        t.answer(answers);
        t
    }

    fn answer(&self, answers: &[&[&str]]) {
        *self.answers.lock().unwrap() = answers.iter().map(|a| a.iter().map(|n| (*n).to_string()).collect()).collect();
        self.asked.store(0, Ordering::SeqCst);
    }
}

impl LocalTorrents for Torrents {
    fn using(&self, _path: &Path) -> BoxFuture<'static, Result<Vec<String>, String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let names = self.answers.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(async move { Ok(names) })
    }

    fn moved(&self) {
        self.moved.fetch_add(1, Ordering::SeqCst);
    }
}

fn folder_of(h: &trs_sync_net::SyncHandle, id: &FolderId) -> Option<trs_sync_net::status::FolderStatus> {
    h.snapshot().folders.into_iter().find(|f| f.id == id.to_string())
}

/// Review of the on-demand fix, #2: the torrents were asked only before the folder stopped, which can take a minute; a
/// torrent added meanwhile (or one the engine moved into the folder) lost its files to the move. Fails when the guard
/// is not asked again right before the move: freeing the local copies and `sync move` must both be refused by the
/// second answer, with every file where it was and the folder syncing on there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_that_turns_up_while_the_folder_stops_keeps_its_files() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let movies = work.path().join("Movies");
    std::fs::create_dir_all(movies.join("V for Vendetta (2005)")).unwrap();
    let film = movies.join("V for Vendetta (2005)/film.mkv");
    std::fs::write(&film, b"a film").unwrap();
    let t = Torrents::answering(&[&[], &["Added meanwhile"]]);
    let lent: Arc<dyn LocalTorrents> = t.clone();
    let h = start_at(state.path(), "Beta", |o| o.torrents = Some(lent)).await;
    let id: FolderId = h.add_folder(movies.clone()).await.unwrap().id.parse().unwrap();
    let journal = state.path().join("sync").join(format!("{id}.journal"));

    let err = h.free_local_copies(&id, None).await.unwrap_err().to_string();
    assert!(err.starts_with("a torrent uses files in this folder (“Added meanwhile”)"), "{err}");
    assert_eq!(t.asked.load(Ordering::SeqCst), 2, "asked before and again once the folder stopped");
    assert_eq!(read(&film).as_deref(), Some(&b"a film"[..]), "the file stays where it is");
    assert!(!state.path().join("sync").join(format!("{id}.cache")).exists() && !journal.exists());
    assert!(folder_of(&h, &id).is_some_and(|f| !f.on_demand), "the folder syncs on, full, where it is");

    t.answer(&[&[], &["Moved in meanwhile"]]);
    let to = work.path().join("Elsewhere");
    let err = h.move_folder(&id, to.clone()).await.unwrap_err().to_string();
    assert!(err.starts_with("a torrent uses files in this folder (“Moved in meanwhile”)"), "{err}");
    assert_eq!(t.asked.load(Ordering::SeqCst), 2);
    assert!(!to.exists(), "nothing went to the new place");
    assert_eq!(read(&film).as_deref(), Some(&b"a film"[..]));
    assert!(folder_of(&h, &id).is_some_and(|f| Path::new(&f.path) == movies) && !journal.exists());
    h.shutdown().await;
}

/// Review of the on-demand fix, #6: a switch back to full that a start finishes from its journal put the files back
/// without telling the torrents, and the daemon has no disk observer, so a torrent missing them waited for its retry
/// (up to an hour). Fails when finishing the switch at the start does not tell the torrents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_to_full_finished_at_the_next_start_tells_the_torrents() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"held here").unwrap();
    let h = start_at(state.path(), "Beta", |_| {}).await;
    let id: FolderId = h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    h.free_local_copies(&id, None).await.unwrap();
    let cache = h.od_cache_path(&id, "").unwrap();
    h.shutdown().await;
    // Cut off after the files moved back on one volume, before it was saved.
    std::fs::rename(&cache, &docs).unwrap();
    let text = |p: &Path| format!("{:?}", p.display().to_string());
    let j = format!(r#"{{"kind":"switch","on":false,"from":{},"to":{},"place":{},"copied":false}}"#, text(&cache), text(&docs), text(&docs));
    std::fs::write(state.path().join("sync").join(format!("{id}.journal")), j).unwrap();

    let t = Torrents::answering(&[]);
    let lent: Arc<dyn LocalTorrents> = t.clone();
    let h = start_at(state.path(), "Beta", |o| o.torrents = Some(lent)).await;
    let f = folder_of(&h, &id).unwrap();
    assert!(!f.on_demand && Path::new(&f.path) == docs, "{f:?}");
    assert!(t.moved.load(Ordering::SeqCst) >= 1, "the torrents were told the files are back");
    assert_eq!(read(&docs.join("a.txt")).as_deref(), Some(&b"held here"[..]));
    h.shutdown().await;
}

/// Review of the on-demand fix, #5: Add with *Files on demand* was an add and then a separate save of the mark; the
/// mark failing left the folder added (Start again: "already synced"). Now one step: the folder comes with its mark,
/// and nothing on this device moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_added_with_files_on_demand_comes_with_its_mark_in_one_step() {
    let (state, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"stays").unwrap();
    let h = start_at(state.path(), "Beta", |_| {}).await;
    let f = h.add_folder_with(docs.clone(), true).await.unwrap();
    assert!(!f.on_demand && f.on_demand_others && Path::new(&f.path) == docs, "{f:?}");
    let id: FolderId = f.id.parse().unwrap();
    assert!(h.offers_on_demand(&id));
    assert!(std::fs::read_to_string(state.path().join("sync.toml")).unwrap().contains("on_demand_others = true"), "saved with the folder");
    assert_eq!(read(&docs.join("a.txt")).as_deref(), Some(&b"stays"[..]));
    h.shutdown().await;
}
