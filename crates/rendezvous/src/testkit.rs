//! Loopback fakes for tests: a tiny HTTP/1.1 server that records what it was sent, a rendezvous that keeps records
//! as `worker.js` does (trust on first use, later times only, the size cap), and the Cloudflare API endpoints the
//! setup uses, answering in the shapes of Cloudflare's API reference. Nothing here reaches the Internet.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::client::signed_text;
use crate::{MAX_RECORD, unhex};

/// One request the fake was sent.
#[derive(Clone, Debug)]
pub struct Recorded {
    /// `GET`, `PUT`, …
    pub method: String,
    /// The path and query.
    pub path: String,
    /// The headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Recorded {
    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// An answer: status, content type, body.
pub type Reply = (u16, &'static str, Vec<u8>);

/// A fake HTTP server on loopback.
pub struct FakeServer {
    /// Its address.
    pub addr: SocketAddr,
    /// Every request so far.
    pub log: Arc<Mutex<Vec<Recorded>>>,
    /// Answer nothing (a rendezvous that is down: requests fail).
    pub down: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeServer {
    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The requests so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How many requests with `method` whose path starts with `prefix`.
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.requests().iter().filter(|r| r.method == method && r.path.starts_with(prefix)).count()
    }
}

/// Serves `handler` on a loopback port (one request per connection).
pub async fn serve(handler: Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>) -> FakeServer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("a loopback port");
    let addr = listener.local_addr().expect("its address");
    let log: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (l2, d2) = (log.clone(), down.clone());
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            if d2.load(std::sync::atomic::Ordering::SeqCst) {
                drop(s);
                continue;
            }
            let (handler, log) = (handler.clone(), l2.clone());
            tokio::spawn(async move {
                let Some(req) = read_request(&mut s).await else { return };
                log.lock().unwrap_or_else(|e| e.into_inner()).push(req.clone());
                let (status, ty, body) = handler(&req);
                let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {ty}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    FakeServer { addr, log, down, task }
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Recorded { method, path, headers, body })
}

fn err(status: u16, e: &str) -> Reply {
    (status, "application/json", json!({ "error": e }).to_string().into_bytes())
}

#[derive(Default)]
struct Slot {
    key: String,
    time: u64,
    data: Option<Vec<u8>>,
}

/// A rendezvous on loopback that behaves as `worker.js` (without the rate limit and the expiry).
pub async fn fake_rendezvous() -> FakeServer {
    let records: Arc<Mutex<HashMap<String, Slot>>> = Arc::default();
    serve(Arc::new(move |r: &Recorded| {
        if r.path == "/v1" {
            return (200, "application/json", json!({ "service": "seedwell-rendezvous", "api": 1, "max_record": MAX_RECORD, "ttl_days": 7 }).to_string().into_bytes());
        }
        let Some(id) = r.path.strip_prefix("/v1/r/").filter(|i| i.len() == 64 && i.chars().all(|c| c.is_ascii_hexdigit())) else {
            return err(404, "not found");
        };
        let mut map = records.lock().unwrap_or_else(|e| e.into_inner());
        if r.method == "GET" {
            return match map.get(id).and_then(|s| s.data.clone()) {
                Some(d) => (200, "application/octet-stream", d),
                None => err(404, "no record"),
            };
        }
        if r.body.len() > MAX_RECORD {
            return err(413, "too large");
        }
        let (Some(key), Some(time), Some(sig)) = (r.header("seedwell-key"), r.header("seedwell-time").and_then(|t| t.parse::<u64>().ok()), r.header("seedwell-signature")) else {
            return err(400, "headers");
        };
        let (Some(pk), Some(sig)) = (unhex(key), unhex(sig)) else { return err(400, "hex") };
        let text = signed_text(&r.method, id, time, &r.body);
        if ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk).verify(text.as_bytes(), &sig).is_err() {
            return err(401, "the signature does not verify");
        }
        let slot = map.entry(id.to_string()).or_default();
        if !slot.key.is_empty() && slot.key != key {
            return err(403, "another write key");
        }
        if time <= slot.time {
            return err(409, "not newer than the last write (a replay?)");
        }
        slot.key = key.to_string();
        slot.time = time;
        slot.data = (r.method == "PUT").then(|| r.body.clone());
        (204, "application/json", Vec::new())
    }))
    .await
}

/// How the fake Cloudflare account is.
#[derive(Clone, Debug)]
pub struct Account {
    /// The token it accepts.
    pub token: String,
    /// Its workers.dev subdomain (`None`: not made yet; the setup makes one).
    pub subdomain: Option<String>,
    /// The script's migration tag, when it is there already.
    pub migration_tag: Option<String>,
}

fn envelope(status: u16, result: Value) -> Reply {
    (status, "application/json", json!({ "success": true, "errors": [], "messages": [], "result": result }).to_string().into_bytes())
}

fn cf_error(status: u16, code: i64, message: &str) -> Reply {
    (status, "application/json", json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null }).to_string().into_bytes())
}

