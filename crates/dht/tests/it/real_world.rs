//! Live-network test against the real Mainline DHT. Run explicitly:
//!
//! ```text
//! cargo test -p trs-dht --test real_world -- --ignored --nocapture
//! ```
//!
//! Polite by construction: one lookup, no `announce_peer` (we have no data), and the node is shut
//! down as soon as the lookup ends. The target is the Debian netinst amd64 ISO, whose info hash is
//! derived from the official `.torrent` (SHA-1 of the original `info` bytes).

use sha1::{Digest, Sha1};
use std::process::Command;
use std::time::{Duration, Instant};
use trs_dht::{DriverConfig, LookupUpdate, PublicInfoHash, spawn};

const DIR_URL: &str = "https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/";
/// Fallback if the directory listing cannot be fetched: Debian 13.7.0 amd64 netinst.
const FALLBACK_NAME: &str = "debian-13.7.0-amd64-netinst.iso.torrent";

fn curl(url: &str, out: Option<&std::path::Path>) -> Option<Vec<u8>> {
    let mut c = Command::new("curl");
    c.args(["-sSfL", "-m", "60"]);
    if let Some(o) = out {
        c.arg("-o").arg(o);
    }
    c.arg(url);
    let o = c.output().ok()?;
    if !o.status.success() {
        return None;
    }
    match out {
        Some(p) => std::fs::read(p).ok(),
        None => Some(o.stdout),
    }
}

/// Skips one bencoded value starting at `i`; returns the index after it.
fn skip(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'i' => Some(i + b[i..].iter().position(|&c| c == b'e')? + 1),
        b'l' => {
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                j = skip(b, j)?;
            }
            Some(j + 1)
        }
        b'd' => {
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                j = skip(b, j)?;
                j = skip(b, j)?;
            }
            Some(j + 1)
        }
        b'0'..=b'9' => {
            let colon = i + b[i..].iter().position(|&c| c == b':')?;
            let len: usize = std::str::from_utf8(&b[i..colon]).ok()?.parse().ok()?;
            Some(colon + 1 + len)
        }
        _ => None,
    }
}

/// SHA-1 of the raw bytes of the top-level `info` value.
fn info_hash(torrent: &[u8]) -> Option<[u8; 20]> {
    if *torrent.first()? != b'd' {
        return None;
    }
    let mut i = 1;
    while *torrent.get(i)? != b'e' {
        let k_end = skip(torrent, i)?;
        let colon = i + torrent[i..].iter().position(|&c| c == b':')?;
        let key = &torrent[colon + 1..k_end];
        let v_end = skip(torrent, k_end)?;
        if key == b"info" {
            return Some(Sha1::digest(&torrent[k_end..v_end]).into());
        }
        i = v_end;
    }
    None
}

fn debian_info_hash() -> ([u8; 20], String) {
    let dir = std::env::temp_dir().join(format!("trs-dht-realworld-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let listing = curl(DIR_URL, None)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let name = listing
        .split('"')
        .find(|s| {
            s.starts_with("debian-")
                && s.ends_with("-amd64-netinst.iso.torrent")
                && !s.contains("edu")
                && !s.contains("mac")
        })
        .unwrap_or(FALLBACK_NAME)
        .to_owned();
    let path = dir.join(&name);
    let bytes = curl(&format!("{DIR_URL}{name}"), Some(&path)).expect("download Debian .torrent");
    let h = info_hash(&bytes).expect("parse info dict");
    let _ = std::fs::remove_dir_all(&dir);
    (h, name)
}

fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn info_hash_extraction_matches_a_known_vector() {
    // d4:infod4:name1:xe3:fooi1ee -> SHA-1("d4:name1:xe")
    let t = b"d3:fooi1e4:infod4:name1:xee";
    assert_eq!(
        info_hash(t).unwrap().to_vec(),
        Sha1::digest(b"d4:name1:xe").to_vec()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "talks to the public DHT; run explicitly"]
async fn debian_netinst_get_peers_on_the_real_dht() {
    let t_dl = Instant::now();
    let (ih, name) = debian_info_hash();
    eprintln!("(fetched .torrent in {:.1?})", t_dl.elapsed());
    eprintln!("torrent: {name}\ninfo hash: {}", hex(&ih));
    let target = PublicInfoHash::new(ih, false).unwrap();

    let t_start = Instant::now();
    let dht = spawn(DriverConfig::default()).await.unwrap();
    eprintln!("listening on {}", dht.local_addr());

    // Phase 1: bootstrap.
    let mut t_boot = None;
    while t_start.elapsed() < Duration::from_secs(60) {
        let i = dht.info().await.unwrap();
        if i.table_nodes >= 8 {
            t_boot = Some(t_start.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let t_boot = t_boot.expect("bootstrap: >= 8 routing-table nodes within 60 s");
    let after_boot = dht.info().await.unwrap();
    eprintln!(
        "bootstrap: {} nodes in table after {:.2?}; queries so far {}",
        after_boot.table_nodes, t_boot, after_boot.stats.queries_sent
    );

    // Phase 2: one get_peers lookup, no announce.
    let t_lookup = Instant::now();
    let mut stream = dht
        .get_peers(target)
        .await
        .unwrap()
        .expect("lookup accepted");
    let mut peers = std::collections::HashSet::new();
    let (mut t_first, mut t_20) = (None, None);
    let mut stats = None;
    let deadline = Instant::now() + Duration::from_secs(90);
    while let Ok(Some(u)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        stream.next(),
    )
    .await
    {
        match u {
            LookupUpdate::Peers(p) => {
                peers.extend(p);
                t_first.get_or_insert_with(|| t_lookup.elapsed());
                if peers.len() >= 20 {
                    t_20.get_or_insert_with(|| t_lookup.elapsed());
                }
            }
            LookupUpdate::Finished(s) => stats = Some(s),
            LookupUpdate::Announced { .. } => panic!("we never announce in this test"),
        }
    }
    let info = dht.info().await.unwrap();
    eprintln!("--- real DHT result ---");
    eprintln!("first peer after      : {:?}", t_first);
    eprintln!(
        ">= 20 peers after     : {:?} (since lookup start); {:.2?} since process start",
        t_20,
        t_20.map(|t| t + t_boot).unwrap_or_default()
    );
    eprintln!("distinct peers found  : {}", peers.len());
    eprintln!("lookup stats          : {:?}", stats);
    eprintln!(
        "queries sent (total)  : {} (bootstrap {} + lookup {})",
        info.stats.queries_sent,
        after_boot.stats.queries_sent,
        info.stats.queries_sent - after_boot.stats.queries_sent
    );
    eprintln!(
        "responses / timeouts  : {} / {}",
        info.stats.responses_received, info.stats.timeouts
    );
    eprintln!(
        "table nodes           : {} (good {})",
        info.table_nodes, info.good_nodes
    );
    eprintln!("external ip learned   : {:?}", info.external_ip);
    let state = dht.shutdown().await.unwrap();
    eprintln!("persisted state       : {} bytes", state.len());
    assert!(
        !peers.is_empty(),
        "the real DHT returned no peers for a popular Debian ISO"
    );
}
