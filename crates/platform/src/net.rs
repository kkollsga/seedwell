//! Sockets.

use std::io;
use std::net::{IpAddr, SocketAddr};

/// Let other processes on this machine bind the same UDP port (the app and a daemon, or tests, all
/// listening for LAN discovery): `SO_REUSEADDR` and, on Unix, `SO_REUSEPORT`. On Windows
/// `SO_REUSEADDR` alone shares a UDP port.
pub fn share_udp_port(s: &socket2::Socket) -> io::Result<()> {
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    Ok(())
}

/// A TCP listener on `addr` as tokio's `TcpListener::bind` makes one (`SO_REUSEADDR` on Unix, so a restart listens
/// again at once while old connections linger; not on Windows, where it would let another program take the port; a
/// backlog of 1024), except that one on the IPv6 any-address `[::]` takes IPv4 connections too: `IPV6_V6ONLY` is
/// turned off, on every system. Windows turns it on by default (macOS and Linux off), so a listener on `[::]` there
/// refused every IPv4 connection (Seedwell 0.1.18 and earlier). Inside a tokio runtime.
pub fn listen_tcp(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    listen_tcp_on(socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::STREAM, Some(socket2::Protocol::TCP))?, addr)
}

fn listen_tcp_on(s: socket2::Socket, addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    dual_stack(&s, addr)?;
    #[cfg(unix)]
    s.set_reuse_address(true)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    s.listen(1024)?;
    tokio::net::TcpListener::from_std(s.into())
}

/// A UDP socket on `addr` as tokio's `UdpSocket::bind` makes one, except that one on `[::]` takes IPv4 datagrams
/// too (`IPV6_V6ONLY` off, on every system: see [`listen_tcp`]). Inside a tokio runtime.
pub fn bind_udp(addr: SocketAddr) -> io::Result<tokio::net::UdpSocket> {
    bind_udp_on(socket2::Socket::new(socket2::Domain::for_address(addr), socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?, addr)
}

fn bind_udp_on(s: socket2::Socket, addr: SocketAddr) -> io::Result<tokio::net::UdpSocket> {
    dual_stack(&s, addr)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    tokio::net::UdpSocket::from_std(s.into())
}

/// A socket for `[::]` takes IPv4 too (the OS default is not relied on); other addresses keep the default.
fn dual_stack(s: &socket2::Socket, addr: SocketAddr) -> io::Result<()> {
    match addr.ip() {
        IpAddr::V6(ip) if ip.is_unspecified() => s.set_only_v6(false),
        _ => Ok(()),
    }
}

/// Whether this system can make a socket on `[::]` that takes IPv4 as well: false when IPv6 is turned off (no IPv6
/// socket can be made) or the system refuses to turn `IPV6_V6ONLY` off. A listener for every address then binds
/// `0.0.0.0` instead.
pub fn dual_stack_available() -> bool {
    socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, Some(socket2::Protocol::TCP)).and_then(|s| s.set_only_v6(false)).is_ok()
}

/// Whether a failed multicast join means the socket is in the group already: `EADDRINUSE` on Unix,
/// `WSAEINVAL` on Windows (checked on Windows 11: a second join of the same group answers 10022).
pub fn already_member(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.kind() == io::ErrorKind::AddrInUse;
    #[cfg(windows)]
    return e.kind() == io::ErrorKind::InvalidInput;
}

