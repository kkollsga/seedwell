//! Faults that clear by themselves, stalled downloads, the verification gate and the bulk
//! pause / resume (docs/qbittorrent-parity.md #2 to #5). "Minutes" are shortened with
//! `EngineConfig::retry_minute`; the arithmetic of the schedules is unit-tested in
//! `session/recover.rs` and `queue.rs`.
use crate::common;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;

async fn seeded(t: &TestTorrent, cfg: impl FnOnce(&mut EngineConfig)) -> Node {
    let seeder = node(cfg).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    seeder
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Create the folders and empty files of `t` below `dir`, read-only: every write is refused.
fn lock_files(t: &TestTorrent, dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for i in 0..t.files.len() {
        let p = t.file_path(dir, i);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"").unwrap();
        if !t.files[i].1.is_empty() {
            chmod(&p, 0o444);
            files.push(p);
        }
    }
    files
}

// ---- #2 disk errors --------------------------------------------------------------------------

/// A write that is refused does not fail the torrent for good: it stops asking for blocks, keeps
/// its actor (and what it has), says why on the row, does not hold a queue slot, and tries again
/// by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_write_waits_and_retries_by_itself() {
    if running_as_root() {
        return;
    }
    let t = standard_torrent(2_000_000, None, false, 901);
    let other = standard_named("other", 600_000, None, false, 902);
    let seeder = seeded(&t, |_| {}).await;
    other.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(other.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &other.id, 30).await;

    // One "minute" is 250 ms: the first retry comes after 10 of them.
    let leech = node(|c| {
        c.retry_minute = Duration::from_millis(250);
        c.max_active_downloads = 1;
    })
    .await;
    let locked = lock_files(&t, leech.dir.path());
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    // The second torrent would be queued behind the first (limit 1) ...
    leech.h.add_torrent_file(other.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();

    let row = wait_for("the refusal shows on the row", 20, || async {
        let r = leech.row(&t.id).await?;
        r.hold_note.is_some().then_some(r)
    })
    .await;
    let note = row.hold_note.clone().unwrap();
    assert!(note.starts_with("Not allowed to write — retrying in "), "{note}");
    assert_eq!(row.state, TorrentState::Downloading, "not Error: it is still a live torrent");
    assert_eq!(row.waiting_note().unwrap().text, note, "one text for every front end");
    let spawned = leech.h.diagnostics().actors_spawned;
    // ... but a torrent whose disk refuses writes does not hold the slot: the other one finishes.
    wait_done(&leech, &other.id, 30).await;
    other.verify(leech.dir.path(), &[]);
    assert_eq!(leech.state(&t.id).await, Some(TorrentState::Downloading));

    // The user fixes the permissions; the next retry restarts the actor, which redials the peers it
    // had, and the download completes.
    for p in &locked {
        chmod(p, 0o644);
    }
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.diagnostics().actors_spawned > spawned, "the retry restarted the actor");
}

/// Not enough room at start-up is a wait, not a dead end: the torrent retries and the space
/// check of the new start decides. (The free space is the engine's test override.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_enough_space_is_retried_when_room_appears() {
    let t = standard_torrent(1_000_000, None, false, 903);
    let seeder = seeded(&t, |_| {}).await;
    let leech = node(|c| {
        c.retry_minute = Duration::from_millis(100);
        c.smart_add.free_space_override = Some(100_000);
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let r = wait_for("an error with a retry", 20, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Error && r.next_retry_ms.is_some()).then_some(r)
    })
    .await;
    assert!(r.error.as_deref().unwrap_or("").starts_with("Not enough space"), "{:?}", r.error);
    leech.h.update_config(|c| c.smart_add.free_space_override = None);
    wait_for("it started again", 20, || async { (leech.state(&t.id).await? == TorrentState::Downloading).then_some(()) }).await;
    leech.h.add_peers(t.id, vec![seeder.addr()]);
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
}

