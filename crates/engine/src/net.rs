//! Peer transports: TCP and uTP behind one stream type, and the set of peers known to speak uTP.
//!
//! The peer task is written against [`PeerIo`], so it does not care which transport carries a
//! connection. Both transports offer the same readiness-style API (`poll_read_ready` / `try_read_buf` /
//! `poll_write_ready` / `try_write`), which is what lets an idle peer lease its read buffer only while
//! bytes flow (ENGINE.md 4).

use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use trs_utp::UtpStream;
use trs_wire::mse::Rc4;

/// A peer connection over either transport.
// size: the larger of the two handles (about 48 bytes)
pub(crate) enum PeerIo {
    Tcp(TcpStream),
    Utp(UtpStream),
    /// Either of them with the RC4 stream cipher of an encrypted connection (MSE) on top.
    Rc4(Box<Rc4Io>),
}

/// A transport whose bytes go through RC4 in both directions.
///
/// Reads are decrypted in place. A write is encrypted at once into `out` (the key stream cannot be
/// rewound) and `out` is flushed as the socket allows; `try_write` only takes new data when `out`
/// is empty, so at most one chunk of encrypted bytes is ever held back.
pub(crate) struct Rc4Io {
    inner: PeerIo,
    dec: Rc4,
    enc: Rc4,
    out: Vec<u8>,
    sent: usize,
}

impl Rc4Io {
    pub(crate) fn new(inner: PeerIo, dec: Rc4, enc: Rc4) -> Self {
        Rc4Io { inner, dec, enc, out: Vec::new(), sent: 0 }
    }

    /// Push held-back encrypted bytes to the socket. `Ok(true)`: all gone.
    fn flush_out(&mut self) -> io::Result<bool> {
        while self.sent < self.out.len() {
            match self.inner.try_write(&self.out[self.sent..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        self.out.clear();
        self.sent = 0;
        if self.out.capacity() > 64 * 1024 {
            self.out = Vec::new();
        }
        Ok(true)
    }
}

/// How an outgoing connection is attempted (decided by the torrent, executed by the peer task).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dial {
    /// TCP only (uTP off, or no socket).
    Tcp,
    /// uTP only (TCP outgoing disabled).
    Utp,
    /// TCP first; a peer that refuses or ignores TCP is tried over uTP.
    TcpThenUtp,
    /// A peer known to speak uTP: uTP first, TCP when that does not connect quickly.
    UtpThenTcp,
}

impl PeerIo {
    pub(crate) fn is_utp(&self) -> bool {
        match self {
            PeerIo::Utp(_) => true,
            PeerIo::Rc4(r) => r.inner.is_utp(),
            PeerIo::Tcp(_) => false,
        }
    }

    /// The connection is encrypted with RC4.
    #[cfg(test)]
    pub(crate) fn is_encrypted(&self) -> bool {
        matches!(self, PeerIo::Rc4(_))
    }

    pub(crate) fn set_nodelay(&self) {
        match self {
            PeerIo::Tcp(s) => {
                let _ = s.set_nodelay(true);
            }
            PeerIo::Rc4(r) => r.inner.set_nodelay(),
            PeerIo::Utp(_) => {}
        }
    }

    /// Encrypted bytes are waiting for the socket (the write buffer above may be empty).
    pub(crate) fn has_pending_write(&self) -> bool {
        matches!(self, PeerIo::Rc4(r) if r.sent < r.out.len())
    }

    /// Poll for read readiness (data, EOF or an error is pending). Stateless: the future of a peer
    /// task does not carry a readiness future per direction.
    pub(crate) fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            PeerIo::Tcp(s) => s.poll_read_ready(cx),
            PeerIo::Utp(s) => s.poll_read_ready(cx),
            PeerIo::Rc4(r) => r.inner.poll_read_ready(cx),
        }
    }

