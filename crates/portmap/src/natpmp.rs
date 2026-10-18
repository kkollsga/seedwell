//! NAT-PMP (RFC 6886) packet codec. Pure functions; every input is untrusted.

use std::net::Ipv4Addr;

use crate::Protocol;

/// UDP port the gateway listens on.
pub const PORT: u16 = 5351;
/// Initial retransmission delay (RFC 6886 section 3.1).
pub const INITIAL_RETRY_MS: u64 = 250;
/// Maximum number of transmissions of one request (RFC 6886 section 3.1).
pub const MAX_TRIES: u8 = 9;
/// Datagrams longer than this are not NAT-PMP responses.
pub const MAX_RESPONSE: usize = 64;

/// Result code of a response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultCode {
    /// 0
    Success,
    /// 1: the gateway does not speak this version.
    UnsupportedVersion,
    /// 2: mapping is administratively disabled or the client is not allowed.
    NotAuthorized,
    /// 3: the gateway has no (working) external network.
    NetworkFailure,
    /// 4: the gateway ran out of mappings.
    OutOfResources,
    /// 5
    UnsupportedOpcode,
    /// Anything else.
    Other(u16),
}

impl ResultCode {
    /// From the wire value.
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Success,
            1 => Self::UnsupportedVersion,
            2 => Self::NotAuthorized,
            3 => Self::NetworkFailure,
            4 => Self::OutOfResources,
            5 => Self::UnsupportedOpcode,
            o => Self::Other(o),
        }
    }
}

/// A decoded response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Response {
    /// Answer to the external-address request.
    ExternalAddress {
        /// Seconds since the gateway's mapping table started.
        epoch: u32,
        /// `None` when the gateway reports `0.0.0.0`.
        address: Option<Ipv4Addr>,
    },
    /// Answer to a mapping request (also to a delete: `lifetime == 0`).
    Mapping {
        /// Protocol of the request.
        protocol: Protocol,
        /// Gateway epoch.
        epoch: u32,
        /// Our internal port, echoed.
        internal_port: u16,
        /// Port the gateway mapped.
        external_port: u16,
        /// Granted lifetime in seconds.
        lifetime: u32,
    },
    /// A non-zero result code. `opcode` is the request's opcode (0, 1 or 2).
    Failure {
        /// Request opcode.
        opcode: u8,
        /// Result.
        code: ResultCode,
        /// Gateway epoch.
        epoch: u32,
    },
}

/// Why a datagram is not a usable response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Shorter than the fixed layout.
    TooShort,
    /// Longer than [`MAX_RESPONSE`].
    TooLong,
    /// Version byte is not 0.
    BadVersion(u8),
    /// Response bit not set (this is a request).
    NotAResponse,
    /// Opcode other than 0, 1, 2.
    BadOpcode(u8),
}

/// External-address request (opcode 0).
pub fn encode_external_address_request() -> [u8; 2] {
    [0, 0]
}

/// Mapping request (opcode 1 UDP / 2 TCP). A delete is lifetime 0 with external port 0.
pub fn encode_map_request(
    protocol: Protocol,
    internal_port: u16,
    suggested_external: u16,
    lifetime: u32,
) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[1] = match protocol {
        Protocol::Udp => 1,
        Protocol::Tcp => 2,
    };
    b[4..6].copy_from_slice(&internal_port.to_be_bytes());
    b[6..8].copy_from_slice(&suggested_external.to_be_bytes());
    b[8..12].copy_from_slice(&lifetime.to_be_bytes());
    b
}

/// Decode a datagram from the gateway.
pub fn decode_response(d: &[u8]) -> Result<Response, DecodeError> {
    if d.len() > MAX_RESPONSE {
        return Err(DecodeError::TooLong);
    }
    let [version, op, r0, r1, e0, e1, e2, e3, rest @ ..] = d else {
        return Err(DecodeError::TooShort);
    };
    if *version != 0 {
        return Err(DecodeError::BadVersion(*version));
    }
    if op & 0x80 == 0 {
        return Err(DecodeError::NotAResponse);
    }
    let opcode = op & 0x7f;
    if opcode > 2 {
        return Err(DecodeError::BadOpcode(opcode));
    }
    let code = ResultCode::from_u16(u16::from_be_bytes([*r0, *r1]));
    let epoch = u32::from_be_bytes([*e0, *e1, *e2, *e3]);
    if code != ResultCode::Success {
        return Ok(Response::Failure {
            opcode,
            code,
            epoch,
        });
    }
    match (opcode, rest) {
        (0, [a, b, c, dd, ..]) => {
            let ip = Ipv4Addr::new(*a, *b, *c, *dd);
            Ok(Response::ExternalAddress {
                epoch,
                address: (!ip.is_unspecified()).then_some(ip),
            })
        }
        (1 | 2, [i0, i1, x0, x1, l0, l1, l2, l3, ..]) => Ok(Response::Mapping {
            protocol: if opcode == 1 {
                Protocol::Udp
            } else {
                Protocol::Tcp
            },
            epoch,
            internal_port: u16::from_be_bytes([*i0, *i1]),
            external_port: u16::from_be_bytes([*x0, *x1]),
            lifetime: u32::from_be_bytes([*l0, *l1, *l2, *l3]),
        }),
        _ => Err(DecodeError::TooShort),
    }
}

