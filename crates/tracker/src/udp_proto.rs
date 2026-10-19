//! BEP 15 UDP tracker wire format and BEP 41 URL data - sans-I/O, panic-free
//! on any input, fuzzed. The socket logic lives in the `udp` transport.

use std::time::Duration;

use crate::response::{
    AnnounceResponse, MAX_PEERS, clamp_timing, push_compact_v4, push_compact_v6, sanitize_text,
};
use crate::scrape::{MAX_SCRAPE_HASHES, ScrapeEntry};
use crate::types::{AnnounceParams, InfoHash20};

/// The magic constant of a connect request.
pub const PROTOCOL_ID: u64 = 0x0417_2710_1980;
/// A connection id may be reused for this long (BEP 15).
pub const CONNECTION_ID_TTL: Duration = Duration::from_secs(60);
/// First retransmission wait (BEP 15: `15 * 2^n` seconds).
pub const BASE_TIMEOUT: Duration = Duration::from_secs(15);
/// BEP 15 allows `n` up to 8 (about 64 minutes). **Our scheduler caps `n` at
/// 4**, i.e. waits of 15, 30, 60, 120 and 240 s, 465 s in total per exchange:
/// beyond that the announce machine's own exponential backoff takes over, and
/// a UDP request must not pin one of a host's two request slots for an hour.
pub const MAX_EXPONENT: u32 = 4;
/// Longest BEP 41 URL data we send; longer paths are not sent at all (a
/// truncated path would be wrong).
pub const MAX_URL_DATA: usize = 1000;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_SCRAPE: u32 = 2;
const ACTION_ERROR: u32 = 3;

/// Wait before retransmission number `n` (0-based) with our cap on `n`.
pub fn retransmit_timeout(n: u32, base: Duration, max_exponent: u32) -> Duration {
    base.saturating_mul(1u32 << n.min(max_exponent).min(16))
}

/// Connect request.
pub fn encode_connect(tid: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    b[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    b[12..].copy_from_slice(&tid.to_be_bytes());
    b
}

/// Announce request with optional BEP 41 URL data (`path?query` of the
/// announce URL, which carries passkeys for UDP-capable private trackers).
pub fn encode_announce(
    connection_id: u64,
    tid: u32,
    p: &AnnounceParams,
    url_data: Option<&str>,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(98 + 16);
    b.extend_from_slice(&connection_id.to_be_bytes());
    b.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    b.extend_from_slice(&tid.to_be_bytes());
    b.extend_from_slice(&p.info_hash);
    b.extend_from_slice(&p.peer_id);
    b.extend_from_slice(&p.downloaded.to_be_bytes());
    b.extend_from_slice(&p.left.to_be_bytes());
    b.extend_from_slice(&p.uploaded.to_be_bytes());
    b.extend_from_slice(&p.event.map_or(0u32, |e| e.udp_code()).to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // ip: let the tracker use the source address
    b.extend_from_slice(&p.key.to_be_bytes());
    b.extend_from_slice(&i32::try_from(p.numwant).unwrap_or(i32::MAX).to_be_bytes());
    b.extend_from_slice(&p.port.to_be_bytes());
    if let Some(data) = url_data {
        let d = data.as_bytes();
        if !d.is_empty() && d != b"/" && d.len() <= MAX_URL_DATA {
            for chunk in d.chunks(255) {
                b.push(0x2); // URLData option
                b.push(chunk.len() as u8);
                b.extend_from_slice(chunk);
            }
            b.push(0x0); // EndOfOptions
        }
    }
    b
}

/// Scrape request (at most [`MAX_SCRAPE_HASHES`] hashes).
pub fn encode_scrape(connection_id: u64, tid: u32, hashes: &[InfoHash20]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + 20 * hashes.len());
    b.extend_from_slice(&connection_id.to_be_bytes());
    b.extend_from_slice(&ACTION_SCRAPE.to_be_bytes());
    b.extend_from_slice(&tid.to_be_bytes());
    for h in hashes.iter().take(MAX_SCRAPE_HASHES) {
        b.extend_from_slice(h);
    }
    b
}

/// A decoded tracker datagram.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// Connect response.
    Connect {
        /// Echoed transaction id.
        tid: u32,
        /// Connection id to use for up to a minute.
        connection_id: u64,
    },
    /// Announce response (interval clamped, peers capped).
    Announce {
        /// Echoed transaction id.
        tid: u32,
        /// Parsed response.
        response: Box<AnnounceResponse>,
    },
    /// Scrape response.
    Scrape {
        /// Echoed transaction id.
        tid: u32,
        /// One entry per requested hash, in order.
        entries: Vec<ScrapeEntry>,
    },
    /// The tracker refused (action 3).
    Error {
        /// Echoed transaction id.
        tid: u32,
        /// Sanitised message.
        message: String,
    },
}

