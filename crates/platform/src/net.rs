//! Sockets.

use std::io;

/// Let other processes on this machine bind the same UDP port (the app and a daemon, or tests, all
/// listening for LAN discovery): `SO_REUSEADDR` and, on Unix, `SO_REUSEPORT`. On Windows
/// `SO_REUSEADDR` alone shares a UDP port.
pub fn share_udp_port(s: &socket2::Socket) -> io::Result<()> {
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};

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
