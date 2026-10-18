//! Simulated-network tests: 2,000 virtual nodes on virtual time.

use crate::sim;

use sim::{Sim, node_addr, sim_config};
use std::net::SocketAddrV4;
use trs_dht::rng::Rng;
use trs_dht::{Event, Id};

fn report(sim: &Sim, label: &str) {
    let mut lens: Vec<usize> = sim.nodes.iter().map(|n| n.table_len()).collect();
    lens.sort_unstable();
    eprintln!(
        "[{label}] nodes={} avg_table={:.1} min={} median={} max={} packets={}",
        sim.nodes.len(),
        sim.avg_table(),
        lens[0],
        lens[lens.len() / 2],
        lens[lens.len() - 1],
        sim.packets
    );
}

#[test]
fn small_network_forms_and_is_deterministic() {
    let run = || {
        let mut s = Sim::new(120, 7, |_| sim_config());
        s.join_all(200, 120_000);
        (s.transcript, s.packets, s.avg_table())
    };
    let a = run();
    let b = run();
    assert_eq!(a.0, b.0, "transcript must be bit-identical across runs");
    assert_eq!(a.1, b.1);
    assert!(a.2 > 10.0, "avg table {}", a.2);
}

fn check_lookup_quality(
    sim: &mut Sim,
    announcers: &[usize],
    hashes: usize,
    rng: &mut Rng,
) -> (f64, f64, f64, f64) {
    let n = sim.nodes.len();
    let mut found_ratio = 0.0;
    let mut queries = 0.0;
    let mut secs = 0.0;
    let mut ok = 0.0;
    for h in 0..hashes {
        let ih: Id = rng.id();
        for (k, &a) in announcers.iter().enumerate() {
            sim.announce(a, ih, 20_000 + k as u16);
        }
        sim.run_for(90_000);
        // A node that did not announce asks.
        let mut asker = rng.below(n as u64) as usize;
        while announcers.contains(&asker) || sim.dead.contains(&asker) {
            asker = rng.below(n as u64) as usize;
        }
        let t0 = sim.now_ms;
        sim.get_peers(asker, ih);
        sim.run_for(60_000);
        let peers = sim.peers_found(asker, &ih, t0);
        let want: Vec<SocketAddrV4> = announcers
            .iter()
            .enumerate()
            .map(|(k, &a)| SocketAddrV4::new(*node_addr(a).ip(), 20_000 + k as u16))
            .collect();
        let hit = want.iter().filter(|w| peers.contains(w)).count();
        let (tfin, st) = sim
            .finished(asker, &ih, t0)
            .unwrap_or_else(|| panic!("lookup {h} did not finish"));
        found_ratio += hit as f64 / want.len() as f64;
        queries += f64::from(st.queries);
        secs += (tfin - t0) as f64 / 1000.0;
        if hit * 10 >= want.len() * 9 {
            ok += 1.0;
        }
    }
    let h = hashes as f64;
    (found_ratio / h, queries / h, secs / h, ok / h)
}

#[test]
fn two_thousand_nodes_converge_and_find_announced_peers() {
    let t = std::time::Instant::now();
    let mut sim = Sim::new(2000, 42, |_| sim_config());
    sim.join_all(100, 300_000);
    report(&sim, "joined");
    let empty = sim.nodes.iter().filter(|n| n.table_len() < 8).count();
    assert!(empty < 20, "{empty} nodes with <8 contacts");
    assert!(sim.avg_table() > 40.0, "avg {}", sim.avg_table());
    let mem: Vec<usize> = sim.nodes.iter().map(|n| n.memory_bytes()).collect();
    let (avg, max) = (
        mem.iter().sum::<usize>() / mem.len(),
        mem.iter().max().unwrap(),
    );
    eprintln!(
        "memory per node after join: avg {avg} B, max {max} B (table avg {:.1} nodes)",
        sim.avg_table()
    );
    assert!(*max < 64 * 1024, "idle node memory {max}");
    let q_join = sim.total_queries();
    eprintln!(
        "join: {} queries total ({:.1}/node), wall {:?}",
        q_join,
        q_join as f64 / 2000.0,
        t.elapsed()
    );

    let mut rng = Rng::from_seed(99);
    let announcers: Vec<usize> = (0..30).map(|_| 100 + rng.below(1800) as usize).collect();
    let (ratio, q, secs, ok) = check_lookup_quality(&mut sim, &announcers, 10, &mut rng);
    eprintln!(
        "lookup quality over 10 hashes (30 announcers): found {:.3} of peers, {:.1} queries/lookup, {:.2}s virtual, {:.0}% lookups >=90%",
        ratio,
        q,
        secs,
        ok * 100.0
    );
    assert!(ratio > 0.9, "found ratio {ratio}");
    assert!(ok >= 0.9);
    assert!(q < 100.0, "queries/lookup {q}");
    assert!(secs < 30.0, "lookup time {secs}");
}

#[test]
fn lookups_survive_loss_and_dead_nodes() {
    let mut sim = Sim::new(600, 5, |_| sim_config());
    sim.join_all(100, 200_000);
    let mut rng = Rng::from_seed(11);
    // 15% of nodes vanish silently and the network drops 10% of datagrams.
    for _ in 0..90 {
        let d = 10 + rng.below(590) as usize;
        sim.dead.insert(d);
    }
    sim.loss_pct = 10;
    let announcers: Vec<usize> = (0..20)
        .map(|_| 10 + rng.below(590) as usize)
        .filter(|a| !sim.dead.contains(a))
        .collect();
    let (ratio, q, secs, ok) = check_lookup_quality(&mut sim, &announcers, 6, &mut rng);
    eprintln!(
        "lossy: found {:.3}, {:.1} queries/lookup, {:.2}s, ok {:.0}%",
        ratio,
        q,
        secs,
        ok * 100.0
    );
    assert!(ratio > 0.7, "found ratio {ratio}");
}

#[test]
fn idle_network_is_nearly_silent() {
    let mut sim = Sim::new(400, 3, |_| sim_config());
    sim.join_all(100, 300_000);
    // Let startup refreshes burn off, then measure one quiet hour with no commands.
    sim.run_for(30 * 60_000);
    let q0 = sim.total_queries();
    let p0 = sim.packets;
    let hour = 3_600_000;
    sim.run_for(hour);
    let q = sim.total_queries() - q0;
    let per_node_per_min = q as f64 / 400.0 / 60.0;
    eprintln!(
        "idle hour: {} queries ({:.2} per node per minute), {} datagrams total",
        q,
        per_node_per_min,
        sim.packets - p0
    );
    assert!(
        per_node_per_min < 5.0,
        "idle chatter {per_node_per_min}/node/min"
    );
    // No lookups were started by anyone: only refreshes.
    assert!(
        sim.events
            .iter()
            .all(|(t, _, e)| !matches!(e, Event::Peers { .. }) || *t < sim.now_ms - hour)
    );
}
