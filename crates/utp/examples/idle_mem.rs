//! Memory per idle connection: `cargo run --release -p trs-utp --example idle_mem -- [N]`
//! Opens N connections between two sockets in this process, exchanges a few bytes, waits for the
//! idle-release timer, and reports RSS growth per connection (both ends counted).

use std::net::SocketAddr;
use std::process::Command;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use trs_utp::{Config, UtpSocket};

fn rss_kib() -> u64 {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(1000);
    let cfg = Config {
        max_per_ip: n + 10,
        max_connections: 2 * n + 10,
        max_half_open: n + 10,
        accept_backlog: n + 10,
        ..Config::default()
    };
    let lo: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let a = UtpSocket::bind(lo, cfg.clone()).await.unwrap();
    let b = UtpSocket::bind(lo, cfg).await.unwrap();
    // warm up allocator, runtime and tables with a handful of connections first
    {
        let mut warm = Vec::new();
        for _ in 0..16 {
            let (c, s) = tokio::join!(a.connect(b.local_addr()), b.accept());
            warm.push((c.unwrap(), s.unwrap().0));
        }
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    let base = rss_kib();
    let mut conns = Vec::with_capacity(n);
    for _ in 0..n {
        let (c, s) = tokio::join!(a.connect(b.local_addr()), b.accept());
        conns.push((c.unwrap(), s.unwrap().0));
    }
    let after_open = rss_kib();
    for (c, s) in conns.iter_mut() {
        c.write_all(b"\x13BitTorrent protocol").await.unwrap();
        let mut buf = [0u8; 20];
        s.read_exact(&mut buf).await.unwrap();
    }
    let after_data = rss_kib();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let idle = rss_kib();
    let w0 = (a.stats().wakeups, b.stats().wakeups);
    tokio::time::sleep(Duration::from_secs(20)).await;
    let w1 = (a.stats().wakeups, b.stats().wakeups);
    let heap: usize = conns.iter().map(|(c, s)| c.buffered_heap_bytes() + s.buffered_heap_bytes()).sum();
    let per = |kib: u64| (kib.saturating_sub(base)) as f64 * 1024.0 / (2 * n) as f64;
    println!("{n} connections ({} endpoint-side connections), RSS baseline {base} KiB", 2 * n);
    println!("  after connect:      +{:>6} KiB = {:>6.0} B per connection end", after_open - base.min(after_open), per(after_open));
    println!("  after 20 B + read:  +{:>6} KiB = {:>6.0} B per connection end", after_data.saturating_sub(base), per(after_data));
    println!("  idle (buffers freed): +{:>6} KiB = {:>6.0} B per connection end", idle.saturating_sub(base), per(idle));
    println!("  buffer heap still held by streams after idling: {heap} B");
    println!(
        "  driver wake-ups while all {n} connections sit idle: {:.2}/s (socket a), {:.2}/s (socket b) over 20 s",
        (w1.0 - w0.0) as f64 / 20.0,
        (w1.1 - w0.1) as f64 / 20.0
    );
    println!("  live: a={} b={}", a.stats().live_connections, b.stats().live_connections);
}
