//! Peer exchange follows one rule (SPEC 7B.4): `pex = global && (!private || tracker.allow_pex)`.
//! The handshake advertises `ut_pex` exactly when the rule says so, a `ut_pex` message is used or
//! ignored accordingly, a change applies live, and DHT stays off for private torrents whatever PEX does.
use crate::common;
use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trs_engine::*;
use trs_state::trackers::{Profile, UrlPattern};
use trs_state::{RuleSet, TrackerProfiles};

fn policy(allow_pex: bool) -> PolicyData {
    let mut tp = TrackerProfiles::default();
    tp.add(Profile {
        id: "mock".into(),
        name: "Mock".into(),
        urls: vec![UrlPattern::parse("127.0.0.1").unwrap()],
        allow_pex,
        ..Profile::default()
    })
    .unwrap();
    PolicyData { rules: RuleSet::default(), profiles: tp, private_min_seed: None, idle: None }
}

/// An automatic tracker's entry (no favorite): allowed by the domain of its host.
fn policy_auto(domain: &str) -> PolicyData {
    let mut tp = TrackerProfiles::default();
    tp.set_pex(domain, true).unwrap();
    PolicyData { rules: RuleSet::default(), profiles: tp, private_min_seed: None, idle: None }
}

/// A raw peer that speaks the extension protocol.
struct Raw {
    s: TcpStream,
    /// Reserved bytes of the handshake we got.
    reserved: [u8; 8],
}

impl Raw {
    async fn connect(addr: std::net::SocketAddr, ih: TorrentId) -> (Raw, Vec<u8>) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut out = Vec::new();
        out.extend(b"\x13BitTorrent protocol");
        out.extend([0, 0, 0, 0, 0, 0x10, 0, 0]);
        out.extend(ih);
        // A fresh peer id each time: the engine drops a second connection that claims an id it already has.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        out.extend(format!("-PR0001-{:012}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)).as_bytes());
        s.write_all(&out).await.unwrap();
        let mut hs = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut hs)).await.unwrap().unwrap();
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&hs[20..28]);
        let mut raw = Raw { s, reserved };
        let first = raw.next_ext_handshake(5).await.expect("an extended handshake");
        (raw, first)
    }

    /// The next extended handshake on this connection, if one arrives within `secs`.
    async fn next_ext_handshake(&mut self, secs: u64) -> Option<Vec<u8>> {
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let mut len = [0u8; 4];
            tokio::time::timeout_at(end, self.s.read_exact(&mut len)).await.ok()?.ok()?;
            let n = u32::from_be_bytes(len) as usize;
            let mut body = vec![0u8; n];
            if n > 0 {
                tokio::time::timeout_at(end, self.s.read_exact(&mut body)).await.ok()?.ok()?;
            }
            if body.len() >= 2 && body[0] == 20 && body[1] == 0 {
                return Some(body[2..].to_vec());
            }
        }
    }

    /// Send a `ut_pex` message with one address (the id the engine assigns to `ut_pex` is 2).
    async fn send_pex(&mut self) -> std::io::Result<()> {
        let mut payload = b"d5:added6:".to_vec();
        payload.extend([127, 0, 0, 1, 0, 9]);
        payload.push(b'e');
        let mut m = Vec::new();
        m.extend(((2 + payload.len()) as u32).to_be_bytes());
        m.extend([20, 2]);
        m.extend(&payload);
        self.s.write_all(&m).await
    }
}

fn has_pex(hs: &[u8]) -> bool {
    hs.windows(6).any(|w| w == b"ut_pex")
}

async fn pex_received(n: &Node) -> u64 {
    n.h.diagnostics().pex_received
}

