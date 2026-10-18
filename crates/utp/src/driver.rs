//! tokio driver: one UDP socket, one task, one timer for every uTP connection.
//!
//! All connection state lives in a single [`Endpoint`] behind a `std::sync::Mutex` (never held
//! across an `.await`). A [`UtpStream`] operation locks, tries the operation, and on `WouldBlock`
//! parks its waker in a per-slot side table in the same critical section, so wake-ups cannot be
//! lost. The driver task drains the socket, runs due timers, flushes outgoing datagrams, and
//! wakes whichever wakers the endpoint reports ready. Idle connections own no task, no timer
//! and no buffers.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::config::Config;
use crate::conn::{EV_READ, EV_WRITE, End, Stats};
use crate::endpoint::{ConnHandle, ConnStatus, Disposition, Endpoint};

/// Added to elapsed time so endpoint timestamps are never 0 (0 means "unset" internally).
const CLOCK_OFFSET_US: u64 = 1_000_000;
/// Datagrams read per wake-up before flushing, bounding latency of acks and timers.
const RECV_BATCH: usize = 32;

/// A datagram that was not uTP (DHT, UDP tracker, ...), received on the shared socket.
#[derive(Debug)]
pub struct ForeignDatagram {
    /// Sender.
    pub from: SocketAddr,
    /// Payload.
    pub data: Box<[u8]>,
}

#[derive(Default)]
struct ConnWakers {
    read: Option<Waker>,
    write: Option<Waker>,
}

struct Inner {
    ep: Endpoint,
    wakers: Vec<ConnWakers>,
    accept_waker: Option<Waker>,
    foreign: Option<mpsc::Sender<ForeignDatagram>>,
    datagrams_in: u64,
    datagrams_out: u64,
    foreign_in: u64,
    foreign_dropped: u64,
    wakeups: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    socket: UdpSocket,
    kick: Notify,
    start: Instant,
    handles: AtomicUsize,
    local: SocketAddr,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_micros() as u64 + CLOCK_OFFSET_US
    }

    fn instant_of(&self, t: u64) -> Instant {
        self.start + Duration::from_micros(t.saturating_sub(CLOCK_OFFSET_US))
    }
}

/// Counted reference: the driver task exits once every user-facing handle is gone and the
/// endpoint has drained.
struct Ref(Arc<Shared>);

impl Ref {
    fn new(s: Arc<Shared>) -> Ref {
        s.handles.fetch_add(1, Ordering::Relaxed);
        Ref(s)
    }
}

impl Clone for Ref {
    fn clone(&self) -> Self {
        Ref::new(self.0.clone())
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        self.0.handles.fetch_sub(1, Ordering::Relaxed);
        self.0.kick.notify_one();
    }
}

impl Inner {
    fn wakers_for(&mut self, idx: usize) -> &mut ConnWakers {
        if self.wakers.len() <= idx {
            self.wakers.resize_with(idx + 1, ConnWakers::default);
        }
        // Resized above, so the index exists.
        &mut self.wakers[idx]
    }

    /// Move newly-ready wakers into `out` (woken after the lock is released).
    fn collect_wakers(&mut self, out: &mut Vec<Waker>) {
        while let Some((h, ev)) = self.ep.poll_event() {
            if let Some(w) = self.wakers.get_mut(h.index()) {
                if ev & EV_READ != 0 {
                    out.extend(w.read.take());
                }
                if ev & EV_WRITE != 0 {
                    out.extend(w.write.take());
                }
            }
        }
        if self.ep.has_pending_accept() {
            out.extend(self.accept_waker.take());
        }
        if self.ep.live_connections() == 0 && !self.wakers.is_empty() {
            self.wakers = Vec::new(); // release the side table with the last connection
        }
    }
}

/// Aggregate counters, mostly for tests and benchmarks.
#[derive(Clone, Copy, Debug, Default)]
pub struct SocketStats {
    /// Routable connections.
    pub live_connections: usize,
    /// Inbound connections awaiting address confirmation.
    pub half_open: usize,
    /// UDP datagrams received (uTP and foreign).
    pub datagrams_in: u64,
    /// UDP datagrams sent by the uTP engine.
    pub datagrams_out: u64,
    /// Non-uTP datagrams seen.
    pub foreign_in: u64,
    /// Non-uTP datagrams dropped because the application queue was full or absent.
    pub foreign_dropped: u64,
    /// Times the driver task woke up (idle cost: should stay near 1/s with idle connections).
    pub wakeups: u64,
}

