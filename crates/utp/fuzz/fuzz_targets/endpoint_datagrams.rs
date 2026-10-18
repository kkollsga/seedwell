#![no_main]
//! Stateful fuzzing: an endpoint with one outgoing connection and an accepting side receives an
//! arbitrary sequence of datagrams from a few addresses while the clock advances. Nothing may
//! panic, queues must stay bounded and timers must always make progress.

use std::net::SocketAddr;

use libfuzzer_sys::fuzz_target;
use trs_utp::{Config, Endpoint};

fuzz_target!(|data: &[u8]| {
    let mut ep = Endpoint::new(Config { max_connections: 64, ..Config::default() }, 1);
    let peers: [SocketAddr; 3] = [
        "192.0.2.1:1000".parse().unwrap(),
        "192.0.2.2:1000".parse().unwrap(),
        "[2001:db8::1]:1000".parse().unwrap(),
    ];
    let mut now = 1_000_000u64;
    let h = ep.connect(now, peers[0]).ok();
    let mut buf = vec![0u8; ep.transmit_buffer_len()];
    let mut rest = data;
    while let [sel, len, tail @ ..] = rest {
        let n = (*len as usize).min(tail.len());
        let (dg, next) = tail.split_at(n);
        rest = next;
        now += (*sel as u64) * 700;
        let from = peers[(*sel as usize) % 3];
        ep.handle_datagram(now, from, dg);
        if sel & 8 != 0 {
            ep.handle_timeout(now);
        }
        let mut guard = 0;
        while ep.poll_transmit(now, &mut buf).is_some() {
            guard += 1;
            assert!(guard < 10_000, "unbounded transmit loop");
        }
        if let Some(h) = h {
            let mut rb = [0u8; 256];
            let _ = ep.read(h, now, &mut rb);
            let _ = ep.write(h, now, &rb[..(*sel as usize) % 200]);
        }
        let _ = ep.poll_accept();
        while ep.poll_event().is_some() {}
        if let Some(t) = ep.poll_timeout() {
            assert!(t > 0);
        }
    }
});
