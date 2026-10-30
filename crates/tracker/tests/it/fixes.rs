//! Regression tests for the design changes that followed the adversarial audit:
//!
//! * A: `failure reason` halts only when it looks permanent, everything else is
//!   retried on the slow schedule;
//! * B: a `completed` that provably never reached the tracker is sent again,
//!   an ambiguous one is not;
//! * the clock model (backward steps re-base, forward excursions heal);
//! * canonical host budgets, scrape gap, user-action floors.
//!
//! (The one-defect-per-test regressions live in the `adversarial_*` files.)

use crate::adversarial_support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adversarial_support::*;
use trs_tracker::gate::{GateConfig, GateEvent};
use trs_tracker::response::{AnnounceError, TransportError, is_permanent_reason};
use trs_tracker::{Event, Millis, Report, TorrentEvent};

const A: &str = "http://a.test/announce?passkey=SECRET";

fn start(lab: &mut Lab, n: u32, urls: &[&str]) {
    lab.add(spec(n, &[urls]));
    lab.stats(n, 0, 0, 1000, 1000);
    lab.ev(n, TorrentEvent::Start);
}

// ----------------------------------------------------------------------- A

#[test]
fn permanent_looking_reasons_halt_and_others_do_not() {
    for r in [
        "Unregistered torrent",
        "torrent not registered with this tracker",
        "TORRENT NOT FOUND",
        "Info hash not found",
        "Banned client",
        "This client is banned",
        "Invalid passkey",
        "passkey invalid",
        "Bad passkey, re-download the torrent",
        "Unknown passkey",
    ] {
        assert!(is_permanent_reason(r), "{r}");
    }
    for r in [
        "Tracker is down for maintenance",
        "Try again later",
        "Too many requests",
        "Database error",
        "overloaded",
        "",
        "x",
        "Rate limit exceeded",
        "Server busy",
    ] {
        assert!(!is_permanent_reason(r), "{r}");
    }
}

#[test]
fn permanent_failure_reason_halts_for_good_until_the_user_acts() {
    let mut lab = Lab::new(always(|| reject("Unregistered torrent")));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(30 * 86_400));
    assert_eq!(lab.sent.len(), 1, "one try, then silence for 30 days");
    assert!(lab.reports.iter().any(
        |r| matches!(&r.report, Report::Halted { reason } if reason == "Unregistered torrent")
    ));
}

#[test]
fn maintenance_failure_reason_is_retried_on_the_slow_schedule_and_never_halts() {
    let mut lab = Lab::new(always(|| reject("Tracker is down for maintenance")));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(30 * 86_400));
    let times: Vec<u64> = lab.sent.iter().map(|x| x.t.0).collect();
    assert!(times.len() >= 30, "{} tries in 30 days", times.len());
    // gaps: 30 min, 1 h, 2 h, 4 h, 8 h, then 24 h (+ up to 20 % jitter, and the
    // 100 ms the lab's answer takes)
    let nominal_min = [30u64, 60, 120, 240, 480];
    let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]) / 1000).collect();
    for (i, g) in gaps.iter().enumerate() {
        let nom = nominal_min.get(i).copied().unwrap_or(1440) * 60;
        assert!(
            *g >= nom && *g <= nom + nom / 5 + 2,
            "gap #{i}: {g} s, nominal {nom} s"
        );
    }
    assert!(
        lab.reports
            .iter()
            .all(|r| !matches!(r.report, Report::Halted { .. })),
        "a maintenance message must never halt announcing"
    );
    let warnings = lab
        .reports
        .iter()
        .filter(|r| matches!(&r.report, Report::Warning(w) if w.contains("maintenance")))
        .count();
    assert!(
        warnings >= 30,
        "surfaced as a warning each time ({warnings})"
    );
}

