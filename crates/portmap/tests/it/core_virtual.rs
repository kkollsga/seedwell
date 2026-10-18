//! The sans-I/O core against pure mock gateways on a virtual clock: schedules,
//! renewals, back-off, reboots, shutdown and a rate bound under a hostile gateway.

use crate::common;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use common::*;
use trs_portmap::*;

const GW: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
const GW_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(GW, 5351));
const PORT: u16 = 51413;

fn net() -> NetInfo {
    NetInfo {
        gateway: GW,
        local: LOCAL,
    }
}

fn cfg(methods: &[Method]) -> Config {
    Config {
        wanted: Wanted::tcp_and_udp(PORT),
        methods: methods.to_vec(),
        seed: 1,
        ..Config::default()
    }
}

enum Pending {
    Udp(SocketAddr, Vec<u8>),
    Http(u64, Result<Vec<u8>, HttpFailure>),
}

struct World {
    core: PortMapper,
    gw: MockGateway,
    igd: MockIgd,
    igd_on: bool,
    ssdp_location: String,
    now: u64,
    events: Vec<(u64, Event)>,
    /// datagrams sent to the gateway's NAT-PMP/PCP port
    sent: Vec<(u64, Vec<u8>)>,
    ssdp_sent: Vec<u64>,
    http_sent: Vec<(u64, String)>,
}

impl World {
    fn new(cfg: Config) -> Self {
        let mut gw = MockGateway::new(GatewayBehavior::default());
        gw.client_ip = LOCAL;
        World {
            core: PortMapper::new(cfg),
            gw,
            igd: MockIgd::default(),
            igd_on: false,
            ssdp_location: format!("http://{GW}:5000/rootDesc.xml"),
            now: 0,
            events: Vec::new(),
            sent: Vec::new(),
            ssdp_sent: Vec::new(),
            http_sent: Vec::new(),
        }
    }

    fn start(&mut self) {
        self.feed(Input::Network(Some(net())));
    }

    fn feed(&mut self, input: Input<'_>) {
        let outs = self.core.handle(input, Millis(self.now));
        let mut queue = VecDeque::new();
        self.process(outs, &mut queue);
        while let Some(p) = queue.pop_front() {
            let outs = match &p {
                Pending::Udp(from, data) => self
                    .core
                    .handle(Input::Udp { from: *from, data }, Millis(self.now)),
                Pending::Http(id, res) => self.core.handle(
                    Input::HttpDone {
                        id: *id,
                        outcome: res.clone(),
                    },
                    Millis(self.now),
                ),
            };
            self.process(outs, &mut queue);
        }
    }

    fn process(&mut self, outs: Vec<Output>, queue: &mut VecDeque<Pending>) {
        for o in outs {
            match o {
                Output::Udp { to, data } if to == GW_ADDR => {
                    self.sent.push((self.now, data.clone()));
                    if let Some(r) = self.gw.handle(&data, self.now) {
                        queue.push_back(Pending::Udp(GW_ADDR, r));
                    }
                }
                Output::Udp { to, data } => {
                    assert_eq!(
                        to,
                        SocketAddr::V4(trs_portmap::ssdp::MULTICAST),
                        "unexpected destination"
                    );
                    assert!(data.starts_with(b"M-SEARCH"));
                    self.ssdp_sent.push(self.now);
                    if self.igd_on {
                        queue.push_back(Pending::Udp(
                            SocketAddr::V4(SocketAddrV4::new(GW, 1900)),
                            ssdp_reply(&self.ssdp_location),
                        ));
                    }
                }
                Output::Http(req) => {
                    let first = String::from_utf8_lossy(&req.request)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    self.http_sent.push((self.now, first));
                    let res = if self.igd_on && *req.to.ip() == GW {
                        Ok(self.igd.handle_http(&req.request, &req.to.to_string()))
                    } else {
                        Err(HttpFailure::Connect)
                    };
                    queue.push_back(Pending::Http(req.id, res));
                }
                Output::Event(e) => self.events.push((self.now, e)),
            }
        }
    }

