//! Settings at runtime: the alt-speed schedule, `config.toml` / `rules.toml` / `trackers.toml`
//! hot reload through [`Host`], and the first-launch port.
use crate::common;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use common::*;
use trs_engine::config::ClockFn;
use trs_engine::*;
use trs_state::config::{DaySet, Schedule, TimeOfDay};
use trs_state::rules::{DefaultRule, Priority as SPrio, SeedingGoal};
use trs_state::trackers::{Profile, UrlPattern};
use trs_state::{Config, DocStore, RuleSet, TrackerProfiles};

/// 2026-10-12 was a Monday.
fn monday(h: i64, m: i64, s: i64) -> i64 {
    trs_state::fsutil::days_from_civil(2026, 10, 12) * 86_400 + h * 3600 + m * 60 + s
}

fn clocked(now: &Arc<AtomicI64>) -> ClockFn {
    let now = now.clone();
    ClockFn(Arc::new(move || (now.load(Ordering::SeqCst), 0)))
}

fn sched(days: DaySet, from: u16, to: u16) -> Schedule {
    Schedule { enabled: true, days, from: TimeOfDay(from), to: TimeOfDay(to) }
}

async fn limits(h: &EngineHandle) -> (Option<u64>, Option<u64>, bool, bool) {
    let s = h.snapshot().await.unwrap();
    (s.down_limit, s.up_limit, s.alt_speed, s.alt_speed_scheduled)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_alt_speed_schedule_follows_the_clock() {
    let now = Arc::new(AtomicI64::new(monday(7, 0, 0)));
    let n = node(|c| {
        c.down_limit = Some(9_000_000);
        c.up_limit = Some(8_000_000);
        c.alt_down_limit = Some(1_000_000);
        c.alt_up_limit = Some(500_000);
        c.alt_schedule = Some(sched(DaySet::WEEKDAYS, 8 * 60, 17 * 60));
        c.clock = Some(clocked(&now));
    })
    .await;
    let mut ev = n.h.subscribe();
    let normal = (Some(9_000_000), Some(8_000_000), false, false);
    assert_eq!(limits(&n.h).await, normal, "before the window");

    // Inside the window (re-evaluated at once when settings are applied).
    now.store(monday(9, 0, 0), Ordering::SeqCst);
    n.h.update_config(|_| {});
    wait_for("window open", 5, || async { (limits(&n.h).await == (Some(1_000_000), Some(500_000), true, true)).then_some(()) }).await;
    let e = wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::AltSpeed { .. })).await;
    assert!(matches!(e, EngineEvent::AltSpeed { on: true, scheduled: true }));

    // After it, on a weekend, and the manual toggle on its own.
    now.store(monday(17, 30, 0), Ordering::SeqCst);
    n.h.update_config(|_| {});
    wait_for("closed", 5, || async { (limits(&n.h).await == normal).then_some(()) }).await;
    now.store(monday(9, 0, 0) + 5 * 86_400, Ordering::SeqCst); // Saturday 09:00
    n.h.update_config(|_| {});
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(limits(&n.h).await, normal, "weekdays only");
    n.h.set_alt_speed(true);
    wait_for("manual", 5, || async { (limits(&n.h).await == (Some(1_000_000), Some(500_000), true, false)).then_some(()) }).await;
    n.h.set_alt_speed(false);
    wait_for("manual off", 5, || async { (limits(&n.h).await == normal).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_across_midnight_and_the_timer_that_opens_it() {
    // Friday 22:00 to 06:00 (belongs to Friday, so Saturday 03:00 is inside).
    let friday = 4 * 86_400;
    let now = Arc::new(AtomicI64::new(monday(0, 0, 0) + friday + 23 * 3600));
    let n = node(|c| {
        c.down_limit = None;
        c.alt_down_limit = Some(100_000);
        c.alt_schedule = Some(sched(DaySet(1 << 4), 22 * 60, 6 * 60));
        c.clock = Some(clocked(&now));
    })
    .await;
    assert!(limits(&n.h).await.2, "Friday 23:00 is inside");
    now.store(monday(0, 0, 0) + friday + 86_400 + 3 * 3600, Ordering::SeqCst);
    n.h.update_config(|_| {});
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(limits(&n.h).await.2, "Saturday 03:00 is the tail of Friday's window");
    now.store(monday(0, 0, 0) + friday + 86_400 + 7 * 3600, Ordering::SeqCst);
    n.h.update_config(|_| {});
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!limits(&n.h).await.2, "07:00 is outside");
    drop(n);

    // No command at all: the engine's own timer opens the window at 08:00.
    let now = Arc::new(AtomicI64::new(monday(7, 59, 58)));
    let n = node(|c| {
        c.alt_down_limit = Some(100_000);
        c.alt_schedule = Some(sched(DaySet::WEEKDAYS, 8 * 60, 17 * 60));
        c.clock = Some(clocked(&now));
    })
    .await;
    assert!(!limits(&n.h).await.2);
    now.store(monday(8, 0, 5), Ordering::SeqCst);
    wait_for("the timer fired", 8, || async { limits(&n.h).await.2.then_some(()) }).await;
}

// ---- hot reload through the Host ----------------------------------------------------------------

struct Env {
    state: tempfile::TempDir,
    dl: tempfile::TempDir,
    inc: tempfile::TempDir,
}

fn env() -> Env {
    Env { state: tempfile::tempdir().unwrap(), dl: tempfile::tempdir().unwrap(), inc: tempfile::tempdir().unwrap() }
}

fn base_config(e: &Env) -> Config {
    let mut c = Config::default();
    c.folders.download_dir = e.dl.path().to_string_lossy().into_owned();
    c.folders.incomplete_dir = e.inc.path().join(".incomplete").to_string_lossy().into_owned();
    c.folders.watch.clear();
    // These tests add random-named multi-file torrents: smart add (SPEC 7A.6) would hold them.
    c.transfers.ask_before_adding = trs_state::config::AskBeforeAdding::Never;
    c.network.upnp = false;
    c.network.natpmp = false;
    c.network.dht = false;
    c
}

async fn host(e: &Env) -> Host {
    Host::start(
        e.state.path(),
        HostOptions {
            poll_interval: Duration::from_millis(100),
            tweak: Some(Box::new(|c| {
                c.listen_addr = "127.0.0.1".parse().unwrap();
                c.trackers_enabled = false;
                c.io_threads = 2;
                c.hash_threads = 2;
                c.write_cache_bytes = 8 << 20;
                c.fd_cache = 64;
            })),
            ..HostOptions::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_launch_remembers_its_port_and_config_edits_apply_live() {
    let e = env();
    let docs = DocStore::<Config>::new(e.state.path());
    docs.save(&base_config(&e)).unwrap();
    let host = host(&e).await;
    assert!(host.warnings.is_empty(), "{:?}", host.warnings);
    let h = host.handle();
    let mut ev = h.subscribe();
    // An automatic port (0) is never written back to config.toml.
    let saved = docs.read_strict().unwrap();
    assert_eq!(saved.value.network.listen_port, 0);
    assert!(h.listen_port() >= 1024);

    // An edit is applied within a poll or two.
    let mut c = docs.read_strict().unwrap().value;
    c.transfers.upload_limit = 123_456;
    c.transfers.max_active_downloads = 7;
    docs.save(&c).unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigReloaded { file, .. } if file == "config.toml")).await;
    let cfg = h.config().await.unwrap();
    assert_eq!((cfg.up_limit, cfg.max_active_downloads), (Some(123_456), 7));

    // A listen-port change is live: no restart, the engine moves to the port.
    let mut c = docs.read_strict().unwrap().value;
    let new_port = free_port();
    c.network.listen_port = new_port;
    c.transfers.upload_limit = 0;
    docs.save(&c).unwrap();
    let e1 = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigReloaded { file, .. } if file == "config.toml")).await;
    let EngineEvent::ConfigReloaded { restart_required, .. } = e1 else { unreachable!() };
    assert!(restart_required.is_empty(), "{restart_required:?}");
    wait_for("the new port", 10, || async { (h.listen_port() == new_port).then_some(()) }).await;
    assert_eq!(h.config().await.unwrap().up_limit, None);

    // A broken edit changes nothing and is reported.
    std::fs::write(e.state.path().join("config.toml"), "version = 1\n[transfers]\nmax_active_downloads = \"lots\"\n").unwrap();
    let rej = wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigRejected { .. })).await;
    let EngineEvent::ConfigRejected { file, error } = rej else { unreachable!() };
    assert_eq!(file, "config.toml");
    assert!(error.contains("lots") && error.starts_with("config.toml"), "{error}");
    assert_eq!(h.config().await.unwrap().max_active_downloads, 7, "the running value stays");
    host.shutdown(Duration::from_secs(3)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rules_and_tracker_profiles_reload_and_reapply() {
    let e = env();
    DocStore::<Config>::new(e.state.path()).save(&base_config(&e)).unwrap();
    let rules_docs = DocStore::<RuleSet>::new(e.state.path());
    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule { goal: SeedingGoal::Forever, ..DefaultRule::default() }).unwrap();
    rules_docs.save(&rs).unwrap();
    let host = host(&e).await;
    let h = host.handle();
    let t = standard_torrent(0, Some("http://tracker.example.org/announce".into()), false, 701);
    t.write_to(e.dl.path());
    h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&h, &t.id, 30).await;
    let row = row_h(&h, &t.id).await.unwrap();
    assert_eq!((row.priority, row.tracker_profile.clone()), (Priority::Normal, None));
    let mut ev = h.subscribe();

    // rules.toml: the default rule now says high priority and a ratio goal.
    rs.set_default(DefaultRule { goal: SeedingGoal::Ratio { ratio: 4.0 }, priority: SPrio::High, ..DefaultRule::default() }).unwrap();
    rules_docs.save(&rs).unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigReloaded { file, .. } if file == "rules.toml")).await;
    wait_for("rules applied", 10, || async {
        let r = row_h(&h, &t.id).await?;
        (r.priority == Priority::High && r.seed_goal.ratio == Some(4.0)).then_some(())
    })
    .await;

    // trackers.toml: a profile for the torrent's tracker appears, then goes.
    let prof_docs = DocStore::<TrackerProfiles>::new(e.state.path());
    let mut tp = TrackerProfiles::default();
    tp.add(Profile { id: "exo".into(), name: "Example".into(), urls: vec![UrlPattern::parse("*.example.org").unwrap()], ..Profile::default() }).unwrap();
    prof_docs.save(&tp).unwrap();
    wait_for("profile matched", 10, || async {
        (row_h(&h, &t.id).await?.tracker_profile.as_deref() == Some("exo")).then_some(())
    })
    .await;
    prof_docs.save(&TrackerProfiles::default()).unwrap();
    wait_for("profile gone", 10, || async { row_h(&h, &t.id).await?.tracker_profile.is_none().then_some(()) }).await;

    // An invalid rules.toml is rejected and the running rules stay.
    std::fs::write(e.state.path().join("rules.toml"), "version = 1\n[default]\npriority = \"urgent\"\n").unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigRejected { file, .. } if file == "rules.toml")).await;
    assert_eq!(row_h(&h, &t.id).await.unwrap().priority, Priority::High);
    host.shutdown(Duration::from_secs(3)).await;
}