#[test]
fn slow_schedule_ends_with_the_first_success() {
    let down = Rc::new(RefCell::new(true));
    let dn = down.clone();
    let mut lab = Lab::new(Box::new(move |_| {
        if *dn.borrow() {
            vec![(d(1), reject("try again later"))]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(10 * 3600));
    *down.borrow_mut() = false;
    lab.run_for(d(30 * 3600)); // the next retry is at most ~8 h away
    let n = lab.sent.len();
    lab.run_for(d(6 * 3600));
    assert!(
        lab.sent.len() >= n + 11,
        "regular 30 min announces after recovery"
    );
}

#[test]
fn start_resume_and_manual_over_a_halt_or_a_slow_failure_keep_the_floor() {
    for reason in ["Unregistered torrent", "Server busy"] {
        let mut lab = Lab::new(always(move || reject(reason)));
        start(&mut lab, 1, &[A]);
        lab.run_for(d(30));
        for i in 0..600 {
            match i % 3 {
                0 => lab.ev(1, TorrentEvent::Start),
                1 => lab.ev(1, TorrentEvent::Resume),
                _ => lab.manual(1),
            }
            lab.run_for(d(1));
        }
        assert!(
            lab.sent.len() <= 62,
            "{reason}: {} requests in 10 minutes of Start/Resume/Manual mashing",
            lab.sent.len()
        );
        for w in lab.sent.windows(2) {
            assert!(w[1].t.0 - w[0].t.0 >= 10_000, "{reason}");
        }
    }
}

// ----------------------------------------------------------------------- B

/// Builds the answer to the first `completed`.
type Mk = fn() -> Resp;

fn completed_lab(first_completed: Mk) -> Lab {
    let tries = Rc::new(RefCell::new(0u32));
    let mut lab = Lab::new(Box::new(move |sent| {
        if sent.p.event == Some(Event::Completed) {
            let mut t = tries.borrow_mut();
            *t += 1;
            if *t == 1 {
                return vec![(d(1), first_completed())];
            }
        }
        vec![(d(1), ok(1800, None))]
    }));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(20));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(6 * 3600));
    lab
}

fn completed_sent(lab: &Lab) -> Vec<&Sent> {
    lab.sent
        .iter()
        .filter(|x| x.event() == Some(Event::Completed))
        .collect()
}

#[test]
fn completed_that_provably_never_reached_the_tracker_is_sent_again() {
    let kinds: [(&str, Mk); 5] = [
        ("dns", || Err(AnnounceError::Transport(TransportError::Dns))),
        ("connect", || {
            Err(AnnounceError::Transport(TransportError::Connect))
        }),
        ("connect timeout", || {
            Err(AnnounceError::Transport(TransportError::ConnectTimeout))
        }),
        ("tls", || Err(AnnounceError::Transport(TransportError::Tls))),
        ("policy", || {
            Err(AnnounceError::Transport(TransportError::Forbidden(trs_tracker::policy::Refusal::Loopback)))
        }),
    ];
    for (name, f) in kinds {
        let lab = completed_lab(f);
        let c = completed_sent(&lab);
        assert_eq!(
            c.len(),
            2,
            "{name}: first attempt never arrived, one resend"
        );
        assert!(
            c[1].t.0 - c[0].t.0 >= 60_000,
            "{name}: the resend waits for the backoff"
        );
        assert_eq!(c[1].p.left, 0);
        assert!(
            lab.gate_events
                .iter()
                .all(|e| !matches!(e, GateEvent::HostTripped { .. })),
            "{name}: the auditor must not take the resend for a second completed"
        );
        // after the resend arrived nothing else is sent
        assert_eq!(completed_sent(&lab).len(), 2, "{name}");
    }
}

#[test]
fn ambiguous_failures_never_resend_completed() {
    let kinds: [(&str, Mk); 5] = [
        ("timeout", timeout),
        ("io", || Err(AnnounceError::Transport(TransportError::Io))),
        ("500", || http_status(500)),
        ("503", || http_status(503)),
        ("garbage", || {
            Err(AnnounceError::Garbage(
                trs_tracker::response::ParseError::NotBencode,
            ))
        }),
    ];
    for (name, f) in kinds {
        let lab = completed_lab(f);
        assert_eq!(
            completed_sent(&lab).len(),
            1,
            "{name}: the tracker may have counted it, never send it twice"
        );
        // the retry carries left = 0 instead
        assert!(
            lab.sent
                .iter()
                .filter(|x| x.event().is_none())
                .all(|x| x.p.left == 0),
            "{name}"
        );
    }
}

#[test]
fn completed_resend_survives_a_crash_between_failure_and_retry() {
    // the first two completed attempts fail with DNS (never reached the tracker)
    let failed = Rc::new(RefCell::new(0u32));
    let mut lab = Lab::new(Box::new(move |sent| {
        let mut f = failed.borrow_mut();
        if sent.p.event == Some(Event::Completed) && *f < 2 {
            *f += 1;
            vec![(d(1), Err(AnnounceError::Transport(TransportError::Dns)))]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(20));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(10)); // the first completed failed with DNS, a retry is pending
    assert_eq!(completed_sent(&lab).len(), 1);
    lab.crash();
    lab.stats(1, 0, 0, 0, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(3 * 3600));
    let delivered = completed_sent(&lab).len() - 2; // two never arrived
    assert_eq!(delivered, 1, "exactly one completed reached the tracker");
}

#[test]
fn late_success_for_an_undelivered_completed_cancels_the_resend() {
    // dual-stack shape: the DNS failure of one family comes first, then the
    // other family's success for the same logical request
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Completed) {
            vec![
                (d(1), Err(AnnounceError::Transport(TransportError::Connect))),
                (d(2), ok(1800, None)),
            ]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(20));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(6 * 3600));
    assert_eq!(completed_sent(&lab).len(), 1, "the success proves delivery");
    assert!(
        lab.gate_events
            .iter()
            .all(|e| !matches!(e, GateEvent::HostTripped { .. }))
    );
}

// ------------------------------------------------------------------- clock

#[test]
fn backward_step_keeps_the_remaining_wait_and_never_makes_announces_early() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.now = s(1_000_000);
    start(&mut lab, 1, &[A]);
    lab.run_for(d(1000)); // success at ~+0.1 s, next due at ~+1800 s
    let n0 = lab.sent.len();
    let success_at = lab.sent[n0 - 1].t.0 + 100;
    let before_step = lab.now.0;
    let remaining = success_at + 1_800_000 - before_step; // ~800 s still to wait
    // the wall clock is stepped back by an hour
    lab.now = Millis(before_step - 3_600_000);
    let step_at = lab.now;
    lab.run_for(d(3000));
    let waited = lab.sent[n0].t.0 - step_at.0;
    assert!(
        (remaining..=remaining + 5_000).contains(&waited),
        "next announce {waited} ms after the step, expected the remaining {remaining} ms"
    );
}