    /// Advance virtual time by `ms`, firing the core's timers on the way.
    fn run_for(&mut self, ms: u64) {
        let target = self.now + ms;
        let mut spins = 0u32;
        while let Some(w) = self.core.next_wakeup() {
            if w.0 > target {
                break;
            }
            self.now = w.0.max(self.now);
            self.feed(Input::Tick);
            spins += 1;
            assert!(
                spins < 2_000_000,
                "core keeps waking up without progress at t={}",
                self.now
            );
        }
        self.now = target;
        self.feed(Input::Tick);
    }

    fn evs(&self) -> Vec<&Event> {
        self.events.iter().map(|(_, e)| e).collect()
    }

    fn count(&self, f: impl Fn(&Event) -> bool) -> usize {
        self.events.iter().filter(|(_, e)| f(e)).count()
    }

    fn time_of(&self, f: impl Fn(&Event) -> bool) -> Option<u64> {
        self.events.iter().find(|(_, e)| f(e)).map(|(t, _)| *t)
    }

    /// Times of mapping requests (NAT-PMP, TCP opcode 2) sent to the gateway.
    fn tcp_map_times(&self) -> Vec<u64> {
        self.sent
            .iter()
            .filter(|(_, d)| d.len() == 12 && d[1] == 2)
            .map(|(t, _)| *t)
            .collect()
    }
}

const H: u64 = 3_600_000;

#[test]
fn natpmp_discovery_maps_tcp_and_udp_serially() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1000);
    let lens: Vec<usize> = w.sent.iter().map(|(_, d)| d.len()).collect();
    assert_eq!(
        lens,
        [2, 12, 12],
        "ext-addr probe, then TCP, then UDP; no retransmits"
    );
    assert_eq!(w.sent[1].1[1], 2);
    assert_eq!(w.sent[2].1[1], 1);
    // asks for the same external port and 2 h
    assert_eq!(
        &w.sent[1].1[4..12],
        &[0xc8, 0xd5, 0xc8, 0xd5, 0, 0, 0x1c, 0x20]
    );
    let evs = w.evs();
    assert_eq!(evs[0], &Event::MethodSelected(Method::NatPmp));
    assert_eq!(
        evs[1],
        &Event::ExternalIp {
            ip: EXTERNAL_IP,
            public: true
        }
    );
    for (e, p) in [(evs[2], Protocol::Tcp), (evs[3], Protocol::Udp)] {
        assert_eq!(
            e,
            &Event::Mapped(Mapping {
                protocol: p,
                internal_port: PORT,
                external_port: PORT,
                external_ip: Some(EXTERNAL_IP),
                lifetime_secs: 7200,
                method: Method::NatPmp,
            })
        );
    }
    assert_eq!(w.core.mappings().len(), 2);
    assert_eq!(w.core.method(), Some(Method::NatPmp));
    assert_eq!(w.gw.has(Protocol::Tcp, PORT), Some(PORT));
}

#[test]
fn natpmp_retransmit_schedule_is_250ms_doubling_nine_tries() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.core = PortMapper::new(Config {
        natpmp_discovery_tries: 9,
        ..cfg(&[Method::NatPmp])
    });
    w.gw.behavior.silent = true;
    w.start();
    w.run_for(10 * 60_000);
    let times: Vec<u64> = w.sent.iter().map(|(t, _)| *t).collect();
    assert_eq!(times, [0, 250, 750, 1750, 3750, 7750, 15750, 31750, 63750]);
    // gives up one final wait (64 s) after the 9th transmission
    assert_eq!(w.time_of(|e| *e == Event::NoMethod), Some(127_750));
    assert!(w.sent.iter().all(|(_, d)| d == &vec![0, 0]));
}

