//! Developer mode (SPEC 8.7): what the log holds for a private-style tracker, that the passkey and
//! the `key` never reach the file, and that off means nothing is written.
use crate::common;
use std::path::Path;
use std::time::Duration;

use common::*;
use serde_json::Value;
use trs_engine::*;

const PASSKEY: &str = "0123456789abcdef0123456789abcdef";

fn lines(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("trackers.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON object per line"))
        .collect()
}

fn count(dir: &Path, f: impl Fn(&Value) -> bool) -> usize {
    lines(dir).iter().filter(|v| f(v)).count()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchanges_are_logged_with_masked_secrets_and_stop_when_switched_off() {
    let tracker = MockTracker::start(60).await;
    let url = format!("{}?passkey={PASSKEY}", tracker.url());
    let t = standard_torrent(200_000, Some(url), false, 301);
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.developer_mode = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started at the tracker", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    let d = dir.clone();
    wait_for("the started line", 10, || async { (count(&d, |v| s(v, "kind") == "announce" && s(v, "event") == "started") == 1).then_some(()) }).await;

    let started = lines(&dir).into_iter().find(|v| s(v, "kind") == "announce").unwrap();
    assert_eq!(s(&started, "torrent"), hex_of(&t.id));
    assert_eq!(s(&started, "id"), &hex_of(&t.id)[..8]);
    assert_eq!(s(&started, "transport"), "http");
    assert_eq!(s(&started, "family"), "IPv4");
    assert_eq!(s(&started, "ip"), "127.0.0.1");
    let req = &started["request"];
    let rurl = s(req, "url");
    assert!(rurl.contains("passkey=••••••"), "{rurl}");
    for p in ["info_hash=%", "peer_id=", "port=", "uploaded=0", "downloaded=0", "left=", "numwant=", "key=••••••", "compact=1", "event=started"] {
        assert!(rurl.contains(p), "{p} missing from {rurl}");
    }
    let headers: Vec<&str> = req["headers"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(headers.iter().any(|h| h.starts_with("User-Agent: Seedwell/")), "{headers:?}");
    assert!(headers.iter().any(|h| h.starts_with("Host: 127.0.0.1")), "{headers:?}");
    let resp = &started["response"];
    assert_eq!(resp["status"], 200);
    assert_eq!(resp["fields"]["interval"], "60");
    assert_eq!(resp["fields"]["min_interval"], "60");
    assert!(started["duration_ms"].is_u64());
    let next = &started["next"];
    assert!(next["why"].as_str().unwrap().contains("interval 60 s"), "{next}");
    let in_s = next["in_s"].as_i64().unwrap();
    assert!((50..=65).contains(&in_s), "next announce in {in_s} s");
    assert!(count(&dir, |v| s(v, "decision") == "session_opened") >= 1, "the tier walk is logged");

    // Pause: `stopped`, no next announce.
    node.h.pause(&[t.id]);
    let d = dir.clone();
    wait_for("the stopped line", 15, || async { (count(&d, |v| s(v, "event") == "stopped") == 1).then_some(()) }).await;
    let stopped = lines(&dir).into_iter().find(|v| s(v, "event") == "stopped").unwrap();
    assert!(stopped.get("next").is_none(), "{stopped}");

    // The secrets: neither the passkey nor the client key is in any file.
    let key = tracker.log_for(&t.id)[0].key.clone().unwrap();
    let all = std::fs::read_to_string(dir.join("trackers.log")).unwrap();
    assert!(!all.contains(PASSKEY), "passkey leaked");
    assert!(!all.contains(&key), "client key leaked");
    assert!(all.contains("info_hash=%"), "info_hash may be logged");

    // Off: the very next exchange is not written. The existing log stays.
    let before = lines(&dir).len();
    node.h.update_config(|c| c.developer_mode = false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    node.h.resume(&[t.id]);
    wait_for("a second started at the tracker", 15, || async {
        (tracker.log_for(&t.id).iter().filter(|a| a.event.as_deref() == Some("started")).count() == 2).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(lines(&dir).len(), before, "nothing is written once the mode is off");
    assert!(!std::fs::read_to_string(dir.join("trackers.log")).unwrap().contains(PASSKEY));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_written_when_the_mode_is_off() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(200_000, Some(format!("{}?passkey={PASSKEY}", tracker.url())), false, 302);
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().join("logs");
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started at the tracker", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    node.h.pause(&[t.id]);
    wait_for("stopped at the tracker", 15, || async {
        tracker.log_for(&t.id).iter().any(|a| a.event.as_deref() == Some("stopped")).then_some(())
    })
    .await;
    assert!(!dir.exists(), "no log folder, no file, while the mode is off");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_tracker_is_logged_with_its_error_and_backoff() {
    // A port nothing listens on: connection refused.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let t = standard_torrent(200_000, Some(format!("http://{addr}/announce?passkey={PASSKEY}")), false, 303);
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.developer_mode = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let d = dir.clone();
    wait_for("the failed started", 20, || async { (count(&d, |v| s(v, "kind") == "announce") >= 1).then_some(()) }).await;
    let e = lines(&dir).into_iter().find(|v| s(v, "kind") == "announce").unwrap();
    let err = e["response"]["error"].as_str().unwrap();
    assert!(err.contains("connection"), "{err}");
    let why = e["next"]["why"].as_str().unwrap();
    assert!(why.contains("backoff step 1"), "{why}");
    let all = std::fs::read_to_string(dir.join("trackers.log")).unwrap();
    assert!(!all.contains(PASSKEY));
}

/// Put an `announce-list` with one tier of `urls` into a torrent file (the info-hash is unchanged).
fn with_tier(bytes: &[u8], urls: &[&str]) -> Vec<u8> {
    let mut list = String::from("13:announce-listll");
    for u in urls {
        list.push_str(&format!("{}:{u}", u.len()));
    }
    list.push_str("ee");
    let at = bytes.windows(6).position(|w| w == b"4:info").unwrap();
    let mut out = bytes[..at].to_vec();
    out.extend(list.as_bytes());
    out.extend(&bytes[at..]);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_failover_is_logged_dead_tracker_first_then_the_one_behind_it() {
    let good = MockTracker::start(60).await;
    // The dead tracker is spelled `localhost` so that it is another host for the gate than the mock
    // on 127.0.0.1: the host circuit breaker (three torrents failing on a host pause it, whatever
    // the port) must not pause the good tracker along with the dead one.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("localhost:{}", l.local_addr().unwrap().port())
    };
    let (dead_url, good_url) = (format!("http://{dead}/announce?passkey={PASSKEY}"), format!("{}?passkey={PASSKEY}", good.url()));
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.developer_mode = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    // The order inside a tier is shuffled once per torrent: with twelve torrents, one of them is certain to (4095 in 4096)
    // tries the dead tracker first.
    let mut ids = Vec::new();
    for seed in 0..12 {
        let t = standard_torrent(200_000, Some(good_url.clone()), false, 400 + 2 * seed);
        let bytes = with_tier(&t.bytes, &[&dead_url, &good_url]);
        node.h.add_torrent_file(bytes, AddOptions::default()).await.unwrap();
        ids.push(hex_of(&t.id));
    }
    let (d, dead_host, ids2) = (dir.clone(), dead.to_string(), ids.clone());
    let dump = dir.clone();
    let found = tokio::time::timeout(Duration::from_secs(60), wait_for("a failover", 55, || async {
        let ls = lines(&d);
        ids2.iter().find(|id| {
            let mine: Vec<&Value> = ls.iter().filter(|v| s(v, "torrent") == id.as_str()).collect();
            let failed = mine.iter().any(|v| s(v, "kind") == "announce" && s(v, "host") == dead_host && v["response"]["error"].is_string());
            let behind = mine.iter().any(|v| s(v, "kind") == "announce" && s(v, "host") != dead_host && v["response"]["status"] == 200);
            failed && behind
        }).cloned()
    }))
    .await
    .unwrap_or_else(|_| panic!("no failover in:\n{}", std::fs::read_to_string(dump.join("trackers.log")).unwrap_or_default()));
    let ls = lines(&dir);
    let mine: Vec<&Value> = ls.iter().filter(|v| s(v, "torrent") == found).collect();
    let opened = mine.iter().filter(|v| s(v, "decision") == "session_opened").count();
    assert_eq!(opened, 2, "both trackers of the tier were opened, one after the other: {mine:#?}");
    assert!(mine.iter().any(|v| s(v, "decision") == "session_opened" && s(v, "note").contains("tier 1")));
    let all = std::fs::read_to_string(dir.join("trackers.log")).unwrap();
    assert!(!all.contains(PASSKEY));
}

/// A tracker that answers every HTTP request with a bencoded `failure reason`.
async fn rejecting_tracker(reason: String) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else { return };
            let reason = reason.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut n = 0;
                while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf[n..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(k) => n += k,
                    }
                }
                let body = format!("d14:failure reason{}:{reason}e", reason.len());
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_reason_is_logged_masked_and_a_permanent_one_is_a_decision() {
    let addr = rejecting_tracker(format!("Unregistered torrent, bad passkey {PASSKEY} for https://t.example/a/{PASSKEY}/announce")).await;
    let t = standard_torrent(200_000, Some(format!("http://{addr}/announce?passkey={PASSKEY}")), false, 304);
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.developer_mode = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let d = dir.clone();
    wait_for("the rejection", 20, || async {
        (count(&d, |v| s(v, "kind") == "announce") >= 1 && count(&d, |v| s(v, "decision") == "halted") >= 1).then_some(())
    })
    .await;
    let e = lines(&dir).into_iter().find(|v| s(v, "kind") == "announce").unwrap();
    let reason = e["response"]["fields"]["failure_reason"].as_str().unwrap();
    assert!(reason.contains("Unregistered torrent"), "{reason}");
    assert!(e["response"]["error"].as_str().unwrap().contains("tracker failure reason"));
    let all = std::fs::read_to_string(dir.join("trackers.log")).unwrap();
    assert!(!all.contains(PASSKEY), "{all}");
}

/// A minimal BEP 15 tracker: connect and announce, no peers.
async fn udp_tracker() -> std::net::SocketAddr {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut b).await {
            let mut out = Vec::new();
            if n == 16 && b[8..12] == [0, 0, 0, 0] {
                out.extend(0u32.to_be_bytes());
                out.extend(&b[12..16]);
                out.extend(0x1122_3344_5566_7788u64.to_be_bytes());
            } else if n >= 98 && b[8..12] == [0, 0, 0, 1] {
                out.extend(1u32.to_be_bytes());
                out.extend(&b[12..16]);
                out.extend(900u32.to_be_bytes());
                out.extend(2u32.to_be_bytes());
                out.extend(5u32.to_be_bytes());
            } else {
                continue;
            }
            let _ = sock.send_to(&out, from).await;
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_udp_exchange_is_logged_with_its_decoded_fields() {
    let addr = udp_tracker().await;
    let t = standard_torrent(200_000, Some(format!("udp://{addr}/announce")), false, 305);
    let logs = tempfile::tempdir().unwrap();
    let dir = logs.path().to_path_buf();
    let d2 = dir.clone();
    let node = node(move |c| {
        c.trackers_enabled = true;
        c.developer_mode = true;
        c.devlog_dir = Some(d2);
    })
    .await;
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let d = dir.clone();
    wait_for("the UDP announce", 20, || async { (count(&d, |v| s(v, "kind") == "announce" && s(v, "transport") == "udp") >= 1).then_some(()) }).await;
    let e = lines(&dir).into_iter().find(|v| s(v, "kind") == "announce" && s(v, "transport") == "udp").unwrap();
    let f = &e["request"]["fields"];
    assert_eq!(f["announce.action"], "1 (announce)");
    assert_eq!(f["announce.info_hash"], hex_of(&t.id));
    assert_eq!(f["announce.event"], "2 (started)");
    assert_eq!(f["announce.key"], "••••••", "the client key is masked");
    assert!(f["connection_id"].as_str().unwrap().contains("1122334455667788"));
    assert_eq!(f["connect.used"], "new handshake");
    let r = &e["response"]["fields"];
    assert_eq!((r["interval"].as_str(), r["complete"].as_str(), r["incomplete"].as_str()), (Some("900"), Some("5"), Some("2")));
    assert_eq!(s(&e, "ip"), "127.0.0.1");
    assert!(e["next"]["why"].as_str().unwrap().contains("interval 900 s"));
}
