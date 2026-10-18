//! `TrackerHub::set_port`: a new external port is announced by the next regular announce, with no
//! extra request, no earlier schedule and no announce-spam.

use crate::common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::time::Millis;
use trs_tracker::{Event, InfoHash20};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    at: Millis,
    ih: InfoHash20,
    port: u16,
    event: Option<Event>,
}

/// Answers every announce with `interval` and records what arrived, like a tracker that checks
/// intervals: it counts non-event announces closer together than the interval it promised.
struct Recorder {
    interval: u64,
    seen: Rc<RefCell<Vec<Seen>>>,
    spam: Rc<RefCell<SpamDetector>>,
}

impl Responder for Recorder {
    fn respond(&mut self, now: Millis, _host: &str, req: &GatedRequest) -> Reply {
        let p = req.announce_params().expect("announce");
        self.seen.borrow_mut().push(Seen { at: now, ih: p.info_hash, port: p.port, event: p.event });
        self.spam.borrow_mut().arrive(now, p.info_hash, p.event);
        Reply::new(Duration::from_millis(50), Ok(ok_response(self.interval, None)))
    }
}

type Log = Rc<RefCell<Vec<Seen>>>;

fn world(interval: u64) -> (World<Recorder>, Log, Rc<RefCell<SpamDetector>>) {
    let seen: Log = Rc::new(RefCell::new(Vec::new()));
    // A tracker whose floor is the interval itself (the strictest reading).
    let spam = Rc::new(RefCell::new(SpamDetector::new(Duration::from_secs(interval / 2))));
    (
        World::new(Recorder { interval, seen: seen.clone(), spam: spam.clone() }, GateConfig::default()),
        seen,
        spam,
    )
}

fn add(w: &mut World<Recorder>, n: u32, port: u16) {
    w.add_torrent(spec(n, port, &[&["http://a.test/announce"]]), 1000, 1000);
    w.start(ih(n));
}

#[test]
fn next_regular_announce_carries_the_new_port_and_nothing_is_sent_now() {
    let (mut w, seen, _) = world(1800);
    add(&mut w, 1, 6881);
    w.run_for(Duration::from_secs(60));
    assert_eq!(seen.borrow().len(), 1);
    assert_eq!(seen.borrow()[0].port, 6881);
    assert_eq!(seen.borrow()[0].event, Some(Event::Started));

    let before = w.total_sent;
    w.hub().set_port(51413);
    w.run_for(Duration::from_secs(1));
    assert_eq!(w.total_sent, before, "a port change is not a reason to announce now");

    w.run_for(Duration::from_secs(1800));
    let seen = seen.borrow();
    assert_eq!(seen.len(), 2, "exactly one more announce in the next interval: {seen:?}");
    assert_eq!(seen[1].port, 51413);
    assert_eq!(seen[1].event, None, "an ordinary re-announce, not a started");
    // It came when the schedule said, i.e. about `interval` after the first one.
    let gap = seen[1].at.since(seen[0].at);
    assert!(gap >= Duration::from_secs(1700), "next announce came after only {gap:?}");
}

#[test]
fn the_schedule_with_and_without_port_changes_is_identical() {
    let run = |flap: bool| {
        let (mut w, seen, spam) = world(1800);
        for n in 1..=5 {
            add(&mut w, n, 6881);
        }
        // Two hours, a port change every 7 minutes (a NAT-PMP lease being renewed by a router
        // that keeps handing out a different port).
        for step in 0..(120 / 7) {
            w.run_for(Duration::from_secs(7 * 60));
            if flap {
                w.hub().set_port(40000 + (step % 3) as u16);
            }
        }
        let times: Vec<(u64, u16)> =
            seen.borrow().iter().map(|s| (s.at.0, s.port)).collect();
        let spam_n = spam.borrow().spam;
        (times, spam_n, w.total_sent)
    };
    let (plain, plain_spam, plain_sent) = run(false);
    let (flap, flap_spam, flap_sent) = run(true);
    assert_eq!(plain_spam, 0);
    assert_eq!(flap_spam, 0, "port changes must not look like announce spam");
    assert_eq!(flap_sent, plain_sent, "same number of requests");
    let t = |v: &[(u64, u16)]| v.iter().map(|x| x.0).collect::<Vec<_>>();
    assert_eq!(t(&flap), t(&plain), "same timing, whatever the port");
    // and the ports did change on the wire
    assert!(flap.iter().any(|x| x.1 >= 40000));
    assert!(plain.iter().all(|x| x.1 == 6881));
}

#[test]
fn stopped_announce_and_late_torrents_use_the_current_port() {
    let (mut w, seen, _) = world(1800);
    add(&mut w, 1, 6881);
    w.run_for(Duration::from_secs(10));
    w.hub().set_port(7000);
    w.hub().set_port(7000); // idempotent
    w.stop(ih(1));
    w.run_for(Duration::from_secs(10));
    let seen = seen.borrow();
    let last = seen.last().unwrap();
    assert_eq!(last.event, Some(Event::Stopped));
    assert_eq!(last.port, 7000);
}

#[test]
fn zero_is_ignored_and_the_last_port_wins() {
    let (mut w, seen, _) = world(1800);
    for n in 1..=50 {
        add(&mut w, n, 6881);
    }
    w.run_for(Duration::from_secs(30));
    w.hub().set_port(0);
    for p in 2000..2100u16 {
        w.hub().set_port(p);
    }
    w.hub().set_port(6881);
    w.run_for(Duration::from_secs(3600));
    assert!(seen.borrow().iter().skip(50).all(|s| s.port == 6881));
}