#[test]
fn pcp_retransmit_schedule_is_3s_doubling_with_jitter() {
    let mut w = World::new(Config {
        pcp_discovery_tries: 4,
        ..cfg(&[Method::Pcp])
    });
    w.gw.behavior.silent = true;
    w.start();
    w.run_for(10 * 60_000);
    let t: Vec<u64> = w.sent.iter().map(|(t, _)| *t).collect();
    assert_eq!(t.len(), 4);
    let gaps: Vec<u64> = t.windows(2).map(|p| p[1] - p[0]).collect();
    for (g, base) in gaps.iter().zip([3000u64, 6000, 12000]) {
        assert!(
            *g >= base * 9 / 10 && *g <= base * 11 / 10,
            "gap {g} for base {base}"
        );
    }
    let failed = w.time_of(|e| *e == Event::NoMethod).unwrap();
    assert!((failed - t[3]) >= 21_600 && (failed - t[3]) <= 26_400);
    assert!(w.sent.iter().all(|(_, d)| d.len() == 60 && d[0] == 2));
}

#[test]
fn pcp_works_and_nat_pmp_only_gateway_falls_through_immediately() {
    // PCP gateway
    let mut w = World::new(cfg(&[Method::Pcp, Method::NatPmp, Method::Upnp]));
    w.gw.behavior.natpmp = false;
    w.start();
    w.run_for(1000);
    assert_eq!(w.core.method(), Some(Method::Pcp));
    assert!(w.sent.iter().all(|(_, d)| d.len() == 60));
    assert_eq!(w.sent.len(), 2, "one MAP per protocol, nothing else");
    assert_eq!(w.core.mappings().len(), 2);
    assert_eq!(w.core.external_ip(), Some(EXTERNAL_IP));

    // NAT-PMP-only gateway answers PCP with "unsupported version": no waiting for timeouts
    let mut w = World::new(cfg(&[Method::Pcp, Method::NatPmp, Method::Upnp]));
    w.gw.behavior.pcp = false;
    w.start();
    w.run_for(1000);
    assert_eq!(w.core.method(), Some(Method::NatPmp));
    assert_eq!(
        w.events[0],
        (
            0,
            Event::MethodFailed {
                method: Method::Pcp,
                reason: Failure::Unsupported
            }
        )
    );
    assert_eq!(
        w.time_of(|e| matches!(e, Event::MethodSelected(Method::NatPmp))),
        Some(0)
    );
    assert_eq!(w.core.mappings().len(), 2);
}

#[test]
fn pcp_address_mismatch_and_nonce_checks() {
    let mut w = World::new(cfg(&[Method::Pcp]));
    w.gw.client_ip = Ipv4Addr::new(10, 0, 0, 99); // gateway sees someone else
    w.start();
    w.run_for(1000);
    assert!(w.evs().contains(&&Event::MethodFailed {
        method: Method::Pcp,
        reason: Failure::AddressMismatch
    }));
    assert_eq!(w.core.method(), None);

    // a reply with the wrong nonce is ignored; the right one is accepted
    let mut w = World::new(cfg(&[Method::Pcp]));
    w.gw.behavior.silent = true;
    w.start();
    let req = w.sent[0].1.clone();
    w.gw.behavior.silent = false;
    let mut good = w.gw.handle(&req, 0).unwrap();
    let mut forged = good.clone();
    forged[24] ^= 0xff;
    w.core.handle(
        Input::Udp {
            from: GW_ADDR,
            data: &forged,
        },
        Millis(10),
    );
    assert_eq!(w.core.method(), None, "forged nonce ignored");
    good[3] = 0;
    let outs = w.core.handle(
        Input::Udp {
            from: GW_ADDR,
            data: &good,
        },
        Millis(20),
    );
    assert!(outs.contains(&Output::Event(Event::MethodSelected(Method::Pcp))));
}

#[test]
fn replies_from_anyone_but_the_gateway_port_are_ignored() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.gw.behavior.silent = true;
    w.start();
    w.gw.behavior.silent = false;
    let reply = w.gw.handle(&[0, 0], 0).unwrap();
    for from in [
        SocketAddr::from(([10, 0, 0, 99], 5351)),
        SocketAddr::from(([10, 0, 0, 1], 5352)),
        SocketAddr::from(([127, 0, 0, 1], 5351)),
    ] {
        let outs = w.core.handle(Input::Udp { from, data: &reply }, Millis(5));
        assert!(outs.is_empty(), "{from}");
    }
    assert_eq!(w.core.method(), None);
    let outs = w.core.handle(
        Input::Udp {
            from: GW_ADDR,
            data: &reply,
        },
        Millis(6),
    );
    assert!(outs.contains(&Output::Event(Event::MethodSelected(Method::NatPmp))));
}

