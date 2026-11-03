//! The QA switch that makes the speed limits apply to LAN sessions too (`SEEDWELL_QA_LIMIT_LAN`, SPEC §7G.3): honoured
//! only in an isolated run (a state folder inside a temporary folder). Loopback only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use trs_sync_net::SyncHandle;
use trs_sync_net::limit::QA_LIMIT_LAN;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

const SIZE: usize = 1 << 20;

/// How long a 1 MiB file takes from A to B over loopback, with A's upload and B's download limits (KiB/s, 0: none),
/// each device's state in a folder of `states`. `None`: not there after `secs`.
async fn transfer(states: &Path, up_kib: u32, down_kib: u32, secs: u64) -> Option<Duration> {
    let a = crate::chat::start_at(&states.join("a"), "Alpha", |_| {}).await;
    let b = crate::chat::start_at(&states.join("b"), "Beta", |_| {}).await;
    a.set_settings(trs_sync_net::config::Settings { up_kib, ..a.settings() }).unwrap();
    b.set_settings(trs_sync_net::config::Settings { down_kib, ..b.settings() }).unwrap();
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
    assert!(!trs_platform::dirs::in_temp_folder(&states), "the build's own folder is not a temporary one: {}", states.display());
    let outside = transfer(&states, 1, 1, 60).await;
    let _ = std::fs::remove_dir_all(&states);
    assert!(outside.is_some(), "the switch is ignored outside an isolated run");
    set(false);
}