impl Reply {
    /// Transaction id of any reply.
    pub fn tid(&self) -> u32 {
        match self {
            Reply::Connect { tid, .. }
            | Reply::Announce { tid, .. }
            | Reply::Scrape { tid, .. }
            | Reply::Error { tid, .. } => *tid,
        }
    }
}

/// Why a datagram could not be decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Shorter than the action requires.
    TooShort,
    /// Unknown action number.
    BadAction(u32),
}

/// Transaction id and action of a datagram, if it has the 8-byte header.
pub fn peek_header(buf: &[u8]) -> Option<(u32, u32)> {
    let action = u32::from_be_bytes(buf.get(0..4)?.try_into().ok()?);
    let tid = u32::from_be_bytes(buf.get(4..8)?.try_into().ok()?);
    Some((action, tid))
}

fn be32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

/// Decode a datagram. `v6` says whether the peers section uses 18-byte
/// entries (the datagram came over IPv6).
pub fn decode(buf: &[u8], v6: bool) -> Result<Reply, DecodeError> {
    let (action, tid) = peek_header(buf).ok_or(DecodeError::TooShort)?;
    match action {
        ACTION_CONNECT => {
            let id = u64::from_be_bytes(
                buf.get(8..16)
                    .ok_or(DecodeError::TooShort)?
                    .try_into()
                    .map_err(|_| DecodeError::TooShort)?,
            );
            Ok(Reply::Connect {
                tid,
                connection_id: id,
            })
        }
        ACTION_ANNOUNCE => {
            let interval = be32(buf, 8).ok_or(DecodeError::TooShort)?;
            let leechers = be32(buf, 12).ok_or(DecodeError::TooShort)?;
            let seeders = be32(buf, 16).ok_or(DecodeError::TooShort)?;
            let peers_raw = buf.get(20..).ok_or(DecodeError::TooShort)?;
            let mut peers = Vec::new();
            if v6 {
                push_compact_v6(&mut peers, peers_raw);
            } else {
                push_compact_v4(&mut peers, peers_raw);
            }
            peers.truncate(MAX_PEERS);
            // The wire field is unsigned; values above i32::MAX are absurd anyway.
            let timing = clamp_timing(Some(i64::from(interval)), None);
            Ok(Reply::Announce {
                tid,
                response: Box::new(AnnounceResponse {
                    interval: timing.interval,
                    min_interval: timing.min_interval,
                    tracker_id: None,
                    complete: Some(u64::from(seeders)),
                    incomplete: Some(u64::from(leechers)),
                    downloaded: None,
                    peers,
                    warning: None,
                    external_ip: None,
                }),
            })
        }
        ACTION_SCRAPE => {
            let body = buf.get(8..).ok_or(DecodeError::TooShort)?;
            let entries = body
                .as_chunks::<12>().0.iter()
                .take(MAX_SCRAPE_HASHES)
                .map(|c| ScrapeEntry {
                    complete: u64::from(u32::from_be_bytes([c[0], c[1], c[2], c[3]])),
                    downloaded: u64::from(u32::from_be_bytes([c[4], c[5], c[6], c[7]])),
                    incomplete: u64::from(u32::from_be_bytes([c[8], c[9], c[10], c[11]])),
                })
                .collect();
            Ok(Reply::Scrape { tid, entries })
        }
        ACTION_ERROR => Ok(Reply::Error {
            tid,
            message: sanitize_text(buf.get(8..).unwrap_or(&[])),
        }),
        other => Err(DecodeError::BadAction(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Event;

    fn params() -> AnnounceParams {
        AnnounceParams {
            info_hash: [0x11; 20],
            peer_id: [0x22; 20],
            port: 6881,
            uploaded: 7,
            downloaded: 8,
            corrupt: 0,
            left: 9,
            key: 0xCAFEBABE,
            event: Some(Event::Started),
            numwant: 50,
            tracker_id: None,
            ipv4: None,
            ipv6: None,
            crypto: Default::default(),
        }
    }

    #[test]
    fn connect_request_bytes() {
        let b = encode_connect(0x0102_0304);
        assert_eq!(
            b,
            [
                0, 0, 4, 0x17, 0x27, 0x10, 0x19, 0x80, 0, 0, 0, 0, 1, 2, 3, 4
            ]
        );
    }

    #[test]
    fn announce_request_layout() {
        let b = encode_announce(0xAABB_CCDD_1122_3344, 5, &params(), None);
        assert_eq!(b.len(), 98);
        assert_eq!(&b[0..8], &0xAABB_CCDD_1122_3344u64.to_be_bytes());
        assert_eq!(&b[8..12], &[0, 0, 0, 1]);
        assert_eq!(&b[12..16], &[0, 0, 0, 5]);
        assert_eq!(&b[16..36], &[0x11; 20]);
        assert_eq!(&b[36..56], &[0x22; 20]);
        assert_eq!(
            &b[56..64],
            &8u64.to_be_bytes(),
            "downloaded before left before uploaded"
        );
        assert_eq!(&b[64..72], &9u64.to_be_bytes());
        assert_eq!(&b[72..80], &7u64.to_be_bytes());
        assert_eq!(&b[80..84], &[0, 0, 0, 2], "started = 2");
        assert_eq!(&b[84..88], &[0, 0, 0, 0]);
        assert_eq!(&b[88..92], &0xCAFEBABEu32.to_be_bytes());
        assert_eq!(&b[92..96], &50i32.to_be_bytes());
        assert_eq!(&b[96..98], &6881u16.to_be_bytes());
    }

    #[test]
    fn event_codes() {
        let mut p = params();
        for (e, code) in [
            (None, 0u8),
            (Some(Event::Completed), 1),
            (Some(Event::Started), 2),
            (Some(Event::Stopped), 3),
        ] {
            p.event = e;
            assert_eq!(encode_announce(1, 1, &p, None)[83], code);
        }
    }

    #[test]
    fn bep41_url_data() {
        let b = encode_announce(1, 2, &params(), Some("/announce?passkey=abc"));
        assert_eq!(b.len(), 98 + 2 + 21 + 1);
        assert_eq!(&b[98..100], &[0x2, 21]);
        assert_eq!(&b[100..121], b"/announce?passkey=abc");
        assert_eq!(b[121], 0x0);
        // long data is split into 255-byte options and re-assembles
        let long = format!("/{}", "a".repeat(600));
        let b = encode_announce(1, 2, &params(), Some(&long));
        let mut at = 98;
        let mut got: Vec<u8> = Vec::new();
        while b[at] == 0x2 {
            let l = b[at + 1] as usize;
            got.extend(&b[at + 2..at + 2 + l]);
            at += 2 + l;
        }
        assert_eq!(got, long.as_bytes());
        assert_eq!(b[at], 0);
        // nothing to send, or too long to send honestly
        assert_eq!(encode_announce(1, 2, &params(), Some("/")).len(), 98);
        assert_eq!(encode_announce(1, 2, &params(), Some("")).len(), 98);
        assert_eq!(
            encode_announce(1, 2, &params(), Some(&"x".repeat(MAX_URL_DATA + 1))).len(),
            98
        );
    }

    #[test]
    fn decode_connect_and_error() {
        let mut b = vec![0, 0, 0, 0, 0, 0, 0, 9];
        b.extend(0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(
            decode(&b, false),
            Ok(Reply::Connect {
                tid: 9,
                connection_id: 0x1122_3344_5566_7788
            })
        );
        b.truncate(12);
        assert_eq!(decode(&b, false), Err(DecodeError::TooShort));
        let mut e = vec![0, 0, 0, 3, 0, 0, 0, 7];
        e.extend(b"torrent not\x00 found");
        assert_eq!(
            decode(&e, false),
            Ok(Reply::Error {
                tid: 7,
                message: "torrent not  found".into()
            })
        );
    }

    #[test]
    fn decode_announce_v4_and_v6() {
        let mut b = vec![0, 0, 0, 1, 0, 0, 0, 3];
        b.extend(1800u32.to_be_bytes());
        b.extend(4u32.to_be_bytes()); // leechers
        b.extend(9u32.to_be_bytes()); // seeders
        b.extend([10, 0, 0, 1, 0x1A, 0xE1, 10, 0, 0, 2, 0x1A, 0xE2, 1, 2, 3]);
        let Ok(Reply::Announce { tid, response }) = decode(&b, false) else {
            panic!()
        };
        assert_eq!(tid, 3);
        assert_eq!((response.complete, response.incomplete), (Some(9), Some(4)));
        assert_eq!(response.interval, Duration::from_secs(1800));
        assert_eq!(response.peers.len(), 2);
        assert_eq!(response.peers[1].addr, "10.0.0.2:6882".parse().unwrap());
        let mut b6 = b[..20].to_vec();
        b6.extend(
            "2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        b6.extend([0x1A, 0xE1]);
        let Ok(Reply::Announce { response, .. }) = decode(&b6, true) else {
            panic!()
        };
        assert_eq!(
            response.peers[0].addr,
            "[2001:db8::1]:6881".parse().unwrap()
        );
        // absurd intervals are clamped
        let mut b = b[..20].to_vec();
        b[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        let Ok(Reply::Announce { response, .. }) = decode(&b, false) else {
            panic!()
        };
        assert_eq!(response.interval, Duration::from_secs(86_400));
        b[8..12].copy_from_slice(&0u32.to_be_bytes());
        let Ok(Reply::Announce { response, .. }) = decode(&b, false) else {
            panic!()
        };
        assert_eq!(response.interval, Duration::from_secs(1800));
    }

    #[test]
    fn decode_scrape_and_garbage() {
        let mut b = vec![0, 0, 0, 2, 0, 0, 0, 1];
        for v in [5u32, 6, 7, 8, 9, 10] {
            b.extend(v.to_be_bytes());
        }
        let Ok(Reply::Scrape { entries, .. }) = decode(&b, false) else {
            panic!()
        };
        assert_eq!(
            entries,
            vec![
                ScrapeEntry {
                    complete: 5,
                    downloaded: 6,
                    incomplete: 7
                },
                ScrapeEntry {
                    complete: 8,
                    downloaded: 9,
                    incomplete: 10
                }
            ]
        );
        assert_eq!(decode(&[], false), Err(DecodeError::TooShort));
        assert_eq!(decode(&[0; 7], false), Err(DecodeError::TooShort));
        assert_eq!(
            decode(&[0, 0, 0, 9, 0, 0, 0, 0], false),
            Err(DecodeError::BadAction(9))
        );
        assert_eq!(
            decode(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0], false),
            Err(DecodeError::TooShort)
        );
        assert_eq!(peek_header(&[0, 0, 0, 1, 0, 0, 0, 7]), Some((1, 7)));
    }

    #[test]
    fn retransmit_schedule_is_15_times_two_to_the_n_capped() {
        let secs: Vec<u64> = (0..8)
            .map(|n| retransmit_timeout(n, BASE_TIMEOUT, MAX_EXPONENT).as_secs())
            .collect();
        assert_eq!(secs, [15, 30, 60, 120, 240, 240, 240, 240]);
        assert_eq!(
            (0..=MAX_EXPONENT)
                .map(|n| retransmit_timeout(n, BASE_TIMEOUT, MAX_EXPONENT).as_secs())
                .sum::<u64>(),
            465
        );
    }
}
