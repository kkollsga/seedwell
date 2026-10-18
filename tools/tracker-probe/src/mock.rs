//! Local mock tracker used for positive controls (e.g. banned-client
//! regression runs). It answers every announce with a valid, empty-swarm
//! response and can recreate a dual-stack host whose IPv6 path is broken.

use crate::log;
use crate::record::{self, Record, now_ms};
use crate::server::{self, Resp};
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// How the `[::1]` side of the mock behaves.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum V6Mode {
    /// Nothing listens on IPv6: connections are refused immediately.
    Refuse,
    /// Listens on IPv6 but never answers (client must time out).
    Blackhole,
    /// Accepts on IPv6 and immediately closes (empty reply).
    Close,
    /// Serves normally on IPv6 too.
    Ok,
}

/// Arguments for the `mock` subcommand.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// IPv4 address to serve on.
    #[arg(long, default_value = "127.0.0.1:7071")]
    pub listen: SocketAddr,
    /// `interval` returned to clients (seconds).
    #[arg(long, default_value_t = 60)]
    pub interval: i64,
    /// `min interval` returned (seconds); 0 omits the key.
    #[arg(long, default_value_t = 30)]
    pub min_interval: i64,
    /// Behaviour of the IPv6 loopback listener on the same port.
    #[arg(long, value_enum, default_value_t = V6Mode::Refuse)]
    pub v6: V6Mode,
    /// IPv6 address to serve the `--v6` behaviour on (default `[::1]:<port of --listen>`). Needed when
    /// the client refuses loopback trackers (Seedwell's production destination policy): bind a
    /// documentation-range address such as `[2001:db8::9]:7071` on the loopback interface instead.
    #[arg(long)]
    pub listen6: Option<SocketAddr>,
    /// Hold every announce/scrape response back this many milliseconds (makes concurrent in-flight
    /// requests observable; the record's `inflight` field and the exit summary report the maximum).
    #[arg(long, default_value_t = 0)]
    pub delay_ms: u64,
    /// Answer announces with this HTTP status instead of a tracker response (e.g. 503, as the
    /// Ubuntu tracker does when it is overloaded). 0 = off. Scrapes and markers are unaffected.
    #[arg(long, default_value_t = 0)]
    pub fail_status: u16,
    /// With `--fail-status`: only the first N announces fail, then the mock answers normally
    /// (a tracker that recovers). 0 = every announce fails.
    #[arg(long, default_value_t = 0)]
    pub fail_first: u64,
    /// Answer every announce with HTTP 200 and a bencoded `failure reason` of this text (a tracker
    /// that rejects the torrent, the client key or the client). Scrapes are unaffected.
    #[arg(long)]
    pub failure_reason: Option<String>,
    /// IPv4 peers (`ip:port`, comma separated) to hand out in every response, for local
    /// swarm experiments. Default: empty peer list.
    #[arg(long, value_delimiter = ',')]
    pub peers: Vec<std::net::SocketAddrV4>,
    /// JSONL capture file (appended to).
    #[arg(long)]
    pub out: PathBuf,
    /// Label stored in every record.
    #[arg(long)]
    pub label: Option<String>,
}

struct State {
    peers: Vec<std::net::SocketAddrV4>,
    interval: i64,
    min_interval: i64,
    log: log::LogTx,
    label: Option<String>,
    delay_ms: u64,
    fail_status: u16,
    fail_first: u64,
    failure_reason: Option<String>,
    announces: std::sync::atomic::AtomicU64,
    inflight: std::sync::atomic::AtomicU32,
    max_inflight: std::sync::atomic::AtomicU32,
    /// `event=stopped` announces that arrived (counted on arrival, so also those the client
    /// hung up on before the delay was over) and how many of those were never answered.
    stopped_seen: std::sync::atomic::AtomicU32,
    stopped_unanswered: std::sync::atomic::AtomicU32,
}

/// Build the bencoded announce response (empty peer list).
pub fn announce_body(interval: i64, min_interval: i64, compact: bool, peers: &[std::net::SocketAddrV4]) -> Vec<u8> {
    // Keys are emitted in sorted order as the spec requires of encoders.
    let mut b = Vec::new();
    b.extend_from_slice(b"d8:completei0e10:incompletei0e8:intervali");
    b.extend_from_slice(interval.to_string().as_bytes());
    b.push(b'e');
    if min_interval > 0 {
        b.extend_from_slice(b"12:min intervali");
        b.extend_from_slice(min_interval.to_string().as_bytes());
        b.push(b'e');
    }
    if compact {
        b.extend_from_slice(format!("5:peers{}:", peers.len() * 6).as_bytes());
        for p in peers {
            b.extend_from_slice(&p.ip().octets());
            b.extend_from_slice(&p.port().to_be_bytes());
        }
    } else {
        b.extend_from_slice(b"5:peersl");
        for p in peers {
            let ip = p.ip().to_string();
            b.extend_from_slice(format!("d2:ip{}:{ip}4:porti{}ee", ip.len(), p.port()).as_bytes());
        }
        b.push(b'e');
    }
    b.push(b'e');
    b
}

