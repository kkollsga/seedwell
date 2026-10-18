//! Flat placement (SPEC 7A.3): whatever is detected, a torrent keeps its own layout (its top
//! folder, or the single file's name) directly inside its finished location. Junk is skipped,
//! nothing half-downloaded is visible, and the files keep seeding byte-exact after a restart.
use crate::common;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use trs_engine::config::DetectionConfig;
use trs_engine::*;

struct Lab {
    /// The finished location (the app default).
    dl: PathBuf,
    /// `.incomplete` staging folder.
    inc: PathBuf,
    state: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn lab() -> Lab {
    let (dl, inc, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    Lab { dl: dl.path().into(), inc: inc.path().join(".incomplete"), state: state.path().into(), _dirs: vec![dl, inc, state] }
}

/// Detection on (it still drives the category, the title and smart add), staging on.
fn flat(l: &Lab) -> impl FnOnce(&mut EngineConfig) + use<> {
    let inc = l.inc.clone();
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
    }
}

fn movie() -> TestTorrent {
    let n = "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP";
    make_torrent(&TorrentSpec {
        name: n,
        piece_len: 32 * 1024,
        files: &[
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv", 900_000),
            ("Sample/sample.mkv", 150_000),
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.nfo", 2_000),
        ],
        private: false,
        announce: None,
        seed: 401,
    })
}

fn tv() -> TestTorrent {
    make_torrent(&TorrentSpec {
        name: "Some.Show.S02.1080p.WEB-DL.x264-GRP",
        piece_len: 32 * 1024,
        files: &[
            ("Some.Show.S02E01.1080p.WEB-DL.x264-GRP.mkv", 400_000),
            ("Some.Show.S02E02.1080p.WEB-DL.x264-GRP.mkv", 410_000),
            ("Some.Show.S02E03.1080p.WEB-DL.x264-GRP.mkv", 420_000),
        ],
        private: false,
        announce: None,
        seed: 402,
    })
}

fn music() -> TestTorrent {
    make_torrent(&TorrentSpec {
        name: "The Beatles - Abbey Road (1969) [FLAC]",
        piece_len: 32 * 1024,
        files: &[("01 - Come Together.flac", 300_000), ("02 - Something.flac", 280_000), ("cover.jpg", 40_000)],
        private: false,
        announce: None,
        seed: 403,
    })
}

fn book() -> TestTorrent {
    make_torrent(&TorrentSpec {
        name: "Jane Austen - Pride and Prejudice.epub",
        piece_len: 32 * 1024,
        files: &[("Jane Austen - Pride and Prejudice.epub", 500_000)],
        private: false,
        announce: None,
        seed: 404,
    })
}

async fn seeder_of(t: &TestTorrent) -> Node {
    let n = node(|_| {}).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

fn assert_file(p: &Path, content: &[u8]) {
    let got = std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert!(got == content, "{} differs", p.display());
}

fn dir_is_empty_or_gone(p: &Path) -> bool {
    std::fs::read_dir(p).map(|mut d| d.next().is_none()).unwrap_or(true)
}

/// Download `t`, check that every wanted file is at `<finished>/<torrent name>/...` (the torrent's
/// own layout), restart and serve a third node.
async fn run_case(t: &TestTorrent, absent: &[usize], expect_category: Category) {
    let l = lab();
    let seeder = seeder_of(t).await;
    let mut n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let pv = n.h.preview(AddSource::TorrentFile(t.bytes.clone()), AddOptions::default()).await.unwrap();
    assert_eq!(pv.category, expect_category, "detected {:?} with confidence {}", pv.detected, pv.confidence);
    assert!(!pv.uncertain, "confidence {}", pv.confidence);
    assert_eq!(pv.destination.as_deref(), Some(l.dl.as_path()), "the finished location, nothing below it but the torrent's own layout");

    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    // `Finished` comes after the files left `.incomplete`.
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    // Everything wanted is at <finished>/<torrent name>/<path in the torrent>, byte-exact.
    for (i, (_, bytes)) in t.files.iter().enumerate() {
        let own = t.file_path(&l.dl, i);
        if absent.contains(&i) {
            // Skipped junk is never downloaded; at most the edge of a piece reaches into it (files
            // are sparse, so look at the content that is really there).
            if let Ok(got) = std::fs::read(&own) {
                let same = got.iter().zip(bytes).filter(|(a, b)| a == b).count();
                assert!(same < bytes.len() / 2, "junk {} was downloaded", own.display());
            }
        } else {
            assert_file(&own, bytes);
        }
    }
    // Nothing but the torrent's own top entry is in the finished location, and nothing half-finished
    // is left behind in the staging folder.
    let top: Vec<_> = std::fs::read_dir(&l.dl).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(top, std::slice::from_ref(&t.name), "flat: only the torrent's own folder or file");
    assert!(dir_is_empty_or_gone(&l.inc), "staging not clean: {:?}", std::fs::read_dir(&l.inc).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()));
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.category, Some(expect_category));
    assert!(!row.organized, "nothing is placed file by file any more");
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    drop(n);

