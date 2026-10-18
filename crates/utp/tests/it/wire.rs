//! Byte-level protocol conformance against hand-built libutp-style peers, plus abuse resistance.

use std::net::SocketAddr;

use trs_utp::packet::{self, Header, Packet, PacketType};
use trs_utp::rng::Rng;
use trs_utp::{Config, ConnStatus, Disposition, Endpoint};

const T0: u64 = 1_000_000;

fn a(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, last], port))
}

#[allow(clippy::too_many_arguments)]
fn pkt(ty: PacketType, conn_id: u16, seq: u16, ack: u16, wnd: u32, sack: &[u8], payload: &[u8]) -> Vec<u8> {
    let h = Header { ty, conn_id, timestamp: 5_000, timestamp_diff: 1_234, wnd_size: wnd, seq_nr: seq, ack_nr: ack };
    let mut buf = vec![0u8; 20 + 2 + sack.len() + payload.len()];
    let off = packet::encode(&h, sack, &mut buf).unwrap();
    buf[off..off + payload.len()].copy_from_slice(payload);
    buf.truncate(off + payload.len());
    buf
}

fn out(ep: &mut Endpoint, now: u64) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut v = Vec::new();
    let mut buf = vec![0u8; ep.transmit_buffer_len()];
    while let Some((to, n)) = ep.poll_transmit(now, &mut buf) {
        v.push((to, buf[..n].to_vec()));
    }
    v
}

fn parse(d: &[u8]) -> Packet<'_> {
    Packet::parse(d).expect("valid uTP packet")
}

fn cfg() -> Config {
    Config::default()
}