/// A bound UDP socket that speaks uTP and passes everything else through. Cheap to clone.
#[derive(Clone)]
pub struct UtpSocket {
    r: Ref,
}

impl UtpSocket {
    /// Bind `addr` and start the driver task (must be called inside a tokio runtime).
    pub async fn bind(addr: SocketAddr, cfg: Config) -> io::Result<UtpSocket> {
        Self::from_udp(bound(addr).await?, cfg, None)
    }

    /// Like [`bind`](Self::bind), forwarding non-uTP datagrams (DHT, UDP tracker replies) to `foreign`.
    pub async fn bind_with_foreign(
        addr: SocketAddr,
        cfg: Config,
        foreign: mpsc::Sender<ForeignDatagram>,
    ) -> io::Result<UtpSocket> {
        Self::from_udp(bound(addr).await?, cfg, Some(foreign))
    }

    /// Wrap an already-bound socket (e.g. one created with specific options).
    pub fn from_udp(
        socket: UdpSocket,
        cfg: Config,
        foreign: Option<mpsc::Sender<ForeignDatagram>>,
    ) -> io::Result<UtpSocket> {
        let local = socket.local_addr()?;
        crate::trace::init_from_env();
        let seed = crate::rng::Rng::from_os().next_u64();
        let mut ep = Endpoint::new(cfg, seed);
        ep.set_same_host_check(is_own_address);
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                ep,
                wakers: Vec::new(),
                accept_waker: None,
                foreign,
                datagrams_in: 0,
                datagrams_out: 0,
                foreign_in: 0,
                foreign_dropped: 0,
                wakeups: 0,
            }),
            socket,
            kick: Notify::new(),
            start: Instant::now(),
            handles: AtomicUsize::new(0),
            local,
        });
        let r = Ref::new(shared.clone());
        tokio::spawn(run(shared));
        Ok(UtpSocket { r })
    }

    /// Address the socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.r.0.local
    }

    /// Install or replace the sink for non-uTP datagrams.
    pub fn set_foreign_sink(&self, tx: Option<mpsc::Sender<ForeignDatagram>>) {
        self.r.0.lock().foreign = tx;
    }

    /// Send a non-uTP datagram through the shared socket (DHT / UDP tracker replies).
    pub async fn send_foreign(&self, to: SocketAddr, data: &[u8]) -> io::Result<usize> {
        self.r.0.socket.send_to(data, to).await
    }

    /// Like [`send_foreign`](Self::send_foreign) but never waits: `WouldBlock` when the kernel
    /// buffer is full (the DHT treats that as packet loss).
    pub fn try_send_foreign(&self, to: SocketAddr, data: &[u8]) -> io::Result<usize> {
        self.r.0.socket.try_send_to(data, to)
    }

    /// Stop (or resume) accepting inbound connections.
    pub fn set_accepting(&self, on: bool) {
        self.r.0.lock().ep.set_accepting(on);
    }

    /// Counters.
    pub fn stats(&self) -> SocketStats {
        let g = self.r.0.lock();
        SocketStats {
            live_connections: g.ep.live_connections(),
            half_open: g.ep.half_open(),
            datagrams_in: g.datagrams_in,
            datagrams_out: g.datagrams_out,
            foreign_in: g.foreign_in,
            foreign_dropped: g.foreign_dropped,
            wakeups: g.wakeups,
        }
    }

    /// Wait for the next inbound connection.
    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)> {
        let sh = &self.r.0;
        let h = std::future::poll_fn(|cx| {
            let mut g = sh.lock();
            match g.ep.poll_accept() {
                Some(h) => Poll::Ready(h),
                None => {
                    g.accept_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;
        let peer = sh.lock().ep.peer_addr(h).ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionReset))?;
        Ok((UtpStream::new(self.r.clone(), h, peer), peer))
    }

    /// Open a connection to `remote`. Fails with `TimedOut` after the SYN retries are exhausted
    /// (about 15 s with defaults); wrap in `tokio::time::timeout` for a shorter bound.
    pub async fn connect(&self, remote: SocketAddr) -> io::Result<UtpStream> {
        let sh = &self.r.0;
        let h = sh.lock().ep.connect(sh.now(), remote)?;
        sh.kick.notify_one();
        // From here the stream's Drop cleans up if this future is cancelled.
        let stream = UtpStream::new(self.r.clone(), h, remote);
        std::future::poll_fn(|cx| {
            let mut g = sh.lock();
            match g.ep.status(h) {
                ConnStatus::Connecting => {
                    g.wakers_for(h.index()).write = Some(cx.waker().clone());
                    Poll::Pending
                }
                ConnStatus::Open | ConnStatus::Closed(End::Graceful) => Poll::Ready(Ok(())),
                ConnStatus::Closed(e) => Poll::Ready(Err(match e {
                    End::Refused => io::Error::from(io::ErrorKind::ConnectionRefused),
                    End::TimedOut => io::Error::from(io::ErrorKind::TimedOut),
                    _ => io::Error::from(io::ErrorKind::ConnectionReset),
                })),
            }
        })
        .await?;
        Ok(stream)
    }
}

/// A uTP connection usable like a `TcpStream`.
pub struct UtpStream {
    r: Ref,
    h: ConnHandle,
    peer: SocketAddr,
}

impl UtpStream {
    fn new(r: Ref, h: ConnHandle, peer: SocketAddr) -> Self {
        UtpStream { r, h, peer }
    }

    /// Remote address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Local UDP address (shared with every other stream on the socket).
    pub fn local_addr(&self) -> SocketAddr {
        self.r.0.local
    }

    /// Congestion-control snapshot (RTT, window, queuing delay, ...).
    pub fn stats(&self) -> Option<Stats> {
        self.r.0.lock().ep.conn_stats(self.h)
    }

    /// Heap bytes this connection's buffers hold right now (0 when idle).
    pub fn buffered_heap_bytes(&self) -> usize {
        self.r.0.lock().ep.conn_heap_bytes(self.h)
    }

    /// Poll for read readiness: data is buffered, or the peer closed or the connection failed (the
    /// next [`try_read`](Self::try_read) then reports it). Parks `cx`'s waker otherwise. For
    /// hand-written `poll_fn` loops, like `TcpStream::poll_read_ready`.
    pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let sh = &self.r.0;
        let mut g = sh.lock();
        match g.ep.read(self.h, sh.now(), &mut []) {
            Ok(_) => Poll::Ready(Ok(())),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                g.wakers_for(self.h.index()).read = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Wait until a read would not block. Like `TcpStream::readable`, for callers that want to
    /// lease a buffer only while bytes flow.
    pub async fn readable(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_read_ready(cx)).await
    }

    /// Read without waiting: `Ok(0)` is EOF, `WouldBlock` means call [`readable`](Self::readable).
    pub fn try_read(&self, out: &mut [u8]) -> io::Result<usize> {
        let sh = &self.r.0;
        let mut g = sh.lock();
        let n = g.ep.read(self.h, sh.now(), out)?;
        if g.ep.has_pending_transmit() {
            sh.kick.notify_one();
        }
        Ok(n)
    }

    /// Poll for write readiness: the send buffer has room (or the connection failed).
    pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let sh = &self.r.0;
        let mut g = sh.lock();
        if g.ep.write_ready(self.h) {
            Poll::Ready(Ok(()))
        } else {
            g.wakers_for(self.h.index()).write = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// Wait until the send buffer has room (or the connection failed).
    pub async fn writable(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_write_ready(cx)).await
    }

    /// Queue bytes without waiting; returns how many were taken, `WouldBlock` when the buffer is full.
    pub fn try_write(&self, data: &[u8]) -> io::Result<usize> {
        let sh = &self.r.0;
        let mut g = sh.lock();
        let n = g.ep.write(self.h, sh.now(), data)?;
        if g.ep.has_pending_transmit() {
            sh.kick.notify_one();
        }
        Ok(n)
    }

    /// Abort with RESET instead of a graceful close.
    pub fn reset(&self) {
        let sh = &self.r.0;
        sh.lock().ep.reset(self.h, sh.now());
        sh.kick.notify_one();
    }
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        let sh = &self.r.0;
        {
            let mut g = sh.lock();
            let now = sh.now();
            g.ep.close(self.h, now);
            if let Some(w) = g.wakers.get_mut(self.h.index()) {
                w.read = None;
                w.write = None;
            }
        }
        sh.kick.notify_one();
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let sh = &me.r.0;
        let mut g = sh.lock();
        // Only zero-initialise what could actually be filled: `ReadBuf` offers no safe way to
        // hand out uninitialised memory, and a 64 KiB memset per small read is measurable.
        let want = buf.remaining().min(g.ep.readable_len(me.h).max(1));
        let out = buf.initialize_unfilled_to(want);
        if out.is_empty() {
            return Poll::Ready(Ok(()));
        }
        match g.ep.read(me.h, sh.now(), out) {
            Ok(n) => {
                buf.advance(n);
                if g.ep.has_pending_transmit() {
                    sh.kick.notify_one();
                }
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                g.wakers_for(me.h.index()).read = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        let sh = &me.r.0;
        let mut g = sh.lock();
        match g.ep.write(me.h, sh.now(), data) {
            Ok(n) => {
                if g.ep.has_pending_transmit() {
                    sh.kick.notify_one();
                }
                Poll::Ready(Ok(n))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                g.wakers_for(me.h.index()).write = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Data is handed to the transport as fast as the congestion window allows; there is no
    /// application-level flush barrier (matches `TcpStream`).
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// Queues a FIN behind the buffered data; reads keep working (half-close).
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let sh = &me.r.0;
        sh.lock().ep.shutdown_write(me.h, sh.now());
        sh.kick.notify_one();
        Poll::Ready(Ok(()))
    }
}

/// Datagrams taken from the endpoint per pass before they are sent, and read from the socket per
/// pass before they are handed to the endpoint. Syscalls never run under the endpoint's lock: with
/// hundreds of connections every `read`/`write` of a stream takes that lock, and a pass of 64
/// `sendto` calls inside it (about 100 us each) starved them all.
const TX_BATCH: usize = 64;

/// Datagrams waiting to be sent: bytes in one buffer, `(to, start, len, dont_fragment)` per datagram.
struct TxBatch {
    /// `TX_BATCH` slots of one maximal datagram each; the endpoint writes straight into a slot.
    data: Vec<u8>,
    items: Vec<(SocketAddr, usize, usize, bool)>,
    /// Index of the first datagram not sent yet.
    next: usize,
}

impl TxBatch {
    fn is_done(&self) -> bool {
        self.next >= self.items.len()
    }

    fn clear(&mut self) {
        self.items.clear();
        self.next = 0;
    }
}

/// Whether `ip` is one of this host's own addresses (the kernel routes traffic to it over the
/// loopback interface): a UDP socket "connected" to it picks that same address as its source.
/// A connect on UDP sends nothing.
fn is_own_address(ip: std::net::IpAddr) -> bool {
    let any = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let Ok(s) = std::net::UdpSocket::bind(any) else { return false };
    s.connect((ip, 9)).is_ok() && s.local_addr().is_ok_and(|a| a.ip() == ip)
}

/// The single task serving every connection on the socket.
async fn run(sh: Arc<Shared>) {
    let slot = sh.lock().ep.transmit_buffer_len().clamp(2048, 65_536);
    let mut rbuf = vec![0u8; RECV_BATCH * slot].into_boxed_slice();
    let tlen = sh.lock().ep.transmit_buffer_len();
    let mut tx = TxBatch { data: vec![0u8; TX_BATCH * tlen], items: Vec::with_capacity(TX_BATCH), next: 0 };
    let mut sent_count = 0u64;
    let mut wake: Vec<Waker> = Vec::new();
    let mut next_snap = std::time::Instant::now();
    let mut write_blocked_last = false;
    loop {
        // 1. Timers, then take what the endpoint wants to send (under the lock, without I/O).
        let mut yielded = false;
        let deadline;
        {
            let mut g = sh.lock();
            g.wakeups += 1;
            g.datagrams_out += std::mem::take(&mut sent_count);
            let now = sh.now();
            g.ep.handle_timeout(now);
            if crate::trace::on() && std::time::Instant::now() >= next_snap {
                next_snap = std::time::Instant::now() + Duration::from_millis(500);
                g.ep.trace_dump(now);
                eprintln!("utp {} drv pending_tx={} blocked={}", crate::trace::wall_ms(), tx.items.len() - tx.next, write_blocked_last);
            }
            if tx.is_done() {
                tx.clear();
                while tx.items.len() < TX_BATCH {
                    let start = tx.items.len() * tlen;
                    let Some(slot) = tx.data.get_mut(start..start + tlen) else { break };
                    let Some((to, n)) = g.ep.poll_transmit(now, slot) else { break };
                    tx.items.push((to, start, n, g.ep.dont_fragment()));
                }
                yielded = tx.items.len() >= TX_BATCH;
            }
            g.collect_wakers(&mut wake);
            deadline = g.ep.poll_timeout();
            if sh.handles.load(Ordering::Relaxed) == 0
                && g.ep.live_connections() == 0
                && tx.is_done()
                && !g.ep.has_pending_transmit()
            {
                return;
            }
        }
        for w in wake.drain(..) {
            w.wake();
        }
        // 2. Send, outside the lock.
        let mut write_blocked = false;
        let mut df_on = false;
        while let Some(&(to, start, n, df)) = tx.items.get(tx.next) {
            if df != df_on {
                crate::sockopt::set_dont_fragment(&sh.socket, df);
                df_on = df;
            }
            match sh.socket.try_send_to(&tx.data[start..start + n], to) {
                Ok(_) => {
                    sent_count += 1;
                    tx.next += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    write_blocked = true;
                    break;
                }
                // Unreachable host, no buffer space, ...: UDP semantics, treat as loss and let
                // retransmission deal with it.
                Err(_) => tx.next += 1,
            }
        }
        if df_on {
            crate::sockopt::set_dont_fragment(&sh.socket, false);
        }
        write_blocked_last = write_blocked;
        if yielded && !write_blocked {
            tokio::task::yield_now().await;
            continue;
        }

        // 3. Sleep until something happens.
        let snap_at = crate::trace::on().then_some(next_snap);
        let sleep = async {
            let at = deadline.map(|t| sh.instant_of(t));
            let at = match (at, snap_at) {
                (Some(a), Some(b)) => Some(a.min(tokio::time::Instant::from_std(b))),
                (None, Some(b)) => Some(tokio::time::Instant::from_std(b)),
                (a, None) => a,
            };
            match at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            r = sh.socket.readable() => {
                if r.is_ok() {
                    recv_batch(&sh, &mut rbuf, slot);
                }
            }
            _ = sh.socket.writable(), if write_blocked => {}
            _ = sh.kick.notified() => {}
            _ = sleep => {}
        }
    }
}

/// Read up to `RECV_BATCH` datagrams (no lock held) and feed them to the endpoint (one lock).
fn recv_batch(sh: &Shared, rbuf: &mut [u8], slot: usize) {
    let mut got: [(SocketAddr, usize); RECV_BATCH] = [(sh.local, 0); RECV_BATCH];
    let mut count = 0;
    for (i, buf) in rbuf.chunks_mut(slot).take(RECV_BATCH).enumerate() {
        match sh.socket.try_recv_from(buf) {
            Ok((n, from)) => {
                // A datagram that fills the slot may have been cut short; uTP has no checksum, so
                // a truncated payload would be taken for the real thing. Drop it instead.
                let n = if n >= slot { 0 } else { n };
                got[i] = (from, n);
                count = i + 1;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            // e.g. ICMP-induced errors; nothing to do, but do not lose the slot
            Err(_) => {
                got[i] = (sh.local, 0);
                count = i + 1;
            }
        }
    }
    let mut g = sh.lock();
    for (i, (from, n)) in got.iter().take(count).enumerate() {
        if *n == 0 {
            continue;
        }
        g.datagrams_in += 1;
        let Some(data) = rbuf.get(i * slot..i * slot + n) else { continue };
        if g.ep.handle_datagram(sh.now(), *from, data) == Disposition::Foreign {
            g.foreign_in += 1;
            let delivered = match &g.foreign {
                Some(tx) => tx.try_send(ForeignDatagram { from: *from, data: data.into() }).is_ok(),
                None => false,
            };
            if !delivered {
                g.foreign_dropped += 1;
            }
        }
    }
}

/// Bind a UDP socket and wait for tokio's first writable event. Until it arrives, `try_send_to`
/// (used by [`UtpSocket::try_send_foreign`], i.e. the DHT sharing this socket) fails with
/// `WouldBlock` although the socket can send; the first DHT datagrams, bootstrap included, were
/// silently lost that way (seen on Linux/epoll, where the event is delivered asynchronously).
async fn bound(addr: SocketAddr) -> io::Result<UdpSocket> {
    let s = UdpSocket::bind(addr).await?;
    s.writable().await?;
    Ok(s)
}
