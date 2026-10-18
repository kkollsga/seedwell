//! Joins the real DHT and runs one get_peers lookup for a hex info hash, printing progress.
//!
//! ```text
//! cargo run -p trs-dht --example lookup -- 7acf8fb590b2060dd9c3146ef770169d593433b0
//! ```
//!
//! No announce is ever sent. Only use it with info hashes of legal public torrents.

use std::time::{Duration, Instant};
use trs_dht::{DriverConfig, LookupUpdate, PublicInfoHash, spawn};

fn parse_hex(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Some(hash) = std::env::args().nth(1).as_deref().and_then(parse_hex) else {
        eprintln!("usage: lookup <40-char hex info hash>");
        std::process::exit(2);
    };
    let target = PublicInfoHash::new(hash, false).expect("public");
    let dht = spawn(DriverConfig::default()).await.expect("bind");
    let t0 = Instant::now();
    let mut stream = dht
        .get_peers(target)
        .await
        .expect("task")
        .expect("accepted");
    let mut total = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            u = stream.next() => match u {
                Some(LookupUpdate::Peers(p)) => { total += p.len(); eprintln!("{:>6.1?}  +{} peers ({total})", t0.elapsed(), p.len()); }
                Some(LookupUpdate::Finished(s)) => eprintln!("{:>6.1?}  finished: {s:?}", t0.elapsed()),
                Some(LookupUpdate::Announced { .. }) | None => break,
            },
            _ = tick.tick() => {
                let i = dht.info().await.expect("info");
                eprintln!("{:>6.1?}  table={} good={} queries={} replies={} timeouts={}", t0.elapsed(), i.table_nodes, i.good_nodes, i.stats.queries_sent, i.stats.responses_received, i.stats.timeouts);
            }
        }
    }
}
