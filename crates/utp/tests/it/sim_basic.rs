//! Deterministic virtual-time scenarios: integrity, throughput, loss, reordering, duplication,
//! jitter, blackouts, close semantics.

use crate::common;

use common::*;
use trs_utp::{Config, End, State};

const MB: usize = 1024 * 1024;

fn run_transfer(sim: &mut Sim, a2b: usize, b2a: usize, limit_s: u64) -> Transfer {
    let mut t = Transfer::new(pseudo_data(1, a2b), pseudo_data(2, b2a));
    let ok = sim.run_for(limit_s * 1_000_000, &mut |s| t.step(s));
    assert!(ok, "transfer did not finish in {limit_s} virtual seconds (a: {}/{} rx {}, b: {}/{} rx {})",
        t.a.pos, t.a.tx.len(), t.a.rx.len(), t.b.pos, t.b.tx.len(), t.b.rx.len());
    assert_eq!(t.a.err, None);
    assert_eq!(t.b.err, None);
    assert!(t.b.rx == t.a.tx, "A->B corrupted: got {} want {}", t.b.rx.len(), t.a.tx.len());
    assert!(t.a.rx == t.b.tx, "B->A corrupted: got {} want {}", t.a.rx.len(), t.b.tx.len());
    t
}

#[test]
fn clean_path_goodput() {
    let link = LinkCfg::clean(10_000_000, 20_000, 300_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 7);
    let t = run_transfer(&mut sim, 8 * MB, 0, 60);
    let secs = (t.b.done_at - t.start) as f64 / 1e6;
    let goodput = 8.0 * MB as f64 * 8.0 / secs;
    let pct = goodput / 10_000_000.0 * 100.0;
    println!("clean: {:.2} s, goodput {:.2} Mbit/s = {:.1}% of link, drops q={} retx={}",
        secs, goodput / 1e6, pct, sim.ab.dropped_queue, sim.a.conn_stats(t.a.handle.unwrap()).map_or(0, |s| s.retransmits));
    assert!(pct >= 80.0, "only {pct:.1}% of the link");
}

#[test]
fn bidirectional_clean() {
    let link = LinkCfg::clean(20_000_000, 15_000, 400_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 11);
    let t = run_transfer(&mut sim, 4 * MB, 4 * MB, 60);
    // both connections wind down completely once the app closes them
    let mut t = t;
    t.close_both(&mut sim);
    let mut idle = |_: &mut Sim| false;
    sim.run_for(10_000_000, &mut idle);
    assert_eq!(sim.a.live_connections(), 0);
    assert_eq!(sim.b.live_connections(), 0);
}

struct Row {
    name: &'static str,
    secs: f64,
    mbit: f64,
    retx: u32,
    timeouts: u32,
}

fn lossy(name: &'static str, cfg: LinkCfg, bytes: usize, seed: u64) -> Row {
    let mut sim = Sim::new(Config::default(), cfg.clone(), cfg, seed);
    let t = run_transfer(&mut sim, bytes, bytes, 900);
    let secs = (t.a.done_at.max(t.b.done_at) - t.start) as f64 / 1e6;
    let st = sim.a.conn_stats(t.a.handle.unwrap()).unwrap();
    let st2 = sim.b.conn_stats(t.b.handle.unwrap()).unwrap();
    let row = Row {
        name,
        secs,
        mbit: bytes as f64 * 8.0 / secs / 1e6,
        retx: st.retransmits + st2.retransmits,
        timeouts: st.timeouts + st2.timeouts,
    };
    println!(
        "{:<28} {:>8.2} s  {:>6.2} Mbit/s per direction  retransmits {:>5}  RTOs {:>3}",
        row.name, row.secs, row.mbit, row.retx, row.timeouts
    );
    row
}

#[test]
fn loss_reorder_duplication_jitter() {
    let base = LinkCfg::clean(10_000_000, 20_000, 200_000);
    let n = 2 * MB;
    let r1 = lossy("loss 1%", LinkCfg { loss: 0.01, ..base.clone() }, n, 21);
    let r5 = lossy("loss 5%", LinkCfg { loss: 0.05, ..base.clone() }, n, 22);
    let r10 = lossy("loss 10%", LinkCfg { loss: 0.10, ..base.clone() }, n, 23);
    let _ = lossy(
        "reorder 10% (+30 ms)",
        LinkCfg { reorder: 0.10, reorder_extra_us: 30_000, ..base.clone() },
        n,
        24,
    );
    let _ = lossy("duplicate 5%", LinkCfg { dup: 0.05, ..base.clone() }, n, 25);
    let _ = lossy("jitter 0-40 ms", LinkCfg { jitter_us: 40_000, ..base.clone() }, n, 26);
    let _ = lossy(
        "loss 3% + reorder + dup + jitter",
        LinkCfg {
            loss: 0.03,
            reorder: 0.05,
            reorder_extra_us: 25_000,
            dup: 0.03,
            jitter_us: 20_000,
            ..base.clone()
        },
        n,
        27,
    );
    assert!(r1.mbit > r5.mbit && r5.mbit > r10.mbit * 0.8, "throughput should fall with loss");
    assert!(r10.mbit > 0.3, "10% loss must still make progress, got {}", r10.mbit);
    assert!(r1.timeouts < 40);
}

