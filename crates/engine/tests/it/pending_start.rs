//! The grace period before an auto-added torrent starts (SPEC 7A.7): held and silent on the
//! network, a countdown that belongs to the engine, pause / start now / choose files, restarts,
//! and the switches that skip it.
use crate::common;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use tokio::net::TcpListener;
use trs_engine::config::{AfterAdd, AskMode, SmartAddConfig, WatchConfig, WatchMode};
use trs_engine::*;

const DELAY: u32 = 3;

fn movie(announce: Option<String>, seed: u64) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name: "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
        piece_len: 32 * 1024,
        files: &[("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv", 300_000)],
        private: false,
        announce,
        seed,
    })
}

/// A seeder that has all of `t` and nothing else going on.
async fn seeder_of(t: &TestTorrent) -> Node {
    let n = node(|_| {}).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

/// A TCP listener that only counts connections.
async fn counting_listener() -> (SocketAddr, Arc<AtomicU64>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let n = Arc::new(AtomicU64::new(0));
    let n2 = n.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { break };
            n2.fetch_add(1, Ordering::Relaxed);
            drop(s);
        }
    });
    (addr, n)
}

/// The settings of an auto-adding app with the grace period `secs`, trackers and a (dead-end) DHT on.
fn grace(secs: u32) -> impl Fn(&mut EngineConfig) + Copy {
    move |c| {
        c.start_delay_secs = secs;
        c.trackers_enabled = true;
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_allow_local = true;
        c.dht_bootstrap = vec!["127.0.0.1:9".into()];
        c.smart_add = SmartAddConfig { mode: AskMode::Never, ..SmartAddConfig::default() };
    }
}

