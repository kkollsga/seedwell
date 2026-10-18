//! Default-gateway discovery without unsafe code or extra crates: run the system's
//! own routing tool and parse its text output. The parsers are pure and fuzzed.
//!
//! macOS/BSD: `/sbin/route -n get default` prints `gateway: 192.168.1.1`.
//! Linux (for CI and development): `ip -4 route show default` prints
//! `default via 192.168.1.1 dev eth0`.

use std::net::Ipv4Addr;

/// Longest tool output we parse.
pub const MAX_OUTPUT: usize = 16 * 1024;

fn usable(ip: Ipv4Addr) -> bool {
    !(ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast())
}

/// Parse `route -n get default` (BSD/macOS).
pub fn parse_bsd_route_get(out: &str) -> Option<Ipv4Addr> {
    out.get(..out.len().min(MAX_OUTPUT))
        .unwrap_or("")
        .lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .and_then(|v| v.trim().parse::<Ipv4Addr>().ok())
        .filter(|&ip| usable(ip))
}

/// Parse `ip -4 route show default` (Linux).
pub fn parse_linux_ip_route(out: &str) -> Option<Ipv4Addr> {
    out.get(..out.len().min(MAX_OUTPUT))
        .unwrap_or("")
        .lines()
        .filter(|l| l.trim_start().starts_with("default"))
        .find_map(|l| {
            let mut words = l.split_ascii_whitespace();
            words.find(|&w| w == "via")?;
            words.next()?.parse::<Ipv4Addr>().ok()
        })
        .filter(|&ip| usable(ip))
}

#[cfg(feature = "driver")]
pub use detect::{NetInfo, detect};

#[cfg(feature = "driver")]
mod detect {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::{MAX_OUTPUT, parse_bsd_route_get, parse_linux_ip_route};

    pub use crate::core::NetInfo;

    fn run(program: &str, args: &[&str]) -> io::Result<String> {
        let out = Command::new(program)
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.truncate(MAX_OUTPUT);
        Ok(s)
    }

    /// Find the default IPv4 gateway and the local address we would use to reach it.
    ///
    /// No packet is sent: the local address comes from connecting a UDP socket, which
    /// only consults the routing table.
    pub async fn detect(probe_port: u16) -> io::Result<NetInfo> {
        let gateway = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(|| {
                // Absolute paths: never resolve the tool through a user-controlled PATH.
                if cfg!(target_os = "macos") {
                    run("/sbin/route", &["-n", "get", "default"]).map(|o| parse_bsd_route_get(&o))
                } else {
                    run("/sbin/ip", &["-4", "route", "show", "default"])
                        .or_else(|_| run("/usr/sbin/ip", &["-4", "route", "show", "default"]))
                        .or_else(|_| run("/bin/ip", &["-4", "route", "show", "default"]))
                        .map(|o| parse_linux_ip_route(&o))
                }
            }),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "route lookup timed out"))?
        .map_err(io::Error::other)??
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default IPv4 gateway"))?;
        let probe = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        probe
            .connect(SocketAddr::from((gateway, probe_port)))
            .await?;
        match probe.local_addr()? {
            SocketAddr::V4(a) if !a.ip().is_unspecified() => Ok(NetInfo {
                gateway,
                local: *a.ip(),
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no local IPv4 address",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_route_get() {
        let out = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.168.1.1\n  interface: en0\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(
            parse_bsd_route_get(out),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        // VPN default route without a gateway, on-link gateways, junk
        assert_eq!(
            parse_bsd_route_get("    gateway: link#4\n  interface: utun3\n"),
            None
        );
        assert_eq!(parse_bsd_route_get("gateway: fe80::1%en0"), None);
        assert_eq!(parse_bsd_route_get("gateway: 0.0.0.0"), None);
        assert_eq!(parse_bsd_route_get("gateway: 127.0.0.1"), None);
        assert_eq!(
            parse_bsd_route_get("route: writing to routing socket: not in table"),
            None
        );
        assert_eq!(parse_bsd_route_get(""), None);
    }

    #[test]
    fn linux_ip_route() {
        assert_eq!(
            parse_linux_ip_route("default via 10.0.0.1 dev eth0 proto dhcp metric 100\n"),
            Some(Ipv4Addr::new(10, 0, 0, 1))
        );
        assert_eq!(
            parse_linux_ip_route("10.0.0.0/24 dev eth0 scope link\n"),
            None
        );
        assert_eq!(parse_linux_ip_route("default dev tun0 scope link\n"), None);
        assert_eq!(parse_linux_ip_route("default via"), None);
        assert_eq!(parse_linux_ip_route("default via 255.255.255.255"), None);
    }

    #[test]
    fn huge_output_is_capped() {
        let big = format!("{}gateway: 192.168.1.1\n", "x\n".repeat(MAX_OUTPUT));
        assert_eq!(parse_bsd_route_get(&big), None);
    }
}