#[test]
fn acceptor_follows_libutp_handshake() {
    let mut ep = Endpoint::new(cfg(), 1);
    let peer = a(9, 40000);
    let c = 0x1234u16;
    // libutp initiator: SYN carries its recv id, seq_nr 1.
    let syn = pkt(PacketType::Syn, c, 1, 0, 1 << 20, &[], &[]);
    assert_eq!(ep.handle_datagram(T0, peer, &syn), Disposition::Utp);
    let replies = out(&mut ep, T0);
    assert_eq!(replies.len(), 1);
    let st = parse(&replies[0].1);
    assert_eq!(st.header.ty, PacketType::State);
    assert_eq!(st.header.conn_id, c, "acceptor sends with the SYN's connection id");
    assert_eq!(st.header.ack_nr, 1, "STATE acknowledges the SYN's seq_nr");
    assert_eq!(st.header.timestamp_diff, 0u32.wrapping_add(T0 as u32).wrapping_sub(5_000) , "echoes the SYN's timestamp delta");
    let s = st.header.seq_nr;
    assert_eq!(replies[0].1.len(), 20, "no payload/extension on the SYN-ACK");
    // Not accepted until the initiator proves it saw our STATE.
    assert!(ep.poll_accept().is_none());
    assert_eq!(ep.half_open(), 1);

    // Initiator's first data: connection id c+1, seq_nr 2, ack_nr = S - 1 (STATE consumed nothing).
    let data = pkt(PacketType::Data, c + 1, 2, s.wrapping_sub(1), 1 << 20, &[], b"hello");
    ep.handle_datagram(T0 + 10_000, peer, &data);
    let h = ep.poll_accept().expect("confirmed connection is accepted");
    assert_eq!(ep.half_open(), 0);
    let mut buf = [0u8; 16];
    assert_eq!(ep.read(h, T0 + 10_000, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");

    // Delayed ack goes out within the configured bound with ack_nr 2.
    assert!(out(&mut ep, T0 + 10_000).is_empty(), "single packet: ack is delayed");
    let t = ep.poll_timeout().expect("ack timer");
    assert!(t <= T0 + 10_000 + 25_000 + 1);
    ep.handle_timeout(t);
    let acks = out(&mut ep, t);
    assert_eq!(parse(&acks[0].1).header.ack_nr, 2);

    // Our first data packet reuses seq S (the STATE did not consume it).
    ep.write(h, t, b"world").unwrap();
    let d = out(&mut ep, t);
    let p = parse(&d[0].1);
    assert_eq!(p.header.ty, PacketType::Data);
    assert_eq!(p.header.conn_id, c);
    assert_eq!(p.header.seq_nr, s);
    assert_eq!(p.header.ack_nr, 2);
    assert_eq!(p.payload, b"world");
    assert_eq!(p.header.wnd_size, 512 * 1024);
}

#[test]
fn initiator_follows_libutp_handshake() {
    let mut ep = Endpoint::new(cfg(), 2);
    let peer = a(7, 6881);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    assert_eq!(syn.len(), 1);
    assert_eq!(syn[0].0, peer);
    let p = parse(&syn[0].1);
    assert_eq!(p.header.ty, PacketType::Syn);
    assert_eq!(p.header.seq_nr, 1, "SYN carries seq_nr 1");
    assert_eq!(syn[0].1[0], 0x41);
    assert_eq!(syn[0].1[1], 0, "no extension on the SYN");
    assert_eq!(syn[0].1.len(), 20);
    let x = p.header.conn_id;
    assert_eq!(ep.status(h), ConnStatus::Connecting);
    // Data written before the handshake completes waits.
    ep.write(h, T0, b"early").unwrap();
    assert!(out(&mut ep, T0).is_empty());

    // libutp acceptor: STATE with conn id = SYN's id, ack_nr 1, its own random seq_nr.
    let state = pkt(PacketType::State, x, 777, 1, 1 << 20, &[], &[]);
    ep.handle_datagram(T0 + 50_000, peer, &state);
    assert_eq!(ep.status(h), ConnStatus::Open);
    let d = out(&mut ep, T0 + 50_000);
    let p = parse(&d[0].1);
    assert_eq!(p.header.ty, PacketType::Data);
    assert_eq!(p.header.conn_id, x.wrapping_add(1), "after the SYN the initiator sends with recv_id + 1");
    assert_eq!(p.header.seq_nr, 2);
    assert_eq!(p.header.ack_nr, 776, "ack_nr starts at the acceptor's seq_nr - 1");
    assert_eq!(p.payload, b"early");

    // Acceptor's first data uses seq 777 and connection id x.
    let data = pkt(PacketType::Data, x, 777, 2, 1 << 20, &[], b"abc");
    ep.handle_datagram(T0 + 60_000, peer, &data);
    let mut buf = [0u8; 8];
    assert_eq!(ep.read(h, T0 + 60_000, &mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"abc");
}

#[test]
fn selective_ack_wire_format_and_fast_retransmit() {
    let mut ep = Endpoint::new(cfg(), 3);
    let peer = a(8, 1000);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0 + 1000, peer, &pkt(PacketType::State, x, 100, 1, 1 << 20, &[], &[]));

    // Receiving side: packets 100 (in order), then 102 and 104 (gaps at 101 and 103).
    ep.handle_datagram(T0 + 2000, peer, &pkt(PacketType::Data, x, 100, 2, 1 << 20, &[], b"a"));
    ep.handle_datagram(T0 + 3000, peer, &pkt(PacketType::Data, x, 102, 2, 1 << 20, &[], b"c"));
    ep.handle_datagram(T0 + 4000, peer, &pkt(PacketType::Data, x, 104, 2, 1 << 20, &[], b"e"));
    let acks = out(&mut ep, T0 + 4000);
    let last = parse(&acks.last().unwrap().1);
    assert_eq!(last.header.ack_nr, 100);
    let mask = last.sack.expect("SACK extension present while there are gaps");
    assert_eq!(mask.len() % 4, 0);
    // ack_nr+2 = 102 -> bit 0; 104 -> bit 2.
    assert_eq!(mask[0], 0b0000_0101);
    assert_eq!(acks.last().unwrap().1[1], 1, "extension byte announces the SACK");
    // Filling the first gap advances ack_nr to 102 and leaves the bit for 104 at offset 0.
    ep.handle_datagram(T0 + 5000, peer, &pkt(PacketType::Data, x, 101, 2, 1 << 20, &[], b"b"));
    let acks = out(&mut ep, T0 + 5000);
    let last = parse(&acks.last().unwrap().1);
    assert_eq!(last.header.ack_nr, 102);
    assert_eq!(last.sack.unwrap()[0], 0b0000_0001);
    let _ = h;

    // Sending side: 6 packets in flight (small MSS so we get several), peer SACKs 3 after the hole.
    let mut ep = Endpoint::new(Config { max_payload: 256, initial_window_packets: 16, ..cfg() }, 4);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0 + 1000, peer, &pkt(PacketType::State, x, 500, 1, 1 << 20, &[], &[]));
    ep.write(h, T0 + 1000, &[7u8; 256 * 6]).unwrap();
    let sent = out(&mut ep, T0 + 1000);
    let seqs: Vec<u16> = sent.iter().map(|(_, d)| parse(d).header.seq_nr).collect();
    assert_eq!(seqs, vec![2, 3, 4, 5, 6, 7]);
    // Peer got 2 and then 4,5,6 (hole at 3): ack_nr=2, mask bits for 4,5,6 = offsets 0,1,2.
    ep.handle_datagram(T0 + 30_000, peer, &pkt(PacketType::State, x, 500, 2, 1 << 20, &[0b0000_0111, 0, 0, 0], &[]));
    let re = out(&mut ep, T0 + 30_000);
    let retx: Vec<u16> = re.iter().map(|(_, d)| parse(d).header.seq_nr).collect();
    assert_eq!(retx, vec![3], "exactly the hole is retransmitted after 3 SACKs beyond it: {retx:?}");
    let st = ep.conn_stats(h).unwrap();
    assert_eq!(st.retransmits, 1);
}

#[test]
fn forged_and_stale_acks_cannot_move_the_window() {
    let mut ep = Endpoint::new(cfg(), 5);
    let peer = a(8, 1000);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0 + 1000, peer, &pkt(PacketType::State, x, 500, 1, 1 << 20, &[], &[]));
    ep.write(h, T0 + 1000, &[1u8; 3000]).unwrap();
    let _ = out(&mut ep, T0 + 1000);
    let before = ep.conn_stats(h).unwrap();
    assert!(before.in_flight > 0);
    // ack_nr far beyond anything sent
    ep.handle_datagram(T0 + 2000, peer, &pkt(PacketType::State, x, 500, 9000, 1 << 20, &[], &[]));
    // ack_nr from the distant past
    ep.handle_datagram(T0 + 2000, peer, &pkt(PacketType::State, x, 500, 60000, 1 << 20, &[], &[]));
    let after = ep.conn_stats(h).unwrap();
    assert_eq!(after.in_flight, before.in_flight);
    assert_eq!(after.cwnd, before.cwnd);
    // SACK bits pointing past the end of the flight are ignored
    ep.handle_datagram(T0 + 2000, peer, &pkt(PacketType::State, x, 500, 1, 1 << 20, &[0xff; 32], &[]));
    assert_eq!(ep.status(h), ConnStatus::Open);
}

