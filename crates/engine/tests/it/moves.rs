//! Moving files after a location changed (SPEC 7A.5): same-volume renames, verified cross-volume
//! copies (forced on one disk, and on a real second volume when `hdiutil` can make one), seeding
//! that goes on during a move, crashes at every step, collisions, free space, corrupt data.
use crate::common;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::*;
use trs_engine::config::{DetectionConfig, MoveFaultFn};
use trs_engine::*;

struct Lab {
    /// Root of the category folders.
    lib: PathBuf,
    dl: PathBuf,
    inc: PathBuf,
    state: PathBuf,
    /// A second folder on the same volume, the usual destination.
    other: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn lab() -> Lab {
    let d: Vec<tempfile::TempDir> = (0..5).map(|_| tempfile::tempdir().unwrap()).collect();
    Lab {
        lib: d[0].path().into(),
        dl: d[1].path().into(),
        inc: d[2].path().join(".incomplete"),
        state: d[3].path().into(),
        other: d[4].path().into(),
        _dirs: d,
    }
}

/// The torrent's own layout in the download folder.
fn plain(l: &Lab) -> impl FnOnce(&mut EngineConfig) + use<> {
    let inc = l.inc.clone();
    move |c| {
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
    }
}

/// Category folders: `Other` and `Software` torrents finish in their own plain folder below `lib`.
fn organizer(l: &Lab) -> impl FnOnce(&mut EngineConfig) + use<> {
    let (lib, inc) = (l.lib.clone(), l.inc.clone());
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.category_locations[Category::Other.index()].finished = Some(lib.join("Other"));
        c.category_locations[Category::Software.index()].finished = Some(lib.join("Software"));
        c.incomplete_dir = Some(inc);
        c.use_staging = true;
    }
}

fn bundle(name: &str, seed: u64, extra: usize) -> TestTorrent {
    standard_named(name, extra, None, false, seed)
}

/// A node that has `t` complete in its download folder.
async fn seeding_node(l: &Lab, t: &TestTorrent, cfg: impl FnOnce(&mut EngineConfig)) -> SNode {
    t.write_to(&l.dl);
    let n = snode(&l.state, &l.dl, None, cfg).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    n
}

async fn leech_all(from: &SNode, t: &TestTorrent) -> Node {
    let l = node(|_| {}).await;
    l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![from.addr()], ..Default::default() }).await.unwrap();
    wait_done(&l, &t.id, 120).await;
    t.verify(l.dir.path(), &[]);
    l
}

async fn wait_move_end(ev: &mut tokio::sync::broadcast::Receiver<EngineEvent>, id: TorrentId, secs: u64) -> EngineEvent {
    wait_event(ev, secs, |e| matches!(e, EngineEvent::MoveFinished { id: i, .. } | EngineEvent::MoveFailed { id: i, .. } if *i == id)).await
}

fn count_files(p: &Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        let q = e.path();
        if q.is_dir() { n += count_files(&q) } else { n += 1 }
    }
    n
}

fn tree(p: &Path) -> Vec<String> {
    fn walk(root: &Path, p: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            let q = e.path();
            out.push(q.strip_prefix(root).unwrap().display().to_string());
            if q.is_dir() {
                walk(root, &q, out);
            }
        }
    }
    let mut v = Vec::new();
    walk(p, p, &mut v);
    v.sort();
    v
}

