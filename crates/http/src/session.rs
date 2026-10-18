//! One run of a download: find or create the staged `.part`, talk to the
//! server, write bytes with positional I/O, checkpoint, verify, move.
//!
//! Safety rules enforced here (SPEC 7D "never stitches together corrupt data"):
//! * a continuation is only requested with a strong validator (`If-Range`);
//! * a `206` is only accepted if `Content-Range` starts exactly where asked,
//!   ends inside what was asked, and agrees with `Content-Length` and the
//!   known total; otherwise **no byte of it is written**;
//! * a `200` to a range request, or any validator/size change, means restart
//!   from zero (the staged files are deleted, the user is told);
//! * sidecar ranges only ever cover bytes that were `fsync`ed first.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::Response;
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap,
    LAST_MODIFIED, RETRY_AFTER,
};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::core::Core;
use crate::error::Error;
use crate::gate::RateGate;
use crate::name::{derive_file_name, sanitize_file_name, unique_name};
use crate::net::{Opened, Piece};
use crate::ranges::RangeSet;
use crate::sidecar::Sidecar;
use crate::types::{Event, RestartReason, State};
use crate::util::{backoff, parse_retry_after};

/// Per-connection write buffer (SPEC: bounded memory, no full-file buffering).
pub(crate) const BUFFER_BYTES: usize = 256 * 1024;
/// Granularity of rate-gate requests.
const GATE_CHUNK: usize = 64 * 1024;
/// First request of a segmented download: also the probe for range support.
const PROBE_BYTES: u64 = 1024 * 1024;
const MIN_SEGMENT: u64 = 1024 * 1024;
const MAX_SEGMENT: u64 = 16 * 1024 * 1024;
/// A server that keeps changing the file is not worth more restarts.
const MAX_RESTARTS: u32 = 3;
/// Buffered bytes older than this are written out even if the buffer is not full.
const FLUSH_AFTER: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(250);
const SIDECAR_SUFFIX: &str = ".part.json";

pub(crate) enum SessionEnd {
    Completed(PathBuf),
    Stopped,
    Failed(Error),
}

/// How a unit of work ended early.
#[derive(Debug)]
pub(crate) enum Abort {
    Stopped,
    Restart(RestartReason),
    Fatal(Error),
    Retry { err: Error, after: Option<Duration> },
}

impl From<Error> for Abort {
    fn from(e: Error) -> Self {
        if e.is_retryable() {
            Abort::Retry { err: e, after: None }
        } else {
            Abort::Fatal(e)
        }
    }
}

/// Cooperative stop signal (pause / cancel / sibling failure).
#[derive(Clone)]
pub(crate) struct Stop(pub watch::Receiver<bool>);

impl Stop {
    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Run `f` unless stop fires first. Only used at await points where
    /// dropping the future loses nothing (network, gate, sleeps) - never in the
    /// middle of a disk write.
    pub async fn guard<T>(&self, f: impl Future<Output = T>) -> Result<T, Abort> {
        let mut rx = self.0.clone();
        tokio::select! {
            biased;
            _ = async { let _ = rx.wait_for(|v| *v).await; } => Err(Abort::Stopped),
            v = f => Ok(v),
        }
    }
}

