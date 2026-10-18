//! Deterministic simulation: 1000 torrents x 50 hosts x 90 simulated days on a
//! virtual clock with injected faults (timeouts, 5xx, 429 + Retry-After,
//! garbage bodies, DNS failure, failure reason, host outages, a global
//! outage, sleep/wake jumps, network changes, crashes and a crash loop).
//!
//! Run: `cargo test --release -p trs-tracker --test simulation -- --nocapture`
//!
//! Asserted: all SPEC 7C.3 invariants continuously (independent Oracle), the
//! per-host request budget, the per-(torrent,tracker) announce-count bound,
//! zero "announce spam" as a tracker would detect it, clean wind-down.

use crate::common;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use common::*;
use trs_tracker::crash::CrashLoopGuard;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::response::{AnnounceError, ParseError, RetryIn, TransportError};
use trs_tracker::rng::{Rng, SplitMix64};
use trs_tracker::{InfoHash20, Millis, clamp_timing};

/// Fixed for reproducibility; `SIM_SEED=n` explores other worlds.
fn base_seed() -> u64 {
    0x7261_6e73_696d
        ^ std::env::var("SIM_SEED")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
}

struct Scale {
    torrents: u32,
    hosts: u32,
    days: u64,
}

fn scale() -> Scale {
    let full = Scale {
        torrents: 1000,
        hosts: 50,
        days: 90,
    };
    if let Ok(v) = std::env::var("SIM_SCALE") {
        let p: Vec<u64> = v.split('x').filter_map(|x| x.parse().ok()).collect();
        if p.len() == 3 {
            return Scale {
                torrents: p[0] as u32,
                hosts: p[1] as u32,
                days: p[2],
            };
        }
    }
    if cfg!(debug_assertions) {
        // Unoptimised builds are ~20x slower; keep `cargo test` quick.
        Scale {
            torrents: 100,
            hosts: 12,
            days: 12,
        }
    } else {
        full
    }
}

struct HostProfile {
    name: String,
    interval: u64,
    min: Option<u64>,
    /// Probability (per mille) that a request fails when the host is up.
    p_fail: u32,
}

impl HostProfile {
    fn min_eff(&self) -> Duration {
        Duration::from_secs(self.min.unwrap_or(300).max(60))
    }
}

struct SimNet {
    rng: SplitMix64,
    index: HashMap<String, usize>,
    hosts: Vec<HostProfile>,
    outages: Vec<Vec<(Millis, Millis)>>,
    global_down: Vec<(Millis, Millis)>,
    reject_pairs: HashSet<(u32, usize)>,
    last_ok: HashMap<(InfoHash20, usize), Millis>,
    spam: u64,
    faults: [u64; 9],
    answered_ok: u64,
    hourly: Vec<Vec<u32>>,
}

impl SimNet {
    fn in_window(w: &[(Millis, Millis)], now: Millis) -> bool {
        w.iter().any(|(a, b)| now >= *a && now < *b)
    }
}

