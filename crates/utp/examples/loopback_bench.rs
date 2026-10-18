//! Loopback throughput: `cargo run --release -p trs-utp --example loopback_bench -- [payload] [MiB] [streams]`
//! Run under `/usr/bin/time -l` to see CPU time and peak RSS.

use std::net::SocketAddr;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use trs_utp::{Config, UtpSocket};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let payload: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1400);
    let mib: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(256);
    let streams: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(1);
    let cfg = Config {
        max_payload: payload,
        send_buffer: 2 << 20,
        recv_buffer: 2 << 20,
        max_per_ip: 1024,
        ..Config::default()
    };
    let lo: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let a = UtpSocket::bind(lo, cfg.clone()).await.unwrap();
    let b = UtpSocket::bind(lo, cfg).await.unwrap();
    let total = (mib << 20) as u64 / streams as u64;
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..streams {
        let (c, s) = tokio::join!(a.connect(b.local_addr()), b.accept());
        let (mut c, mut s) = (c.unwrap(), s.unwrap().0);
        tasks.push(tokio::spawn(async move {
            let wr = tokio::spawn(async move {
                let buf = vec![0x5Au8; 64 * 1024];
                let mut sent = 0u64;
                while sent < total {
                    let n = buf.len().min((total - sent) as usize);
                    c.write_all(&buf[..n]).await.unwrap();
                    sent += n as u64;
                }
                c.shutdown().await.unwrap();
                c
            });
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = 0u64;
            loop {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                got += n as u64;
            }
            assert_eq!(got, total);
            wr.await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let st = a.stats();
    let sb = b.stats();
    println!(
        "payload {payload} B, {streams} stream(s), {mib} MiB: {secs:.3} s = {:.1} MB/s ({:.2} Gbit/s); datagrams a: in {} out {}, b: in {} out {}",
        (mib << 20) as f64 / 1e6 / secs,
        (mib << 20) as f64 * 8.0 / secs / 1e9,
        st.datagrams_in,
        st.datagrams_out,
        sb.datagrams_in,
        sb.datagrams_out
    );
}