/// One cell of the matrix: does a torrent with these facts use PEX?
async fn cell(global: bool, private: bool, allow: bool) -> (bool, bool) {
    let t = standard_torrent(0, Some("http://127.0.0.1:1/announce".into()), private, 900 + u64::from(global) * 4 + u64::from(private) * 2 + u64::from(allow));
    let n = node(|c| {
        c.pex_enabled = global;
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_allow_local = true;
        c.dht_bootstrap = vec!["127.0.0.1:9".into()];
    })
    .await;
    n.h.apply_policy(policy(allow));
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    let (mut raw, hs) = Raw::connect(n.addr(), t.id).await;
    let before = pex_received(&n).await;
    raw.send_pex().await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let used = pex_received(&n).await > before;
    if private {
        assert_eq!(raw.reserved[7] & 1, 0, "a private torrent never advertises DHT (global {global}, allow {allow})");
        let d = n.h.diagnostics();
        assert_eq!((d.dht_lookups, d.dht_announces), (0, 0));
    }
    (has_pex(&hs), used)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_full_matrix_global_by_public_private_by_tracker_setting() {
    // (global, private, tracker allows) -> PEX advertised and used?
    let table = [
        (false, false, false, false),
        (false, false, true, false),
        (false, true, false, false),
        (false, true, true, false), // the master switch overrides the tracker
        (true, false, false, true),
        (true, false, true, true), // a public torrent ignores the tracker's setting
        (true, true, false, false), // private: off unless the tracker allows it
        (true, true, true, true),
    ];
    for (global, private, allow, want) in table {
        let (advertised, used) = cell(global, private, allow).await;
        assert_eq!((advertised, used), (want, want), "global {global}, private {private}, tracker allows {allow}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_automatic_tracker_can_allow_pex_by_its_domain() {
    let t = standard_torrent(0, Some("http://127.0.0.1:1/announce".into()), true, 920);
    let n = node(|c| c.pex_enabled = true).await;
    t.write_to(n.dir.path());
    // 127.0.0.1 is its own registrable domain.
    n.h.apply_policy(policy_auto("127.0.0.1"));
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    let (_raw, hs) = Raw::connect(n.addr(), t.id).await;
    assert!(has_pex(&hs));
    // Another domain's entry does nothing for it.
    n.h.apply_policy(policy_auto("other.example"));
    wait_for("no longer advertised", 10, || async {
        let (_r, hs) = Raw::connect(n.addr(), t.id).await;
        (!has_pex(&hs)).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_applies_live_to_new_and_existing_connections() {
    let t = standard_torrent(0, Some("http://127.0.0.1:1/announce".into()), true, 930);
    let n = node(|c| c.pex_enabled = true).await;
    n.h.apply_policy(policy(false));
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;

    // Off for this private tracker: not advertised, a PEX message is forbidden.
    let (mut old, hs) = Raw::connect(n.addr(), t.id).await;
    assert!(!has_pex(&hs));

    // The tracker's page turns it on: new connections advertise it, and the one already open is told.
    n.h.apply_policy(policy(true));
    wait_for("advertised to new connections", 10, || async {
        let (_r, hs) = Raw::connect(n.addr(), t.id).await;
        has_pex(&hs).then_some(())
    })
    .await;
    let again = old.next_ext_handshake(5).await.expect("the open connection gets a fresh extended handshake");
    assert!(has_pex(&again));
    let before = pex_received(&n).await;
    old.send_pex().await.unwrap();
    wait_for("pex accepted on the old connection", 5, || async { (pex_received(&n).await > before).then_some(()) }).await;

    // The master switch goes off: at once not advertised, and the open connection's PEX is ignored (it is
    // not punished: the peer only learns late).
    n.h.update_config(|c| c.pex_enabled = false);
    wait_for("not advertised", 10, || async {
        let (_r, hs) = Raw::connect(n.addr(), t.id).await;
        (!has_pex(&hs)).then_some(())
    })
    .await;
    let before = pex_received(&n).await;
    for _ in 0..3 {
        old.send_pex().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(pex_received(&n).await, before, "ignored while the master switch is off");
    assert!(old.send_pex().await.is_ok(), "and the connection is still open");

    // Back on: the tracker's setting decides again for this private torrent (it allows), advertised again.
    n.h.update_config(|c| c.pex_enabled = true);
    wait_for("advertised again", 10, || async {
        let (_r, hs) = Raw::connect(n.addr(), t.id).await;
        has_pex(&hs).then_some(())
    })
    .await;
    // And off for the tracker: gone at once.
    n.h.apply_policy(policy(false));
    wait_for("tracker setting off", 10, || async {
        let (_r, hs) = Raw::connect(n.addr(), t.id).await;
        (!has_pex(&hs)).then_some(())
    })
    .await;
}
