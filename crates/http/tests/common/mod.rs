//! In-test HTTP server (hyper) that can misbehave on demand, plus helpers.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::HeaderMap;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use trs_http::{BoxFuture, DownloadRequest, DownloaderConfig, RateGate};

// ---------------------------------------------------------------- patterns

/// Deterministic file content: byte at absolute offset `i`.
pub fn pat(i: u64, salt: u64) -> u8 {
    ((i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8) ^ (salt as u8)
}

pub fn pattern(start: u64, len: u64, salt: u64) -> Vec<u8> {
    (start..start + len).map(|i| pat(i, salt)).collect()
}

pub fn assert_file_matches(path: &Path, len: u64, salt: u64) {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(data.len() as u64, len, "length of {}", path.display());
    for (chunk_no, chunk) in data.chunks(1 << 16).enumerate() {
        let off = (chunk_no as u64) << 16;
        if chunk != pattern(off, chunk.len() as u64, salt).as_slice() {
            panic!("content differs in 64 KiB chunk {chunk_no}");
        }
    }
}

// ---------------------------------------------------------------- server

#[derive(Clone, Debug)]
pub struct Req {
    pub path: String,
    pub headers: HeaderMap,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
    }
}

#[derive(Clone, Default)]
pub struct BodySpec {
    pub start: u64,
    pub len: u64,
    pub salt: u64,
    /// Bytes per second per response (None = as fast as the client reads).
    pub pace_bps: Option<u64>,
    /// Frame size (default 16 KiB).
    pub chunk: Option<usize>,
    /// Abort the connection after this many body bytes.
    pub drop_after: Option<u64>,
    /// Sleep this long before the first body byte.
    pub stall: Option<Duration>,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<BodySpec>,
    /// Content-Length header to advertise (defaults to body.len).
    pub content_length: Option<u64>,
    /// Raw non-pattern body.
    pub raw: Option<Vec<u8>>,
    /// Send with chunked transfer encoding (length unknown to the client).
    pub chunked: bool,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Reply { status, headers: vec![], body: None, content_length: None, raw: None, chunked: false }
    }
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_owned(), v.into()));
        self
    }
    pub fn redirect(status: u16, location: &str) -> Self {
        Reply::status(status).header("Location", location)
    }
    pub fn text(status: u16, body: &str) -> Self {
        let mut r = Reply::status(status);
        r.raw = Some(body.as_bytes().to_vec());
        r
    }
}

pub struct TestBody {
    rx: mpsc::Receiver<Result<Bytes, io::Error>>,
    exact: Option<u64>,
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(Frame::data(b)))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
    fn size_hint(&self) -> SizeHint {
        match self.exact {
            Some(n) => SizeHint::with_exact(n),
            None => SizeHint::default(),
        }
    }
}

pub struct Stats {
    pub log: Mutex<Vec<Req>>,
    pub inflight: AtomicUsize,
    pub max_inflight: AtomicUsize,
    pub bytes_sent: AtomicU64,
}

