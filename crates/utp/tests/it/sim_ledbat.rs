//! LEDBAT behaviour on a shared bottleneck: target queuing delay, yielding to a loss-based TCP
//! flow, and recovering the link afterwards.

use crate::common;

use common::*;
use trs_utp::Config;

const SEC: u64 = 1_000_000;

/// 4 Mbit/s bottleneck, 40 ms RTT, 250 KB of buffer (500 ms): a bufferbloated access link.
fn bloated() -> LinkCfg {
    LinkCfg::clean(4_000_000, 20_000, 250_000)
}

fn ack_link() -> LinkCfg {
    LinkCfg::clean(0, 20_000, 1 << 30)
}

#[test]
fn single_flow_holds_queue_near_target() {
    let mut sim = Sim::new(Config::default(), bloated(), ack_link(), 71);
    let mut t = Transfer::new(pseudo_data(1, 30 << 20), Vec::new());
    let mut samples = Vec::new();
    sim.run_for(60 * SEC, &mut |s| {
        t.step(s);
        if s.now > START + 15 * SEC {
            samples.push(s.ab.queue_delay(s.now));
        }
        false
    });
    let (med, p95) = queue_delays_us(&samples);
    let got = t.b.rx.len() as f64 * 8.0 / 60.0 / 1e6;
    println!(
        "single flow: queue delay median {:.0} ms, p95 {:.0} ms (target 100), goodput {:.2} of 4 Mbit/s, queue drops {}",
        med as f64 / 1e3,
        p95 as f64 / 1e3,
        got,
        sim.ab.dropped_queue
    );
    assert!(t.b.rx == t.a.tx[..t.b.rx.len()], "integrity");
    assert!((60_000..=150_000).contains(&med), "median queue delay {med} us not near the 100 ms target");
    assert!(p95 < 220_000, "p95 queue delay {p95} us");
    assert!(got > 3.4, "utilisation too low: {got}");
}

#[test]
fn yields_to_tcp_and_recovers() {
    let mut sim = Sim::new(Config::default(), bloated(), ack_link(), 72);
    let mut t = Transfer::new(pseudo_data(2, 100 << 20), Vec::new());
    let t_tcp_start = START + 30 * SEC;
    let t_tcp_stop = START + 90 * SEC;
    sim.wake_at(t_tcp_start);
    sim.wake_at(t_tcp_stop);
    let mut tcp_on = false;
    let mut mark = Vec::new(); // (time, utp bytes, tcp pkts)
    let mut next_mark = START;
    let mut cwnd_before = 0u32;
    let mut cwnd_during = 0u32;
    sim.run_for(150 * SEC, &mut |s| {
        t.step(s);
        if !tcp_on && s.now >= t_tcp_start {
            tcp_on = true;
            cwnd_before = s.a.conn_stats(t.a.handle.unwrap()).unwrap().cwnd;
            s.start_tcp();
        }
        if tcp_on && s.now >= t_tcp_stop && s.tcp.active {
            s.tcp.active = false;
        }
        while s.now >= next_mark {
            mark.push((next_mark, t.b.rx.len() as u64, s.tcp.arrived));
            next_mark += 5 * SEC;
        }
        if s.now >= t_tcp_start + 40 * SEC && cwnd_during == 0 {
            cwnd_during = s.a.conn_stats(t.a.handle.unwrap()).unwrap().cwnd;
        }
        false
    });
    let rate = |a: usize, b: usize| {
        let (t0, u0, c0) = mark[a];
        let (t1, u1, c1) = mark[b];
        let dt = (t1 - t0) as f64 / 1e6;
        ((u1 - u0) as f64 * 8.0 / dt / 1e6, (c1 - c0) as f64 * TCP_PKT as f64 * 8.0 / dt / 1e6)
    };
    // marks are every 5 s from START: index i = 5*i s
    let (u_alone, _) = rate(2, 6); // 10..30 s
    let (u_comp, c_comp) = rate(10, 18); // 50..90 s (TCP established)
    let (_, c_first) = rate(6, 10); // 30..50 s
    let (u_after, _) = rate(24, 28); // 120..140 s
    for (i, m) in mark.iter().enumerate().step_by(2) {
        println!("  t={:>3} s utp {:>6.1} MB tcp {:>6} pkts", i * 5, m.1 as f64 / 1e6, m.2);
    }
    println!(
        "alone: uTP {:.2} Mbit/s | with TCP: uTP {:.2}, TCP {:.2} Mbit/s (first 20 s: TCP {:.2}) | after TCP stops: uTP {:.2} Mbit/s (link 4.0)",
        u_alone, u_comp, c_comp, c_first, u_after
    );
    println!("uTP cwnd before TCP {} B, 40 s into the competition {} B", cwnd_before, cwnd_during);
    assert!(t.b.rx == t.a.tx[..t.b.rx.len()]);
    assert!(u_alone > 3.4);
    assert!(c_comp > 3.4 && c_comp > 10.0 * u_comp.max(0.05), "TCP must win the link: tcp {c_comp} utp {u_comp}");
    assert!(u_comp < 0.9, "uTP should back off to a trickle under a queue-filling flow, got {u_comp}");
    assert!(cwnd_during < cwnd_before / 2, "window must shrink: {cwnd_before} -> {cwnd_during}");
    assert!(u_after > 3.0, "uTP must take the link back, got {u_after}");
}

#[test]
fn two_utp_flows_share_without_starving() {
    // Two LEDBAT flows on one bottleneck: both make progress (late-comer advantage is a known
    // LEDBAT property, so only a floor is asserted).
    let mut sim = Sim::new(Config::default(), bloated(), ack_link(), 73);
    let mut t1 = Transfer::new(pseudo_data(3, 20 << 20), Vec::new());
    t1.step(&mut sim);
    // second flow from the same endpoint pair: another connection
    let mut hb2 = None;
    let mut a2 = Side::new(pseudo_data(4, 20 << 20));
    let mut b2 = Side::new(Vec::new());
    b2.hold_open = true;
    sim.run_for(60 * SEC, &mut |s| {
        t1.step(s);
        if s.now > START + 10 * SEC {
            if a2.handle.is_none() {
                a2.handle = Some(s.a.connect(s.now, s.addr_b).unwrap());
            }
            if hb2.is_none() {
                // t1's server side was accepted first; the next accept is flow 2
                hb2 = s.b.poll_accept();
                b2.handle = hb2;
            }
            let now = s.now;
            pump(&mut s.a, &mut a2, now);
            pump(&mut s.b, &mut b2, now);
        }
        false
    });
    let r1 = t1.b.rx.len() as f64;
    let r2 = b2.rx.len() as f64;
    println!("two flows: flow1 {:.1} MB, flow2 {:.1} MB in 60 s (flow2 started at 10 s)", r1 / 1e6, r2 / 1e6);
    assert!(r2 > 1_000_000.0 && r1 > 1_000_000.0);
    assert!(b2.rx == a2.tx[..b2.rx.len()]);
}
