//! A SOCKS5 client for torrent traffic (SPEC §7H): CONNECT (RFC 1928) with username and password (RFC 1929).
//!
//! A target is an address or a host name; a name is sent to the proxy as it is and the proxy resolves it, so no name
//! of a peer or a tracker is ever looked up on this computer. Only the proxy's own name is looked up here (it has to
//! be reached).
//!
//! The credentials live in [`Credentials`], which never prints them ([`std::fmt::Debug`] says `hidden`) and wipes
//! them when dropped. Every error says in words what went wrong ([`Error`]'s `Display`) and whether the proxy itself
//! is at fault ([`Error::is_proxy_fault`]) or only the target could not be reached through it.
//!
//! With the `testkit` feature, [`testkit::FakeProxy`] is a loopback SOCKS5 server with auth for tests.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[cfg(feature = "testkit")]
pub mod testkit;

/// The SOCKS version byte.
const VER: u8 = 5;
/// Method: no authentication.
const NO_AUTH: u8 = 0x00;
/// Method: username and password (RFC 1929).
const USER_PASS: u8 = 0x02;
/// The proxy accepts none of the offered methods.
const NO_ACCEPTABLE: u8 = 0xFF;

/// A username and password for the proxy. Never printed, wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    user: String,
    pass: String,
}

impl Credentials {
    /// Both trimmed of surrounding white space (a copy from a web page often brings some). RFC 1929 allows 1 to 255
    /// bytes each; anything else is refused with a sentence that says which.
    pub fn new(user: &str, pass: &str) -> Result<Credentials, &'static str> {
        let (user, pass) = (user.trim(), pass.trim());
        if user.is_empty() {
            return Err("The username is empty.");
        }
        if pass.is_empty() {
            return Err("The password is empty.");
        }
        if user.len() > 255 || pass.len() > 255 {
            return Err("The username or the password is longer than a SOCKS5 proxy takes (255 bytes).");
        }
        Ok(Credentials { user: user.to_owned(), pass: pass.to_owned() })
    }

    /// The username (for the secret store only: never shown or logged).
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The password (for the secret store only: never shown or logged).
    pub fn pass(&self) -> &str {
        &self.pass
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(hidden)")
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8, so the strings stay valid while they are wiped.
        unsafe {
            self.user.as_mut_vec().fill(0);
            self.pass.as_mut_vec().fill(0);
        }
    }
}

/// A SOCKS5 proxy: where it is and how to log in.
#[derive(Clone, PartialEq, Eq)]
pub struct Proxy {
    /// A host name or an address.
    pub host: String,
    pub port: u16,
    pub auth: Option<Credentials>,
}

impl Proxy {
    /// `host:port` (`[v6]:port` for an IPv6 address), for messages and the model.
    pub fn label(&self) -> String {
        match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("[{ip}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Proxy({}, auth: {})", self.label(), if self.auth.is_some() { "hidden" } else { "none" })
    }
}

/// Where the proxy should connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An address (a peer).
    Addr(SocketAddr),
    /// A host name the proxy resolves (a tracker), and the port.
    Name(String, u16),
}

/// What went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The proxy could not be reached: its name was not found, or the connection to it failed. Why, in a few plain
    /// words ([`io_words`]; never the system's text with its "os error N").
    Unreachable(String),
    /// The proxy did not answer its handshake in time.
    Timeout,
    /// The proxy turned the username and password down.
    AuthRejected,
    /// The proxy asks for a username and password and none is set, or accepts none of the ways offered.
    NoMethod,
    /// The proxy answered with something that is not SOCKS5.
    NotSocks,
    /// The proxy answered the request with this reply code (RFC 1928 §6): 1 general failure, 2 not allowed, 3 network
    /// unreachable, 4 host unreachable, 5 refused, 6 TTL expired, 7 command not supported, 8 address type not supported.
    Reply(u8),
    /// The target did not answer through the proxy in time (the proxy itself had answered).
    TargetTimeout,
    /// The connection broke during the handshake: how, in a few plain words ([`io_words`]).
    Broken(String),
    /// The target cannot be written as a SOCKS5 request (a name longer than 255 bytes).
    BadTarget,
}