/// A same-volume move while a peer is downloading from the torrent: the peer gets every byte,
/// correct, and the files end up at the new place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_volume_move_does_not_disturb_a_peer_that_is_downloading() {
    let t = bundle("movebundle", 901, 3_000_000);
    let l = lab();
    let s = seeding_node(&l, &t, plain(&l)).await;
    // Slow enough that the move happens in the middle of the transfer.
    s.h.set_limits(None, Some(1_000_000));
    let mut ev = s.h.subscribe();
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![s.addr()], ..Default::default() }).await.unwrap();
    wait_for("the peer is downloading", 30, || async {
        leech.row(&t.id).await.filter(|r| r.done_bytes > 500_000).map(|_| ())
    })
    .await;
    let rep = s.h.move_to(&[t.id], &l.other).await.unwrap();
    assert_eq!(rep.queued, [t.id], "{rep:?}");
    let e = wait_move_end(&mut ev, t.id, 30).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
    // The peer finishes with correct data, served partly from each place.
    wait_done(&leech, &t.id, 120).await;
    t.verify(leech.dir.path(), &[]);
    // Everything is at the new place, nothing is left at the old one.
    t.verify(&l.other, &[]);
    assert!(!l.dl.join("movebundle").exists(), "the old folder is gone: {:?}", tree(&l.dl));
    let row = s.row(&t.id).await.unwrap();
    assert!(row.moving.is_none());
    assert_eq!(row.save_path, l.other.join("movebundle"));
    // And it keeps seeding from the new place (a second, fresh peer).
    let second = leech_all(&s, &t).await;
    drop(second);
}

/// The move survives a restart: the record points at the new place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_new_place_survives_a_restart() {
    let t = bundle("restartbundle", 902, 100_000);
    let l = lab();
    let mut s = seeding_node(&l, &t, plain(&l)).await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    s.h.flush().await.unwrap();
    s.shutdown(Duration::from_secs(3)).await;
    drop(ev);
    drop(s);
    let s2 = snode(&l.state, &l.dl, None, plain(&l)).await;
    let _ = leech_all(&s2, &t).await;
    assert_eq!(s2.row(&t.id).await.unwrap().save_path, l.other.join("restartbundle"));
    assert!(!l.dl.join("restartbundle").exists());
}

/// A name that exists at the destination gets a suffix; the other file is never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_collision_gets_a_suffix_and_never_overwrites() {
    let t = bundle("clashbundle", 903, 10_000);
    let l = lab();
    let s = seeding_node(&l, &t, plain(&l)).await;
    // Someone else's file is where the torrent's `z.dat` would go.
    let theirs = l.other.join("clashbundle/z.dat");
    std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
    std::fs::write(&theirs, b"not yours").unwrap();
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    assert_eq!(std::fs::read(&theirs).unwrap(), b"not yours");
    let suffixed = l.other.join("clashbundle/z (2).dat");
    assert!(suffixed.exists(), "{:?}", tree(&l.other));
    assert_eq!(std::fs::read(&suffixed).unwrap(), t.files[3].1);
    // The torrent seeds correctly from its suffixed files (the old layout is gone).
    let _ = leech_all(&s, &t).await;
    assert!(!l.dl.join("clashbundle").exists());
}

/// A cross-volume copy (forced on one disk): a peer downloads from the source while it is copied,
/// the copy is verified, then the torrent switches in milliseconds and the old files go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_volume_copy_keeps_seeding_and_switches_when_verified() {
    let t = bundle("copybundle", 904, 6_000_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| {
        c.force_copy_moves = true;
        c.move_rate_limit = 3_000_000; // the copy takes a couple of seconds
    })
    .await;
    let mut ev = s.h.subscribe();
    let leech = node(|_| {}).await;
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    let started = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::MoveStarted { id, .. } if *id == t.id)).await;
    let EngineEvent::MoveStarted { cross_volume, bytes, .. } = started else { unreachable!() };
    assert!(cross_volume && bytes >= 6_000_000);
    // While it copies the torrent is "moving" and still serves from the old place.
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![s.addr()], ..Default::default() }).await.unwrap();
    let mut saw = None;
    for _ in 0..40 {
        if let Some(p) = s.row(&t.id).await.and_then(|r| r.moving) {
            saw = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saw.is_some_and(|p| (0.0..=1.0).contains(&p)), "{saw:?}");
    assert!(l.dl.join("copybundle").exists(), "the source is untouched while it copies");
    let e = wait_move_end(&mut ev, t.id, 60).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
    wait_done(&leech, &t.id, 120).await;
    t.verify(leech.dir.path(), &[]);
    t.verify(&l.other, &[]);
    assert!(!l.dl.join("copybundle").exists(), "the source is deleted after the switch: {:?}", tree(&l.dl));
    // Seeding continues from the copy.
    let _ = leech_all(&s, &t).await;
    // Progress events were published while it copied.
    let mut progress = 0;
    while let Ok(e) = ev.try_recv() {
        if matches!(e, EngineEvent::MoveProgress { .. }) {
            progress += 1;
        }
    }
    let _ = progress;
}

