#![no_main]
//! Drives a whole `DhtCore` with arbitrary datagrams, interleaved with ticks and lookups, and
//! answers the core's own queries with fuzz-derived (but well-formed) responses so that lookups,
//! announces and the routing table actually progress. Must never panic.

use libfuzzer_sys::fuzz_target;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};
use trs_dht::codec::{self, Envelope, ResponseOut};
use trs_dht::{Config, DhtCore, Input, Intent, PublicInfoHash};

fn answer_queries(core: &mut DhtCore, out: Vec<Intent>, fuzz: &[u8], now: Instant, depth: u8) {
    if depth == 0 {
        return;
    }
    for i in out {
        let Intent::Send { to, data } = i else {
            continue;
        };
        let Ok(m) = codec::parse(&data) else { continue };
        let mut id = [0u8; 20];
        for (k, b) in id.iter_mut().enumerate() {
            *b = fuzz.get(k).copied().unwrap_or(k as u8);
        }
        let nodes = &fuzz[fuzz.len().min(20)..];
        let nodes = &nodes[..nodes.len().min(26 * 8)];
        let values = [SocketAddrV4::new(
            Ipv4Addr::new(1, 2, 3, fuzz.first().copied().unwrap_or(4)),
            5000,
        )];
        let mut resp = Vec::new();
        codec::encode_response(
            &mut resp,
            &Envelope {
                tid: m.tid,
                version: None,
                read_only: false,
                ip: Some(SocketAddrV4::new(Ipv4Addr::new(80, 1, 2, 3), 9)),
            },
            &ResponseOut {
                id: &id,
                token: Some(b"tok"),
                nodes,
                values: &values,
            },
        );
        let next = core.handle(
            Input::Packet {
                from: to,
                data: &resp,
            },
            now,
        );
        answer_queries(core, next, fuzz, now, depth - 1);
    }
}

fuzz_target!(|data: &[u8]| {
    let base = Instant::now();
    let cfg = Config {
        seed: Some(1),
        allow_local_addrs: data.first().is_some_and(|b| b & 1 == 1),
        ..Config::default()
    };
    let mut core = DhtCore::new(cfg, base);
    let seed = SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 6881);
    let mut now = base;
    let out = core.handle(Input::Bootstrap(&[seed]), now);
    answer_queries(&mut core, out, data, now, 3);
    // Split the input into datagrams: first byte = length, next byte picks the source.
    let mut rest = data;
    while let [len, src, tail @ ..] = rest {
        let n = (*len as usize % 200).min(tail.len());
        let (pkt, next) = tail.split_at(n);
        rest = next;
        now += Duration::from_millis(u64::from(*src) * 37);
        let from = SocketAddrV4::new(
            Ipv4Addr::new(10 + (*src % 4), 0, 0, *src),
            1000 + u16::from(*src),
        );
        let out = core.handle(Input::Packet { from, data: pkt }, now);
        answer_queries(&mut core, out, pkt, now, 2);
        if *src % 5 == 0 {
            if let Some(h) = PublicInfoHash::new([*src; 20], false) {
                let out = core.handle(
                    if *src % 10 == 0 {
                        Input::Announce {
                            info_hash: h,
                            port: 6881,
                        }
                    } else {
                        Input::GetPeers(h)
                    },
                    now,
                );
                answer_queries(&mut core, out, pkt, now, 3);
            }
        }
        let out = core.handle(Input::Tick, now);
        answer_queries(&mut core, out, pkt, now, 2);
        let _ = core.next_wakeup();
    }
});
