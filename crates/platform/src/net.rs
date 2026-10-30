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
