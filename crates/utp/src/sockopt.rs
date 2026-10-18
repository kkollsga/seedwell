//! The one place in this crate that talks to the OS directly: switch the Don't-Fragment bit of the
//! uTP socket on for the duration of a path-MTU probe, so an oversized probe is dropped (or refused
//! with `EMSGSIZE`) instead of being fragmented into a "success". Like libutp, DF is set only on
//! the first transmission of a probe; everything else, including a probe's retransmission, may be
//! fragmented.
//!
//! * macOS: `IP_DONTFRAG` / `IPV6_DONTFRAG`.
//! * Linux: `IP_MTU_DISCOVER = IP_PMTUDISC_PROBE` (DF set, local path-MTU cache ignored, so we can
//!   probe above what the kernel has learnt) and `IPV6_MTU_DISCOVER = IPV6_PMTUDISC_PROBE`.
//!
//! Failures are ignored: without DF the probes still work on paths that fragment lazily (a
//! fragmenting path just reports a bigger MTU than it has), and every size we confirm is capped
//! by [`Config`](crate::Config) anyway.
#![allow(unsafe_code)]

use tokio::net::UdpSocket;

/// Best effort: set the Don't-Fragment behaviour on `sock` for IPv4 and (if it is an IPv6 or
/// dual-stack socket) IPv6. Returns whether at least one option was accepted.
#[cfg(unix)]
pub(crate) fn set_dont_fragment(sock: &UdpSocket, on: bool) -> bool {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, val: libc::c_int| -> bool {
        // SAFETY: `fd` is a live socket owned by `sock` for the whole call; `val` outlives the
        // call and its size is passed correctly.
        let r = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&val as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        r == 0
    };
    let v6 = sock.local_addr().map(|a| a.is_ipv6()).unwrap_or(false);
    let mut ok = false;
    #[cfg(target_vendor = "apple")]
    {
        const IPV6_DONTFRAG: libc::c_int = 62;
        ok |= set(libc::IPPROTO_IP, libc::IP_DONTFRAG, on as libc::c_int);
        if v6 {
            ok |= set(libc::IPPROTO_IPV6, IPV6_DONTFRAG, on as libc::c_int);
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        const IPV6_MTU_DISCOVER: libc::c_int = 23;
        const IPV6_PMTUDISC_PROBE: libc::c_int = 3;
        let (v4, v6mode) = if on { (libc::IP_PMTUDISC_PROBE, IPV6_PMTUDISC_PROBE) } else { (libc::IP_PMTUDISC_DONT, 0) };
        ok |= set(libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, v4);
        if v6 {
            ok |= set(libc::IPPROTO_IPV6, IPV6_MTU_DISCOVER, v6mode);
        }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        let _ = (&set, v6);
    }
    ok
}

#[cfg(not(unix))]
pub(crate) fn set_dont_fragment(_sock: &UdpSocket, _on: bool) -> bool {
    false
}

#[cfg(all(test, any(target_vendor = "apple", target_os = "linux")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dont_fragment_is_accepted_on_v4_and_v6_sockets() {
        let v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert!(set_dont_fragment(&v4, true));
        assert!(set_dont_fragment(&v4, false));
        if let Ok(v6) = UdpSocket::bind("[::1]:0").await {
            assert!(set_dont_fragment(&v6, true));
            assert!(set_dont_fragment(&v6, false));
        }
    }

    /// Linux: the option is not just accepted, it reads back as PROBE (3) when on and DONT (0) when off.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn linux_pmtu_discovery_reads_back_probe_then_dont() {
        use std::os::fd::AsRawFd;
        fn get(fd: i32, level: libc::c_int, name: libc::c_int) -> libc::c_int {
            let mut v: libc::c_int = -1;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: valid fd, `v`/`len` live for the call.
            let r = unsafe { libc::getsockopt(fd, level, name, (&mut v as *mut libc::c_int).cast(), &mut len) };
            assert_eq!(r, 0, "getsockopt {name}");
            v
        }
        let v4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert!(set_dont_fragment(&v4, true));
        assert_eq!(get(v4.as_raw_fd(), libc::IPPROTO_IP, libc::IP_MTU_DISCOVER), libc::IP_PMTUDISC_PROBE);
        assert!(set_dont_fragment(&v4, false));
        assert_eq!(get(v4.as_raw_fd(), libc::IPPROTO_IP, libc::IP_MTU_DISCOVER), libc::IP_PMTUDISC_DONT);
        if let Ok(v6) = UdpSocket::bind("[::1]:0").await {
            assert!(set_dont_fragment(&v6, true));
            assert_eq!(get(v6.as_raw_fd(), libc::IPPROTO_IPV6, 23), 3, "IPV6_PMTUDISC_PROBE");
            assert!(set_dont_fragment(&v6, false));
            assert_eq!(get(v6.as_raw_fd(), libc::IPPROTO_IPV6, 23), 0, "IPV6_PMTUDISC_DONT");
        }
    }
}