/// Did the gateway lose its mapping table? RFC 6886 section 3.6: the epoch must advance
/// at least at 7/8 of our clock rate (2 s of slack for rounding); anything less, or a
/// smaller epoch, means a reboot or reset.
pub fn epoch_reset(prev_epoch: u32, prev_at_ms: u64, new_epoch: u32, now_ms: u64) -> bool {
    if new_epoch < prev_epoch {
        return true;
    }
    let gateway_ms = u64::from(new_epoch - prev_epoch) * 1000;
    let local_ms = now_ms.saturating_sub(prev_at_ms);
    gateway_ms + 2000 < local_ms / 8 * 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout_matches_rfc() {
        assert_eq!(encode_external_address_request(), [0, 0]);
        assert_eq!(
            encode_map_request(Protocol::Tcp, 6881, 6882, 7200),
            [0, 2, 0, 0, 0x1a, 0xe1, 0x1a, 0xe2, 0, 0, 0x1c, 0x20]
        );
        assert_eq!(
            encode_map_request(Protocol::Udp, 1, 0, 0),
            [0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn decodes_each_response() {
        let ext = [0, 128, 0, 0, 0, 0, 0, 5, 203, 0, 113, 9];
        assert_eq!(
            decode_response(&ext),
            Ok(Response::ExternalAddress {
                epoch: 5,
                address: Some(Ipv4Addr::new(203, 0, 113, 9))
            })
        );
        let mut z = ext;
        z[8..].fill(0);
        assert!(matches!(
            decode_response(&z),
            Ok(Response::ExternalAddress { address: None, .. })
        ));
        let map = [
            0, 130, 0, 0, 0, 0, 1, 0, 0x1a, 0xe1, 0x1a, 0xe3, 0, 0, 0x1c, 0x20,
        ];
        assert_eq!(
            decode_response(&map),
            Ok(Response::Mapping {
                protocol: Protocol::Tcp,
                epoch: 256,
                internal_port: 6881,
                external_port: 6883,
                lifetime: 7200
            })
        );
        // failures carry only the 8-byte header
        let fail = [0, 129, 0, 2, 0, 0, 0, 9];
        assert_eq!(
            decode_response(&fail),
            Ok(Response::Failure {
                opcode: 1,
                code: ResultCode::NotAuthorized,
                epoch: 9
            })
        );
        let unsupported = [0, 128, 0, 1, 0, 0, 0, 0];
        assert!(matches!(
            decode_response(&unsupported),
            Ok(Response::Failure {
                code: ResultCode::UnsupportedVersion,
                ..
            })
        ));
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert_eq!(decode_response(&[]), Err(DecodeError::TooShort));
        assert_eq!(decode_response(&[0; 7]), Err(DecodeError::TooShort));
        assert_eq!(
            decode_response(&[1, 128, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::BadVersion(1))
        );
        assert_eq!(
            decode_response(&[0, 2, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::NotAResponse)
        );
        assert_eq!(
            decode_response(&[0, 131, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::BadOpcode(3))
        );
        // success but truncated body
        assert_eq!(
            decode_response(&[0, 129, 0, 0, 0, 0, 0, 0, 1, 2]),
            Err(DecodeError::TooShort)
        );
        assert_eq!(decode_response(&[0; 65]), Err(DecodeError::TooLong));
        for len in 0..40 {
            for fill in [0u8, 0x80, 0xff] {
                let _ = decode_response(&vec![fill; len]);
            }
        }
    }

    #[test]
    fn epoch_rule() {
        // gateway kept pace
        assert!(!epoch_reset(1000, 0, 1060, 60_000));
        // within the 7/8 + 2 s tolerance
        assert!(!epoch_reset(1000, 0, 1051, 60_000));
        assert!(epoch_reset(1000, 0, 1050, 60_000));
        // epoch went backwards
        assert!(epoch_reset(1000, 0, 999, 60_000));
        // epoch barely moved while a minute passed: rebooted
        assert!(epoch_reset(1000, 0, 1003, 60_000));
        // equal epoch after a long wait is a reset too
        assert!(epoch_reset(5, 0, 5, 3_600_000));
        // right after the previous answer nothing can be concluded
        assert!(!epoch_reset(5, 0, 5, 1_000));
    }
}
