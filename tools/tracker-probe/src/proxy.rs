//! Logging reverse proxy: `/t/<id>/announce?<q>` -> `<upstream>?<q>` with the
//! query forwarded byte-for-byte, the upstream response returned unchanged.
//!
//! This only OBSERVES real clients. It never alters parameters and never
//! synthesises requests of its own.

use crate::log::{self, LogTx};
use crate::record::{self, Record, now_ms};
use crate::server::{self, Resp};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::watch;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Arguments for the `proxy` subcommand.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Address to listen on, e.g. 127.0.0.1:7070.
    #[arg(long, default_value = "127.0.0.1:7070")]
    pub listen: SocketAddr,
    /// `<id>=<upstream_announce_url>`; repeatable.
    #[arg(long = "map", value_name = "ID=URL")]
    pub maps: Vec<String>,
    /// JSONL capture file (appended to).
    #[arg(long)]
    pub out: PathBuf,
    /// Label stored in every record (e.g. `transmission-4.1.3`).
    #[arg(long)]
    pub label: Option<String>,
}

type Upstream = Client<HttpsConnector<HttpConnector>, Empty<Bytes>>;

struct State {
    maps: HashMap<String, String>,
    client: Upstream,
    log: LogTx,
    label: Option<String>,
}

/// Parse `id=url` mappings and validate them.
pub fn parse_maps(raw: &[String]) -> Result<HashMap<String, String>> {
    let mut m = HashMap::new();
    for item in raw {
        let (id, url) = item.split_once('=').with_context(|| format!("--map {item:?} must be ID=URL"))?;
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            bail!("map id {id:?} must be [A-Za-z0-9_-]+");
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("upstream for {id:?} must be http(s): {}", record::redact_url(url));
        }
        url.parse::<Uri>().with_context(|| format!("invalid upstream URL for {id}"))?;
        if m.insert(id.to_owned(), url.to_owned()).is_some() {
            bail!("duplicate map id {id:?}");
        }
    }
    Ok(m)
}

/// Compute the upstream URL for a request, appending `raw_query` unchanged.
/// `scrape` rewrites the last path component per BEP 48.
pub fn upstream_url(base: &str, kind: &str, raw_query: &str) -> Option<String> {
    let (base_path, base_query) = match base.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base, None),
    };
    let path = if kind == "scrape" {
        let slash = base_path.rfind('/')?;
        let last = &base_path[slash + 1..];
        let rest = last.strip_prefix("announce")?;
        format!("{}/scrape{rest}", &base_path[..slash])
    } else {
        base_path.to_owned()
    };
    let mut q = String::new();
    if let Some(bq) = base_query.filter(|s| !s.is_empty()) {
        q.push_str(bq);
    }
    if !raw_query.is_empty() {
        if !q.is_empty() {
            q.push('&');
        }
        q.push_str(raw_query);
    }
    Some(if q.is_empty() { path } else { format!("{path}?{q}") })
}

/// Hop-by-hop headers that must not be forwarded (RFC 9110 7.6.1).
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "proxy-connection" | "proxy-authenticate" | "proxy-authorization" | "te" | "trailer" | "transfer-encoding" | "upgrade" | "host"
    )
}

/// Parse `/t/<id>/<announce|scrape>`.
pub fn route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/t/")?;
    let (id, kind) = rest.split_once('/')?;
    matches!(kind, "announce" | "scrape").then_some((id, kind))
}

fn make_client() -> Upstream {
    let connector = HttpsConnectorBuilder::new().with_webpki_roots().https_or_http().enable_http1().build();
    Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(connector)
}

/// Run the proxy until Ctrl-C / SIGTERM.
pub async fn run(args: Args) -> Result<()> {
    let maps = parse_maps(&args.maps)?;
    if maps.is_empty() {
        bail!("at least one --map ID=URL is required");
    }
    let client = make_client();
    let (log_tx, log_task) = log::start(&args.out)?;
    let state = Arc::new(State { maps, client, log: log_tx.clone(), label: args.label.clone() });
    let listener = TcpListener::bind(args.listen).await.with_context(|| format!("binding {}", args.listen))?;
    eprintln!("tracker-probe proxy listening on http://{}  ->  {}", listener.local_addr()?, args.out.display());
    for (id, url) in &state.maps {
        eprintln!("  /t/{id}/announce  ->  {}", record::redact_url(url));
    }
    let (sd_tx, sd_rx) = watch::channel(false);
    tokio::spawn(server::shutdown_signal(sd_tx));
    let st = state.clone();
    server::serve(listener, move |req, remote, local| handle(st.clone(), req, remote, local), sd_rx).await;
    drop(state);
    drop(log_tx);
    let _ = log_task.await;
    eprintln!("tracker-probe proxy: shut down cleanly, log flushed");
    Ok(())
}

