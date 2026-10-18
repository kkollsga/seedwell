//! Rules and tracker profiles applied by the engine (SPEC 7, 7E): priority, goal, floors, limits,
//! the action at goal, and statistics per profile.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;
use trs_state::duration::Secs;
use trs_state::rules::{DefaultRule, Match, OnGoal, Priority as SPrio, Rule, SeedingGoal};
use trs_state::trackers::{Profile, UrlPattern};
use trs_state::{RuleSet, StatsScope, TrackerProfiles};

fn profile(id: &str, host: &str, rule: Option<&str>) -> Profile {
    Profile {
        id: id.into(),
        name: id.into(),
        urls: vec![UrlPattern::parse(host).unwrap()],
        rule: rule.map(str::to_owned),
        ..Profile::default()
    }
}

fn forever_default() -> DefaultRule {
    DefaultRule { goal: SeedingGoal::Forever, ..DefaultRule::default() }
}

fn rule(id: &str) -> Rule {
    Rule { id: id.into(), name: id.into(), ..Rule::default() }
}

fn policy(rules: Vec<Rule>, profiles: Vec<Profile>) -> PolicyData {
    let mut rs = RuleSet::default();
    rs.set_default(forever_default()).unwrap();
    for r in rules {
        rs.add_rule(r, None).unwrap();
    }
    let mut tp = TrackerProfiles::default();
    for p in profiles {
        tp.add(p).unwrap();
    }
    PolicyData { rules: rs, profiles: tp, private_min_seed: None, idle: None }
}

/// A node that already has `t` complete in its download folder.
async fn crossseed(t: &TestTorrent, policy: PolicyData, opts: AddOptions, trackers: bool) -> Node {
    let n = node(|c| c.trackers_enabled = trackers).await;
    n.h.apply_policy(policy);
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_apply_at_add_and_follow_edits_but_not_user_choices() {
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 301);
    let mut fast = rule("fast");
    fast.priority = Some(SPrio::High);
    fast.goal = Some(SeedingGoal::Ratio { ratio: 2.0 });
    fast.up_limit = Some(100_000);
    let pol = policy(vec![fast.clone()], vec![profile("mock", "127.0.0.1", Some("fast"))]);
    let n = crossseed(&t, pol, AddOptions::default(), true).await;

    let r = n.row(&t.id).await.unwrap();
    assert_eq!(r.tracker_profile.as_deref(), Some("mock"), "matched by the announce URL");
    assert_eq!(r.priority, Priority::High);
    assert_eq!(r.rule_in_effect.as_deref(), Some("fast"), "linked from the profile");
    assert_eq!(r.seed_goal.ratio, Some(2.0));
    assert!(r.up_limit.is_some_and(|l| (100_000..101_100).contains(&l)), "{:?}", r.up_limit);

    // Edit the rule: the torrent follows at once.
    fast.priority = Some(SPrio::Low);
    fast.goal = Some(SeedingGoal::Ratio { ratio: 3.0 });
    fast.up_limit = Some(0);
    n.h.apply_policy(policy(vec![fast.clone()], vec![profile("mock", "127.0.0.1", Some("fast"))]));
    wait_for("rule edit applied", 10, || async {
        let r = n.row(&t.id).await?;
        (r.priority == Priority::Low && r.seed_goal.ratio == Some(3.0) && r.up_limit.is_none()).then_some(())
    })
    .await;

    // A priority the user chose stays; the goal kind the user chose stays but the rule's floor and action apply.
    n.h.set_priority(&[t.id], Priority::High);
    n.h.set_seed_goal(&[t.id], SeedGoal::ratio(9.0));
    fast.priority = Some(SPrio::Normal);
    fast.min_seed_time = Some(Secs::hours(72));
    fast.on_goal = Some(OnGoal::RemoveKeepFiles);
    n.h.apply_policy(policy(vec![fast], vec![profile("mock", "127.0.0.1", Some("fast"))]));
    wait_for("floor applied", 10, || async {
        let r = n.row(&t.id).await?;
        (r.seed_goal.min_seed_secs == 72 * 3600).then_some(r)
    })
    .await;
    let r = n.row(&t.id).await.unwrap();
    assert_eq!(r.priority, Priority::High, "the user's priority survives rule edits");
    assert_eq!((r.seed_goal.ratio, r.seed_goal.at_goal), (Some(9.0), AtGoal::Remove));

    // The profile disappears: the torrent counts as Public again.
    n.h.apply_policy(policy(vec![], vec![]));
    wait_for("profile dropped", 10, || async { n.row(&t.id).await?.tracker_profile.is_none().then_some(()) }).await;
}