impl Error {
    /// The proxy itself is at fault (unreachable, silent, wrong credentials, not a SOCKS5 proxy, an unsupported
    /// request): another server, or the user, has to fix it. A target the proxy could not reach (reply codes 1 to 6,
    /// a target that does not answer) is not the proxy's fault.
    pub fn is_proxy_fault(&self) -> bool {
        match self {
            Error::Unreachable(_) | Error::Timeout | Error::AuthRejected | Error::NoMethod | Error::NotSocks | Error::Broken(_) => true,
            Error::Reply(c) => !(1..=6).contains(c),
            Error::TargetTimeout | Error::BadTarget => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreachable(e) => f.write_str(e),
            Error::Timeout => f.write_str("the proxy didn\u{2019}t answer in time"),
            Error::AuthRejected => f.write_str("the proxy turned the username and password down"),
            Error::NoMethod => f.write_str("the proxy wants another way of logging in"),
            Error::NotSocks => f.write_str("the server doesn\u{2019}t answer as a SOCKS5 proxy"),
            Error::Reply(c) => f.write_str(match c {
                1 => "the proxy failed to connect",
                2 => "the proxy doesn\u{2019}t allow that connection",
                3 => "the proxy can\u{2019}t reach that network",
                4 => "the proxy can\u{2019}t reach that host",
                5 => "the connection was refused at the other end",
                6 => "the connection expired on the way",
                7 => "the proxy doesn\u{2019}t support connecting",
                8 => "the proxy doesn\u{2019}t support that kind of address",
                _ => "the proxy answered with an unknown error",
            }),
            Error::TargetTimeout => f.write_str("the other end didn\u{2019}t answer through the proxy in time"),
            Error::Broken(e) => f.write_str(e),
            Error::BadTarget => f.write_str("the address is too long for a SOCKS5 proxy"),
        }
    }
}

impl std::error::Error for Error {}

/// How long the steps took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The connection to the proxy plus its greeting and login: what the proxy's own speed is.
    pub handshake: Duration,
    /// From the CONNECT request to the proxy's answer: the proxy's way to the target and back.
    pub connect: Duration,
}

fn broken(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof { Error::NotSocks } else { Error::Broken(io_words(&e, false).into()) }
}

/// Why connecting to the proxy (`connecting`) or talking to it failed, as a short phrase a sentence can hold: the
/// common kinds by name (refused, timed out, unreachable, offline, closed, a name not found), never the system's own
/// text with its "os error 61" and nested parentheses.
pub fn io_words(e: &std::io::Error, connecting: bool) -> &'static str {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => "the proxy refused the connection",
        K::TimedOut => "the connection to the proxy timed out",
        K::HostUnreachable | K::NetworkUnreachable | K::AddrNotAvailable => "the proxy can\u{2019}t be reached from this network",
        K::NetworkDown => "this computer has no network connection",
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => "the proxy closed the connection",
        K::PermissionDenied => "this computer blocked the connection",
        _ if is_name_not_found(e) => "the proxy\u{2019}s name wasn\u{2019}t found",
        _ if connecting => "the proxy can\u{2019}t be reached",
        _ => "the connection to the proxy broke",
    }
}

/// A name lookup that failed: the systems say so only in words (macOS "nodename nor servname provided, or not
/// known", Linux "Name or service not known", Windows "No such host is known"; tokio prefixes "failed to lookup
/// address information").
fn is_name_not_found(e: &std::io::Error) -> bool {
    let text = e.to_string().to_ascii_lowercase();
    ["lookup address", "not known", "no such host", "name resolution", "nodename"].iter().any(|w| text.contains(w))
}

/// Connects to the proxy and logs in. Within `deadline`; a late step is [`Error::Timeout`].
async fn open(proxy: &Proxy, deadline: tokio::time::Instant) -> Result<TcpStream, Error> {
    let connect = TcpStream::connect((proxy.host.as_str(), proxy.port));
    let mut s = match tokio::time::timeout_at(deadline, connect).await {
        Err(_) => return Err(Error::Timeout),
        Ok(Err(e)) => return Err(Error::Unreachable(io_words(&e, true).into())),
        Ok(Ok(s)) => s,
    };
    let _ = s.set_nodelay(true);
    let steps = async {
        // Offer only what we can do: username and password when they are set, else no authentication.
        let method = if proxy.auth.is_some() { USER_PASS } else { NO_AUTH };
        s.write_all(&[VER, 1, method]).await.map_err(broken)?;
        let mut reply = [0u8; 2];
        s.read_exact(&mut reply).await.map_err(broken)?;
        if reply[0] != VER {
            return Err(Error::NotSocks);
        }
        if reply[1] == NO_ACCEPTABLE || reply[1] != method {
            return Err(Error::NoMethod);
        }
        if let Some(c) = &proxy.auth {
            let mut msg = Vec::with_capacity(3 + c.user.len() + c.pass.len());
            msg.push(1);
            msg.push(c.user.len() as u8);
            msg.extend_from_slice(c.user.as_bytes());
            msg.push(c.pass.len() as u8);
            msg.extend_from_slice(c.pass.as_bytes());
            let sent = s.write_all(&msg).await;
            msg.fill(0);
            sent.map_err(broken)?;
            let mut status = [0u8; 2];
            // A proxy that closes instead of answering a wrong login (some do) said no all the same.
            match s.read_exact(&mut status).await {
                Ok(_) if status[0] != 1 => return Err(Error::NotSocks),
                Ok(_) if status[1] != 0 => return Err(Error::AuthRejected),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::AuthRejected),
                Err(e) => return Err(broken(e)),
            }
        }
        Ok::<(), Error>(())
    };
    match tokio::time::timeout_at(deadline, steps).await {
        Err(_) => Err(Error::Timeout),
        Ok(r) => r.map(|()| s),
    }
}

