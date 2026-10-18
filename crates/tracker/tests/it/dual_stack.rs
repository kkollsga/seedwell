//! SPEC 7C.4a regression: the Transmission 4.0.6 announce-spam bug.
//!
//! A dual-stack tracker host with IPv6 blackholed: every logical announce gets
//! TWO answers - an IPv4 success and an IPv6 failure - in either order. A
//! client that lets the failure overwrite the success re-announces "in 20
//! seconds" instead of at the tracker's interval and gets banned.

use crate::common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::response::{AnnounceError, TransportError};
use trs_tracker::{Event, InfoHash20, Millis};

struct DualStack {
    detector: Rc<RefCell<SpamDetector>>,
    interval: u64,
    min: Option<u64>,
    flip: u64,
}

impl Responder for DualStack {
    fn respond(&mut self, now: Millis, _host: &str, req: &GatedRequest) -> Reply {
        let p = req.announce_params().unwrap();
        // The IPv4 side of the tracker sees (and counts) the announce.
        self.detector.borrow_mut().arrive(now, p.info_hash, p.event);
        self.flip = self
            .flip
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let ok = Ok(ok_response(self.interval, self.min));
        let bad = Err(AnnounceError::Transport(TransportError::Connect));
        match (self.flip >> 33) % 3 {
            // success first, failure later
            0 => Reply {
                latency: Duration::from_millis(200),
                result: ok,
                extra: vec![(Duration::from_secs(30), bad)],
            },
            // failure first, success shortly after
            1 => Reply {
                latency: Duration::from_millis(50),
                result: bad,
                extra: vec![(Duration::from_millis(300), ok)],
            },
            // both at the very same instant, failure processed first
            _ => Reply {
                latency: Duration::from_millis(200),
                result: bad,
                extra: vec![(Duration::ZERO, ok)],
            },
        }
    }
}

fn run(torrents: u32, interval: u64, min: Option<u64>) -> (u64, u64, World<DualStack>) {
    let detector = Rc::new(RefCell::new(SpamDetector::new(Duration::from_secs(
        min.unwrap_or(300).max(60),
    ))));
    let mut w = World::new(
        DualStack {
            detector: detector.clone(),
            interval,
            min,
            flip: 7,
        },
        GateConfig::default(),
    );
    w.oracle.trace_on = false; // 100k+ events: keep the run fast
    for n in 1..=torrents {
        w.add_torrent(spec(n, 6881, &[&["http://dual.test/announce"]]), 1000, 1000);
    }
    for n in 1..=torrents {
        w.start(ih(n));
    }
    w.run_for(Duration::from_secs(24 * 3600));
    let d = detector.borrow();
    (d.spam, d.non_event, w)
}

#[test]
fn ipv6_blackholed_500_torrents_24h() {
    for (interval, min) in [
        (1800u64, None),
        (1800, Some(900)),
        (900, Some(900)),
        (3600, None),
    ] {
        let torrents: u32 = if cfg!(debug_assertions) { 100 } else { 500 };
        let (spam, non_event, w) = run(torrents, interval, min);
        assert_eq!(
            spam, 0,
            "tracker flagged announce spam (interval {interval}, min {min:?})"
        );
        let eff = trs_tracker::clamp_timing(Some(interval as i64), min.map(|m| m as i64))
            .interval
            .as_secs();
        let bound = (24u64 * 3600).div_ceil(eff);
        let mut worst = 0;
        for (&(h, _slot), &sent) in &w.per_key_sent {
            let events = w.per_key_events.get(&(h, 0)).copied().unwrap_or(0);
            worst = worst.max(sent);
            assert!(
                sent <= bound + events,
                "torrent {:02x?}: {sent} announces in 24 h, bound {bound} + {events} events",
                &h[..4]
            );
        }
        println!(
            "interval {interval} min {min:?}: spam {spam}, non-event announces {non_event} (<= {} theoretical), worst per torrent {worst} (bound {bound}+events)",
            u64::from(torrents) * bound
        );
        assert!(non_event <= u64::from(torrents) * bound);
    }
}

/// Positive control: a client with the 4.0.6 flaw (the failure processed last
/// overwrites the success and reschedules in 20 s, and the success resets the
/// failure counter so there is never a backoff) MUST be flagged by the same
/// detector. This proves the detector can see the bug.
#[test]
fn detector_flags_a_client_with_the_4_0_6_flaw() {
    let mut detector = SpamDetector::new(Duration::from_secs(900));
    let torrents: Vec<InfoHash20> = (1..=500).map(ih).collect();
    // Event-driven model: each torrent announces; both answers arrive; the
    // buggy rule picks the LAST processed answer (the failure).
    let mut queue: std::collections::BinaryHeap<std::cmp::Reverse<(Millis, usize)>> = (0..torrents
        .len())
        .map(|i| std::cmp::Reverse((Millis(i as u64 * 1000), i)))
        .collect();
    let end = Millis(24 * 3600 * 1000);
    while let Some(std::cmp::Reverse((t, i))) = queue.pop() {
        if t >= end {
            break;
        }
        detector.arrive(
            t,
            torrents[i],
            if t.0 < 600_000 {
                Some(Event::Started)
            } else {
                None
            },
        );
        // success says "come back in 1800 s"; the processed-last failure says "20 s"
        queue.push(std::cmp::Reverse((t + Duration::from_secs(20), i)));
    }
    assert!(
        detector.spam > 100_000,
        "the detector should flag the buggy client massively, got {}",
        detector.spam
    );
}