/// "Stop when the download finishes": `completed`, then `stopped` right behind it, never an upload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_when_finished_sends_stopped_promptly_and_never_seeds() {
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(1_000_000, Some(tracker.url()), false, 311);
    // Slow enough that the tracker session is open (`started` sent) before the download ends.
    let seeder = node(|c| c.up_limit = Some(400_000)).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let mut stop = rule("leech-only");
    stop.goal = Some(SeedingGoal::StopWhenFinished);
    let pol = policy(vec![stop], vec![profile("mock", "127.0.0.1", Some("leech-only"))]);
    let leech = node(|c| c.trackers_enabled = true).await;
    leech.h.apply_policy(pol);
    leech
        .h
        .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() })
        .await
        .unwrap();
    let mut events = leech.h.subscribe();
    let leech_port = leech.h.listen_port();
    wait_for("started announce", 20, || async {
        tracker.log_for(&t.id).into_iter().find(|a| a.port == leech_port && a.event.as_deref() == Some("started"))
    })
    .await;
    let done_at = wait_for("stopped by the goal", 60, || async {
        (leech.row(&t.id).await?.state == TorrentState::Paused).then(std::time::Instant::now)
    })
    .await;
    t.verify(leech.dir.path(), &[]);
    let port = leech.h.listen_port();
    let stopped = wait_for("a stopped announce", 20, || async {
        tracker.log_for(&t.id).into_iter().find(|a| a.port == port && a.event.as_deref() == Some("stopped"))
    })
    .await;
    eprintln!("stopped announce {:?} after the goal was reached", stopped.at.saturating_duration_since(done_at));
    let mine: Vec<_> = tracker.log_for(&t.id).into_iter().filter(|a| a.port == port).collect();
    let events_seen: Vec<_> = mine.iter().filter_map(|a| a.event.clone()).collect();
    let pos = |name: &str| events_seen.iter().position(|e| e == name);
    assert!(pos("started").is_some() && pos("completed").is_some() && pos("stopped").is_some(), "{events_seen:?}");
    assert!(pos("completed") < pos("stopped"), "completed first, stopped right behind it: {events_seen:?}");
    assert_eq!(stopped.left, 0);
    assert_eq!(stopped.uploaded, 0, "never uploaded a byte");
    assert_eq!(leech.row(&t.id).await.unwrap().uploaded_total, 0);
    let mut reached = false;
    while let Ok(e) = events.try_recv() {
        reached |= matches!(e, EngineEvent::SeedingGoalReached { .. });
    }
    assert!(reached, "SeedingGoalReached was announced");
}