    // Restart: no recheck, and the files seed.
    let b = snode(&l.state, &l.dl, None, flat(&l)).await;
    let row = wait_for("row", 10, || async { b.row(&t.id).await }).await;
    assert_eq!(row.tier, Tier::Cold);
    let (l3s, l3d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let third = snode(l3s.path(), l3d.path(), None, |_| {}).await;
    // The third node wants exactly what the second one has: skipped files stay skipped.
    let prios: Vec<FilePriority> = (0..t.files.len()).map(|i| if absent.contains(&i) { FilePriority::Skip } else { FilePriority::Normal }).collect();
    third
        .h
        .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![b.addr()], file_priorities: prios, ..Default::default() })
        .await
        .unwrap();
    wait_done_h(&third.h, &t.id, 60).await;
    t.verify(l3d.path(), absent);
    assert_eq!(b.h.diagnostics().rechecks, 0, "a clean restart must not recheck");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_file_movie_lands_in_its_own_folder_and_junk_is_skipped() {
    run_case(&movie(), &[1, 2], Category::Movie).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tv_episodes_stay_together_in_the_torrents_folder() {
    run_case(&tv(), &[], Category::Tv).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn music_keeps_the_torrents_folder_and_file_names() {
    run_case(&music(), &[], Category::Music).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_file_torrent_is_the_file_itself_in_the_finished_location() {
    let t = book();
    run_case(&t, &[], Category::Book).await;
    assert_eq!(t.files.len(), 1);
}

/// Detection never decides a folder: with a category chosen by the user the layout is still flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_category_does_not_change_where_files_go() {
    let t = book();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], category: Some(Category::Movie), ..Default::default() })
        .await
        .unwrap();
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    t.verify(&l.dl, &[]);
    assert_eq!(n.row(&t.id).await.unwrap().category, Some(Category::Movie));
}

/// A name that is taken by another file is never overwritten: the torrent reports the problem and
/// the user's file is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_file_in_the_finished_location_is_never_overwritten() {
    let t = book();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let dest = l.dl.join("Jane Austen - Pride and Prejudice.epub");
    std::fs::write(&dest, b"my own copy").unwrap();
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let row = wait_for("the problem is reported", 60, || async { n.row(&t.id).await.filter(|r| r.error.is_some()) }).await;
    assert!(row.error.is_some_and(|e| e.contains("already exists")));
    assert_eq!(std::fs::read(&dest).unwrap(), b"my own copy");
}

/// While it downloads, nothing appears in the finished location (media apps never see half files).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_visible_in_the_finished_location_before_the_download_ends() {
    let t = book();
    let l = lab();
    let seeder = {
        let n = node(|c| c.up_limit = Some(200_000)).await;
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&n, &t.id, 30).await;
        n
    };
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("some data", 20, || async { (n.row(&t.id).await?.done_bytes > 50_000).then_some(()) }).await;
    assert!(dir_is_empty_or_gone(&l.dl), "the finished location is untouched until the move");
    assert!(l.inc.exists(), "data is staged");
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    assert!(t.file_path(&l.dl, 0).exists());
}

/// An explicit folder is the finished location of that torrent (and pins it there).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_explicit_save_path_is_used_as_it_is() {
    let t = book();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let own = tempfile::tempdir().unwrap();
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], save_path: Some(own.path().into()), ..Default::default() },
    )
    .await
    .unwrap();
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    t.verify(own.path(), &[]);
    assert!(dir_is_empty_or_gone(&l.dl));
}

