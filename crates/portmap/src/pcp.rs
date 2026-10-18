//! PCP (RFC 6887) codec for the MAP opcode over IPv4. Pure functions; every input
//! is untrusted.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::Protocol;

/// UDP port the PCP server listens on (same as NAT-PMP).
pub const PORT: u16 = 5351;
/// PCP version.
pub const VERSION: u8 = 2;
/// Initial retransmission timeout, RFC 6887 section 8.1.1.
pub const INITIAL_RETRY_MS: u64 = 3000;
/// Size of a MAP request and response.
pub const MAP_LEN: usize = 60;
/// Longest datagram we look at (RFC 6887 caps PCP messages at 1100 bytes).
pub const MAX_RESPONSE: usize = 1100;

/// PCP result code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultCode {
    /// 0
    Success,
    /// 1: server speaks another version.
    UnsupportedVersion,
    /// 2: not authorized.
    NotAuthorized,
    /// 3
    MalformedRequest,
    /// 4
    UnsupportedOpcode,
    /// 5
    UnsupportedOption,
    /// 6
    MalformedOption,
    /// 7: no usable external network; retry later.
    NetworkFailure,
    /// 8: out of mappings; retry later.
    NoResources,
    /// 9
    UnsupportedProtocol,
    /// 10: per-user quota reached.
    UserExQuota,
    /// 11
    CannotProvideExternal,
    /// 12: the client address in the header is not our source address.
    AddressMismatch,
    /// 13
    ExcessiveRemotePeers,
    /// Anything else.
    Other(u8),
}

impl ResultCode {
    /// From the wire value.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Success,
            1 => Self::UnsupportedVersion,
            2 => Self::NotAuthorized,
            3 => Self::MalformedRequest,
            4 => Self::UnsupportedOpcode,
            5 => Self::UnsupportedOption,
            6 => Self::MalformedOption,
            7 => Self::NetworkFailure,
            8 => Self::NoResources,
            9 => Self::UnsupportedProtocol,
            10 => Self::UserExQuota,
            11 => Self::CannotProvideExternal,
            12 => Self::AddressMismatch,
            13 => Self::ExcessiveRemotePeers,
            o => Self::Other(o),
        }
    }
}

/// A decoded MAP response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapResponse {
    /// Granted lifetime in seconds.
    pub lifetime: u32,
    /// Server epoch.
    pub epoch: u32,
    /// Echo of our nonce.
    pub nonce: [u8; 12],
    /// Echo of the protocol number (6 TCP, 17 UDP).
    pub protocol: u8,
    /// Echo of our internal port.
    pub internal_port: u16,
    /// Assigned external port.
    pub external_port: u16,
    /// Assigned external address (IPv4-mapped addresses are unwrapped).
    pub external_ip: IpAddr,
}

/// A decoded response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Response {
    /// Successful MAP.
    Map(MapResponse),
    /// Non-zero result. `lifetime` is the server's "retry no sooner than" hint.
    Failure {
        /// Request opcode.
        opcode: u8,
        /// Result.
        code: ResultCode,
        /// Retry hint in seconds.
        lifetime: u32,
        /// Server epoch.
        epoch: u32,
        /// Echo of our nonce, when the server included the MAP block.
        nonce: Option<[u8; 12]>,
    },
}

/// Why a datagram is not a usable response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Shorter than the fixed layout.
    TooShort,
    /// Longer than [`MAX_RESPONSE`] or not a multiple of 4.
    BadLength,
    /// The server answered in NAT-PMP format (version 0): it is a NAT-PMP-only gateway.
    LegacyNatPmpServer,
    /// Version other than 2 (or 0, see above).
    BadVersion(u8),
    /// Response bit not set.
    NotAResponse,
    /// Opcode other than MAP.
    BadOpcode(u8),
}

fn v4_mapped(ip: Ipv4Addr) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[10] = 0xff;
    b[11] = 0xff;
    b[12..].copy_from_slice(&ip.octets());
    b
}

/// MAP request. `lifetime == 0` deletes the mapping with this `nonce`, `protocol` and
/// `internal_port`.
pub fn encode_map_request(
    client_ip: Ipv4Addr,
    nonce: [u8; 12],
    protocol: Protocol,
    internal_port: u16,
    suggested_external: u16,
    lifetime: u32,
) -> [u8; MAP_LEN] {
    let mut b = [0u8; MAP_LEN];
    b[0] = VERSION;
    b[1] = 1; // R = 0 (request), opcode MAP
    b[4..8].copy_from_slice(&lifetime.to_be_bytes());
    b[8..24].copy_from_slice(&v4_mapped(client_ip));
    b[24..36].copy_from_slice(&nonce);
    b[36] = protocol.number();
    b[40..42].copy_from_slice(&internal_port.to_be_bytes());
    b[42..44].copy_from_slice(&suggested_external.to_be_bytes());
    // suggested external address stays all-zero: no preference.
    b
}