/// The proxy's handshake alone (connect, greeting, login), then the connection is closed: the health check. Says how
/// long it took. Within `limit`.
pub async fn handshake(proxy: &Proxy, limit: Duration) -> Result<Duration, Error> {
    let start = Instant::now();
    let s = open(proxy, tokio::time::Instant::now() + limit).await?;
    let took = start.elapsed();
    drop(s);
    Ok(took)
}

/// A connection to `target` through `proxy`, ready for the target's protocol, and how long the steps took. The proxy
/// resolves a [`Target::Name`]. Within `limit` for the whole: a proxy that is late with its handshake is
/// [`Error::Timeout`], one that is late with the CONNECT answer is [`Error::TargetTimeout`].
pub async fn connect(proxy: &Proxy, target: &Target, limit: Duration) -> Result<(TcpStream, Timing), Error> {
    let mut req = vec![VER, 1, 0];
    match target {
        Target::Addr(SocketAddr::V4(a)) => {
            req.push(1);
            req.extend_from_slice(&a.ip().octets());
            req.extend_from_slice(&a.port().to_be_bytes());
        }
        Target::Addr(SocketAddr::V6(a)) => {
            match a.ip().to_ipv4_mapped() {
                Some(v4) => {
                    req.push(1);
                    req.extend_from_slice(&v4.octets());
                }
                None => {
                    req.push(4);
                    req.extend_from_slice(&a.ip().octets());
                }
            }
            req.extend_from_slice(&a.port().to_be_bytes());
        }
        Target::Name(name, port) => {
            // An address written as a name is sent as the address (the proxy would resolve it the same).
            if let Ok(ip) = name.trim_matches(['[', ']']).parse::<IpAddr>() {
                return Box::pin(connect(proxy, &Target::Addr(SocketAddr::new(ip, *port)), limit)).await;
            }
            if name.is_empty() || name.len() > 255 {
                return Err(Error::BadTarget);
            }
            req.push(3);
            req.push(name.len() as u8);
            req.extend_from_slice(name.as_bytes());
            req.extend_from_slice(&port.to_be_bytes());
        }
    }
    let start = Instant::now();
    let deadline = tokio::time::Instant::now() + limit;
    let mut s = open(proxy, deadline).await?;
    let handshake = start.elapsed();
    let asked = Instant::now();
    let answer = async {
        s.write_all(&req).await.map_err(broken)?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await.map_err(broken)?;
        if head[0] != VER {
            return Err(Error::NotSocks);
        }
        if head[1] != 0 {
            return Err(Error::Reply(head[1]));
        }
        // The bound address, which we do not need.
        let rest = match head[3] {
            1 => 4 + 2,
            4 => 16 + 2,
            3 => {
                let mut n = [0u8; 1];
                s.read_exact(&mut n).await.map_err(broken)?;
                usize::from(n[0]) + 2
            }
            _ => return Err(Error::NotSocks),
        };
        let mut skip = vec![0u8; rest];
        s.read_exact(&mut skip).await.map_err(broken)?;
        Ok::<(), Error>(())
    };
    match tokio::time::timeout_at(deadline, answer).await {
        Err(_) => Err(Error::TargetTimeout),
        Ok(Err(e)) => Err(e),
        Ok(Ok(())) => Ok((s, Timing { handshake, connect: asked.elapsed() })),
    }
}