#[test]
fn forward_excursion_that_is_corrected_costs_at_most_one_early_announce() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(100));
    let real = lab.now;
    let before = lab.sent.len();
    lab.now = Millis(real.0 + 5 * 365 * 86_400_000);
    lab.pump();
    let during = lab.sent.len() - before;
    assert!(during <= 1, "{during} announces at the excursion");
    lab.now = real;
    let n0 = lab.sent.len();
    lab.run_for(d(24 * 3600));
    let n = lab.sent.len() - n0;
    assert!(
        (46..=50).contains(&n),
        "{n} announces in the 24 h after the correction"
    );
    // and none of them earlier than the interval
    for w in lab.sent[n0..].windows(2) {
        assert!(w[1].t.0 - w[0].t.0 >= 1_800_000 - 1000);
    }
}

#[test]
fn persisted_timings_stay_on_the_wall_clock_after_a_backward_step() {
    use trs_tracker::store::{TimingKey, TimingStore};
    let mut lab = Lab::new(always(|| ok(1800, Some(900))));
    lab.now = s(1_000_000);
    start(&mut lab, 1, &[A]);
    lab.run_for(d(1000));
    // step back by 10 minutes, then a crash
    lab.now = Millis(lab.now.0 - 600_000);
    lab.run_for(d(5));
    let wall_now = lab.now;
    let rec = lab
        .hub_ref()
        .store()
        .load(&TimingKey {
            info_hash: ih(1),
            tracker_url: A.to_owned(),
        })
        .expect("record");
    let max_wall = [
        rec.last_announce_at,
        rec.next_allowed_at,
        rec.min_allowed_at,
    ]
    .into_iter()
    .flatten()
    .map(|t| t.0)
    .max()
    .unwrap();
    // every stored instant is on the (stepped-back) wall time line: at most one
    // interval ahead of "now", not 10 minutes more
    assert!(
        max_wall <= wall_now.0 + 1_800_000,
        "stored {max_wall}, wall {}",
        wall_now.0
    );
    lab.crash();
    lab.stats(1, 0, 0, 1000, 1000);
    lab.ev(1, TorrentEvent::Start);
    let n0 = lab.sent.len();
    lab.run_for(d(1200));
    assert!(lab.sent.len() > n0, "restarts and announces");
}

