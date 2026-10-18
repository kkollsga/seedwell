//! Behavioural tests of the sans-I/O core with hand-crafted packets and a hand-driven clock.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};
use trs_dht::codec::{self, Body, Envelope, Query, QueryOut, ResponseOut};
use trs_dht::{Config, DhtCore, Event, Id, Input, Intent, PublicInfoHash, Refused};

const ME: Id = [0x42; 20];

fn peer(n: u32) -> SocketAddrV4 {
    SocketAddrV4::new(
        Ipv4Addr::new(20 + (n >> 16) as u8, (n >> 8) as u8, n as u8, 7),
        6881,
    )
}

/// Ids spread over ~30 buckets relative to ME so many contacts fit in the routing table.
fn nid(n: u32) -> Id {
    let mut id = ME;
    let bit = (n % 30) as usize;
    id[bit / 8] ^= 0x80 >> (bit % 8);
    id[10..14].copy_from_slice(&n.wrapping_mul(0x9E37_79B1).to_be_bytes());
    id[14..18].copy_from_slice(&n.to_be_bytes());
    id
}

struct H {
    core: DhtCore,
    base: Instant,
}

impl H {
    fn with(cfg: Config) -> Self {
        let base = Instant::now();
        let cfg = Config {
            seed: Some(1),
            initial_node_id: Some(ME),
            ..cfg
        };
        Self {
            core: DhtCore::new(cfg, base),
            base,
        }
    }
    fn new() -> Self {
        Self::with(Config::default())
    }
    fn at(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }
    fn pkt(&mut self, ms: u64, from: SocketAddrV4, data: &[u8]) -> Vec<Intent> {
        let now = self.at(ms);
        self.core.handle(Input::Packet { from, data }, now)
    }
    fn tick(&mut self, ms: u64) -> Vec<Intent> {
        let now = self.at(ms);
        self.core.handle(Input::Tick, now)
    }
    fn input(&mut self, ms: u64, i: Input<'_>) -> Vec<Intent> {
        let now = self.at(ms);
        self.core.handle(i, now)
    }
}

fn sends(i: &[Intent]) -> Vec<(SocketAddrV4, Vec<u8>)> {
    i.iter()
        .filter_map(|x| match x {
            Intent::Send { to, data } => Some((*to, data.clone())),
            _ => None,
        })
        .collect()
}

/// Events, without the (frequent, uninteresting here) `NeedBootstrap`.
fn events(i: &[Intent]) -> Vec<Event> {
    all_events(i)
        .into_iter()
        .filter(|e| *e != Event::NeedBootstrap)
        .collect()
}

