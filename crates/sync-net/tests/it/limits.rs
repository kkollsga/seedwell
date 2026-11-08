//! The QA switch that makes the speed limits apply to LAN sessions too (`SEEDWELL_QA_LIMIT_LAN`, SPEC §7G.3): honoured
//! only in an isolated run (a state folder inside a temporary folder); and a syncing folder's time left over a link it
//! limits. Loopback only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use seedwell_sync_net::SyncHandle;
use seedwell_sync_net::limit::QA_LIMIT_LAN;
use seedwell_sync_net::status::FolderState;
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::members::Role;

const SIZE: usize = 1 << 20;

/// How long a 1 MiB file takes from A to B over loopback, with A's upload and B's download limits (KiB/s, 0: none),
/// each device's state in a folder of `states`. `None`: not there after `secs`.
async fn transfer(states: &Path, up_kib: u32, down_kib: u32, secs: u64) -> Option<Duration> {
    let a = crate::chat::start_at(&states.join("a"), "Alpha", |_| {}).await;
    let b = crate::chat::start_at(&states.join("b"), "Beta", |_| {}).await;
    a.set_settings(seedwell_sync_net::config::Settings { up_kib, ..a.settings() }).unwrap();
    b.set_settings(seedwell_sync_net::config::Settings { down_kib, ..b.settings() }).unwrap();
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("big.bin"), vec![9u8; SIZE]).unwrap();
    let st = a.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let to = fb.path().join("Shared");
    let start = Instant::now();
    b.join_folder(&link, to.clone()).await.unwrap();
    b.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port())).unwrap();
    let end = start + Duration::from_secs(secs);
    let mut took = None;
    while Instant::now() < end {
        if std::fs::metadata(to.join("big.bin")).is_ok_and(|m| m.len() == SIZE as u64) {
            took = Some(start.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shut(a, b).await;
    took
}

async fn shut(a: SyncHandle, b: SyncHandle) {
    a.shutdown().await;
    b.shutdown().await;
}

fn set(on: bool) {
    // SAFETY: the one test that changes it; each test runs in its own process (nextest), and the services read it
    // only when they start, after this.
    unsafe {
        if on {
            std::env::set_var(QA_LIMIT_LAN, "1");
        } else {
            std::env::remove_var(QA_LIMIT_LAN);
        }
    }
}

/// The LAN is never limited, but in an isolated run with the QA switch on; the user's own Seedwell (a state folder
/// outside any temporary folder) ignores the switch. (One test: the switch is the process's environment.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_qa_switch_limits_the_lan_only_in_an_isolated_run() {
    let temp = tempfile::tempdir().unwrap();

    // Without the switch, a limit of 1 KiB/s (1024 s for the file) does not hold up the LAN.
    set(false);
    assert!(transfer(&temp.path().join("off"), 1, 1, 60).await.is_some(), "the LAN is not limited");

    // With it, in an isolated run: 200 KiB/s down (about 5 s for the file), then 200 KiB/s up.
    set(true);
    let down = transfer(&temp.path().join("down"), 0, 200, 60).await.expect("the file came");
    assert!(down >= Duration::from_millis(2500), "the download limit holds on the LAN: {down:?}");
    let up = transfer(&temp.path().join("up"), 200, 0, 60).await.expect("the file came");
    assert!(up >= Duration::from_millis(2500), "the upload limit holds on the LAN: {up:?}");

    // With it, but the state folders outside any temporary folder (as the user's Seedwell's are): ignored.
    let states = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("qa-limit-lan-{}", std::process::id()));
    assert!(!seedwell_platform::dirs::in_temp_folder(&states), "the build's own folder is not a temporary one: {}", states.display());
    let outside = transfer(&states, 1, 1, 60).await;
    let _ = std::fs::remove_dir_all(&states);
    assert!(outside.is_some(), "the switch is ignored outside an isolated run");
    set(false);
}

/// Bytes that differ throughout (so every chunk is its own and is fetched).
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (s >> 33) as u8
        })
        .collect()
}

/// A member fetching a folder over a slow link, its status polled as the app polls it (every 250 ms): the time left
/// shows while it syncs, in step with the real time left, and goes once it is up to date. Fails if the status never
/// has a time on a real sync (the estimate not fed, the bytes still needed not known while the chunks come), if the
/// time is far off either way (the wait for the first bytes measured as rate, a wrong unit, bytes left undercounted),
/// or if it lingers on a folder that is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_syncing_folder_says_its_time_left_until_it_is_done() {
    const FILES: usize = 4;
    const LEN: usize = 2_000_000;
    let temp = tempfile::tempdir().unwrap();
    let a = crate::chat::start_at(&temp.path().join("a"), "Alpha", |_| {}).await;
    let b = crate::chat::start_at(&temp.path().join("b"), "Beta", |_| {}).await;
    // 320 KiB/s on the LAN: about 25 s for the 8 MB.
    const CAP: f64 = 320.0 * 1024.0;
    b.test_limit_lan(true);
    b.set_settings(seedwell_sync_net::config::Settings { down_kib: 320, ..b.settings() }).unwrap();
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for i in 0..FILES {
        std::fs::write(fa.path().join(format!("part{i}.bin")), noise(LEN, 7 + i as u64)).unwrap();
    }
    let st = a.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let to = fb.path().join("Shared");
    b.join_folder(&link, to.clone()).await.unwrap();
    b.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port())).unwrap();

    let status = || b.snapshot().folders.into_iter().find(|f| f.id == st.id).expect("the folder");
    let all_here = || (0..FILES).all(|i| std::fs::metadata(to.join(format!("part{i}.bin"))).is_ok_and(|m| m.len() == LEN as u64));
    let start = Instant::now();
    let end = start + Duration::from_secs(180);
    // (seconds since the start, the time left the status said)
    let mut shown: Vec<(f64, u64)> = Vec::new();
    loop {
        assert!(Instant::now() < end, "not synced after 180 s; times shown: {shown:?}");
        let f = status();
        if let Some(eta) = f.eta_secs {
            assert_eq!(f.state, FolderState::Syncing, "a time only while it syncs: {f:?}");
            shown.push((start.elapsed().as_secs_f64(), eta));
        }
        if f.state == FolderState::UpToDate && all_here() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let done = start.elapsed().as_secs_f64();
    assert!(!shown.is_empty(), "a time left while it synced ({done:.0} s)");
    for &(at, eta) in &shown {
        let real = done - at;
        assert!((eta as f64) <= real * 3.0 + 15.0, "{eta} s said at {at:.1} s, done {real:.1} s later: {shown:?}");
        // Never wildly short. The cap makes the real time left at least the bytes not fetched by then (all of them,
        // less what the cap lets in since the start and its half-second burst) over the cap; load only makes it
        // longer, so this holds on a slow runner too, where a bound from the measured end would not.
        let least = (FILES * LEN) as f64 / CAP - at - 0.5;
        assert!((eta as f64) >= least * 0.75 - 2.0, "{eta} s said at {at:.1} s, at least {least:.1} s were left: {shown:?}");
    }
    assert_eq!(status().eta_secs, None, "no time once it is up to date");
    shut(a, b).await;
}
