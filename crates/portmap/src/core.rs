//! The sans-I/O port-mapping state machine.
//!
//! `handle(input, now) -> Vec<Output>`: no sockets, no clock, no randomness source of
//! its own beyond the seeded [`Rng`]. The driver turns [`Output`]s into datagrams and
//! TCP exchanges and feeds the answers back as [`Input`]s.
//!
//! Politeness rules, all enforced here (and tested on a virtual clock):
//!
//! * at most **one outstanding transaction** to the gateway at any time;
//! * NAT-PMP retransmits at 250 ms doubling, at most 9 transmissions (RFC 6886 3.1);
//!   PCP at 3 s doubling with +-10 % jitter (RFC 6887 8.1.1);
//! * mappings are renewed at **half their granted lifetime**, never closer together
//!   than `min_renew_gap` (default 60 s), so a router that grants tiny lifetimes
//!   cannot make us spam it;
//! * errors back off 1 min doubling to 1 h (a PCP `lifetime` hint or a "not
//!   authorized" answer lengthens that), with jitter;
//! * **one discovery per network change**: when every method fails the machine stays
//!   in `Failed` until the next network change or an explicit `Retry`; discoveries
//!   are at least `discovery_min_gap` apart;
//! * on shutdown every mapping is deleted, bounded by `shutdown_deadline`.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use crate::http::{self, Url};
use crate::rng::Rng;
use crate::{Method, Millis, Protocol, natpmp, pcp, ssdp, upnp};

/// Most mappings that can be requested at once.
pub const MAX_WANTED: usize = 8;

/// One mapping we want: usually TCP and UDP for the listen port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wanted {
    /// Protocol.
    pub protocol: Protocol,
    /// Local port.
    pub internal_port: u16,
    /// Preferred external port; 0 means "same as the internal port".
    pub suggested_external: u16,
}

impl Wanted {
    /// TCP and UDP for one port, external port preferred equal.
    pub fn tcp_and_udp(port: u16) -> Vec<Wanted> {
        [Protocol::Tcp, Protocol::Udp]
            .into_iter()
            .map(|protocol| Wanted {
                protocol,
                internal_port: port,
                suggested_external: 0,
            })
            .collect()
    }
}

/// The network we are on: default gateway and our address towards it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NetInfo {
    /// Default IPv4 gateway.
    pub gateway: Ipv4Addr,
    /// Our IPv4 address on that network.
    pub local: Ipv4Addr,
}

/// Machine configuration. Defaults are the polite production values.
#[derive(Clone, Debug)]
pub struct Config {
    /// Mappings to create (truncated to [`MAX_WANTED`]).
    pub wanted: Vec<Wanted>,
    /// Lifetime we ask for, seconds (RFC 6886 recommends 7200).
    pub lifetime_secs: u32,
    /// Methods to try, in order.
    pub methods: Vec<Method>,
    /// UDP port of the gateway's NAT-PMP/PCP server (tests override).
    pub gateway_port: u16,
    /// SSDP multicast target (tests override).
    pub ssdp_addr: SocketAddrV4,
    /// Description shown in the router's UPnP table.
    pub description: String,
    /// `User-Agent` for SSDP/HTTP. Honest, like the tracker identity.
    pub user_agent: String,
    /// PCP transmissions during discovery.
    pub pcp_discovery_tries: u8,
    /// NAT-PMP transmissions for the external-address probe during discovery.
    pub natpmp_discovery_tries: u8,
    /// NAT-PMP transmissions for mapping requests (RFC maximum 9).
    pub natpmp_tries: u8,
    /// PCP transmissions for mapping requests.
    pub pcp_tries: u8,
    /// Transmissions for delete requests at shutdown.
    pub delete_tries: u8,
    /// TCP exchange timeout for SOAP/description fetches.
    pub http_timeout: Duration,
    /// How long to listen for SSDP replies.
    pub ssdp_window: Duration,
    /// Smallest spacing between two requests for the same mapping.
    pub min_renew_gap: Duration,
    /// First back-off after an error.
    pub backoff_min: Duration,
    /// Back-off cap.
    pub backoff_max: Duration,
    /// How often the external address is re-read (NAT-PMP/UPnP).
    pub ext_refresh: Duration,
    /// Smallest spacing between two discoveries.
    pub discovery_min_gap: Duration,
    /// Total time allowed for deleting mappings at shutdown.
    pub shutdown_deadline: Duration,
    /// Accept an IGD that is not the default gateway (double NAT). Off by default:
    /// a LAN device must not be able to steer us to another host.
    pub allow_foreign_igd: bool,
    /// Seed for jitter, nonces and port choices (default: from OS entropy; tests pin it).
    pub seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            wanted: Vec::new(),
            lifetime_secs: 7200,
            methods: vec![Method::Pcp, Method::NatPmp, Method::Upnp],
            gateway_port: natpmp::PORT,
            ssdp_addr: ssdp::MULTICAST,
            description: "Seedwell".to_owned(),
            user_agent: format!("Seedwell/{} UPnP/1.1", env!("CARGO_PKG_VERSION")),
            pcp_discovery_tries: 3,
            natpmp_discovery_tries: 4,
            natpmp_tries: natpmp::MAX_TRIES,
            pcp_tries: 4,
            delete_tries: 2,
            http_timeout: Duration::from_secs(5),
            ssdp_window: Duration::from_secs(4),
            min_renew_gap: Duration::from_secs(60),
            backoff_min: Duration::from_secs(60),
            backoff_max: Duration::from_secs(3600),
            ext_refresh: Duration::from_secs(3600),
            discovery_min_gap: Duration::from_secs(30),
            shutdown_deadline: Duration::from_secs(3),
            allow_foreign_igd: false,
            seed: Rng::from_entropy().next_u64(),
        }
    }
}

/// An active mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// Protocol.
    pub protocol: Protocol,
    /// Local port.
    pub internal_port: u16,
    /// Port on the gateway's external address. Report this one to trackers.
    pub external_port: u16,
    /// Gateway's external address, when known.
    pub external_ip: Option<Ipv4Addr>,
    /// Lifetime granted at the last (re)mapping, seconds (0 = permanent, UPnP only).
    pub lifetime_secs: u32,
    /// Mechanism.
    pub method: Method,
}

/// Why a request failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// The gateway refuses (not authorized / disabled).
    Refused,
    /// The gateway has no working external network.
    NetworkFailure,
    /// The gateway is out of mappings or quota.
    NoResources,
    /// The gateway does not support this protocol/version/opcode.
    Unsupported,
    /// PCP: the client address in the request is not the source address.
    AddressMismatch,
    /// No answer after all retransmissions.
    Timeout,
    /// TCP/HTTP level failure, or an unusable reply.
    Transport,
    /// A protocol error code we do not specially handle (also zero lifetime = 0).
    Protocol(u32),
}