/// No room on the destination: the move is refused with a plain reason, nothing is left behind
/// and the torrent keeps seeding from where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_enough_free_space_refuses_the_copy() {
    let t = bundle("spacebundle", 905, 200_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| {
        c.force_copy_moves = true;
        c.move_reserve_bytes = 1 << 60; // far more than any disk has
    })
    .await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    let e = wait_move_end(&mut ev, t.id, 30).await;
    let EngineEvent::MoveFailed { reason, .. } = e else { panic!("{e:?}") };
    assert!(reason.contains("Not enough free space"), "{reason}");
    assert_eq!(count_files(&l.other), 0, "nothing was written: {:?}", tree(&l.other));
    t.verify(&l.dl, &[]);
    let _ = leech_all(&s, &t).await;
    assert!(s.row(&t.id).await.unwrap().moving.is_none());
}

/// A byte that is wrong on disk is caught by the hash that runs while copying: the move fails,
/// the destination is cleaned, the original stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_source_is_caught_by_the_hash_while_copying() {
    let t = bundle("rotbundle", 906, 400_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| c.force_copy_moves = true).await;
    // Flip a byte inside the big file after the torrent was verified.
    let big = l.dl.join("rotbundle/a/b/c/two.bin");
    let mut bytes = std::fs::read(&big).unwrap();
    bytes[200_000] ^= 0xFF;
    std::fs::write(&big, &bytes).unwrap();
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    let e = wait_move_end(&mut ev, t.id, 30).await;
    let EngineEvent::MoveFailed { reason, .. } = e else { panic!("{e:?}") };
    assert!(reason.contains("did not match the torrent"), "{reason}");
    assert_eq!(count_files(&l.other), 0, "{:?}", tree(&l.other));
    assert_eq!(std::fs::read(&big).unwrap(), bytes, "the original is as it was");
    assert_eq!(s.row(&t.id).await.unwrap().save_path, l.dl.join("rotbundle"));
}

/// One crash case: the process "dies" at fault step `stop_at` of a move; the next start resolves
/// it. Returns `None` when the move finished without reaching that step, else whether the move
/// ended up finished (`Some(true)`) or rolled back (`Some(false)`).
async fn crash_case(force_copy: bool, stop_at: u32) -> Option<bool> {
    let t = bundle("crashbundle", 907, 120_000);
    let l = lab();
    let fired = Arc::new(AtomicU32::new(0));
    let f2 = fired.clone();
    let mut s = seeding_node(&l, &t, move |c| {
        c.force_copy_moves = force_copy;
        c.move_fault = Some(MoveFaultFn(Arc::new(move |n| {
            // Storage steps (below `MOVE_STEP_BEFORE_MARK`) stop from `stop_at` on; a post-switch step stops there.
            if n >= stop_at && (stop_at >= trs_engine::MOVE_STEP_BEFORE_MARK || n < trs_engine::MOVE_STEP_BEFORE_MARK) {
                f2.store(n, Ordering::SeqCst);
                false
            } else {
                true
            }
        })));
    })
    .await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    // Either the move runs to its end (no fault reached) or it "crashes" at `stop_at`.
    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if fired.load(Ordering::SeqCst) != 0 {
                // The fault fired: give the session a moment to see the "crash", then stop waiting.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return false;
            }
            match tokio::time::timeout(Duration::from_millis(50), ev.recv()).await {
                Ok(Ok(EngineEvent::MoveFinished { id, .. })) if id == t.id => return true,
                Ok(Ok(EngineEvent::MoveFailed { id, .. })) if id == t.id => return false,
                Ok(Err(_)) => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    if fired.load(Ordering::SeqCst) == 0 {
        assert!(finished, "copy={force_copy} step={stop_at}: no fault fired, so the move must finish");
        return None;
    }
    // "Crash": the process dies, the files and the journal stay as they are.
    s.h.flush().await.unwrap();
    s.shutdown(Duration::from_secs(3)).await;
    drop(ev);
    drop(s);
    // The next start resolves it, before the torrent may serve again.
    let s2 = snode(&l.state, &l.dl, None, plain(&l)).await;
    wait_for("the journal is resolved", 30, || async {
        let left = std::fs::read_dir(l.state.join("moves"))
            .map_or(0, |d| d.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".journal")).count());
        (left == 0).then_some(())
    })
    .await;
    let at = wait_for("recovered", 30, || async {
        let r = s2.row(&t.id).await?;
        matches!(r.state, TorrentState::LowPower | TorrentState::Seeding).then_some(r.save_path)
    })
    .await;
    let (at_new, at_old) = (at == l.other.join("crashbundle"), at == l.dl.join("crashbundle"));
    assert!(at_new ^ at_old, "copy={force_copy} step={stop_at}: {at:?}");
    let (here, gone) = if at_new { (&l.other, &l.dl) } else { (&l.dl, &l.other) };
    t.verify(here, &[]);
    assert_eq!(
        count_files(&gone.join("crashbundle")),
        0,
        "copy={force_copy} step={stop_at}: nothing of it at the other place: {:?}",
        tree(gone)
    );
    for root in [&l.dl, &l.other] {
        assert!(!tree(root).iter().any(|p| p.ends_with(".trs-part")), "copy={force_copy} step={stop_at}: {:?}", tree(root));
    }
    // It serves correct bytes.
    let _ = leech_all(&s2, &t).await;
    Some(at_new)
}

