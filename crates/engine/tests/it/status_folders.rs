//! Organize by seed status (SPEC 7A.3): finished torrents live in `Active/`, `Dormant/`,
//! `Stopped/` or `Dead/` below their finished location, moved with the ordinary same-volume
//! relocation (SPEC 7A.5), checked at most once an hour (here every 250 ms), never while the
//! torrent is uploading, never while it downloads, never when the user placed it.
use crate::common;
use std::path::PathBuf;
use std::time::Duration;

use common::*;
use trs_engine::config::DetectionConfig;
use trs_engine::*;

struct Lab {
    dl: PathBuf,
    inc: PathBuf,
    state: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn lab() -> Lab {
    let d: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    Lab { dl: d[0].path().into(), inc: d[1].path().join(".incomplete"), state: d[2].path().into(), _dirs: d }
}

/// Staging on; status folders on or off; the check runs every 250 ms.
fn cfg(l: &Lab, by_status: bool) -> impl FnOnce(&mut EngineConfig) + use<> {
    cfg_every(l, by_status, Duration::from_millis(250))
}

fn cfg_every(l: &Lab, by_status: bool, every: Duration) -> impl FnOnce(&mut EngineConfig) + use<> {
    let inc = l.inc.clone();
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
        c.by_status = by_status;
        c.status_interval = every;
    }
}

fn bundle(name: &str, seed: u64) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name,
        piece_len: 32 * 1024,
        files: &[("one.bin", 600_000), ("sub/two.bin", 400_000)],
        private: false,
        announce: None,
        seed,
    })
}

/// Complete data in `<dl>/<folder>`, added to a node with the given settings.
async fn seeding(l: &Lab, t: &TestTorrent, folder: Option<&str>, by_status: bool) -> SNode {
    seeding_every(l, t, folder, by_status, Duration::from_millis(250)).await
}

async fn seeding_every(l: &Lab, t: &TestTorrent, folder: Option<&str>, by_status: bool, every: Duration) -> SNode {
    let at = folder.map_or(l.dl.clone(), |f| l.dl.join(f));
    t.write_to(&at);
    let n = snode(&l.state, &l.dl, None, cfg_every(l, by_status, every)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    n
}

async fn leech(from: &SNode, t: &TestTorrent) -> Node {
    let n = node(|_| {}).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![from.addr()], ..Default::default() }).await.unwrap();
    wait_done(&n, &t.id, 120).await;
    t.verify(n.dir.path(), &[]);
    n
}

async fn wait_move_end(ev: &mut tokio::sync::broadcast::Receiver<EngineEvent>, id: TorrentId, secs: u64) {
    let e = wait_event(ev, secs, |e| matches!(e, EngineEvent::MoveFinished { id: i, .. } | EngineEvent::MoveFailed { id: i, .. } if *i == id)).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
}

async fn wait_at(t: &TestTorrent, dir: PathBuf, what: &str) {
    wait_for(what, 30, || async { t.file_path(&dir, 1).exists().then_some(()) }).await;
    t.verify(&dir, &[]);
}

fn top(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// Turning the switch on proposes moving the finished torrents into `Active/`; accepting does it,
/// and turning it off proposes (and does) the way back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabling_files_finished_torrents_in_active_and_disabling_moves_them_back() {
    let t = bundle("statusbundle", 1301);
    let l = lab();
    let n = seeding(&l, &t, None, false).await;
    let mut ev = n.h.subscribe();
    assert!(n.h.location_moves().await.unwrap().items.is_empty());

    n.h.update_config(|c| c.by_status = true);
    let e = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let EngineEvent::LocationsChanged { torrents, .. } = e else { unreachable!() };
    assert_eq!(torrents, 1);
    let p = n.h.location_moves().await.unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!((p.items[0].kind, p.items[0].cross_volume), (MoveKind::Finished, false), "a rename, not a copy");
    assert_eq!(p.items[0].to, l.dl.join("Active").join("statusbundle"));
    t.verify(&l.dl, &[]); // nothing moves before the user says so
    n.h.move_torrents(&[t.id]).await.unwrap();
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Active"), "in Active").await;
    assert_eq!(top(&l.dl), ["Active"], "flat otherwise");
    assert!(n.h.location_moves().await.unwrap().items.is_empty());
    let _ = leech(&n, &t).await;

    n.h.update_config(|c| c.by_status = false);
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let p = n.h.location_moves().await.unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].to, l.dl.join("statusbundle"));
    n.h.move_torrents(&[t.id]).await.unwrap();
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.clone(), "back at the top").await;
    assert!(!l.dl.join("Active/statusbundle").exists());
    assert_eq!(top(&l.dl), ["statusbundle"], "the emptied Active/ folder is gone too");
}