/// What the machine tells the application.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// A method answered and will be used.
    MethodSelected(Method),
    /// A method did not work (informational; the next one is tried).
    MethodFailed {
        /// Method.
        method: Method,
        /// Why.
        reason: Failure,
    },
    /// The gateway's external address. `public` is false for private, CGNAT and
    /// link-local addresses: a mapping there is not reachable from the Internet.
    ExternalIp {
        /// Address.
        ip: Ipv4Addr,
        /// Whether it is a public address.
        public: bool,
    },
    /// A mapping was created or changed (new external port).
    Mapped(Mapping),
    /// A mapping was renewed without change.
    Renewed(Mapping),
    /// A mapping expired or could not be kept.
    Lost {
        /// Protocol.
        protocol: Protocol,
        /// Local port.
        internal_port: u16,
    },
    /// A request for one mapping failed; the next try is `retry_in` away.
    SlotFailed {
        /// Protocol.
        protocol: Protocol,
        /// Local port.
        internal_port: u16,
        /// Why.
        reason: Failure,
        /// Delay until the next attempt.
        retry_in: Duration,
    },
    /// The gateway's table was reset (NAT-PMP/PCP epoch went backwards); all
    /// mappings are being re-created.
    GatewayRestarted,
    /// Every method failed. Nothing more happens until a network change or `Retry`.
    NoMethod,
    /// There is no usable network.
    NetworkLost,
    /// A mapping was deleted (shutdown).
    Deleted {
        /// Protocol.
        protocol: Protocol,
        /// Local port.
        internal_port: u16,
    },
    /// Shutdown finished: deletions done or the deadline passed.
    ShutdownComplete,
}

/// Why a TCP exchange failed (reported by the driver).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpFailure {
    /// Could not connect.
    Connect,
    /// Timed out.
    Timeout,
    /// Read/write error or early close with nothing received.
    Io,
    /// The response exceeded the size cap.
    TooLarge,
}

/// A TCP exchange the driver must perform: connect, write `request`, read until EOF
/// or `max_response` bytes, then report with [`Input::HttpDone`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpRequest {
    /// Correlation id.
    pub id: u64,
    /// Where to connect.
    pub to: SocketAddrV4,
    /// Full request bytes.
    pub request: Vec<u8>,
    /// Read cap.
    pub max_response: usize,
    /// Overall timeout for connect + exchange.
    pub timeout: Duration,
}

/// Inputs.
#[derive(Debug)]
pub enum Input<'a> {
    /// The network changed (or we are starting). `None` = no usable network.
    Network(Option<NetInfo>),
    /// A datagram arrived.
    Udp {
        /// Source.
        from: SocketAddr,
        /// Payload.
        data: &'a [u8],
    },
    /// A TCP exchange finished: raw response bytes or a failure.
    HttpDone {
        /// Id of the [`HttpRequest`].
        id: u64,
        /// Raw bytes read.
        outcome: Result<Vec<u8>, HttpFailure>,
    },
    /// Timer fired (or just check timers).
    Tick,
    /// Explicit user retry after `NoMethod`.
    Retry,
    /// Delete the mappings and stop.
    Shutdown,
}

/// Outputs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Output {
    /// Send a datagram (gateway or SSDP multicast).
    Udp {
        /// Destination.
        to: SocketAddr,
        /// Payload.
        data: Vec<u8>,
    },
    /// Perform a TCP exchange.
    Http(HttpRequest),
    /// Tell the application.
    Event(Event),
}

#[derive(Clone, Copy, Debug)]
enum SlotState {
    Idle,
    Mapped {
        lifetime: u32,
        renew_at: Millis,
        expires_at: Millis,
    },
    Backoff {
        until: Millis,
    },
}

#[derive(Clone, Debug)]
struct Slot {
    want: Wanted,
    nonce: [u8; 12],
    state: SlotState,
    busy: bool,
    maybe_mapped: bool,
    fails: u8,
    ext_hint: u16,
    permanent: bool,
    conflict_retries: u8,
    last: Option<Mapping>,
}

#[derive(Clone, Debug)]
enum UdpWhat {
    /// NAT-PMP external-address probe during discovery.
    Probe,
    /// NAT-PMP external-address refresh.
    ExtAddr,
    Map(usize),
    Delete(usize),
}

#[derive(Clone, Debug)]
struct UdpTxn {
    what: UdpWhat,
    method: Method,
    packet: Vec<u8>,
    sent: u8,
    max_tries: u8,
    wait_ms: u64,
    next_at: Millis,
}

#[derive(Clone, Debug)]
enum HttpWhat {
    Description(Url),
    ExtIp { discovery: bool },
    Add(usize),
    Delete(usize),
}

#[derive(Clone, Debug)]
struct HttpTxn {
    id: u64,
    what: HttpWhat,
    deadline: Millis,
}

#[derive(Clone, Debug)]
enum Txn {
    Udp(UdpTxn),
    Http(HttpTxn),
}

#[derive(Clone, Debug)]
enum Step {
    Udp,
    Ssdp {
        round: u8,
        next_at: Millis,
        end_at: Millis,
    },
    Description,
    ExtIp,
}

#[derive(Clone, Debug)]
enum Phase {
    Idle,
    Waiting {
        until: Millis,
    },
    Discovering {
        idx: usize,
        step: Step,
    },
    Active(Method),
    Failed,
    Stopping {
        deadline: Millis,
        queue: VecDeque<usize>,
        method: Method,
    },
    Done,
}

#[derive(Clone, Debug)]
struct UpnpTarget {
    service_type: String,
    control: Url,
}

/// Is `ip` plausibly reachable from the Internet? Private (RFC 1918), CGNAT
/// (100.64/10), link-local, loopback, multicast and documentation-less ranges are not.
pub fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || o[0] == 0
        || o[0] >= 240)
}

/// What a gateway granted for one mapping.
#[derive(Clone, Copy)]
struct Grant {
    method: Method,
    external_port: u16,
    /// Lifetime the renewal schedule is based on, seconds.
    lifetime: u32,
    /// Lifetime shown to the application (0 = permanent).
    reported: u32,
}

impl Grant {
    fn new(method: Method, external_port: u16, lifetime: u32) -> Self {
        Grant {
            method,
            external_port,
            lifetime,
            reported: lifetime,
        }
    }
}

/// The state machine.
pub struct PortMapper {
    cfg: Config,
    rng: Rng,
    net: Option<NetInfo>,
    phase: Phase,
    slots: Vec<Slot>,
    txn: Option<Txn>,
    epoch: Option<(u32, Millis)>,
    ext_ip: Option<Ipv4Addr>,
    ext_refresh_at: Option<Millis>,
    upnp: Option<UpnpTarget>,
    next_id: u64,
    last_discovery: Option<Millis>,
}

impl PortMapper {
    /// New machine. Nothing happens until [`Input::Network`] arrives.
    pub fn new(mut cfg: Config) -> Self {
        cfg.wanted.truncate(MAX_WANTED);
        let mut rng = Rng::from_seed(cfg.seed);
        let slots = cfg.wanted.iter().map(|&w| new_slot(w, &mut rng)).collect();
        PortMapper {
            cfg,
            rng,
            net: None,
            phase: Phase::Idle,
            slots,
            txn: None,
            epoch: None,
            ext_ip: None,
            ext_refresh_at: None,
            upnp: None,
            next_id: 1,
            last_discovery: None,
        }
    }