#[test]
fn renewal_happens_at_half_the_lifetime() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(25 * H);
    let tcp = w.tcp_map_times();
    let want: Vec<u64> = (0..=25).map(|k| k * H).collect();
    assert_eq!(tcp, want, "granted 7200 s -> renewal every 3600 s");
    // renewals are Renewed, never a fresh Mapped
    assert_eq!(w.count(|e| matches!(e, Event::Mapped(_))), 2);
    assert_eq!(w.count(|e| matches!(e, Event::Renewed(_))), 2 * 25);
    // the external address is re-read hourly, not more
    let probes = w.sent.iter().filter(|(_, d)| d.len() == 2).count();
    assert!((25..=28).contains(&probes), "{probes}");
}

#[test]
fn shorter_grants_shorten_the_renewal_but_never_below_the_gap() {
    // gateway grants 1200 s: renew at 600 s
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.gw.behavior.grant_lifetime = Some(1200);
    w.start();
    w.run_for(2 * H);
    let t = w.tcp_map_times();
    assert_eq!(&t[..4], &[0, 600_000, 1_200_000, 1_800_000]);

    // a router that grants 10 s must not be asked more than once a minute
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.gw.behavior.grant_lifetime = Some(10);
    w.start();
    w.run_for(H);
    let t = w.tcp_map_times();
    assert!(t.len() >= 2);
    for p in t.windows(2) {
        assert!(p[1] - p[0] >= 60_000, "{p:?}");
    }
}

#[test]
fn gateway_reboot_is_detected_by_the_epoch_and_everything_is_remapped() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1_000_000);
    w.gw.reboot(w.now); // table gone, epoch restarts
    assert!(w.gw.table.is_empty());
    let before = w.sent.len();
    w.run_for(2 * H);
    assert_eq!(w.count(|e| *e == Event::GatewayRestarted), 1);
    assert_eq!(
        w.gw.has(Protocol::Tcp, PORT),
        Some(PORT),
        "same external port re-requested"
    );
    assert_eq!(w.gw.has(Protocol::Udp, PORT), Some(PORT));
    // the reset costs one extra mapping request per protocol, not a storm
    let extra = w.sent.len() - before;
    assert!(extra <= 8, "{extra}");
    // no mapping was announced as lost: they were simply re-created
    assert_eq!(w.count(|e| matches!(e, Event::Lost { .. })), 0);
}

#[test]
fn pcp_gateway_reboot_is_detected_too() {
    let mut w = World::new(cfg(&[Method::Pcp]));
    w.start();
    w.run_for(1_000_000);
    w.gw.reboot(w.now);
    w.run_for(2 * H);
    assert_eq!(w.count(|e| *e == Event::GatewayRestarted), 1);
    assert!(w.gw.has(Protocol::Tcp, PORT).is_some() && w.gw.has(Protocol::Udp, PORT).is_some());
}

#[test]
fn refused_means_one_try_per_six_hours_not_a_storm() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1000); // mapped
    w.gw.behavior.map_result = Some(2); // not authorized from now on
    w.run_for(48 * H);
    let t = w.tcp_map_times();
    assert!((3..=10).contains(&t.len()), "{}", t.len());
    for p in t[1..].windows(2) {
        assert!(p[1] - p[0] >= 6 * H, "at most every 6 h: {p:?}");
    }
    let failed = w.events.iter().find_map(|(_, e)| match e {
        Event::SlotFailed {
            reason, retry_in, ..
        } => Some((*reason, *retry_in)),
        _ => None,
    });
    let (reason, retry) = failed.unwrap();
    assert_eq!(reason, Failure::Refused);
    assert!(retry.as_secs() >= 6 * 3600);
}