// ---------------------------------------------------------------------------
// response validation
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Known {
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Accepted {
    start: u64,
    end: Option<u64>,
    total: Option<u64>,
    partial: bool,
}

fn header_str(h: &HeaderMap, name: hyper::header::HeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn content_length(h: &HeaderMap) -> Option<u64> {
    header_str(h, CONTENT_LENGTH)?.trim().parse().ok()
}

/// `bytes a-b/total` or `bytes a-b/*`.
pub(crate) fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.trim().split_once('-')?;
    let (a, b) = (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?);
    if b < a {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    Some((a, b, total))
}

/// Strong ETags only: `W/"x"` promises semantic, not byte, equality.
fn strong_etag(h: &HeaderMap) -> Option<String> {
    let v = header_str(h, ETAG)?.trim();
    (v.starts_with('"') && v.ends_with('"') && v.len() >= 2).then(|| v.to_owned())
}

fn validators_match(h: &HeaderMap, known: &Known) -> bool {
    if let Some(ke) = &known.etag {
        // A strong ETag is authoritative; Last-Modified is only the fallback
        // (some dynamic servers stamp it with "now").
        return strong_etag(h).is_none_or(|e| &e == ke);
    }
    if let Some(kl) = &known.last_modified {
        return header_str(h, LAST_MODIFIED).is_none_or(|l| l.trim() == kl);
    }
    true
}

fn check_response(
    resp: &Response<Incoming>,
    asked: Option<Piece>,
    known: &Known,
    fresh: bool,
    max_retry_after: Duration,
) -> Result<Accepted, Abort> {
    let status = resp.status().as_u16();
    let h = resp.headers();
    let mismatch = |m: String| Abort::Fatal(Error::RangeMismatch(m));
    match status {
        206 => {
            let want_start = asked.map_or(0, |p| p.start);
            let want_end = asked.and_then(|p| p.end);
            if header_str(h, CONTENT_TYPE).is_some_and(|t| t.to_ascii_lowercase().starts_with("multipart/")) {
                return Err(mismatch("multipart/byteranges answer".into()));
            }
            let (a, b, total) = header_str(h, CONTENT_RANGE)
                .and_then(parse_content_range)
                .ok_or_else(|| mismatch("missing or malformed Content-Range".into()))?;
            if a != want_start {
                return Err(mismatch(format!("asked for offset {want_start}, got {a}")));
            }
            if let Some(we) = want_end
                && b >= we
            {
                return Err(mismatch(format!("asked up to {}, got up to {b}", we - 1)));
            }
            if let Some(t) = total
                && b >= t
            {
                return Err(mismatch(format!("range end {b} beyond total {t}")));
            }
            if let Some(cl) = content_length(h)
                && cl != b - a + 1
            {
                return Err(mismatch(format!("Content-Length {cl} != range size {}", b - a + 1)));
            }
            if let (Some(kt), Some(t)) = (known.total, total)
                && kt != t
            {
                return Err(Abort::Restart(RestartReason::SizeChanged));
            }
            if !validators_match(h, known) {
                return Err(Abort::Restart(RestartReason::ValidatorChanged));
            }
            Ok(Accepted { start: a, end: Some(b + 1), total: total.or(known.total), partial: true })
        }
        200 => {
            let ranged = asked.is_some_and(|p| p.start > 0 || p.end.is_some());
            if ranged && !fresh {
                return Err(Abort::Restart(if validators_match(h, known) {
                    RestartReason::RangeIgnored
                } else {
                    RestartReason::ValidatorChanged
                }));
            }
            let cl = content_length(h);
            Ok(Accepted { start: 0, end: cl, total: cl, partial: false })
        }
        416 if !fresh => Err(Abort::Restart(RestartReason::SizeChanged)),
        408 | 429 | 500..=599 => {
            let after = header_str(h, RETRY_AFTER)
                .and_then(parse_retry_after)
                .map(|d| d.min(max_retry_after));
            Err(Abort::Retry { err: Error::Http { status }, after })
        }
        400..=499 => Err(Abort::Fatal(Error::Http { status })),
        s => Err(Abort::Fatal(Error::BadResponse(format!("unexpected status {s}")))),
    }
}

// ---------------------------------------------------------------------------
// retry policy
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Retrier {
    failures: u32,
}

impl Retrier {
    fn reset(&mut self) {
        self.failures = 0;
    }

    /// Count a failure and sit out the backoff (or `Retry-After`).
    async fn fail(
        &mut self,
        core: &Core,
        stop: &Stop,
        err: Error,
        after: Option<Duration>,
    ) -> Result<(), Abort> {
        let cfg = &core.inner.cfg;
        self.failures += 1;
        if self.failures > cfg.max_retries {
            return Err(Abort::Fatal(Error::RetriesExhausted {
                attempts: self.failures,
                last: Box::new(err),
            }));
        }
        let own = backoff(cfg.backoff_base, cfg.backoff_max, self.failures);
        let delay = after.map_or(own, |a| a.max(own));
        core.hub.update(|p| {
            p.retries += 1;
            p.error = Some(err.clone());
        });
        core.hub.event(Event::Retrying { attempt: self.failures, delay, error: err });
        core.hub.set_state(State::Retrying);
        stop.guard(tokio::time::sleep(delay)).await?;
        core.hub.set_state(State::Downloading);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// staged download on disk
// ---------------------------------------------------------------------------

fn sidecar_path_for(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_owned();
    s.push(".json");
    PathBuf::from(s)
}

fn part_path_for(sidecar: &Path) -> Option<PathBuf> {
    let s = sidecar.to_str()?;
    s.strip_suffix(".json").map(PathBuf::from)
}

/// Every sidecar in `dir` (bounded scan).
pub(crate) fn scan_sidecars(dir: &Path) -> Vec<(Sidecar, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in rd.flatten().take(100_000) {
        let path = entry.path();
        let is_sidecar = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(SIDECAR_SUFFIX));
        if !is_sidecar {
            continue;
        }
        if let Some(sc) = Sidecar::read(&path) {
            out.push((sc, path));
        }
    }
    out
}

/// The staged state for `id`, if any: (sidecar, sidecar path).
fn find_staged(dir: &Path, id: &str) -> Option<(Sidecar, PathBuf)> {
    scan_sidecars(dir).into_iter().find(|(sc, _)| sc.id == id)
}

/// Remove the staged files of `id`.
pub(crate) fn remove_staged(dir: &Path, id: &str) {
    if let Some((_, sc_path)) = find_staged(dir, id) {
        if let Some(part) = part_path_for(&sc_path) {
            let _ = fs::remove_file(part);
        }
        let _ = fs::remove_file(&sc_path);
        let mut tmp = sc_path.into_os_string();
        tmp.push(".tmp");
        let _ = fs::remove_file(tmp);
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Io(format!("blocking task failed: {e}")))
}

/// Everything the workers share once the staged `.part` exists.
pub(crate) struct Active {
    core: Arc<Core>,
    stop: Stop,
    file: Arc<File>,
    sidecar_path: PathBuf,
    part_path: PathBuf,
    side: Mutex<Sidecar>,
    etag: Option<String>,
    last_modified: Option<String>,
    received: AtomicU64,
    dirty: AtomicBool,
    conns: AtomicU32,
    base_url: Mutex<String>,
    base_reset: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl Active {
    fn new(core: Arc<Core>, stop: Stop, file: File, part_path: PathBuf, side: Sidecar, base_url: String) -> Arc<Self> {
        let received = side.ranges.total();
        Arc::new(Active {
            core,
            stop,
            file: Arc::new(file),
            sidecar_path: sidecar_path_for(&part_path),
            part_path,
            etag: side.etag.clone(),
            last_modified: side.last_modified.clone(),
            side: Mutex::new(side),
            received: AtomicU64::new(received),
            dirty: AtomicBool::new(true),
            conns: AtomicU32::new(0),
            base_url: Mutex::new(base_url),
            base_reset: AtomicBool::new(false),
        })
    }

    fn known(&self) -> Known {
        Known {
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            total: lock(&self.side).total,
        }
    }

    /// `If-Range` value: strong ETag preferred, else Last-Modified.
    fn if_range(&self) -> Option<&str> {
        self.etag.as_deref().or(self.last_modified.as_deref())
    }

    fn record(&self, start: u64, end: u64) {
        lock(&self.side).ranges.insert(start, end);
        self.dirty.store(true, Ordering::Release);
    }

    fn learn_total(&self, total: u64) {
        let mut s = lock(&self.side);
        if s.total.is_none() {
            s.total = Some(total);
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Fall back from a (possibly expired, signed) redirect target to the
    /// original URL, once.
    fn reset_base(&self) -> bool {
        let orig = &self.core.req.url;
        let mut b = lock(&self.base_url);
        if *b == *orig || self.base_reset.swap(true, Ordering::AcqRel) {
            return false;
        }
        b.clone_from(orig);
        true
    }

    /// Make the recorded ranges durable: snapshot ranges, fsync data, then
    /// atomically replace the sidecar. (Snapshot first: bytes written after the
    /// snapshot are not claimed.)
    async fn persist(&self) -> Result<(), Error> {
        let snapshot = lock(&self.side).clone();
        self.dirty.store(false, Ordering::Release);
        let file = self.file.clone();
        let path = self.sidecar_path.clone();
        let res = blocking(move || -> io::Result<()> {
            file.sync_data()?;
            snapshot.write_atomic(&path)
        })
        .await?;
        if let Err(e) = res {
            self.dirty.store(true, Ordering::Release);
            return Err(e.into());
        }
        Ok(())
    }
}

/// Per-connection buffered positional writer.
struct Writer {
    act: Arc<Active>,
    buf: Vec<u8>,
    buf_off: u64,
    next_off: u64,
    last_flush: Instant,
}

impl Writer {
    fn new(act: Arc<Active>) -> Self {
        Writer { act, buf: Vec::with_capacity(BUFFER_BYTES), buf_off: 0, next_off: 0, last_flush: Instant::now() }
    }

    /// Start writing at `off` (buffer must be flushed).
    fn begin(&mut self, off: u64) {
        debug_assert!(self.buf.is_empty());
        self.buf_off = off;
        self.next_off = off;
        self.last_flush = Instant::now();
    }

    async fn push(&mut self, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            if self.buf.is_empty() {
                self.buf_off = self.next_off;
            }
            let room = BUFFER_BYTES - self.buf.len();
            let n = room.min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            self.next_off += n as u64;
            data = &data[n..];
            if self.buf.len() >= BUFFER_BYTES {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn maybe_flush(&mut self) -> Result<(), Error> {
        if !self.buf.is_empty() && self.last_flush.elapsed() >= FLUSH_AFTER {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.buf);
        let off = self.buf_off;
        let len = buf.len() as u64;
        let file = self.act.file.clone();
        let (mut buf, res) = blocking(move || {
            let r = file.write_all_at(&buf, off);
            (buf, r)
        })
        .await?;
        res?;
        self.act.record(off, off + len);
        buf.clear();
        self.buf = buf;
        self.buf_off = self.next_off;
        self.last_flush = Instant::now();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// run loop
// ---------------------------------------------------------------------------

pub(crate) async fn run(core: Arc<Core>, stop: Stop) -> SessionEnd {
    let mut restarts = 0u32;
    loop {
        match run_once(&core, &stop).await {
            Ok(path) => return SessionEnd::Completed(path),
            Err(Abort::Stopped) => return SessionEnd::Stopped,
            Err(Abort::Restart(reason)) => {
                restarts += 1;
                core.hub.event(Event::Restarted(reason));
                core.hub.update(|p| p.restarts += 1);
                if restarts > MAX_RESTARTS {
                    return SessionEnd::Failed(Error::BadResponse(
                        "the server keeps changing the file; giving up".into(),
                    ));
                }
                discard(&core, false).await;
                core.hub.update(|p| {
                    p.done = 0;
                    p.total = None;
                });
            }
            Err(Abort::Fatal(e)) => return SessionEnd::Failed(e),
            Err(Abort::Retry { err, .. }) => return SessionEnd::Failed(err),
        }
    }
}

/// Delete (or, with `keep`, leave) the staged files of this task.
pub(crate) async fn discard(core: &Core, keep: bool) {
    if keep {
        return;
    }
    let dir = core.req.incomplete_dir.clone();
    let id = core.req.id.clone();
    let _ = blocking(move || remove_staged(&dir, &id)).await;
}

fn worker_count(core: &Core) -> u32 {
    let cap = core.inner.cfg.max_connections_per_host.max(1) as u32;
    core.req.connections.clamp(1, cap)
}

async fn run_once(core: &Arc<Core>, stop: &Stop) -> Result<PathBuf, Abort> {
    let conns = worker_count(core);
    let (ltx, lrx) = watch::channel(false);
    let local = Stop(lrx);
    let dir = core.req.incomplete_dir.clone();
    let id = core.req.id.clone();
    let staged = blocking(move || find_staged(&dir, &id)).await?;
    if let Some((side, sc_path)) = staged {
        let part = part_path_for(&sc_path);
        let usable = part.as_ref().is_some_and(|p| p.is_file()) && side.url == core.req.url;
        if let (true, Some(part)) = (usable, part) {
            if side.ranges.is_empty() {
                discard(core, false).await;
            } else {
                return resume(core, stop, local, ltx, side, part, conns).await;
            }
        } else {
            discard(core, false).await;
        }
    }
    fresh(core, stop, local, ltx, conns).await
}

fn chunk_pieces(holes: &[(u64, u64)], total: u64, workers: u32) -> VecDeque<Piece> {
    let chunk = if workers > 1 {
        // Bounded piece count even for absurd (hostile) sizes: at most ~4096 pieces.
        (total / (u64::from(workers) * 4))
            .clamp(MIN_SEGMENT, MAX_SEGMENT)
            .max(total / 4096)
    } else {
        u64::MAX
    };
    let mut out = VecDeque::new();
    for &(mut a, b) in holes {
        while a < b {
            let e = a.saturating_add(chunk).min(b);
            out.push_back(Piece { start: a, end: Some(e) });
            a = e;
        }
    }
    out
}

async fn resume(
    core: &Arc<Core>,
    stop: &Stop,
    local: Stop,
    ltx: watch::Sender<bool>,
    side: Sidecar,
    part: PathBuf,
    conns: u32,
) -> Result<PathBuf, Abort> {
    if !side.has_validator() {
        return Err(Abort::Restart(RestartReason::NoValidator));
    }
    let file = blocking({
        let part = part.clone();
        move || OpenOptions::new().read(true).write(true).open(part)
    })
    .await?
    .map_err(Error::from)?;
    let len = file.metadata().map_err(Error::from)?.len();
    let consistent = match side.total {
        Some(t) => len == t,
        None => side.ranges.contiguous_prefix() == side.ranges.total() && len >= side.ranges.total(),
    };
    if !consistent {
        return Err(Abort::Restart(RestartReason::PartMismatch));
    }
    let workers = if side.segmentable { conns } else { 1 };
    let pieces = match side.total {
        Some(t) => chunk_pieces(&side.ranges.missing(t), t, workers),
        None => VecDeque::from([Piece { start: side.ranges.total(), end: None }]),
    };
    core.hub.update(|p| {
        p.total = side.total;
        p.done = side.ranges.total();
        p.file_name = Some(side.name.clone());
        p.final_url = Some(side.final_url.clone());
        p.content_type = side.content_type.clone();
    });
    let act = Active::new(core.clone(), local, file, part, side, core.req.url.clone());
    execute(act, stop, ltx, pieces, None, workers).await
}

async fn fresh(
    core: &Arc<Core>,
    stop: &Stop,
    local: Stop,
    ltx: watch::Sender<bool>,
    conns: u32,
) -> Result<PathBuf, Abort> {
    let cfg = &core.inner.cfg;
    core.hub.set_state(State::Connecting);
    let first_asked = (conns > 1).then_some(Piece { start: 0, end: Some(PROBE_BYTES) });
    let mut retrier = Retrier::default();
    let (opened, acc) = loop {
        let r = async {
            let o = stop.guard(core.inner.net.open(&core.req.url, first_asked, None)).await??;
            let acc = check_response(&o.resp, first_asked, &Known::default(), true, cfg.max_retry_after)?;
            Ok::<_, Abort>((o, acc))
        }
        .await;
        match r {
            Ok(x) => break x,
            Err(Abort::Retry { err, after }) => retrier.fail(core, stop, err, after).await?,
            Err(e) => return Err(e),
        }
    };
    let h = opened.resp.headers();
    let final_url = opened.url.to_string();
    let content_type = header_str(h, CONTENT_TYPE).map(|s| s.trim().to_owned());
    let name = match &core.req.file_name {
        Some(n) => sanitize_file_name(n),
        None => derive_file_name(
            h.get(CONTENT_DISPOSITION).map(|v| v.as_bytes()),
            &core.req.url,
            &final_url,
        ),
    };
    let etag = strong_etag(h);
    let last_modified = header_str(h, LAST_MODIFIED).map(|s| s.trim().to_owned());
    let accept_bytes = header_str(h, ACCEPT_RANGES).is_some_and(|v| v.to_ascii_lowercase().contains("bytes"));
    let has_validator = etag.is_some() || last_modified.is_some();
    let segmentable = (acc.partial || accept_bytes) && acc.total.is_some() && has_validator;
    let side = Sidecar {
        id: core.req.id.clone(),
        url: core.req.url.clone(),
        final_url: final_url.clone(),
        name,
        etag,
        last_modified,
        total: acc.total,
        segmentable,
        content_type: content_type.clone(),
        ranges: RangeSet::new(),
    };
    let (file, part_path, side) = stage(core, side).await?;
    core.hub.update(|p| {
        p.total = side.total;
        p.file_name = Some(side.name.clone());
        p.final_url = Some(final_url.clone());
        p.content_type = content_type.clone();
        p.error = None;
    });
    core.hub.event(Event::Resolved {
        file_name: side.name.clone(),
        total: side.total,
        content_type,
        final_url: final_url.clone(),
    });
    // Continue from the resolved URL, falling back to the original if it fails.
    let base = if opened.hops > 0 { final_url } else { core.req.url.clone() };
    let workers = if segmentable { conns } else { 1 };
    let first_piece = Piece { start: acc.start, end: acc.end };
    let mut pieces = VecDeque::new();
    if let (Some(t), Some(e)) = (acc.total, acc.end)
        && e < t
    {
        pieces = chunk_pieces(&[(e, t)], t, workers);
    }
    let act = Active::new(core.clone(), local, file, part_path, side, base);
    execute(act, stop, ltx, pieces, Some((first_piece, opened, acc)), workers).await
}

/// Claim `<name>.part` (exclusively) and write the first sidecar.
async fn stage(core: &Arc<Core>, mut side: Sidecar) -> Result<(File, PathBuf, Sidecar), Error> {
    let dest = core.req.dest_dir.clone();
    let inc = core.req.incomplete_dir.clone();
    let total = side.total;
    blocking(move || {
        fs::create_dir_all(&inc)?;
        let mut attempts = 0;
        loop {
            let name = unique_name(&side.name, |n| {
                dest.join(n).exists() || inc.join(format!("{n}.part")).exists() || inc.join(format!("{n}{SIDECAR_SUFFIX}")).exists()
            });
            let part = inc.join(format!("{name}.part"));
            match OpenOptions::new().read(true).write(true).create_new(true).open(&part) {
                Ok(f) => {
                    if let Some(t) = total {
                        f.set_len(t)?; // sparse: blocks are allocated as data arrives
                    }
                    side.name = name;
                    side.write_atomic(&sidecar_path_for(&part))?;
                    return Ok((f, part, side));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 100 => attempts += 1,
                Err(e) => return Err(e),
            }
        }
    })
    .await?
    .map_err(Error::from)
}

struct Sched {
    q: Mutex<VecDeque<Piece>>,
}

impl Sched {
    fn pop(&self) -> Option<Piece> {
        lock(&self.q).pop_front()
    }
}

fn merge_abort(slot: &mut Option<Abort>, new: Abort) {
    let replace = match (&*slot, &new) {
        (None, _) => true,
        (Some(Abort::Stopped), n) => !matches!(n, Abort::Stopped),
        _ => false,
    };
    if replace {
        *slot = Some(new);
    }
}

type First = (Piece, Opened, Accepted);

async fn execute(
    act: Arc<Active>,
    stop: &Stop,
    ltx: watch::Sender<bool>,
    pieces: VecDeque<Piece>,
    mut first: Option<First>,
    workers: u32,
) -> Result<PathBuf, Abort> {
    let core = act.core.clone();
    core.hub.set_state(State::Downloading);
    let sched = Arc::new(Sched { q: Mutex::new(pieces) });
    let mut set: JoinSet<Result<(), Abort>> = JoinSet::new();
    let needed = lock(&sched.q).len() as u32 + u32::from(first.is_some());
    for i in 0..workers.min(needed.max(1)) {
        let f = if i == 0 { first.take() } else { None };
        set.spawn(worker(act.clone(), sched.clone(), f));
    }
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sampler = RateSampler::default();
    let mut last_persist = Instant::now();
    let mut outcome: Option<Abort> = None;
    let mut ext = stop.0.clone();
    let mut ext_seen = false;
    let checkpoint = core.inner.cfg.checkpoint_interval;
    loop {
        tokio::select! {
            joined = set.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(a))) => { merge_abort(&mut outcome, a); let _ = ltx.send(true); }
                Some(Err(e)) => {
                    merge_abort(&mut outcome, Abort::Fatal(Error::Io(format!("worker failed: {e}"))));
                    let _ = ltx.send(true);
                }
            },
            _ = ticker.tick() => {
                let done = act.received.load(Ordering::Relaxed);
                let rate = sampler.sample(done);
                let conns = act.conns.load(Ordering::Relaxed);
                core.hub.update(|p| { p.done = done; p.rate = rate; p.connections = conns; });
                if act.dirty.load(Ordering::Acquire) && last_persist.elapsed() >= checkpoint {
                    last_persist = Instant::now();
                    if let Err(e) = act.persist().await {
                        merge_abort(&mut outcome, Abort::Fatal(e));
                        let _ = ltx.send(true);
                    }
                }
            },
            _ = async { let _ = ext.wait_for(|v| *v).await; }, if !ext_seen => {
                ext_seen = true;
                let _ = ltx.send(true);
            },
        }
    }
    let done = act.received.load(Ordering::Relaxed);
    core.hub.update(|p| { p.done = done; p.rate = 0; p.connections = 0; });
    // Final checkpoint, also on stop: this is what makes pause lossless.
    if let Err(e) = act.persist().await {
        merge_abort(&mut outcome, Abort::Fatal(e));
    }
    if let Some(a) = outcome {
        return Err(a);
    }
    finalize(&act, stop).await
}

#[derive(Default)]
struct RateSampler {
    samples: VecDeque<(Instant, u64)>,
}

impl RateSampler {
    fn sample(&mut self, done: u64) -> u64 {
        let now = Instant::now();
        self.samples.push_back((now, done));
        while self.samples.len() > 9 {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let dt = now.duration_since(t0).as_secs_f64();
        if dt < 0.1 {
            return 0;
        }
        (done.saturating_sub(b0) as f64 / dt) as u64
    }
}

struct ConnGuard<'a>(&'a AtomicU32);
impl<'a> ConnGuard<'a> {
    fn new(c: &'a AtomicU32) -> Self {
        c.fetch_add(1, Ordering::Relaxed);
        ConnGuard(c)
    }
}
impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn worker(act: Arc<Active>, sched: Arc<Sched>, mut first: Option<First>) -> Result<(), Abort> {
    let mut w = Writer::new(act.clone());
    let mut retrier = Retrier::default();
    loop {
        let (mut piece, mut pre) = match first.take() {
            Some((p, o, a)) => (p, Some((o, a))),
            None => match sched.pop() {
                Some(p) => (p, None),
                None => return Ok(()),
            },
        };
        loop {
            if matches!(piece.end, Some(e) if piece.start >= e) && pre.is_none() {
                break;
            }
            let before = piece.start;
            w.begin(piece.start);
            let r = attempt(&act, &piece, &mut w, pre.take()).await;
            let flushed = w.flush().await;
            piece.start = w.next_off;
            let progressed = piece.start > before;
            if progressed {
                retrier.reset();
            }
            flushed.map_err(Abort::Fatal)?;
            match r {
                Ok(true) => break,
                Ok(false) if progressed => {}
                Ok(false) => {
                    let err = Error::Network("server returned no data".into());
                    retrier.fail(&act.core, &act.stop, err, None).await?;
                }
                Err(Abort::Retry { err, after }) => retrier.fail(&act.core, &act.stop, err, after).await?,
                Err(e) => return Err(e),
            }
        }
    }
}

/// One request/response for (the rest of) `piece`. `Ok(true)`: the piece is
/// complete. `Ok(false)`: the server legitimately answered less than asked.
async fn attempt(
    act: &Arc<Active>,
    piece: &Piece,
    w: &mut Writer,
    pre: Option<(Opened, Accepted)>,
) -> Result<bool, Abort> {
    let core = &act.core;
    let cfg = &core.inner.cfg;
    let (opened, acc) = match pre {
        Some(x) => x,
        None => {
            if piece.start > 0 && act.if_range().is_none() {
                // Cannot prove the file is unchanged: do not stitch.
                return Err(Abort::Restart(RestartReason::NoValidator));
            }
            let asked = (!(piece.start == 0 && piece.end.is_none())).then_some(*piece);
            let url = lock(&act.base_url).clone();
            let r = async {
                let o = act
                    .stop
                    .guard(core.inner.net.open(&url, asked, act.if_range().filter(|_| asked.is_some())))
                    .await??;
                let acc = check_response(&o.resp, asked, &act.known(), false, cfg.max_retry_after)?;
                Ok::<_, Abort>((o, acc))
            }
            .await;
            match r {
                Ok(x) => x,
                Err(Abort::Fatal(Error::Http { status })) if matches!(status, 401 | 403 | 404 | 410) && act.reset_base() => {
                    return Err(Abort::Retry { err: Error::Http { status }, after: None });
                }
                Err(e) => return Err(e),
            }
        }
    };
    if let Some(t) = acc.total {
        act.learn_total(t);
    }
    let Opened { resp, _permit, .. } = opened;
    let _conn = ConnGuard::new(&act.conns);
    let mut body = resp.into_body();
    let limit = match (piece.end, acc.end) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    debug_assert_eq!(acc.start, piece.start);
    loop {
        let frame = act.stop.guard(tokio::time::timeout(cfg.read_timeout, body.frame())).await?;
        let frame = match frame {
            Err(_) => return Err(Abort::Retry { err: Error::Timeout, after: None }),
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(Abort::Retry { err: Error::Network(e.to_string()), after: None });
            }
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else { continue };
        if let Some(l) = limit
            && w.next_off + data.len() as u64 > l
        {
            return Err(Abort::Fatal(Error::BadResponse("server sent more bytes than requested".into())));
        }
        for sub in data.chunks(GATE_CHUNK) {
            act.stop
                .guard(async {
                    core.limiter.acquire(sub.len()).await;
                    core.inner.gate.acquire(sub.len()).await;
                })
                .await?;
            w.push(sub).await.map_err(Abort::Fatal)?;
            act.received.fetch_add(sub.len() as u64, Ordering::Relaxed);
        }
        w.maybe_flush().await.map_err(Abort::Fatal)?;
    }
    let pos = w.next_off;
    if let Some(x) = acc.end
        && pos < x
    {
        return Err(Abort::Retry { err: Error::Network("connection closed before the body was complete".into()), after: None });
    }
    Ok(match piece.end {
        Some(e) => pos >= e,
        None => true,
    })
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

async fn finalize(act: &Arc<Active>, stop: &Stop) -> Result<PathBuf, Abort> {
    let core = &act.core;
    let (total, name) = {
        let mut s = lock(&act.side);
        let t = s.total.unwrap_or_else(|| s.ranges.total());
        if !s.ranges.covers(0, t) {
            return Err(Abort::Fatal(Error::Io("download incomplete: bytes missing".into())));
        }
        s.total = Some(t);
        (t, s.name.clone())
    };
    {
        let file = act.file.clone();
        blocking(move || -> io::Result<()> {
            if file.metadata()?.len() != total {
                file.set_len(total)?;
            }
            file.sync_all()
        })
        .await?
        .map_err(Error::from)?;
    }
    core.hub.update(|p| p.total = Some(total));
    if let Some(expected) = core.req.sha256 {
        core.hub.set_state(State::Verifying);
        let actual = hash_file(act, stop, total).await?;
        if actual != expected {
            return Err(Abort::Fatal(Error::ChecksumMismatch { expected: hex(&expected), actual: hex(&actual) }));
        }
    }
    let part = act.part_path.clone();
    let sidecar = act.sidecar_path.clone();
    let dest_dir = core.req.dest_dir.clone();
    let path = blocking(move || move_into_place(&part, &dest_dir, &name, &sidecar)).await?.map_err(Error::from)?;
    core.hub.update(|p| {
        p.final_path = Some(path.clone());
        p.file_name = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
        p.error = None;
    });
    Ok(path)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn hash_file(act: &Arc<Active>, stop: &Stop, total: u64) -> Result<[u8; 32], Abort> {
    const BLOCK: usize = 1024 * 1024;
    let mut hasher = Sha256::new();
    let mut off = 0u64;
    while off < total {
        if stop.is_set() {
            return Err(Abort::Stopped);
        }
        let file = act.file.clone();
        let want = BLOCK.min((total - off) as usize);
        let (h, res) = blocking(move || {
            let mut buf = vec![0u8; want];
            let r = file.read_exact_at(&mut buf, off);
            if r.is_ok() {
                hasher.update(&buf);
            }
            (hasher, r)
        })
        .await?;
        res.map_err(Error::from)?;
        hasher = h;
        off += want as u64;
    }
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(out.as_slice());
    Ok(arr)
}

/// Move `part` into `dest_dir/name` without ever overwriting: a hard link
/// fails atomically if the name is taken (then ` (2)`, ` (3)`, ...); on
/// filesystems without hard links fall back to exists-check + rename, and
/// across volumes to copy + fsync + rename.
fn move_into_place(part: &Path, dest_dir: &Path, name: &str, sidecar: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dest_dir)?;
    let mut tried = 0u32;
    let final_path = loop {
        let cand = unique_name(name, |n| dest_dir.join(n).exists());
        let target = dest_dir.join(&cand);
        match fs::hard_link(part, &target) {
            Ok(()) => {
                fs::remove_file(part)?;
                break target;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tried < 100 => tried += 1,
            Err(_) => {
                // No hard links here (FAT/exFAT, SMB) or a different volume.
                if target.exists() {
                    tried += 1;
                    if tried >= 100 {
                        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"));
                    }
                    continue;
                }
                match fs::rename(part, &target) {
                    Ok(()) => break target,
                    Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                        let tmp = dest_dir.join(format!(".{cand}.trs-tmp"));
                        fs::copy(part, &tmp)?;
                        File::open(&tmp)?.sync_all()?;
                        fs::rename(&tmp, &target)?;
                        fs::remove_file(part)?;
                        break target;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    };
    if let Ok(d) = File::open(dest_dir) {
        let _ = d.sync_all();
    }
    let _ = fs::remove_file(sidecar);
    Ok(final_path)
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

pub(crate) async fn probe(
    inner: &crate::core::Inner,
    url: &str,
) -> Result<crate::types::ProbeInfo, Error> {
    let asked = Some(Piece { start: 0, end: Some(1) });
    let o = inner.net.open(url, asked, None).await?;
    let status = o.resp.status().as_u16();
    let h = o.resp.headers();
    let (total, ranges) = match status {
        206 => {
            let t = header_str(h, CONTENT_RANGE).and_then(parse_content_range).and_then(|c| c.2);
            (t, true)
        }
        200 => (
            content_length(h),
            header_str(h, ACCEPT_RANGES).is_some_and(|v| v.to_ascii_lowercase().contains("bytes")),
        ),
        416 => (None, false),
        s => return Err(Error::Http { status: s }),
    };
    let final_url = o.url.to_string();
    Ok(crate::types::ProbeInfo {
        file_name: derive_file_name(h.get(CONTENT_DISPOSITION).map(|v| v.as_bytes()), url, &final_url),
        content_type: header_str(h, CONTENT_TYPE).map(|s| s.trim().to_owned()),
        total,
        accept_ranges: ranges,
        etag: strong_etag(h),
        last_modified: header_str(h, LAST_MODIFIED).map(|s| s.trim().to_owned()),
        final_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 99, Some(1000))));
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5, None)));
        assert_eq!(parse_content_range(" bytes  10-20 / 30 "), Some((10, 20, Some(30))));
        assert_eq!(parse_content_range("bytes */1000"), None);
        assert_eq!(parse_content_range("bytes 9-1/20"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 0-99"), None);
        assert_eq!(parse_content_range("bytes a-b/c"), None);
        assert_eq!(parse_content_range("bytes 0-99999999999999999999999/1"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn chunking_covers_holes_exactly() {
        let p = chunk_pieces(&[(0, 10 * MIN_SEGMENT + 5), (20 * MIN_SEGMENT, 21 * MIN_SEGMENT)], 100 * MIN_SEGMENT, 4);
        let mut covered = RangeSet::new();
        for piece in &p {
            covered.insert(piece.start, piece.end.unwrap_or(0));
        }
        assert_eq!(covered.total(), 10 * MIN_SEGMENT + 5 + MIN_SEGMENT);
        let single = chunk_pieces(&[(3, 1000)], 1000, 1);
        assert_eq!(single, VecDeque::from([Piece { start: 3, end: Some(1000) }]));
    }
}