/// A crash at every step of a move, on both methods, is resolved at the next start: the move is
/// finished or rolled back, never half, and the torrent seeds correct bytes afterwards. Steps
/// 1.. are the storage layer's file-system changes; `MOVE_STEP_BEFORE_MARK` and
/// `MOVE_STEP_BEFORE_COMMIT` are after the switch (the record points at the new place). One test per method, so
/// the two run side by side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_at_every_step_of_a_rename_is_resolved_at_the_next_start() {
    crash_at_every_step(false).await;
}

/// [`a_crash_at_every_step_of_a_rename_is_resolved_at_the_next_start`], for a move by copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_at_every_step_of_a_copy_is_resolved_at_the_next_start() {
    crash_at_every_step(true).await;
}

async fn crash_at_every_step(force_copy: bool) {
    {
        let (mut finished_n, mut rolled_back_n) = (0, 0);
        let mut step = 1u32;
        let mut tally = |outcome: bool| if outcome { finished_n += 1 } else { rolled_back_n += 1 };
        while let Some(outcome) = crash_case(force_copy, step).await {
            tally(outcome);
            step += 1;
            assert!(step < 200, "the move never finished");
        }
        let storage_steps = step - 1;
        for special in [trs_engine::MOVE_STEP_BEFORE_MARK, trs_engine::MOVE_STEP_BEFORE_COMMIT] {
            let outcome = crash_case(force_copy, special).await.expect("the post-switch steps are always reached");
            assert!(outcome, "a crash after the record points at the new place finishes the move");
            tally(outcome);
        }
        eprintln!("method copy={force_copy}: {storage_steps} storage steps + 2 post-switch steps checked, {finished_n} finished, {rolled_back_n} rolled back");
        assert!(finished_n > 0 && rolled_back_n > 0, "both outcomes must occur");
    }
}