struct InflightGuard(Arc<Stats>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub stats: Arc<Stats>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start<F>(handler: F) -> Server
    where
        F: Fn(&Req, usize) -> Reply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stats = Arc::new(Stats {
            log: Mutex::new(vec![]),
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(0),
            bytes_sent: AtomicU64::new(0),
        });
        let handler = Arc::new(handler);
        let st = stats.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let handler = handler.clone();
                let st = st.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        let st = st.clone();
                        async move { Ok::<_, Infallible>(respond(&*handler, &st, req)) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Server { addr, stats, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.stats.log.lock().unwrap().clone()
    }

    pub fn max_inflight(&self) -> usize {
        self.stats.max_inflight.load(Ordering::SeqCst)
    }
}

fn respond(
    handler: &(dyn Fn(&Req, usize) -> Reply + Send + Sync),
    st: &Arc<Stats>,
    req: Request<Incoming>,
) -> Response<TestBody> {
    let r = Req {
        path: req.uri().path_and_query().map_or("/".into(), |p| p.to_string()),
        headers: req.headers().clone(),
    };
    let idx = {
        let mut log = st.log.lock().unwrap();
        log.push(r.clone());
        log.len() - 1
    };
    let reply = handler(&r, idx);
    let n = st.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    st.max_inflight.fetch_max(n, Ordering::SeqCst);
    let guard = InflightGuard(st.clone());
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(2);
    let mut b = Response::builder().status(reply.status);
    for (k, v) in &reply.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let exact = if let Some(raw) = reply.raw {
        let len = raw.len() as u64;
        tokio::spawn(async move {
            let _g = guard;
            let _ = tx.send(Ok(Bytes::from(raw))).await;
        });
        Some(len)
    } else if let Some(spec) = reply.body {
        let len = reply.content_length.unwrap_or(spec.len);
        let chunked = reply.chunked;
        let st = st.clone();
        tokio::spawn(async move {
            let _g = guard;
            produce(spec, tx, st).await;
        });
        (!chunked).then_some(len)
    } else {
        drop(tx);
        drop(guard);
        Some(0)
    };
    b.body(TestBody { rx, exact }).unwrap()
}

async fn produce(spec: BodySpec, tx: mpsc::Sender<Result<Bytes, io::Error>>, st: Arc<Stats>) {
    if let Some(d) = spec.stall {
        tokio::time::sleep(d).await;
    }
    let chunk = spec.chunk.unwrap_or(16 * 1024) as u64;
    let mut sent = 0u64;
    while sent < spec.len {
        let mut n = chunk.min(spec.len - sent);
        if let Some(da) = spec.drop_after {
            if sent >= da {
                let _ = tx.send(Err(io::Error::other("injected connection drop"))).await;
                return;
            }
            n = n.min(da - sent);
        }
        let data = Bytes::from(pattern(spec.start + sent, n, spec.salt));
        if tx.send(Ok(data)).await.is_err() {
            return;
        }
        st.bytes_sent.fetch_add(n, Ordering::Relaxed);
        sent += n;
        // No sleep after the last frame: the in-flight gauge must drop when
        // the response is complete, not a pacing interval later.
        if let (Some(bps), true) = (spec.pace_bps, sent < spec.len) {
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / bps as f64)).await;
        }
    }
}

// ---------------------------------------------------------------- file server

/// A configurable "static file" endpoint with every misbehaviour knob.
pub struct FileSrv {
    pub len: u64,
    pub salt: AtomicU64,
    pub etag: Mutex<Option<String>>,
    pub last_modified: Mutex<Option<String>>,
    pub honor_range: AtomicBool,
    pub ignore_if_range: AtomicBool,
    pub bad_content_range: AtomicBool,
    pub content_disposition: Mutex<Option<String>>,
    pub pace_bps: AtomicU64,
    pub chunk: AtomicUsize,
    /// Pre-flight statuses for the next requests: (status, Retry-After).
    pub status_plan: Mutex<VecDeque<(u16, Option<String>)>>,
    /// Drop the connection after N bytes for each of the next responses.
    pub drop_plan: Mutex<VecDeque<u64>>,
    /// Stall before the first body byte of the next responses.
    pub stall_plan: Mutex<VecDeque<Duration>>,
    pub content_type: Mutex<Option<String>>,
}

impl FileSrv {
    pub fn new(len: u64) -> Arc<Self> {
        Arc::new(FileSrv {
            len,
            salt: AtomicU64::new(0),
            etag: Mutex::new(Some("\"v1\"".into())),
            last_modified: Mutex::new(Some("Wed, 21 Oct 2015 07:28:00 GMT".into())),
            honor_range: AtomicBool::new(true),
            ignore_if_range: AtomicBool::new(false),
            bad_content_range: AtomicBool::new(false),
            content_disposition: Mutex::new(None),
            pace_bps: AtomicU64::new(0),
            chunk: AtomicUsize::new(16 * 1024),
            status_plan: Mutex::new(VecDeque::new()),
            drop_plan: Mutex::new(VecDeque::new()),
            stall_plan: Mutex::new(VecDeque::new()),
            content_type: Mutex::new(None),
        })
    }

    pub fn set_etag(&self, e: Option<&str>) {
        *self.etag.lock().unwrap() = e.map(str::to_owned);
    }

