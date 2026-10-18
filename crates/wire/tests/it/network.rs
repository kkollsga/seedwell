//! Live test against the public Debian netinst swarm. `#[ignore]`d: run explicitly with
//!
//!   cargo test -p trs-wire --test network -- --ignored --nocapture
//!
//! Politeness: exactly ONE `started` and ONE `stopped` announce to the Debian tracker,
//! then at most `MAX_PEERS` short connections that only exchange handshakes, extended
//! handshakes and the `ut_metadata` pieces, then disconnect. Nothing is downloaded or
//! uploaded. Debian 13.7.0 amd64 netinst (an official, legal torrent).

use bytes::BytesMut;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime};
use trs_wire::*;

const INFO_HASH: [u8; 20] = hex20("7acf8fb590b2060dd9c3146ef770169d593433b0");
const TRACKER: &str = "bttracker.debian.org:6969";
const LEFT: u64 = 792_723_456;
const MAX_PEERS: usize = 12;

const fn hex20(s: &str) -> [u8; 20] {
    let b = s.as_bytes();
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}
const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("hex"),
    }
}

fn peer_id() -> [u8; 20] {
    let mut id = *b"-SX0001-000000000000";
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut x = t as u64 ^ (std::process::id() as u64) << 32 | 1;
    for b in &mut id[8..] {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = b'0' + (x % 10) as u8;
    }
    id
}

fn pct(b: &[u8]) -> String {
    b.iter().map(|x| format!("%{x:02x}")).collect()
}

fn announce(event: &str, numwant: u32, pid: &[u8; 20]) -> Vec<u8> {
    let addr = TRACKER.to_socket_addrs().unwrap().next().unwrap();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let req = format!(
        "GET /announce?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={LEFT}&compact=1&numwant={numwant}&event={event} HTTP/1.0\r\nHost: bttracker.debian.org:6969\r\nUser-Agent: Seedwell/0.0.1\r\nConnection: close\r\n\r\n",
        pct(&INFO_HASH),
        pct(pid)
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut body = Vec::new();
    s.read_to_end(&mut body).unwrap();
    body
}

fn compact_peers(resp: &[u8]) -> Vec<SocketAddr> {
    let Some(i) = resp.windows(7).position(|w| w == b"5:peers") else {
        return vec![];
    };
    let rest = &resp[i + 7..];
    let Some(colon) = rest.iter().position(|&c| c == b':') else {
        return vec![];
    };
    let Ok(n) = std::str::from_utf8(&rest[..colon])
        .unwrap_or("x")
        .parse::<usize>()
    else {
        return vec![];
    };
    let data = &rest[colon + 1..];
    data.get(..n)
        .unwrap_or(data)
        .as_chunks::<6>().0.iter()
        .map(|c| SocketAddr::from(([c[0], c[1], c[2], c[3]], u16::from_be_bytes([c[4], c[5]]))))
        .collect()
}

#[derive(Debug, Default)]
struct Outcome {
    addr: Option<SocketAddr>,
    client: Option<String>,
    ext: bool,
    fast: bool,
    metadata_verified: bool,
    kinds: Vec<&'static str>,
    error: Option<String>,
    notes: Vec<String>,
}

fn kind(m: &OwnedMessage) -> &'static str {
    match m {
        Message::Bitfield(_) => "bitfield",
        Message::HaveAll => "have_all",
        Message::HaveNone => "have_none",
        Message::Have(_) => "have",
        Message::Extended { .. } => "extended",
        Message::Unchoke => "unchoke",
        Message::Choke => "choke",
        Message::Port(_) => "port",
        Message::AllowedFast(_) => "allowed_fast",
        Message::KeepAlive => "keepalive",
        _ => "other",
    }
}

fn talk(addr: SocketAddr, pid: [u8; 20]) -> Outcome {
    let mut out = Outcome {
        addr: Some(addr),
        ..Default::default()
    };
    if let Err(e) = talk_inner(addr, pid, &mut out) {
        out.error = Some(e);
    }
    out
}

