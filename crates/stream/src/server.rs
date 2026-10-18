//! The loopback server and the stream registry.

use crate::conn;
use crate::index;
use crate::mime::{self, Container};
use crate::source::{DynSource, FileRange, OwnerId, PieceSource, StreamId};
use crate::stream::{Stream, StreamCfg};
use crate::urgency::{self, PlanConfig};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, watch};

/// Callback receiving every [`Event`]; must not block.
pub type EventSink = Arc<dyn Fn(&Event) + Send + Sync>;

#[derive(Clone)]
pub struct Config {
    /// Address to bind. Must be loopback; the default is `127.0.0.1:0` (random port).
    pub bind: SocketAddr,
    /// A response waiting for a piece ends after this long without progress (SPEC: 60 s).
    pub stall_timeout: Duration,
    /// Largest single read/write (default 256 KiB). This is the only file data a response holds.
    pub chunk_size: usize,
    /// How long a keep-alive connection may sit idle between requests.
    pub idle_keepalive: Duration,
    /// How long a started request head may take to arrive.
    pub header_timeout: Duration,
    /// A completed request keeps counting as a playhead this long (coalesces small requests).
    pub linger: Duration,
    /// A cursor whose socket has not drained for this long stops claiming urgency.
    pub stale_after: Duration,
    /// Simultaneous connections; further ones are dropped.
    pub max_connections: usize,
    pub plan: PlanConfig,
    /// Observer for requests and chunks (tests, diagnostics, the app's debug log).
    pub on_event: Option<EventSink>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            stall_timeout: Duration::from_secs(60),
            chunk_size: 256 * 1024,
            idle_keepalive: Duration::from_secs(120),
            header_timeout: Duration::from_secs(15),
            linger: Duration::from_secs(3),
            stale_after: Duration::from_secs(8),
            max_connections: 64,
            plan: PlanConfig::default(),
            on_event: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// Everything was sent.
    Complete,
    /// The player closed the connection.
    ClientGone,
    /// The stream was closed (user, torrent removal, shutdown).
    Closed,
    /// No progress on the needed piece for the stall timeout.
    Stalled,
    /// The engine dropped the source (torrent removed).
    SourceGone,
    /// A read failed or returned short data.
    Error,
}

