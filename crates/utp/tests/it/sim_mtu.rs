//! Path-MTU discovery on the simulated network (virtual time).

use crate::common;

use common::*;
use trs_utp::Config;

const MB: usize = 1 << 20;

fn path_mtu(sim: &Sim, t: &Transfer) -> u32 {
    sim.a.conn_stats(t.a.handle.unwrap()).unwrap().path_mtu
}

fn finish(sim: &mut Sim, t: &mut Transfer, secs: u64) {
    let ok = sim.run_for(secs * 1_000_000, &mut |s| t.step(s));
    assert!(ok, "transfer did not finish");
    assert!(t.b.rx == t.a.tx, "A->B corrupted");
}

#[test]
fn ordinary_path_settles_at_the_configured_ceiling() {
    // A 1500 B MTU path (1472 B UDP payload) with the default ceiling of 1400 + 54 B.
    let mut link = LinkCfg::clean(100_000_000, 10_000, 500_000);
    link.max_dgram = 1472;
    let mut sim = Sim::new(Config::default(), link.clone(), link, 301);
    let mut t = Transfer::new(pseudo_data(1, 4 * MB), Vec::new());
    t.a.hold_open = true;
    sim.run_for(20_000_000, &mut |s| t.step(s));
    let mtu = path_mtu(&sim, &t);
    println!("1500 B path settled at a {mtu} B datagram, {} probes dropped", sim.ab.dropped_mtu);
    assert_eq!(mtu, 1454, "default ceiling is max_payload + 54");
    assert!((1452..=1472).contains(&mtu));
    assert_eq!(sim.ab.dropped_mtu, 0, "the ceiling is probed first and the path carries it");
    assert!(t.b.rx == t.a.tx);
}

#[test]
fn new_connections_start_conservative() {
    let link = LinkCfg::clean(100_000_000, 10_000, 500_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 302);
    let mut t = Transfer::new(pseudo_data(1, 100), Vec::new());
    t.a.hold_open = true;
    sim.run_for(200_000, &mut |s| t.step(s));
    assert_eq!(path_mtu(&sim, &t), 1232, "a short exchange never probes");
}

#[test]
fn a_path_that_drops_big_datagrams_settles_back_without_congestion_response() {
    // MTU 1428 (e.g. a tunnel): datagrams above 1400 B UDP payload vanish.
    let mut link = LinkCfg::clean(100_000_000, 10_000, 500_000);
    link.max_dgram = 1400;
    let mut sim = Sim::new(Config::default(), link.clone(), link, 303);
    let mut t = Transfer::new(pseudo_data(1, 16 * MB), Vec::new());
    let mut cwnd_dips = 0;
    let mut last = 0u32;
    let mut timeouts_seen = 0;
    let ok = sim.run_for(60_000_000, &mut |s| {
        let done = t.step(s);
        if let Some(h) = t.a.handle {
            if let Some(st) = s.a.conn_stats(h) {
                // The window may shrink for delay reasons only at LEDBAT's gentle pace; a loss
                // response would halve it in one step.
                if last > 20_000 && st.cwnd < last / 2 {
                    cwnd_dips += 1;
                }
                last = st.cwnd;
                timeouts_seen = st.timeouts;
            }
        }
        done
    });
    assert!(ok);
    assert!(t.b.rx == t.a.tx);
    let mtu = path_mtu(&sim, &t);
    println!("tunnel path settled at {mtu} B, {} oversized datagrams dropped", sim.ab.dropped_mtu);
    assert!(mtu <= 1400, "settled above the path MTU: {mtu}");
    assert!(mtu >= 1400 - 16, "bisection should land within its resolution of 1400, got {mtu}");
    assert!(sim.ab.dropped_mtu >= 1, "the ceiling probe must have been lost");
    assert!(sim.ab.dropped_mtu <= 8, "bisection is logarithmic, dropped {}", sim.ab.dropped_mtu);
    assert_eq!(cwnd_dips, 0, "probe loss halved the congestion window");
    assert_eq!(timeouts_seen, 0, "probe loss must not count as a retransmission timeout");
}