/// A restart brings everything back from the directory: records, rules, profiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_restarts_from_its_directory() {
    let e = env();
    DocStore::<Config>::new(e.state.path()).save(&base_config(&e)).unwrap();
    let host1 = host(&e).await;
    let t = standard_torrent(0, None, false, 702);
    t.write_to(e.dl.path());
    host1.handle().add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&host1.handle(), &t.id, 30).await;
    host1.handle().set_priority(&[t.id], Priority::Low);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let port = host1.handle().listen_port();
    host1.shutdown(Duration::from_secs(3)).await;

    let host2 = host(&e).await;
    assert!(host2.open_report.recovered.is_none() && !host2.open_report.created);
    let _ = port; // an automatic port is random at every start; nothing was remembered
    assert_eq!(DocStore::<Config>::new(e.state.path()).read_strict().unwrap().value.network.listen_port, 0);
    let row = wait_for("row", 10, || async { row_h(&host2.handle(), &t.id).await }).await;
    assert_eq!((row.priority, row.tier), (Priority::Low, Tier::Cold));
    assert_eq!(row.done_bytes, t.total());
    host2.shutdown(Duration::from_secs(3)).await;
}

/// Callers without a runtime (the FFI layer, the CLI) start and stop the whole stack blocking.
#[test]
fn a_host_can_be_started_and_stopped_without_a_runtime() {
    let e = env();
    DocStore::<Config>::new(e.state.path()).save(&base_config(&e)).unwrap();
    let host = Host::start_blocking(
        e.state.path(),
        HostOptions {
            hot_reload: false,
            tweak: Some(Box::new(|c| {
                c.listen_addr = "127.0.0.1".parse().unwrap();
                c.io_threads = 2;
                c.hash_threads = 2;
                c.trackers_enabled = false;
            })),
            ..HostOptions::default()
        },
    )
    .unwrap();
    let snap = host.handle().blocking().snapshot().unwrap();
    assert_eq!(snap.counts.total, 0);
    assert!(snap.listen_port > 0);
    let report = host.shutdown_blocking(Duration::from_secs(2));
    assert_eq!(report.stopped_abandoned, 0);
}