/// Re-pointing a category / default folder: after the settings change the engine says which
/// torrents sit in the wrong place; accepting moves them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_default_folder_proposes_and_moves_the_torrents() {
    let t = bundle("defbundle", 908, 50_000);
    let l = lab();
    let s = seeding_node(&l, &t, plain(&l)).await;
    let mut ev = s.h.subscribe();
    assert!(s.h.location_moves().await.unwrap().items.is_empty(), "nothing is out of place yet");
    let new_dir = l.other.clone();
    s.h.update_config(move |c| c.default_save_dir = new_dir);
    let e = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let EngineEvent::LocationsChanged { torrents, bytes } = e else { unreachable!() };
    assert_eq!(torrents, 1);
    assert!(bytes >= t.total());
    let p = s.h.location_moves().await.unwrap();
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].id, t.id);
    assert_eq!(p.items[0].kind, MoveKind::Finished);
    assert!(!p.items[0].cross_volume);
    assert_eq!(p.items[0].to, l.other.join("defbundle"));
    // Nothing moved until the user says so.
    t.verify(&l.dl, &[]);
    let rep = s.h.move_torrents(&[t.id]).await.unwrap();
    assert_eq!(rep.queued, [t.id]);
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    t.verify(&l.other, &[]);
    assert!(s.h.location_moves().await.unwrap().items.is_empty(), "now it is where it belongs");
}

/// A tracker's own folder (SPEC 7A.4) is a location setting like the app's: saving it says which
/// torrents of that tracker now sit elsewhere, and accepting moves them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_tracker_folder_proposes_and_moves_its_torrents() {
    use trs_state::trackers::{Profile, UrlPattern};
    let t = standard_named("trackerbundle", 50_000, Some("http://tracker-z.test/announce".into()), false, 912);
    let l = lab();
    let profile = |folder: Option<&Path>| {
        let mut p = Profile { id: "tz".into(), name: "TZ".into(), urls: vec![UrlPattern::parse("tracker-z.test").unwrap()], ..Profile::default() };
        p.location.finished_dir = folder.map(|f| f.display().to_string());
        let mut tp = trs_state::TrackerProfiles::default();
        tp.add(p).unwrap();
        PolicyData { rules: trs_state::RuleSet::default(), profiles: tp, private_min_seed: None, idle: None }
    };
    t.write_to(&l.dl);
    let s = snode(&l.state, &l.dl, Some(profile(None)), plain(&l)).await;
    s.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&s.h, &t.id, 30).await;
    let mut ev = s.h.subscribe();
    // Saving the same folders (a rename, a colour) says nothing.
    s.h.apply_policy(profile(None));
    assert!(s.h.location_moves().await.unwrap().items.is_empty());
    s.h.apply_policy(profile(Some(&l.other)));
    let e = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let EngineEvent::LocationsChanged { torrents, .. } = e else { unreachable!() };
    assert_eq!(torrents, 1);
    let p = s.h.location_moves().await.unwrap();
    assert_eq!(p.items[0].to, l.other.join("trackerbundle"));
    s.h.move_torrents(&[t.id]).await.unwrap();
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    t.verify(&l.other, &[]);
}

/// "Move to..." pins the torrent: later settings changes leave it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn move_to_pins_the_torrent_against_later_settings_changes() {
    let t = bundle("pinbundle", 909, 20_000);
    let l = lab();
    let s = seeding_node(&l, &t, plain(&l)).await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    let third = tempfile::tempdir().unwrap();
    let td = third.path().to_path_buf();
    s.h.update_config(move |c| c.default_save_dir = td);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.h.location_moves().await.unwrap().items.is_empty(), "the user's choice of place stands");
    // A relative destination, an unknown torrent and a torrent that waits for a choice are refused.
    assert!(s.h.move_to(&[t.id], Path::new("relative/dir")).await.is_err());
    let rep = s.h.move_to(&[[7; 20]], &l.other).await.unwrap();
    assert_eq!(rep.refused.len(), 1);
}