/// A low-confidence name is not guessed: it is `Other`, flagged uncertain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_names_are_flagged_and_filed_as_other() {
    let t = make_torrent(&TorrentSpec {
        name: "stuff",
        piece_len: 32 * 1024,
        files: &[("a.bin", 50_000), ("b.bin", 50_000)],
        private: false,
        announce: None,
        seed: 405,
    });
    let l = lab();
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let pv = n.h.preview(AddSource::TorrentFile(t.bytes.clone()), AddOptions::default()).await.unwrap();
    assert!(pv.uncertain && pv.category == Category::Other, "{pv:?}");
    // The user can decide: forcing a category changes the category, not the place.
    let pv = n
        .h
        .preview(AddSource::TorrentFile(t.bytes.clone()), AddOptions { category: Some(Category::Book), ..Default::default() })
        .await
        .unwrap();
    assert!(!pv.uncertain && pv.category == Category::Book);
    assert_eq!(pv.destination.as_deref(), Some(l.dl.as_path()));
}

/// Removing a torrent with its files deletes the torrent's own folder and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_torrent_with_its_files_deletes_its_folder_only() {
    let t = tv();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    std::fs::write(l.dl.join("keep.txt"), b"mine").unwrap();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done_h(&n.h, &t.id, 60).await;
    wait_for("moved", 40, || async { t.file_path(&l.dl, 0).exists().then_some(()) }).await;
    n.h.remove(&[t.id], true).await.unwrap();
    wait_for("folder gone", 30, || async { (!l.dl.join(&t.name).exists()).then_some(()) }).await;
    assert_eq!(std::fs::read(l.dl.join("keep.txt")).unwrap(), b"mine");
}

/// A magnet link is classified once its metadata is in, and the user's choices made at add time
/// are kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_is_classified_when_the_metadata_arrives() {
    let t = movie();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let mut ev = n.h.subscribe();
    let uri = format!("magnet:?xt=urn:btih:{}&dn={}", hex_of(&t.id), t.name);
    n.h.add_magnet(&uri, AddOptions { peers: vec![seeder.addr()], priority: Some(Priority::High), ..Default::default() })
        .await
        .unwrap();
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    assert_file(&t.file_path(&l.dl, 0), &t.files[0].1);
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.category, Some(Category::Movie));
    assert_eq!(row.priority, Priority::High, "the choice made at add time survives the metadata step");
}

/// The process died after the move finished but before the record said so: the restart finds the
/// files in their place instead of reporting them missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_staged_flag_does_not_break_the_restart() {
    use trs_engine::persist::Store;
    let t = tv();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let mut n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    let store = n.store.clone();
    drop(n);
    // Put the record back to "still staged".
    let mut rec = store.load_torrents().unwrap().remove(0).record;
    assert!(!rec.staged);
    rec.staged = true;
    store.commit(trs_engine::persist::Batch { torrents: vec![rec], ..Default::default() }).unwrap();
    drop(store);

    let b = snode(&l.state, &l.dl, None, flat(&l)).await;
    b.h.add_peers(t.id, vec!["127.0.0.1:9".parse().unwrap()]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let row = b.row(&t.id).await.unwrap();
    assert!(matches!(row.state, TorrentState::LowPower | TorrentState::Seeding), "{:?} {:?}", row.state, row.error);
    assert_eq!(row.done_bytes, t.total());
}

/// Adding a torrent again after "remove, keep files" finds the data where it was left: no second
/// copy, no download.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn re_adding_a_removed_torrent_reuses_its_files() {
    let t = book();
    let l = lab();
    let seeder = seeder_of(&t).await;
    let n = snode(&l.state, &l.dl, None, flat(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    let dest = t.file_path(&l.dl, 0);
    assert_file(&dest, &t.files[0].1);
    n.h.remove(&[t.id], false).await.unwrap();
    assert!(dest.exists());
    // No peers at all: the torrent can only complete from what is on disk.
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&n.h, &t.id, 20).await;
    assert_eq!(n.row(&t.id).await.unwrap().done_bytes, t.total());
    assert_eq!(std::fs::read_dir(&l.dl).unwrap().count(), 1, "no second copy");
    assert_file(&dest, &t.files[0].1);
}
