//! Opening a connection: the magic, then TLS. What follows is [`crate::peer`].
//!
//! A session runs over any byte stream ([`BoxIo`]): TCP, uTP, or a relayed pipe (a member relay's
//! `RelayData` messages, or a `seedwell relay` server's paired streams). TLS always runs end to end between
//! the two devices, so a relay sees only ciphertext.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;
use trs_sync_proto::codec::Magic;
use trs_sync_proto::ids::DeviceId;
use trs_utp::UtpSocket;

use crate::tls::{self, Established, Tls};

/// Connecting, the magic and the TLS handshake together may take this long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// A byte stream a session can run over.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Io for T {}

/// A boxed [`Io`]: TCP, uTP or a relayed pipe.
pub type BoxIo = Box<dyn Io>;

/// What a session runs over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// A direct TCP connection.
    Tcp,
    /// A direct uTP connection (possibly through a punched hole).
    Utp,
    /// Through a relay: a member relaying, or a `seedwell relay` server.
    Relay,
}

impl Via {
    /// `TCP`, `uTP` or `Relay`.
    pub fn name(self) -> &'static str {
        match self {
            Via::Tcp => "TCP",
            Via::Utp => "uTP",
            Via::Relay => "Relay",
        }
    }
}

/// A connection after TLS: who is on the other end, and the stream.
pub struct Connected {
    /// The encrypted stream.
    pub stream: TlsStream<BoxIo>,
    /// The peer's key and the session's exporter value.
    pub est: Established,
    /// The peer's address (for a relayed session: the relay's).
    pub remote: SocketAddr,
    /// Our end.
    pub local: SocketAddr,
    /// We opened it.
    pub dialed: bool,
    /// What it runs over.
    pub via: Via,
}

impl Connected {
    /// The peer's device id.
    pub fn device(&self) -> DeviceId {
        DeviceId::of_public_key(&self.est.peer_pk)
    }
}

fn timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the handshake took too long")
}

fn tls_error(e: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Is this address on the local network (private, link-local, loopback, unique local)? Such connections
/// are not subject to Internet speed limits, and are dialed over TCP only.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The address as an IPv4 one when it is IPv4-mapped (`[::ffff:192.168.1.2]:5000`, which a dual-stack listener
/// reports for an IPv4 peer), so that it is shown and compared as the peer's own address.
pub fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

fn tune(s: &TcpStream) {
    // Small index updates and requests should not wait for Nagle.
    let _ = s.set_nodelay(true);
}

/// Sends the session magic over `io` and runs the TLS handshake as the client (no timeout of its own).
async fn client_over(tls: &Tls, mut io: BoxIo, remote: SocketAddr, local: SocketAddr, via: Via) -> io::Result<Connected> {
    io.write_all(&Magic::Session.bytes()).await?;
    let stream = tls.client.connect(Tls::server_name(), io).await?;
    let est = tls::established(stream.get_ref().1).map_err(tls_error)?;
    Ok(Connected { stream: TlsStream::Client(stream), est, remote: canonical(remote), local: canonical(local), dialed: true, via })
}

/// Reads the session magic from `io` and runs the TLS handshake as the server (no timeout of its own).
async fn server_over(tls: &Tls, mut io: BoxIo, remote: SocketAddr, local: SocketAddr, via: Via) -> io::Result<Connected> {
    let mut magic = [0u8; 8];
    io.read_exact(&mut magic).await?;
    if Magic::parse(&magic) != Some(Magic::Session) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a Seedwell Sync session"));
    }
    let stream = tls.server.accept(io).await?;
    let est = tls::established(stream.get_ref().1).map_err(tls_error)?;
    Ok(Connected { stream: TlsStream::Server(stream), est, remote: canonical(remote), local: canonical(local), dialed: false, via })
}

fn check_expected(c: Connected, expect: Option<DeviceId>) -> io::Result<Connected> {
    if let Some(want) = expect
        && c.device() != want
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "a different device answered at this address"));
    }
    Ok(c)
}

/// Opens a session over a stream that is already connected (a relayed pipe): the magic, then TLS as the
/// client. With `expect`, a peer with another key is refused.
pub async fn connect_over(
    tls: &Tls,
    io: BoxIo,
    remote: SocketAddr,
    local: SocketAddr,
    via: Via,
    expect: Option<DeviceId>,
) -> io::Result<Connected> {
    let c = tokio::time::timeout(HANDSHAKE_TIMEOUT, client_over(tls, io, remote, local, via)).await.map_err(|_| timeout())??;
    check_expected(c, expect)
}