/// The Cloudflare API endpoints the setup uses, for one account (`acc0123`, "Kristian's Account").
pub async fn fake_cloudflare(account: Account) -> FakeServer {
    let state = Arc::new(Mutex::new(account));
    serve(Arc::new(move |r: &Recorded| {
        let mut a = state.lock().unwrap_or_else(|e| e.into_inner());
        if r.header("authorization") != Some(&format!("Bearer {}", a.token)) {
            return cf_error(401, 10000, "Authentication error");
        }
        let acc = "/accounts/acc0123";
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/user/tokens/verify") => envelope(200, json!({ "id": "tok", "status": "active" })),
            ("GET", p) if p.starts_with("/accounts?") => envelope(200, json!([{ "id": "acc0123", "name": "Kristian's Account", "type": "standard" }])),
            ("GET", p) if p == format!("{acc}/workers/scripts") => {
                let list: Vec<Value> = a.migration_tag.iter().map(|t| json!({ "id": "seedwell-rendezvous", "migration_tag": t })).collect();
                envelope(200, json!(list))
            }
            ("PUT", p) if p == format!("{acc}/workers/scripts/seedwell-rendezvous") => {
                let ty = r.header("content-type").unwrap_or("");
                let Some(boundary) = ty.strip_prefix("multipart/form-data; boundary=") else { return cf_error(400, 10021, "multipart") };
                let body = String::from_utf8_lossy(&r.body).to_string();
                let meta = body
                    .split(&format!("--{boundary}"))
                    .find(|p| p.contains("name=\"metadata\""))
                    .and_then(|p| p.split_once("\r\n\r\n"))
                    .map(|(_, v)| v.trim_end_matches("\r\n").to_string());
                let Some(meta) = meta.and_then(|m| serde_json::from_str::<Value>(&m).ok()) else { return cf_error(400, 10021, "no metadata") };
                if !body.contains("name=\"worker.js\"; filename=\"worker.js\"\r\nContent-Type: application/javascript+module") {
                    return cf_error(400, 10021, "no module");
                }
                let tag = meta.get("migrations").and_then(|m| m.get("new_tag")).and_then(Value::as_str).map(str::to_owned);
                match (&a.migration_tag, tag) {
                    (None, None) => return cf_error(400, 10074, "Cannot create binding for class Rendezvous because it is not currently configured to implement Durable Objects"),
                    (Some(_), Some(_)) => return cf_error(400, 10079, "migration tag precondition failed"),
                    (_, Some(t)) => a.migration_tag = Some(t),
                    _ => {}
                }
                envelope(200, json!({ "id": "seedwell-rendezvous", "migration_tag": a.migration_tag }))
            }
            ("GET", p) if p == format!("{acc}/workers/subdomain") => match &a.subdomain {
                Some(s) => envelope(200, json!({ "subdomain": s })),
                None => cf_error(404, 10007, "haven't registered workers.dev"),
            },
            ("PUT", p) if p == format!("{acc}/workers/subdomain") => {
                let want = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v.get("subdomain").and_then(Value::as_str).map(str::to_owned));
                let Some(want) = want else { return cf_error(400, 10000, "subdomain") };
                a.subdomain = Some(want.clone());
                envelope(200, json!({ "subdomain": want }))
            }
            ("POST", p) if p == format!("{acc}/workers/scripts/seedwell-rendezvous/subdomain") => {
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                envelope(200, json!({ "enabled": v["enabled"], "previews_enabled": v["previews_enabled"] }))
            }
            _ => cf_error(404, 7003, "Could not route"),
        }
    }))
    .await
}