async fn handle(st: Arc<State>, req: Request<Incoming>, remote: SocketAddr, local: SocketAddr) -> Resp {
    let ts_ms = now_ms();
    let method = req.method().as_str().to_owned();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let headers: Vec<(String, String)> =
        req.headers().iter().map(|(k, v)| (k.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect();
    let mut rec = Record {
        ts_ms,
        label: st.label.clone(),
        remote: remote.to_string(),
        local: Some(local.to_string()),
        method: method.clone(),
        path: path.clone(),
        query: query.clone(),
        headers,
        ..Record::default()
    };
    // Run markers: `curl http://proxy/_marker/<name>` timestamps lifecycle steps in the same log.
    if let Some(name) = path.strip_prefix("/_marker/") {
        rec.kind = "marker".into();
        rec.marker = Some(String::from_utf8_lossy(&record::percent_decode(name)).into_owned());
        st.log.send(rec);
        return server::text(200, "ok\n");
    }
    let Some((id, kind)) = route(&path) else {
        rec.kind = "other".into();
        rec.error = Some("no route".into());
        st.log.send(rec);
        return server::text(404, "not found\n");
    };
    rec.kind = kind.to_owned();
    rec.id = Some(id.to_owned());
    rec.params = record::parse_params(&query);
    rec.info_hash = rec.param("info_hash").map(str::to_owned);
    rec.peer_id = rec.param("peer_id").map(str::to_owned);
    if method != "GET" {
        rec.error = Some("method not allowed".into());
        st.log.send(rec);
        return server::text(405, "GET only\n");
    }
    let Some(base) = st.maps.get(id) else {
        rec.error = Some("unknown id".into());
        st.log.send(rec);
        return server::text(404, "unknown tracker id\n");
    };
    let Some(url) = upstream_url(base, kind, &query) else {
        rec.error = Some("upstream URL has no announce component to derive scrape from".into());
        st.log.send(rec);
        return server::text(400, "cannot derive scrape URL\n");
    };
    rec.upstream = Some(record::redact_url(&url));
    let uri: Uri = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            rec.error = Some(format!("bad upstream uri: {e}"));
            st.log.send(rec);
            return server::text(400, "query is not a valid URI\n");
        }
    };
    let mut up = Request::builder().method("GET").uri(uri);
    if let Some(h) = up.headers_mut() {
        for (k, v) in req.headers() {
            if !hop_by_hop(k.as_str()) {
                h.append(k.clone(), v.clone());
            }
        }
    }
    let up = match up.body(Empty::<Bytes>::new()) {
        Ok(r) => r,
        Err(e) => {
            rec.error = Some(format!("build request: {e}"));
            st.log.send(rec);
            return server::text(500, "internal error\n");
        }
    };
    let started = Instant::now();
    let result = tokio::time::timeout(UPSTREAM_TIMEOUT, async {
        let resp = st.client.request(up).await.map_err(|e| format!("upstream request: {e}"))?;
        let (parts, body) = resp.into_parts();
        let bytes = Limited::new(body, MAX_RESPONSE).collect().await.map_err(|e| format!("upstream body: {e}"))?.to_bytes();
        Ok::<_, String>((parts, bytes))
    })
    .await;
    rec.latency_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    match result {
        Ok(Ok((parts, body))) => {
            let ce = parts.headers.get(hyper::header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).map(str::to_owned);
            rec.upstream_status = Some(parts.status.as_u16());
            rec.response_size = Some(body.len());
            rec.response = Some(record::summarize_response(&body, ce.as_deref()));
            rec.content_encoding = ce;
            let mut out = Response::new(Full::new(body));
            *out.status_mut() = parts.status;
            for (k, v) in &parts.headers {
                let n = k.as_str();
                if !hop_by_hop(n) && n != "content-length" {
                    out.headers_mut().append(k.clone(), v.clone());
                }
            }
            st.log.send(rec);
            out
        }
        Ok(Err(e)) => {
            rec.error = Some(e);
            st.log.send(rec);
            gateway_error("bad gateway")
        }
        Err(_) => {
            rec.error = Some(format!("upstream timeout after {}s", UPSTREAM_TIMEOUT.as_secs()));
            st.log.send(rec);
            gateway_error("gateway timeout")
        }
    }
}

