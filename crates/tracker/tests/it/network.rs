//! Real-network conformance, `#[ignore]`d by default.
//!
//! `cargo test -p trs-tracker --test network -- --ignored --nocapture`
//!
//! Announces to Debian's PUBLIC tracker for the current Debian netinst
//! info-hash: ONE `started` and ONE `stopped`, nothing else (be polite). If the
//! torrent lists a `udp://` tracker, one started/stopped pair goes there too.
//! Drives the real `TrackerHub` + `HttpTransport`/`UdpTransport`, so it doubles
//! as the reference event loop. Never contacts a private tracker: the tracker
//! list is restricted to hosts under `debian.org`.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use tokio::sync::mpsc;
use trs_tracker::gate::{GateConfig, Ticket};
use trs_tracker::http::HttpTransport;
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::response::{AnnounceError, AnnounceResponse};
use trs_tracker::udp::{UdpConfig, UdpTransport};
use trs_tracker::*;

const LISTING: &str = "https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/";

fn curl(url: &str) -> Vec<u8> {
    let out = std::process::Command::new("curl")
        .args(["-sSL", "--max-time", "30", url])
        .output()
        .expect("curl is needed for this test");
    assert!(
        out.status.success(),
        "curl {url}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

struct Meta {
    info_hash: [u8; 20],
    size: u64,
    trackers: Vec<String>,
}

fn load_debian_netinst() -> Meta {
    let listing = String::from_utf8_lossy(&curl(LISTING)).to_string();
    let name = listing
        .split('"')
        .find(|s| {
            s.starts_with("debian-")
                && s.ends_with("-amd64-netinst.iso.torrent")
                && !s.starts_with("debian-edu")
                && !s.starts_with("debian-mac")
        })
        .expect("no netinst torrent in the listing")
        .to_owned();
    println!("torrent: {name}");
    let data = curl(&format!("{LISTING}{name}"));
    let info = trs_bencode::raw_dict_get(&data, b"info")
        .unwrap()
        .expect("info dict");
    let info_hash: [u8; 20] = Sha1::digest(info).into();
    let v = trs_bencode::decode(&data).unwrap();
    let size = trs_bencode::decode(info)
        .unwrap()
        .get(b"length")
        .and_then(|l| l.as_int())
        .unwrap_or(0) as u64;
    let mut trackers = BTreeSet::new();
    if let Some(a) = v.get(b"announce").and_then(|a| a.as_str()) {
        trackers.insert(a.to_owned());
    }
    for tier in v
        .get(b"announce-list")
        .and_then(|l| l.as_list())
        .unwrap_or(&[])
    {
        for t in tier.as_list().unwrap_or(&[]) {
            if let Some(s) = t.as_str() {
                trackers.insert(s.to_owned());
            }
        }
    }
    // Safety: only Debian's own public trackers, never anything else.
    let trackers: Vec<String> = trackers
        .into_iter()
        .filter(|t| {
            TrackerUrl::parse(t).is_ok_and(|u| {
                u.host().ends_with(".debian.org")
                    && matches!(
                        u.scheme(),
                        trs_tracker::url::Scheme::Http
                            | trs_tracker::url::Scheme::Https
                            | trs_tracker::url::Scheme::Udp
                    )
            })
        })
        .collect();
    assert!(!trackers.is_empty(), "no Debian tracker in the torrent");
    Meta {
        info_hash,
        size,
        trackers,
    }
}

enum Done {
    Http(Ticket, Result<AnnounceResponse, AnnounceError>, u32, String),
    Udp(Ticket, Result<AnnounceResponse, AnnounceError>),
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "talks to the real network (Debian's public tracker)"]
async fn debian_netinst_started_then_stopped() {
    let meta = load_debian_netinst();
    println!(
        "info-hash {}  size {}  trackers {:?}",
        meta.info_hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        meta.size,
        meta.trackers
    );
    let tiers: Vec<Vec<TrackerUrl>> = vec![
        meta.trackers
            .iter()
            .map(|t| TrackerUrl::parse(t).unwrap())
            .collect(),
    ];
    let mut peer_id = *b"-SX0001-............";
    for (i, b) in peer_id[8..].iter_mut().enumerate() {
        *b = b'a' + ((meta.info_hash[i] ^ 0x5a) % 26);
    }
    let mut spec = TorrentSpec::new(meta.info_hash, peer_id, 0x1234_5678, 6881, tiers);
    spec.numwant = 10; // moderate
    let t0 = Instant::now();
    let clock = move || Millis(t0.elapsed().as_millis() as u64);

    let mut hub = TrackerHub::new(GateConfig::default(), MemoryStore::new(), 7);
    hub.add_torrent(spec, clock()).unwrap();
    let http = HttpTransport::new(env!("CARGO_PKG_VERSION"));
    let udp = UdpTransport::bind(UdpConfig::default()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<Done>();

    let ih = meta.info_hash;
    hub.set_stats(
        &ih,
        Stats {
            uploaded: 0,
            downloaded: 0,
            corrupt: 0,
            left: meta.size,
            size: meta.size,
        },
        clock(),
    );
    hub.torrent_event(&ih, TorrentEvent::Start, clock());

    let mut started_seen = false;
    let mut stop_sent = false;
    let mut stopped_seen = false;
    let mut announced: Vec<(Option<Event>, AnnounceResponse)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sent = 0;
    while !stopped_seen {
        assert!(
            Instant::now() < deadline,
            "timed out; announced so far: {announced:?}"
        );
        let now = clock();
        for req in hub.tick(now) {
            sent += 1;
            let (http, udp, tx) = (http.clone(), udp.clone(), tx.clone());
            tokio::spawn(async move {
                let ticket = req.ticket();
                if req.url().scheme() == trs_tracker::url::Scheme::Udp {
                    let r = udp.announce(&req).await;
                    let _ = tx.send(Done::Udp(ticket, r));
                } else {
                    let out = http.announce(&req).await;
                    let _ = tx.send(Done::Http(
                        ticket,
                        out.result,
                        out.extra_hops,
                        req.url().host().to_owned(),
                    ));
                }
            });
        }
        for r in hub.take_reports() {
            match r.report {
                Report::Announced { response, event } => {
                    println!(
                        "[{:>6.2}s] announced event={event:?} interval={:?} min={:?} seeders={:?} leechers={:?} peers={}",
                        t0.elapsed().as_secs_f64(),
                        response.interval,
                        response.min_interval,
                        response.complete,
                        response.incomplete,
                        response.peers.len()
                    );
                    started_seen |= event == Some(Event::Started);
                    stopped_seen |= event == Some(Event::Stopped);
                    announced.push((event, *response));
                }
                Report::Failed { error, .. } => panic!("announce failed: {error}"),
                Report::Halted { reason } => panic!("tracker rejected us: {reason}"),
                Report::Warning(w) => println!("tracker warning: {w}"),
                _ => {}
            }
        }
        if started_seen && !stop_sent {
            stop_sent = true;
            hub.torrent_event(&ih, TorrentEvent::Stop, clock());
            continue;
        }
        let wait = hub
            .next_wake(clock())
            .map_or(Duration::from_millis(200), |w| {
                Duration::from_millis(w.0.saturating_sub(clock().0).clamp(1, 500))
            });
        tokio::select! {
            Some(done) = rx.recv() => {
                let now = clock();
                match done {
                    Done::Http(t, r, hops, host) => {
                        hub.charge_extra(&host, hops);
                        hub.complete(t, r, now);
                    }
                    Done::Udp(t, r) => hub.complete(t, r, now),
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }
    // Exactly one pair per tracker we actually used (one tracker is active at a time).
    assert!((2..=4).contains(&sent), "polite: {sent} requests");
    let started = announced
        .iter()
        .find(|(e, _)| *e == Some(Event::Started))
        .expect("started answered");
    assert!(
        started.1.interval >= Duration::from_secs(60),
        "interval was clamped to a sane value"
    );
    assert!(
        started.1.complete.is_some() || !started.1.peers.is_empty(),
        "the tracker returned swarm information"
    );
    assert!(hub.is_quiescent(&ih));
}
