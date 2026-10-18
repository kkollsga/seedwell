//! A tiny HTTP/1.1 client for the tests (raw sockets, so malformed and half-read cases are easy).

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn h(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

pub struct Client {
    pub sock: TcpStream,
    pub buf: Vec<u8>,
}

pub fn req_bytes(method: &str, path: &str, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.into_bytes()
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).await.unwrap();
        Self { sock, buf: Vec::new() }
    }

    pub async fn send(&mut self, method: &str, path: &str, headers: &[(&str, &str)]) {
        self.sock.write_all(&req_bytes(method, path, headers)).await.unwrap();
    }

    /// Read a response head; the body is left in the socket.
    pub async fn read_head(&mut self) -> Option<Resp> {
        loop {
            if let Some(p) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8(self.buf[..p].to_vec()).unwrap();
                self.buf.drain(..p + 4);
                let mut lines = head.split("\r\n");
                let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
                let headers = lines
                    .map(|l| {
                        let (k, v) = l.split_once(':').unwrap();
                        (k.trim().to_ascii_lowercase(), v.trim().to_string())
                    })
                    .collect();
                return Some(Resp { status, headers, body: Vec::new() });
            }
            let mut tmp = [0u8; 4096];
            let n = self.sock.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Read exactly `n` body bytes (or fewer if the server closes). Returns what arrived.
    pub async fn read_body(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n.min(1 << 24));
        let take = self.buf.len().min(n);
        out.extend(self.buf.drain(..take));
        let mut tmp = vec![0u8; 256 * 1024];
        while out.len() < n {
            let want = (n - out.len()).min(tmp.len());
            match self.sock.read(&mut tmp[..want]).await {
                Ok(0) | Err(_) => break,
                Ok(k) => out.extend_from_slice(&tmp[..k]),
            }
        }
        out
    }

    /// Read a whole response (GET: `Content-Length` body; HEAD/416: none when `head_only`).
    pub async fn read_response(&mut self, head_only: bool) -> Option<Resp> {
        let mut r = self.read_head().await?;
        let len: usize = r.h("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if !head_only {
            r.body = self.read_body(len).await;
        }
        Some(r)
    }

    /// True if the server closed the connection (EOF within `wait`).
    pub async fn closed_within(&mut self, wait: Duration) -> bool {
        let mut tmp = [0u8; 64];
        matches!(tokio::time::timeout(wait, self.sock.read(&mut tmp)).await, Ok(Ok(0)) | Ok(Err(_)))
    }
}

pub async fn get(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Resp {
    let mut c = Client::connect(addr).await;
    c.send("GET", path, headers).await;
    c.read_response(false).await.expect("response")
}

pub async fn head(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Resp {
    let mut c = Client::connect(addr).await;
    c.send("HEAD", path, headers).await;
    c.read_response(true).await.expect("response")
}

/// Path part of a stream URL.
pub fn path_of(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap();
    rest[rest.find('/').unwrap()..].to_string()
}

pub fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Poll `f` until true or `secs` pass.
pub async fn eventually(secs: f64, mut f: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs_f64(secs);
    while std::time::Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    f()
}