#[test]
fn network_failure_backs_off_exponentially_to_the_cap() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.gw.behavior.map_result = Some(3);
    w.run_for(24 * H);
    let t = w.tcp_map_times();
    let gaps: Vec<u64> = t[1..].windows(2).map(|p| p[1] - p[0]).collect();
    assert!(gaps.len() >= 10);
    assert!(
        gaps.iter().all(|&g| g >= 300_000),
        "floor of 5 min for result 3: {gaps:?}"
    );
    assert!(
        gaps.windows(2).all(|p| p[1] + p[1] / 10 >= p[0]),
        "non-decreasing up to jitter: {gaps:?}"
    );
    assert!(
        gaps.iter().all(|&g| g <= H + H / 10 + 1000),
        "capped at 1 h (+10 % jitter): {gaps:?}"
    );
    assert!(*gaps.last().unwrap() >= H);
    assert!(t.len() <= 30, "{}", t.len());
}

#[test]
fn out_of_resources_and_zero_lifetime_are_handled() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.gw.behavior.map_result = Some(4);
    w.run_for(3 * H);
    assert!(w.events.iter().any(|(_, e)| matches!(
        e,
        Event::SlotFailed {
            reason: Failure::NoResources,
            ..
        }
    )));
    // a gateway that "grants" lifetime 0 is a failure, not a mapping
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.gw.behavior.grant_lifetime = Some(0);
    w.start();
    w.run_for(1000);
    assert!(w.core.mappings().is_empty());
    assert!(w.events.iter().any(|(_, e)| matches!(
        e,
        Event::SlotFailed {
            reason: Failure::Protocol(0),
            ..
        }
    )));
}

#[test]
fn pcp_error_lifetime_is_a_retry_hint() {
    let mut w = World::new(cfg(&[Method::Pcp]));
    w.start();
    w.run_for(1000);
    w.gw.behavior.map_result = Some(8); // NO_RESOURCES
    w.gw.behavior.error_lifetime = 1800;
    w.run_for(2 * H);
    let retry = w.events.iter().find_map(|(_, e)| match e {
        Event::SlotFailed {
            reason: Failure::NoResources,
            retry_in,
            ..
        } => Some(*retry_in),
        _ => None,
    });
    assert!(retry.unwrap().as_secs() >= 1800);
}

#[test]
fn everything_failing_stops_after_one_discovery_until_the_network_changes() {
    let mut w = World::new(cfg(&[Method::Pcp, Method::NatPmp, Method::Upnp]));
    w.gw.behavior.silent = true;
    w.start();
    w.run_for(2 * 60_000);
    assert_eq!(w.count(|e| *e == Event::NoMethod), 1);
    // PCP 3 tries + NAT-PMP 4 tries + SSDP 2 rounds x 2 targets
    assert_eq!(w.sent.len(), 7);
    assert_eq!(w.ssdp_sent.len(), 4);
    let (s, d) = (w.sent.len(), w.ssdp_sent.len());
    w.run_for(72 * H);
    assert_eq!(
        (w.sent.len(), w.ssdp_sent.len()),
        (s, d),
        "silence after the failed discovery"
    );
    // same network again: still nothing
    w.feed(Input::Network(Some(net())));
    w.run_for(H);
    assert_eq!((w.sent.len(), w.ssdp_sent.len()), (s, d));
    // a real change: exactly one more discovery
    w.feed(Input::Network(Some(NetInfo {
        gateway: GW,
        local: Ipv4Addr::new(10, 0, 0, 3),
    })));
    w.run_for(H);
    assert_eq!((w.sent.len(), w.ssdp_sent.len()), (2 * s, 2 * d));
    // explicit retry: one more
    w.feed(Input::Retry);
    w.run_for(H);
    assert_eq!((w.sent.len(), w.ssdp_sent.len()), (3 * s, 3 * d));
}

#[test]
fn rapid_network_flaps_are_debounced_to_one_discovery_per_30s() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(10_000);
    let first = w.sent.len();
    w.feed(Input::Network(Some(NetInfo {
        gateway: GW,
        local: Ipv4Addr::new(10, 0, 0, 3),
    })));
    w.run_for(5_000);
    w.feed(Input::Network(Some(NetInfo {
        gateway: GW,
        local: Ipv4Addr::new(10, 0, 0, 4),
    })));
    w.run_for(5_000);
    assert_eq!(
        w.sent.len(),
        first,
        "no packets before the 30 s gap is over"
    );
    w.run_for(60_000);
    let second_start = w.sent[first].0;
    assert!(second_start >= 30_000, "{second_start}");
    assert_eq!(w.sent.len(), 2 * first, "one discovery for both flaps");
}

