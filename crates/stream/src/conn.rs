//! Connection handling: parse requests, route by token, answer, stream bodies piece by piece.

use crate::http::{self, Head, Method, ParseError, RangeSpec, Request};
use crate::server::{EndReason, Event, Inner};
use crate::source::DynSource;
use crate::stream::{CursorGuard, Stream};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until, timeout};

/// Watches the socket's read side while a response body is parked or being written: EOF or an
/// error means the player went away. Bytes that arrive meanwhile (pipelined requests) are kept.
pub(crate) struct Disconnect<'a> {
    rd: &'a OwnedReadHalf,
    buf: &'a mut Vec<u8>,
}

impl Disconnect<'_> {
    pub async fn gone(&mut self) {
        let mut tmp = [0u8; 2048];
        loop {
            if self.rd.readable().await.is_err() {
                return;
            }
            match self.rd.try_read(&mut tmp) {
                Ok(0) => return,
                Ok(n) => {
                    if self.buf.len() + n > http::MAX_HEADER_BYTES * 4 {
                        return; // a client flooding requests while we stream is treated as gone
                    }
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
    }
}

async fn gone_or_pending(g: &mut Option<&mut Disconnect<'_>>) {
    match g {
        Some(d) => d.gone().await,
        None => std::future::pending().await,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    Ready,
    Cancelled,
    Stalled,
    SourceGone,
    ClientGone,
}

/// Wait until `piece` is verified. Gives up after `stall` without growth of the piece's
/// partial progress.
pub(crate) async fn wait_piece(
    src: &dyn DynSource,
    rx: &mut watch::Receiver<u64>,
    piece: u32,
    stall: Duration,
    cancel: &mut watch::Receiver<bool>,
    mut gone: Option<&mut Disconnect<'_>>,
) -> Wait {
    let mut deadline = Instant::now() + stall;
    let mut progress = src.piece_progress(piece);
    loop {
        if src.has(piece) {
            return Wait::Ready;
        }
        if *cancel.borrow() {
            return Wait::Cancelled;
        }
        tokio::select! {
            r = rx.changed() => if r.is_err() { return if src.has(piece) { Wait::Ready } else { Wait::SourceGone } },
            _ = cancel.changed() => return Wait::Cancelled,
            () = sleep_until(deadline) => {
                let p = src.piece_progress(piece);
                if p == progress {
                    return Wait::Stalled;
                }
                progress = p;
                deadline = Instant::now() + stall;
            }
            () = gone_or_pending(&mut gone) => return Wait::ClientGone,
        }
    }
}

pub(crate) async fn serve_conn(inner: Arc<Inner>, sock: TcpStream, peer: SocketAddr) {
    let _ = sock.set_nodelay(true);
    let (mut rd, mut wr) = sock.into_split();
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut shutdown = inner.shutdown.subscribe();
    loop {
        let req = match read_request(&inner, &mut rd, &mut buf, &mut shutdown).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(status) => {
                let _ = write_simple(&mut wr, status, &[], false).await;
                return;
            }
        };
        let keep = match handle(&inner, &req, &mut rd, &mut buf, &mut wr, peer).await {
            Some(k) => k && req.wants_keep_alive(),
            None => false,
        };
        if !keep {
            let _ = wr.shutdown().await;
            return;
        }
    }
}

/// `Ok(None)`: clean close (EOF, idle timeout, shutdown). `Err(status)`: malformed.
async fn read_request(
    inner: &Inner,
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<Request>, u16> {
    loop {
        match http::parse_request(buf) {
            Ok(Some((req, n))) => {
                buf.drain(..n);
                if req.content_length > 0 {
                    return Err(400);
                }
                return Ok(Some(req));
            }
            Ok(None) => {}
            Err(ParseError::TooLarge) => return Err(431),
            Err(ParseError::Version) => return Err(505),
            Err(ParseError::Bad) => return Err(400),
        }
        let wait = if buf.is_empty() { inner.cfg.idle_keepalive } else { inner.cfg.header_timeout };
        if *shutdown.borrow() {
            return Ok(None);
        }
        let n = tokio::select! {
            r = timeout(wait, rd.read_buf(buf)) => match r {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => return Ok(None),
                Err(_) => return if buf.is_empty() { Ok(None) } else { Err(408) },
            },
            _ = shutdown.changed() => return Ok(None),
        };
        if n == 0 {
            return Ok(None);
        }
    }
}

async fn write_simple(wr: &mut OwnedWriteHalf, status: u16, extra: &[(&str, &str)], keep: bool) -> std::io::Result<()> {
    let mut h = Head::new(status).header("Content-Length", "0").header("Connection", if keep { "keep-alive" } else { "close" });
    for (k, v) in extra {
        h = h.header(k, v);
    }
    wr.write_all(&h.finish()).await
}

/// Answer one request. `Some(true)` = the connection can carry another request.
async fn handle(
    inner: &Arc<Inner>,
    req: &Request,
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    wr: &mut OwnedWriteHalf,
    _peer: SocketAddr,
) -> Option<bool> {
    let keep = req.wants_keep_alive();
    if let Some(h) = &req.host
        && !http::host_is_loopback(h)
    {
        write_simple(wr, 403, &[], false).await.ok()?;
        return None;
    }
    if req.method == Method::Other {
        write_simple(wr, 405, &[("Allow", "GET, HEAD")], keep).await.ok()?;
        return Some(true);
    }
    let token = req.path.strip_prefix('/').and_then(|p| p.split('/').next()).unwrap_or("");
    let Some(stream) = inner.lookup(token) else {
        write_simple(wr, 404, &[], keep).await.ok()?;
        return Some(true);
    };
    let len = stream.file.len;
    let range_hdr = match (&req.if_range, &req.range) {
        (Some(v), Some(r)) if *v == stream.etag => Some(r.as_str()),
        (Some(_), _) => None,
        (None, r) => r.as_deref(),
    };
    let spec = http::resolve_range(range_hdr, len);
    let req_id = inner.next_id();
    let (status, start, end_incl) = match spec {
        RangeSpec::Full => (200u16, 0u64, len.saturating_sub(1)),
        RangeSpec::Partial { start, end } => (206, start, end),
        RangeSpec::Unsatisfiable => (416, 0, 0),
    };
    let body_len = if status == 416 || len == 0 { 0 } else { end_incl - start + 1 };
    let mut head = Head::new(status)
        .header("Content-Type", stream.content_type)
        .header("Accept-Ranges", "bytes")
        .header("Content-Length", &body_len.to_string())
        .header("ETag", &stream.etag)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Connection", if keep { "keep-alive" } else { "close" });
    match status {
        206 => head = head.header("Content-Range", &format!("bytes {start}-{end_incl}/{len}")),
        416 => head = head.header("Content-Range", &format!("bytes */{len}")),
        _ => {}
    }
    inner.emit(|| Event::Request {
        req: req_id,
        stream: stream.id,
        token: stream.token.clone(),
        head_only: req.method == Method::Head,
        status,
        range: req.range.clone(),
        start,
        end_excl: start + body_len,
        at: std::time::Instant::now(),
    });
    if wr.write_all(&head.finish()).await.is_err() {
        return None;
    }
    if req.method == Method::Head || body_len == 0 {
        inner.emit(|| Event::End { req: req_id, reason: EndReason::Complete, sent: 0, at: std::time::Instant::now() });
        return Some(true);
    }
    let mut gone = Disconnect { rd, buf };
    let (reason, sent) = send_body(inner, &stream, req_id, start, start + body_len, wr, &mut gone).await;
    inner.emit(|| Event::End { req: req_id, reason, sent, at: std::time::Instant::now() });
    (reason == EndReason::Complete).then_some(true)
}

/// Stream file bytes `start..end_excl` (file offsets). Each chunk is sent only after every piece
/// it touches is verified.
async fn send_body(
    inner: &Arc<Inner>,
    stream: &Arc<Stream>,
    req_id: u64,
    start: u64,
    end_excl: u64,
    wr: &mut OwnedWriteHalf,
    gone: &mut Disconnect<'_>,
) -> (EndReason, u64) {
    let cid = inner.next_id();
    inner.cursors.fetch_add(1, Ordering::Relaxed);
    stream.cursor_begin(cid, start);
    let mut guard = CursorGuard { stream: stream.clone(), id: cid, completed: false, pos: start, live: inner.cursors.clone() };
    let src = stream.source.clone();
    let geom = stream.geom;
    let mut rx = src.subscribe();
    let mut cancel = stream.cancel.subscribe();
    let mut chunk: Vec<u8> = Vec::with_capacity(stream.cfg.chunk_size);
    let mut pos = start;
    let mut sent = 0u64;
    while pos < end_excl {
        stream.cursor_advance(cid, pos);
        let abs = stream.file.offset + pos;
        let piece = geom.piece_of(abs);
        match wait_piece(&*src, &mut rx, piece, stream.cfg.stall_timeout, &mut cancel, Some(gone)).await {
            Wait::Ready => {}
            Wait::Cancelled => return (EndReason::Closed, sent),
            Wait::Stalled => return (EndReason::Stalled, sent),
            Wait::SourceGone => return (EndReason::SourceGone, sent),
            Wait::ClientGone => return (EndReason::ClientGone, sent),
        }
        // Longest run from `abs` made only of verified pieces, at most one chunk.
        let mut n = (stream.cfg.chunk_size as u64).min(end_excl - pos);
        let last = geom.piece_of(abs + n - 1);
        for q in piece + 1..=last {
            if !src.has(q) {
                n = geom.piece_start(q) - abs;
                break;
            }
        }
        let n = n as usize;
        let buf = std::mem::take(&mut chunk);
        let read = tokio::select! {
            r = src.read(abs, n, buf) => r,
            _ = cancel.changed() => return (EndReason::Closed, sent),
            () = gone.gone() => return (EndReason::ClientGone, sent),
        };
        chunk = match read {
            Ok(b) if b.len() == n => b,
            _ => return (EndReason::Error, sent),
        };
        stream.cursor_blocked(cid, true);
        let w = tokio::select! {
            r = wr.write_all(&chunk) => r,
            _ = cancel.changed() => return (EndReason::Closed, sent),
        };
        if w.is_err() {
            return (EndReason::ClientGone, sent);
        }
        pos += n as u64;
        sent += n as u64;
        guard.pos = pos;
        inner.emit(|| Event::Chunk { req: req_id, offset: pos - n as u64, len: n, at: std::time::Instant::now() });
    }
    guard.completed = true;
    (EndReason::Complete, sent)
}