/// The hit-and-run floor holds the goal back until the torrent was served long enough.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn min_seed_time_defers_the_goal() {
    let t = standard_torrent(0, None, false, 321);
    let mut r = rule("floor");
    r.goal = Some(SeedingGoal::StopWhenFinished);
    r.min_seed_time = Some(Secs::from_secs(4));
    let pol = policy(vec![r], vec![]);
    let n = crossseed(&t, pol, AddOptions { rule: Some("floor".into()), ..Default::default() }, false).await;
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(matches!(n.state(&t.id).await, Some(TorrentState::LowPower | TorrentState::Seeding)), "floor not served yet");
    wait_for("goal after the floor", 20, || async { (n.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    assert!(started.elapsed() >= Duration::from_secs(3), "not before the floor: {:?}", started.elapsed());
}

/// All three actions at goal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actions_at_goal_stop_remove_and_delete() {
    let (a, b, c) = (
        standard_named("keep-a", 0, None, false, 331),
        standard_named("rm-b", 0, None, false, 332),
        standard_named("del-c", 0, None, false, 333),
    );
    let mk = |id: &str, on: OnGoal| {
        let mut r = rule(id);
        r.goal = Some(SeedingGoal::StopWhenFinished);
        r.on_goal = Some(on);
        r
    };
    let pol = policy(
        vec![mk("keep", OnGoal::StopKeep), mk("rm", OnGoal::RemoveKeepFiles), mk("del", OnGoal::RemoveDeleteFiles)],
        vec![],
    );
    let n = node(|_| {}).await;
    n.h.apply_policy(pol);
    for t in [&a, &b, &c] {
        t.write_to(n.dir.path());
    }
    n.h.add_torrent_file(a.bytes.clone(), AddOptions { rule: Some("keep".into()), ..Default::default() }).await.unwrap();
    n.h.add_torrent_file(b.bytes.clone(), AddOptions { rule: Some("rm".into()), ..Default::default() }).await.unwrap();
    n.h.add_torrent_file(c.bytes.clone(), AddOptions { rule: Some("del".into()), ..Default::default() }).await.unwrap();
    wait_for("goals applied", 30, || async {
        let s = n.h.snapshot().await.ok()?;
        let only_a = s.rows.len() == 1 && s.rows[0].id == a.id && s.rows[0].state == TorrentState::Paused;
        only_a.then_some(())
    })
    .await;
    a.verify(n.dir.path(), &[]);
    b.verify(n.dir.path(), &[]); // removed from the client, files kept
    // The row goes first; its files are deleted right after, off the engine's loop.
    let gone = n.dir.path().join(&c.name);
    wait_for("removed and deleted", 10, || async { (!gone.exists()).then_some(()) }).await;
}

/// Statistics land in the profile's scope; `completed` and files are counted per profile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statistics_go_to_the_trackers_domain_favorite_or_not() {
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(500_000, Some(tracker.url()), false, 341);
    let u = standard_named("other", 400_000, None, false, 342);
    let seeder = node(|_| {}).await;
    for x in [&t, &u] {
        x.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(x.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &x.id, 30).await;
    }
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut leech = snode(state.path(), dl.path(), Some(policy(vec![], vec![profile("mock", "127.0.0.1", None)])), |c| {
        c.trackers_enabled = true
    })
    .await;
    for x in [&t, &u] {
        leech.h.add_torrent_file(x.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    }
    wait_done_h(&leech.h, &t.id, 60).await;
    wait_done_h(&leech.h, &u.id, 60).await;
    leech.shutdown(Duration::from_secs(3)).await;
    let s = leech.store.state();
    let mock = s.stats_total(&StatsScope::domain("127.0.0.1")).unwrap();
    assert_eq!((mock.torrents_added, mock.torrents_completed), (1, 1));
    assert_eq!(mock.files_completed, 4, "the wanted files of the torrent");
    assert_eq!(mock.files_bytes, t.total());
    assert!(mock.downloaded >= t.total(), "{mock:?}");
    let public = s.stats_total(&StatsScope::Public).unwrap();
    assert_eq!((public.torrents_added, public.torrents_completed), (1, 1));
    assert!(public.downloaded >= u.total());
    assert_eq!(s.stats_total(&StatsScope::All).unwrap().torrents_completed, 2);
    // Neither torrent leaked into the other bucket.
    assert!(mock.downloaded < u.total() + t.total());
    let _ = Match::default();
}

/// A rule's `up_limit` caps what the torrent uploads, on top of the global limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rules_upload_limit_caps_one_torrent() {
    let t = standard_torrent(1_600_000, None, false, 351);
    let mut cap = rule("cap");
    cap.up_limit = Some(400_000);
    let seeder = crossseed(&t, policy(vec![cap], vec![]), AddOptions { rule: Some("cap".into()), ..Default::default() }, false).await;
    assert!(seeder.row(&t.id).await.unwrap().up_limit.is_some());
    let leech = node(|_| {}).await;
    let t0 = std::time::Instant::now();
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    let took = t0.elapsed();
    t.verify(leech.dir.path(), &[]);
    // 3.7 MB at 400 kB/s with a one-second burst: well over 5 s, and not unbounded.
    assert!(took >= Duration::from_secs(5), "{took:?}");
    assert!(took < Duration::from_secs(25), "{took:?}");
    // Lifting the cap through the rule is picked up by the running torrent.
    let mut open = rule("cap");
    open.up_limit = Some(0);
    seeder.h.apply_policy(policy(vec![open], vec![]));
    wait_for("cap lifted", 10, || async { seeder.row(&t.id).await?.up_limit.is_none().then_some(()) }).await;
}

/// Resuming a torrent that stopped at its goal keeps it sharing: the goal becomes the user's
/// "forever" instead of stopping it again at the next goal check (the user saw "Resume" do nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_after_the_goal_keeps_sharing() {
    let t = standard_torrent(0, None, false, 341);
    let mut r = rule("done");
    r.goal = Some(SeedingGoal::StopWhenFinished);
    let pol = policy(vec![r], vec![]);
    let n = crossseed(&t, pol.clone(), AddOptions { rule: Some("done".into()), ..Default::default() }, false).await;
    wait_for("stopped at goal", 20, || async { (n.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    n.h.resume(std::slice::from_ref(&t.id));
    wait_for("sharing again", 10, || async { (n.state(&t.id).await? != TorrentState::Paused).then_some(()) }).await;
    // Re-applying the rules runs the goal check at once; before the fix this stopped it again.
    n.h.apply_policy(pol);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_ne!(n.state(&t.id).await, Some(TorrentState::Paused), "still sharing after a goal check");
}