fn talk_inner(addr: SocketAddr, pid: [u8; 20], out: &mut Outcome) -> Result<(), String> {
    let e = |x: &dyn std::fmt::Display| x.to_string();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|x| e(&x))?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let ours = Handshake::new(INFO_HASH, pid, false);
    s.write_all(&ours.to_bytes()).map_err(|x| e(&x))?;
    let policy = ExtPolicy::for_torrent(false);
    let mut enc = Encoder::new();
    let hs = LocalExtendedHandshake::new("0.0.1", policy);
    enc.push_extended(0, |b| hs.encode_payload(b));
    s.write_all(enc.pending()).map_err(|x| e(&x))?;
    let n = enc.pending().len();
    enc.advance(n);

    let deadline = Instant::now() + Duration::from_secs(12);
    let mut rx = BytesMut::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let mut remote: Option<Handshake> = None;
    let mut dec = Decoder::new(Limits::default());
    let mut state: Option<PeerState> = None;
    let mut asm: Option<MetadataAssembler> = None;
    while Instant::now() < deadline {
        match s.read(&mut chunk) {
            Ok(0) => return Err("closed".into()),
            Ok(n) => rx.extend_from_slice(&chunk[..n]),
            Err(x)
                if matches!(
                    x.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(x) => return Err(e(&x)),
        }
        if remote.is_none() {
            match Handshake::decode(&rx).map_err(|x| e(&x))? {
                None => continue,
                Some(h) => {
                    let _ = rx.split_to(HANDSHAKE_LEN);
                    if h.info_hash != INFO_HASH {
                        return Err("wrong info hash".into());
                    }
                    out.ext = h.reserved.extension_protocol();
                    out.fast = h.reserved.fast();
                    dec = Decoder::new(Limits::default()).negotiated(&ours.reserved, &h.reserved);
                    state = Some(PeerState::new(h.reserved));
                    remote = Some(h);
                }
            }
        }
        while let Some(m) = dec.decode(&mut rx).map_err(|x| e(&x))? {
            if out.kinds.len() < 12 && !out.kinds.contains(&kind(&m)) {
                out.kinds.push(kind(&m));
            }
            let Message::Extended { id, payload } = &m else {
                continue;
            };
            let Some(st) = state.as_mut() else { continue };
            match ExtendedMessage::parse(*id, payload, policy) {
                Ok(ExtendedMessage::Handshake(h)) => {
                    st.apply_extended_handshake(&h);
                    out.client = st.remote_client.map(|c| c.to_string_lossy());
                    let want = st.remote_ut_metadata != 0
                        && (1..=1_000_000).contains(&st.remote_metadata_size);
                    if let (true, Ok(a)) = (
                        want,
                        MetadataAssembler::new(INFO_HASH, st.remote_metadata_size),
                    ) {
                        for p in a.missing() {
                            enc.push_extended(st.remote_ut_metadata, |b| {
                                MetadataMessage::Request { piece: p }.encode_payload(b)
                            });
                        }
                        s.write_all(enc.pending()).map_err(|x| e(&x))?;
                        let n = enc.pending().len();
                        enc.advance(n);
                        asm = Some(a);
                    } else {
                        return Ok(()); // got what we came for
                    }
                }
                Ok(ExtendedMessage::Metadata(MetadataMessage::Data { piece, data, .. })) => {
                    if let Some(a) = asm.as_mut() {
                        a.insert(piece, data).map_err(|x| e(&x))?;
                        if a.is_complete() {
                            a.finish().map_err(|x| e(&x))?;
                            out.metadata_verified = true;
                            return Ok(());
                        }
                    }
                }
                Ok(ExtendedMessage::Metadata(MetadataMessage::Reject { .. })) => {
                    out.notes.push("metadata rejected".into());
                }
                Ok(_) => {}
                Err(x) => out.notes.push(format!("ext {id}: {x}")),
            }
        }
    }
    if out.client.is_some() {
        Ok(())
    } else {
        Err("timeout before ext handshake".into())
    }
}

#[test]
#[ignore = "network: contacts the Debian tracker and swarm"]
fn debian_swarm_extended_handshake_and_metadata() {
    let pid = peer_id();
    let resp = announce("started", 30, &pid);
    let mut peers = compact_peers(&resp);
    assert!(
        !peers.is_empty(),
        "tracker returned no peers: {:?}",
        String::from_utf8_lossy(&resp[..resp.len().min(300)])
    );
    // Say goodbye promptly, as a polite client does, then talk to the peers.
    let _ = announce("stopped", 0, &pid);
    peers.truncate(MAX_PEERS);
    println!(
        "tracker gave {} peers; contacting {}",
        peers.len(),
        peers.len()
    );
    let handles: Vec<_> = peers
        .into_iter()
        .map(|a| std::thread::spawn(move || talk(a, pid)))
        .collect();
    let results: Vec<Outcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let mut clients = std::collections::BTreeMap::<String, u32>::new();
    for r in &results {
        println!(
            "{:?} client={:?} ext={} fast={} metadata_verified={} msgs={:?} err={:?} notes={:?}",
            r.addr, r.client, r.ext, r.fast, r.metadata_verified, r.kinds, r.error, r.notes
        );
        if let Some(c) = &r.client {
            *clients.entry(c.clone()).or_default() += 1;
        }
    }
    println!("client strings seen: {clients:?}");
    assert!(
        results.iter().any(|r| r.client.is_some()),
        "no peer completed an extended handshake"
    );
}