/// A bencoded failure so clients treat proxy-side errors like tracker errors.
fn gateway_error(msg: &str) -> Resp {
    let body = format!("d14:failure reason{}:{}e", msg.len() + 7, format_args!("proxy: {msg}"));
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = hyper::StatusCode::BAD_GATEWAY;
    r.headers_mut().insert(HeaderName::from_static("content-type"), HeaderValue::from_static("text/plain"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_appended_byte_exact() {
        let q = "info_hash=%5B%0A%FF%aB&peer_id=-TR4130-%2F&weird=%zz&a=b+c&uploaded=0";
        let u = upstream_url("http://bttracker.debian.org:6969/announce", "announce", q).unwrap();
        assert_eq!(u, format!("http://bttracker.debian.org:6969/announce?{q}"));
    }

    #[test]
    fn existing_upstream_query_is_joined() {
        let u = upstream_url("https://t.example/announce?key=abc", "announce", "x=1").unwrap();
        assert_eq!(u, "https://t.example/announce?key=abc&x=1");
        assert_eq!(upstream_url("https://t.example/announce", "announce", "").unwrap(), "https://t.example/announce");
    }

    #[test]
    fn scrape_derivation() {
        assert_eq!(upstream_url("http://t/x/announce", "scrape", "info_hash=%01").unwrap(), "http://t/x/scrape?info_hash=%01");
        assert_eq!(upstream_url("http://t/announce.php", "scrape", "").unwrap(), "http://t/scrape.php");
        assert!(upstream_url("http://t/foo", "scrape", "").is_none());
    }

    #[test]
    fn routes() {
        assert_eq!(route("/t/debian/announce"), Some(("debian", "announce")));
        assert_eq!(route("/t/debian/scrape"), Some(("debian", "scrape")));
        assert_eq!(route("/t/debian/other"), None);
        assert_eq!(route("/announce"), None);
    }

    /// End-to-end: a raw upstream sees the exact request target the client sent,
    /// and the client gets the upstream body back unchanged.
    #[tokio::test]
    async fn end_to_end_query_and_body_preserved() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = upstream.local_addr().unwrap();
        let body: &[u8] = b"d8:intervali1800e5:peers0:e";
        let up_task = tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            s.write_all(resp.as_bytes()).await.unwrap();
            s.write_all(body).await.unwrap();
            head
        });
        let dir = std::env::temp_dir().join(format!("tp-test-{}", std::process::id()));
        let out = dir.join("cap.jsonl");
        let (log_tx, log_task) = log::start(&out).unwrap();
        let maps = parse_maps(&[format!("t=http://{up_addr}/announce")]).unwrap();
        let st = Arc::new(State { maps, client: make_client(), log: log_tx.clone(), label: Some("e2e".into()) });
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (sd_tx, sd_rx) = watch::channel(false);
        let st2 = st.clone();
        let srv = tokio::spawn(server::serve(l, move |r, a, b| handle(st2.clone(), r, a, b), sd_rx));
        let query = "info_hash=%5B%0A%FF%aB%00&peer_id=-TR4130-abc&uploaded=0&event=started&weird=%zz";
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET /t/t/announce?{query} HTTP/1.1\r\nHost: x\r\nUser-Agent: Test/1\r\nConnection: close\r\n\r\n");
        c.write_all(req.as_bytes()).await.unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).await.unwrap();
        assert!(got.ends_with(body), "{:?}", String::from_utf8_lossy(&got));
        let head = up_task.await.unwrap();
        assert!(head.starts_with(&format!("GET /announce?{query} HTTP/1.1\r\n")), "{head}");
        assert!(head.to_ascii_lowercase().contains("user-agent: test/1"));
        sd_tx.send(true).unwrap();
        srv.await.unwrap();
        drop(st);
        drop(log_tx);
        log_task.await.unwrap();
        let (recs, bad) = record::read_records(&out).unwrap();
        assert_eq!(bad, 0);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].query, query);
        assert_eq!(recs[0].info_hash.as_deref(), Some("5b0aff ab00".replace(' ', "").as_str()));
        assert_eq!(recs[0].response.as_ref().and_then(|r| r.interval), Some(1800));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn map_parsing() {
        let m = parse_maps(&["a=http://x/announce".into(), "b-1=https://y/announce".into()]).unwrap();
        assert_eq!(m.len(), 2);
        assert!(parse_maps(&["a=udp://x:1/announce".into()]).is_err());
        assert!(parse_maps(&["bad id=http://x/a".into()]).is_err());
        assert!(parse_maps(&["a=http://x/a".into(), "a=http://y/a".into()]).is_err());
        assert!(parse_maps(&["nourl".into()]).is_err());
    }
}
