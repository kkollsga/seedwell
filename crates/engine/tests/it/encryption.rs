//! Peer-wire encryption (MSE): what each pair of policies does.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::config::EncryptionPolicy as Enc;
use trs_engine::*;

async fn pair(seed_policy: Enc, leech_policy: Enc, seed: u64) -> (Node, Node, TestTorrent) {
    let t = standard_torrent(1_500_000, None, false, seed);
    let seeder = node(|c| c.encryption = seed_policy).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|c| c.encryption = leech_policy).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    (seeder, leech, t)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefer_and_prefer_open_with_the_encrypted_handshake() {
    let (seeder, leech, t) = pair(Enc::Prefer, Enc::Prefer, 301).await;
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.diagnostics().mse_dialed >= 1);
    assert!(seeder.h.diagnostics().mse_accepted >= 1);
    assert_eq!(leech.h.diagnostics().mse_fallbacks, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_carries_the_whole_torrent_over_rc4() {
    let (seeder, leech, t) = pair(Enc::Require, Enc::Require, 302).await;
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(seeder.h.diagnostics().mse_accepted >= 1 && leech.h.diagnostics().mse_dialed >= 1);
    // A plain leecher is not served by a seeder that requires encryption.
    let plain = node(|c| c.encryption = Enc::Off).await;
    plain.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(plain.row(&t.id).await.unwrap().done_bytes == 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_without_encryption_is_redialled_in_the_clear() {
    // The seeder refuses the encrypted handshake (policy off); the leecher prefers it, fails, and
    // connects plain.
    let (_seeder, leech, t) = pair(Enc::Off, Enc::Prefer, 303).await;
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.diagnostics().mse_fallbacks >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_does_not_fall_back_to_plain() {
    let (_seeder, leech, t) = pair(Enc::Off, Enc::Require, 304).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(leech.row(&t.id).await.unwrap().done_bytes == 0);
    assert_eq!(leech.h.diagnostics().mse_fallbacks, 0);
}

/// Encrypted handshakes work over uTP too (the combination Transmission uses by default).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encryption_over_utp() {
    let t = standard_torrent(1_500_000, None, false, 305);
    let seeder = node(|c| c.encryption = Enc::Prefer).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|c| {
        c.encryption = Enc::Require;
        c.outgoing_tcp = false;
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.diagnostics().utp_dialed >= 1 && leech.h.diagnostics().mse_dialed >= 1);
}

/// A client with many torrents opens all its connections in the same instant; none may be refused
/// for lack of a handshake slot (a refused connection is retried only after a back-off, which
/// left some torrents of a Transmission leecher without a peer for minutes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_simultaneous_encrypted_handshakes_are_all_accepted() {
    let seeder = node(|c| c.encryption = Enc::Prefer).await;
    let leech = node(|c| c.encryption = Enc::Prefer).await;
    let mut ts = Vec::new();
    for i in 0..30u64 {
        let t = make_torrent(&TorrentSpec {
            name: &format!("many{i}.bin"),
            piece_len: 32 * 1024,
            files: &[(&format!("many{i}.bin"), 100_000)],
            private: false,
            announce: None,
            seed: 500 + i,
        });
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        ts.push(t);
    }
    for t in &ts {
        wait_done(&seeder, &t.id, 60).await;
    }
    // Everything is added before the first dial leaves.
    for t in &ts {
        leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    }
    for t in &ts {
        wait_done(&leech, &t.id, 120).await;
    }
    assert_eq!(seeder.h.diagnostics().incoming_refused, 0, "{:?}", seeder.h.diagnostics());
    assert!(seeder.h.diagnostics().mse_accepted >= 30);
}