#[derive(Clone, Debug)]
pub enum Event {
    Request {
        req: u64,
        stream: StreamId,
        token: String,
        head_only: bool,
        status: u16,
        /// The raw `Range` header.
        range: Option<String>,
        /// Resolved file offsets `start..end_excl` of the body.
        start: u64,
        end_excl: u64,
        at: Instant,
    },
    /// A chunk of `len` bytes at file `offset` was written to the socket.
    Chunk { req: u64, offset: u64, len: usize, at: Instant },
    End { req: u64, reason: EndReason, sent: u64, at: Instant },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ServerStats {
    pub streams: usize,
    pub connections: usize,
    pub cursors: usize,
}

/// What `open` returns. The app needs `url`; the rest tells it which players can play the file.
#[derive(Clone, Debug)]
pub struct StreamHandle {
    pub url: String,
    pub token: String,
    pub id: StreamId,
    pub file_name: String,
    pub file_len: u64,
    pub content_type: &'static str,
    pub container: Container,
    /// MP4 / M4V / MOV / M4A: QuickTime Player (AVFoundation) can play it. False for MKV etc.
    pub is_mp4_family: bool,
}

pub(crate) struct Inner {
    pub cfg: Config,
    pub scfg: Arc<StreamCfg>,
    streams: Mutex<HashMap<String, Arc<Stream>>>,
    next: AtomicU64,
    pub cursors: Arc<AtomicUsize>,
    pub shutdown: watch::Sender<bool>,
    conns: Arc<Semaphore>,
    rt: Handle,
    addr: SocketAddr,
}

impl Inner {
    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
    pub fn lookup(&self, token: &str) -> Option<Arc<Stream>> {
        self.streams.lock().unwrap().get(token).cloned()
    }
    pub fn emit(&self, f: impl FnOnce() -> Event) {
        if let Some(cb) = &self.cfg.on_event {
            cb(&f());
        }
    }
}

pub struct StreamServer {
    inner: Arc<Inner>,
    accept: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn token() -> io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Only loopback peers may talk to the server.
pub fn peer_allowed(peer: &SocketAddr) -> bool {
    peer.ip().to_canonical().is_loopback()
}

impl StreamServer {
    /// Bind `cfg.bind` (loopback only) and start accepting.
    pub async fn start(cfg: Config) -> io::Result<Self> {
        if !cfg.bind.ip().is_loopback() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "stream server binds loopback only"));
        }
        let l = TcpListener::bind(cfg.bind).await?;
        Self::start_with_listener(cfg, l).await
    }

    /// Serve on an already-bound listener. The per-peer loopback check still applies; tests bind
    /// a non-loopback address through this to prove the refusal.
    pub async fn start_with_listener(cfg: Config, listener: TcpListener) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let (shutdown, _) = watch::channel(false);
        let scfg = Arc::new(StreamCfg {
            plan: cfg.plan.clone(),
            linger: cfg.linger,
            stale_after: cfg.stale_after,
            stall_timeout: cfg.stall_timeout,
            chunk_size: cfg.chunk_size.clamp(4096, 1 << 20),
        });
        let inner = Arc::new(Inner {
            conns: Arc::new(Semaphore::new(cfg.max_connections)),
            cfg,
            scfg,
            streams: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            cursors: Arc::new(AtomicUsize::new(0)),
            shutdown,
            rt: Handle::current(),
            addr,
        });
        let i2 = inner.clone();
        let accept = tokio::spawn(async move {
            let mut sd = i2.shutdown.subscribe();
            loop {
                let (sock, peer) = tokio::select! {
                    r = listener.accept() => match r {
                        Ok(x) => x,
                        Err(_) => { tokio::time::sleep(Duration::from_millis(50)).await; continue }
                    },
                    _ = sd.changed() => return,
                };
                if !peer_allowed(&peer) {
                    drop(sock);
                    continue;
                }
                let Ok(permit) = i2.conns.clone().try_acquire_owned() else {
                    drop(sock);
                    continue;
                };
                let i3 = i2.clone();
                tokio::spawn(async move {
                    conn::serve_conn(i3, sock, peer).await;
                    drop(permit);
                });
            }
        });
        Ok(Self { inner, accept: Mutex::new(Some(accept)) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// Start streaming `file` of the torrent behind `source`. Marks the head and tail urgent at
    /// once and starts the index probe. `owner` ties the stream to a torrent for [`close_owner`].
    ///
    /// [`close_owner`]: Self::close_owner
    pub fn open<S: PieceSource>(&self, source: Arc<S>, file: FileRange, owner: OwnerId) -> io::Result<StreamHandle> {
        let src: Arc<dyn DynSource> = source;
        let geom = src.geometry();
        if geom.piece_length == 0 || file.offset.checked_add(file.len).is_none_or(|e| e > geom.total_len) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "file outside the torrent"));
        }
        let tok = token()?;
        let id = self.inner.next_id();
        let name = file.name.clone();
        let stream = Stream::new(id, tok.clone(), owner, src, file, self.inner.scfg.clone(), self.inner.rt.clone());
        let handle = StreamHandle {
            url: format!("http://127.0.0.1:{}/{}/{}", self.inner.addr.port(), tok, mime::encode_path_segment(&name)),
            token: tok.clone(),
            id,
            file_len: stream.file.len,
            content_type: stream.content_type,
            container: mime::container_of(&name),
            is_mp4_family: mime::is_mp4_family(&name),
            file_name: name,
        };
        self.inner.streams.lock().unwrap().insert(tok, stream.clone());
        for r in urgency::head_tail(stream.file.len, &self.inner.cfg.plan) {
            stream.add_index(r);
        }
        self.inner.rt.spawn(index::run(stream));
        Ok(handle)
    }

    /// Close one stream. Its responses end, its hints are cleared, the token stops working.
    pub fn close(&self, token: &str) -> bool {
        let s = self.inner.streams.lock().unwrap().remove(token);
        match s {
            Some(s) => {
                s.close();
                true
            }
            None => false,
        }
    }

    /// Close every stream of a torrent (call when the torrent is removed). Returns how many.
    pub fn close_owner(&self, owner: OwnerId) -> usize {
        let gone: Vec<Arc<Stream>> = {
            let mut m = self.inner.streams.lock().unwrap();
            let toks: Vec<String> = m.iter().filter(|(_, s)| s.owner == owner).map(|(k, _)| k.clone()).collect();
            toks.iter().filter_map(|t| m.remove(t)).collect()
        };
        for s in &gone {
            s.close();
        }
        gone.len()
    }

    /// Does `owner` have a live stream?
    pub fn has_owner(&self, owner: OwnerId) -> bool {
        self.inner.streams.lock().unwrap().values().any(|s| s.owner == owner)
    }

    pub fn stats(&self) -> ServerStats {
        ServerStats {
            streams: self.inner.streams.lock().unwrap().len(),
            connections: self.inner.cfg.max_connections - self.inner.conns.available_permits(),
            cursors: self.inner.cursors.load(Ordering::Relaxed),
        }
    }

    /// Duration read from the container so far (diagnostics, tests).
    pub fn duration_of(&self, token: &str) -> Option<Duration> {
        self.inner.lookup(token)?.duration()
    }

    /// The urgency most recently pushed to the source for a stream (diagnostics, tests).
    pub fn last_urgency(&self, token: &str) -> Option<Vec<crate::source::UrgentRange>> {
        Some(self.inner.lookup(token)?.last_pushed())
    }

    /// Stop accepting, close every stream, and wait (bounded) for connections to finish.
    pub async fn shutdown(self) {
        self.stop().await;
    }

    /// [`shutdown`](Self::shutdown) for a server shared behind an `Arc` (the engine keeps it in its
    /// shared state). The server refuses nothing afterwards; it simply no longer accepts.
    pub async fn shutdown_shared(&self) {
        self.stop().await;
    }

    async fn stop(&self) {
        let all: Vec<Arc<Stream>> = self.inner.streams.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in &all {
            s.close();
        }
        self.inner.shutdown.send_replace(true);
        let h = self.accept.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.await;
        }
        let n = self.inner.cfg.max_connections as u32;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.conns.acquire_many(n)).await;
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        let all: Vec<Arc<Stream>> = self.inner.streams.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in &all {
            s.close();
        }
        self.inner.shutdown.send_replace(true);
    }
}
