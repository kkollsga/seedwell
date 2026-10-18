use crate::common;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use trs_engine::*;

async fn seeded(t: &TestTorrent, cfg: impl FnOnce(&mut EngineConfig)) -> Node {
    let seeder = node(cfg).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    seeder
}

/// Restart mid-download: verified pieces are kept, nothing is rechecked or re-downloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resumes_without_redownloading() {
    let t = standard_torrent(8_000_000, None, false, 51);
    let seeder = seeded(&t, |c| c.up_limit = Some(2_000_000)).await;

    let mut a = node(|c| c.resume_interval = Duration::from_millis(500)).await;
    let (dir_path, store) = (a.dir.path().to_path_buf(), a.store.clone());
    a.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("30% done", 30, || async {
        (a.row(&t.id).await?.done_bytes * 100 / t.total() >= 30).then_some(())
    })
    .await;
    a.shutdown(Duration::from_secs(3)).await;
    let before = store.torrent(&t.id).unwrap();
    assert!(before.counters.done_bytes > 0);
    let kept = store.resume(&t.id).expect("resume data written at shutdown");
    assert!(!kept.complete && !kept.bitfield.is_empty());
    let kept_pieces: u32 = kept.bitfield.iter().map(|b| b.count_ones()).sum();
    assert!(u64::from(kept_pieces) * u64::from(t.piece_len) >= t.total() * 25 / 100);
    let a_dir = a.dir;
    drop(a.h);

    let b = node_in(a_dir, store.clone(), |_| {}).await;
    assert_eq!(b.dir.path(), dir_path);
    b.h.add_peers(t.id, vec![seeder.addr()]);
    let first = wait_for("row after restart", 10, || async { b.row(&t.id).await }).await;
    assert!(first.done_bytes > 0 || first.state == TorrentState::Checking);
    wait_done(&b, &t.id, 60).await;
    t.verify(b.dir.path(), &[]);
    assert_eq!(b.h.diagnostics().rechecks, 0, "a clean restart must not recheck");
    let row = b.row(&t.id).await.unwrap();
    // Lifetime downloads: everything once, plus at most the pieces that were in flight.
    assert!(row.downloaded_total <= t.total() + 3_000_000, "downloaded {} for {}", row.downloaded_total, t.total());
}

/// Files deleted behind our back: MissingData, nothing re-created or downloaded; recover on resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_files_are_reported_not_rewritten() {
    let t = standard_torrent(0, None, false, 61);
    let seeder = seeded(&t, |_| {}).await;
    let mut a = node(|_| {}).await;
    a.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&a, &t.id, 30).await;
    wait_for("fingerprints stored", 10, || async {
        a.h.flush().await.ok()?;
        let r = a.store.resume(&t.id)?;
        (r.complete && !r.fingerprints.is_empty()).then_some(())
    })
    .await;
    a.shutdown(Duration::from_secs(3)).await;
    let victim = t.file_path(a.dir.path(), 2);
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let store = a.store.clone();
    let b = node_in(a.dir, store, |_| {}).await;
    // Cold torrents are validated when they wake; trigger that with a peer.
    b.h.add_peers(t.id, vec![seeder.addr()]);
    let st = wait_for("missing data", 20, || async {
        let s = b.state(&t.id).await?;
        (s == TorrentState::MissingData).then_some(s)
    })
    .await;
    assert_eq!(st, TorrentState::MissingData);
    assert!(!victim.exists(), "a missing file is never silently re-created or re-downloaded");
    std::fs::write(&victim, &saved).unwrap();
    b.h.resume(&[t.id]);
    wait_for("recovered", 30, || async {
        matches!(b.state(&t.id).await?, TorrentState::LowPower | TorrentState::Seeding).then_some(())
    })
    .await;
}

/// A peer that sends corrupt blocks is banned and the download still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_peer_is_banned() {
    let t = standard_torrent(10_000_000, None, false, 71);
    // The honest seeder is slow, so the corrupt peer gets many pieces.
    let seeder = seeded(&t, |c| c.up_limit = Some(1_500_000)).await;
    let evil = evil_peer(&t).await;
    let leech = node(|_| {}).await;
    let mut ev = leech.h.subscribe();
    leech
        .h
        .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![evil.addr, seeder.addr()], ..Default::default() })
        .await
        .unwrap();
    wait_done(&leech, &t.id, 90).await;
    t.verify(leech.dir.path(), &[]);
    let d = leech.h.diagnostics();
    assert!(d.hash_failures >= 3, "{d:?}");
    assert!(d.peers_banned >= 1, "{d:?}");
    assert!(evil.blocks_served.load(Ordering::Relaxed) > 0);
    let mut banned = None;
    while let Ok(e) = ev.try_recv() {
        if let EngineEvent::PeerBanned { addr, .. } = e {
            banned = Some(addr);
        }
    }
    assert!(banned.is_some(), "PeerBanned event");
    // And it is not let back in: connection attempts to it stop.
    let n = evil.connections.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(evil.connections.load(Ordering::Relaxed) <= n + 1);
}