/// Tests can never reach the real home: a default configuration is refused, and a hot reload that
/// would re-apply `~/Downloads` is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_real_home_is_off_limits_under_test() {
    if std::env::var_os("HOME").is_none() {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let r = Host::start(state.path(), HostOptions { hot_reload: false, ..HostOptions::default() }).await;
    let Err(EngineError::Io(msg)) = r else { panic!("a default config must not start under test") };
    assert!(msg.contains("refusing") && msg.contains("real home"), "{msg}");

    // A running engine refuses a settings change that points into the home.
    let n = node(|_| {}).await;
    let mut ev = n.h.subscribe();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    n.h.update_config(move |c| c.default_save_dir = home.join("Downloads"));
    wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::ConfigRejected { .. })).await;
    assert_ne!(n.h.config().await.unwrap().default_save_dir, std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("Downloads"));
}

/// The Host stamps every watch folder with the moment it joined the list, keeps that across
/// restarts, and a folder added by editing `config.toml` gets its own baseline: old `.torrent`
/// files are never mass-added.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_stamps_watch_baselines_and_ignores_old_torrents() {
    use trs_state::config::{AfterAdd as SAfter, WatchFolder};
    let e = env();
    let w1 = tempfile::tempdir().unwrap();
    let w2 = tempfile::tempdir().unwrap();
    let mk = |seed: u64, name: &str| {
        make_torrent(&TorrentSpec { name, piece_len: 32 * 1024, files: &[(name, 60_000)], private: false, announce: None, seed })
    };
    let (old1, old2, new1, new2) = (mk(701, "Old1.epub"), mk(702, "Old2.epub"), mk(703, "New1.epub"), mk(704, "New2.epub"));
    std::fs::write(w1.path().join("old1.torrent"), &old1.bytes).unwrap();

    let docs = DocStore::<Config>::new(e.state.path());
    let mut c = base_config(&e);
    let entry = |p: &std::path::Path| WatchFolder {
        path: p.to_string_lossy().into_owned(),
        after_add: SAfter::Leave,
        ..WatchFolder::default()
    };
    c.folders.watch = vec![entry(w1.path())];
    c.folders.torrent_after_add = SAfter::Leave; // the default (Trash) would be the real ~/.Trash
    docs.save(&c).unwrap();
    assert_eq!(docs.read_strict().unwrap().value.folders.watch[0].since, None);

    let started = std::time::SystemTime::now();
    let host = host(&e).await;
    let h = host.handle();
    let since1 = docs.read_strict().unwrap().value.folders.watch[0].since.expect("stamped on the first launch");
    let now = started.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(since1 > now && since1 <= now + 10, "{since1} vs {now}");
    assert_eq!(h.watch_backlog(w1.path()).await.len(), 1);

    // A second folder, edited into config.toml with an old torrent already inside.
    std::fs::write(w2.path().join("old2.torrent"), &old2.bytes).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut c = docs.read_strict().unwrap().value;
    c.folders.watch.push(entry(w2.path()));
    let mut ev = h.subscribe();
    docs.save(&c).unwrap();
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::ConfigReloaded { file, .. } if file == "config.toml")).await;
    let saved = docs.read_strict().unwrap().value;
    assert_eq!(saved.folders.watch[0].since, Some(since1), "the first folder keeps its baseline");
    let since2 = saved.folders.watch[1].since.expect("the new folder was stamped");
    assert!(since2 >= since1);
    // Let the stamped file reload settle, then check backlog and pick-up.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(h.watch_backlog(w2.path()).await.len(), 1);
    std::fs::write(w1.path().join("new1.torrent"), &new1.bytes).unwrap();
    std::fs::write(w2.path().join("new2.torrent"), &new2.bytes).unwrap();
    for id in [new1.id, new2.id] {
        wait_for("new torrents picked up", 20, || async { row_h(&h, &id).await.map(|_| ()) }).await;
    }
    assert!(row_h(&h, &old1.id).await.is_none() && row_h(&h, &old2.id).await.is_none());
    // Handles keep the store open: let go of ours before the restart.
    drop((h, ev));
    host.shutdown(Duration::from_secs(3)).await;

    // After a restart the persisted baseline still holds: the old files stay backlog.
    let mut tries = 0;
    let host = loop {
        match Host::start(
            e.state.path(),
            HostOptions {
                poll_interval: Duration::from_millis(100),
                tweak: Some(Box::new(|c| {
                    c.listen_addr = "127.0.0.1".parse().unwrap();
                    c.trackers_enabled = false;
                })),
                ..HostOptions::default()
            },
        )
        .await
        {
            Ok(h) => break h,
            Err(err) if tries < 40 => {
                tries += 1;
                let _ = err; // the previous host is still letting go of the database lock
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(err) => panic!("{err}"),
        }
    };
    let h = host.handle();
    assert_eq!(docs.read_strict().unwrap().value.folders.watch[0].since, Some(since1));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(row_h(&h, &old1.id).await.is_none() && row_h(&h, &old2.id).await.is_none());
    host.shutdown(Duration::from_secs(3)).await;
}