async fn state_of(n: &Node, id: &TorrentId) -> TorrentState {
    n.row(id).await.expect("row").state
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_torrent_is_silent_for_the_whole_wait_and_starts_at_the_deadline() {
    let tracker = MockTracker::start(60).await;
    let t = movie(Some(tracker.url()), 11);
    let seeder = seeder_of(&t).await;
    let n = node(grace(DELAY)).await;
    let (probe, conns) = counting_listener().await;
    let began = Instant::now();
    let added = n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![probe, seeder.addr()], ..Default::default() }).await.unwrap();
    assert!(!added.needs_choice);
    let p = added.pending.expect("the add reports the grace period");
    assert!(!p.paused && p.remaining_ms > 2000 && p.remaining_ms <= 3000, "{p:?}");
    let now_ms = trs_engine::util::unix_ms();
    assert!(p.deadline_ms >= now_ms && p.deadline_ms <= now_ms + 3100, "{p:?} vs {now_ms}");

    // The seeder is told about us and dials; nobody answers, nothing is announced, no lookup.
    seeder.h.add_peers(t.id, vec![n.addr()]);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.state, TorrentState::Pending);
    assert!(row.pending.is_some_and(|p| !p.paused && p.remaining_ms < 1200), "{:?}", row.pending);
    assert!(tracker.log_for(&t.id).is_empty(), "{:?}", tracker.log());
    assert_eq!(conns.load(Ordering::Relaxed), 0, "no peer is dialled while pending");
    let d = n.h.diagnostics();
    assert_eq!((d.dht_lookups, d.dht_announces, d.pex_sent, d.pex_received, d.incoming_accepted), (0, 0, 0, 0, 0), "{d:?}");
    let totals = n.h.totals().await.unwrap();
    assert_eq!(totals.connections, 0, "no connection is open");
    assert_eq!(row.done_bytes, 0);

    // The deadline passes: it starts as an immediate add would, and finishes from the seeder.
    wait_for("start", 10, || async { (state_of(&n, &t.id).await != TorrentState::Pending).then_some(()) }).await;
    assert!(began.elapsed() >= Duration::from_millis(2900), "started after {:?}", began.elapsed());
    assert!(n.row(&t.id).await.unwrap().pending.is_none());
    wait_for("announce", 10, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    wait_done(&n, &t.id, 30).await;
    let log = tracker.log_for(&t.id);
    assert_eq!(log.first().and_then(|a| a.event.as_deref()), Some("started"), "{log:?}");
    assert!(log[0].at.duration_since(began) >= Duration::from_millis(2900), "announced after {:?}", log[0].at.duration_since(began));
    t.verify(n.dir.path(), &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_holds_the_torrent_until_resume_or_start_now() {
    let tracker = MockTracker::start(60).await;
    let t = movie(Some(tracker.url()), 31);
    let n = node(grace(2)).await;
    let mut ev = n.h.subscribe();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::Pending { change: PendingChange::Added, .. })).await;
    n.h.pending_pause(t.id).await.unwrap();
    wait_event(&mut ev, 5, |e| matches!(e, EngineEvent::Pending { change: PendingChange::Paused, .. })).await;
    // Well past the original deadline: still held, still silent, the time left stands still.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.state, TorrentState::Pending);
    let p = row.pending.unwrap();
    assert!(p.paused && p.remaining_ms > 1000, "{p:?}");
    assert!(tracker.log_for(&t.id).is_empty());
    let again = n.row(&t.id).await.unwrap().pending.unwrap();
    assert!(again.remaining_ms.abs_diff(p.remaining_ms) < 50, "{p:?} vs {again:?}");

    // Resume: counts down again (at least three seconds) and then starts.
    n.h.pending_resume(t.id).await.unwrap();
    let p = n.row(&t.id).await.unwrap().pending.unwrap();
    assert!(!p.paused && p.remaining_ms >= 2500, "{p:?}");
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::Pending { change: PendingChange::Started, .. })).await;
    assert_ne!(state_of(&n, &t.id).await, TorrentState::Pending);
    wait_for("announce", 10, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_now_starts_at_once_even_from_a_paused_countdown() {
    let tracker = MockTracker::start(60).await;
    let t = movie(Some(tracker.url()), 33);
    let n = node(grace(60)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    n.h.pending_pause(t.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(tracker.log_for(&t.id).is_empty());
    n.h.start_now(t.id).await.unwrap();
    assert_ne!(state_of(&n, &t.id).await, TorrentState::Pending);
    wait_for("announce", 5, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    // It is not pending any more: a second call says so.
    assert!(matches!(n.h.start_now(t.id).await, Err(EngineError::Invalid(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_delay_start_now_a_paused_add_and_a_chosen_selection_skip_the_wait() {
    let tracker = MockTracker::start(60).await;
    // Setting 0: today's immediate start.
    let t0 = movie(Some(tracker.url()), 35);
    let n = node(grace(0)).await;
    let a = n.h.add_torrent_file(t0.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(a.pending.is_none());
    wait_for("immediate announce", 5, || async { (!tracker.log_for(&t0.id).is_empty()).then_some(()) }).await;

    // A delay is set, but the caller says now (CLI `--now`).
    let n = node(grace(30)).await;
    let t1 = movie(Some(tracker.url()), 37);
    let a = n.h.add_torrent_file(t1.bytes.clone(), AddOptions { start_now: true, ..Default::default() }).await.unwrap();
    assert!(a.pending.is_none());
    wait_for("announce", 5, || async { (!tracker.log_for(&t1.id).is_empty()).then_some(()) }).await;

    // Paused adds and adds with a file selection are the caller's decision already.
    let t2 = movie(Some(tracker.url()), 39);
    let a = n.h.add_torrent_file(t2.bytes.clone(), AddOptions { paused: true, ..Default::default() }).await.unwrap();
    assert!(a.pending.is_none());
    assert_eq!(state_of(&n, &t2.id).await, TorrentState::Paused);
    let t3 = movie(Some(tracker.url()), 41);
    let a = n.h.add_torrent_file(t3.bytes.clone(), AddOptions { file_priorities: vec![FilePriority::Normal], ..Default::default() }).await.unwrap();
    assert!(a.pending.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_torrent_held_with_a_fresh_countdown() {
    let tracker = MockTracker::start(60).await;
    let t = movie(Some(tracker.url()), 43);
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let mut n = snode(state.path(), dl.path(), None, grace(3)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    n.h.flush().await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    drop(n);
    // The original deadline passes while the app is not running: nothing starts behind anyone's back.
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let began = Instant::now();
    let n = snode(state.path(), dl.path(), None, grace(3)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.state, TorrentState::Pending);
    let p = row.pending.unwrap();
    assert!(!p.paused && p.remaining_ms > 2000, "a fresh countdown, not the old one: {p:?}");
    assert!(tracker.log_for(&t.id).is_empty());
    wait_for("start", 10, || async { (n.row(&t.id).await.unwrap().state != TorrentState::Pending).then_some(()) }).await;
    assert!(began.elapsed() >= Duration::from_millis(2800), "{:?}", began.elapsed());
    wait_for("announce", 10, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn choosing_files_cancels_the_countdown_and_keeps_the_torrent_held() {
    let tracker = MockTracker::start(60).await;
    let t = movie(Some(tracker.url()), 45);
    let n = node(grace(2)).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(n.h.pending_choose(t.id).await.unwrap(), "the file list is known: the chooser opens now");
    let row = n.row(&t.id).await.unwrap();
    assert_eq!((row.state, row.pending.is_none()), (TorrentState::AwaitingChoice, true));
    assert!(n.h.choice(t.id).await.unwrap().awaiting);
    // Past the old deadline: still held and silent, as a held torrent always is.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert_eq!(state_of(&n, &t.id).await, TorrentState::AwaitingChoice);
    assert!(tracker.log_for(&t.id).is_empty());
    n.h.confirm_choice(t.id, ChoiceSelection::Default).await.unwrap();
    assert_ne!(state_of(&n, &t.id).await, TorrentState::AwaitingChoice);
    wait_for("announce", 5, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_magnet_link_fetches_nothing_until_the_wait_is_over() {
    let tracker = MockTracker::start(60).await;
    let t = movie(None, 47);
    let seeder = seeder_of(&t).await;
    let n = node(grace(DELAY)).await;
    let uri = format!("magnet:?xt=urn:btih:{}&dn={}&tr={}", hex_of(&t.id), t.name, tracker.url());
    let (probe, conns) = counting_listener().await;
    let began = Instant::now();
    let a = n.h.add_magnet(&uri, AddOptions { peers: vec![probe, seeder.addr()], ..Default::default() }).await.unwrap();
    assert!(a.pending.is_some());
    seeder.h.add_peers(t.id, vec![n.addr()]);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(state_of(&n, &t.id).await, TorrentState::Pending);
    assert!(tracker.log_for(&t.id).is_empty(), "no announce for a held magnet: {:?}", tracker.log());
    assert_eq!(conns.load(Ordering::Relaxed), 0);
    let d = n.h.diagnostics();
    assert_eq!((d.dht_lookups, d.dht_announces, d.incoming_accepted), (0, 0, 0), "{d:?}");
    assert_eq!(n.h.totals().await.unwrap().connections, 0);
    // Then it fetches the metadata from the seeder and downloads.
    wait_done(&n, &t.id, 30).await;
    assert!(began.elapsed() >= Duration::from_millis(2900));
    t.verify(n.dir.path(), &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_that_needs_a_choice_is_not_given_a_countdown() {
    let names: Vec<String> = (1..=2)
        .flat_map(|s| (1..=3).map(move |e| format!("Some.Show.S{s:02}E{e:02}.1080p.WEB-DL.x264-GRP.mkv")))
        .collect();
    let files: Vec<(&str, usize)> = names.iter().map(|n| (n.as_str(), 100_000)).collect();
    let t = make_torrent(&TorrentSpec {
        name: "Some.Show.S01-S02.1080p.WEB-DL.x264-GRP",
        piece_len: 32 * 1024,
        files: &files,
        private: false,
        announce: None,
        seed: 21,
    });
    let n = node(|c| {
        grace(DELAY)(c);
        c.detect = Some(trs_engine::config::DetectionConfig::default());
        c.smart_add = SmartAddConfig { mode: AskMode::Smart, ..SmartAddConfig::default() };
    })
    .await;
    let a = n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    assert!(a.needs_choice && a.pending.is_none());
    assert_eq!(state_of(&n, &t.id).await, TorrentState::AwaitingChoice);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_folder_auto_add_waits_too() {
    let t = movie(None, 51);
    let watch = tempfile::tempdir().unwrap();
    let w: PathBuf = watch.path().to_path_buf();
    let n = node(move |c| {
        grace(DELAY)(c);
        c.watch = vec![WatchConfig { path: w, mode: WatchMode::Auto, after_add: AfterAdd::Leave, category: None, rule: None, since: None }];
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut ev = n.h.subscribe();
    std::fs::write(watch.path().join("movie.torrent"), &t.bytes).unwrap();
    wait_event(&mut ev, 20, |e| matches!(e, EngineEvent::Pending { change: PendingChange::Added, .. })).await;
    let row = n.row(&t.id).await.unwrap();
    assert_eq!(row.state, TorrentState::Pending);
    assert!(row.pending.is_some());
    wait_event(&mut ev, 10, |e| matches!(e, EngineEvent::Pending { change: PendingChange::Started, .. })).await;
}