    /// The method in use, if discovery succeeded.
    pub fn method(&self) -> Option<Method> {
        match self.phase {
            Phase::Active(m) => Some(m),
            _ => None,
        }
    }

    /// The gateway's external address, when known.
    pub fn external_ip(&self) -> Option<Ipv4Addr> {
        self.ext_ip
    }

    /// Currently held mappings.
    pub fn mappings(&self) -> Vec<Mapping> {
        self.slots
            .iter()
            .filter_map(|s| match (s.state, s.last) {
                (SlotState::Mapped { .. }, Some(m)) => Some(m),
                _ => None,
            })
            .collect()
    }

    /// True once the machine will never act again.
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// Feed one input; returns what to do.
    pub fn handle(&mut self, input: Input<'_>, now: Millis) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::Network(n) => self.on_network(n, now, &mut out),
            Input::Retry => {
                if matches!(self.phase, Phase::Failed) {
                    self.schedule_discovery(now, &mut out);
                }
            }
            Input::Udp { from, data } => self.on_udp(from, data, now, &mut out),
            Input::HttpDone { id, outcome } => self.on_http(id, outcome, now, &mut out),
            Input::Tick => {}
            Input::Shutdown => self.on_shutdown(now, &mut out),
        }
        self.run_timers(now, &mut out);
        self.pump(now, &mut out);
        out
    }

    /// When to call [`handle`](Self::handle) with `Tick` next; `None` = only on input.
    pub fn next_wakeup(&self) -> Option<Millis> {
        let mut t: Option<Millis> = None;
        let mut min = |m: Millis| t = Some(t.map_or(m, |c| c.min(m)));
        match &self.phase {
            Phase::Waiting { until } => min(*until),
            Phase::Discovering {
                step: Step::Ssdp {
                    next_at, end_at, ..
                },
                ..
            } => min((*next_at).min(*end_at)),
            Phase::Stopping { deadline, .. } => min(*deadline),
            Phase::Active(m) if self.txn.is_none() => {
                if let Some(d) = self.next_due(*m) {
                    min(d);
                }
            }
            _ => {}
        }
        match &self.txn {
            Some(Txn::Udp(u)) => min(u.next_at),
            Some(Txn::Http(h)) => min(h.deadline),
            None => {}
        }
        // slot expiry bookkeeping
        if matches!(self.phase, Phase::Active(_)) {
            for s in &self.slots {
                if let SlotState::Mapped { expires_at, .. } = s.state
                    && !s.busy {
                        min(expires_at);
                    }
            }
        }
        t
    }

    // ---- network / discovery ------------------------------------------------

    fn on_network(&mut self, n: Option<NetInfo>, now: Millis, out: &mut Vec<Output>) {
        if matches!(self.phase, Phase::Stopping { .. } | Phase::Done) {
            return;
        }
        if n == self.net && !matches!(self.phase, Phase::Idle) {
            return; // same network: no new discovery, mappings keep renewing
        }
        self.net = n;
        self.reset_for_new_network();
        if n.is_none() {
            self.phase = Phase::Idle;
            out.push(Output::Event(Event::NetworkLost));
            return;
        }
        self.schedule_discovery(now, out);
    }

    fn reset_for_new_network(&mut self) {
        self.txn = None;
        self.epoch = None;
        self.ext_ip = None;
        self.ext_refresh_at = None;
        self.upnp = None;
        let wanted: Vec<Wanted> = self.slots.iter().map(|s| s.want).collect();
        self.slots = wanted
            .into_iter()
            .map(|w| new_slot(w, &mut self.rng))
            .collect();
    }

    fn schedule_discovery(&mut self, now: Millis, out: &mut Vec<Output>) {
        let earliest = self
            .last_discovery
            .map(|t| t.plus(self.cfg.discovery_min_gap));
        match earliest {
            Some(e) if e > now => self.phase = Phase::Waiting { until: e },
            _ => self.begin_discovery(now, out),
        }
    }

    fn begin_discovery(&mut self, now: Millis, out: &mut Vec<Output>) {
        self.last_discovery = Some(now);
        self.reset_for_new_network();
        self.try_method(0, now, out);
    }

    fn try_method(&mut self, idx: usize, now: Millis, out: &mut Vec<Output>) {
        let Some(&method) = self.cfg.methods.get(idx) else {
            self.phase = Phase::Failed;
            self.txn = None;
            out.push(Output::Event(Event::NoMethod));
            return;
        };
        if self.net.is_none() {
            self.phase = Phase::Idle;
            return;
        }
        if self.slots.is_empty() {
            self.phase = Phase::Failed;
            out.push(Output::Event(Event::NoMethod));
            return;
        }
        match method {
            Method::Pcp => {
                // The probe is the real mapping request for the first slot: PCP has
                // no cheaper "are you there" message.
                self.phase = Phase::Discovering {
                    idx,
                    step: Step::Udp,
                };
                let tries = self.cfg.pcp_discovery_tries;
                self.start_map_udp(0, Method::Pcp, tries, now, out);
            }
            Method::NatPmp => {
                self.phase = Phase::Discovering {
                    idx,
                    step: Step::Udp,
                };
                let packet = natpmp::encode_external_address_request().to_vec();
                let tries = self.cfg.natpmp_discovery_tries;
                self.start_udp(UdpWhat::Probe, Method::NatPmp, packet, tries, now, out);
            }
            Method::Upnp => {
                let window = self.cfg.ssdp_window;
                self.phase = Phase::Discovering {
                    idx,
                    step: Step::Ssdp {
                        round: 0,
                        next_at: now,
                        end_at: now.plus(window),
                    },
                };
                self.ssdp_round(now, out);
            }
        }
    }

    fn method_failed(&mut self, idx: usize, reason: Failure, now: Millis, out: &mut Vec<Output>) {
        self.txn = None;
        if let Some(&method) = self.cfg.methods.get(idx) {
            out.push(Output::Event(Event::MethodFailed { method, reason }));
        }
        // anything the failed probe left in flight is forgotten
        for s in &mut self.slots {
            s.busy = false;
            s.maybe_mapped = false;
        }
        self.try_method(idx + 1, now, out);
    }

    fn select_method(&mut self, method: Method, now: Millis, out: &mut Vec<Output>) {
        self.phase = Phase::Active(method);
        out.push(Output::Event(Event::MethodSelected(method)));
        if matches!(method, Method::NatPmp | Method::Upnp) {
            self.ext_refresh_at = Some(now.plus(self.cfg.ext_refresh));
        }
    }

    fn set_ext_ip(&mut self, ip: Option<Ipv4Addr>, out: &mut Vec<Output>) {
        let Some(ip) = ip.filter(|i| !i.is_unspecified()) else {
            return;
        };
        if self.ext_ip != Some(ip) {
            self.ext_ip = Some(ip);
            out.push(Output::Event(Event::ExternalIp {
                ip,
                public: is_public_ipv4(ip),
            }));
        }
    }

    // ---- SSDP -----------------------------------------------------------------

    fn ssdp_round(&mut self, now: Millis, out: &mut Vec<Output>) {
        let Phase::Discovering {
            step: Step::Ssdp { round, next_at, .. },
            ..
        } = &mut self.phase
        else {
            return;
        };
        // two rounds, 1.5 s apart: multicast is lossy, and that is all we ever send
        *round += 1;
        *next_at = if *round >= 2 {
            Millis(u64::MAX)
        } else {
            now.plus_ms(1500)
        };
        for st in ssdp::SEARCH_TARGETS {
            out.push(Output::Udp {
                to: SocketAddr::V4(self.cfg.ssdp_addr),
                data: ssdp::build_search(st, &self.cfg.user_agent),
            });
        }
    }

    fn on_ssdp(&mut self, from: SocketAddr, data: &[u8], now: Millis, out: &mut Vec<Output>) {
        let Some(net) = self.net else { return };
        let Phase::Discovering { idx, .. } = self.phase else {
            return;
        };
        let Some(reply) = ssdp::parse_reply(data) else {
            return;
        };
        let Some(url) = http::parse_url(&reply.location) else {
            return;
        };
        // The description must come from the device that answered, and (unless
        // explicitly allowed) from the default gateway itself: LOCATION is attacker
        // controlled data on a shared LAN.
        if from.ip() != IpAddr::V4(url.ip) {
            return;
        }
        if url.ip != net.gateway && !(self.cfg.allow_foreign_igd && lan_address(url.ip)) {
            return;
        }
        let request = http::build_get(&url, &self.cfg.user_agent);
        self.phase = Phase::Discovering {
            idx,
            step: Step::Description,
        };
        self.start_http(HttpWhat::Description(url.clone()), url, request, now, out);
    }

    // ---- UDP transactions -----------------------------------------------------

    fn start_udp(
        &mut self,
        what: UdpWhat,
        method: Method,
        packet: Vec<u8>,
        max_tries: u8,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        let Some(net) = self.net else { return };
        let base = match method {
            Method::Pcp => pcp::INITIAL_RETRY_MS,
            _ => natpmp::INITIAL_RETRY_MS,
        };
        let wait = self.jittered(method, base);
        out.push(Output::Udp {
            to: SocketAddr::V4(SocketAddrV4::new(net.gateway, self.cfg.gateway_port)),
            data: packet.clone(),
        });
        self.txn = Some(Txn::Udp(UdpTxn {
            what,
            method,
            packet,
            sent: 1,
            max_tries: max_tries.max(1),
            wait_ms: base,
            next_at: now.plus_ms(wait),
        }));
    }

    /// PCP retransmission timers get +-10 % jitter (RFC 6887 8.1.1); NAT-PMP's are exact.
    fn jittered(&mut self, method: Method, ms: u64) -> u64 {
        if method == Method::Pcp {
            let span = ms / 5;
            ms - ms / 10 + self.rng.below(span + 1)
        } else {
            ms
        }
    }

    fn start_map_udp(
        &mut self,
        i: usize,
        method: Method,
        tries: u8,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        let (Some(net), Some(slot)) = (self.net, self.slots.get_mut(i)) else {
            return;
        };
        // Ask for the external port we last had, else the preferred one, else the
        // internal port: a stable external port is what trackers and peers need.
        let ext = if slot.ext_hint != 0 {
            slot.ext_hint
        } else if slot.want.suggested_external != 0 {
            slot.want.suggested_external
        } else {
            slot.want.internal_port
        };
        let packet = match method {
            Method::Pcp => pcp::encode_map_request(
                net.local,
                slot.nonce,
                slot.want.protocol,
                slot.want.internal_port,
                ext,
                self.cfg.lifetime_secs,
            )
            .to_vec(),
            _ => natpmp::encode_map_request(
                slot.want.protocol,
                slot.want.internal_port,
                ext,
                self.cfg.lifetime_secs,
            )
            .to_vec(),
        };
        slot.busy = true;
        slot.maybe_mapped = true;
        self.start_udp(UdpWhat::Map(i), method, packet, tries, now, out);
    }

    fn start_delete_udp(&mut self, i: usize, method: Method, now: Millis, out: &mut Vec<Output>) {
        let (Some(net), Some(slot)) = (self.net, self.slots.get(i)) else {
            return;
        };
        let packet = match method {
            Method::Pcp => pcp::encode_map_request(
                net.local,
                slot.nonce,
                slot.want.protocol,
                slot.want.internal_port,
                0,
                0,
            )
            .to_vec(),
            _ => natpmp::encode_map_request(slot.want.protocol, slot.want.internal_port, 0, 0)
                .to_vec(),
        };
        let tries = self.cfg.delete_tries;
        self.start_udp(UdpWhat::Delete(i), method, packet, tries, now, out);
    }

    fn on_udp(&mut self, from: SocketAddr, data: &[u8], now: Millis, out: &mut Vec<Output>) {
        let Some(net) = self.net else { return };
        if matches!(
            self.phase,
            Phase::Discovering {
                step: Step::Ssdp { .. },
                ..
            }
        ) {
            self.on_ssdp(from, data, now, out);
            return;
        }
        // Only the gateway's server port may answer a NAT-PMP/PCP request.
        if from.ip() != IpAddr::V4(net.gateway) || from.port() != self.cfg.gateway_port {
            return;
        }
        let Some(Txn::Udp(t)) = &self.txn else { return };
        match t.method {
            Method::NatPmp => self.on_natpmp(data, now, out),
            Method::Pcp => self.on_pcp(data, now, out),
            Method::Upnp => {}
        }
    }

    fn discovery_idx(&self) -> Option<usize> {
        match self.phase {
            Phase::Discovering { idx, .. } => Some(idx),
            _ => None,
        }
    }

    fn on_natpmp(&mut self, data: &[u8], now: Millis, out: &mut Vec<Output>) {
        let Ok(resp) = natpmp::decode_response(data) else {
            return;
        };
        let Some(Txn::Udp(t)) = &self.txn else { return };
        let what = t.what.clone();
        match (what, resp) {
            (
                UdpWhat::Probe | UdpWhat::ExtAddr,
                natpmp::Response::ExternalAddress { epoch, address },
            ) => {
                let was_probe = matches!(
                    self.txn,
                    Some(Txn::Udp(UdpTxn {
                        what: UdpWhat::Probe,
                        ..
                    }))
                );
                self.txn = None;
                self.check_epoch(epoch, now, out);
                if was_probe {
                    self.select_method(Method::NatPmp, now, out);
                } else {
                    self.ext_refresh_at = Some(now.plus(self.cfg.ext_refresh));
                }
                self.set_ext_ip(address, out);
            }
            (
                UdpWhat::Probe,
                natpmp::Response::Failure {
                    opcode: 0, code, ..
                },
            ) => {
                let (reason, _) = natpmp_failure(code);
                if let Some(idx) = self.discovery_idx() {
                    self.method_failed(idx, reason, now, out);
                }
            }
            (
                UdpWhat::ExtAddr,
                natpmp::Response::Failure {
                    opcode: 0, epoch, ..
                },
            ) => {
                self.txn = None;
                self.check_epoch(epoch, now, out);
                self.ext_refresh_at = Some(now.plus(self.cfg.ext_refresh));
            }
            (
                UdpWhat::Map(i),
                natpmp::Response::Mapping {
                    protocol,
                    epoch,
                    internal_port,
                    external_port,
                    lifetime,
                },
            ) => {
                let ok = self.slots.get(i).is_some_and(|s| {
                    s.want.protocol == protocol && s.want.internal_port == internal_port
                });
                if !ok {
                    return;
                }
                self.txn = None;
                self.check_epoch(epoch, now, out);
                self.mapping_granted(
                    i,
                    Grant::new(Method::NatPmp, external_port, lifetime),
                    now,
                    out,
                );
            }
            (
                UdpWhat::Map(i),
                natpmp::Response::Failure {
                    opcode,
                    code,
                    epoch,
                },
            ) => {
                let Some(s) = self.slots.get(i) else { return };
                let want_op = if s.want.protocol == Protocol::Tcp {
                    2
                } else {
                    1
                };
                if opcode != want_op {
                    return;
                }
                self.txn = None;
                self.check_epoch(epoch, now, out);
                let (reason, floor) = natpmp_failure(code);
                self.slot_failed(i, reason, floor, now, out);
            }
            (
                UdpWhat::Delete(i),
                natpmp::Response::Mapping {
                    protocol,
                    internal_port,
                    ..
                },
            ) => {
                let ok = self.slots.get(i).is_some_and(|s| {
                    s.want.protocol == protocol && s.want.internal_port == internal_port
                });
                if ok {
                    self.txn = None;
                    self.deleted(i, true, out);
                }
            }
            (UdpWhat::Delete(i), natpmp::Response::Failure { opcode, .. }) => {
                let Some(s) = self.slots.get(i) else { return };
                if opcode
                    == if s.want.protocol == Protocol::Tcp {
                        2
                    } else {
                        1
                    }
                {
                    self.txn = None;
                    self.deleted(i, false, out);
                }
            }
            _ => {}
        }
    }

    fn on_pcp(&mut self, data: &[u8], now: Millis, out: &mut Vec<Output>) {
        let decoded = pcp::decode_response(data);
        let Some(Txn::Udp(t)) = &self.txn else { return };
        let what = t.what.clone();
        let (UdpWhat::Map(i) | UdpWhat::Delete(i)) = what else {
            return;
        };
        let deleting = matches!(what, UdpWhat::Delete(_));
        let in_discovery = self.discovery_idx();
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        match decoded {
            Err(pcp::DecodeError::LegacyNatPmpServer) => {
                if let Some(idx) = in_discovery {
                    self.method_failed(idx, Failure::Unsupported, now, out);
                }
            }
            Err(_) => {}
            Ok(pcp::Response::Map(m)) => {
                if m.nonce != slot.nonce
                    || m.protocol != slot.want.protocol.number()
                    || m.internal_port != slot.want.internal_port
                {
                    return;
                }
                self.txn = None;
                if deleting {
                    self.deleted(i, true, out);
                    return;
                }
                self.check_epoch(m.epoch, now, out);
                if let Some(idx) = in_discovery {
                    let _ = idx;
                    self.select_method(Method::Pcp, now, out);
                }
                if let IpAddr::V4(ip) = m.external_ip {
                    self.set_ext_ip(Some(ip), out);
                }
                self.mapping_granted(
                    i,
                    Grant::new(Method::Pcp, m.external_port, m.lifetime),
                    now,
                    out,
                );
            }
            Ok(pcp::Response::Failure {
                code,
                lifetime,
                epoch,
                nonce,
                ..
            }) => {
                if nonce.is_some_and(|n| n != slot.nonce) {
                    return;
                }
                self.txn = None;
                if deleting {
                    self.deleted(i, false, out);
                    return;
                }
                self.check_epoch(epoch, now, out);
                let (reason, floor) = pcp_failure(code, lifetime);
                match in_discovery {
                    Some(idx) => self.method_failed(idx, reason, now, out),
                    None => self.slot_failed(i, reason, floor, now, out),
                }
            }
        }
    }

    // ---- epoch ------------------------------------------------------------------

    /// RFC 6886 3.6 / RFC 6887 8.5: a gateway whose epoch fell behind lost its table.
    fn check_epoch(&mut self, epoch: u32, now: Millis, out: &mut Vec<Output>) {
        let reset = self
            .epoch
            .is_some_and(|(e, at)| natpmp::epoch_reset(e, at.0, epoch, now.0));
        self.epoch = Some((epoch, now));
        if reset {
            out.push(Output::Event(Event::GatewayRestarted));
            for s in &mut self.slots {
                if !s.busy {
                    if matches!(s.state, SlotState::Mapped { .. }) {
                        s.last = None;
                    }
                    s.state = SlotState::Idle;
                    s.fails = 0;
                }
            }
        }
    }

    // ---- slot results -------------------------------------------------------------

    /// `reported` is the lifetime shown to the application (0 = permanent).
    fn mapping_granted(&mut self, i: usize, g: Grant, now: Millis, out: &mut Vec<Output>) {
        let Grant {
            method,
            external_port,
            lifetime,
            reported,
        } = g;
        if lifetime == 0 {
            self.slot_failed(i, Failure::Protocol(0), Duration::ZERO, now, out);
            return;
        }
        let gap = u64::try_from(self.cfg.min_renew_gap.as_millis()).unwrap_or(u64::MAX);
        let renew_after = (u64::from(lifetime) * 1000 / 2).max(gap);
        let ext_ip = self.ext_ip;
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        slot.busy = false;
        slot.fails = 0;
        slot.conflict_retries = 0;
        slot.ext_hint = external_port;
        slot.state = SlotState::Mapped {
            lifetime,
            renew_at: now.plus_ms(renew_after),
            expires_at: now.plus_ms(u64::from(lifetime) * 1000),
        };
        let m = Mapping {
            protocol: slot.want.protocol,
            internal_port: slot.want.internal_port,
            external_port,
            external_ip: ext_ip,
            lifetime_secs: reported,
            method,
        };
        let changed = slot
            .last
            .is_none_or(|l| l.external_port != external_port || l.method != method);
        slot.last = Some(m);
        out.push(Output::Event(if changed {
            Event::Mapped(m)
        } else {
            Event::Renewed(m)
        }));
    }

    fn backoff(&mut self, fails: u8) -> Duration {
        let shift = u32::from(fails.saturating_sub(1)).min(16);
        let min = self.cfg.backoff_min.as_millis();
        let max = self.cfg.backoff_max.as_millis().max(min);
        let base = (min << shift).min(max);
        let jitter = u128::from(
            self.rng
                .below(u64::try_from(base / 10).unwrap_or(u64::MAX) + 1),
        );
        Duration::from_millis(u64::try_from(base + jitter).unwrap_or(u64::MAX))
    }

    fn slot_failed(
        &mut self,
        i: usize,
        reason: Failure,
        floor: Duration,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        let Some(fails) = self.slots.get_mut(i).map(|s| {
            s.busy = false;
            s.fails = s.fails.saturating_add(1);
            s.fails
        }) else {
            return;
        };
        let delay = self
            .backoff(fails)
            .max(floor)
            .min(Duration::from_secs(24 * 3600));
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        let (protocol, internal_port) = (slot.want.protocol, slot.want.internal_port);
        let transient = matches!(reason, Failure::Timeout | Failure::Transport);
        match slot.state {
            // A renewal that failed while the old mapping is still valid: keep it,
            // and retry before it lapses (but never closer than the renew gap).
            SlotState::Mapped {
                expires_at,
                lifetime,
                ..
            } if transient && now < expires_at => {
                let left = expires_at.since(now);
                let retry = delay.min(left / 2).max(self.cfg.min_renew_gap);
                slot.state = SlotState::Mapped {
                    lifetime,
                    renew_at: now.plus(retry),
                    expires_at,
                };
                out.push(Output::Event(Event::SlotFailed {
                    protocol,
                    internal_port,
                    reason,
                    retry_in: retry,
                }));
            }
            state => {
                if matches!(state, SlotState::Mapped { .. }) {
                    slot.last = None;
                    out.push(Output::Event(Event::Lost {
                        protocol,
                        internal_port,
                    }));
                }
                slot.state = SlotState::Backoff {
                    until: now.plus(delay),
                };
                out.push(Output::Event(Event::SlotFailed {
                    protocol,
                    internal_port,
                    reason,
                    retry_in: delay,
                }));
            }
        }
    }

    // ---- HTTP (UPnP) ----------------------------------------------------------------

    fn start_http(
        &mut self,
        what: HttpWhat,
        url: Url,
        request: Vec<u8>,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let timeout = self.cfg.http_timeout;
        out.push(Output::Http(HttpRequest {
            id,
            to: url.addr(),
            request,
            max_response: http::MAX_RESPONSE,
            timeout,
        }));
        self.txn = Some(Txn::Http(HttpTxn {
            id,
            what,
            deadline: now.plus(timeout).plus_ms(1000),
        }));
    }

    fn soap(&self, action: &str, body: &str) -> Option<(Url, Vec<u8>)> {
        let t = self.upnp.as_ref()?;
        let req = http::build_soap_post(
            &t.control,
            &self.cfg.user_agent,
            &t.service_type,
            action,
            body,
        );
        Some((t.control.clone(), req))
    }

    fn start_ext_ip_http(&mut self, discovery: bool, now: Millis, out: &mut Vec<Output>) {
        let Some(t) = &self.upnp else { return };
        let body = upnp::external_ip_body(&t.service_type);
        if let Some((url, req)) = self.soap("GetExternalIPAddress", &body) {
            self.start_http(HttpWhat::ExtIp { discovery }, url, req, now, out);
        }
    }

    fn upnp_ext_port(&self, slot: &Slot) -> u16 {
        if slot.ext_hint != 0 {
            slot.ext_hint
        } else if slot.want.suggested_external != 0 {
            slot.want.suggested_external
        } else {
            slot.want.internal_port
        }
    }

    fn start_add_http(&mut self, i: usize, now: Millis, out: &mut Vec<Output>) {
        let (Some(net), Some(t)) = (self.net, self.upnp.as_ref()) else {
            return;
        };
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        let ext = self.upnp_ext_port(slot);
        let lease = if slot.permanent {
            0
        } else {
            self.cfg.lifetime_secs
        };
        let body = upnp::add_mapping_body(
            &t.service_type,
            slot.want.protocol,
            ext,
            slot.want.internal_port,
            net.local,
            &self.cfg.description,
            lease,
        );
        let Some((url, req)) = self.soap("AddPortMapping", &body) else {
            return;
        };
        if let Some(slot) = self.slots.get_mut(i) {
            slot.busy = true;
            slot.maybe_mapped = true;
            slot.ext_hint = ext;
        }
        self.start_http(HttpWhat::Add(i), url, req, now, out);
    }

    fn start_delete_http(&mut self, i: usize, now: Millis, out: &mut Vec<Output>) {
        let Some(t) = self.upnp.as_ref() else { return };
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        let ext = self.upnp_ext_port(slot);
        let body = upnp::delete_mapping_body(&t.service_type, slot.want.protocol, ext);
        if let Some((url, req)) = self.soap("DeletePortMapping", &body) {
            self.start_http(HttpWhat::Delete(i), url, req, now, out);
        }
    }

    fn on_http(
        &mut self,
        id: u64,
        outcome: Result<Vec<u8>, HttpFailure>,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        let Some(Txn::Http(t)) = &self.txn else {
            return;
        };
        if t.id != id {
            return; // late answer for something we gave up on
        }
        let what = t.what.clone();
        self.txn = None;
        let response = outcome
            .map_err(|_| ())
            .and_then(|raw| http::parse_response(&raw, http::MAX_RESPONSE).map_err(|_| ()));
        match what {
            HttpWhat::Description(url) => {
                let idx = self.discovery_idx();
                let Some(idx) = idx else { return };
                match response.ok().and_then(|r| self.accept_description(&url, r)) {
                    Some(()) => {
                        self.phase = Phase::Discovering {
                            idx,
                            step: Step::ExtIp,
                        };
                        self.start_ext_ip_http(true, now, out);
                    }
                    None => self.method_failed(idx, Failure::Transport, now, out),
                }
            }
            HttpWhat::ExtIp { discovery } => {
                let reply = response.map(|r| upnp::parse_reply(r.status, &r.body));
                if discovery {
                    let Some(idx) = self.discovery_idx() else {
                        return;
                    };
                    match reply {
                        Ok(upnp::SoapReply::Ok { external_ip }) => {
                            self.select_method(Method::Upnp, now, out);
                            self.set_ext_ip(external_ip, out);
                        }
                        Ok(upnp::SoapReply::Fault { code, .. }) => {
                            self.method_failed(idx, Failure::Protocol(code.unwrap_or(0)), now, out)
                        }
                        Err(()) => self.method_failed(idx, Failure::Transport, now, out),
                    }
                } else {
                    self.ext_refresh_at = Some(now.plus(self.cfg.ext_refresh));
                    if let Ok(upnp::SoapReply::Ok { external_ip }) = reply {
                        self.set_ext_ip(external_ip, out);
                    }
                }
            }
            HttpWhat::Add(i) => {
                let reply = response.map(|r| upnp::parse_reply(r.status, &r.body));
                self.upnp_add_result(i, reply, now, out);
            }
            HttpWhat::Delete(i) => {
                let ok = matches!(
                    response.map(|r| upnp::parse_reply(r.status, &r.body)),
                    Ok(upnp::SoapReply::Ok { .. })
                );
                self.deleted(i, ok, out);
            }
        }
    }

    /// Validate a device description and remember its WAN service.
    fn accept_description(&mut self, location: &Url, r: http::Response) -> Option<()> {
        if r.status != 200 {
            return None;
        }
        let text = String::from_utf8_lossy(&r.body);
        let desc = upnp::parse_description(&text).ok()?;
        let (svc, _) = upnp::pick_service(&desc)?;
        let base = desc
            .url_base
            .as_deref()
            .and_then(http::parse_url)
            .filter(|b| b.ip == location.ip)
            .unwrap_or_else(|| location.clone());
        let control = http::resolve(&base, &svc.control_url)?;
        // The control endpoint must stay on the device we discovered.
        if control.ip != location.ip {
            return None;
        }
        self.upnp = Some(UpnpTarget {
            service_type: svc.service_type.clone(),
            control,
        });
        Some(())
    }

    fn upnp_add_result(
        &mut self,
        i: usize,
        reply: Result<upnp::SoapReply, ()>,
        now: Millis,
        out: &mut Vec<Output>,
    ) {
        match reply {
            Ok(upnp::SoapReply::Ok { .. }) => {
                let (lease, ext) = match self.slots.get(i) {
                    Some(s) => (
                        if s.permanent {
                            0
                        } else {
                            self.cfg.lifetime_secs
                        },
                        s.ext_hint,
                    ),
                    None => return,
                };
                self.upnp_granted(i, ext, lease, now, out);
            }
            Ok(upnp::SoapReply::Fault { code, .. }) => {
                let Some(slot) = self.slots.get_mut(i) else {
                    return;
                };
                match code {
                    // OnlyPermanentLeasesSupported: ask again without a lease.
                    Some(725) if !slot.permanent => {
                        slot.permanent = true;
                        slot.busy = false;
                        slot.state = SlotState::Idle;
                    }
                    // ConflictInMappingEntry: someone else holds that external port.
                    Some(718) if slot.conflict_retries < 3 => {
                        slot.conflict_retries += 1;
                        slot.ext_hint = 49152 + self.rng.below(65535 - 49152 + 1) as u16;
                        slot.busy = false;
                        slot.state = SlotState::Idle;
                    }
                    // SamePortValuesRequired: external must equal internal.
                    Some(724) if slot.ext_hint != slot.want.internal_port => {
                        slot.ext_hint = slot.want.internal_port;
                        slot.busy = false;
                        slot.state = SlotState::Idle;
                    }
                    c => {
                        let reason = match c {
                            Some(718 | 724 | 727) => Failure::NoResources,
                            Some(729) => Failure::Refused,
                            Some(n) => Failure::Protocol(n),
                            None => Failure::Transport,
                        };
                        self.slot_failed(i, reason, Duration::ZERO, now, out);
                    }
                }
            }
            Err(()) => self.slot_failed(i, Failure::Transport, Duration::ZERO, now, out),
        }
    }

    /// UPnP grants nothing back: the mapping is what we asked for. A permanent
    /// mapping (lease 0) is re-asserted every 2 h (renewal at half) in case the
    /// router forgot it.
    fn upnp_granted(&mut self, i: usize, ext: u16, lease: u32, now: Millis, out: &mut Vec<Output>) {
        let effective = if lease > 0 { lease } else { 2 * 3600 };
        let grant = Grant {
            reported: lease,
            ..Grant::new(Method::Upnp, ext, effective)
        };
        self.mapping_granted(i, grant, now, out);
    }

    // ---- shutdown -----------------------------------------------------------------------

    fn on_shutdown(&mut self, now: Millis, out: &mut Vec<Output>) {
        if matches!(self.phase, Phase::Stopping { .. } | Phase::Done) {
            return;
        }
        self.txn = None;
        let method = match self.phase {
            Phase::Active(m) => Some(m),
            _ => None,
        };
        let queue: VecDeque<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.maybe_mapped)
            .map(|(i, _)| i)
            .collect();
        for s in &mut self.slots {
            s.busy = false;
        }
        match method {
            Some(method) if !queue.is_empty() => {
                self.phase = Phase::Stopping {
                    deadline: now.plus(self.cfg.shutdown_deadline),
                    queue,
                    method,
                };
            }
            _ => {
                self.phase = Phase::Done;
                out.push(Output::Event(Event::ShutdownComplete));
            }
        }
    }

    fn deleted(&mut self, i: usize, ok: bool, out: &mut Vec<Output>) {
        if let Some(s) = self.slots.get_mut(i) {
            s.maybe_mapped = false;
            s.state = SlotState::Idle;
            s.last = None;
            if ok {
                out.push(Output::Event(Event::Deleted {
                    protocol: s.want.protocol,
                    internal_port: s.want.internal_port,
                }));
            }
        }
    }

    // ---- timers and pump ----------------------------------------------------------------------

    fn run_timers(&mut self, now: Millis, out: &mut Vec<Output>) {
        // discovery wait
        if let Phase::Waiting { until } = self.phase
            && now >= until {
                self.begin_discovery(now, out);
            }
        // SSDP rounds
        if let Phase::Discovering {
            idx,
            step: Step::Ssdp {
                next_at, end_at, ..
            },
        } = self.phase
        {
            if now >= end_at {
                self.method_failed(idx, Failure::Timeout, now, out);
            } else if now >= next_at {
                self.ssdp_round(now, out);
            }
        }
        // transaction timers
        match self.txn.clone() {
            Some(Txn::Udp(t)) if now >= t.next_at => self.udp_timer(t, now, out),
            Some(Txn::Http(h)) if now >= h.deadline => {
                self.on_http(h.id, Err(HttpFailure::Timeout), now, out)
            }
            _ => {}
        }
        // shutdown deadline
        if let Phase::Stopping { deadline, .. } = self.phase
            && now >= deadline {
                self.txn = None;
                self.phase = Phase::Done;
                out.push(Output::Event(Event::ShutdownComplete));
            }
        // mappings that lapsed without a successful renewal
        if matches!(self.phase, Phase::Active(_)) {
            for i in 0..self.slots.len() {
                let expired = matches!(
                    self.slots[i].state,
                    SlotState::Mapped { expires_at, .. } if now >= expires_at
                ) && !self.slots[i].busy;
                if expired {
                    self.slot_failed(i, Failure::Timeout, Duration::ZERO, now, out);
                }
            }
        }
    }

    fn udp_timer(&mut self, mut t: UdpTxn, now: Millis, out: &mut Vec<Output>) {
        if t.sent < t.max_tries {
            // retransmit: 250 ms, 500 ms, ... (NAT-PMP) / 3 s, 6 s, ... (PCP)
            let Some(net) = self.net else { return };
            t.sent += 1;
            t.wait_ms = t.wait_ms.saturating_mul(2);
            let wait = self.jittered(t.method, t.wait_ms);
            t.next_at = now.plus_ms(wait);
            out.push(Output::Udp {
                to: SocketAddr::V4(SocketAddrV4::new(net.gateway, self.cfg.gateway_port)),
                data: t.packet.clone(),
            });
            self.txn = Some(Txn::Udp(t));
            return;
        }
        self.txn = None;
        match t.what {
            UdpWhat::Probe => {
                if let Some(idx) = self.discovery_idx() {
                    self.method_failed(idx, Failure::Timeout, now, out);
                }
            }
            UdpWhat::ExtAddr => self.ext_refresh_at = Some(now.plus(Duration::from_secs(600))),
            UdpWhat::Map(i) => match self.discovery_idx() {
                Some(idx) => self.method_failed(idx, Failure::Timeout, now, out),
                None => self.slot_failed(i, Failure::Timeout, Duration::ZERO, now, out),
            },
            UdpWhat::Delete(i) => self.deleted(i, false, out),
        }
    }

    /// Earliest time something is due in the active phase.
    fn next_due(&self, _m: Method) -> Option<Millis> {
        let slots = self
            .slots
            .iter()
            .filter(|s| !s.busy)
            .map(|s| match s.state {
                SlotState::Idle => Millis(0),
                SlotState::Mapped { renew_at, .. } => renew_at,
                SlotState::Backoff { until } => until,
            });
        slots.chain(self.ext_refresh_at).min()
    }

    fn pump(&mut self, now: Millis, out: &mut Vec<Output>) {
        if self.txn.is_some() {
            return;
        }
        match self.phase.clone() {
            Phase::Active(method) => {
                // earliest due slot, in slot order on ties
                let mut best: Option<(Millis, Option<usize>)> = None;
                for (i, s) in self.slots.iter().enumerate() {
                    if s.busy {
                        continue;
                    }
                    let due = match s.state {
                        SlotState::Idle => Millis(0),
                        SlotState::Mapped { renew_at, .. } => renew_at,
                        SlotState::Backoff { until } => until,
                    };
                    if best.is_none_or(|(d, _)| due < d) {
                        best = Some((due, Some(i)));
                    }
                }
                if let Some(at) = self.ext_refresh_at
                    && best.is_none_or(|(d, _)| at < d) {
                        best = Some((at, None));
                    }
                let Some((due, which)) = best else { return };
                if due > now {
                    return;
                }
                match (method, which) {
                    (Method::Upnp, Some(i)) => self.start_add_http(i, now, out),
                    (Method::Upnp, None) => self.start_ext_ip_http(false, now, out),
                    (Method::NatPmp, Some(i)) => {
                        let tries = self.cfg.natpmp_tries;
                        self.start_map_udp(i, Method::NatPmp, tries, now, out)
                    }
                    (Method::Pcp, Some(i)) => {
                        let tries = self.cfg.pcp_tries;
                        self.start_map_udp(i, Method::Pcp, tries, now, out)
                    }
                    (Method::NatPmp, None) => {
                        let packet = natpmp::encode_external_address_request().to_vec();
                        let tries = self.cfg.natpmp_tries;
                        self.start_udp(UdpWhat::ExtAddr, Method::NatPmp, packet, tries, now, out)
                    }
                    (Method::Pcp, None) => self.ext_refresh_at = None,
                }
                if self.txn.is_none() {
                    // Could not start (no UPnP target, ...): back off instead of spinning.
                    match which {
                        Some(i) => {
                            self.slot_failed(i, Failure::Transport, Duration::ZERO, now, out)
                        }
                        None => self.ext_refresh_at = Some(now.plus(self.cfg.ext_refresh)),
                    }
                }
            }
            Phase::Stopping { queue, method, .. } => {
                let mut queue = queue;
                match queue.pop_front() {
                    Some(i) => {
                        if let Phase::Stopping { queue: q, .. } = &mut self.phase {
                            *q = queue;
                        }
                        match method {
                            Method::Upnp => self.start_delete_http(i, now, out),
                            m => self.start_delete_udp(i, m, now, out),
                        }
                        if self.txn.is_none() {
                            // nothing could be sent for this slot; move on next pump
                            self.pump(now, out);
                        }
                    }
                    None => {
                        self.phase = Phase::Done;
                        out.push(Output::Event(Event::ShutdownComplete));
                    }
                }
            }
            _ => {}
        }
    }
}