/// A torrent added while the switch is on is filed in `Active/` from the start, and pausing it
/// sends it to `Stopped/` at the next check; resuming brings it back once two checks agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_goes_to_stopped_and_resume_back_to_active() {
    let t = bundle("pausebundle", 1302);
    let l = lab();
    let n = seeding(&l, &t, Some("Active"), true).await;
    let mut ev = n.h.subscribe();
    t.verify(&l.dl.join("Active"), &[]);
    n.h.pause(&[t.id]);
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Stopped"), "in Stopped").await;
    assert!(!l.dl.join("Active/pausebundle").exists());
    // It still seeds from the new place after a resume, and is filed back in Active/.
    n.h.resume(&[t.id]);
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Active"), "back in Active").await;
    let _ = leech(&n, &t).await;
}

/// A seeding goal that is reached stops the torrent: it moves to `Stopped/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reached_goal_moves_the_torrent_to_stopped() {
    let t = bundle("goalbundle", 1303);
    let l = lab();
    t.write_to(&l.dl.join("Active"));
    let n = snode(&l.state, &l.dl, None, cfg(&l, true)).await;
    let mut ev = n.h.subscribe();
    let goal = SeedGoal { ratio: Some(0.5), seed_secs: None, min_seed_secs: 0, any: true, at_goal: AtGoal::Stop, stop_on_finish: false, idle_mb: 0, idle_hours: 0 };
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { seed_goal: Some(goal), ..Default::default() }).await.unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    let _ = leech(&n, &t).await;
    wait_event(&mut ev, 30, |e| matches!(e, EngineEvent::SeedingGoalReached { id, .. } if *id == t.id)).await;
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Stopped"), "in Stopped").await;
}

/// No move while the torrent is uploading: it sits in `Stopped/` (paused earlier), is resumed while
/// a peer downloads from it, and stays until the peer is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_moves_while_the_torrent_is_uploading() {
    let t = make_torrent(&TorrentSpec {
        name: "busybundle",
        piece_len: 32 * 1024,
        files: &[("one.bin", 1_200_000), ("sub/two.bin", 800_000)],
        private: false,
        announce: None,
        seed: 1304,
    });
    let l = lab();
    // Checks every 1.2 s: the peer is connected and downloading well before a second check could agree.
    let n = seeding_every(&l, &t, Some("Active"), true, Duration::from_millis(1200)).await;
    let mut ev = n.h.subscribe();
    n.h.pause(&[t.id]);
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Stopped"), "in Stopped").await;
    // Slow enough that the peer is still downloading for several checks (about thirteen seconds).
    n.h.set_limits(None, Some(150_000));
    n.h.resume(&[t.id]);
    let peer = node(|_| {}).await;
    peer.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![n.addr()], ..Default::default() }).await.unwrap();
    wait_for("the peer is receiving", 20, || async { (peer.row(&t.id).await?.done_bytes > 50_000).then_some(()) }).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(600)).await;
        if peer.row(&t.id).await.is_some_and(|r| r.done_bytes >= t.total()) {
            break;
        }
        assert!(l.dl.join("Stopped/busybundle").exists(), "it moved while uploading: {:?}", top(&l.dl));
    }
    // The peer finishes, the uploading stops, and the move happens.
    wait_done(&peer, &t.id, 60).await;
    wait_move_end(&mut ev, t.id, 40).await;
    wait_at(&t, l.dl.join("Active"), "back in Active").await;
}

/// A download in progress is in the incomplete location: the status check never touches it, and
/// it finishes into `Active/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_in_progress_is_not_moved_and_finishes_in_active() {
    let t = bundle("flowbundle", 1305);
    let l = lab();
    let seeder = {
        let n = node(|c| c.up_limit = Some(300_000)).await;
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&n, &t.id, 30).await;
        n
    };
    let n = snode(&l.state, &l.dl, None, cfg(&l, true)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("some data", 20, || async { (n.row(&t.id).await?.done_bytes > 50_000).then_some(()) }).await;
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if n.row(&t.id).await.is_some_and(|r| r.done_bytes >= t.total()) {
            break;
        }
        assert!(!l.dl.join("Active/flowbundle").exists(), "nothing is in the finished location before the download ends: {:?}", top(&l.dl));
        assert!(l.inc.join(hex_of(&t.id)).exists(), "the data is staged");
    }
    wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::Finished { id } if *id == t.id)).await;
    wait_at(&t, l.dl.join("Active"), "finished into Active").await;
    assert_eq!(top(&l.dl), ["Active"]);
}

/// A torrent the user placed (a save path of their own) is not organized by status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_the_user_placed_stays_put() {
    let t = bundle("pinnedbundle", 1306);
    let l = lab();
    let own = tempfile::tempdir().unwrap();
    t.write_to(own.path());
    let n = snode(&l.state, &l.dl, None, cfg(&l, true)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { save_path: Some(own.path().into()), ..Default::default() }).await.unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    n.h.pause(&[t.id]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    t.verify(own.path(), &[]);
    assert!(top(&l.dl).is_empty(), "{:?}", top(&l.dl));
    assert!(n.h.location_moves().await.unwrap().items.is_empty());
}

/// Turning the switch on only proposes: a declined move leaves the torrent where it is, also at the
/// next status checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declined_move_is_not_made_behind_the_users_back() {
    let t = bundle("declinebundle", 1307);
    let l = lab();
    let n = seeding(&l, &t, None, false).await;
    n.h.update_config(|c| c.by_status = true);
    n.h.pause(&[t.id]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    t.verify(&l.dl, &[]);
    assert_eq!(top(&l.dl), ["declinebundle"]);
}