// -------------------------------------------------------------- host keys

#[test]
fn equivalent_host_spellings_share_one_budget_end_to_end() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    for n in 0..30u32 {
        let url = match n % 3 {
            0 => "http://Tracker.test/announce?k=1",
            1 => "http://tracker.test./announce?k=1",
            _ => "http://TRACKER.TEST:80/announce?k=1",
        };
        lab.add(spec(n, &[&[url]]));
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(10));
    assert!(
        lab.sent.len() <= 11,
        "{} requests in 10 s to one machine (bucket 1/s, empty at start)",
        lab.sent.len()
    );
    assert!(max_in_window(&lab.sent, "tracker.test", d(1)) <= 4);
}

#[test]
fn scrape_gap_follows_the_announce_interval() {
    use trs_tracker::gate::SubmitError;
    let mut lab = Lab::with_cfg(
        always(|| ok(7200, None)),
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    start(&mut lab, 1, &["http://s.test/announce?pk=1"]);
    lab.run_for(d(10));
    let now = lab.now;
    assert!(lab.hub().request_scrape(&ih(1), 0, now).is_ok());
    let tickets: Vec<_> = lab
        .hub()
        .tick(now)
        .into_iter()
        .filter(|r| r.scrape_hashes().is_some())
        .map(|r| r.ticket())
        .collect();
    assert_eq!(tickets.len(), 1);
    for t in tickets {
        lab.hub().complete_scrape(t, true, now);
    }
    // 31 min later (> the 30 min floor, < the 2 h announce interval): too soon
    let later = Millis(now.0 + 31 * 60_000);
    assert_eq!(
        lab.hub().request_scrape(&ih(1), 0, later),
        Err(SubmitError::ScrapeTooSoon)
    );
    let much_later = Millis(now.0 + 7_200_000 + 1000);
    assert!(lab.hub().request_scrape(&ih(1), 0, much_later).is_ok());
}

#[test]
fn debug_output_of_the_store_never_contains_the_passkey() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    start(&mut lab, 1, &[A]);
    lab.run_for(d(10));
    let dbg = format!("{:?}", lab.hub_ref().store());
    assert!(!dbg.contains("SECRET"), "{dbg}");
    let _ = Duration::ZERO;
}

// ------------------------------------------------------------------- roles

use trs_tracker::TorrentRole;

fn role_lab(role: TorrentRole, answers: impl Fn(&Sent) -> Resp + 'static) -> Lab {
    let mut lab = Lab::new(Box::new(move |s| {
        vec![(Duration::from_millis(100), answers(s))]
    }));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 1000, 1000);
    set_role(&mut lab, role);
    lab.ev(1, TorrentEvent::Start);
    lab
}

fn set_role(lab: &mut Lab, role: TorrentRole) {
    let now = lab.now;
    lab.hub().set_role(&ih(1), role, now);
}

/// Seconds from the end of each (100 ms) failed request to the next request.
fn retry_gaps(lab: &Lab) -> Vec<f64> {
    lab.sent
        .windows(2)
        .map(|w| (w[1].t.0 - w[0].t.0) as f64 / 1000.0 - 0.1)
        .collect()
}