#[test]
fn reset_requires_the_right_peer_and_id() {
    let mut ep = Endpoint::new(cfg(), 6);
    let peer = a(8, 1000);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0 + 1000, peer, &pkt(PacketType::State, x, 500, 1, 1 << 20, &[], &[]));
    // wrong source address
    ep.handle_datagram(T0 + 2000, a(66, 1000), &pkt(PacketType::Reset, x, 1, 1, 0, &[], &[]));
    // wrong connection id
    ep.handle_datagram(T0 + 2000, peer, &pkt(PacketType::Reset, x.wrapping_add(40), 1, 1, 0, &[], &[]));
    assert_eq!(ep.status(h), ConnStatus::Open);
    // libutp resets with the id it saw on the wire, which is our send id (recv + 1)
    ep.handle_datagram(T0 + 3000, peer, &pkt(PacketType::Reset, x.wrapping_add(1), 1, 1, 0, &[], &[]));
    assert_eq!(ep.status(h), ConnStatus::Closed(trs_utp::End::Reset));
    let mut b = [0u8; 4];
    assert_eq!(ep.read(h, T0 + 3000, &mut b).unwrap_err().kind(), std::io::ErrorKind::ConnectionReset);
    assert_eq!(ep.write(h, T0 + 3000, b"x").unwrap_err().kind(), std::io::ErrorKind::ConnectionReset);
}

#[test]
fn unknown_connection_gets_rate_limited_reset_and_never_a_reset_to_a_reset() {
    let mut ep = Endpoint::new(Config { resets_per_sec: 10, ..cfg() }, 7);
    let peer = a(5, 5);
    for i in 0..1000u16 {
        ep.handle_datagram(T0, peer, &pkt(PacketType::Data, i, i, 0, 100, &[], b"x"));
    }
    let mut total = 0;
    // The queue is tiny and bucketed: nowhere near 1000 replies.
    for (to, d) in out(&mut ep, T0) {
        assert_eq!(to, peer);
        assert_eq!(parse(&d).header.ty, PacketType::Reset);
        total += 1;
    }
    assert!(total <= 16, "{total} resets");
    // a stray RESET gets no answer
    ep.handle_datagram(T0 + 5_000_000, peer, &pkt(PacketType::Reset, 1, 1, 1, 0, &[], &[]));
    assert!(out(&mut ep, T0 + 5_000_000).is_empty());
}