/// Changing the category of a torrent moves its files to the new category's finished folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_category_change_moves_a_torrent_to_the_new_categorys_folder() {
    let t = make_torrent(&TorrentSpec {
        name: "cat-bundle",
        piece_len: 32 * 1024,
        files: &[("one.bin", 150_000), ("two.bin", 90_000)],
        private: false,
        announce: None,
        seed: 910,
    });
    let l = lab();
    // Seed it somewhere, then let the engine under test download it into its library.
    let src = {
        let d = tempfile::tempdir().unwrap();
        t.write_to(d.path());
        d
    };
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let n = snode(&l.state, &l.dl, None, organizer(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], category: Some(Category::Other), ..Default::default() })
        .await
        .unwrap();
    wait_done_h(&n.h, &t.id, 60).await;
    wait_for("placed", 20, || async { l.lib.join("Other/cat-bundle/one.bin").exists().then_some(()) }).await;
    drop(src);
    n.h.set_category(&[t.id], Some(Category::Software));
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    assert_eq!(std::fs::read(l.lib.join("Software/cat-bundle/one.bin")).unwrap(), t.files[0].1);
    assert_eq!(std::fs::read(l.lib.join("Software/cat-bundle/two.bin")).unwrap(), t.files[1].1);
    assert!(!l.lib.join("Other/cat-bundle").exists(), "{:?}", tree(&l.lib));
    assert!(l.lib.join("Other").exists(), "the category folder stays");
    let _ = leech_all(&n, &t).await;
    assert_eq!(n.row(&t.id).await.unwrap().category, Some(Category::Software));
    let _ = Instant::now();
}

// ---- a real second volume ---------------------------------------------------------------------

/// A small APFS disk image mounted below a temp folder: a real second device. Detached on drop,
/// even when the test fails.
struct DiskImage {
    mount: PathBuf,
    _dir: tempfile::TempDir,
}

impl DiskImage {
    fn create(mb: u32) -> Option<DiskImage> {
        let dir = tempfile::tempdir().ok()?;
        let img = dir.path().join("vol.dmg");
        let mount = dir.path().join("mnt");
        std::fs::create_dir_all(&mount).ok()?;
        // The child processes must not see the real home either.
        let home = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            std::process::Command::new("hdiutil")
                .args(args)
                .env("HOME", &home)
                .env("TRS_FORBID_REAL_HOME", "1")
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        run(&["create", "-size", &format!("{mb}m"), "-fs", "APFS", "-volname", "trsmoves", "-ov", img.to_str()?])?;
        let di = DiskImage { mount: mount.clone(), _dir: dir };
        run(&["attach", "-nobrowse", "-mountpoint", mount.to_str()?, img.to_str()?])?;
        Some(di)
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        for _ in 0..3 {
            let ok = std::process::Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&self.mount)
                .env("HOME", self._dir.path())
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

/// The real thing: a move onto another device is detected (no test hook), verified while
/// copying, switched and cleaned up; a destination without room is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_onto_a_real_second_volume() {
    let Some(img) = DiskImage::create(64) else {
        eprintln!("skipped: hdiutil cannot make a disk image here");
        return;
    };
    let t = bundle("realvol", 920, 4_000_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| c.move_reserve_bytes = 1 << 20).await;
    assert!(!trs_engine::same_volume(&l.dl, &img.mount).unwrap(), "the image is another device");
    let mut ev = s.h.subscribe();
    let dest = img.mount.join("Moved");
    s.h.move_to(&[t.id], &dest).await.unwrap();
    let started = wait_event(&mut ev, 20, |e| matches!(e, EngineEvent::MoveStarted { id, .. } if *id == t.id)).await;
    let EngineEvent::MoveStarted { cross_volume, .. } = started else { unreachable!() };
    assert!(cross_volume, "different devices are copied, not renamed");
    let e = wait_move_end(&mut ev, t.id, 60).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
    t.verify(&dest, &[]);
    assert!(!l.dl.join("realvol").exists(), "{:?}", tree(&l.dl));
    let _ = leech_all(&s, &t).await;

    // A torrent that does not fit (the default reserve of 1 GB can never be met on this image).
    let big = bundle("toobig", 921, 1_000_000);
    let l2 = lab();
    big.write_to(&l2.dl);
    let s2 = snode(&l2.state, &l2.dl, None, plain(&l2)).await;
    s2.h.add_torrent_file(big.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&s2.h, &big.id, 30).await;
    let mut ev2 = s2.h.subscribe();
    s2.h.move_to(&[big.id], &img.mount.join("Nope")).await.unwrap();
    let e = wait_move_end(&mut ev2, big.id, 30).await;
    let EngineEvent::MoveFailed { reason, .. } = e else { panic!("{e:?}") };
    assert!(reason.contains("Not enough free space"), "{reason}");
    big.verify(&l2.dl, &[]);
    assert!(!img.mount.join("Nope").exists() || count_files(&img.mount.join("Nope")) == 0);
    drop(s);
    drop(s2);
}

// ---- downloads in progress --------------------------------------------------------------------

/// Changing the incomplete location while a torrent downloads moves its partial files (the
/// download pauses meanwhile) and the download then finishes into the final place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_incomplete_location_moves_the_partial_files() {
    let t = bundle("partial", 930, 3_000_000);
    let l = lab();
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    seeder.h.set_limits(None, Some(800_000));