// ---- files that cannot be moved --------------------------------------------------------------

/// `chflags uchg` (Finder "Locked") on a file, undone on drop so a failing test leaves nothing behind.
struct Locked(PathBuf);

impl Locked {
    fn new(p: PathBuf) -> Self {
        assert!(std::process::Command::new("chflags").arg("uchg").arg(&p).status().unwrap().success());
        Locked(p)
    }
    fn unlock(&self) {
        let _ = std::process::Command::new("chflags").arg("nouchg").arg(&self.0).stderr(std::process::Stdio::null()).status();
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        self.unlock();
    }
}

/// A folder made read-only, put back on drop.
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn new(p: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o555)).unwrap();
        ReadOnly(p)
    }
    fn restore(&self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        self.restore();
    }
}

async fn failures(ev: &mut tokio::sync::broadcast::Receiver<EngineEvent>, id: TorrentId, ms: u64) -> (Vec<String>, u32) {
    let end = std::time::Instant::now() + Duration::from_millis(ms);
    let (mut failed, mut started) = (Vec::new(), 0);
    while std::time::Instant::now() < end {
        if let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(50), ev.recv()).await {
            match e {
                EngineEvent::MoveFailed { id: i, reason } if i == id => failed.push(reason),
                EngineEvent::MoveStarted { id: i, .. } if i == id => started += 1,
                _ => {}
            }
        }
    }
    (failed, started)
}

/// A Finder-locked file stops the automatic move before anything is touched: one friendly
/// failure for many checks, no "Moving", the torrent keeps seeding where it is with the reason on
/// its row, and the move goes through by itself once the file is unlocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locked_file_is_reported_once_and_the_move_follows_the_unlock() {
    let t = bundle("lockedbundle", 1308);
    let l = lab();
    let n = seeding(&l, &t, Some("Active"), true).await;
    let mut ev = n.h.subscribe();
    let lock = Locked::new(l.dl.join("Active/lockedbundle/sub/two.bin"));
    n.h.pause(&[t.id]); // belongs in Stopped/ at the next check
    let (failed, started) = failures(&mut ev, t.id, 2500).await; // about ten checks
    assert_eq!(
        failed,
        ["\u{201c}two.bin\u{201d} is locked (Finder \u{203a} Get Info \u{203a} Locked), so the torrent stays where it is"]
    );
    assert_eq!(started, 0, "the preflight runs before the move is announced");
    assert!(!failed[0].contains("file #"));
    let r = row_h(&n.h, &t.id).await.unwrap();
    assert!(r.error.is_none() && r.moving.is_none(), "{r:?}");
    assert_eq!(r.move_note.as_deref(), Some(failed[0].as_str()));
    t.verify(&l.dl.join("Active"), &[]);
    assert!(!l.dl.join("Stopped").exists(), "nothing was renamed and renamed back");
    // Still seeds from the old place while locked (resumed it belongs in Active/ again, a new episode).
    n.h.resume(&[t.id]);
    let peer = leech(&n, &t).await;
    drop(peer);
    n.h.pause(&[t.id]);
    // Unlocked: the next check moves it, and the note is gone.
    lock.unlock();
    wait_move_end(&mut ev, t.id, 30).await;
    wait_at(&t, l.dl.join("Stopped"), "in Stopped").await;
    assert!(row_h(&n.h, &t.id).await.unwrap().move_note.is_none());
}

/// A move the user asked for says so every time it fails, in words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_the_user_asked_for_reports_a_locked_file_every_time() {
    let t = bundle("userlocked", 1309);
    let l = lab();
    let n = seeding(&l, &t, None, false).await;
    let mut ev = n.h.subscribe();
    let _lock = Locked::new(l.dl.join("userlocked/one.bin"));
    let dest = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        n.h.move_to(&[t.id], dest.path()).await.unwrap();
        let (failed, _) = failures(&mut ev, t.id, 800).await;
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].starts_with("\u{201c}one.bin\u{201d} is locked"), "{failed:?}");
    }
    t.verify(&l.dl, &[]);
    assert!(top(dest.path()).is_empty());
    assert!(n.h.snapshot().await.unwrap().rows.iter().find(|r| r.id == t.id).unwrap().move_note.is_none(), "the note is for automatic moves");
}

/// A folder that cannot be written is named, and nothing is touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_without_permission_is_named() {
    let t = bundle("nopermbundle", 1310);
    let l = lab();
    let n = seeding(&l, &t, Some("Active"), true).await;
    let mut ev = n.h.subscribe();
    let ro = ReadOnly::new(l.dl.clone()); // Stopped/ cannot be created here
    n.h.pause(&[t.id]);
    let (failed, started) = failures(&mut ev, t.id, 1500).await;
    ro.restore();
    let leaf = l.dl.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(failed, [format!("No permission to change \u{201c}{leaf}\u{201d}")]);
    assert_eq!(started, 0);
    t.verify(&l.dl.join("Active"), &[]);
}
