//! A host that recovers pulls forward the retries of other torrents that failed on it (SPEC 7C.4),
//! and a user's re-announce overrides Seedwell's own backoff but not the tracker's minimum interval.

use crate::common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::response::{AnnounceError, TransportError};
use trs_tracker::*;

#[derive(Clone, Copy, PartialEq)]
enum Fail {
    None,
    Http502,
    Http403,
    Reason,
    Refused,
}

/// `/a` fails as scripted; `/b` always works. Same host.
struct Host {
    a: Rc<RefCell<Fail>>,
}
impl Responder for Host {
    fn respond(&mut self, _now: Millis, _host: &str, req: &GatedRequest) -> Reply {
        let lat = Duration::from_millis(100);
        if !req.url().raw().ends_with("/a") {
            return Reply::new(lat, Ok(ok_response(1800, None)));
        }
        let r = match *self.a.borrow() {
            Fail::None => Ok(ok_response(1800, None)),
            Fail::Http502 => Err(AnnounceError::Http { status: 502, retry_after: None }),
            Fail::Http403 => Err(AnnounceError::Http { status: 403, retry_after: None }),
            Fail::Reason => Err(AnnounceError::Rejected { reason: "down for maintenance".into(), retry_in: None }),
            Fail::Refused => Err(AnnounceError::Transport(TransportError::Connect)),
        };
        Reply::new(lat, r)
    }
}

fn rig(mode: Fail) -> (World<Host>, Rc<RefCell<Fail>>) {
    let a = Rc::new(RefCell::new(mode));
    let mut w = World::new(Host { a: a.clone() }, GateConfig::default());
    w.add_torrent(spec(1, 6881, &[&["http://h.test/a"]]), 1000, 0);
    w.add_torrent(spec(2, 6881, &[&["http://h.test/b"]]), 1000, 0);
    let now = w.now;
    w.hub().set_role(&ih(1), TorrentRole::Seeding, now);
    w.hub().set_role(&ih(2), TorrentRole::Seeding, now);
    (w, a)
}

fn retry_of(w: &mut World<Host>, n: u32) -> Option<Millis> {
    w.hub().set(&ih(n)).unwrap().machine(0).unwrap().retry_at()
}

/// A fails first; B starts 10 minutes later and succeeds. Returns (retry before B, B's success time).
fn a_fails_then_b_succeeds(w: &mut World<Host>) -> (Millis, Millis) {
    w.start(ih(1));
    w.run_for(Duration::from_secs(60));
    let before = retry_of(w, 1).expect("A is in backoff");
    assert!(before >= Millis::from_secs(14 * 60), "a seed backs off 15 minutes: {before:?}");
    w.run_for(Duration::from_secs(540));
    w.start(ih(2));
    let started = w.now;
    w.run_for(Duration::from_secs(5));
    (before, started)
}

#[test]
fn a_host_that_answers_pulls_forward_a_sibling_waiting_after_a_host_failure() {
    for mode in [Fail::Http502, Fail::Refused] {
        let (mut w, _) = rig(mode);
        let (before, started) = a_fails_then_b_succeeds(&mut w);
        let after = retry_of(&mut w, 1).unwrap();
        assert!(after < before, "pulled forward: {after:?} < {before:?}");
        assert!(after <= started + Duration::from_secs(70), "soon after B's success: {after:?} vs {started:?}");
        assert!(after >= started, "never in the past");
    }
}

#[test]
fn a_tracker_rejection_is_about_the_torrent_and_gets_no_early_retry() {
    for mode in [Fail::Reason, Fail::Http403] {
        let (mut w, _) = rig(mode);
        let (before, _) = a_fails_then_b_succeeds(&mut w);
        assert_eq!(retry_of(&mut w, 1), Some(before));
    }
}

#[test]
fn the_early_retry_happens_once_then_normal_backoff_goes_on() {
    let (mut w, _) = rig(Fail::Http502);
    let (_, started) = a_fails_then_b_succeeds(&mut w);
    // The pulled retry fires and fails again: normal backoff, 30 minutes for the second failure.
    w.run_for(Duration::from_secs(120));
    let again = retry_of(&mut w, 1).unwrap();
    assert!(again >= started + Duration::from_secs(25 * 60), "second failure backs off normally: {again:?}");
    // B succeeds again when its interval is up: A is not pulled a second time.
    w.run_for(Duration::from_secs(1800));
    let next = w.hub().set(&ih(1)).unwrap().machine(0).unwrap().failures();
    assert!(next >= 2);
    assert!(retry_of(&mut w, 1).is_none_or(|r| r >= w.now) , "no storm of retries");
}

#[test]
fn a_user_ask_overrides_the_backoff_but_waits_for_the_minimum_interval_unless_forced() {
    let (mut w, a) = rig(Fail::Http502);
    w.start(ih(1));
    w.run_for(Duration::from_secs(60));
    let before = retry_of(&mut w, 1).unwrap();
    assert!(before >= Millis::from_secs(14 * 60));
    *a.borrow_mut() = Fail::None;
    w.manual(ih(1));
    let after = retry_of(&mut w, 1).unwrap();
    assert!(after < before, "the user's ask beats Seedwell's backoff");
    assert!(after <= w.now + Duration::from_secs(11), "within the double-click floor: {after:?}");
    w.run_for(Duration::from_secs(30));
    assert_eq!(w.hub().set(&ih(1)).unwrap().machine(0).unwrap().failures(), 0, "it went out and worked");
}