/// An unplugged drive: torrents on it say "Drive not connected" the moment the app reports the
/// unmount, finished ones included (they have no actor to find out), and come back on the mount
/// without a recheck.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unplugged_drive_is_noticed_in_bulk_and_comes_back_without_rechecks() {
    const N: u64 = 60;
    let vol = tempfile::tempdir().unwrap();
    let mnt = vol.path().join("Disk");
    let off = vol.path().join("Disk.off");
    let save = mnt.join("dl");
    std::fs::create_dir_all(&save).unwrap();
    let n = node({
        let save = save.clone();
        move |c| {
            c.default_save_dir = save;
            // The periodic retry must not be what makes this work.
            c.retry_minute = Duration::from_secs(3600);
        }
    })
    .await;
    let mut ids = Vec::new();
    for i in 0..N {
        let t = make_torrent(&TorrentSpec { name: &format!("tiny{i}"), piece_len: 16384, files: &[("f.bin", 20_000)], private: false, announce: None, seed: 1000 + i });
        t.write_to(&save);
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        ids.push(t.id);
    }
    for id in &ids {
        wait_done(&n, id, 30).await;
    }
    let before = n.h.diagnostics();

    // Unplug.
    std::fs::rename(&mnt, &off).unwrap();
    let t0 = Instant::now();
    n.h.volumes_changed();
    wait_for("all marked", 10, || async {
        let rows = n.h.snapshot().await.ok()?.rows;
        rows.iter().all(|r| r.state == TorrentState::MissingData && r.error.as_deref() == Some("Drive not connected")).then_some(())
    })
    .await;
    eprintln!("{N} torrents flipped in {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert!(n.h.diagnostics().replans - before.replans <= 3, "one plan for the whole batch");

    // Plug it back in.
    std::fs::rename(&off, &mnt).unwrap();
    n.h.volumes_changed();
    wait_for("all back", 10, || async {
        let rows = n.h.snapshot().await.ok()?.rows;
        rows.iter().all(|r| matches!(r.state, TorrentState::LowPower | TorrentState::Seeding)).then_some(())
    })
    .await;
    let after = n.h.diagnostics();
    assert_eq!(after.rechecks, before.rechecks, "no recheck storm after a reconnect");
    assert_eq!(after.actors_spawned, before.actors_spawned, "finished torrents need no actor to come back");
}

/// Missing data is retried on the 1, 5, 15, 60 minute schedule (here one minute is 100 ms) and
/// the retry keeps failing quietly while the drive is away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_data_backs_off_instead_of_a_flat_minute() {
    let vol = tempfile::tempdir().unwrap();
    let mnt = vol.path().join("Disk");
    let off = vol.path().join("Disk.off");
    let save = mnt.join("dl");
    std::fs::create_dir_all(&save).unwrap();
    let t = standard_torrent(8_000_000, None, false, 904);
    let seeder = seeded(&t, |c| c.up_limit = Some(1_500_000)).await;
    let leech = node({
        let save = save.clone();
        move |c| {
            c.default_save_dir = save;
            c.retry_minute = Duration::from_millis(100);
        }
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("some data", 30, || async { (leech.row(&t.id).await?.done_bytes > 1_000_000).then_some(()) }).await;
    // (Open file handles survive the rename: the app's unmount notification is what tells the engine.)
    std::fs::rename(&mnt, &off).unwrap();
    // The storage layer refuses to re-create a missing volume only below /Volumes; a plain file
    // where the folder was (ENOTDIR) is the same "not there" for this test.
    std::fs::write(&mnt, b"").unwrap();
    leech.h.volumes_changed();
    wait_for("missing data", 20, || async { (leech.state(&t.id).await? == TorrentState::MissingData).then_some(()) }).await;

    // Count the attempts (each one starts an actor) for a few seconds.
    let mut stamps = Vec::new();
    let mut last = leech.h.diagnostics().actors_spawned;
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(9) && stamps.len() < 4 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let now = leech.h.diagnostics().actors_spawned;
        if now > last {
            stamps.push(began.elapsed());
            last = now;
        }
    }
    eprintln!("attempts at {stamps:?}; row {:?}", leech.row(&t.id).await.map(|r| (r.state, r.error, r.next_retry_ms)));
    assert!(stamps.len() >= 3, "retries happened: {stamps:?}");
    let gaps: Vec<f64> = stamps.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
    // Steps 5, 15, 60 "minutes" = 0.5 s, 1.5 s, 6 s (the first gap may include the 1-minute step).
    assert!(gaps[0] >= 0.35, "second wait is about 5 minutes: {gaps:?}");
    if gaps.len() > 1 {
        assert!(gaps[1] >= 1.2 && gaps[1] > gaps[0] * 1.8, "third wait is about 15 minutes: {gaps:?}");
    }

    // Back again: the mount notification resumes it at once, no waiting for the 60 minute step.
    std::fs::remove_file(&mnt).unwrap();
    std::fs::rename(&off, &mnt).unwrap();
    leech.h.volumes_changed();
    leech.h.add_peers(t.id, vec![seeder.addr()]);
    wait_done(&leech, &t.id, 60).await;
    t.verify(&save, &[]);
}

