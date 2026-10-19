//! Tracker statistics accuracy, byte for byte (SPEC 7B.2). A loopback tracker records every announce;
//! scripted wire peers count the payload crossing their own socket; the two must agree exactly:
//! `downloaded` is the size of the pieces that passed their hash check, `corrupt` the size of the pieces
//! that failed, `uploaded` the piece payload written to the peer, `left` exact, nothing replayed.
use crate::common;
use crate::common::scripted::*;
use std::time::Duration;

use common::*;
use trs_engine::*;

fn ev(a: &Announce) -> &str {
    a.event.as_deref().unwrap_or("regular")
}

/// The announces of each tracker session (a session starts at `started`).
fn sessions(log: &[Announce]) -> Vec<Vec<Announce>> {
    let mut out: Vec<Vec<Announce>> = Vec::new();
    for a in log {
        if ev(a) == "started" || out.is_empty() {
            out.push(Vec::new());
        }
        out.last_mut().unwrap().push(a.clone());
    }
    out
}

/// Counters never decrease inside a session, and a session starts from zero.
fn assert_session_rules(log: &[Announce]) {
    for s in sessions(log) {
        let mut prev = (0u64, 0u64, 0u64);
        for a in &s {
            let now = (a.uploaded, a.downloaded, a.corrupt.unwrap_or(0));
            assert!(now.0 >= prev.0 && now.1 >= prev.1 && now.2 >= prev.2, "counters decreased: {prev:?} -> {now:?} in {a:?}");
            prev = now;
        }
    }
}

fn find<'a>(log: &'a [Announce], event: &str, nth: usize) -> &'a Announce {
    log.iter().filter(|a| ev(a) == event && a.status == 200).nth(nth).unwrap_or_else(|| panic!("no {event} #{nth} in {log:#?}"))
}