    pub fn handle(&self, req: &Req) -> Reply {
        if let Some((status, ra)) = self.status_plan.lock().unwrap().pop_front() {
            let mut r = Reply::text(status, "nope");
            if let Some(ra) = ra {
                r = r.header("Retry-After", ra);
            }
            return r;
        }
        let etag = self.etag.lock().unwrap().clone();
        let lm = self.last_modified.lock().unwrap().clone();
        let salt = self.salt.load(Ordering::SeqCst);
        let pace = self.pace_bps.load(Ordering::SeqCst);
        let mut spec = BodySpec {
            start: 0,
            len: self.len,
            salt,
            pace_bps: (pace > 0).then_some(pace),
            chunk: Some(self.chunk.load(Ordering::SeqCst)),
            drop_after: self.drop_plan.lock().unwrap().pop_front(),
            stall: self.stall_plan.lock().unwrap().pop_front(),
        };
        let honor = self.honor_range.load(Ordering::SeqCst);
        let mut reply = Reply::status(200);
        if let Some(e) = &etag {
            reply = reply.header("ETag", e.clone());
        }
        if let Some(l) = &lm {
            reply = reply.header("Last-Modified", l.clone());
        }
        if honor {
            reply = reply.header("Accept-Ranges", "bytes");
        }
        if let Some(cd) = self.content_disposition.lock().unwrap().clone() {
            reply = reply.header("Content-Disposition", cd);
        }
        if let Some(ct) = self.content_type.lock().unwrap().clone() {
            reply = reply.header("Content-Type", ct);
        }
        let range = req.header("range").and_then(|r| parse_range(&r, self.len));
        if let (true, Some((a, b))) = (honor, range) {
            let if_range_ok = match req.header("if-range") {
                None => true,
                Some(v) => self.ignore_if_range.load(Ordering::SeqCst) || Some(&v) == etag.as_ref() || Some(&v) == lm.as_ref(),
            };
            if a >= self.len {
                return Reply::status(416).header("Content-Range", format!("bytes */{}", self.len));
            }
            if if_range_ok {
                spec.start = a;
                spec.len = b - a + 1;
                reply.status = 206;
                let shown_a = if self.bad_content_range.load(Ordering::SeqCst) { a + 1 } else { a };
                reply = reply.header("Content-Range", format!("bytes {shown_a}-{b}/{}", self.len));
                reply.body = Some(spec);
                return reply;
            }
        }
        reply.body = Some(spec);
        reply
    }
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` -> inclusive (a, b).
pub fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
    let r = v.strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return Some((len.saturating_sub(n), len - 1));
    }
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
    Some((a, b))
}

pub async fn serve_file(srv: &Arc<FileSrv>) -> Server {
    let s = srv.clone();
    Server::start(move |req, _| s.handle(req)).await
}

// ---------------------------------------------------------------- client helpers

pub fn test_cfg() -> DownloaderConfig {
    DownloaderConfig {
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(200),
        checkpoint_interval: Duration::from_millis(100),
        read_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        ..DownloaderConfig::default()
    }
}

pub struct Dirs {
    pub tmp: tempfile::TempDir,
    pub dest: PathBuf,
    pub inc: PathBuf,
}

pub fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    let inc = tmp.path().join(".incomplete");
    Dirs { tmp, dest, inc }
}

impl Dirs {
    pub fn req(&self, url: &str) -> DownloadRequest {
        DownloadRequest::new(url, &self.dest, &self.inc)
    }
    pub fn staged_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.inc)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
    pub fn dest_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.dest)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

/// Wait until `cond(progress)` holds, with a deadline.
pub async fn wait_until(task: &trs_http::DownloadTask, what: &str, cond: impl Fn(&trs_http::Progress) -> bool) -> trs_http::Progress {
    let mut rx = task.subscribe();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let p = rx.borrow_and_update().clone();
        if cond(&p) {
            return p;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}; last progress {p:?}");
        }
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
    }
}

/// A rate gate written independently of `TokenBucket`: strict serial pacing.
pub struct PacedGate {
    bps: f64,
    next: Mutex<Instant>,
    pub total: AtomicU64,
}

impl PacedGate {
    pub fn new(bps: u64) -> Arc<Self> {
        Arc::new(PacedGate { bps: bps as f64, next: Mutex::new(Instant::now()), total: AtomicU64::new(0) })
    }
}

impl RateGate for PacedGate {
    fn acquire(&self, bytes: usize) -> BoxFuture<'_, ()> {
        self.total.fetch_add(bytes as u64, Ordering::SeqCst);
        let wait = {
            let mut n = self.next.lock().unwrap();
            let now = Instant::now();
            let start = (*n).max(now);
            *n = start + Duration::from_secs_f64(bytes as f64 / self.bps);
            start.saturating_duration_since(now)
        };
        Box::pin(async move {
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        })
    }
}