// ---- #3 stalled downloads ----------------------------------------------------------------------

/// A download that moves nothing for a while stops counting toward the limit, the next one
/// starts, the running total stays within twice the limit, and the row says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_download_gives_up_its_queue_slot() {
    let ts: Vec<TestTorrent> = (0..3).map(|i| standard_named(&format!("stall{i}"), 100_000, None, false, 910 + i)).collect();
    let n = node(|c| {
        c.max_active_downloads = 1;
        c.stall_after = Duration::from_millis(900);
    })
    .await;
    for t in &ts {
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    // Nobody has the data. The first one holds the only slot until it has been idle for a while.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(n.state(&ts[0].id).await, Some(TorrentState::Downloading));
    assert_eq!(n.state(&ts[1].id).await, Some(TorrentState::Queued));
    let began = Instant::now();
    wait_for("the second starts", 10, || async { (n.state(&ts[1].id).await? == TorrentState::Downloading).then_some(()) }).await;
    assert!(began.elapsed() >= Duration::from_millis(300), "not before it has been idle for the whole period");
    let r0 = n.row(&ts[0].id).await.unwrap();
    assert!(r0.stalled);
    assert_eq!(r0.waiting_note().unwrap().text, STALLED_TEXT);
    // Once the second is idle too, the third (which counts) takes the one counted slot, and the
    // total stays within twice the limit: the idle ones keep what is left, the earliest first.
    wait_for("the third starts", 15, || async { (n.state(&ts[2].id).await? == TorrentState::Downloading).then_some(()) }).await;
    let running = n.h.snapshot().await.unwrap().rows.iter().filter(|r| r.state == TorrentState::Downloading).count();
    assert_eq!(running, 2, "2 x limit at most");
    assert_eq!(n.state(&ts[0].id).await, Some(TorrentState::Downloading), "the earliest idle one keeps running");

    // Data arrives for the first one: it counts again (after the hold) and says nothing odd.
    let seeder = seeded(&ts[0], |_| {}).await;
    n.h.add_peers(ts[0].id, vec![seeder.addr()]);
    wait_done(&n, &ts[0].id, 30).await;
    ts[0].verify(n.dir.path(), &[]);
}

// ---- #4 verification gate ----------------------------------------------------------------------