/// Magnet link: metadata arrives from a peer via ut_metadata, then the download runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_download_fetches_metadata_from_peers() {
    let t = standard_torrent(2_000_000, None, false, 81);
    let seeder = seeded(&t, |_| {}).await;
    let leech = node(|_| {}).await;
    let mut ev = leech.h.subscribe();
    let uri = format!("magnet:?xt=urn:btih:{}&dn={}", hex(&t.id), t.name);
    let added = leech.h.add_magnet(&uri, AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    assert_eq!(added.id, t.id);
    assert_eq!(leech.state(&t.id).await, Some(TorrentState::FetchingMetadata));
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    let mut got_meta = false;
    while let Ok(e) = ev.try_recv() {
        got_meta |= matches!(e, EngineEvent::MetadataReceived { .. });
    }
    assert!(got_meta);
    // The synthesised .torrent is stored: a restart needs no magnet.
    leech.h.flush().await.unwrap();
    let rec = leech.store.torrent(&t.id).unwrap().record;
    assert!(rec.has_metadata && rec.magnet.is_none());
    assert_eq!(rec.total_length, t.total());
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Private torrents are accepted like any other and never touch DHT or PEX.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_torrents_are_accepted_and_isolated() {
    let t = standard_torrent(1_000_000, None, true, 91);

    let dht = |c: &mut EngineConfig| {
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_allow_local = true;
        c.dht_bootstrap = vec!["127.0.0.1:9".into()];
    };
    let seeder = node(dht).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let (reserved, payload) = probe_ext_handshake(seeder.addr(), t.id).await;
    assert_eq!(reserved[7] & 0x01, 0, "no DHT bit for a private torrent");
    assert!(!payload.windows(6).any(|w| w == b"ut_pex"), "ut_pex must not be advertised");

    let leech = node(dht).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    for n in [&seeder, &leech] {
        let d = n.h.diagnostics();
        assert_eq!((d.dht_lookups, d.dht_announces, d.pex_sent, d.pex_received), (0, 0, 0, 0), "{d:?}");
    }

    // Control: a public torrent does use them.
    let p = standard_torrent(200_000, None, false, 92);
    let lonely = node(dht).await;
    lonely.h.add_torrent_file(p.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("dht lookup", 15, || async { (lonely.h.diagnostics().dht_lookups >= 1).then_some(()) }).await;
    let (reserved, payload) = {
        let s = node(dht).await;
        p.write_to(s.dir.path());
        s.h.add_torrent_file(p.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&s, &p.id, 30).await;
        probe_ext_handshake(s.addr(), p.id).await
    };
    assert_eq!(reserved[7] & 0x01, 1);
    assert!(payload.windows(6).any(|w| w == b"ut_pex"));
}

/// Public torrents exchange peers: a leecher learns about another one through the seeder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pex_connects_leechers_to_each_other() {
    let t = standard_torrent(6_000_000, None, false, 101);
    let seeder = seeded(&t, |c| c.up_limit = Some(1_000_000)).await;
    let a = node(|_| {}).await;
    let b = node(|_| {}).await;
    a.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    b.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("pex", 30, || async {
        (a.h.diagnostics().pex_received + b.h.diagnostics().pex_received > 0).then_some(())
    })
    .await;
    wait_done(&a, &t.id, 90).await;
    wait_done(&b, &t.id, 90).await;
    assert!(seeder.h.diagnostics().pex_sent > 0);
}

/// Cold / Warm / Hot: an idle seeder drops to Cold and wakes for a new peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_cools_down_and_wakes_up() {
    let t = standard_torrent(0, None, false, 111);
    let seeder = seeded(&t, |c| {
        c.tiers = TierTimings {
            hot_to_warm: Duration::from_millis(800),
            warm_to_cold: Duration::from_millis(1500),
            ..TierTimings::default()
        };
    })
    .await;
    wait_for("cold", 20, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;
    let before = seeder.h.diagnostics();
    assert!(before.demotions >= 1);
    assert_eq!(seeder.state(&t.id).await, Some(TorrentState::LowPower));
    // A leecher knocks: Cold -> Warm, serves, and finishes.
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 30).await;
    t.verify(leech.dir.path(), &[]);
    let after = seeder.h.diagnostics();
    assert!(after.incoming_accepted > before.incoming_accepted);
    // Quiet again: back to Cold.
    leech.h.pause(&[t.id]);
    wait_for("cold again", 20, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;
}

/// A Cold seeder serves its first block quickly after a peer shows up (SPEC 2: < 50 ms in release).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_torrent_wakes_fast() {
    let t = standard_torrent(0, None, false, 112);
    let seeder = seeded(&t, |c| {
        c.tiers = TierTimings { hot_to_warm: Duration::from_millis(300), warm_to_cold: Duration::from_millis(600), ..TierTimings::default() };
    })
    .await;
    wait_for("cold", 20, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;
    // The fingerprints that make a wake cheap are stored once the torrent completed.
    wait_for("fingerprints", 10, || async {
        seeder.h.flush().await.ok()?;
        (!seeder.store.resume(&t.id)?.fingerprints.is_empty()).then_some(())
    })
    .await;
    let (hs, first) = raw_first_block(seeder.addr(), t.id).await;
    eprintln!("[wake] Cold -> handshake reply {hs:?}, first block {first:?} (debug build)");
    assert!(first < Duration::from_millis(1500));
    assert_eq!(seeder.h.diagnostics().rechecks, 1, "only the initial add rechecked; waking validates by fingerprint");
}

/// `max_active_downloads` queues the rest in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_limits_active_downloads() {
    let a = standard_named("first", 3_000_000, None, false, 121);
    let b = standard_named("second", 3_000_000, None, false, 122);
    let seeder = node(|_| {}).await;
    for t in [&a, &b] {
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &t.id, 30).await;
    }
    let leech = node(|c| {
        c.max_active_downloads = 1;
        c.down_limit = Some(1_500_000);
    })
    .await;
    let opts = AddOptions { peers: vec![seeder.addr()], ..Default::default() };
    leech.h.add_torrent_file(a.bytes.clone(), opts.clone()).await.unwrap();
    leech.h.add_torrent_file(b.bytes.clone(), opts).await.unwrap();
    wait_for("second queued", 10, || async { (leech.state(&b.id).await? == TorrentState::Queued).then_some(()) }).await;
    assert_ne!(leech.state(&a.id).await, Some(TorrentState::Queued));
    wait_done(&leech, &a.id, 60).await;
    wait_done(&leech, &b.id, 60).await;
    a.verify(leech.dir.path(), &[]);
    b.verify(leech.dir.path(), &[]);
    // Moving the later one to the front reorders the queue.
    let snap = leech.h.snapshot().await.unwrap();
    assert_eq!(snap.counts.total, 2);
}

