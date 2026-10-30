//! Destination policy: where a tracker request may go (SSRF protection).
//!
//! A tracker URL comes from a `.torrent` or magnet link, i.e. from a stranger.
//! Left alone, it can aim an HTTP `GET` with an attacker-chosen path and query
//! at the user's router (`http://192.168.1.1/cgi-bin/...`), at a local service
//! (`http://127.0.0.1:9091/...`) or at a cloud metadata endpoint
//! (`http://169.254.169.254/...`). libtorrent has `ssrf_mitigation` for exactly
//! this. The policy is checked by both transports **after name resolution**, on
//! every connection (so also on every redirect hop and every retry), and the
//! transports connect to the vetted address itself, so a DNS answer cannot
//! change between the check and the connect (no DNS rebinding).
//!
//! | Class | Examples | Refused |
//! |---|---|---|
//! | unspecified, multicast, broadcast, "this network", reserved | `0.0.0.0`, `::`, `224.0.0.1`, `ff02::1`, `255.255.255.255`, `240.0.0.1` | always |
//! | link-local and cloud metadata | `169.254.0.0/16` (incl. `169.254.169.254`), `fe80::/10`, `100.100.100.200`, `168.63.129.16`, `fd00:ec2::254`, `192.0.0.0/24` | always |
//! | loopback | `127.0.0.0/8`, `::1` | unless [`DestinationPolicy::allow_loopback`] (tests and local development only) |
//! | private networks | `10/8`, `172.16/12`, `192.168/16`, CGNAT `100.64/10`, `fc00::/7`, `fec0::/10` | unless [`DestinationPolicy::allow_lan_trackers`] |
//!
//! IPv4-mapped / compatible IPv6 (`::ffff:127.0.0.1`), NAT64 (`64:ff9b::/96`)
//! and 6to4 (`2002::/16`) addresses are judged by the IPv4 address they embed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Where the transports may connect. The default is the strict production
/// policy: public addresses only.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DestinationPolicy {
    /// Allow RFC 1918, CGNAT and unique-local (ULA) trackers: a tracker on the
    /// user's own LAN or VPN. A user-visible opt-in, off by default.
    pub allow_lan_trackers: bool,
    /// Allow loopback trackers. Meant for tests and local development; there
    /// is no reason for a production UI to offer it.
    pub allow_loopback: bool,
    /// Allow public addresses: every real tracker. Off only in an isolated (test) run, which must never reach a real
    /// tracker whatever torrent it is given.
    pub allow_public: bool,
}

/// Why an address was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// `0.0.0.0`, `::`, `0.0.0.0/8`.
    Unspecified,
    /// Multicast or broadcast.
    Multicast,
    /// Reserved ranges (`240.0.0.0/4`).
    Reserved,
    /// Link-local (`169.254/16`, `fe80::/10`).
    LinkLocal,
    /// A known cloud metadata endpoint.
    Metadata,
    /// Loopback while `allow_loopback` is off.
    Loopback,
    /// RFC 1918 / CGNAT / ULA while `allow_lan_trackers` is off.
    PrivateNetwork,
    /// A public address while `allow_public` is off (an isolated test run).
    Public,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Unspecified => "unspecified address",
            Refusal::Multicast => "multicast or broadcast address",
            Refusal::Reserved => "reserved address",
            Refusal::LinkLocal => "link-local address",
            Refusal::Metadata => "cloud metadata address",
            Refusal::Loopback => "loopback address",
            Refusal::PrivateNetwork => "private network address (enable allow_lan_trackers)",
            Refusal::Public => "public address (an isolated test run reaches only local trackers)",
        })
    }
}
impl std::error::Error for Refusal {}

impl DestinationPolicy {
    /// A public address: allowed unless `allow_public` is off.
    fn public(&self) -> Result<(), Refusal> {
        if self.allow_public { Ok(()) } else { Err(Refusal::Public) }
    }