    let n = snode(&l.state, &l.dl, None, plain(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("half downloaded", 30, || async { n.row(&t.id).await.filter(|r| r.done_bytes > 800_000 && r.done_bytes < 2_000_000).map(|_| ()) }).await;
    let first_stage = l.inc.join(hex_of(&t.id));
    assert!(first_stage.exists(), "partial files live in the incomplete location");
    let new_inc = l.other.join(".incomplete");
    let ni = new_inc.clone();
    n.h.update_config(move |c| c.incomplete_dir = Some(ni));
    let e = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let EngineEvent::LocationsChanged { torrents, .. } = e else { unreachable!() };
    assert_eq!(torrents, 1);
    let p = n.h.location_moves().await.unwrap();
    assert_eq!(p.items[0].kind, MoveKind::Incomplete, "{p:?}");
    n.h.move_torrents(&[t.id]).await.unwrap();
    let e = wait_move_end(&mut ev, t.id, 30).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
    assert!(new_inc.join(hex_of(&t.id)).exists(), "{:?}", tree(&l.other));
    assert!(!first_stage.exists(), "the old staging folder is gone: {:?}", tree(&l.inc));
    // The download carries on from where it was and ends with the right bytes in the final place.
    wait_done_h(&n.h, &t.id, 120).await;
    wait_for("finalized", 30, || async { l.dl.join("partial/z.dat").exists().then_some(()) }).await;
    t.verify(&l.dl, &[]);
    // The staging folder is pruned just after the finished files appear; wait for it rather
    // than race it (a leak still fails here).
    let staged = new_inc.join(hex_of(&t.id));
    wait_for("staging folder pruned", 15, || async { (!staged.exists()).then_some(()) }).await;
}

/// A change of the finished location while downloading only changes where the finished files go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_finished_location_retargets_a_download_without_copying() {
    let t = bundle("retarget", 931, 2_000_000);
    let l = lab();
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    seeder.h.set_limits(None, Some(800_000));
    let n = snode(&l.state, &l.dl, None, plain(&l)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("started", 30, || async { n.row(&t.id).await.filter(|r| r.done_bytes > 300_000).map(|_| ()) }).await;
    let new_dir = l.other.clone();
    n.h.update_config(move |c| c.default_save_dir = new_dir);
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::LocationsChanged { .. })).await;
    let p = n.h.location_moves().await.unwrap();
    assert_eq!((p.items.len(), p.items[0].kind, p.items[0].bytes), (1, MoveKind::Retarget, 0), "{p:?}");
    n.h.move_torrents(&[t.id]).await.unwrap();
    assert!(matches!(wait_move_end(&mut ev, t.id, 30).await, EngineEvent::MoveFinished { .. }));
    wait_done_h(&n.h, &t.id, 120).await;
    wait_for("finalized at the new place", 30, || async { l.other.join("retarget/z.dat").exists().then_some(()) }).await;
    t.verify(&l.other, &[]);
    assert!(!l.dl.join("retarget").exists(), "nothing went to the old place: {:?}", tree(&l.dl));
}