async fn announced(tracker: &MockTracker, id: &TorrentId, event: &str, n: usize) -> Vec<Announce> {
    wait_for(event, 30, || async {
        let l = tracker.log_for(id);
        (l.iter().filter(|a| ev(a) == event && a.status == 200).count() >= n).then_some(l)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloaded_is_the_verified_pieces_and_equals_the_wire() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 701);
    let seeder = scripted_seeder(&t, &[], Duration::from_millis(20)).await;
    let mut n = node(|c| c.trackers_enabled = true).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr], ..Default::default() }).await.unwrap();
    wait_done(&n, &t.id, 60).await;
    let log = announced(&tracker, &t.id, "completed", 1).await;

    let wire = seeder.good.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(wire, t.total(), "every byte was sent once");
    let completed = find(&log, "completed", 0);
    assert_eq!((completed.downloaded, completed.left, completed.uploaded), (wire, 0, 0), "{completed:?}");
    assert_eq!(completed.corrupt, None, "nothing failed its hash, so no corrupt= at all");
    for a in &log {
        assert!(!a.raw_query.contains("corrupt"), "{}", a.raw_query);
        // A fresh download: the bytes verified so far plus the bytes still missing are the whole torrent.
        assert_eq!(a.downloaded + a.left, t.total(), "{a:?}");
    }
    let row = n.row(&t.id).await.unwrap();
    assert_eq!((row.downloaded_total, row.uploaded_total), (wire, 0), "the lifetime total is the same figure");

    // Pause: `stopped` carries the session totals; resume: a new `started` begins at zero.
    n.h.pause(&[t.id]);
    let log = announced(&tracker, &t.id, "stopped", 1).await;
    let stopped = find(&log, "stopped", 0);
    assert_eq!((stopped.downloaded, stopped.uploaded, stopped.left), (wire, 0, 0));
    n.h.resume(&[t.id]);
    let log = announced(&tracker, &t.id, "started", 2).await;
    let again = find(&log, "started", 1);
    assert_eq!((again.uploaded, again.downloaded, again.left), (0, 0, 0), "nothing of the earlier session is replayed");
    assert_session_rules(&log);
    n.shutdown(Duration::from_secs(10)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupt_piece_is_reported_as_corrupt_and_never_as_downloaded() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 702);
    let bad_piece = 3u32;
    let seeder = scripted_seeder(&t, &[bad_piece], Duration::from_millis(20)).await;
    let mut n = node(|c| c.trackers_enabled = true).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr], ..Default::default() }).await.unwrap();
    wait_done(&n, &t.id, 60).await;
    let log = announced(&tracker, &t.id, "completed", 1).await;

    let total = t.total();
    let bad_size = u64::from(t.piece_len);
    let (good, bad) = (
        seeder.good.load(std::sync::atomic::Ordering::Relaxed),
        seeder.bad.load(std::sync::atomic::Ordering::Relaxed),
    );
    assert_eq!((good, bad), (total, bad_size), "the wire carried the torrent once plus the one bad piece");
    let c = find(&log, "completed", 0);
    assert_eq!(c.downloaded, good, "downloaded counts verified pieces only: {c:?}");
    assert_eq!(c.corrupt, Some(bad), "corrupt is the size of the piece that failed: {c:?}");
    assert_eq!(c.left, 0);
    assert!(c.raw_query.contains(&format!("&corrupt={bad}&event=completed")), "Transmission's position, before event: {}", c.raw_query);
    for a in &log {
        // The failed piece never moved `left`, and never entered `downloaded`.
        assert_eq!(a.downloaded + a.left, total, "{a:?}");
        assert!(a.corrupt.unwrap_or(0) == 0 || a.corrupt == Some(bad), "{a:?}");
    }
    assert_eq!(n.h.diagnostics().hash_failures, 1);
    assert_eq!(n.row(&t.id).await.unwrap().downloaded_total, good, "the ratio does not see the failed bytes either");

    // `stopped` repeats the session's corrupt bytes; the next `started` begins at zero and sends none.
    n.h.pause(&[t.id]);
    let log = announced(&tracker, &t.id, "stopped", 1).await;
    assert_eq!(find(&log, "stopped", 0).corrupt, Some(bad));
    n.h.resume(&[t.id]);
    let log = announced(&tracker, &t.id, "started", 2).await;
    let s2 = find(&log, "started", 1);
    assert_eq!((s2.downloaded, s2.corrupt, s2.uploaded), (0, None, 0), "{s2:?}");
    assert_session_rules(&log);
    n.shutdown(Duration::from_secs(10)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploaded_is_the_piece_payload_the_peer_received_cross_seed_pause_and_resume() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 703);
    let mut n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    announced(&tracker, &t.id, "started", 1).await;

    let first = leech(n.addr(), &t, 0..5).await;
    assert!(first.payload > 0 && first.overhead > 68, "protocol messages came too: {}", first.overhead);
    n.h.pause(&[t.id]);
    let log = announced(&tracker, &t.id, "stopped", 1).await;
    let stopped = find(&log, "stopped", 0);
    assert_eq!(stopped.uploaded, first.payload, "uploaded is exactly the payload that crossed the wire, not the headers");
    assert_eq!((stopped.downloaded, stopped.left), (0, 0));
    assert_eq!(n.row(&t.id).await.unwrap().uploaded_total, first.payload);
    drop(first.stream);

    n.h.resume(&[t.id]);
    let log = announced(&tracker, &t.id, "started", 2).await;
    let second_start = find(&log, "started", 1);
    assert_eq!((second_start.uploaded, second_start.downloaded, second_start.left), (0, 0, 0));
    let second = leech(n.addr(), &t, 5..8).await;
    n.h.pause(&[t.id]);
    let log = announced(&tracker, &t.id, "stopped", 2).await;
    let stopped2 = find(&log, "stopped", 1);
    assert_eq!(stopped2.uploaded, second.payload, "only this session's upload, nothing replayed");
    assert_eq!(n.row(&t.id).await.unwrap().uploaded_total, first_payload_of(&log) + second.payload);

    // A torrent added complete never sends `completed` and never reports a download.
    assert!(log.iter().all(|a| ev(a) != "completed" && a.left == 0 && a.downloaded == 0), "{log:#?}");
    assert_session_rules(&log);
    n.shutdown(Duration::from_secs(10)).await;
}