#[test]
fn syn_flood_is_capped_and_never_amplified() {
    let c = Config { max_half_open: 50, max_connections: 100, max_per_ip: 8, ..cfg() };
    let mut ep = Endpoint::new(c, 8);
    let mut rng = Rng::new(99);
    let (mut bytes_in, mut bytes_out) = (0usize, 0usize);
    for i in 0..20_000u32 {
        let src = SocketAddr::from(([203, 0, (i >> 8) as u8, i as u8], 1024 + (rng.next_u64() % 60000) as u16));
        let syn = pkt(PacketType::Syn, rng.next_u16(), 1, 0, 1 << 20, &[], &[]);
        bytes_in += syn.len();
        ep.handle_datagram(T0, src, &syn);
        for (_, d) in out(&mut ep, T0) {
            bytes_out += d.len();
        }
    }
    assert!(ep.live_connections() <= 100);
    assert!(ep.half_open() <= 50);
    assert!(bytes_out <= bytes_in, "amplification: in {bytes_in} out {bytes_out}");
    // One address cannot hold more than max_per_ip connections
    let mut ep = Endpoint::new(Config { max_per_ip: 8, ..cfg() }, 9);
    let src = a(1, 4000);
    for i in 0..100u16 {
        ep.handle_datagram(T0, src, &pkt(PacketType::Syn, i * 10, 1, 0, 1 << 20, &[], &[]));
    }
    assert_eq!(ep.live_connections(), 8);
    let _ = out(&mut ep, T0);
    // Retransmitted SYNs (same id) re-trigger the SYN-ACK 1:1 but never create more state
    let before = ep.live_connections();
    let mut out_bytes = 0;
    for _ in 0..500 {
        ep.handle_datagram(T0 + 1, src, &pkt(PacketType::Syn, 0, 1, 0, 1 << 20, &[], &[]));
        for (_, d) in out(&mut ep, T0 + 1) {
            out_bytes += d.len();
        }
    }
    assert_eq!(ep.live_connections(), before);
    assert!(out_bytes <= 500 * 20, "{out_bytes}");
}

#[test]
fn unconfirmed_connections_expire_without_traffic() {
    let mut ep = Endpoint::new(cfg(), 10);
    ep.handle_datagram(T0, a(1, 1), &pkt(PacketType::Syn, 5, 1, 0, 1 << 20, &[], &[]));
    let _ = out(&mut ep, T0);
    assert_eq!(ep.half_open(), 1);
    let t = ep.poll_timeout().unwrap();
    assert!(t <= T0 + 11_000_000);
    ep.handle_timeout(t);
    assert_eq!(ep.half_open(), 0);
    assert_eq!(ep.live_connections(), 0);
    assert!(ep.poll_timeout().is_none());
}

#[test]
fn spoofed_confirmation_with_wrong_ack_is_rejected() {
    let mut ep = Endpoint::new(cfg(), 11);
    let src = a(1, 1);
    ep.handle_datagram(T0, src, &pkt(PacketType::Syn, 77, 1, 0, 1 << 20, &[], &[]));
    let st = out(&mut ep, T0);
    let s = parse(&st[0].1).header.seq_nr;
    // An off-path attacker that never saw S guesses wrong
    ep.handle_datagram(T0 + 1, src, &pkt(PacketType::Data, 78, 2, s.wrapping_add(5), 1 << 20, &[], b"junk"));
    assert!(ep.poll_accept().is_none());
    assert_eq!(ep.half_open(), 1);
}

