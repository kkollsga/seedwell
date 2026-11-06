//! A loopback SOCKS5 server with username and password auth, for tests: it relays CONNECTs to addresses and to the
//! names it was told ([`FakeProxy::map_name`]; any other name is "host unreachable", as a real proxy would answer a
//! name it cannot resolve), counts what it was asked, can answer late ([`FakeProxy::set_delay`]) and can go down
//! and come back on the same port ([`FakeProxy::stop`], [`FakeProxy::resume`]). Never reaches anything but what the
//! test hands it.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::{Credentials, Proxy};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Inner {
    user: String,
    pass: String,
    delay_ms: AtomicU64,
    accepted: AtomicUsize,
    auth_failures: AtomicUsize,
    connects: Mutex<Vec<String>>,
    names: Mutex<HashMap<String, IpAddr>>,
    sessions: Mutex<Vec<JoinHandle<()>>>,
}

/// The fake proxy. Dropping it stops it.
pub struct FakeProxy {
    addr: SocketAddr,
    inner: Arc<Inner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl FakeProxy {
    /// Listens on `127.0.0.1` on a free port, asking for `user` and `pass`.
    pub async fn start(user: &str, pass: &str) -> FakeProxy {
        Self::start_on("127.0.0.1:0".parse().expect("an address"), user, pass).await.expect("bind the fake proxy")
    }

    /// The same on `addr` (another loopback address on a port taken on the first, for a second server of one
    /// port). `None` when it cannot listen there (no IPv6 loopback).
    pub async fn start_on(addr: SocketAddr, user: &str, pass: &str) -> Option<FakeProxy> {
        let l = TcpListener::bind(addr).await.ok()?;
        let addr = l.local_addr().expect("its address");
        let inner = Arc::new(Inner {
            user: user.into(),
            pass: pass.into(),
            delay_ms: AtomicU64::new(0),
            accepted: AtomicUsize::new(0),
            auth_failures: AtomicUsize::new(0),
            connects: Mutex::new(Vec::new()),
            names: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(serve(l, inner.clone()));
        Some(FakeProxy { addr, inner, task: Mutex::new(Some(task)) })
    }

    /// Where it listens.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy with the right credentials.
    pub fn proxy(&self) -> Proxy {
        Proxy {
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            auth: Some(Credentials::new(&self.inner.user, &self.inner.pass).expect("test credentials")),
        }
    }

    /// Each answer of the handshake (greeting, login, CONNECT) waits this long first.
    pub fn set_delay(&self, d: Duration) {
        self.inner.delay_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// `name` resolves to `ip` at the proxy.
    pub fn map_name(&self, name: &str, ip: IpAddr) {
        lock(&self.inner.names).insert(name.to_ascii_lowercase(), ip);
    }

    /// The CONNECT targets asked for, in order (`ip:port` or `name:port`).
    pub fn connects(&self) -> Vec<String> {
        lock(&self.inner.connects).clone()
    }

    /// TCP connections the proxy accepted.
    pub fn accepted(&self) -> usize {
        self.inner.accepted.load(Ordering::Relaxed)
    }

    /// Logins turned down.
    pub fn auth_failures(&self) -> usize {
        self.inner.auth_failures.load(Ordering::Relaxed)
    }

    /// Goes down: the port is closed and every relayed connection is cut.
    pub async fn stop(&self) {
        let t = lock(&self.task).take();
        if let Some(t) = t {
            t.abort();
            let _ = t.await;
        }
        for s in lock(&self.inner.sessions).drain(..) {
            s.abort();
        }
    }

    /// Comes back on the same port.
    pub async fn resume(&self) {
        let mut tries = 0;
        let l = loop {
            match TcpListener::bind(self.addr).await {
                Ok(l) => break l,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("the fake proxy cannot listen on {} again: {e}", self.addr),
            }
        };
        *lock(&self.task) = Some(tokio::spawn(serve(l, self.inner.clone())));
    }
}

impl Drop for FakeProxy {
    fn drop(&mut self) {
        if let Some(t) = lock(&self.task).take() {
            t.abort();
        }
        for s in lock(&self.inner.sessions).drain(..) {
            s.abort();
        }
    }
}

async fn serve(l: TcpListener, inner: Arc<Inner>) {
    while let Ok((s, _)) = l.accept().await {
        inner.accepted.fetch_add(1, Ordering::Relaxed);
        let i = inner.clone();
        let h = tokio::spawn(async move {
            let _ = session(s, &i).await;
        });
        let mut v = lock(&inner.sessions);
        v.retain(|h| !h.is_finished());
        v.push(h);
    }
}

async fn pause(inner: &Inner) {
    let ms = inner.delay_ms.load(Ordering::Relaxed);
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

async fn session(mut s: TcpStream, inner: &Inner) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    s.read_exact(&mut methods).await?;
    pause(inner).await;
    if head[0] != 5 || !methods.contains(&2) {
        s.write_all(&[5, 0xFF]).await?;
        return Ok(());
    }
    s.write_all(&[5, 2]).await?;
    let mut v = [0u8; 2];
    s.read_exact(&mut v).await?;
    let mut user = vec![0u8; usize::from(v[1])];
    s.read_exact(&mut user).await?;
    let mut n = [0u8; 1];
    s.read_exact(&mut n).await?;
    let mut pass = vec![0u8; usize::from(n[0])];
    s.read_exact(&mut pass).await?;
    pause(inner).await;
    if user != inner.user.as_bytes() || pass != inner.pass.as_bytes() {
        inner.auth_failures.fetch_add(1, Ordering::Relaxed);
        s.write_all(&[1, 1]).await?;
        return Ok(());
    }
    s.write_all(&[1, 0]).await?;
    let mut req = [0u8; 4];
    if s.read_exact(&mut req).await.is_err() {
        return Ok(()); // a health check: it closes after the login
    }
    let (label, ip) = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            (None, Some(IpAddr::from(a)))
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            (None, Some(IpAddr::from(a)))
        }
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            let mut name = vec![0u8; usize::from(n[0])];
            s.read_exact(&mut name).await?;
            let name = String::from_utf8_lossy(&name).to_ascii_lowercase();
            let ip = lock(&inner.names).get(&name).copied();
            (Some(name), ip)
        }
        _ => return Ok(()),
    };
    let mut p = [0u8; 2];
    s.read_exact(&mut p).await?;
    let port = u16::from_be_bytes(p);
    let shown = match (&label, ip) {
        (Some(n), _) => format!("{n}:{port}"),
        (None, Some(ip)) => SocketAddr::new(ip, port).to_string(),
        (None, None) => String::new(),
    };
    lock(&inner.connects).push(shown);
    pause(inner).await;
    let reply = |code: u8| [5, code, 0, 1, 0, 0, 0, 0, 0, 0];
    let Some(ip) = ip else {
        s.write_all(&reply(4)).await?;
        return Ok(());
    };
    let mut out = match TcpStream::connect(SocketAddr::new(ip, port)).await {
        Ok(o) => o,
        Err(_) => {
            s.write_all(&reply(5)).await?;
            return Ok(());
        }
    };
    s.write_all(&reply(0)).await?;
    let _ = tokio::io::copy_bidirectional(&mut s, &mut out).await;
    Ok(())
}