impl Responder for SimNet {
    fn respond(&mut self, now: Millis, host: &str, req: &GatedRequest) -> Reply {
        let hi = self.index[host];
        let hour = (now.0 / 3_600_000) as usize;
        if let Some(c) = self.hourly[hi].get_mut(hour) {
            *c += 1;
        }
        let params = req.announce_params().expect("announce");
        let secs = Duration::from_secs;
        let torrent_no = u32::from_be_bytes([
            params.info_hash[0],
            params.info_hash[1],
            params.info_hash[2],
            params.info_hash[3],
        ]);
        let r = self.rng.next_u64();
        // outages
        if Self::in_window(&self.global_down, now) || Self::in_window(&self.outages[hi], now) {
            self.faults[0] += 1;
            return if r.is_multiple_of(3) {
                Reply::new(secs(0), Err(AnnounceError::Transport(TransportError::Dns)))
            } else if r % 3 == 1 {
                Reply::new(
                    secs(15),
                    Err(AnnounceError::Transport(TransportError::Connect)),
                )
            } else {
                Reply::new(
                    secs(30),
                    Err(AnnounceError::Transport(TransportError::Timeout)),
                )
            };
        }
        // permanent rejection of (torrent, tracker) pairs: "unregistered torrent"
        if self.reject_pairs.contains(&(torrent_no, hi))
            && params.event != Some(trs_tracker::Event::Stopped)
        {
            self.faults[1] += 1;
            return Reply::new(
                Duration::from_millis(200),
                Err(AnnounceError::Rejected {
                    reason: "Unregistered torrent".into(),
                    retry_in: None,
                }),
            );
        }
        let p = &self.hosts[hi];
        if (r % 1000) as u32 >= 1000 - p.p_fail.min(1000) {
            let k = self.rng.next_u64() % 100;
            let (slot, lat, res) = match k {
                0..=24 => (
                    2,
                    secs(30),
                    Err(AnnounceError::Transport(TransportError::Timeout)),
                ),
                25..=49 => (
                    3,
                    secs(1),
                    Err(AnnounceError::Http {
                        status: 503,
                        retry_after: None,
                    }),
                ),
                50..=64 => {
                    let ra = 60 + self.rng.next_u64() % 7200;
                    (
                        4,
                        secs(1),
                        Err(AnnounceError::Http {
                            status: 429,
                            retry_after: Some(secs(ra)),
                        }),
                    )
                }
                65..=79 => (
                    5,
                    secs(1),
                    Err(AnnounceError::Garbage(ParseError::NotBencode)),
                ),
                80..=89 => (
                    6,
                    secs(0),
                    Err(AnnounceError::Transport(TransportError::Dns)),
                ),
                90..=94 => (
                    7,
                    secs(1),
                    Err(AnnounceError::Rejected {
                        reason: "overloaded".into(),
                        retry_in: Some(RetryIn::Minutes(5 + (self.rng.next_u64() % 40) as u32)),
                    }),
                ),
                95..=97 => (
                    8,
                    secs(1),
                    // not permanent-looking: the slow retry schedule, never a halt
                    Err(AnnounceError::Rejected {
                        reason: "Database is under maintenance, try again later".into(),
                        retry_in: None,
                    }),
                ),
                _ => (
                    2,
                    secs(5000),
                    Err(AnnounceError::Transport(TransportError::Timeout)),
                ), // hang
            };
            self.faults[slot] += 1;
            return Reply::new(lat, res);
        }
        // The tracker's own view of "announce spam": a non-event announce sooner
        // than its min interval after the previous answered one (Transmission
        // 4.0.6 detector, SPEC 7C.4a).
        let key = (params.info_hash, hi);
        if params.event.is_none()
            && let Some(prev) = self.last_ok.get(&key)
                && now.since(*prev) < p.min_eff() {
                    self.spam += 1;
                }
        self.last_ok.insert(key, now);
        self.answered_ok += 1;
        let mut resp = ok_response(p.interval, p.min);
        if torrent_no.is_multiple_of(3) {
            resp.tracker_id = Some(format!("tid{hi}").into_bytes().into());
        }
        Reply::new(
            Duration::from_millis(80 + self.rng.next_u64() % 1500),
            Ok(resp),
        )
    }
}

#[derive(Debug, Clone)]
enum Act {
    Manual(u32),
    Pause(u32),
    Resume(u32),
    Complete(u32),
    Network,
    Sleep(Duration),
    Crash(bool),
    CrashLoop,
    Progress,
}

fn rand_time(rng: &mut SplitMix64, days: u64) -> Millis {
    Millis(rng.next_u64() % (days * 86_400_000))
}