#[test]
fn survives_30s_blackout() {
    let link = LinkCfg::clean(10_000_000, 20_000, 200_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 31);
    sim.blackout = Some((START + 3_000_000, START + 33_000_000));
    let t = run_transfer(&mut sim, 3 * MB, 3 * MB, 300);
    let done = t.a.done_at.max(t.b.done_at);
    println!("blackout: finished at +{:.1} s (outage 3.0..33.0 s)", (done - t.start) as f64 / 1e6);
    assert!(done > START + 33_000_000);
    // recovery must not wait for another full backoff cycle after the link returns
    assert!(done < START + 33_000_000 + 25_000_000, "slow recovery: {}", (done - START) as f64 / 1e6);
}

#[test]
fn permanent_blackout_times_out_and_cleans_up() {
    let link = LinkCfg::clean(10_000_000, 20_000, 200_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 32);
    let mut t = Transfer::new(pseudo_data(5, 4 * MB), Vec::new());
    sim.blackout = Some((START + 2_000_000, u64::MAX));
    let ok = sim.run_for(400_000_000, &mut |s| {
        t.step(s);
        t.a.err.is_some()
    });
    assert!(ok, "app should observe the failure");
    assert_eq!(t.a.err, Some(std::io::ErrorKind::TimedOut), "A saw {:?}", t.a.err);
    let died = (sim.now - (START + 2_000_000)) as f64 / 1e6;
    println!("permanent blackout: sender gave up {died:.1} s after the outage began");
    assert!(died > 30.0 && died < 120.0, "gave up after {died} s");
    // closing frees the slot immediately; B's side dies of idleness
    t.close_both(&mut sim);
    let mut idle = |_: &mut Sim| false;
    sim.run_for(250_000_000, &mut idle);
    assert_eq!(sim.a.live_connections(), 0);
    assert_eq!(sim.b.live_connections(), 0);
    assert!(sim.a.poll_timeout().is_none(), "no timers may remain");
    assert!(sim.b.poll_timeout().is_none());
}

#[test]
fn slow_reader_zero_window_recovers() {
    let link = LinkCfg::clean(20_000_000, 10_000, 200_000);
    let cfg = Config { recv_buffer: 64 * 1024, ..Config::default() };
    let mut sim = Sim::new(cfg, link.clone(), link, 41);
    let mut t = Transfer::new(pseudo_data(8, 2 * MB), Vec::new());
    t.b.read_pause_until = START + 20_000_000;
    sim.wake_at(START + 20_000_000);
    let ok = sim.run_for(120_000_000, &mut |s| t.step(s));
    assert!(ok);
    assert!(t.b.rx == t.a.tx);
    assert!(t.b.done_at > START + 20_000_000);
    let st = sim.a.conn_stats(t.a.handle.unwrap()).unwrap();
    assert_eq!(st.timeouts, 0, "a closed window is not a timeout: {st:?}");
}

#[test]
fn closed_window_survives_lost_window_update() {
    // Drop everything B sends for a while right when its app starts reading, so the window-update
    // ack is lost; the persist probe must reopen the flow.
    let link = LinkCfg::clean(20_000_000, 10_000, 200_000);
    let cfg = Config { recv_buffer: 32 * 1024, ..Config::default() };
    let mut sim = Sim::new(cfg, link.clone(), link, 42);
    let mut t = Transfer::new(pseudo_data(9, MB), Vec::new());
    t.b.read_pause_until = START + 5_000_000;
    sim.wake_at(START + 5_000_000);
    sim.ba.cfg.loss = 0.5;
    let ok = sim.run_for(200_000_000, &mut |s| t.step(s));
    assert!(ok);
    assert!(t.b.rx == t.a.tx);
}

#[test]
fn syn_loss_and_unreachable_peer() {
    // First SYN lost: retransmitted after 1 s and the connection still comes up.
    let link = LinkCfg::clean(10_000_000, 20_000, 100_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link.clone(), 51);
    sim.blackout = Some((START, START + 500_000));
    let t = run_transfer(&mut sim, 100_000, 100_000, 30);
    assert!(t.start + 1_000_000 < t.b.done_at);

    // Nobody home: connect fails with TimedOut after ~15 s (1+2+4+8).
    let mut sim = Sim::new(Config::default(), link.clone(), link, 52);
    sim.blackout = Some((0, u64::MAX));
    let mut t = Transfer::new(vec![1; 10], vec![]);
    let ok = sim.run_for(60_000_000, &mut |s| {
        t.step(s);
        t.a.err.is_some()
    });
    assert!(ok);
    assert_eq!(t.a.err, Some(std::io::ErrorKind::TimedOut));
    let secs = (sim.now - START) as f64 / 1e6;
    assert!((14.0..=17.0).contains(&secs), "connect gave up after {secs} s");
}

