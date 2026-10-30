//! End-to-end tests against an in-process hyper server. No Internet.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use sha2::{Digest, Sha256};
use trs_http::{
    DownloadTask, Downloader, DownloaderConfig, Error, Event, RestartReason, State, TokenBucket, get_small, list_staged,
};

const MIB: u64 = 1024 * 1024;

fn collect_events(task: &DownloadTask) -> Arc<Mutex<Vec<Event>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let o = out.clone();
    let mut rx = task.events().unwrap();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            o.lock().unwrap().push(e);
        }
    });
    out
}

fn has_restart(ev: &Arc<Mutex<Vec<Event>>>, r: RestartReason) -> bool {
    ev.lock().unwrap().contains(&Event::Restarted(r))
}

fn range_start(req: &Req) -> Option<u64> {
    let r = req.header("range")?;
    r.strip_prefix("bytes=")?.split('-').next()?.parse().ok()
}

async fn pause_midway(task: &DownloadTask, min_done: u64) -> u64 {
    wait_until(task, "some progress", |p| p.done >= min_done).await;
    task.pause();
    let p = wait_until(task, "paused", |p| p.state == State::Paused).await;
    p.done
}

// ------------------------------------------------------------------ basics

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_download_is_byte_exact_and_polite() {
    let len = 3 * MIB + 12_345;
    let file = FileSrv::new(len);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/files/archive.bin"));
    req.sha256 = Some(Sha256::digest(pattern(0, len, 0)).into());
    let task = dl.start(req).unwrap();
    let path = task.wait().await.unwrap();
    assert_eq!(path, d.dest.join("archive.bin"));
    assert_file_matches(&path, len, 0);
    let p = task.progress();
    assert_eq!(p.state, State::Completed);
    assert_eq!((p.done, p.total), (len, Some(len)));
    assert!(d.staged_files().is_empty(), "staging cleaned up: {:?}", d.staged_files());
    // Politeness: our own User-Agent, no compression behind our back.
    assert_eq!(dl.user_agent(), concat!("Seedwell/", env!("CARGO_PKG_VERSION")));
    for r in srv.requests() {
        assert_eq!(r.header("user-agent").as_deref(), Some(dl.user_agent()));
        assert_eq!(r.header("accept-encoding").as_deref(), Some("identity"));
        assert!(r.header("authorization").is_none() && r.header("cookie").is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_and_unknown_length_files() {
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let empty = FileSrv::new(0);
    let srv = serve_file(&empty).await;
    let p = dl.start(d.req(&srv.url("/empty.txt"))).unwrap().wait().await.unwrap();
    assert_eq!(std::fs::metadata(p).unwrap().len(), 0);

    // chunked: no Content-Length, no validator
    let file = FileSrv::new(700_000);
    file.set_etag(None);
    *file.last_modified.lock().unwrap() = None;
    let f = file.clone();
    let srv = Server::start(move |r, _| {
        let mut rep = f.handle(r);
        rep.chunked = true;
        rep
    })
    .await;
    let p = dl.start(d.req(&srv.url("/stream.bin"))).unwrap().wait().await.unwrap();
    assert_file_matches(&p, 700_000, 0);
}

// ------------------------------------------------------------------ resume

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_continues_with_range_and_if_range() {
    let len = 4 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/big.iso"))).unwrap();
    let done = pause_midway(&task, 500_000).await;
    assert!(done > 0 && done < len);
    // Pause is lossless: what we report is what is checkpointed on disk.
    let staged = list_staged(&d.inc);
    assert_eq!(staged.len(), 1);
    assert_eq!((staged[0].done, staged[0].total), (done, Some(len)));
    assert!(d.dest_files().is_empty());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(task.progress().done, done, "no bytes arrive while paused");

    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
    let reqs = srv.requests();
    let second = &reqs[1];
    assert_eq!(range_start(second), Some(done));
    assert_eq!(second.header("if-range").as_deref(), Some("\"v1\""));
    assert!(task.progress().restarts == 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_task_resumes_from_sidecar_in_a_new_downloader() {
    let len = 5 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(3_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let req = d.req(&srv.url("/kill.bin"));
    {
        let dl = Downloader::unlimited(test_cfg());
        let task = dl.start(req.clone()).unwrap();
        wait_until(&task, "bytes", |p| p.done >= 800_000).await;
        // Wait for a checkpoint to land, then die without any graceful stop.
        let t0 = Instant::now();
        while list_staged(&d.inc).first().is_none_or(|s| s.done == 0) {
            assert!(t0.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(task);
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let staged = list_staged(&d.inc);
    assert_eq!(staged.len(), 1);
    let on_disk = staged[0].done;
    assert!(on_disk > 0 && on_disk < len);

    file.pace_bps.store(0, Ordering::SeqCst);
    let dl2 = Downloader::unlimited(test_cfg());
    let task = dl2.start(req).unwrap();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
    let resumed = srv.requests().into_iter().filter_map(|r| range_start(&r)).max().unwrap();
    assert!(resumed >= on_disk, "resumed at {resumed}, checkpoint said {on_disk}");
    assert!(d.staged_files().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn validator_change_restarts_from_zero() {
    let len = 3 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/changing.bin"))).unwrap();
    let ev = collect_events(&task);
    pause_midway(&task, 400_000).await;
    // The file changes on the server: new ETag and different bytes.
    file.set_etag(Some("\"v2\""));
    *file.last_modified.lock().unwrap() = Some("Thu, 22 Oct 2015 07:28:00 GMT".into());
    file.salt.store(7, Ordering::SeqCst);
    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 7); // all new bytes, nothing stitched
    assert!(has_restart(&ev, RestartReason::ValidatorChanged), "{:?}", ev.lock().unwrap());
    assert_eq!(task.progress().restarts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_that_ignores_if_range_but_changes_etag_is_caught() {
    let len = 3 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/sneaky.bin"))).unwrap();
    let ev = collect_events(&task);
    pause_midway(&task, 400_000).await;
    file.ignore_if_range.store(true, Ordering::SeqCst); // answers 206 for the new file
    file.set_etag(Some("\"v2\""));
    file.salt.store(9, Ordering::SeqCst);
    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 9);
    assert!(has_restart(&ev, RestartReason::ValidatorChanged));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_ignoring_range_restarts_from_zero() {
    let len = 3 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/norange.bin"))).unwrap();
    let ev = collect_events(&task);
    pause_midway(&task, 400_000).await;
    file.honor_range.store(false, Ordering::SeqCst);
    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
    assert!(has_restart(&ev, RestartReason::RangeIgnored), "{:?}", ev.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_validator_means_restart_not_blind_append() {
    for weak in [false, true] {
        let len = 2 * MIB;
        let file = FileSrv::new(len);
        file.set_etag(weak.then_some("W/\"weak\""));
        *file.last_modified.lock().unwrap() = None;
        file.pace_bps.store(2_000_000, Ordering::SeqCst);
        let srv = serve_file(&file).await;
        let d = dirs();
        let dl = Downloader::unlimited(test_cfg());
        let task = dl.start(d.req(&srv.url("/novalidator.bin"))).unwrap();
        let ev = collect_events(&task);
        pause_midway(&task, 300_000).await;
        file.pace_bps.store(0, Ordering::SeqCst);
        task.resume();
        let path = task.wait().await.unwrap();
        assert_file_matches(&path, len, 0);
        assert!(has_restart(&ev, RestartReason::NoValidator));
        // The continuation was never even requested.
        assert!(srv.requests().iter().all(|r| r.header("range").is_none()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_content_range_is_rejected_and_nothing_is_appended() {
    let len = 3 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/liar.bin"))).unwrap();
    let done = pause_midway(&task, 400_000).await;
    file.bad_content_range.store(true, Ordering::SeqCst);
    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let err = task.wait().await.unwrap_err();
    assert!(matches!(err, Error::RangeMismatch(_)), "{err:?}");
    assert_eq!(task.progress().state, State::Failed);
    // The part file is exactly what was there: prefix intact, tail untouched.
    let part = std::fs::read(d.inc.join("liar.bin.part")).unwrap();
    assert_eq!(part.len() as u64, len);
    assert_eq!(&part[..done as usize], pattern(0, done, 0).as_slice());
    assert!(part[done as usize..].iter().all(|&b| b == 0), "mismatched bytes were written");
    assert!(d.dest_files().is_empty());

    // The server is fixed: retrying (resume from Failed) finishes the job.
    file.bad_content_range.store(false, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
}

// ------------------------------------------------------------------ throttling

async fn measure_rate(task: &DownloadTask, secs: f64) -> (f64, u64) {
    let mut rx = task.subscribe();
    let first = loop {
        let p = rx.borrow_and_update().clone();
        if p.done > 0 {
            break (Instant::now(), p.done);
        }
        rx.changed().await.unwrap();
    };
    let mut last = first;
    while last.0.duration_since(first.0).as_secs_f64() < secs {
        rx.changed().await.unwrap();
        last = (Instant::now(), rx.borrow_and_update().done);
    }
    let rate = (last.1 - first.1) as f64 / last.0.duration_since(first.0).as_secs_f64();
    (rate, last.1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_rate_gate_limits_to_2_mb_per_s_within_10_percent() {
    let file = FileSrv::new(60 * MIB);
    file.chunk.store(64 * 1024, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let gate = PacedGate::new(2_000_000);
    let dl = Downloader::new(test_cfg(), gate.clone());
    let task = dl.start(d.req(&srv.url("/throttled.bin"))).unwrap();
    let (rate, done) = measure_rate(&task, 5.0).await;
    eprintln!("THROTTLE global gate 2.0 MB/s: measured {:.1} kB/s over 5 s ({:.1} %)", rate / 1e3, rate / 2e4);
    assert!((rate - 2_000_000.0).abs() <= 200_000.0, "rate {rate}");
    // every accepted byte went through the gate
    assert!(gate.total.load(Ordering::SeqCst) >= done);
    task.cancel(false);
    task.wait_for_state(State::Cancelled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_bucket_and_per_download_limit_combine_to_the_stricter() {
    let file = FileSrv::new(60 * MIB);
    file.chunk.store(64 * 1024, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::new(test_cfg(), Arc::new(TokenBucket::new(4_000_000)));
    let mut req = d.req(&srv.url("/limited.bin"));
    req.rate_limit = Some(1_000_000);
    let task = dl.start(req).unwrap();
    let (rate, _) = measure_rate(&task, 4.0).await;
    eprintln!("THROTTLE bucket 4 MB/s + per-download 1 MB/s: measured {:.1} kB/s ({:.1} %)", rate / 1e3, rate / 1e4);
    assert!((rate - 1_000_000.0).abs() <= 100_000.0, "rate {rate}");
    // lifting the per-download limit lets the 4 MB/s global bucket govern
    task.set_rate_limit(None);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (rate, _) = measure_rate(&task, 3.0).await;
    eprintln!("THROTTLE bucket 4 MB/s after lifting per-download limit: {:.1} kB/s ({:.1} %)", rate / 1e3, rate / 4e4);
    assert!((rate - 4_000_000.0).abs() <= 400_000.0, "rate {rate}");
    task.cancel(false);
    task.wait_for_state(State::Cancelled).await;
}

// ------------------------------------------------------------------ segments

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_is_byte_exact_and_uses_parallel_ranges() {
    let len = 12 * MIB + 777;
    let file = FileSrv::new(len);
    file.pace_bps.store(3_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/seg.bin"));
    req.connections = 4;
    let task = dl.start(req).unwrap();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
    assert!(srv.max_inflight() >= 2, "no parallelism: {}", srv.max_inflight());
    assert!(srv.max_inflight() <= 4, "too many connections: {}", srv.max_inflight());
    // Requested ranges tile the file with no overlap.
    let mut spans: Vec<(u64, u64)> = srv
        .requests()
        .iter()
        .map(|r| {
            let v = r.header("range").expect("every request is ranged");
            let (a, b) = v.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
            (a.parse().unwrap(), b.parse::<u64>().unwrap() + 1)
        })
        .collect();
    spans.sort();
    assert_eq!(spans[0].0, 0);
    for w in spans.windows(2) {
        assert_eq!(w[0].1, w[1].0, "gap/overlap in {spans:?}");
    }
    assert_eq!(spans.last().unwrap().1, len);
    assert!(spans.len() >= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_pause_resume_is_byte_exact() {
    let len = 12 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(1_500_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/segpause.bin"));
    req.connections = 3;
    let task = dl.start(req).unwrap();
    let done = pause_midway(&task, 3 * MIB).await;
    let staged = list_staged(&d.inc);
    assert_eq!(staged[0].done, done);
    file.pace_bps.store(0, Ordering::SeqCst);
    task.resume();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_host_connection_cap_is_shared_across_tasks() {
    let len = 6 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(2_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let cfg = DownloaderConfig { max_connections_per_host: 2, ..test_cfg() };
    let dl = Downloader::unlimited(cfg);
    let mut tasks = vec![];
    for name in ["one.bin", "two.bin", "three.bin"] {
        let mut req = d.req(&srv.url(&format!("/{name}")));
        req.connections = 4;
        tasks.push(dl.start(req).unwrap());
    }
    for t in &tasks {
        let p = t.wait().await.unwrap();
        assert_file_matches(&p, len, 0);
    }
    assert!(srv.max_inflight() <= 2, "cap broken: {}", srv.max_inflight());
    assert!(srv.max_inflight() >= 1);
}

// ------------------------------------------------------------------ integrity

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checksum_mismatch_fails_and_nothing_is_moved() {
    let len = MIB;
    let file = FileSrv::new(len);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/bad.bin"));
    req.sha256 = Some([0xAB; 32]);
    let task = dl.start(req).unwrap();
    let err = task.wait().await.unwrap_err();
    let actual: String = Sha256::digest(pattern(0, len, 0)).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(err, Error::ChecksumMismatch { expected: "ab".repeat(32), actual });
    assert!(d.dest_files().is_empty(), "nothing may reach the destination");
    assert!(d.staged_files().contains(&"bad.bin.part".to_owned()), "the bytes are kept for inspection");
}

// ------------------------------------------------------------------ retries

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_after_on_429_is_honoured() {
    let file = FileSrv::new(100_000);
    file.status_plan.lock().unwrap().push_back((429, Some("1".into())));
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let t0 = Instant::now();
    let task = dl.start(d.req(&srv.url("/limited.bin"))).unwrap();
    let ev = collect_events(&task);
    let path = task.wait().await.unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(990), "came back after {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_file_matches(&path, 100_000, 0);
    let evs = ev.lock().unwrap();
    assert!(evs.iter().any(|e| matches!(e, Event::Retrying { error: Error::Http { status: 429 }, delay, .. } if *delay >= Duration::from_secs(1))));
    assert_eq!(srv.requests().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_errors_are_retried_with_backoff_then_succeed() {
    let file = FileSrv::new(50_000);
    file.status_plan.lock().unwrap().extend([(503, None), (500, None), (408, None)]);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/flaky.bin"))).unwrap();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, 50_000, 0);
    assert_eq!(task.progress().retries, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gives_up_after_configured_retries_with_a_clear_error() {
    let file = FileSrv::new(50_000);
    file.status_plan.lock().unwrap().extend((0..10).map(|_| (500u16, None)));
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(DownloaderConfig { max_retries: 2, ..test_cfg() });
    let task = dl.start(d.req(&srv.url("/down.bin"))).unwrap();
    let err = task.wait().await.unwrap_err();
    assert_eq!(err, Error::RetriesExhausted { attempts: 3, last: Box::new(Error::Http { status: 500 }) });
    assert!(err.to_string().contains("gave up after 3"));
    assert_eq!(srv.requests().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_http_errors_are_not_retried() {
    let srv = Server::start(|_, _| Reply::text(404, "gone")).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/missing.bin"))).unwrap();
    assert_eq!(task.wait().await.unwrap_err(), Error::Http { status: 404 });
    assert_eq!(srv.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_connection_mid_body_resumes_where_it_stopped() {
    let len = 2 * MIB;
    let file = FileSrv::new(len);
    file.drop_plan.lock().unwrap().extend([300_000, 900_000]);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/dropper.bin"))).unwrap();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, len, 0);
    let reqs = srv.requests();
    assert_eq!(reqs.len(), 3);
    let s1 = range_start(&reqs[1]).unwrap();
    assert!(s1 > 0 && s1 <= 300_000, "second request resumed at {s1}");
    assert_eq!(reqs[1].header("if-range").as_deref(), Some("\"v1\""));
    let s2 = range_start(&reqs[2]).unwrap();
    assert!(s2 > s1 && s2 <= s1 + 900_000);
    assert_eq!(task.progress().retries, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_trickle_is_fine_but_a_stalled_connection_times_out_and_retries() {
    // Steady trickle, each frame well within the idle timeout.
    let file = FileSrv::new(150_000);
    file.pace_bps.store(100_000, Ordering::SeqCst);
    file.chunk.store(4096, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(DownloaderConfig { read_timeout: Duration::from_millis(600), ..test_cfg() });
    let path = dl.start(d.req(&srv.url("/trickle.bin"))).unwrap().wait().await.unwrap();
    assert_file_matches(&path, 150_000, 0);
    assert_eq!(srv.requests().len(), 1);

    // First response sends headers, then goes silent.
    let file = FileSrv::new(300_000);
    file.stall_plan.lock().unwrap().push_back(Duration::from_secs(30));
    let srv = serve_file(&file).await;
    let task = dl.start(d.req(&srv.url("/stall.bin"))).unwrap();
    let path = task.wait().await.unwrap();
    assert_file_matches(&path, 300_000, 0);
    assert_eq!(task.progress().retries, 1);
}

// ------------------------------------------------------------------ redirects

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_chain_is_followed_and_names_come_from_the_final_url() {
    let file = FileSrv::new(200_000);
    let f = file.clone();
    let srv = Server::start(move |r, _| match r.path.as_str() {
        "/r1" => Reply::redirect(302, "/r2"),
        "/r2" => Reply::redirect(301, "/dir/r3"),
        "/dir/r3" => Reply::redirect(307, "final%20name.bin"), // relative to /dir/
        "/dir/final%20name.bin" => f.handle(r),
        p => Reply::text(404, p),
    })
    .await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/r1"))).unwrap();
    let path = task.wait().await.unwrap();
    assert_eq!(path.file_name().unwrap(), "final name.bin");
    assert_file_matches(&path, 200_000, 0);
    assert!(task.progress().final_url.unwrap().ends_with("/dir/final%20name.bin"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_across_hosts_works() {
    let file = FileSrv::new(100_000);
    let origin = serve_file(&file).await;
    let target = origin.url("/real.bin");
    let hop = Server::start(move |_, _| Reply::redirect(302, &target)).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let path = dl.start(d.req(&hop.url("/go"))).unwrap().wait().await.unwrap();
    assert_file_matches(&path, 100_000, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirect_loops_and_endless_chains_are_stopped() {
    let srv = Server::start(|r, _| {
        if let Some(n) = r.path.strip_prefix("/chain/") {
            let n: u32 = n.parse().unwrap_or(0);
            return Reply::redirect(302, &format!("/chain/{}", n + 1));
        }
        match r.path.as_str() {
            "/l1" => Reply::redirect(302, "/l2"),
            "/l2" => Reply::redirect(302, "/l1"),
            "/ftp" => Reply::redirect(302, "ftp://example.com/file"),
            "/nolocation" => Reply::status(302),
            _ => Reply::text(404, ""),
        }
    })
    .await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let e = |p: &str| {
        let t = dl.start(d.req(&srv.url(p))).unwrap();
        async move { t.wait().await.unwrap_err() }
    };
    assert_eq!(e("/l1").await, Error::RedirectLoop);
    assert_eq!(e("/chain/0").await, Error::TooManyRedirects);
    assert!(matches!(e("/ftp").await, Error::BadRedirect(_)));
    assert!(matches!(e("/nolocation").await, Error::BadRedirect(_)));
    // exactly 10 hops are allowed: the 11th request is the last one made
    let chain_requests = srv.requests().iter().filter(|r| r.path.starts_with("/chain/")).count();
    assert_eq!(chain_requests, 11);
}

// ------------------------------------------------------------------ names

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_names_come_from_content_disposition_or_url_and_collisions_get_a_suffix() {
    let file = FileSrv::new(10_000);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let get = |path: &str, cd: Option<&str>| {
        *file.content_disposition.lock().unwrap() = cd.map(str::to_owned);
        let t = dl.start(d.req(&srv.url(path))).unwrap();
        async move { t.wait().await.unwrap() }
    };
    let p = get("/x", Some("attachment; filename=\"report final.pdf\"")).await;
    assert_eq!(p.file_name().unwrap(), "report final.pdf");
    let p = get("/x", Some("attachment; filename=\"report final.pdf\"")).await;
    assert_eq!(p.file_name().unwrap(), "report final (2).pdf");
    let p = get("/x", Some("attachment; filename=\"a.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt")).await;
    assert_eq!(p.file_name().unwrap(), "€ rates.txt");
    let p = get("/x", Some("attachment; filename=\"../../evil.sh\"")).await;
    assert_eq!(p.parent().unwrap(), d.dest, "must stay inside the destination");
    assert_eq!(p.file_name().unwrap(), "_.._evil.sh");
    let p = get("/dir/My%20Data.iso?token=abc", None).await;
    assert_eq!(p.file_name().unwrap(), "My Data.iso");
    let p = get("/", None).await;
    assert_eq!(p.file_name().unwrap(), "download");
    assert!(d.staged_files().is_empty());
    // the parent of dest has only our two dirs
    assert!(!d.tmp.path().join("evil.sh").exists());
}

// ------------------------------------------------------------------ control + misc

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_removes_the_part_unless_told_to_keep_it() {
    let file = FileSrv::new(4 * MIB);
    file.pace_bps.store(1_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/c1.bin"))).unwrap();
    wait_until(&task, "bytes", |p| p.done > 100_000).await;
    task.cancel(false);
    task.wait_for_state(State::Cancelled).await;
    assert!(d.staged_files().is_empty(), "{:?}", d.staged_files());
    assert_eq!(task.wait().await.unwrap_err(), Error::Cancelled);

    let task = dl.start(d.req(&srv.url("/c2.bin"))).unwrap();
    wait_until(&task, "bytes", |p| p.done > 100_000).await;
    task.cancel(true);
    task.wait_for_state(State::Cancelled).await;
    assert_eq!(d.staged_files(), vec!["c2.bin.part".to_owned(), "c2.bin.part.json".to_owned()]);
    // a cancelled task cannot be revived
    task.resume();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(task.progress().state, State::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_paused_waits_for_resume() {
    let file = FileSrv::new(100_000);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/queued.bin"));
    req.start_paused = true;
    let task = dl.start(req).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(task.progress().state, State::Paused);
    assert!(srv.requests().is_empty());
    task.resume();
    let p = task.wait().await.unwrap();
    assert_file_matches(&p, 100_000, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probe_reports_name_type_size_and_range_support() {
    let file = FileSrv::new(123_456);
    *file.content_type.lock().unwrap() = Some("application/x-bittorrent".into());
    *file.content_disposition.lock().unwrap() = Some("attachment; filename=\"ubuntu.iso.torrent\"".into());
    let srv = serve_file(&file).await;
    let dl = Downloader::unlimited(test_cfg());
    let info = dl.probe(&srv.url("/get?id=7")).await.unwrap();
    assert_eq!(info.file_name, "ubuntu.iso.torrent");
    assert_eq!(info.content_type.as_deref(), Some("application/x-bittorrent"));
    assert_eq!(info.total, Some(123_456));
    assert!(info.accept_ranges);
    assert_eq!(info.etag.as_deref(), Some("\"v1\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn content_type_is_visible_in_progress() {
    let file = FileSrv::new(5_000);
    *file.content_type.lock().unwrap() = Some("application/x-bittorrent".into());
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let task = dl.start(d.req(&srv.url("/a.torrent"))).unwrap();
    task.wait().await.unwrap();
    assert_eq!(task.progress().content_type.as_deref(), Some("application/x-bittorrent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn urls_with_credentials_or_odd_schemes_are_refused_up_front() {
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    for url in ["https://user:pw@example.com/f", "ftp://example.com/f", "file:///etc/passwd", "nonsense", ""] {
        assert!(matches!(dl.start(d.req(url)), Err(Error::InvalidUrl(_))), "{url}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[cfg(unix)] // a read-only folder is made with Unix permissions
async fn unwritable_incomplete_dir_surfaces_permission_denied() {
    use std::os::unix::fs::PermissionsExt;
    let file = FileSrv::new(10_000);
    let srv = serve_file(&file).await;
    let d = dirs();
    let ro = d.tmp.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::create_dir(ro.join("probe")).is_ok() {
        return; // running as root: permissions are not enforced
    }
    let dl = Downloader::unlimited(test_cfg());
    let mut req = d.req(&srv.url("/f.bin"));
    req.incomplete_dir = ro.join("inc");
    let err = dl.start(req).unwrap().wait().await.unwrap_err();
    assert!(matches!(err, Error::PermissionDenied(_)), "{err:?}");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The same numbers on macOS and Linux.
#[cfg(unix)]
mod libc_codes {
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const ENOSPC: i32 = 28;
    pub const EROFS: i32 = 30;
}

#[test]
fn disk_full_and_permission_errors_are_typed() {
    use std::io::{Error as IoError, ErrorKind};
    assert_eq!(Error::from(IoError::from(ErrorKind::StorageFull)), Error::DiskFull);
    assert_eq!(Error::from(IoError::from(ErrorKind::QuotaExceeded)), Error::DiskFull);
    assert!(matches!(Error::from(IoError::from(ErrorKind::PermissionDenied)), Error::PermissionDenied(_)));
    assert!(matches!(Error::from(IoError::from(ErrorKind::ReadOnlyFilesystem)), Error::PermissionDenied(_)));
    assert!(matches!(Error::from(IoError::from(ErrorKind::BrokenPipe)), Error::Io(_)));
    // The system's own numbers reach the same kinds.
    #[cfg(unix)]
    {
        assert_eq!(Error::from(IoError::from_raw_os_error(libc_codes::ENOSPC)), Error::DiskFull);
        assert!(matches!(Error::from(IoError::from_raw_os_error(libc_codes::EACCES)), Error::PermissionDenied(_)));
        assert!(matches!(Error::from(IoError::from_raw_os_error(libc_codes::EROFS)), Error::PermissionDenied(_)));
        assert!(matches!(Error::from(IoError::from_raw_os_error(libc_codes::EIO)), Error::Io(_)));
    }
    #[cfg(windows)]
    {
        assert_eq!(Error::from(IoError::from_raw_os_error(112)), Error::DiskFull); // ERROR_DISK_FULL
        assert!(matches!(Error::from(IoError::from_raw_os_error(5)), Error::PermissionDenied(_))); // ERROR_ACCESS_DENIED
        assert!(matches!(Error::from(IoError::from_raw_os_error(30)), Error::Io(_)), "ERROR_READ_FAULT is not EROFS");
        assert!(matches!(Error::from(IoError::from_raw_os_error(122)), Error::Io(_)), "ERROR_INSUFFICIENT_BUFFER is not EDQUOT");
    }
    assert!(!Error::DiskFull.is_retryable());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_downloads_of_the_same_name_do_not_clobber_each_other() {
    let len = 2 * MIB;
    let file = FileSrv::new(len);
    file.pace_bps.store(4_000_000, Ordering::SeqCst);
    let srv = serve_file(&file).await;
    let d = dirs();
    let dl = Downloader::unlimited(test_cfg());
    let a = dl.start(d.req(&srv.url("/same.bin"))).unwrap();
    let b = dl.start(d.req(&srv.url("/same.bin"))).unwrap();
    let (pa, pb) = (a.wait().await.unwrap(), b.wait().await.unwrap());
    assert_ne!(pa, pb);
    assert_file_matches(&pa, len, 0);
    assert_file_matches(&pb, len, 0);
    assert_eq!(d.dest_files(), vec!["same (2).bin".to_owned(), "same.bin".to_owned()]);
}

// ---------------------------------------------------------------- get_small (the app's update check)

#[tokio::test]
async fn get_small_follows_a_redirect_and_reads_the_body() {
    let srv = Server::start(|r, _| match r.path.as_str() {
        "/latest.json" => Reply::redirect(302, "/real.json"),
        _ => Reply::text(200, "{\"version\":\"1.2.3\"}"),
    })
    .await;
    let r = get_small(&srv.url("/latest.json"), "0.1.9", 1024, Duration::from_secs(5)).await.unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, b"{\"version\":\"1.2.3\"}".as_slice()));
    assert_eq!(srv.requests()[0].header("user-agent").as_deref(), Some("Seedwell/0.1.9"));
}

#[tokio::test]
async fn get_small_answers_a_404_as_a_status() {
    let srv = Server::start(|_, _| Reply::status(404)).await;
    let r = get_small(&srv.url("/x"), "0.1.9", 1024, Duration::from_secs(5)).await.unwrap();
    assert_eq!((r.status, r.body.len()), (404, 0));
}

#[tokio::test]
async fn get_small_refuses_a_body_over_its_cap() {
    let big = "x".repeat(2000);
    let srv = Server::start(move |_, _| Reply::text(200, &big)).await;
    let e = get_small(&srv.url("/x"), "0.1.9", 1000, Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(e, Error::BadResponse(_)), "{e:?}");
}

#[tokio::test]
async fn get_small_gives_up_on_a_server_that_never_answers() {
    let srv = Server::start(|_, _| {
        let mut r = Reply::status(200);
        r.body = Some(BodySpec { len: 10, stall: Some(Duration::from_secs(5)), ..Default::default() });
        r
    })
    .await;
    let t = Instant::now();
    let e = get_small(&srv.url("/x"), "0.1.9", 1000, Duration::from_millis(300)).await.unwrap_err();
    assert_eq!(e, Error::Timeout);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}