#[test]
fn ninety_days() {
    let sc = scale();
    let wall = Instant::now();
    let mut rng = SplitMix64::new(base_seed());
    let total = Millis(sc.days * 86_400_000);

    // ---- hosts
    // Realistic announce intervals; (300, 300) is the rare aggressive tracker.
    let intervals = [
        (900u64, None),
        (1800, None),
        (1800, Some(900u64)),
        (3600, None),
        (1800, Some(1800)),
        (2700, Some(1200)),
        (1800, None),
        (3600, Some(1800)),
        (900, Some(600)),
        (1800, None),
    ];
    let mut hosts = Vec::new();
    let mut index = HashMap::new();
    for h in 0..sc.hosts {
        let (interval, min) = if h % 25 == 11 {
            (300, Some(300))
        } else {
            intervals[(rng.next_u64() % intervals.len() as u64) as usize]
        };
        let flaky = h % 7 == 3;
        hosts.push(HostProfile {
            name: format!("h{h}.sim"),
            interval,
            min,
            p_fail: if flaky { 120 } else { 8 },
        });
        index.insert(format!("h{h}.sim"), h as usize);
    }
    let mut outages: Vec<Vec<(Millis, Millis)>> = vec![vec![]; sc.hosts as usize];
    for h in (0..sc.hosts as usize).step_by(6) {
        for _ in 0..(1 + rng.next_u64() % 3) {
            let a = rand_time(&mut rng, sc.days.saturating_sub(4).max(1));
            let len = Duration::from_secs(7200 + rng.next_u64() % (3 * 86_400));
            outages[h].push((a, a + len));
        }
    }
    let day20 = Millis(sc.days.min(20) * 86_400_000 / 2 * 2).max(Millis(86_400_000));
    let global_down = vec![(day20, day20 + Duration::from_secs(3 * 3600))];

    // ---- torrents
    let host_name = |h: u64| format!("h{}.sim", h % u64::from(sc.hosts));
    let url_for = |h: u64, n: u32, private: bool| -> String {
        let host = host_name(h);
        let path = if private {
            format!("/{n:08x}deadbeef/announce")
        } else {
            "/announce".to_owned()
        };
        match h % 9 {
            0 => format!("udp://{host}:1337/announce"),
            1 | 2 => format!("https://{host}{path}"),
            _ => format!("http://{host}{path}"),
        }
    };
    let mut reject_pairs = HashSet::new();
    let mut seeds = Vec::new();
    let mut tracker_hosts: Vec<Vec<u64>> = Vec::new();
    for n in 1..=sc.torrents {
        let private = n % 3 == 0;
        let r = rng.next_u64();
        let h0 = r % 997;
        let layout = (r >> 20) % 10;
        let (tiers, hs): (Vec<Vec<String>>, Vec<u64>) = match layout {
            0..=5 => (vec![vec![url_for(h0, n, private)]], vec![h0]),
            6 | 7 => {
                let h1 = h0 + 1 + (r >> 30) % 13;
                (
                    vec![vec![url_for(h0, n, private), url_for(h1, n, private)]],
                    vec![h0, h1],
                )
            }
            8 => {
                let h1 = h0 + 1 + (r >> 30) % 13;
                (
                    vec![vec![url_for(h0, n, private)], vec![url_for(h1, n, private)]],
                    vec![h0, h1],
                )
            }
            _ => {
                let (h1, h2) = (h0 + 1 + (r >> 30) % 5, h0 + 7 + (r >> 40) % 5);
                (
                    vec![vec![
                        url_for(h0, n, private),
                        url_for(h1, n, private),
                        url_for(h2, n, private),
                    ]],
                    vec![h0, h1, h2],
                )
            }
        };
        // ~2% of (torrent, first host) pairs are permanently rejected
        if (r >> 50).is_multiple_of(50) {
            reject_pairs.insert((n, (h0 % u64::from(sc.hosts)) as usize));
        }
        seeds.push((n, private, tiers));
        tracker_hosts.push(hs);
    }

    let net = SimNet {
        rng: SplitMix64::new(base_seed() ^ 0xfeed),
        index,
        hosts,
        outages,
        global_down: global_down.clone(),
        reject_pairs,
        last_ok: HashMap::new(),
        spam: 0,
        faults: [0; 9],
        answered_ok: 0,
        hourly: vec![vec![0; (sc.days * 24 + 72) as usize]; sc.hosts as usize],
    };
    let mut w = World::new(net, GateConfig::default());
    w.oracle.trace_on = false;
    let mut complete_candidates = Vec::new();
    for (n, private, tiers) in &seeds {
        let refs: Vec<Vec<&str>> = tiers
            .iter()
            .map(|t| t.iter().map(String::as_str).collect())
            .collect();
        let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
        let mut s = spec(*n, 6881 + (*n % 1000) as u16, &slices);
        s.private = *private;
        let size = 700_000_000u64;
        // 50% added complete (cross-seeds), 30% download then complete, 20% leech forever
        let left = match n % 10 {
            0..=4 => 0,
            _ => size,
        };
        if (5..=7).contains(&(n % 10)) {
            complete_candidates.push(*n);
        }
        w.add_torrent(s, size, left);
    }

    // ---- script
    let mut script: Vec<(Millis, Act)> = Vec::new();
    for _ in 0..3 * sc.torrents {
        script.push((
            rand_time(&mut rng, sc.days),
            Act::Manual(1 + (rng.next_u64() % u64::from(sc.torrents)) as u32),
        ));
    }
    for _ in 0..sc.torrents / 3 {
        let n = 1 + (rng.next_u64() % u64::from(sc.torrents)) as u32;
        let t = rand_time(&mut rng, sc.days);
        let back = t + Duration::from_secs(60 + rng.next_u64() % (2 * 86_400));
        script.push((t, Act::Pause(n)));
        script.push((back, Act::Resume(n)));
    }
    for n in &complete_candidates {
        script.push((rand_time(&mut rng, sc.days * 2 / 3), Act::Complete(*n)));
    }
    for _ in 0..(sc.days / 7).max(1) {
        script.push((rand_time(&mut rng, sc.days), Act::Network));
    }
    for _ in 0..6 {
        script.push((
            rand_time(&mut rng, sc.days.saturating_sub(3).max(1)),
            Act::Sleep(Duration::from_secs(1200 + rng.next_u64() % (40 * 3600))),
        ));
    }
    for _ in 0..10 {
        script.push((
            rand_time(&mut rng, sc.days),
            Act::Crash(rng.next_u64().is_multiple_of(2)),
        ));
    }
    let loop_at = Millis(sc.days * 86_400_000 * 4 / 9);
    for k in 0..6u64 {
        script.push((loop_at + Duration::from_secs(k * 70), Act::CrashLoop));
    }
    let mut t = Millis(6 * 3_600_000);
    while t < total {
        script.push((t, Act::Progress));
        t = t + Duration::from_secs(6 * 3600);
    }
    script.sort_by_key(|(t, _)| *t);

    // ---- run
    let all: Vec<InfoHash20> = (1..=sc.torrents).map(ih).collect();
    for h in &all {
        w.start(*h);
    }
    let guard = CrashLoopGuard::default();
    let mut launches: Vec<Millis> = vec![Millis::ZERO];
    let mut manual_count: HashMap<u32, u64> = HashMap::new();
    let mut network_count = 0u64;
    let mut prog = SplitMix64::new(99);
    for (t, act) in script {
        if t > w.now {
            w.run_until(t);
        }
        match act {
            Act::Manual(n) => {
                *manual_count.entry(n).or_default() += 1;
                w.manual(ih(n));
            }
            Act::Pause(n) => w.pause(ih(n)),
            Act::Resume(n) => w.resume(ih(n)),
            Act::Complete(n) => w.complete(ih(n)),
            Act::Network => {
                network_count += 1;
                w.network_changed();
            }
            Act::Sleep(d) => w.sleep(d),
            Act::Crash(id) => {
                w.crash(id);
                launches.push(w.now);
            }
            Act::CrashLoop => {
                w.crash(true);
                launches.push(w.now);
                if let Some(until) = guard.evaluate(&launches, w.now) {
                    w.hold_until = w.hold_until.max(until);
                    w.hub().hold_until(until);
                }
            }
            Act::Progress => {
                for h in &all {
                    let up = prog.next_u64() % 5_000_000;
                    let down = prog.next_u64() % 40_000_000;
                    w.progress(*h, up, down);
                }
            }
        }
        w.pump();
    }
    w.run_until(total);
    let elapsed_s = w.now.0 / 1000;

    // ---- wind down: every torrent stopped, everything quiet
    for h in &all {
        w.stop(*h);
    }
    w.run_for(Duration::from_secs(3 * 86_400));
    for h in &all {
        assert!(
            w.hub_ref().is_quiescent(h),
            "torrent {h:02x?} still active after stop"
        );
    }
    let sent_after_quiet = w.total_sent;
    w.run_for(Duration::from_secs(7 * 86_400));
    assert_eq!(
        w.total_sent, sent_after_quiet,
        "stopped torrents must be silent"
    );

    // ---- assertions & report
    assert_eq!(
        w.responder.spam, 0,
        "a tracker would have flagged announce spam {} times",
        w.responder.spam
    );
    assert_eq!(w.hub_ref().store_errors(), 0);

    let burst = w.gate_cfg.burst as u64;
    let mut host_rows: Vec<(String, u64, u32)> = Vec::new();
    for (i, hp) in w.responder.hosts.iter().enumerate() {
        let sent = w.per_host_sent.get(&hp.name).copied().unwrap_or(0);
        let max_hour = w.responder.hourly[i].iter().copied().max().unwrap_or(0);
        // hard ceiling: bucket
        assert!(
            sent <= burst + (w.now.0 / 1000),
            "{} exceeded the bucket: {sent}",
            hp.name
        );
        assert!(
            u64::from(max_hour) <= burst + 3600,
            "{} hourly {max_hour}",
            hp.name
        );
        host_rows.push((hp.name.clone(), sent, max_hour));
    }

    // per (torrent, tracker) announce-count bound
    let mut worst: (f64, String) = (0.0, String::new());
    let mut violations = Vec::new();
    let crashes = u64::from(w.crashes);
    let mut keys = 0u64;
    let mut max_attempts = 0u64;
    for (&(h, slot), &attempts) in &w.per_key_sent {
        keys += 1;
        let n = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let set = w.hub_ref().set(&h).expect("torrent");
        let url = set.url(slot).expect("slot");
        let hi = w.responder.index[url.host()];
        let hp = &w.responder.hosts[hi];
        let t = clamp_timing(Some(hp.interval as i64), hp.min.map(|m| m as i64));
        let active_s = w.active_ms(&h) as f64 / 1000.0;
        let events = w.per_key_events.get(&(h, slot)).copied().unwrap_or(0);
        let licences = manual_count.get(&n).copied().unwrap_or(0) + network_count;
        let ks = w.key_stats.get(&(h, slot)).cloned().unwrap_or_default();
        let success_bound = (active_s / t.interval.as_secs_f64()).ceil() as u64 + events + licences;
        let failure_bound = 7 * (ks.streaks + crashes) + ks.fail_ms / 3_600_000 + 2;
        let bound = success_bound + failure_bound;
        max_attempts = max_attempts.max(attempts);
        let ratio = attempts as f64 / bound.max(1) as f64;
        if ratio > worst.0 {
            worst = (
                ratio,
                format!(
                    "torrent {n} slot {slot} {url}: {attempts} attempts, bound {bound} (success {success_bound} + failure {failure_bound})"
                ),
            );
        }
        if attempts > bound {
            violations.push(format!("torrent {n} slot {slot}: {attempts} > {bound}"));
        }
    }
    assert!(
        violations.is_empty(),
        "announce-count bound violated:\n{}",
        violations.join("\n")
    );

    host_rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    let total_reqs: u64 = host_rows.iter().map(|r| r.1).sum();
    println!(
        "\n=== simulation: {} torrents x {} hosts x {} days ({} s simulated) ===",
        sc.torrents, sc.hosts, sc.days, elapsed_s
    );
    println!("wall time: {:.1} s", wall.elapsed().as_secs_f64());
    println!(
        "requests released: {total_reqs}  (per simulated second: {:.3})",
        total_reqs as f64 / elapsed_s as f64
    );
    println!(
        "answered ok: {}, faults injected: outage={} rejected-pair={} timeout={} 5xx={} 429={} garbage={} dns={} retry-in={} maintenance={}",
        w.responder.answered_ok,
        w.responder.faults[0],
        w.responder.faults[1],
        w.responder.faults[2],
        w.responder.faults[3],
        w.responder.faults[4],
        w.responder.faults[5],
        w.responder.faults[6],
        w.responder.faults[7],
        w.responder.faults[8]
    );
    println!(
        "per-host totals: max {} ({}), min {}, mean {:.0}",
        host_rows[0].1,
        host_rows[0].0,
        host_rows.last().map_or(0, |r| r.1),
        total_reqs as f64 / host_rows.len() as f64
    );
    println!(
        "per-host max requests in one hour: {}",
        host_rows.iter().map(|r| r.2).max().unwrap_or(0)
    );
    println!(
        "max requests to one host within any 1 s window: {} (bucket burst {})",
        w.oracle.max_sends_in_1s_window, burst
    );
    println!(
        "(torrent,tracker) pairs announced: {keys}; max attempts for one pair: {max_attempts}"
    );
    println!("worst attempts/bound ratio: {:.3}  [{}]", worst.0, worst.1);
    println!(
        "tracker-side announce-spam detections: {}",
        w.responder.spam
    );
    println!("circuit-breaker openings: {}", w.breaker_opens);
    println!("crashes: {}, launches: {}", w.crashes, launches.len());
    assert!(w.oracle.max_sends_in_1s_window as u64 <= burst + 1);
    if !cfg!(debug_assertions) {
        assert!(
            wall.elapsed() < Duration::from_secs(60),
            "simulation too slow: {:?}",
            wall.elapsed()
        );
    }
}