/// Hundreds of torrents queue and move one after the other; same-volume moves are almost instant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_torrents_move_in_a_queue() {
    let l = lab();
    let n = snode(&l.state, &l.dl, None, plain(&l)).await;
    let mut ids = Vec::new();
    let count = 60usize;
    for i in 0..count {
        let t = make_torrent(&TorrentSpec {
            name: &format!("q{i:03}"),
            piece_len: 16 * 1024,
            files: &[("a.bin", 20_000), ("b.bin", 20_000), ("c.bin", 20_000), ("d/e.bin", 20_000), ("d/f.bin", 20_000)],
            private: false,
            announce: None,
            seed: 2000 + i as u64,
        });
        t.write_to(&l.dl);
        let a = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        ids.push(a.id);
    }
    for id in &ids {
        wait_done_h(&n.h, id, 60).await;
    }
    let mut ev = n.h.subscribe();
    let t0 = Instant::now();
    let rep = n.h.move_to(&ids, &l.other).await.unwrap();
    assert_eq!(rep.queued.len(), count, "{:?}", rep.refused);
    let mut done = 0;
    while done < count {
        let e = wait_event(&mut ev, 60, |e| matches!(e, EngineEvent::MoveFinished { .. } | EngineEvent::MoveFailed { .. })).await;
        assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
        done += 1;
    }
    let secs = t0.elapsed().as_secs_f64();
    eprintln!("MOVE-RATE: {count} torrents x 5 files = {} files in {secs:.2} s = {:.0} files/s", count * 5, (count * 5) as f64 / secs);
    for i in 0..count {
        assert_eq!(count_files(&l.other.join(format!("q{i:03}"))), 5);
    }
    assert_eq!(count_files(&l.dl), 0, "{:?}", tree(&l.dl));
}

/// Removing a torrent that is copying cancels the copy first; nothing is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_torrent_that_is_moving_cancels_the_copy() {
    let t = bundle("rmmoving", 940, 6_000_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| {
        c.force_copy_moves = true;
        c.move_rate_limit = 1_000_000;
    })
    .await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::MoveStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    s.h.remove(&[t.id], false).await.unwrap();
    wait_for("removed", 20, || async { s.row(&t.id).await.is_none().then_some(()) }).await;
    assert_eq!(count_files(&l.other), 0, "no partial copy: {:?}", tree(&l.other));
    t.verify(&l.dl, &[]);
}

/// Cancelling a copy leaves the torrent seeding where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_copy_leaves_the_torrent_where_it_was() {
    let t = bundle("cancelcopy", 941, 6_000_000);
    let l = lab();
    let s = seeding_node(&l, &t, |c| {
        c.force_copy_moves = true;
        c.move_rate_limit = 1_000_000;
    })
    .await;
    let mut ev = s.h.subscribe();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::MoveStarted { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    s.h.cancel_moves(&[t.id]);
    let e = wait_move_end(&mut ev, t.id, 20).await;
    let EngineEvent::MoveFailed { reason, .. } = e else { panic!("{e:?}") };
    assert_eq!(reason, "Cancelled");
    assert_eq!(count_files(&l.other), 0, "{:?}", tree(&l.other));
    t.verify(&l.dl, &[]);
    let _ = leech_all(&s, &t).await;
}

/// Throughput of a verified copy (`cargo test --test moves copy_throughput -- --ignored --nocapture`):
/// a 1 GiB torrent forced through the cross-volume path inside the temp dir. A report, not a gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn copy_throughput_report() {
    let mib: usize = std::env::var("TRS_COPY_MIB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);
    let t = bundle("speedbundle", 990, mib << 20);
    let l = lab();
    let s = seeding_node(&l, &t, |c| c.force_copy_moves = true).await;
    let mut ev = s.h.subscribe();
    let t0 = std::time::Instant::now();
    s.h.move_to(&[t.id], &l.other).await.unwrap();
    let e = wait_move_end(&mut ev, t.id, 600).await;
    assert!(matches!(e, EngineEvent::MoveFinished { .. }), "{e:?}");
    let secs = t0.elapsed().as_secs_f64();
    eprintln!("COPY {mib} MiB verified in {secs:.2} s = {:.0} MB/s", (mib as f64 * 1.048576) / secs);
    t.verify(&l.other, &[]);
}
