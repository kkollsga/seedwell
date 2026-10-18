//! The seek storm on its own: it measures the process's resident memory, which other tests running
//! beside it in the same binary would disturb, so it gets a test binary of its own.

mod common;

use common::client::*;
use common::fake::FakeSource;
use common::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trs_stream::FileRange;

fn file(name: &str, offset: u64, len: u64) -> FileRange {

    FileRange { index: 0, name: name.into(), offset, len }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seek_storm_50_random_seeks() {
    let (srv, _ev) = server().await;
    let len = 256 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, 400 * MIB);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let path = Arc::new(path_of(&h.url));
    let addr = srv.addr();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut rss_mid = 0;
    let started = Instant::now();
    for round in 0..10 {
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let off = next() % (len - 4096);
            let want = 64 * 1024 + (next() % (2 * MIB)) as usize;
            let (src, path) = (src.clone(), path.clone());
            tasks.push(tokio::spawn(async move {
                let mut c = Client::connect(addr).await;
                c.send("GET", &path, &[("Range", &format!("bytes={off}-"))]).await;
                let r = c.read_head().await.unwrap();
                assert_eq!(r.status, 206);
                let want = want.min((len - off) as usize);
                let body = c.read_body(want).await;
                assert_eq!(body.len(), want);
                assert_eq!(body, src.expected(off, want));
                // drop: the player seeks away
            }));
        }
        for t in tasks {
            tokio::time::timeout(Duration::from_secs(20), t).await.expect("deadlock").unwrap();
        }
        if round == 3 {
            rss_mid = rss_kb();
        }
    }
    assert!(started.elapsed() < Duration::from_secs(40));
    assert!(eventually(3.0, || srv.stats().cursors == 0 && srv.stats().connections == 0).await, "{:?}", srv.stats());
    assert!(eventually(3.0, || src.active_ids().is_empty()).await, "leaked hints for {:?}", src.active_ids());
    let rss_end = rss_kb();
    assert!(rss_end < rss_mid + 24 * 1024, "RSS grew {} KiB -> {} KiB", rss_mid, rss_end);
    assert_eq!(src.unverified_reads(), 0);
    srv.shutdown().await;
}