/// Connects a TCP stream for an outgoing dial to `addrs` (each address in turn, like
/// `TcpStream::connect`), with how long a dead peer takes to fail made the same on every system.
///
/// Windows retries a SYN that was answered with a reset, every 500 ms, four times: a refused dial
/// took 2.0 s there instead of one round trip. With `SIO_TCP_INITIAL_RTO` set to two SYN
/// retransmissions (round-trip time left to the system) it takes 1.0 s, and a peer that never answers
/// fails after 7 s with SYNs at 0, 1 and 3 s: three SYNs inside the engine's 10 s connect timeout, as
/// before (which were at 0, 3 and 9 s), so a lossy link gets as many tries. Measured on Windows 11:
/// default 2028 ms refused / 21 s silent; MaxSyn 1: 505 ms / 3 s; MaxSyn 2: 1014 ms / 7 s;
/// no SYN retransmissions: 0 ms / 1 s (one lost SYN would lose the peer). Elsewhere a plain connect.
pub async fn connect_tcp(addrs: impl tokio::net::ToSocketAddrs) -> io::Result<tokio::net::TcpStream> {
    let mut last = None;
    for addr in tokio::net::lookup_host(addrs).await? {
        let sock = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
        #[cfg(windows)]
        fewer_syn_retries(&sock);
        match sock.connect(addr).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to connect to")))
}

/// `SIO_TCP_INITIAL_RTO` with two SYN retransmissions (see [`connect_tcp`]). Best effort: a system
/// that refuses it keeps its default.
#[cfg(windows)]
fn fewer_syn_retries(sock: &tokio::net::TcpSocket) {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{SIO_TCP_INITIAL_RTO, TCP_INITIAL_RTO_PARAMETERS, WSAIoctl};
    let p = TCP_INITIAL_RTO_PARAMETERS { Rtt: u16::MAX, MaxSynRetransmissions: 2 };
    let mut returned = 0u32;
    // SAFETY: `p` is a valid input buffer of its own size for this ioctl, there is no output buffer,
    // and the call is synchronous (no overlapped structure or completion routine).
    unsafe {
        WSAIoctl(
            sock.as_raw_socket() as usize,
            SIO_TCP_INITIAL_RTO,
            std::ptr::from_ref(&p).cast(),
            std::mem::size_of::<TCP_INITIAL_RTO_PARAMETERS>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        );
    }
}

/// This computer's addresses on its networks that another device may reach it at: the one its default route
/// leaves from first, then the other interfaces' (Unix), IPv4 before IPv6, without loopback, link-local, multicast
/// or duplicate ones. For a link that says where its maker is (SPEC §7G.5): best effort, an empty list when the
/// computer is on no network.
pub fn lan_addresses() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
    let mut found: Vec<IpAddr> = Vec::new();
    // The default route's source address: a UDP "connect" sends nothing, it only picks the route (to documentation
    // addresses, so nothing is ever contacted).
    let routed = |bind: IpAddr, to: IpAddr| UdpSocket::bind((bind, 0)).and_then(|s| s.connect((to, 9)).and(s.local_addr())).ok().map(|a| a.ip());
    found.extend(routed(Ipv4Addr::UNSPECIFIED.into(), Ipv4Addr::new(192, 0, 2, 1).into()));
    found.extend(routed(Ipv6Addr::UNSPECIFIED.into(), Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into()));
    #[cfg(unix)]
    found.extend(interface_addresses());
    let mut out: Vec<IpAddr> = Vec::new();
    for ip in found {
        if reachable_ip(&ip) && !out.contains(&ip) {
            out.push(ip);
        }
    }
    // (A stable sort: the routed address stays first of its family.)
    out.sort_by_key(IpAddr::is_ipv6);
    out
}

/// An address another device could dial: not unspecified, loopback, multicast or link-local.
fn reachable_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => !(v4.is_unspecified() || v4.is_loopback() || v4.is_multicast() || v4.is_link_local() || v4.is_broadcast()),
        // fe80::/10 needs a scope (an interface) that another device cannot know.
        std::net::IpAddr::V6(v6) => !(v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80),
    }
}

/// The addresses of the interfaces that are up (`getifaddrs`).
#[cfg(unix)]
fn interface_addresses() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocates, freed below with freeifaddrs; nothing keeps a
    // pointer into it past that.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut p = head;
    while !p.is_null() {
        // SAFETY: `p` is a node of the list getifaddrs returned, not freed yet.
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        let flags = ifa.ifa_flags as libc::c_int;
        if flags & libc::IFF_UP == 0 || flags & libc::IFF_LOOPBACK != 0 || ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: `ifa_addr` is non-null and points at a sockaddr whose family says its real type and size.
        let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
        if family == libc::AF_INET {
            // SAFETY: an AF_INET address is a sockaddr_in.
            let sin = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
            out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))));
        } else if family == libc::AF_INET6 {
            // SAFETY: an AF_INET6 address is a sockaddr_in6.
            let sin6 = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in6>() };
            out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
        }
    }
    // SAFETY: `head` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(head) };
    out
}