    /// The production policy: public addresses only.
    pub const STRICT: DestinationPolicy = DestinationPolicy {
        allow_lan_trackers: false,
        allow_loopback: false,
        allow_public: true,
    };

    /// Tests and local development: loopback and private networks allowed.
    pub const LOCAL: DestinationPolicy = DestinationPolicy {
        allow_lan_trackers: true,
        allow_loopback: true,
        allow_public: true,
    };

    /// May we connect to `ip`?
    pub fn check(&self, ip: IpAddr) -> Result<(), Refusal> {
        match ip {
            IpAddr::V4(a) => self.check_v4(a),
            IpAddr::V6(a) => self.check_v6(a),
        }
    }

    /// Keep the addresses we may connect to, in order. If every address is
    /// refused, the reason for the first one is returned.
    pub fn vet(
        &self,
        addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<Vec<SocketAddr>, Refusal> {
        let mut ok = Vec::new();
        let mut first_refusal = None;
        for a in addrs {
            match self.check(a.ip()) {
                Ok(()) => ok.push(a),
                Err(r) => {
                    first_refusal.get_or_insert(r);
                }
            }
        }
        if ok.is_empty() {
            Err(first_refusal.unwrap_or(Refusal::Unspecified))
        } else {
            Ok(ok)
        }
    }

    fn check_v4(&self, a: Ipv4Addr) -> Result<(), Refusal> {
        let o = a.octets();
        if a.is_unspecified() || o[0] == 0 {
            return Err(Refusal::Unspecified);
        }
        if a.is_multicast() || a.is_broadcast() {
            return Err(Refusal::Multicast);
        }
        if o[0] >= 240 {
            return Err(Refusal::Reserved);
        }
        if a.is_link_local() {
            return Err(Refusal::LinkLocal);
        }
        // Metadata endpoints outside 169.254/16 (Alibaba sits in CGNAT space,
        // Azure's wire server and Oracle's address are not private at all).
        if matches!(
            o,
            [100, 100, 100, 200] | [168, 63, 129, 16] | [192, 0, 0, _]
        ) {
            return Err(Refusal::Metadata);
        }
        if a.is_loopback() {
            return if self.allow_loopback {
                Ok(())
            } else {
                Err(Refusal::Loopback)
            };
        }
        let cgnat = o[0] == 100 && (o[1] & 0xC0) == 64;
        let benchmarking = o[0] == 198 && (o[1] & 0xFE) == 18;
        if a.is_private() || cgnat || benchmarking {
            return if self.allow_lan_trackers {
                Ok(())
            } else {
                Err(Refusal::PrivateNetwork)
            };
        }
        self.public()
    }

    fn check_v6(&self, a: Ipv6Addr) -> Result<(), Refusal> {
        if a.is_unspecified() {
            return Err(Refusal::Unspecified);
        }
        if a.is_loopback() {
            return if self.allow_loopback {
                Ok(())
            } else {
                Err(Refusal::Loopback)
            };
        }
        let s = a.segments();
        // Addresses that embed an IPv4 address are judged by it.
        if let Some(v4) = a.to_ipv4_mapped() {
            return self.check_v4(v4);
        }
        // IPv4-compatible (deprecated): ::a.b.c.d
        if s[..6] == [0; 6] {
            return self.check_v4(Ipv4Addr::new(
                (s[6] >> 8) as u8,
                s[6] as u8,
                (s[7] >> 8) as u8,
                s[7] as u8,
            ));
        }
        // NAT64 well-known prefix 64:ff9b::/96
        if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
            return self.check_v4(Ipv4Addr::new(
                (s[6] >> 8) as u8,
                s[6] as u8,
                (s[7] >> 8) as u8,
                s[7] as u8,
            ));
        }
        // 6to4 2002:AABB:CCDD::/48
        if s[0] == 0x2002 {
            return self.check_v4(Ipv4Addr::new(
                (s[1] >> 8) as u8,
                s[1] as u8,
                (s[2] >> 8) as u8,
                s[2] as u8,
            ));
        }
        if (s[0] & 0xff00) == 0xff00 {
            return Err(Refusal::Multicast);
        }
        if (s[0] & 0xffc0) == 0xfe80 {
            return Err(Refusal::LinkLocal);
        }
        // AWS IPv6 instance metadata
        if s == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254] {
            return Err(Refusal::Metadata);
        }
        // ULA fc00::/7, deprecated site-local fec0::/10, local-use NAT64
        let ula = (s[0] & 0xfe00) == 0xfc00;
        let site_local = (s[0] & 0xffc0) == 0xfec0;
        let nat64_local = s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1;
        if ula || site_local || nat64_local {
            return if self.allow_lan_trackers {
                Ok(())
            } else {
                Err(Refusal::PrivateNetwork)
            };
        }
        self.public()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn strict_policy_refuses_everything_local() {
        let p = DestinationPolicy::STRICT;
        let refused = [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.100.100.200",
            "168.63.129.16",
            "192.0.0.192",
            "224.0.0.1",
            "239.255.255.250",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "fd00:ec2::254",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
            "fec0::1",
        ];
        for a in refused {
            assert!(p.check(ip(a)).is_err(), "{a} must be refused");
        }
        let allowed = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ];
        for a in allowed {
            assert!(p.check(ip(a)).is_ok(), "{a} must be allowed");
        }
    }

    #[test]
    fn lan_opt_in_allows_private_but_never_link_local_or_metadata() {
        let p = DestinationPolicy {
            allow_lan_trackers: true,
            allow_loopback: false,
            allow_public: true,
        };
        for a in [
            "10.1.2.3",
            "172.20.0.5",
            "192.168.0.10",
            "100.64.1.1",
            "fd12::1",
            "::ffff:192.168.0.1",
        ] {
            assert!(p.check(ip(a)).is_ok(), "{a}");
        }
        for a in [
            "127.0.0.1",
            "::1",
            "169.254.169.254",
            "fe80::1",
            "100.100.100.200",
            "fd00:ec2::254",
            "0.0.0.0",
            "224.0.0.1",
        ] {
            assert!(p.check(ip(a)).is_err(), "{a} stays refused");
        }
    }

    #[test]
    fn loopback_opt_in_is_only_loopback() {
        let p = DestinationPolicy {
            allow_lan_trackers: false,
            allow_loopback: true,
            allow_public: true,
        };
        assert!(p.check(ip("127.0.0.1")).is_ok());
        assert!(p.check(ip("::1")).is_ok());
        assert!(p.check(ip("::ffff:127.0.0.1")).is_ok());
        assert!(p.check(ip("192.168.1.1")).is_err());
        assert!(p.check(ip("169.254.169.254")).is_err());
    }

    #[test]
    fn an_isolated_run_reaches_local_trackers_and_no_public_one() {
        let p = DestinationPolicy { allow_public: false, ..DestinationPolicy::LOCAL };
        for a in ["127.0.0.1", "::1", "192.168.1.10", "fd12::1"] {
            assert!(p.check(ip(a)).is_ok(), "{a}");
        }
        for a in ["38.145.60.21", "2606:4700::6810:84e5", "::ffff:8.8.8.8", "2002:0808:0808::1"] {
            assert_eq!(p.check(ip(a)), Err(Refusal::Public), "{a}");
        }
    }

    #[test]
    fn vet_filters_and_reports() {
        let p = DestinationPolicy::STRICT;
        let a: SocketAddr = "8.8.8.8:80".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:80".parse().unwrap();
        assert_eq!(p.vet([b, a]).unwrap(), vec![a]);
        assert_eq!(p.vet([b]).unwrap_err(), Refusal::Loopback);
        assert!(p.vet([]).is_err());
        assert_eq!(DestinationPolicy::LOCAL.vet([b, a]).unwrap(), vec![b, a]);
    }
}