#[test]
fn transient_failure_schedules_follow_the_role() {
    let nominal_min: [(TorrentRole, [u64; 8]); 3] = [
        (TorrentRole::Downloading, [1, 2, 4, 8, 16, 32, 60, 60]),
        (TorrentRole::Seeding, [15, 30, 60, 120, 240, 360, 360, 360]),
        (
            TorrentRole::SeedingDormant,
            [30, 60, 120, 240, 480, 720, 720, 720],
        ),
    ];
    for (role, nominal) in nominal_min {
        let mut lab = role_lab(role, |_| timeout());
        lab.run_for(d(10 * 86_400));
        let gaps = retry_gaps(&lab);
        assert!(gaps.len() >= 8, "{role:?}");
        for (i, n) in nominal.iter().enumerate() {
            let nom = (n * 60) as f64;
            assert!(
                gaps[i] >= nom - 0.001 && gaps[i] <= nom * 1.2 + 0.001,
                "{role:?} retry #{}: {} s, nominal {nom} s",
                i + 1,
                gaps[i]
            );
        }
    }
}

#[test]
fn regular_announces_follow_the_tracker_interval_for_every_role() {
    for role in [
        TorrentRole::Downloading,
        TorrentRole::Seeding,
        TorrentRole::SeedingDormant,
    ] {
        let mut lab = role_lab(role, |_| ok(1800, None));
        lab.run_for(d(6 * 3600));
        let g = retry_gaps(&lab);
        assert!(g.len() >= 11, "{role:?}: {} announces in 6 h", g.len() + 1);
        for x in &g[1..] {
            assert!((*x - 1800.0).abs() < 0.01, "{role:?}: gap {x}");
        }
    }
}

#[test]
fn failure_reason_schedule_does_not_depend_on_the_role() {
    for role in [
        TorrentRole::Downloading,
        TorrentRole::Seeding,
        TorrentRole::SeedingDormant,
    ] {
        let mut lab = role_lab(role, |_| reject("Server busy, try later"));
        lab.run_for(d(2 * 86_400));
        let g = retry_gaps(&lab);
        for (i, nom) in [1800.0, 3600.0, 7200.0, 14_400.0, 28_800.0, 86_400.0]
            .iter()
            .enumerate()
        {
            assert!(
                g[i] >= *nom && g[i] <= nom * 1.2 + 0.01,
                "{role:?} #{i}: {}",
                g[i]
            );
        }
    }
}

fn http503(secs: u64) -> Resp {
    Err(AnnounceError::Http {
        status: 503,
        retry_after: Some(Duration::from_secs(secs)),
    })
}

#[test]
fn retry_after_downloading_waits_exactly_seeding_waits_for_the_later() {
    // three plain timeouts, then a 503 with Retry-After
    let answers = |hint: u64| move |s: &Sent| if s.n < 3 { timeout() } else { http503(hint) };
    // downloading: after 3 failures its own schedule says 8 min; the hint says 2 min: 2 min
    let mut lab = role_lab(TorrentRole::Downloading, answers(120));
    lab.run_for(d(3 * 3600));
    assert!(
        (retry_gaps(&lab)[3] - 120.0).abs() < 0.01,
        "{:?}",
        retry_gaps(&lab)
    );
    // ... and a hint longer than the schedule is honoured too
    let mut lab = role_lab(TorrentRole::Downloading, answers(1500));
    lab.run_for(d(3 * 3600));
    assert!((retry_gaps(&lab)[3] - 1500.0).abs() < 0.01);
    // seeding: the 4th failure's schedule is 2 h; a short hint cannot shorten it ...
    let mut lab = role_lab(TorrentRole::Seeding, answers(120));
    lab.run_for(d(12 * 3600));
    let g = retry_gaps(&lab)[3];
    assert!((7200.0..=7200.0 * 1.2 + 0.1).contains(&g), "{g}");
    // ... a long one wins
    let mut lab = role_lab(TorrentRole::SeedingDormant, answers(20 * 3600));
    lab.run_for(d(48 * 3600));
    assert!((retry_gaps(&lab)[3] - 72_000.0).abs() < 0.01);
}

#[test]
fn role_is_read_when_each_retry_is_scheduled() {
    let mut lab = role_lab(TorrentRole::Downloading, |_| timeout());
    lab.run_for(d(100)); // failure 1 -> retry in ~1 min, then failure 2 -> ~2 min
    set_role(&mut lab, TorrentRole::Seeding);
    lab.run_for(d(10 * 86_400));
    let g = retry_gaps(&lab);
    // the retry that was already scheduled keeps its pace, later ones use the new role
    assert!(g[0] < 80.0, "{g:?}");
    let last_dl = g.iter().position(|x| *x > 600.0).expect("seeding gap");
    // (the failure counter goes on: the third failure on the seeding schedule is 1 h)
    assert!(g[last_dl] >= 3600.0 && g[last_dl] <= 4320.1, "{g:?}");
}

