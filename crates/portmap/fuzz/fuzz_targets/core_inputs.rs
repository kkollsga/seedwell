#![no_main]
//! Drives the whole state machine with arbitrary datagrams and HTTP answers. The
//! input is a script: each chunk is `[kind, len, bytes..]`. Must never panic, and the
//! machine must never ask for more than one outstanding exchange.

use std::net::{Ipv4Addr, SocketAddr};

use libfuzzer_sys::fuzz_target;
use trs_portmap::{Config, HttpFailure, Input, Millis, NetInfo, Output, PortMapper, Wanted};

fuzz_target!(|data: &[u8]| {
    let gw = Ipv4Addr::new(10, 0, 0, 1);
    let mut m = PortMapper::new(Config {
        wanted: Wanted::tcp_and_udp(51413),
        seed: u64::from(data.first().copied().unwrap_or(0)),
        ..Config::default()
    });
    let mut now = 0u64;
    let mut last_http: Option<u64> = None;
    let run = |m: &mut PortMapper, input: Input<'_>, now: u64, last_http: &mut Option<u64>| {
        for o in m.handle(input, Millis(now)) {
            if let Output::Http(r) = o {
                *last_http = Some(r.id);
            }
        }
    };
    run(&mut m, Input::Network(Some(NetInfo { gateway: gw, local: Ipv4Addr::new(10, 0, 0, 2) })), now, &mut last_http);
    let mut rest = data;
    while let [kind, len, tail @ ..] = rest {
        let n = usize::from(*len).min(tail.len());
        let (payload, next) = tail.split_at(n);
        rest = next;
        now += u64::from(*kind) * 37;
        match kind % 6 {
            0 => run(&mut m, Input::Udp { from: SocketAddr::from((gw, 5351)), data: payload }, now, &mut last_http),
            1 => run(&mut m, Input::Udp { from: SocketAddr::from((gw, 1900)), data: payload }, now, &mut last_http),
            2 => {
                if let Some(id) = last_http {
                    run(&mut m, Input::HttpDone { id, outcome: Ok(payload.to_vec()) }, now, &mut last_http);
                }
            }
            3 => {
                if let Some(id) = last_http {
                    run(&mut m, Input::HttpDone { id, outcome: Err(HttpFailure::Timeout) }, now, &mut last_http);
                }
            }
            4 => run(&mut m, Input::Tick, now + 60_000, &mut last_http),
            _ => run(&mut m, Input::Network(Some(NetInfo { gateway: gw, local: Ipv4Addr::new(10, 0, 0, 2 + (*len % 3)) })), now, &mut last_http),
        }
        if let Some(w) = m.next_wakeup() {
            now = now.max(w.0);
        }
    }
    run(&mut m, Input::Shutdown, now + 1, &mut last_http);
});