/// The first session's upload, as the tracker was told.
fn first_payload_of(log: &[Announce]) -> u64 {
    find(log, "stopped", 0).uploaded
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_but_unwritten_blocks_are_not_uploaded() {
    // The peer asks for blocks and vanishes without reading: whatever the engine could not write to the
    // socket must not be counted. (Loopback buffers swallow small amounts, so ask for a lot and close at once.)
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(4_000_000, Some(tracker.url()), false, 704);
    let mut n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    announced(&tracker, &t.id, "started", 1).await;
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(n.addr()).await.unwrap();
        let mut out = Vec::new();
        out.extend(b"\x13BitTorrent protocol");
        out.extend([0u8; 8]);
        out.extend(t.id);
        out.extend(b"-SL0002-scriptleechr");
        out.extend([0, 0, 0, 1, 2]);
        s.write_all(&out).await.unwrap();
        let mut hs = [0u8; 68];
        s.read_exact(&mut hs).await.unwrap();
        // Wait for the unchoke, then ask for 200 blocks (3.2 MB) and never read.
        loop {
            let mut len = [0u8; 4];
            s.read_exact(&mut len).await.unwrap();
            let k = u32::from_be_bytes(len) as usize;
            let mut body = vec![0u8; k];
            if k > 0 {
                s.read_exact(&mut body).await.unwrap();
            }
            if body.first() == Some(&1) {
                break;
            }
        }
        let mut reqs = Vec::new();
        for i in 0..200u32 {
            reqs.extend([0, 0, 0, 13, 6]);
            reqs.extend((i / 2).to_be_bytes());
            reqs.extend(((i % 2) * 16384).to_be_bytes());
            reqs.extend(16384u32.to_be_bytes());
        }
        s.write_all(&reqs).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    } // closed without reading a byte
    tokio::time::sleep(Duration::from_millis(500)).await;
    n.h.pause(&[t.id]);
    let log = announced(&tracker, &t.id, "stopped", 1).await;
    let up = find(&log, "stopped", 0).uploaded;
    let requested = 200 * 16384u64;
    assert!(up < requested, "never more than was requested, and not what stayed queued: {up}");
    assert_eq!(n.row(&t.id).await.unwrap().uploaded_total, up);
    n.shutdown(Duration::from_secs(10)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_tracker_outage_the_next_announce_carries_the_full_cumulative_counts() {
    let tracker = MockTracker::start(60).await;
    tracker.set_failing(true);
    let t = standard_torrent(0, Some(tracker.url()), false, 705);
    let bad_piece = 5u32;
    let seeder = scripted_seeder(&t, &[bad_piece], Duration::from_millis(40)).await;
    let mut n = node(|c| c.trackers_enabled = true).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr], ..Default::default() }).await.unwrap();
    // The tracker answers 503 to `started` while the transfer runs.
    wait_for("a failed started", 30, || async { tracker.log_for(&t.id).iter().any(|a| a.status == 503).then_some(()) }).await;
    wait_done(&n, &t.id, 60).await;
    let failed = tracker.log_for(&t.id);
    assert!(failed.iter().all(|a| a.status == 503 && a.downloaded + a.left == t.total()), "{failed:#?}");
    tracker.set_failing(false);
    // Recovery: a re-announce goes out at once (the engine's own backoff never holds it back; only a ten
    // second double-click floor does, so ask again every eleven seconds until one is answered).
    let mut last = None::<std::time::Instant>;
    let log = wait_for("an announce after the outage", 60, || {
        let (h, tracker, id) = (n.h.clone(), tracker.clone(), t.id);
        let due = last.is_none_or(|l| l.elapsed() > Duration::from_secs(11));
        if due {
            last = Some(std::time::Instant::now());
        }
        async move {
            if due {
                let _ = h.reannounce(&[id]).await;
            }
            let l = tracker.log_for(&id);
            l.iter().any(|a| a.status == 200).then_some(l)
        }
    })
    .await;
    let after = log.iter().find(|a| a.status == 200).unwrap();
    let (good, bad) = (
        seeder.good.load(std::sync::atomic::Ordering::Relaxed),
        seeder.bad.load(std::sync::atomic::Ordering::Relaxed),
    );
    assert_eq!((good, bad), (t.total(), u64::from(t.piece_len)));
    assert_eq!(ev(after), "started");
    assert_eq!(after.downloaded, good, "the full count since the session began, not a delta: {after:?}");
    assert_eq!(after.corrupt, Some(bad), "{after:?}");
    assert_eq!(after.left, 0);
    assert_session_rules(&log);
    n.shutdown(Duration::from_secs(10)).await;
}