#[test]
fn receiver_bounds_out_of_order_state() {
    let c = Config { max_reorder_packets: 32, recv_buffer: 64 * 1024, ..cfg() };
    let mut ep = Endpoint::new(c, 12);
    let peer = a(8, 1000);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0 + 1, peer, &pkt(PacketType::State, x, 100, 1, 1 << 20, &[], &[]));
    for i in 0..1000u16 {
        ep.handle_datagram(T0 + 2, peer, &pkt(PacketType::Data, x, 102 + i, 2, 1 << 20, &[], &[9u8; 1000]));
    }
    // only seq 100 is missing (hole), at most 32 packets of 1000 B can be held
    let held = ep.conn_heap_bytes(h);
    assert!(held <= 40 * 1000, "held {held} bytes");
    let mut b = [0u8; 8];
    assert_eq!(ep.read(h, T0 + 3, &mut b).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    // the sender violating the advertised window is dropped, not buffered
    ep.handle_datagram(T0 + 4, peer, &pkt(PacketType::Data, x, 100, 2, 1 << 20, &[], &[1u8; 1000]));
}

#[test]
fn foreign_datagrams_are_handed_back_untouched() {
    let mut ep = Endpoint::new(cfg(), 13);
    let dht = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
    assert_eq!(ep.handle_datagram(T0, a(1, 1), dht), Disposition::Foreign);
    let tracker_connect_reply = [0u8, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(ep.handle_datagram(T0, a(1, 1), &tracker_connect_reply), Disposition::Foreign);
    assert_eq!(ep.handle_datagram(T0, a(1, 1), &[]), Disposition::Foreign);
    assert!(out(&mut ep, T0).is_empty(), "foreign traffic must never provoke a reply");
    // A uTP-looking but malformed datagram is consumed silently
    let mut bad = pkt(PacketType::Data, 1, 1, 1, 1, &[], &[]);
    bad[1] = 1; // claims an extension that is not there
    assert_eq!(ep.handle_datagram(T0, a(1, 1), &bad), Disposition::Utp);
    assert!(out(&mut ep, T0).is_empty());
}

#[test]
fn random_garbage_and_mutations_never_panic() {
    let mut ep = Endpoint::new(Config { accept_backlog: 8, ..cfg() }, 14);
    let peer = a(8, 1000);
    let h = ep.connect(T0, peer).unwrap();
    let syn = out(&mut ep, T0);
    let x = parse(&syn[0].1).header.conn_id;
    ep.handle_datagram(T0, peer, &pkt(PacketType::State, x, 100, 1, 1 << 20, &[], &[]));
    ep.write(h, T0, &[3u8; 20_000]).unwrap();
    let mut rng = Rng::new(0xFEED);
    let mut now = T0;
    let seeds = [
        pkt(PacketType::Data, x, 100, 2, 1 << 20, &[], b"payload"),
        pkt(PacketType::State, x, 100, 3, 1 << 20, &[0xAA; 8], &[]),
        pkt(PacketType::Fin, x, 101, 2, 1 << 20, &[], &[]),
        pkt(PacketType::Syn, x.wrapping_add(7), 1, 0, 1 << 20, &[], &[]),
        pkt(PacketType::Reset, x ^ 1, 1, 1, 1, &[], &[]),
    ];
    let mut buf = vec![0u8; ep.transmit_buffer_len()];
    for i in 0..300_000u32 {
        let mut d = if rng.next_u64().is_multiple_of(3) {
            let n = (rng.next_u64() % 100) as usize;
            let mut v: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
            if !v.is_empty() {
                v[0] = (v[0] & 0xf0) | 1;
            }
            v
        } else {
            seeds[(rng.next_u64() % seeds.len() as u64) as usize].clone()
        };
        for _ in 0..(rng.next_u64() % 4) {
            if d.is_empty() {
                break;
            }
            let k = (rng.next_u64() % d.len() as u64) as usize;
            d[k] = rng.next_u64() as u8;
        }
        if rng.next_u64().is_multiple_of(8) {
            d.truncate((rng.next_u64() % (d.len() as u64 + 1)) as usize);
        }
        now += 1 + rng.next_u64() % 3000;
        let from = if rng.next_u64().is_multiple_of(4) { a(rng.next_u64() as u8, 2000) } else { peer };
        ep.handle_datagram(now, from, &d);
        if i % 16 == 0 {
            ep.handle_timeout(now);
        }
        while ep.poll_transmit(now, &mut buf).is_some() {}
        let _ = ep.poll_accept();
        let mut rb = [0u8; 64];
        let _ = ep.read(h, now, &mut rb);
        let _ = ep.write(h, now, &[1u8; 100]);
    }
}