/// Counts one request as in flight until dropped, also when the client hangs up first.
struct InflightGuard(Arc<State>, bool, bool);

impl InflightGuard {
    /// The delay is over: the request is answered, not aborted.
    fn answered(&mut self) {
        self.2 = true;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if self.1 && !self.2 {
            self.0.stopped_unanswered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Bencoded `failure reason` response.
pub fn failure_body(reason: &str) -> Vec<u8> {
    format!("d14:failure reason{}:{reason}e", reason.len()).into_bytes()
}

/// Run the mock until Ctrl-C / SIGTERM.
pub async fn run(args: Args) -> Result<()> {
    let (log_tx, log_task) = log::start(&args.out)?;
    let st = Arc::new(State { peers: args.peers.clone(), interval: args.interval, min_interval: args.min_interval, log: log_tx.clone(), label: args.label.clone(), delay_ms: args.delay_ms, fail_status: args.fail_status, fail_first: args.fail_first, failure_reason: args.failure_reason.clone(), announces: Default::default(), inflight: Default::default(), max_inflight: Default::default(), stopped_seen: Default::default(), stopped_unanswered: Default::default() });
    let v4 = TcpListener::bind(args.listen).await.with_context(|| format!("binding {}", args.listen))?;
    let port = v4.local_addr()?.port();
    eprintln!(
        "tracker-probe mock on {} (interval {}s, min interval {}s, ipv6={:?}) -> {}",
        v4.local_addr()?,
        args.interval,
        args.min_interval,
        args.v6,
        args.out.display()
    );
    let (sd_tx, sd_rx) = watch::channel(false);
    tokio::spawn(server::shutdown_signal(sd_tx));
    let mut tasks = Vec::new();
    let s4 = st.clone();
    tasks.push(tokio::spawn(server::serve(v4, move |req, remote, local| handle(s4.clone(), req, remote, local), sd_rx.clone())));
    if args.v6 != V6Mode::Refuse {
        let addr6: SocketAddr = match args.listen6 {
            Some(a) => a,
            None => format!("[::1]:{port}").parse()?,
        };
        let l6 = TcpListener::bind(addr6).await.with_context(|| format!("binding {addr6}"))?;
        match args.v6 {
            V6Mode::Ok => {
                let s6 = st.clone();
                tasks.push(tokio::spawn(server::serve(l6, move |req, remote, local| handle(s6.clone(), req, remote, local), sd_rx.clone())));
            }
            V6Mode::Blackhole | V6Mode::Close => {
                let hold = args.v6 == V6Mode::Blackhole;
                tasks.push(tokio::spawn(broken_v6(l6, hold, sd_rx.clone())));
            }
            V6Mode::Refuse => {}
        }
    }
    for t in tasks {
        let _ = t.await;
    }
    eprintln!("tracker-probe mock: max concurrent in-flight announce/scrape requests: {}", st.max_inflight.load(std::sync::atomic::Ordering::Relaxed));
    eprintln!(
        "tracker-probe mock: stopped announces received: {} ({} hung up on before the answer)",
        st.stopped_seen.load(std::sync::atomic::Ordering::Relaxed),
        st.stopped_unanswered.load(std::sync::atomic::Ordering::Relaxed)
    );
    drop(st);
    drop(log_tx);
    let _ = log_task.await;
    eprintln!("tracker-probe mock: shut down cleanly, log flushed");
    Ok(())
}

/// Accept IPv6 connections and either hold them silently or drop them.
async fn broken_v6(l: TcpListener, hold: bool, mut sd: watch::Receiver<bool>) {
    let mut held = Vec::new();
    let mut count = 0u64;
    loop {
        tokio::select! {
            r = l.accept() => {
                let Ok((s, _)) = r else { continue };
                count += 1;
                if hold {
                    held.push((s, std::time::Instant::now()));
                    held.retain(|(_, t)| t.elapsed() < Duration::from_secs(120));
                }
            }
            _ = sd.changed() => break,
        }
    }
    eprintln!("tracker-probe mock: {count} IPv6 connection attempts were deliberately broken");
}

async fn handle(st: Arc<State>, req: Request<hyper::body::Incoming>, remote: SocketAddr, local: SocketAddr) -> Resp {
    let ts_ms = now_ms();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let mut rec = Record {
        ts_ms,
        label: st.label.clone(),
        remote: remote.to_string(),
        local: Some(local.to_string()),
        method: req.method().as_str().to_owned(),
        path: path.clone(),
        query: query.clone(),
        headers: req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect(),
        id: Some("mock".into()),
        upstream: Some("mock".into()),
        ..Record::default()
    };
    if let Some(name) = path.strip_prefix("/_marker/") {
        rec.kind = "marker".into();
        rec.marker = Some(String::from_utf8_lossy(&record::percent_decode(name)).into_owned());
        st.log.send(rec);
        return server::text(200, "ok\n");
    }
    let is_scrape = path.ends_with("/scrape") || path == "/scrape";
    rec.kind = if is_scrape { "scrape" } else { "announce" }.into();
    rec.params = record::parse_params(&query);
    rec.info_hash = rec.param("info_hash").map(str::to_owned);
    rec.peer_id = rec.param("peer_id").map(str::to_owned);
    let compact = rec.param("compact") != Some("0");
    let body = if is_scrape {
        b"d5:filesdee".to_vec()
    } else if let Some(reason) = &st.failure_reason {
        failure_body(reason)
    } else { announce_body(st.interval, st.min_interval, compact, &st.peers) };
    use std::sync::atomic::Ordering::Relaxed;
    let now_in = st.inflight.fetch_add(1, Relaxed) + 1;
    st.max_inflight.fetch_max(now_in, Relaxed);
    rec.inflight = Some(now_in);
    // Released when the delay is over or when the client aborts the request (the future is dropped).
    let is_stopped = !is_scrape && rec.param("event") == Some("stopped");
    if is_stopped {
        st.stopped_seen.fetch_add(1, Relaxed);
    }
    let mut guard = InflightGuard(st.clone(), is_stopped, false);
    if st.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(st.delay_ms)).await;
    }
    guard.answered();
    drop(guard);
    let failing = !is_scrape && st.fail_status != 0 && {
        let n = st.announces.fetch_add(1, Relaxed);
        st.fail_first == 0 || n < st.fail_first
    };
    if failing {
        let msg = Bytes::from_static(b"service unavailable\n");
        rec.upstream_status = Some(st.fail_status);
        rec.latency_ms = Some(st.delay_ms);
        rec.response_size = Some(msg.len());
        st.log.send(rec);
        let mut r = Response::new(Full::new(msg));
        *r.status_mut() = hyper::StatusCode::from_u16(st.fail_status).unwrap_or(hyper::StatusCode::SERVICE_UNAVAILABLE);
        return r;
    }
    rec.upstream_status = Some(200);
    rec.latency_ms = Some(st.delay_ms);
    rec.response_size = Some(body.len());
    rec.response = Some(record::summarize_response(&body, None));
    st.log.send(rec);
    let mut r = Response::new(Full::new(Bytes::from(body)));
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/plain"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode;

    #[tokio::test]
    async fn an_aborted_request_is_no_longer_in_flight() {
        let st = Arc::new(State {
            peers: vec![],
            interval: 60,
            min_interval: 30,
            log: log::start(&std::env::temp_dir().join(format!("tp-mock-guard-{}.jsonl", std::process::id()))).unwrap().0,
            label: None,
            delay_ms: 0,
            fail_status: 0,
            fail_first: 0,
            failure_reason: None,
            announces: Default::default(),
            inflight: Default::default(),
            max_inflight: Default::default(),
            stopped_seen: Default::default(),
            stopped_unanswered: Default::default(),
        });
        st.inflight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let g = InflightGuard(st.clone(), false, false);
        assert_eq!(st.inflight.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(g);
        assert_eq!(st.inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn failure_reason_body_is_valid_bencode() {
        let v = bencode::decode(&failure_body("Unregistered torrent")).unwrap();
        assert_eq!(v.get("failure reason").and_then(bencode::Value::as_bytes), Some(&b"Unregistered torrent"[..]));
    }

    #[test]
    fn response_is_valid_bencode_with_expected_keys() {
        for compact in [true, false] {
            let b = announce_body(60, 30, compact, &[]);
            let v = bencode::decode(&b).unwrap();
            assert_eq!(v.get("interval").and_then(bencode::Value::as_int), Some(60));
            assert_eq!(v.get("min interval").and_then(bencode::Value::as_int), Some(30));
            assert!(v.get("peers").is_some());
        }
        let with_peers = announce_body(60, 30, true, &["1.2.3.4:6881".parse().unwrap()]);
        let v = bencode::decode(&with_peers).unwrap();
        assert_eq!(v.get("peers").and_then(bencode::Value::as_bytes), Some(&[1u8, 2, 3, 4, 0x1a, 0xe1][..]));
        let no_min = announce_body(60, 0, true, &[]);
        assert!(bencode::decode(&no_min).unwrap().get("min interval").is_none());
    }
}