#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::*;
    use crate::testkit::FakeProxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Failure: the credentials reach a log or a panic message through `{:?}`.
    #[test]
    fn credentials_never_print() {
        let c = Credentials::new(" alice ", "s3cret-pass").unwrap();
        let p = Proxy { host: "nl.socks.nordhold.net".into(), port: 1080, auth: Some(c.clone()) };
        let text = format!("{c:?} {p:?}");
        assert!(!text.contains("alice") && !text.contains("s3cret"), "{text}");
        assert_eq!(c.user(), "alice", "trimmed");
        assert!(Credentials::new("", "x").is_err() && Credentials::new("x", " ").is_err());
    }

    /// Failure: a peer or tracker dialled through the proxy does not arrive, or a name is resolved here instead of
    /// at the proxy (the fake resolves only the names it was told, so a local lookup would not be noticed by it but
    /// a name sent as an address would fail).
    #[tokio::test]
    async fn connects_to_addresses_and_names_through_the_proxy_with_auth() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taddr = target.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = target.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 4];
                    if s.read_exact(&mut b).await.is_ok() {
                        let _ = s.write_all(&b).await;
                    }
                });
            }
        });
        let fake = FakeProxy::start("user", "pass").await;
        fake.map_name("tracker.example", taddr.ip());
        let proxy = fake.proxy();
        let (mut s, t) = connect(&proxy, &Target::Addr(taddr), Duration::from_secs(5)).await.unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping");
        assert!(t.handshake < Duration::from_secs(5));
        let (mut s, _) = connect(&proxy, &Target::Name("tracker.example".into(), taddr.port()), Duration::from_secs(5)).await.unwrap();
        s.write_all(b"pong").await.unwrap();
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"pong");
        assert_eq!(fake.connects(), vec![taddr.to_string(), format!("tracker.example:{}", taddr.port())]);
        // A name the proxy cannot resolve is the target's problem, not the proxy's.
        let e = connect(&proxy, &Target::Name("nowhere.example".into(), 80), Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(e, Error::Reply(4));
        assert!(!e.is_proxy_fault());
    }

    /// Failure: wrong credentials look like a network problem (the user would never learn the key is wrong).
    #[tokio::test]
    async fn wrong_credentials_are_an_auth_failure() {
        let fake = FakeProxy::start("user", "pass").await;
        let mut p = fake.proxy();
        p.auth = Some(Credentials::new("user", "wrong").unwrap());
        let e = handshake(&p, Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(e, Error::AuthRejected);
        assert!(e.is_proxy_fault());
        p.auth = None;
        assert_eq!(handshake(&p, Duration::from_secs(5)).await.unwrap_err(), Error::NoMethod);
        assert_eq!(fake.auth_failures(), 1);
    }

    /// Failure: a refused or failed connection is said with the system's own text ("Connection refused (os error 61)")
    /// that the hosts show to the user, with its error number and nested parentheses.
    #[tokio::test]
    async fn a_refused_connection_is_said_in_plain_words() {
        let fake = FakeProxy::start("u", "p").await;
        let proxy = fake.proxy();
        fake.stop().await;
        let e = handshake(&proxy, Duration::from_secs(5)).await.unwrap_err();
        let said = e.to_string();
        assert!(!said.is_empty() && !said.contains("os error") && !said.contains('(') && !said.contains("61"), "{said}");
        use std::io::{Error as IoError, ErrorKind as K};
        for (err, connecting) in [
            (IoError::from(K::TimedOut), true),
            (IoError::from(K::HostUnreachable), true),
            (IoError::from(K::ConnectionReset), false),
            (IoError::other("failed to lookup address information: nodename nor servname provided, or not known"), true),
            (IoError::from_raw_os_error(61), true),
            (IoError::other("something new (os error 99)"), false),
        ] {
            let words = io_words(&err, connecting);
            assert!(!words.is_empty() && !words.contains("os error") && !words.contains('('), "{err}: {words}");
        }
    }

    /// Failure: a proxy that is down, or one that never answers, hangs the caller instead of failing in time.
    #[tokio::test]
    async fn a_dead_or_silent_proxy_fails_in_time() {
        let fake = FakeProxy::start("u", "p").await;
        let proxy = fake.proxy();
        fake.stop().await;
        let e = handshake(&proxy, Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(e, Error::Unreachable(_)), "{e:?}");
        fake.resume().await;
        assert!(handshake(&proxy, Duration::from_secs(5)).await.is_ok(), "back on the same port");
        fake.set_delay(Duration::from_millis(400));
        let t = Instant::now();
        assert_eq!(handshake(&proxy, Duration::from_millis(300)).await.unwrap_err(), Error::Timeout);
        assert!(t.elapsed() < Duration::from_secs(2));
        // A slow proxy that does answer is measured, not failed.
        fake.set_delay(Duration::from_millis(100));
        let took = handshake(&proxy, Duration::from_secs(5)).await.unwrap();
        assert!(took >= Duration::from_millis(200), "two answers, each delayed: {took:?}");
    }
}