/// Decode a datagram from the server.
pub fn decode_response(d: &[u8]) -> Result<Response, DecodeError> {
    if d.len() < 8 {
        return Err(DecodeError::TooShort);
    }
    // A NAT-PMP-only gateway answers a version-2 request with a version-0 packet
    // (RFC 6887 section 9). Report that distinctly so discovery can fall back.
    if d[0] == 0 {
        return Err(DecodeError::LegacyNatPmpServer);
    }
    if d[0] != VERSION {
        return Err(DecodeError::BadVersion(d[0]));
    }
    if d.len() < 24 {
        return Err(DecodeError::TooShort);
    }
    if d.len() > MAX_RESPONSE || !d.len().is_multiple_of(4) {
        return Err(DecodeError::BadLength);
    }
    if d[1] & 0x80 == 0 {
        return Err(DecodeError::NotAResponse);
    }
    let opcode = d[1] & 0x7f;
    if opcode != 1 {
        return Err(DecodeError::BadOpcode(opcode));
    }
    let code = ResultCode::from_u8(d[3]);
    let lifetime = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    let epoch = u32::from_be_bytes([d[8], d[9], d[10], d[11]]);
    if code != ResultCode::Success {
        let nonce = d.get(24..36).and_then(|n| <[u8; 12]>::try_from(n).ok());
        return Ok(Response::Failure {
            opcode,
            code,
            lifetime,
            epoch,
            nonce,
        });
    }
    if d.len() < MAP_LEN {
        return Err(DecodeError::TooShort);
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&d[24..36]);
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&d[44..60]);
    let v6 = Ipv6Addr::from(raw);
    let external_ip = match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    };
    Ok(Response::Map(MapResponse {
        lifetime,
        epoch,
        nonce,
        protocol: d[36],
        internal_port: u16::from_be_bytes([d[40], d[41]]),
        external_port: u16::from_be_bytes([d[42], d[43]]),
        external_ip,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn request_layout_matches_rfc() {
        let r = encode_map_request(
            Ipv4Addr::new(192, 168, 1, 20),
            NONCE,
            Protocol::Tcp,
            6881,
            6882,
            7200,
        );
        assert_eq!(r.len(), 60);
        assert_eq!(&r[..8], &[2, 1, 0, 0, 0, 0, 0x1c, 0x20]);
        assert_eq!(&r[8..18], &[0; 10]);
        assert_eq!(&r[18..24], &[0xff, 0xff, 192, 168, 1, 20]);
        assert_eq!(&r[24..36], &NONCE);
        assert_eq!(&r[36..44], &[6, 0, 0, 0, 0x1a, 0xe1, 0x1a, 0xe2]);
        assert_eq!(&r[44..], &[0; 16]);
    }

    fn response(code: u8, lifetime: u32, epoch: u32, ext: Ipv4Addr) -> Vec<u8> {
        let mut b = vec![0u8; 60];
        b[0] = 2;
        b[1] = 0x81;
        b[3] = code;
        b[4..8].copy_from_slice(&lifetime.to_be_bytes());
        b[8..12].copy_from_slice(&epoch.to_be_bytes());
        b[24..36].copy_from_slice(&NONCE);
        b[36] = 17;
        b[40..42].copy_from_slice(&6881u16.to_be_bytes());
        b[42..44].copy_from_slice(&40000u16.to_be_bytes());
        b[44..60].copy_from_slice(&v4_mapped(ext));
        b
    }

    #[test]
    fn decodes_success_and_failure() {
        let ok = response(0, 7200, 99, Ipv4Addr::new(203, 0, 113, 7));
        assert_eq!(
            decode_response(&ok),
            Ok(Response::Map(MapResponse {
                lifetime: 7200,
                epoch: 99,
                nonce: NONCE,
                protocol: 17,
                internal_port: 6881,
                external_port: 40000,
                external_ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)),
            }))
        );
        let bad = response(8, 300, 99, Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            decode_response(&bad),
            Ok(Response::Failure {
                opcode: 1,
                code: ResultCode::NoResources,
                lifetime: 300,
                epoch: 99,
                nonce: Some(NONCE)
            })
        );
        // failure with only the 24-byte header is fine
        assert!(matches!(
            decode_response(&bad[..24]),
            Ok(Response::Failure { .. })
        ));
        // success needs the whole MAP block
        let ok24 = &response(0, 1, 1, Ipv4Addr::LOCALHOST)[..24];
        assert_eq!(decode_response(ok24), Err(DecodeError::TooShort));
    }

    #[test]
    fn nat_pmp_only_gateways_are_recognised() {
        assert_eq!(
            decode_response(&[0, 129, 0, 1, 0, 0, 0, 7]),
            Err(DecodeError::LegacyNatPmpServer)
        );
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert_eq!(decode_response(&[]), Err(DecodeError::TooShort));
        assert_eq!(decode_response(&[2; 7]), Err(DecodeError::TooShort));
        assert_eq!(decode_response(&[3; 60]), Err(DecodeError::BadVersion(3)));
        assert_eq!(decode_response(&[2; 22]), Err(DecodeError::TooShort));
        let mut r = response(0, 1, 1, Ipv4Addr::LOCALHOST);
        r[1] = 1; // request, not response
        assert_eq!(decode_response(&r), Err(DecodeError::NotAResponse));
        r[1] = 0x82;
        assert_eq!(decode_response(&r), Err(DecodeError::BadOpcode(2)));
        assert_eq!(decode_response(&[2; 1101]), Err(DecodeError::BadLength));
        assert_eq!(decode_response(&[2; 61]), Err(DecodeError::BadLength));
        for len in 0..80 {
            for fill in [0u8, 2, 0x81, 0xff] {
                let _ = decode_response(&vec![fill; len]);
            }
        }
    }

    #[test]
    fn ipv6_external_address_is_kept() {
        let mut r = response(0, 1, 1, Ipv4Addr::LOCALHOST);
        r[44..60].copy_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets());
        let Ok(Response::Map(m)) = decode_response(&r) else {
            panic!()
        };
        assert_eq!(m.external_ip, "2001:db8::1".parse::<IpAddr>().unwrap());
    }
}