/// **Tests:** a TCP port that refuses connections on each of `ips`, for a "dead" address (a tracker that is down, a
/// device that does not answer, a firewall's closed port).
///
/// A port bound to port 0 and let go is in the system's range for ports it hands out (from 32768 on Linux, 49152 on
/// macOS and Windows), so another test of a whole-suite run can be given it and listen there. This one is below those
/// ranges (24000-31999), so no bind to port 0 and no outgoing connection is ever given it, and it is checked when
/// chosen: binding it must work (an address this machine does not have, IPv6, has nothing listening at all), and a
/// connection to it must be refused. The bind alone is not enough: on macOS and Linux `std` binds with
/// `SO_REUSEADDR`, which lets `127.0.0.1:p` be bound while something listens on `0.0.0.0:p` or `[::]:p`.
#[doc(hidden)]
pub fn closed_tcp_port(ips: &[IpAddr]) -> u16 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64) ^ (u64::from(std::process::id()) << 32);
    let candidates = (0..10_000u64).map(|i| 24_000 + ((seed ^ i).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as u16 % 8_000);
    closed_tcp_port_of(ips, candidates).expect("a closed port below the systems' ephemeral ranges")
}

/// The first of `candidates` that is closed on each of `ips` ([`closed_tcp_port`]).
fn closed_tcp_port_of(ips: &[IpAddr], mut candidates: impl Iterator<Item = u16>) -> Option<u16> {
    let closed = |ip: IpAddr, p: u16| {
        let at = SocketAddr::new(ip, p);
        match std::net::TcpListener::bind(at) {
            Ok(l) => drop(l),
            Err(e) => return e.kind() == io::ErrorKind::AddrNotAvailable,
        }
        match std::net::TcpStream::connect_timeout(&at, std::time::Duration::from_millis(200)) {
            Ok(_) => false,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => true,
            // Windows answers a reset with more SYNs and says refused only after about 2 s; there `std` binds without
            // `SO_REUSEADDR`, so the bind above already found any listener on the port.
            Err(e) => cfg!(windows) && e.kind() == io::ErrorKind::TimedOut,
        }
    };
    candidates.find(|p| ips.iter().all(|ip| closed(*ip, *p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};

    /// The failure: a "closed" port that something listens on (another test, on every address: `0.0.0.0` or `[::]`)
    /// was taken because `127.0.0.1:p` could still be bound there (`SO_REUSEADDR` on macOS), and the dead address
    /// answered. The port listened on is passed over; the next, closed, is taken and refuses.
    #[test]
    fn a_closed_port_is_one_nothing_listens_on_at_any_address() {
        let any = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let taken = any.local_addr().unwrap().port();
        let lo = [IpAddr::V4(Ipv4Addr::LOCALHOST)];
        let p = closed_tcp_port(&lo);
        assert!((24_000..32_000).contains(&p), "{p}");
        assert_eq!(closed_tcp_port_of(&lo, [taken, p].into_iter()), Some(p));
        let e = std::net::TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, p)), std::time::Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn a_dial_connects_and_a_refused_one_fails_within_about_a_second() {
        let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let s = connect_tcp(l.local_addr().unwrap()).await.unwrap();
        assert_eq!(s.peer_addr().unwrap(), l.local_addr().unwrap());
        let closed = l.local_addr().unwrap();
        drop((s, l));
        let t = std::time::Instant::now();
        let e = connect_tcp(closed).await.unwrap_err();
        let took = t.elapsed();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused, "{e:?}");
        // Windows: ~1.0 s (2.0 s with its default SYN retries); elsewhere at once.
        assert!(took < std::time::Duration::from_millis(1500), "a refused dial took {took:?}");
    }

    /// A socket made the way Windows makes one: IPv6 only by default. On a Mac or Linux the system default is
    /// dual-stack, so the listener must turn it off itself for the test to see what Windows would.
    fn v6_only_socket(ty: socket2::Type, proto: socket2::Protocol) -> Option<socket2::Socket> {
        if !dual_stack_available() {
            eprintln!("no IPv6 here: nothing to check");
            return None;
        }
        let s = socket2::Socket::new(socket2::Domain::IPV6, ty, Some(proto)).unwrap();
        s.set_only_v6(true).unwrap();
        Some(s)
    }

    /// A TCP listener on `[::]` that refuses IPv4 connections, as Seedwell's sync port did on Windows (where
    /// `IPV6_V6ONLY` is on by default): fails when the listener keeps that default.
    #[tokio::test]
    async fn a_listener_on_any_ipv6_address_takes_ipv4_where_the_default_is_ipv6_only() {
        let Some(s) = v6_only_socket(socket2::Type::STREAM, socket2::Protocol::TCP) else { return };
        let l = listen_tcp_on(s, "[::]:0".parse().unwrap()).unwrap();
        assert!(!socket2::SockRef::from(&l).only_v6().unwrap(), "IPV6_V6ONLY stays on");
        let port = l.local_addr().unwrap().port();
        for ip in ["127.0.0.1", "::1"] {
            let to = SocketAddr::new(ip.parse().unwrap(), port);
            let dial = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::net::TcpStream::connect(to));
            dial.await.expect("dial timed out").unwrap_or_else(|e| panic!("{to}: {e}"));
            tokio::time::timeout(std::time::Duration::from_secs(5), l.accept()).await.expect("accept timed out").unwrap();
        }
    }

    /// The same for UDP (the sync port's uTP twin): fails when a datagram to 127.0.0.1 does not arrive.
    #[tokio::test]
    async fn a_udp_socket_on_any_ipv6_address_takes_ipv4_where_the_default_is_ipv6_only() {
        let Some(s) = v6_only_socket(socket2::Type::DGRAM, socket2::Protocol::UDP) else { return };
        let u = bind_udp_on(s, "[::]:0".parse().unwrap()).unwrap();
        assert!(!socket2::SockRef::from(&u).only_v6().unwrap(), "IPV6_V6ONLY stays on");
        let port = u.local_addr().unwrap().port();
        for (from, to) in [("127.0.0.1:0", "127.0.0.1"), ("[::1]:0", "::1")] {
            let c = tokio::net::UdpSocket::bind(from).await.unwrap();
            c.send_to(b"hello", (to.parse::<IpAddr>().unwrap(), port)).await.unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(5), u.recv_from(&mut buf)).await.expect("no datagram").unwrap();
            assert_eq!(&buf[..n], b"hello");
        }
    }

    #[test]
    fn lan_addresses_are_ones_another_device_can_dial() {
        let all = lan_addresses();
        for ip in &all {
            assert!(reachable_ip(ip), "{ip}");
        }
        for (i, ip) in all.iter().enumerate() {
            assert!(!all[i + 1..].contains(ip), "no duplicates: {all:?}");
        }
        assert!(all.windows(2).all(|w| !(w[0].is_ipv6() && w[1].is_ipv4())), "IPv4 first: {all:?}");
        for no in ["127.0.0.1", "169.254.1.2", "0.0.0.0", "224.0.0.251", "fe80::1", "::1", "::", "ff02::1"] {
            assert!(!reachable_ip(&no.parse().unwrap()), "{no}");
        }
        for yes in ["192.168.68.57", "10.0.0.2", "100.64.1.2", "fd00::5", "2001:db8::7"] {
            assert!(reachable_ip(&yes.parse().unwrap()), "{yes}");
        }
    }

    #[test]
    fn a_second_join_of_a_group_says_already_a_member() {
        let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let g = Ipv4Addr::new(239, 192, 83, 88);
        if let Err(e) = s.join_multicast_v4(&g, &Ipv4Addr::UNSPECIFIED) {
            eprintln!("no multicast here ({e}): nothing to check");
            return;
        }
        let e = s.join_multicast_v4(&g, &Ipv4Addr::UNSPECIFIED).unwrap_err();
        assert!(already_member(&e), "{e:?}");
    }
}