#[test]
fn losing_the_network_silences_everything() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1000);
    w.feed(Input::Network(None));
    let n = w.sent.len();
    w.run_for(72 * H);
    assert_eq!(w.sent.len(), n);
    assert_eq!(w.count(|e| *e == Event::NetworkLost), 1);
    assert!(w.core.mappings().is_empty());
    assert_eq!(w.core.next_wakeup(), None);
}

#[test]
fn shutdown_deletes_every_mapping() {
    for (methods, natpmp, pcp) in [
        (&[Method::NatPmp][..], true, false),
        (&[Method::Pcp][..], false, true),
    ] {
        let mut w = World::new(cfg(methods));
        w.gw.behavior.natpmp = natpmp;
        w.gw.behavior.pcp = pcp;
        w.start();
        w.run_for(10_000);
        assert_eq!(w.gw.table.len(), 2);
        w.feed(Input::Shutdown);
        w.run_for(100);
        assert!(w.gw.table.is_empty(), "{methods:?}");
        assert_eq!(w.count(|e| matches!(e, Event::Deleted { .. })), 2);
        assert_eq!(w.count(|e| *e == Event::ShutdownComplete), 1);
        assert!(w.core.is_done());
        let n = w.sent.len();
        w.run_for(72 * H);
        assert_eq!(w.sent.len(), n, "silent after shutdown");
        assert_eq!(w.core.next_wakeup(), None);
    }
}