/// Adding many torrents whose data already exists verifies at most `verify_slots` at a time;
/// the others wait without an actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_two_torrents_hash_at_once() {
    const N: usize = 10;
    let n = node(|c| c.max_active_downloads = 50).await;
    let mut ts = Vec::new();
    for i in 0..N {
        let t = standard_named(&format!("exist{i}"), 6_000_000, None, false, 920 + i as u64);
        t.write_to(n.dir.path());
        ts.push(t);
    }
    for t in &ts {
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    let mut saw_waiting = false;
    let began = Instant::now();
    loop {
        let rows = n.h.snapshot().await.unwrap().rows;
        let checking = rows.iter().filter(|r| r.state == TorrentState::Checking).count();
        saw_waiting |= rows.iter().any(|r| r.hold_note.as_deref() == Some("Waiting to verify"));
        if rows.iter().all(|r| r.done_bytes == r.total_bytes && r.total_bytes > 0 && r.state != TorrentState::Checking) {
            break;
        }
        let _ = checking;
        assert!(began.elapsed() < Duration::from_secs(60), "never finished");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let d = n.h.diagnostics();
    assert_eq!(d.rechecks, N as u64, "every torrent was verified");
    assert!(d.verify_peak >= 1 && d.verify_peak <= 2, "peak {}", d.verify_peak);
    eprintln!("verify peak {} waiting seen {saw_waiting}", d.verify_peak);
    for t in &ts {
        t.verify(n.dir.path(), &[]);
    }
}

/// Recheck all goes through the same gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_all_is_gated_too() {
    const N: usize = 8;
    let n = node(|c| {
        c.verify_slots = 1;
        c.max_active_downloads = 50;
    })
    .await;
    let mut ids = Vec::new();
    for i in 0..N {
        let t = standard_named(&format!("again{i}"), 3_000_000, None, false, 940 + i as u64);
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        ids.push(t.id);
    }
    for id in &ids {
        wait_done(&n, id, 60).await;
    }
    let before = n.h.diagnostics().rechecks;
    n.h.recheck(&ids);
    wait_for("every recheck ran", 60, || async { (n.h.diagnostics().rechecks >= before + N as u64).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(n.h.diagnostics().verify_peak, 1, "one slot, one at a time");
    for id in &ids {
        wait_done(&n, id, 60).await;
    }
}

// ---- #5 bulk pause / resume --------------------------------------------------------------------

fn magnet(i: u32) -> String {
    let hash: Vec<u8> = (0..20u8).map(|b| b.wrapping_mul(11).wrapping_add(i as u8).wrapping_add((i >> 8) as u8 * 5)).collect();
    format!("magnet:?xt=urn:btih:{}&dn=Bulk.{i}", hex_of(&hash))
}

/// Pause all plans the queue once; Resume all plans once and brings the torrents that need an
/// actor up a couple at a time, like the launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_all_and_resume_all_are_one_pass_and_resume_is_staggered() {
    const N: u32 = 60;
    let n = node(|_| {}).await;
    for i in 0..N {
        n.h.add_magnet(&magnet(500 + i), AddOptions::default()).await.unwrap();
    }
    wait_for("actors up", 30, || async { (n.h.diagnostics().boot_waiting == 0 && n.h.diagnostics().actors_spawned >= u64::from(N)).then_some(()) }).await;

    let r0 = n.h.diagnostics().replans;
    n.h.pause_all();
    wait_for("all paused", 15, || async {
        n.h.snapshot().await.ok()?.rows.iter().all(|r| r.state == TorrentState::Paused).then_some(())
    })
    .await;
    let planned = n.h.diagnostics().replans - r0;
    assert!(planned <= 2, "Pause all planned the queue {planned} times for {N} torrents");

    // Every actor has stopped; resuming must not bring them all back in one instant.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (spawned0, r1) = (n.h.diagnostics().actors_spawned, n.h.diagnostics().replans);
    n.h.resume_all();
    wait_for("resume began", 5, || async { (n.h.diagnostics().replans > r1).then_some(()) }).await;
    let d = n.h.diagnostics();
    assert!(d.replans - r1 <= 2, "Resume all planned {} times", d.replans - r1);
    assert!(d.boot_waiting >= u64::from(N) - 6, "the rest wait for their turn ({} waiting)", d.boot_waiting);
    assert!(d.actors_spawned - spawned0 <= 6, "only the first batch is up at once ({})", d.actors_spawned - spawned0);
    let (mut last, mut max_step, began) = (d.actors_spawned, 0, Instant::now());
    while n.h.diagnostics().boot_waiting > 0 {
        assert!(began.elapsed() < Duration::from_secs(30), "never finished");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = n.h.diagnostics().actors_spawned;
        max_step = max_step.max(now - last);
        last = now;
    }
    assert!(max_step <= 4, "never more than a small batch between two looks ({max_step})");
    assert!(began.elapsed() >= Duration::from_millis(200 * u64::from(N / 2 - 6)), "spread out in time: {:?}", began.elapsed());
    assert_eq!(n.h.diagnostics().actors_spawned - spawned0, u64::from(N), "every torrent came back");
    for r in n.h.snapshot().await.unwrap().rows {
        assert_ne!(r.state, TorrentState::Paused);
    }
}