#[test]
fn role_change_never_makes_a_retry_earlier_except_to_downloading() {
    for (from, to) in [
        (TorrentRole::Seeding, TorrentRole::SeedingDormant),
        (TorrentRole::SeedingDormant, TorrentRole::Seeding),
        (TorrentRole::Downloading, TorrentRole::Seeding),
        (TorrentRole::Downloading, TorrentRole::SeedingDormant),
    ] {
        let mut lab = role_lab(from, |_| timeout());
        lab.run_for(d(30)); // first failure happened; retry pending
        let scheduled = lab
            .hub_ref()
            .set(&ih(1))
            .unwrap()
            .machine(0)
            .unwrap()
            .retry_at()
            .unwrap();
        set_role(&mut lab, to);
        let after = lab
            .hub_ref()
            .set(&ih(1))
            .unwrap()
            .machine(0)
            .unwrap()
            .retry_at()
            .unwrap();
        assert_eq!(
            after, scheduled,
            "{from:?} -> {to:?} must leave the pending retry alone"
        );
    }
}

#[test]
fn seeding_to_downloading_pulls_the_pending_retry_in() {
    for from in [TorrentRole::Seeding, TorrentRole::SeedingDormant] {
        let mut lab = role_lab(from, |_| timeout());
        lab.run_for(d(30));
        let failed_at = lab.sent[0].t.0 + 100;
        let sched = lab
            .hub_ref()
            .set(&ih(1))
            .unwrap()
            .machine(0)
            .unwrap()
            .retry_at()
            .unwrap();
        assert!(sched.0 - failed_at >= 15 * 60_000);
        set_role(&mut lab, TorrentRole::Downloading);
        let pulled = lab
            .hub_ref()
            .set(&ih(1))
            .unwrap()
            .machine(0)
            .unwrap()
            .retry_at()
            .unwrap();
        // the downloading schedule's first slot: 1 min + up to 20 % after the failure
        assert!(
            pulled.0 - failed_at >= 60_000 && pulled.0 - failed_at <= 72_000,
            "{from:?}"
        );
        assert!(pulled < sched);
        lab.run_for(d(120));
        assert_eq!(
            lab.sent.len(),
            2,
            "{from:?}: the retry went out at the downloading pace"
        );
        // and the gate is still the gate: nothing earlier than its bucket allows
        assert!(max_in_window(&lab.sent, "a.test", d(1)) <= 4);
    }
}

#[test]
fn pulling_a_retry_in_still_honours_the_server_hint() {
    let mut lab = role_lab(TorrentRole::Seeding, |_| http503(600));
    lab.run_for(d(60));
    let failed_at = lab.sent[0].t.0 + 100;
    set_role(&mut lab, TorrentRole::Downloading);
    let pulled = lab
        .hub_ref()
        .set(&ih(1))
        .unwrap()
        .machine(0)
        .unwrap()
        .retry_at()
        .unwrap();
    assert_eq!(
        pulled.0 - failed_at,
        600_000,
        "exactly the hint, not the 1 min slot"
    );
    lab.run_for(d(700));
    assert_eq!(lab.sent.len(), 2);
    assert!(lab.sent[1].t.0 - failed_at >= 600_000);
}

#[test]
fn pulling_in_never_applies_to_a_halt_or_a_failure_reason() {
    let mut lab = role_lab(TorrentRole::Seeding, |_| reject("Server busy"));
    lab.run_for(d(30));
    let before = lab
        .hub_ref()
        .set(&ih(1))
        .unwrap()
        .machine(0)
        .unwrap()
        .retry_at()
        .unwrap();
    set_role(&mut lab, TorrentRole::Downloading);
    let after = lab
        .hub_ref()
        .set(&ih(1))
        .unwrap()
        .machine(0)
        .unwrap()
        .retry_at()
        .unwrap();
    assert_eq!(
        before, after,
        "the failure-reason schedule is role independent"
    );
}