fn new_slot(want: Wanted, rng: &mut Rng) -> Slot {
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce);
    Slot {
        want,
        nonce,
        state: SlotState::Idle,
        busy: false,
        maybe_mapped: false,
        fails: 0,
        ext_hint: 0,
        permanent: false,
        conflict_retries: 0,
        last: None,
    }
}

fn lan_address(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local() || ip.is_loopback()
}

fn natpmp_failure(code: natpmp::ResultCode) -> (Failure, Duration) {
    use natpmp::ResultCode as C;
    match code {
        C::NotAuthorized => (Failure::Refused, Duration::from_secs(6 * 3600)),
        C::NetworkFailure => (Failure::NetworkFailure, Duration::from_secs(300)),
        C::OutOfResources => (Failure::NoResources, Duration::from_secs(300)),
        C::UnsupportedVersion | C::UnsupportedOpcode => {
            (Failure::Unsupported, Duration::from_secs(3600))
        }
        C::Other(n) => (Failure::Protocol(u32::from(n)), Duration::ZERO),
        C::Success => (Failure::Protocol(0), Duration::ZERO),
    }
}

fn pcp_failure(code: pcp::ResultCode, lifetime: u32) -> (Failure, Duration) {
    use pcp::ResultCode as C;
    // In an error response the lifetime field is the server's "do not retry before".
    let hint = Duration::from_secs(u64::from(lifetime.min(3600)));
    match code {
        C::NotAuthorized => (Failure::Refused, hint.max(Duration::from_secs(6 * 3600))),
        C::NetworkFailure => (Failure::NetworkFailure, hint.max(Duration::from_secs(300))),
        C::NoResources | C::UserExQuota | C::ExcessiveRemotePeers => {
            (Failure::NoResources, hint.max(Duration::from_secs(300)))
        }
        C::UnsupportedVersion
        | C::UnsupportedOpcode
        | C::UnsupportedProtocol
        | C::UnsupportedOption => (Failure::Unsupported, hint.max(Duration::from_secs(3600))),
        C::AddressMismatch => (Failure::AddressMismatch, hint.max(Duration::from_secs(300))),
        C::Success => (Failure::Protocol(0), hint),
        C::MalformedRequest | C::MalformedOption | C::CannotProvideExternal => {
            let n = match code {
                C::MalformedRequest => 3,
                C::MalformedOption => 6,
                _ => 11,
            };
            (Failure::Protocol(n), hint)
        }
        C::Other(n) => (Failure::Protocol(u32::from(n)), hint),
    }
}
