//! The hub's decision notes (developer log): what is recorded when tracing is on, and that nothing
//! is kept when it is off.

use crate::adversarial_support;

use std::cell::RefCell;
use std::rc::Rc;

use adversarial_support::*;
use trs_tracker::hub::{HubNote, NoteKind};
use trs_tracker::{TorrentEvent, TorrentRole};

const T1: &str = "http://t1.test/announce?pk=1";
const T2: &str = "http://t2.test/announce?pk=1";

fn kinds(notes: &[HubNote]) -> Vec<NoteKind> {
    notes.iter().map(|n| n.kind).collect()
}

fn start(lab: &mut Lab, n: u32, tiers: &[&[&str]]) {
    lab.add(spec(n, tiers));
    lab.stats(n, 0, 0, 10, 10);
    let now = lab.now;
    lab.hub().set_role(&ih(n), TorrentRole::Downloading, now);
    lab.ev(n, TorrentEvent::Start);
}

#[test]
fn nothing_is_recorded_when_tracing_is_off() {
    let mut lab = Lab::new(always(|| http_status(503)));
    start(&mut lab, 1, &[&[T1, T2]]);
    lab.run_for(d(3600));
    lab.network();
    lab.manual(1);
    lab.quit(1);
    lab.run_for(d(60));
    assert!(lab.hub().take_notes().is_empty());
}

#[test]
fn failover_opens_the_next_tracker_and_retires_it_when_the_first_recovers() {
    let primary: Rc<RefCell<Option<String>>> = Rc::default();
    let p2 = primary.clone();
    let mut lab = Lab::new(Box::new(move |s| {
        let t = s.t.0 / 1000;
        let mut p = p2.borrow_mut();
        let first = p.get_or_insert_with(|| s.host.clone()).clone();
        if s.host == first && t < 600 {
            vec![(d(1), http_status(503))]
        } else {
            vec![(d(1), ok(300, None))]
        }
    }));
    lab.hub().set_trace(true);
    start(&mut lab, 1, &[&[T1, T2]]);
    lab.run_for(d(3 * 3600));
    let notes = lab.hub().take_notes();
    let opened: Vec<u16> = notes.iter().filter(|n| n.kind == NoteKind::SessionOpened).map(|n| n.slot).collect();
    assert!(opened.contains(&0) && opened.contains(&1), "both trackers were opened: {notes:#?}");
    let retired = notes.iter().find(|n| matches!(n.kind, NoteKind::Retire | NoteKind::RetireSilent));
    let retired = retired.unwrap_or_else(|| panic!("a retire note: {:?}", kinds(&notes)));
    assert!(retired.detail.contains("tier 1"), "{}", retired.detail);
    assert!(notes.iter().all(|n| n.torrent == ih(1)));
}

#[test]
fn a_network_change_leaves_a_healthy_schedule_alone_and_says_so() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.hub().set_trace(true);
    start(&mut lab, 1, &[&[T1]]);
    lab.run_for(d(300));
    let sent = lab.sent.len();
    let _ = lab.hub().take_notes();
    lab.network();
    lab.run_for(d(1));
    assert_eq!(lab.sent.len(), sent, "a network change never makes a regular announce earlier");
    let notes = lab.hub().take_notes();
    let kept = notes.iter().find(|n| n.kind == NoteKind::KeptSchedule).expect("a note");
    assert!(kept.next.is_some_and(|t| t > lab.now), "the unchanged time is reported");
}

#[test]
fn quit_records_one_suspend_per_open_tracker() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.hub().set_trace(true);
    start(&mut lab, 1, &[&[T1], &[T2]]);
    lab.run_for(d(60));
    let _ = lab.hub().take_notes();
    lab.quit(1);
    let notes = lab.hub().take_notes();
    assert_eq!(notes.iter().filter(|n| n.kind == NoteKind::Suspend).count(), 2, "{notes:#?}");
}

#[test]
fn requests_held_back_by_the_rate_gate_are_noted_once_and_released_with_their_wait() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.hub().set_trace(true);
    for n in 1..=6 {
        start(&mut lab, n, &[&[T1]]);
    }
    lab.run_for(d(120));
    let notes = lab.hub().take_notes();
    let deferred = notes.iter().filter(|n| n.kind == NoteKind::Deferred).count();
    let released = notes.iter().filter(|n| n.kind == NoteKind::Released).count();
    assert!(deferred >= 1, "six torrents on one host cannot all leave at once: {:?}", kinds(&notes));
    assert!(released >= 1);
    assert!(notes.iter().any(|n| n.kind == NoteKind::Deferred && n.detail.contains("rate gate")));
    // once per request: never more deferred notes than requests
    assert!(deferred <= lab.sent.len());
}