#[test]
fn graceful_close_survives_fin_loss() {
    let base = LinkCfg { loss: 0.2, ..LinkCfg::clean(10_000_000, 20_000, 100_000) };
    let mut sim = Sim::new(Config::default(), base.clone(), base, 61);
    let mut t = run_transfer(&mut sim, 200_000, 200_000, 120);
    // The application closes; both connections must reach Closed and disappear.
    t.close_both(&mut sim);
    let mut idle = |_: &mut Sim| false;
    sim.run_for(120_000_000, &mut idle);
    assert_eq!(sim.a.live_connections(), 0);
    assert_eq!(sim.b.live_connections(), 0);
}

#[test]
fn half_close_keeps_reverse_direction_open() {
    let link = LinkCfg::clean(10_000_000, 20_000, 100_000);
    let mut sim = Sim::new(Config::default(), link.clone(), link, 62);
    // A sends 50 KB and half-closes immediately; B only starts replying after it saw EOF.
    let mut a = Side::new(pseudo_data(3, 50_000));
    let mut b = Side::new(Vec::new());
    b.hold_open = true;
    let reply = pseudo_data(4, 300_000);
    let mut ha = None;
    let ok = sim.run_for(60_000_000, &mut |s| {
        if ha.is_none() {
            ha = Some(s.a.connect(s.now, s.addr_b).unwrap());
            a.handle = ha;
        }
        if b.handle.is_none() {
            b.handle = s.b.poll_accept();
        }
        let now = s.now;
        pump(&mut s.a, &mut a, now);
        pump(&mut s.b, &mut b, now);
        if b.eof && b.tx.is_empty() {
            b.tx = reply.clone(); // half-closed peer can still write
            b.hold_open = false;
        }
        a.eof
    });
    assert!(ok);
    assert!(b.rx == a.tx);
    assert!(a.rx == reply);
    let st = sim.a.status(a.handle.unwrap());
    assert!(matches!(st, trs_utp::ConnStatus::Open | trs_utp::ConnStatus::Closed(End::Graceful)), "{st:?}");
    let _ = State::Closed;
}

#[test]
fn sequence_numbers_wrap_cleanly() {
    // 24 MiB in 256-byte packets is ~98k packets in each direction: both 16-bit sequence spaces
    // wrap at least once, under 1% loss so SACK/retransmit also cross the wrap.
    let cfg = Config { max_payload: 256, ..Config::default() };
    let link = LinkCfg { loss: 0.01, ..LinkCfg::clean(200_000_000, 5_000, 500_000) };
    let mut sim = Sim::new(cfg, link.clone(), link, 81);
    let t = run_transfer(&mut sim, 24 * MB, 2 * MB, 600);
    let sent = sim.utp_bytes_ab / 256;
    println!("wrap: {} packets A->B, retransmits {}", sent, sim.a.conn_stats(t.a.handle.unwrap()).unwrap().retransmits);
    assert!(sent > 70_000);
}

#[test]
fn many_connections_share_one_endpoint_pair() {
    let link = LinkCfg::clean(50_000_000, 10_000, 400_000);
    let cfg = Config { max_per_ip: 128, ..Config::default() };
    let mut sim = Sim::new(cfg, link.clone(), link, 91);
    const N: usize = 60;
    let mut sides_a: Vec<Side> = (0..N).map(|i| Side::new(pseudo_data(100 + i as u64, 150_000 + i * 1000))).collect();
    let mut sides_b: Vec<Side> = (0..N).map(|i| Side::new(pseudo_data(200 + i as u64, 80_000))).collect();
    let mut connected = false;
    let mut order_b: Vec<trs_utp::ConnHandle> = Vec::new();
    let ok = sim.run_for(120_000_000, &mut |s| {
        if !connected {
            connected = true;
            for a in sides_a.iter_mut() {
                a.handle = Some(s.a.connect(s.now, s.addr_b).unwrap());
            }
        }
        while let Some(h) = s.b.poll_accept() {
            order_b.push(h);
        }
        let now = s.now;
        for a in sides_a.iter_mut() {
            pump(&mut s.a, a, now);
        }
        // Map each accepted connection to the client it belongs to by its first byte pattern is
        // not possible, so pair them by arrival order of data: assign on first read below.
        for (i, h) in order_b.iter().enumerate() {
            if sides_b[i].handle.is_none() {
                sides_b[i].handle = Some(*h);
            }
        }
        for b in sides_b.iter_mut() {
            pump(&mut s.b, b, now);
        }
        sides_a.iter().all(|x| x.eof) && sides_b.iter().all(|x| x.eof)
    });
    assert!(ok, "not all {N} connections finished");
    // Accept order need not match connect order, so compare multisets of received streams.
    let mut want: Vec<Vec<u8>> = sides_a.iter().map(|a| a.tx.clone()).collect();
    let mut got: Vec<Vec<u8>> = sides_b.iter().map(|b| b.rx.clone()).collect();
    want.sort();
    got.sort();
    assert!(want == got, "A->B streams differ");
    let mut want: Vec<Vec<u8>> = sides_b.iter().map(|b| b.tx.clone()).collect();
    let mut got: Vec<Vec<u8>> = sides_a.iter().map(|a| a.rx.clone()).collect();
    want.sort();
    got.sort();
    assert!(want == got, "B->A streams differ");
}