/// Accepts a session over a stream that is already connected (uTP, a relayed pipe): reads the magic, then
/// TLS as the server.
pub async fn accept_over(tls: &Tls, io: BoxIo, remote: SocketAddr, local: SocketAddr, via: Via) -> io::Result<Connected> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, server_over(tls, io, remote, local, via)).await.map_err(|_| timeout())?
}

/// Connects to `addr` over TCP and runs the session handshake.
pub async fn dial_tcp(tls: &Tls, addr: SocketAddr) -> io::Result<Connected> {
    let run = async {
        let tcp = trs_platform::net::connect_tcp(addr).await?;
        tune(&tcp);
        let local = tcp.local_addr()?;
        client_over(tls, Box::new(tcp), addr, local, Via::Tcp).await
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, run).await.map_err(|_| timeout())?
}

/// Connects to `addr` over uTP (from our shared UDP socket) and runs the session handshake.
pub async fn dial_utp(tls: &Tls, utp: &UtpSocket, addr: SocketAddr) -> io::Result<Connected> {
    let run = async {
        let s = utp.connect(addr).await?;
        let local = s.local_addr();
        client_over(tls, Box::new(s), addr, local, Via::Utp).await
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, run).await.map_err(|_| timeout())?
}

/// How [`dial`] may reach an address.
#[derive(Clone, Copy)]
pub struct DialPlan<'a> {
    /// Our uTP socket, if we have one.
    pub utp: Option<&'a UtpSocket>,
    /// TCP may be used (tests turn it off to force uTP).
    pub tcp: bool,
}

/// Connects to `addr`, sends the session magic and runs the TLS handshake as the client. A LAN address is
/// dialed over TCP only (throughput); any other over TCP and uTP at once, and the first finished handshake
/// wins (the other is dropped). With `expect`, a peer with another key is refused (an address that now
/// belongs to someone else).
pub async fn dial(tls: &Tls, plan: DialPlan<'_>, addr: SocketAddr, expect: Option<DeviceId>) -> io::Result<Connected> {
    let use_tcp = plan.tcp;
    let utp = plan.utp.filter(|_| !plan.tcp || !is_lan(addr.ip()));
    let c = match (use_tcp, utp) {
        (true, None) => dial_tcp(tls, addr).await?,
        (false, Some(u)) => dial_utp(tls, u, addr).await?,
        (false, None) => return Err(io::Error::new(io::ErrorKind::Unsupported, "no transport to dial with")),
        (true, Some(u)) => {
            let tcp = dial_tcp(tls, addr);
            let utp = dial_utp(tls, u, addr);
            tokio::pin!(tcp, utp);
            let (mut tcp_done, mut utp_done) = (false, false);
            loop {
                let err = tokio::select! {
                    r = &mut tcp, if !tcp_done => match r {
                        Ok(c) => break c,
                        Err(e) => { tcp_done = true; e }
                    },
                    r = &mut utp, if !utp_done => match r {
                        Ok(c) => break c,
                        Err(e) => { utp_done = true; e }
                    },
                };
                if tcp_done && utp_done {
                    return Err(err);
                }
            }
        }
    };
    check_expected(c, expect)
}

/// What an accepted connection asked for.
pub enum Accepted {
    /// A sync session, after TLS.
    Session(Box<Connected>),
    /// The relay protocol magic (`SWRELAY1`): the raw stream, magic consumed.
    Relay(TcpStream, SocketAddr),
}

/// Reads the magic of an accepted TCP connection and, for a session, runs the TLS handshake as the server.
pub async fn accept(tls: &Tls, mut tcp: TcpStream, remote: SocketAddr) -> io::Result<Accepted> {
    tune(&tcp);
    let (remote, local) = (canonical(remote), canonical(tcp.local_addr()?));
    let run = async {
        let mut magic = [0u8; 8];
        tcp.read_exact(&mut magic).await?;
        match Magic::parse(&magic) {
            Some(Magic::Session) => {
                let stream = tls.server.accept(Box::new(tcp) as BoxIo).await?;
                let est = tls::established(stream.get_ref().1).map_err(tls_error)?;
                Ok(Accepted::Session(Box::new(Connected {
                    stream: TlsStream::Server(stream),
                    est,
                    remote,
                    local,
                    dialed: false,
                    via: Via::Tcp,
                })))
            }
            Some(Magic::Relay) => Ok(Accepted::Relay(tcp, remote)),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "not a Seedwell Sync connection")),
        }
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, run).await.map_err(|_| timeout())?
}