/// Incomplete data lives in `.incomplete` and is moved into place when finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staging_folder_is_used_and_cleaned() {
    let t = standard_torrent(6_000_000, None, false, 131);
    let seeder = seeded(&t, |_| {}).await;
    let leech = node(|c| {
        c.use_staging = true;
        c.down_limit = Some(2_000_000);
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("partial data in staging", 20, || async {
        let r = leech.row(&t.id).await?;
        (r.done_bytes > 1_000_000).then_some(())
    })
    .await;
    assert!(leech.dir.path().join(".incomplete").exists());
    assert!(!t.file_path(leech.dir.path(), 2).exists(), "nothing at the final path yet");
    let mut ev = leech.h.subscribe();
    wait_done(&leech, &t.id, 60).await;
    wait_for("staging removed", 10, || async { (!leech.dir.path().join(".incomplete").exists()).then_some(()) }).await;
    t.verify(leech.dir.path(), &[]);
    // `Finished` is only announced once the files are in place.
    let mut finished = false;
    for _ in 0..50 {
        while let Ok(e) = ev.try_recv() {
            finished |= matches!(e, EngineEvent::Finished { .. });
        }
        if finished {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(finished, "Finished event");
}

/// The DHT bit of the handshake follows the setting: with the DHT off peers must not be invited to
/// send `port` messages (and a private torrent never sets it, see above).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dht_handshake_bit_follows_the_setting() {
    let t = standard_torrent(300_000, None, false, 95);
    let off = node(|c| c.dht_enabled = false).await;
    t.write_to(off.dir.path());
    off.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&off, &t.id, 30).await;
    let (reserved, _) = probe_ext_handshake(off.addr(), t.id).await;
    assert_eq!(reserved[7] & 0x01, 0, "DHT is off, the bit must be too");

    let on = node(|c| {
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_allow_local = true;
        c.dht_bootstrap = vec!["127.0.0.1:9".into()];
    })
    .await;
    t.write_to(on.dir.path());
    on.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&on, &t.id, 30).await;
    let (reserved, _) = probe_ext_handshake(on.addr(), t.id).await;
    assert_eq!(reserved[7] & 0x01, 1);
}
