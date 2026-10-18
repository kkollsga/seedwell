//! The inactive stop condition (SPEC §7.5): a finished torrent that uploaded less than the
//! threshold during the last window of *seeding* time is stopped like a reached goal. The window
//! is shortened for the tests: `idle_hour_secs = 1` makes "3 hours" three seconds.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;
use trs_state::duration::Secs;
use trs_state::rules::{DefaultRule, IdleSetting, IdleStop, Match, Rule, SeedingGoal};
use trs_state::{RuleSet, TrackerProfiles};

const BELOW: u64 = 1_000_000;
const HOURS: u32 = 3;

fn policy(rules: Vec<Rule>) -> PolicyData {
    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule { goal: SeedingGoal::Forever, ..DefaultRule::default() }).unwrap();
    for r in rules {
        rs.add_rule(r, None).unwrap();
    }
    PolicyData {
        rules: rs,
        profiles: TrackerProfiles::default(),
        private_min_seed: None,
        idle: Some(IdleStop { below: BELOW, hours: HOURS }),
    }
}

fn rule(id: &str) -> Rule {
    Rule { id: id.into(), name: id.into(), ..Rule::default() }
}

async fn seed_node(t: &[&TestTorrent], up_limit: Option<u64>) -> Node {
    let n = node(|c| {
        c.idle_hour_secs = 1;
        c.up_limit = up_limit;
    })
    .await;
    for t in t {
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&n, &t.id, 30).await;
    }
    n
}

async fn stopped(n: &Node, id: &TorrentId, secs: u64) -> TorrentRow {
    wait_for("stopped by the inactive condition", secs, || async {
        let r = n.row(id).await?;
        (r.state == TorrentState::Paused && r.idle_reason().is_some()).then_some(r)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_seed_stops_and_a_seed_serving_a_peer_does_not_until_it_goes_quiet() {
    let busy = standard_named("busy", 12_000_000, None, false, 901);
    let quiet = standard_named("quiet", 0, None, false, 902);
    let seeder = seed_node(&[&busy, &quiet], Some(2_000_000)).await;
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(busy.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    seeder.h.apply_policy(policy(vec![]));
    let t0 = Instant::now();

    let q = stopped(&seeder, &quiet.id, 20).await;
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_secs(2), "not before a full window has been seeded: {waited:?}");
    let idle = q.idle.expect("the figure is kept");
    assert!(idle.stopped && idle.uploaded < BELOW);
    assert_eq!(q.idle_reason().unwrap(), "0 KB uploaded in the last 3 h");
    assert_eq!(q.idle_info().unwrap(), "Uploaded in the last 3 h: 0 KB (stops below 1 MB)");
    assert!(q.goal_text().contains("less than 1 MB in 3 h"), "{}", q.goal_text());
    let b = seeder.row(&busy.id).await.unwrap();
    assert_ne!(b.state, TorrentState::Paused, "a seed that is serving a peer stays");
    assert!(b.idle.is_some_and(|i| i.uploaded > BELOW && !i.stopped), "{:?}", b.idle);

    // Once the peer has everything and the window has passed, it stops as well.
    stopped(&seeder, &busy.id, 40).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resuming_by_hand_restarts_the_window() {
    let quiet = standard_named("quiet", 0, None, false, 903);
    let seeder = seed_node(&[&quiet], None).await;
    seeder.h.apply_policy(policy(vec![]));
    stopped(&seeder, &quiet.id, 20).await;
    seeder.h.resume(&[quiet.id]);
    let t0 = Instant::now();
    wait_for("seeding again", 5, || async { (seeder.row(&quiet.id).await?.state != TorrentState::Paused).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let r = seeder.row(&quiet.id).await.unwrap();
    assert_ne!(r.state, TorrentState::Paused, "the resume is not undone at once");
    assert_eq!(r.idle_reason(), None);
    stopped(&seeder, &quiet.id, 20).await;
    assert!(t0.elapsed() >= Duration::from_secs(2), "a whole new window: {:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_minimum_seed_time_wins_over_the_inactive_condition() {
    let quiet = standard_named("quiet", 0, None, false, 904);
    let seeder = seed_node(&[&quiet], None).await;
    let mut floor = rule("floor");
    floor.min_seed_time = Some(Secs(7));
    seeder.h.apply_policy(policy(vec![floor]));
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let r = seeder.row(&quiet.id).await.unwrap();
    assert_ne!(r.state, TorrentState::Paused, "idle for a whole window, but the floor holds it");
    assert_eq!(r.seed_goal.min_seed_secs, 7);
    stopped(&seeder, &quiet.id, 20).await;
    assert!(t0.elapsed() >= Duration::from_secs(6), "{:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rule_can_override_or_switch_the_condition_off() {
    let public = standard_named("public", 0, None, false, 905);
    let private = standard_named("private", 0, None, true, 906);
    let seeder = seed_node(&[&public, &private], None).await;
    // Private torrents: off. Everything else: the global 1 MB / 3 h.
    let mut off = rule("private-off");
    off.when = Match { private: Some(true), ..Match::default() };
    off.stop_when_idle = Some(IdleSetting::Off);
    seeder.h.apply_policy(policy(vec![off]));
    stopped(&seeder, &public.id, 20).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let p = seeder.row(&private.id).await.unwrap();
    assert_ne!(p.state, TorrentState::Paused, "the rule turned it off");
    assert!(p.idle.is_none() && p.seed_goal.idle().is_none());

    // Editing the rule to its own numbers moves the torrent to them live; an unreachable
    // threshold in a longer window changes the text and keeps nothing from the old ring.
    let mut own_rule = rule("private-own");
    own_rule.when = Match { private: Some(true), ..Match::default() };
    own_rule.stop_when_idle = Some(IdleSetting::On(IdleStop { below: 5_000_000_000, hours: 100 }));
    seeder.h.apply_policy(policy(vec![own_rule]));
    wait_for("own numbers", 10, || async { (seeder.row(&private.id).await?.seed_goal.idle() == Some((5_000_000_000, 100))).then_some(()) }).await;
}