fn all_events(i: &[Intent]) -> Vec<Event> {
    i.iter()
        .filter_map(|x| match x {
            Intent::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

/// The datagram that answers a query (response or error), skipping our own outgoing queries.
fn answer(i: &[Intent]) -> Vec<u8> {
    sends(i)
        .into_iter()
        .map(|(_, d)| d)
        .find(|d| !matches!(codec::parse(d).unwrap().body, Body::Query(_)))
        .expect("an answer was sent")
}

fn answers(i: &[Intent]) -> usize {
    sends(i)
        .iter()
        .filter(|(_, d)| !matches!(codec::parse(d).unwrap().body, Body::Query(_)))
        .count()
}

fn query(tid: &[u8], q: QueryOut<'_>) -> Vec<u8> {
    let mut o = Vec::new();
    codec::encode_query(
        &mut o,
        &Envelope {
            tid,
            version: None,
            read_only: false,
            ip: None,
        },
        &q,
    );
    o
}

fn reply(
    tid: &[u8],
    id: &Id,
    ip: Option<SocketAddrV4>,
    token: Option<&[u8]>,
    nodes: &[u8],
    values: &[SocketAddrV4],
) -> Vec<u8> {
    let mut o = Vec::new();
    codec::encode_response(
        &mut o,
        &Envelope {
            tid,
            version: None,
            read_only: false,
            ip,
        },
        &ResponseOut {
            id,
            token,
            nodes,
            values,
        },
    );
    o
}

fn tid_of(data: &[u8]) -> Vec<u8> {
    codec::parse(data).unwrap().tid.to_vec()
}

/// Makes `from` (with `id`) known: it queries us, we probe it, it answers.
fn introduce(h: &mut H, ms: u64, from: SocketAddrV4, id: Id) -> bool {
    let out = h.pkt(ms, from, &query(b"zz", QueryOut::Ping { id: &id }));
    let s = sends(&out);
    // First send is the ping answer, then our verification probe.
    let probe = s.iter().find(|(to, d)| {
        *to == from
            && matches!(
                codec::parse(d).unwrap().body,
                Body::Query(Query::Ping { .. })
            )
    });
    let Some((_, probe)) = probe else {
        return false;
    };
    let tid = tid_of(probe);
    h.pkt(ms + 10, from, &reply(&tid, &id, None, None, b"", &[]));
    true
}

fn parse_resp(data: &[u8]) -> (Vec<u8>, codec::Response<'_>) {
    let m = codec::parse(data).unwrap();
    match m.body {
        Body::Response(r) => (m.tid.to_vec(), r),
        other => panic!("not a response: {other:?}"),
    }
}

#[test]
fn answers_ping_and_find_node_like_a_good_node() {
    let mut h = H::new();
    for n in 1..=20 {
        assert!(introduce(&mut h, 1000, peer(n), nid(n)));
    }
    assert_eq!(h.core.table_len(), 20);
    // Ping
    let asker = peer(900);
    let out = h.pkt(2000, asker, &query(b"aa", QueryOut::Ping { id: &nid(900) }));
    let data = &answer(&out);
    let (tid, r) = parse_resp(data);
    assert_eq!(tid, b"aa");
    assert_eq!(r.id, ME);
    assert_eq!(
        codec::parse(data).unwrap().ip,
        Some(asker),
        "BEP 42: responses echo the requester's address"
    );
    // find_node returns the closest <= 8 known nodes, nearest first
    let target = nid(5);
    let out = h.pkt(
        2100,
        peer(901),
        &query(
            b"ab",
            QueryOut::FindNode {
                id: &nid(901),
                target: &target,
            },
        ),
    );
    let data = answer(&out);
    let (_, r) = parse_resp(&data);
    let nodes: Vec<_> = r.nodes().collect();
    assert!(!nodes.is_empty() && nodes.len() <= 8);
    assert_eq!(nodes[0].0, target, "exact match is closest");
    for w in nodes.windows(2) {
        assert!(trs_dht::id::cmp_distance(&target, &w[0].0, &w[1].0).is_le());
    }
}

#[test]
fn get_peers_announce_roundtrip_with_token() {
    let mut h = H::new();
    let ih: Id = [9; 20];
    let a = peer(50);
    // get_peers -> token, no values yet
    let out = h.pkt(
        1000,
        a,
        &query(
            b"t1",
            QueryOut::GetPeers {
                id: &nid(50),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let (_, r) = parse_resp(&ans);
    let token = r.token.expect("token issued").to_vec();
    assert_eq!(r.values().count(), 0);
    // announce with a wrong token is an error 203
    let ans = answer(&h.pkt(
        1100,
        a,
        &query(
            b"t2",
            QueryOut::AnnouncePeer {
                id: &nid(50),
                info_hash: &ih,
                port: 5555,
                token: b"nope",
            },
        ),
    ));
    match codec::parse(&ans).unwrap().body {
        Body::Error(e) => assert_eq!(e.code, 203),
        o => panic!("{o:?}"),
    }
    assert_eq!(h.core.stored_peers(), 0);
    // token from another IP is also rejected (IP binding)
    let out = h.pkt(
        1150,
        peer(51),
        &query(
            b"t2",
            QueryOut::AnnouncePeer {
                id: &nid(51),
                info_hash: &ih,
                port: 5555,
                token: &token,
            },
        ),
    );
    let ans = answer(&out);
    assert!(matches!(codec::parse(&ans).unwrap().body, Body::Error(_)));
    // correct token: stored, acknowledged
    let out = h.pkt(
        1200,
        a,
        &query(
            b"t3",
            QueryOut::AnnouncePeer {
                id: &nid(50),
                info_hash: &ih,
                port: 5555,
                token: &token,
            },
        ),
    );
    let ans = answer(&out);
    parse_resp(&ans);
    assert_eq!(h.core.stored_peers(), 1);
    // another node now gets the peer in `values`
    let out = h.pkt(
        1300,
        peer(52),
        &query(
            b"t4",
            QueryOut::GetPeers {
                id: &nid(52),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let (_, r) = parse_resp(&ans);
    assert_eq!(
        r.values().collect::<Vec<_>>(),
        vec![SocketAddrV4::new(*a.ip(), 5555)]
    );
    // 30 minutes later the entry has expired
    let out = h.pkt(
        1000 + 31 * 60_000,
        peer(53),
        &query(
            b"t5",
            QueryOut::GetPeers {
                id: &nid(53),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let (_, r) = parse_resp(&ans);
    assert_eq!(r.values().count(), 0);
}

#[test]
fn implied_port_uses_udp_source_port() {
    let mut h = H::new();
    let ih = [3u8; 20];
    let a = SocketAddrV4::new(Ipv4Addr::new(30, 1, 1, 1), 40000);
    let out = h.pkt(
        0,
        a,
        &query(
            b"t1",
            QueryOut::GetPeers {
                id: &nid(1),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let token = parse_resp(&ans).1.token.unwrap().to_vec();
    let mut raw = b"d1:ad2:id20:".to_vec();
    raw.extend_from_slice(&nid(1));
    raw.extend_from_slice(b"12:implied_porti1e9:info_hash20:");
    raw.extend_from_slice(&ih);
    raw.extend_from_slice(b"4:porti1e5:token8:");
    raw.extend_from_slice(&token);
    raw.extend_from_slice(b"e1:q13:announce_peer1:t2:aa1:y1:qe");
    let out = h.pkt(10, a, &raw);
    let ans = answer(&out);
    parse_resp(&ans);
    let ans = answer(&h.pkt(
        20,
        peer(2),
        &query(
            b"t4",
            QueryOut::GetPeers {
                id: &nid(2),
                info_hash: &ih,
            },
        ),
    ));
    assert_eq!(parse_resp(&ans).1.values().collect::<Vec<_>>(), vec![a]);
}

#[test]
fn token_rotates_every_five_minutes_and_previous_is_accepted() {
    let mut h = H::new();
    let ih = [4u8; 20];
    let a = peer(60);
    let out = h.pkt(
        0,
        a,
        &query(
            b"t1",
            QueryOut::GetPeers {
                id: &nid(60),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let old = parse_resp(&ans).1.token.unwrap().to_vec();
    // After 5+ minutes a new token is issued...
    let out = h.pkt(
        5 * 60_000 + 1,
        a,
        &query(
            b"t2",
            QueryOut::GetPeers {
                id: &nid(60),
                info_hash: &ih,
            },
        ),
    );
    let ans = answer(&out);
    let new = parse_resp(&ans).1.token.unwrap().to_vec();
    assert_ne!(old, new);
    // ...but the previous one still works.
    let out = h.pkt(
        5 * 60_000 + 2,
        a,
        &query(
            b"t3",
            QueryOut::AnnouncePeer {
                id: &nid(60),
                info_hash: &ih,
                port: 1000,
                token: &old,
            },
        ),
    );
    let ans = answer(&out);
    parse_resp(&ans);
    // After another rotation the original token is dead.
    let out = h.pkt(
        10 * 60_000 + 5,
        a,
        &query(
            b"t4",
            QueryOut::AnnouncePeer {
                id: &nid(60),
                info_hash: &ih,
                port: 1001,
                token: &old,
            },
        ),
    );
    let ans = answer(&out);
    assert!(matches!(codec::parse(&ans).unwrap().body, Body::Error(e) if e.code == 203));
}

#[test]
fn malformed_and_unknown_get_proper_errors_or_silence() {
    let mut h = H::new();
    let a = peer(70);
    // query missing its arguments -> 203
    let out = h.pkt(
        0,
        a,
        b"d1:ad2:id20:abcdefghij0123456789e1:q9:find_node1:t2:aa1:y1:qe",
    );
    let ans = answer(&out);
    assert!(matches!(codec::parse(&ans).unwrap().body, Body::Error(e) if e.code == 203));
    // unknown method -> 204
    let out = h.pkt(
        0,
        peer(71),
        b"d1:ad2:id20:abcdefghij0123456789e1:q3:put1:t2:aa1:y1:qe",
    );
    let ans = answer(&out);
    assert!(matches!(codec::parse(&ans).unwrap().body, Body::Error(e) if e.code == 204));
    // garbage and non-queries are dropped silently
    for junk in [
        &b""[..],
        b"\x00\x01\x02",
        b"GET / HTTP/1.1\r\n\r\n",
        b"d1:y1:ee",
        b"d1:t2:aa1:y1:re",
        b"l3:fooe",
    ] {
        assert!(sends(&h.pkt(0, peer(72), junk)).is_empty(), "{junk:?}");
    }
    assert!(h.core.stats().malformed >= 5);
    // oversized datagram
    assert!(sends(&h.pkt(0, peer(73), &vec![b'd'; 5000])).is_empty());
    // invalid source addresses never get answers
    for bad in [
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234),
        SocketAddrV4::new(Ipv4Addr::new(30, 0, 0, 1), 0),
        SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 2), 1234),
    ] {
        assert!(
            sends(&h.pkt(0, bad, &query(b"aa", QueryOut::Ping { id: &nid(1) }))).is_empty(),
            "{bad}"
        );
    }
}

#[test]
fn read_only_node_is_silent_marks_queries_and_never_announces() {
    let mut h = H::with(Config {
        read_only: true,
        ..Config::default()
    });
    assert!(sends(&h.pkt(0, peer(1), &query(b"aa", QueryOut::Ping { id: &nid(1) }))).is_empty());
    let ih = PublicInfoHash::new([1; 20], false).unwrap();
    let ev = events(&h.input(
        0,
        Input::Announce {
            info_hash: ih,
            port: 1234,
        },
    ));
    assert!(ev.contains(&Event::Refused {
        info_hash: [1; 20],
        reason: Refused::ReadOnly
    }));
    // Its own queries carry ro=1 (BEP 43).
    let out = h.input(1, Input::Bootstrap(&[peer(5)]));
    let s = sends(&out);
    assert_eq!(s.len(), 1);
    assert!(codec::parse(&s[0].1).unwrap().read_only);
}

#[test]
fn queries_from_read_only_nodes_do_not_enter_the_table() {
    let mut h = H::new();
    let mut raw = Vec::new();
    codec::encode_query(
        &mut raw,
        &Envelope {
            tid: b"aa",
            version: None,
            read_only: true,
            ip: None,
        },
        &QueryOut::Ping { id: &nid(1) },
    );
    let s = sends(&h.pkt(0, peer(1), &raw));
    assert_eq!(s.len(), 1, "answered, but no verification probe sent");
    assert_eq!(h.core.table_len(), 0);
}

#[test]
fn per_ip_response_limit_stops_amplification() {
    let mut h = H::new();
    let attacker = peer(80);
    let mut answered = 0;
    for _ in 0..200 {
        answered += answers(&h.pkt(
            1000,
            attacker,
            &query(
                b"aa",
                QueryOut::FindNode {
                    id: &nid(80),
                    target: &nid(1),
                },
            ),
        ));
    }
    assert!(answered <= 10, "answered {answered} in one instant");
    assert!(h.core.stats().rate_limited >= 190);
    // A different source is unaffected, and the attacker recovers slowly (5/s).
    assert_eq!(
        answers(&h.pkt(
            1000,
            peer(81),
            &query(b"aa", QueryOut::Ping { id: &nid(81) })
        )),
        1
    );
    let later: usize = (0..20)
        .map(|_| {
            answers(&h.pkt(
                3000,
                attacker,
                &query(b"aa", QueryOut::Ping { id: &nid(80) }),
            ))
        })
        .sum();
    assert!((9..=11).contains(&later), "{later}");
}

#[test]
fn global_response_limit_holds_against_many_sources() {
    let mut h = H::new();
    let mut answered = 0;
    for n in 0..1000 {
        answered += sends(&h.pkt(
            0,
            peer(1000 + n),
            &query(b"aa", QueryOut::Ping { id: &nid(n) }),
        ))
        .iter()
        .filter(|(_, d)| matches!(codec::parse(d).unwrap().body, Body::Response(_)))
        .count();
    }
    assert!(answered <= 100, "burst answered {answered}");
}

#[test]
fn per_hash_cooldown_private_and_forbidden() {
    let mut h = H::new();
    assert!(
        PublicInfoHash::new([5; 20], true).is_none(),
        "private torrents cannot be expressed"
    );
    let ih = PublicInfoHash::new([5; 20], false).unwrap();
    // Seed one contact so lookups start.
    introduce(&mut h, 0, peer(1), nid(1));
    let ev = events(&h.input(1000, Input::GetPeers(ih)));
    assert!(!ev.iter().any(|e| matches!(e, Event::Refused { .. })));
    // same hash again: lookup still running or on cooldown
    let ev = events(&h.input(1500, Input::GetPeers(ih)));
    assert!(ev.contains(&Event::Refused {
        info_hash: [5; 20],
        reason: Refused::InProgress
    }));
    // let the lookup finish (the contact never answers)
    h.tick(1000 + 5_000);
    h.tick(1000 + 10_000);
    let ev = events(&h.input(1000 + 30_000, Input::GetPeers(ih)));
    match ev.as_slice() {
        [
            Event::Refused {
                reason: Refused::Cooldown { retry_after },
                ..
            },
        ] => assert!(
            *retry_after > Duration::from_secs(20) && *retry_after <= Duration::from_secs(60)
        ),
        o => panic!("{o:?}"),
    }
    // after the cooldown it is accepted again
    let ev = events(&h.input(1000 + 61_000, Input::GetPeers(ih)));
    assert!(
        !ev.iter().any(|e| matches!(e, Event::Refused { .. })),
        "{ev:?}"
    );
    // forbidden hash is refused, announce included
    h.core.forbid([6; 20]);
    let f = PublicInfoHash::new([6; 20], false).unwrap();
    assert!(
        events(&h.input(200_000, Input::GetPeers(f))).contains(&Event::Refused {
            info_hash: [6; 20],
            reason: Refused::Forbidden
        })
    );
    assert!(
        events(&h.input(
            200_000,
            Input::Announce {
                info_hash: f,
                port: 1
            }
        ))
        .contains(&Event::Refused {
            info_hash: [6; 20],
            reason: Refused::Forbidden
        })
    );
    // port 0 cannot be announced
    assert!(
        events(&h.input(
            200_000,
            Input::Announce {
                info_hash: ih,
                port: 0
            }
        ))
        .contains(&Event::Refused {
            info_hash: [5; 20],
            reason: Refused::BadPort
        })
    );
}

#[test]
fn announce_cooldown_is_separate_and_longer() {
    let mut h = H::new();
    introduce(&mut h, 0, peer(1), nid(1));
    let ih = PublicInfoHash::new([8; 20], false).unwrap();
    assert!(
        events(&h.input(
            1000,
            Input::Announce {
                info_hash: ih,
                port: 7000
            }
        ))
        .iter()
        .all(|e| !matches!(e, Event::Refused { .. }))
    );
    // Wait out the lookup, ask again after 2 minutes: announce is on a 10 minute cooldown.
    h.tick(10_000);
    h.tick(20_000);
    let ev = events(&h.input(
        120_000,
        Input::Announce {
            info_hash: ih,
            port: 7000,
        },
    ));
    assert!(
        matches!(
            ev.as_slice(),
            [Event::Refused {
                reason: Refused::Cooldown { .. },
                ..
            }]
        ),
        "{ev:?}"
    );
    let ev = events(&h.input(
        11 * 60_000,
        Input::Announce {
            info_hash: ih,
            port: 7000,
        },
    ));
    assert!(!ev.iter().any(|e| matches!(e, Event::Refused { .. })));
}

#[test]
fn known_nodes_get_one_retransmit_then_fail_and_lookup_ends() {
    let mut h = H::new();
    let node = peer(5);
    introduce(&mut h, 0, node, nid(5));
    let ih = PublicInfoHash::new([5; 20], false).unwrap();
    let s = sends(&h.input(100, Input::GetPeers(ih)));
    assert_eq!(s.len(), 1);
    let first_tid = tid_of(&s[0].1);
    // 2 s: retransmit with the same tid
    let s2 = sends(&h.tick(2100));
    assert_eq!(s2.len(), 1);
    assert_eq!(tid_of(&s2[0].1), first_tid);
    assert_eq!(h.core.stats().retransmits, 1);
    // nothing more to send; at 4 s the query is declared lost, the lookup ends
    let out = h.tick(4200);
    assert!(sends(&out).is_empty());
    assert!(events(&out).iter().any(|e| matches!(e, Event::LookupFinished { stats, .. } if stats.queries == 1 && stats.replied == 0)));
    assert_eq!(h.core.stats().timeouts, 1);
    assert_eq!(h.core.pending_queries(), 0);
    // a late answer is ignored (no matching transaction)
    h.pkt(
        4500,
        node,
        &reply(&first_tid, &nid(5), None, None, b"", &[]),
    );
    assert_eq!(h.core.stats().unmatched, 1);
}

#[test]
fn never_seen_contacts_are_not_retransmitted_to() {
    let mut h = H::new();
    let seed = peer(5);
    let s = sends(&h.input(0, Input::Bootstrap(&[seed])));
    assert_eq!(s.len(), 1);
    assert!(
        sends(&h.tick(2000)).is_empty(),
        "no retransmit to an unverified contact"
    );
    assert_eq!(h.core.stats().retransmits, 0);
    assert_eq!(h.core.stats().timeouts, 1);
    assert_eq!(h.core.table_len(), 0);
}

#[test]
fn responses_with_wrong_source_or_tid_are_ignored() {
    let mut h = H::new();
    let seed = peer(5);
    let s = sends(&h.input(0, Input::Bootstrap(&[seed])));
    let tid = tid_of(&s[0].1);
    // right tid, wrong address (spoof attempt)
    h.pkt(10, peer(6), &reply(&tid, &nid(6), None, None, b"", &[]));
    assert_eq!(h.core.table_len(), 0);
    // wrong tid, right address
    h.pkt(
        11,
        seed,
        &reply(&[0xde, 0xad], &nid(5), None, None, b"", &[]),
    );
    assert_eq!(h.core.table_len(), 0);
    // the real one is accepted
    h.pkt(12, seed, &reply(&tid, &nid(5), None, None, b"", &[]));
    assert_eq!(h.core.table_len(), 1);
}

#[test]
fn error_reply_ends_the_candidate_without_blaming_the_table() {
    let mut h = H::new();
    introduce(&mut h, 0, peer(1), nid(1));
    let ih = PublicInfoHash::new([5; 20], false).unwrap();
    let s = sends(&h.input(100, Input::GetPeers(ih)));
    let tid = tid_of(&s[0].1);
    let mut e = Vec::new();
    codec::encode_error(
        &mut e,
        &Envelope {
            tid: &tid,
            version: None,
            read_only: false,
            ip: None,
        },
        201,
        "nope",
    );
    let ev = events(&h.pkt(200, peer(1), &e));
    assert!(ev.iter().any(|x| matches!(x, Event::LookupFinished { .. })));
    assert_eq!(h.core.table_len(), 1);
}

#[test]
fn outgoing_query_budget_is_enforced() {
    let cfg = Config {
        out_burst: 2,
        out_rate: 1,
        ..Config::default()
    };
    let mut h = H::with(cfg);
    let seeds: Vec<_> = (1..=30).map(peer).collect();
    let mut total = sends(&h.input(0, Input::Bootstrap(&seeds))).len();
    // Seeds never answer; each timeout frees a lookup slot, but the bucket paces us.
    let mut t = 0;
    while t < 20_000 {
        t += 500;
        total += sends(&h.tick(t)).len();
    }
    // 2 burst plus 1/s sustained over 20 s, retransmits included
    assert!(total <= 2 + 20 + 1, "sent {total}");
    assert!(h.core.stats().budget_denied > 0);
}

#[test]
fn external_ip_consensus_regenerates_a_secure_node_id() {
    let mut h = H::new();
    let me_seen = SocketAddrV4::new(Ipv4Addr::new(124, 31, 75, 21), 6881);
    let before = h.core.node_id();
    assert_eq!(before, ME);
    let mut changed = None;
    for n in 0..5u32 {
        // Distinct /16 reporters (n*7+30).x.
        let from = SocketAddrV4::new(Ipv4Addr::new(30 + n as u8 * 7, 1, 1, 1), 6881);
        let out = h.input(0, Input::Bootstrap(&[from]));
        let s = sends(&out);
        let tid = tid_of(&s[0].1);
        let ev = events(&h.pkt(
            5,
            from,
            &reply(&tid, &nid(n + 1), Some(me_seen), None, b"", &[]),
        ));
        if let Some(e) = ev.iter().find(|e| matches!(e, Event::NodeIdChanged { .. })) {
            changed = Some((n, e.clone(), ev.clone()));
        }
    }
    let (n, e, ev) = changed.expect("consensus reached");
    assert_eq!(n, 3, "fourth distinct reporter completes the quorum");
    assert!(ev.contains(&Event::ExternalIp(*me_seen.ip())));
    let Event::NodeIdChanged { old, new } = e else {
        unreachable!()
    };
    assert_eq!(old, ME);
    assert_eq!(h.core.node_id(), new);
    assert!(trs_dht::id::is_valid_for_ip(&new, *me_seen.ip()));
    assert_eq!(h.core.external_ip(), Some(*me_seen.ip()));
}

#[test]
fn a_single_liar_cannot_change_our_id() {
    let mut h = H::new();
    for n in 0..10u32 {
        // all from the same /16
        let from = SocketAddrV4::new(Ipv4Addr::new(30, 1, n as u8, 1), 6881);
        let s = sends(&h.input(0, Input::Bootstrap(&[from])));
        let tid = tid_of(&s[0].1);
        h.pkt(
            5,
            from,
            &reply(
                &tid,
                &nid(n + 1),
                Some(SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 1)),
                None,
                b"",
                &[],
            ),
        );
    }
    assert_eq!(h.core.node_id(), ME);
    assert_eq!(h.core.external_ip(), None);
}

#[test]
fn state_roundtrip_seeds_a_bootstrap_lookup() {
    let mut h = H::new();
    for n in 1..=12 {
        introduce(&mut h, 1000, peer(n), nid(n));
    }
    let n_before = h.core.table_len();
    let blob = h.core.save_state();
    assert_eq!(blob.len(), 27 + n_before * 26);
    assert!(blob.len() < 5600, "200 nodes would be ~5.2 KiB");
    let mut h2 = H::with(Config {
        initial_node_id: None,
        ..Config::default()
    });
    // initial_node_id: Some(ME) is forced by H::with; the state's id is the same, which is fine.
    let now = h2.at(0);
    let out = h2.core.load_state(&blob, now).unwrap();
    let s = sends(&out);
    assert!(
        !s.is_empty() && s.len() <= 3,
        "alpha parallel find_node queries, got {}",
        s.len()
    );
    for (to, d) in &s {
        assert!((1..=12).any(|n| peer(n) == *to));
        assert!(matches!(
            codec::parse(d).unwrap().body,
            Body::Query(Query::FindNode { .. })
        ));
    }
    // Truncated / foreign blobs are rejected.
    assert!(h2.core.load_state(&blob[..blob.len() - 1], now).is_err());
    assert!(h2.core.load_state(b"", now).is_err());
    let mut bad = blob.clone();
    bad[0] = 99;
    assert!(h2.core.load_state(&bad, now).is_err());
}

#[test]
fn saved_state_is_capped_at_200_nodes() {
    let mut h = H::new();
    for n in 1..=3000 {
        introduce(&mut h, 1000, peer(n), nid(n));
    }
    assert!(h.core.save_state().len() <= 27 + trs_dht::MAX_SAVED_NODES * 26);
}

#[test]
fn idle_core_wakes_rarely() {
    let mut h = H::new();
    // Empty table: asks for bootstrap, then backs off exponentially up to an hour.
    let ev = all_events(&h.tick(0));
    assert!(ev.contains(&Event::NeedBootstrap));
    let mut gaps = Vec::new();
    let mut last = 0u64;
    for _ in 0..12 {
        let w = h.core.next_wakeup().expect("bootstrap retry scheduled");
        let t = w.duration_since(h.base).as_millis() as u64;
        gaps.push(t - last);
        last = t;
        assert!(all_events(&h.tick(t)).contains(&Event::NeedBootstrap));
    }
    assert!(
        gaps.windows(2)
            .skip(1)
            .all(|w| w[1] >= w[0] || w[1] == 3_600_000),
        "{gaps:?}"
    );
    assert!(*gaps.last().unwrap() >= 3_000_000, "{gaps:?}");

    // With a populated, fresh table the next wake-up is the 15 minute bucket refresh.
    let mut h = H::new();
    for n in 1..=10 {
        introduce(&mut h, 1000, peer(n), nid(n));
    }
    h.tick(2000);
    let w = h
        .core
        .next_wakeup()
        .unwrap()
        .duration_since(h.base)
        .as_secs();
    assert!((14 * 60..=16 * 60 + 5).contains(&w), "next wake in {w}s");
    // A tick before that does nothing: no packets.
    assert!(h.tick(10 * 60_000).is_empty());
}

#[test]
fn stale_bucket_triggers_a_small_refresh_only() {
    let mut h = H::new();
    for n in 1..=10 {
        introduce(&mut h, 1000, peer(n), nid(n));
    }
    let out = h.tick(1000 + 15 * 60_000 + 100);
    let s = sends(&out);
    assert!(!s.is_empty(), "stale bucket must be refreshed");
    assert!(s.len() <= 3);
    for (_, d) in &s {
        assert!(matches!(
            codec::parse(d).unwrap().body,
            Body::Query(Query::FindNode { .. })
        ));
    }
}

#[test]
fn announce_storage_is_bounded() {
    let mut h = H::new();
    let ih = [7u8; 20];
    for n in 0..500u32 {
        let a = peer(2000 + n);
        let out = h.pkt(
            n as u64 * 1000,
            a,
            &query(
                b"t1",
                QueryOut::GetPeers {
                    id: &nid(n),
                    info_hash: &ih,
                },
            ),
        );
        let ans = answer(&out);
        let token = parse_resp(&ans).1.token.unwrap().to_vec();
        h.pkt(
            n as u64 * 1000 + 1,
            a,
            &query(
                b"t2",
                QueryOut::AnnouncePeer {
                    id: &nid(n),
                    info_hash: &ih,
                    port: 1000,
                    token: &token,
                },
            ),
        );
    }
    assert!(
        h.core.stored_peers() <= 100,
        "per-hash cap, got {}",
        h.core.stored_peers()
    );
    // Many distinct hashes: bounded by max_infohashes (256) x per-hash.
    let mut h = H::new();
    for n in 0..600u32 {
        let a = peer(3000 + n);
        let mut ih = [0u8; 20];
        ih[..4].copy_from_slice(&n.to_be_bytes());
        let out = h.pkt(
            n as u64 * 1000,
            a,
            &query(
                b"t1",
                QueryOut::GetPeers {
                    id: &nid(n),
                    info_hash: &ih,
                },
            ),
        );
        let ans = answer(&out);
        let token = parse_resp(&ans).1.token.unwrap().to_vec();
        h.pkt(
            n as u64 * 1000 + 1,
            a,
            &query(
                b"t2",
                QueryOut::AnnouncePeer {
                    id: &nid(n),
                    info_hash: &ih,
                    port: 1000,
                    token: &token,
                },
            ),
        );
    }
    assert!(h.core.stored_peers() <= 256);
}

#[test]
fn private_hash_cannot_reach_the_network() {
    // The only way to express a lookup is PublicInfoHash, and it is None for private torrents.
    let hash = [0xAA; 20];
    assert!(PublicInfoHash::new(hash, true).is_none());
    // And even a public hash, once forbidden, produces no packet at all.
    let mut h = H::new();
    introduce(&mut h, 0, peer(1), nid(1));
    h.core.forbid(hash);
    let p = PublicInfoHash::new(hash, false).unwrap();
    let out = h.input(1000, Input::GetPeers(p));
    assert!(sends(&out).is_empty());
    let out = h.input(
        1000,
        Input::Announce {
            info_hash: p,
            port: 6881,
        },
    );
    assert!(sends(&out).is_empty());
    h.core.unforbid(&hash);
    let out = h.input(2000, Input::GetPeers(p));
    assert!(!sends(&out).is_empty());
}

#[test]
fn lookup_collects_peers_dedupes_and_announces_to_closest_with_tokens() {
    let mut h = H::new();
    for n in 1..=8 {
        introduce(&mut h, 0, peer(n), nid(n));
    }
    let ih: Id = nid(3);
    let p = PublicInfoHash::new(ih, false).unwrap();
    let mut out = h.input(
        1000,
        Input::Announce {
            info_hash: p,
            port: 7777,
        },
    );
    let mut now = 1000;
    let mut found: Vec<SocketAddrV4> = Vec::new();
    let mut announced_to = Vec::new();
    let mut finished = false;
    for _ in 0..60 {
        let mut next = Vec::new();
        for (to, d) in sends(&out) {
            let m = codec::parse(&d).unwrap();
            let Body::Query(q) = m.body else { continue };
            let responder = (1..=8)
                .find(|n| peer(*n) == to)
                .expect("only known nodes are queried");
            now += 5;
            match q {
                Query::GetPeers { .. } => {
                    let values = [peer(500), peer(501)]; // same two peers from everyone: must be deduped
                    next.extend(h.pkt(
                        now,
                        to,
                        &reply(m.tid, &nid(responder), None, Some(b"tkn"), b"", &values),
                    ));
                }
                Query::AnnouncePeer { token, port, .. } => {
                    assert_eq!(token, b"tkn");
                    assert_eq!(port, 7777);
                    announced_to.push(to);
                    next.extend(h.pkt(
                        now,
                        to,
                        &reply(m.tid, &nid(responder), None, None, b"", &[]),
                    ));
                }
                other => panic!("{other:?}"),
            }
        }
        for e in events(&out) {
            if let Event::Peers { peers, .. } = e {
                found.extend(peers);
            }
        }
        out = next;
        if out
            .iter()
            .any(|i| matches!(i, Intent::Event(Event::Announced { .. })))
        {
            for e in events(&out) {
                match e {
                    Event::Announced {
                        accepted,
                        attempted,
                        ..
                    } => {
                        assert_eq!((accepted, attempted), (8, 8));
                        finished = true;
                    }
                    Event::Peers { peers, .. } => found.extend(peers),
                    _ => {}
                }
            }
            break;
        }
        if out.is_empty() {
            break;
        }
    }
    assert!(finished, "announce round completed");
    found.sort();
    assert_eq!(
        found,
        vec![peer(500), peer(501)],
        "each peer reported exactly once"
    );
    announced_to.sort();
    let mut want: Vec<_> = (1..=8).map(peer).collect();
    want.sort();
    assert_eq!(announced_to, want);
}

#[test]
fn full_bucket_of_stale_nodes_pings_the_oldest_and_replaces_it_if_dead() {
    let mut h = H::new();
    // Eight contacts in the far half of the id space (bucket 0), then one nearer so bucket 0 splits off.
    let far: Vec<u32> = (1..=8).map(|k| k * 30).collect();
    for &n in &far {
        assert!(introduce(&mut h, 1000 + u64::from(n), peer(n), nid(n)));
    }
    assert!(introduce(&mut h, 2000, peer(1), nid(1)));
    // A ninth far node is refused while the bucket is full of good nodes (no probe, no table change).
    assert!(!introduce(&mut h, 3000, peer(300), nid(300)));
    // 16 minutes later the far nodes are only "questionable": the newcomer is probed, and since the
    // bucket is full the oldest stale node is pinged.
    let later = 1000 + 16 * 60_000;
    let out = h.pkt(
        later,
        peer(300),
        &query(b"zz", QueryOut::Ping { id: &nid(300) }),
    );
    let probe = sends(&out)
        .into_iter()
        .find(|(to, d)| *to == peer(300) && matches!(codec::parse(d).unwrap().body, Body::Query(_)))
        .expect("probe");
    let out = h.pkt(
        later + 10,
        peer(300),
        &reply(&tid_of(&probe.1), &nid(300), None, None, b"", &[]),
    );
    let verify = sends(&out)
        .into_iter()
        .find(|(to, d)| {
            far.iter().any(|n| peer(*n) == *to)
                && matches!(
                    codec::parse(d).unwrap().body,
                    Body::Query(Query::Ping { .. })
                )
        })
        .expect("stale node pinged");
    let stale_addr = verify.0;
    assert!(h.core.table_len() >= 9);
    // It never answers: after the timeout + one retransmit it is declared dead and the newcomer takes its place.
    h.tick(later + 2100);
    h.tick(later + 4200);
    assert_eq!(h.core.table_len(), 9, "replacement keeps the table size");
    // Prove by asking for nodes near the newcomer: it must be listed, the dead one must not.
    let out = h.pkt(
        later + 5000,
        peer(901),
        &query(
            b"fn",
            QueryOut::FindNode {
                id: &nid(901),
                target: &nid(300),
            },
        ),
    );
    let ans = answer(&out);
    let (_, r) = parse_resp(&ans);
    let listed: Vec<SocketAddrV4> = r.nodes().map(|(_, a)| a).collect();
    assert!(listed.contains(&peer(300)));
    assert!(!listed.contains(&stale_addr));
}

#[test]
fn read_only_can_be_toggled_at_runtime() {
    let mut h = H::new();
    assert!(answers(&h.pkt(0, peer(1), &query(b"aa", QueryOut::Ping { id: &nid(1) }))) == 1);
    h.core.set_read_only(true);
    assert!(sends(&h.pkt(10, peer(2), &query(b"aa", QueryOut::Ping { id: &nid(2) }))).is_empty());
    h.core.set_read_only(false);
    assert!(answers(&h.pkt(20, peer(3), &query(b"aa", QueryOut::Ping { id: &nid(3) }))) == 1);
}

/// SPEC 7C.4c: after an outage the bootstrap backoff (up to an hour) starts over and the
/// empty table bootstraps again at once, exactly once.
#[test]
fn network_restored_resets_the_bootstrap_backoff() {
    let mut h = H::new();
    assert!(all_events(&h.tick(0)).contains(&Event::NeedBootstrap));
    let mut t = 0;
    for _ in 0..10 {
        t = h.core.next_wakeup().unwrap().duration_since(h.base).as_millis() as u64;
        h.tick(t);
    }
    let next = h.core.next_wakeup().unwrap().duration_since(h.base).as_millis() as u64;
    assert!(next - t >= 8 * 60_000, "backed off to {} ms", next - t);
    let now = h.at(t + 1000);
    let ev = all_events(&h.core.network_restored(now));
    assert_eq!(ev.iter().filter(|e| **e == Event::NeedBootstrap).count(), 1, "one bootstrap");
    // ... and the retry after it is the short one again.
    let after = h.core.next_wakeup().unwrap().duration_since(h.base).as_millis() as u64;
    assert!(after - (t + 1000) <= 31_000, "retry in {} ms", after - (t + 1000));
}