#[test]
fn nat_pmp_delete_is_lifetime_zero_and_external_port_zero() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1000);
    let n = w.sent.len();
    w.feed(Input::Shutdown);
    let del = &w.sent[n].1;
    assert_eq!(del[1], 2);
    assert_eq!(&del[4..12], &[0xc8, 0xd5, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn shutdown_is_bounded_when_the_gateway_is_gone() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.start();
    w.run_for(1000);
    w.gw.behavior.silent = true;
    let t0 = w.now;
    w.feed(Input::Shutdown);
    w.run_for(10_000);
    let done = w.time_of(|e| *e == Event::ShutdownComplete).unwrap();
    assert!(done - t0 <= 3000, "{}", done - t0);
    assert!(w.core.is_done());
}

#[test]
fn shutdown_without_mappings_completes_immediately() {
    let mut w = World::new(cfg(&[Method::NatPmp]));
    w.feed(Input::Shutdown);
    assert_eq!(w.count(|e| *e == Event::ShutdownComplete), 1);
    // and ignores everything afterwards
    w.start();
    w.run_for(H);
    assert!(w.sent.is_empty());
}

// ---------------------------------------------------------------- UPnP

fn upnp_world() -> World {
    let mut w = World::new(cfg(&[Method::Upnp]));
    w.igd_on = true;
    w
}

#[test]
fn upnp_discovery_maps_and_renews_at_half_lease_and_deletes() {
    let mut w = upnp_world();
    w.start();
    w.run_for(1000);
    assert_eq!(w.core.method(), Some(Method::Upnp));
    assert_eq!(
        w.ssdp_sent.len(),
        2,
        "first round only: the reply ended the search"
    );
    assert_eq!(w.core.external_ip(), Some(EXTERNAL_IP));
    assert_eq!(w.igd.table.get(&("TCP".into(), PORT)), Some(&(PORT, 7200)));
    assert_eq!(w.igd.table.get(&("UDP".into(), PORT)), Some(&(PORT, 7200)));
    assert_eq!(w.core.mappings().len(), 2);
    w.run_for(3 * H);
    let adds = w
        .igd
        .actions
        .iter()
        .filter(|a| *a == "AddPortMapping")
        .count();
    assert_eq!(
        adds,
        2 * 4,
        "initial + renewals at 1 h, 2 h, 3 h, for both protocols"
    );
    // shutdown removes both
    w.feed(Input::Shutdown);
    w.run_for(1000);
    assert!(w.igd.table.is_empty());
    assert_eq!(w.count(|e| matches!(e, Event::Deleted { .. })), 2);
    assert!(w.core.is_done());
}

#[test]
fn upnp_permanent_lease_only_routers() {
    let mut w = upnp_world();
    w.igd.behavior.only_permanent = true;
    w.start();
    w.run_for(1000);
    assert_eq!(w.igd.table.get(&("TCP".into(), PORT)), Some(&(PORT, 0)));
    let m = w.core.mappings();
    assert_eq!(m.len(), 2);
    assert!(m.iter().all(|m| m.lifetime_secs == 0));
    // re-asserted every hour, no more often
    w.run_for(5 * H);
    let adds = w
        .igd
        .actions
        .iter()
        .filter(|a| *a == "AddPortMapping")
        .count();
    assert!(adds <= 2 * 2 + 2 * 6, "{adds}");
}

#[test]
fn upnp_conflicts_pick_another_external_port_a_bounded_number_of_times() {
    let mut w = upnp_world();
    w.igd.behavior.taken_ports = vec![PORT];
    w.start();
    w.run_for(1000);
    let m = w.core.mappings();
    assert_eq!(m.len(), 2);
    assert!(
        m.iter()
            .all(|m| m.external_port != PORT && m.external_port >= 49152)
    );
    // a router that says "conflict" to everything: 1 try + 3 retries, then back-off
    let mut w = upnp_world();
    w.igd.behavior.add_error = Some(718);
    w.start();
    w.run_for(10_000);
    let adds = w
        .igd
        .actions
        .iter()
        .filter(|a| *a == "AddPortMapping")
        .count();
    assert_eq!(adds, 2 * 4);
    assert!(w.core.mappings().is_empty());
    assert!(
        w.events
            .iter()
            .any(|(_, e)| matches!(e, Event::SlotFailed { .. }))
    );
}

#[test]
fn upnp_same_port_only_routers() {
    let mut w = World::new(Config {
        wanted: vec![Wanted {
            protocol: Protocol::Tcp,
            internal_port: PORT,
            suggested_external: 40000,
        }],
        ..cfg(&[Method::Upnp])
    });
    w.igd_on = true;
    w.igd.behavior.same_port_only = true;
    w.start();
    w.run_for(1000);
    assert_eq!(w.core.mappings()[0].external_port, PORT);
}

#[test]
fn upnp_ignores_foreign_locations_and_foreign_control_urls() {
    // LOCATION points at a different LAN host
    let mut w = upnp_world();
    w.ssdp_location = "http://10.0.0.77:5000/rootDesc.xml".into();
    w.start();
    w.run_for(10_000);
    assert!(
        w.http_sent.is_empty(),
        "never fetched from a non-gateway host"
    );
    assert_eq!(w.count(|e| *e == Event::NoMethod), 1);

    // description is fine but its controlURL points at another host
    let mut w = upnp_world();
    w.igd.behavior.control_url = Some("http://10.0.0.77:80/ctl".into());
    w.start();
    w.run_for(10_000);
    assert_eq!(w.http_sent.len(), 1, "only the description was fetched");
    assert!(w.evs().contains(&&Event::MethodFailed {
        method: Method::Upnp,
        reason: Failure::Transport
    }));

    // hostnames and https are never followed
    for loc in [
        "http://router.local/desc.xml",
        "https://10.0.0.1/desc.xml",
        "http://[::1]/d",
    ] {
        let mut w = upnp_world();
        w.ssdp_location = loc.into();
        w.start();
        w.run_for(10_000);
        assert!(w.http_sent.is_empty(), "{loc}");
    }
}

#[test]
fn upnp_service_variants_and_replies() {
    // PPP-only router, chunked SOAP replies
    let mut w = upnp_world();
    w.igd.behavior.ppp_only = true;
    w.igd.behavior.chunked = true;
    w.start();
    w.run_for(1000);
    assert_eq!(w.core.mappings().len(), 2);
    // GetExternalIPAddress error fails the method
    let mut w = upnp_world();
    w.igd.behavior.ip_error = Some(501);
    w.start();
    w.run_for(10_000);
    assert!(w.evs().contains(&&Event::MethodFailed {
        method: Method::Upnp,
        reason: Failure::Protocol(501)
    }));
    assert_eq!(w.core.method(), None);
    // no IGD at all: SSDP window passes, one NoMethod
    let mut w = World::new(cfg(&[Method::Upnp]));
    w.start();
    w.run_for(H);
    assert_eq!(w.ssdp_sent.len(), 4);
    assert_eq!(w.count(|e| *e == Event::NoMethod), 1);
}

#[test]
fn private_external_addresses_are_flagged() {
    for (ip, public) in [
        ([203, 0, 113, 9], true),
        ([192, 168, 0, 5], false),
        ([100, 64, 1, 1], false),
        ([100, 128, 1, 1], true),
        ([169, 254, 1, 1], false),
        ([10, 1, 2, 3], false),
        ([172, 16, 0, 1], false),
        ([172, 32, 0, 1], true),
    ] {
        assert_eq!(is_public_ipv4(Ipv4Addr::from(ip)), public, "{ip:?}");
    }
}

// ---------------------------------------------------------------- adversary

/// A gateway that flips between healthy, silent, erroring, rebooting and chatty over
/// three days: the core must never panic, never have two overlapping request
/// kinds, and never exceed the packet budget.
#[test]
fn hostile_gateway_cannot_make_us_exceed_the_packet_budget() {
    for seed in 0..20u64 {
        let mut rng = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut w = World::new(Config {
            seed,
            ..cfg(&[Method::Pcp, Method::NatPmp, Method::Upnp])
        });
        w.igd_on = seed % 2 == 0;
        w.start();
        let mut t = 0;
        while t < 72 * H {
            // new gateway mood every 1-30 minutes
            match next() % 7 {
                0 => {
                    w.gw.behavior = GatewayBehavior {
                        silent: true,
                        ..Default::default()
                    }
                }
                1 => {
                    w.gw.behavior = GatewayBehavior {
                        map_result: Some((next() % 14) as u8),
                        error_lifetime: (next() % 4000) as u32,
                        ..Default::default()
                    }
                }
                2 => w.gw.reboot(w.now),
                3 => {
                    w.gw.behavior = GatewayBehavior {
                        natpmp: false,
                        ..Default::default()
                    }
                }
                4 => {
                    w.gw.behavior = GatewayBehavior {
                        grant_lifetime: Some((next() % 400) as u32),
                        ..Default::default()
                    }
                }
                _ => w.gw.behavior = GatewayBehavior::default(),
            }
            // garbage from the gateway
            for _ in 0..(next() % 5) {
                let len = (next() % 80) as usize;
                let junk: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                let outs = w.core.handle(
                    Input::Udp {
                        from: GW_ADDR,
                        data: &junk,
                    },
                    Millis(w.now),
                );
                let mut q = VecDeque::new();
                w.process(outs, &mut q);
            }
            let step = 60_000 + next() % (29 * 60_000);
            w.run_for(step);
            t += step;
        }
        // budget: no 60 s window has more than 12 packets, no hour more than 300
        let times: Vec<u64> = w.sent.iter().map(|(t, _)| *t).collect();
        for (i, &t0) in times.iter().enumerate() {
            let in_minute = times[i..].iter().take_while(|&&t| t < t0 + 60_000).count();
            assert!(
                in_minute <= 12,
                "seed {seed}: {in_minute} packets within 60 s at {t0}"
            );
            let in_hour = times[i..].iter().take_while(|&&t| t < t0 + H).count();
            assert!(
                in_hour <= 300,
                "seed {seed}: {in_hour} packets within an hour at {t0}"
            );
        }
        let ssdp_per_hour = w.ssdp_sent.len() as u64 / 72 + 1;
        assert!(
            ssdp_per_hour <= 8,
            "seed {seed}: ssdp {}",
            w.ssdp_sent.len()
        );
    }
}