    /// Poll for write readiness.
    pub(crate) fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self {
            PeerIo::Tcp(s) => s.poll_write_ready(cx),
            PeerIo::Utp(s) => s.poll_write_ready(cx),
            // Held-back bytes need the socket; with nothing held back a write can be taken.
            PeerIo::Rc4(r) if r.sent < r.out.len() => r.inner.poll_write_ready(cx),
            PeerIo::Rc4(_) => Poll::Ready(Ok(())),
        }
    }

    /// Read into the spare capacity of `buf` without waiting. `Ok(0)` is EOF, `WouldBlock` means
    /// call [`readable`](Self::readable).
    pub(crate) fn try_read_buf(&mut self, buf: &mut BytesMut) -> io::Result<usize> {
        match self {
            PeerIo::Tcp(s) => s.try_read_buf(buf),
            PeerIo::Utp(s) => {
                // uTP reads into initialised memory; the lease is at most 32 KiB, so the extra
                // memset is small next to the datagram handling.
                let start = buf.len();
                let room = (buf.capacity() - start).clamp(1, 64 * 1024);
                buf.resize(start + room, 0);
                match s.try_read(&mut buf[start..]) {
                    Ok(n) => {
                        buf.truncate(start + n);
                        Ok(n)
                    }
                    Err(e) => {
                        buf.truncate(start);
                        Err(e)
                    }
                }
            }
            PeerIo::Rc4(r) => {
                let start = buf.len();
                let n = r.inner.try_read_buf(buf)?;
                r.dec.apply(&mut buf[start..start + n]);
                Ok(n)
            }
        }
    }

    /// Write as much of `data` as fits without waiting.
    pub(crate) fn try_write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            PeerIo::Tcp(s) => s.try_write(data),
            PeerIo::Utp(s) => s.try_write(data),
            PeerIo::Rc4(r) => {
                if !r.flush_out()? {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                if data.is_empty() {
                    return Ok(0);
                }
                let n = data.len().min(64 * 1024);
                r.out.extend_from_slice(&data[..n]);
                let start = r.out.len() - n;
                r.enc.apply(&mut r.out[start..]);
                // Whatever the socket takes now is gone; the rest goes out on the next ready.
                r.flush_out()?;
                Ok(n)
            }
        }
    }
}

impl AsyncRead for PeerIo {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerIo::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            PeerIo::Utp(s) => Pin::new(s).poll_read(cx, buf),
            PeerIo::Rc4(r) => {
                let before = buf.filled().len();
                let res = Pin::new(&mut r.inner).poll_read(cx, buf);
                if matches!(res, Poll::Ready(Ok(()))) {
                    r.dec.apply(&mut buf.filled_mut()[before..]);
                }
                res
            }
        }
    }
}

impl AsyncWrite for PeerIo {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            PeerIo::Tcp(s) => Pin::new(s).poll_write(cx, data),
            PeerIo::Utp(s) => Pin::new(s).poll_write(cx, data),
            PeerIo::Rc4(r) => {
                // (Used for the handshake only: the peer loop writes through `try_write`.)
                match r.flush_out() {
                    Ok(true) => {}
                    Ok(false) => {
                        // Park on the socket until it can take the held-back bytes.
                        return match r.inner.poll_write_ready(cx) {
                            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                            _ => Poll::Pending,
                        };
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                }
                let n = data.len().min(64 * 1024);
                r.out.extend_from_slice(&data[..n]);
                let start = r.out.len() - n;
                r.enc.apply(&mut r.out[start..]);
                let _ = r.flush_out();
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerIo::Tcp(s) => Pin::new(s).poll_flush(cx),
            PeerIo::Utp(s) => Pin::new(s).poll_flush(cx),
            PeerIo::Rc4(r) => match r.flush_out() {
                Ok(true) => Poll::Ready(Ok(())),
                Ok(false) => match r.inner.poll_write_ready(cx) {
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    _ => Poll::Pending,
                },
                Err(e) => Poll::Ready(Err(e)),
            },
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            PeerIo::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            PeerIo::Utp(s) => Pin::new(s).poll_shutdown(cx),
            PeerIo::Rc4(r) => Pin::new(&mut r.inner).poll_shutdown(cx),
        }
    }
}

/// Addresses with a remembered property ("speaks uTP": they connected to us over uTP, we
/// connected to them over uTP, or `ut_pex` said so; "does not do encryption": an encrypted dial
/// failed and the plain one worked). Bounded; the oldest entries go first.
#[derive(Default)]
pub(crate) struct AddrSet {
    set: HashSet<SocketAddr>,
    order: VecDeque<SocketAddr>,
}

/// Most addresses remembered (about 100 KiB).
const MAX_UTP_PEERS: usize = 2048;

impl AddrSet {
    pub(crate) fn insert(&mut self, a: SocketAddr) {
        if self.set.insert(a) {
            self.order.push_back(a);
            while self.order.len() > MAX_UTP_PEERS {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    pub(crate) fn forget(&mut self, a: &SocketAddr) {
        // Rare (a uTP dial failed): the stale entry in `order` is dropped when it ages out.
        self.set.remove(a);
    }

    pub(crate) fn contains(&self, a: &SocketAddr) -> bool {
        self.set.contains(a)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.set.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utp_peers_are_bounded_and_forgettable() {
        let mut p = AddrSet::default();
        for i in 0..(MAX_UTP_PEERS as u32 + 100) {
            p.insert(SocketAddr::from(([10, 0, (i >> 8) as u8, i as u8], 6881)));
        }
        assert_eq!(p.len(), MAX_UTP_PEERS);
        assert!(!p.contains(&SocketAddr::from(([10, 0, 0, 0], 6881))), "oldest entry evicted");
        let last = SocketAddr::from(([10, 0, ((MAX_UTP_PEERS as u32 + 99) >> 8) as u8, (MAX_UTP_PEERS as u32 + 99) as u8], 6881));
        assert!(p.contains(&last));
        p.forget(&last);
        assert!(!p.contains(&last));
    }
}