#[test]
fn probe_loss_leaves_the_window_alone_even_with_few_packets_in_flight() {
    // Thin pipe: the lost probe is detected by the retransmission timer, not by SACKs.
    let mut link = LinkCfg::clean(2_000_000, 40_000, 100_000);
    link.max_dgram = 1300;
    let mut sim = Sim::new(Config::default(), link.clone(), link, 304);
    let mut t = Transfer::new(pseudo_data(1, 2 * MB), Vec::new());
    finish(&mut sim, &mut t, 120);
    let st = sim.a.conn_stats(t.a.handle.unwrap()).unwrap();
    println!("thin path: mtu {} timeouts {} retransmits {}", st.path_mtu, st.timeouts, st.retransmits);
    assert!(st.path_mtu <= 1300 && st.path_mtu >= 1232);
    assert_eq!(st.timeouts, 0, "a lost probe is not a connection-level timeout");
}

#[test]
fn loopback_style_peers_grow_past_1400_and_the_transfer_is_exact() {
    // Same code path as 127.0.0.1: the endpoint raises the ceiling for loopback remotes. The sim
    // uses 10.0.0.x addresses, so ask for a big payload explicitly.
    let cfg = Config { max_payload: 3946, ..Config::default() };
    let link = LinkCfg::clean(1_000_000_000, 100, 4_000_000);
    let mut sim = Sim::new(cfg, link.clone(), link, 305);
    let mut t = Transfer::new(pseudo_data(1, 8 * MB), Vec::new());
    t.a.hold_open = true;
    sim.run_for(20_000_000, &mut |s| t.step(s));
    assert_eq!(path_mtu(&sim, &t), 4000);
    assert!(t.b.rx == t.a.tx);
}

#[test]
fn a_settled_search_starts_over_after_the_reprobe_interval() {
    // The path first carries only 1300 B datagrams, later 1454 B.
    let mut link = LinkCfg::clean(100_000_000, 10_000, 500_000);
    link.max_dgram = 1300;
    let mut sim = Sim::new(Config::default(), link.clone(), link, 306);
    let mut t = Transfer::new(pseudo_data(1, 4 * MB), Vec::new());
    t.a.hold_open = true;
    sim.run_for(20_000_000, &mut |s| t.step(s));
    let first = path_mtu(&sim, &t);
    assert!((1284..=1300).contains(&first), "{first}");
    // The path improves; nothing changes until the 30 minute re-probe.
    sim.ab.cfg.max_dgram = 0;
    let mut more = 0usize;
    let start = sim.now;
    sim.run_for(25 * 60 * 1_000_000, &mut |s| {
        // a steady trickle of full-size traffic
        if s.now >= start + more as u64 * 5_000_000 {
            more += 1;
            t.a.tx.extend_from_slice(&pseudo_data(more as u64, 64 * 1024));
        }
        t.step(s);
        false
    });
    assert_eq!(path_mtu(&sim, &t), first, "no re-probe before 30 minutes");
    sim.run_for(10 * 60 * 1_000_000, &mut |s| {
        if s.now >= start + more as u64 * 5_000_000 {
            more += 1;
            t.a.tx.extend_from_slice(&pseudo_data(more as u64, 64 * 1024));
        }
        t.step(s);
        false
    });
    assert_eq!(path_mtu(&sim, &t), 1454, "re-probe found the bigger path");
    assert!(t.b.rx.len() > 4 * MB && t.a.tx.starts_with(&t.b.rx), "stream must stay byte-exact");
}

#[test]
fn discovery_can_be_switched_off() {
    let cfg = Config { mtu_discovery: false, ..Config::default() };
    let link = LinkCfg::clean(100_000_000, 10_000, 500_000);
    let mut sim = Sim::new(cfg, link.clone(), link, 307);
    let mut t = Transfer::new(pseudo_data(1, 2 * MB), Vec::new());
    t.a.hold_open = true;
    sim.run_for(10_000_000, &mut |s| t.step(s));
    assert_eq!(path_mtu(&sim, &t), 1454);
    assert!(t.b.rx == t.a.tx);
}
