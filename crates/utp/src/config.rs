//! Tunables for connections and the endpoint. Every collection that external peers can grow has a
//! cap here; see the README table.

use std::time::Duration;

/// Configuration shared by an [`Endpoint`](crate::Endpoint) and its connections.
#[derive(Clone, Debug)]
pub struct Config {
    /// Largest payload bytes per packet on an ordinary (non-loopback) path: 1400 plus 20 B header,
    /// up to 34 B of SACK and 28 B IP/UDP fits a 1500 B MTU on IPv4 and IPv6 without
    /// fragmentation. With [`mtu_discovery`](Self::mtu_discovery) on, a
    /// connection starts smaller and probes up to this size (loopback peers up to
    /// [`mtu_loopback_datagram`](Self::mtu_loopback_datagram)); with it off this is the fixed size.
    pub max_payload: usize,
    /// Path-MTU discovery in the libutp style: start at [`mtu_floor_datagram`](Self::mtu_floor_datagram),
    /// probe upward with bigger data packets (DF set on the first transmission only), bisect on probe loss,
    /// never treat probe loss as congestion, re-probe every [`mtu_reprobe`](Self::mtu_reprobe).
    pub mtu_discovery: bool,
    /// Datagram size (UDP payload) every path is assumed to carry: the IPv6 minimum MTU of 1280
    /// minus IP and UDP headers. Connections start here.
    pub mtu_floor_datagram: usize,
    /// Largest datagram (UDP payload) probed towards a peer on this host (loopback, or one of our
    /// own addresses: the kernel routes both over the loopback interface, whose MTU is 16 KiB). Kept under 4096 because Transmission reads UDP into a 4096 B buffer.
    pub mtu_loopback_datagram: usize,
    /// How often a settled path-MTU search starts over from the ceiling (libutp: 30 minutes).
    pub mtu_reprobe: Duration,
    /// Bytes the application may queue for sending per connection (allocated lazily).
    pub send_buffer: usize,
    /// Receive buffer per connection; also the window advertised to the peer (allocated lazily).
    pub recv_buffer: usize,
    /// LEDBAT target queuing delay (libutp and BEP 29: 100 ms).
    pub target_delay: Duration,
    /// Initial congestion window in packets.
    pub initial_window_packets: u32,
    /// Hard ceiling on the congestion window.
    pub max_window: u32,
    /// RTO before any RTT sample exists.
    pub initial_rto: Duration,
    /// RTO floor (libutp uses 500 ms).
    pub min_rto: Duration,
    /// RTO ceiling after exponential backoff.
    pub max_rto: Duration,
    /// Consecutive timeouts without ack progress before the connection is declared dead.
    /// With the default 500 ms floor and doubling this tolerates about 90 s of total blackout.
    pub max_retransmits: u8,
    /// SYN retransmissions before `connect` fails (1 s, 2 s, 4 s, 8 s: gives up after 15 s).
    pub syn_retries: u8,
    /// Longest an in-order data ack may be delayed (acks go out immediately every 2nd packet and
    /// on any gap, duplicate or window update).
    pub delayed_ack: Duration,
    /// Send a STATE packet after this much send silence (libutp: 29 s) so NATs keep the mapping.
    pub keepalive: Duration,
    /// Drop a connection after this long without any packet from the peer. `None` disables.
    pub idle_timeout: Option<Duration>,
    /// How long a cleanly finished connection stays routable to re-ack a retransmitted FIN.
    pub linger: Duration,
    /// An accepted-but-unconfirmed (half-open) connection is dropped after this long.
    pub half_open_timeout: Duration,
    /// Cap on unacknowledged packets per connection (also bounds sender memory metadata).
    pub max_inflight_packets: usize,
    /// Cap on out-of-order packets buffered per connection.
    pub max_reorder_packets: usize,

    /// Cap on live connections per endpoint (inbound + outbound).
    pub max_connections: usize,
    /// Cap on inbound connections that have not yet proven ownership of their address.
    pub max_half_open: usize,
    /// Cap on live connections per remote IP.
    pub max_per_ip: usize,
    /// Cap on confirmed inbound connections waiting for `accept`.
    pub accept_backlog: usize,
    /// RESET replies to packets for unknown connections, per second (token bucket).
    pub resets_per_sec: u32,
    /// Whether SYNs create connections at all (false = outbound only).
    pub accept_incoming: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_payload: 1400,
            mtu_discovery: true,
            mtu_floor_datagram: 1232,
            mtu_loopback_datagram: 4000,
            mtu_reprobe: Duration::from_secs(30 * 60),
            send_buffer: 512 * 1024,
            recv_buffer: 512 * 1024,
            target_delay: Duration::from_millis(100),
            initial_window_packets: 3,
            max_window: 8 * 1024 * 1024,
            initial_rto: Duration::from_secs(1),
            min_rto: Duration::from_millis(500),
            max_rto: Duration::from_secs(16),
            max_retransmits: 8,
            syn_retries: 3,
            delayed_ack: Duration::from_millis(25),
            keepalive: Duration::from_secs(29),
            idle_timeout: Some(Duration::from_secs(180)),
            linger: Duration::from_secs(2),
            half_open_timeout: Duration::from_secs(10),
            max_inflight_packets: 512,
            max_reorder_packets: 512,
            max_connections: 2048,
            max_half_open: 128,
            max_per_ip: 32,
            accept_backlog: 64,
            resets_per_sec: 100,
            accept_incoming: true,
        }
    }
}

impl Config {
    /// Clamp values that would break the protocol into their valid ranges.
    pub fn sanitized(mut self) -> Self {
        self.max_payload = self.max_payload.clamp(256, 60_000);
        self.mtu_loopback_datagram = self.mtu_loopback_datagram.clamp(512, 60_000);
        self.mtu_floor_datagram = self.mtu_floor_datagram.clamp(256 + WIRE_OVERHEAD, 1500);
        if self.mtu_reprobe < Duration::from_secs(1) {
            self.mtu_reprobe = Duration::from_secs(1);
        }
        self.send_buffer = self.send_buffer.max(self.max_payload);
        self.recv_buffer = self.recv_buffer.clamp(self.max_payload, u32::MAX as usize / 2);
        self.max_inflight_packets = self.max_inflight_packets.clamp(4, 4096);
        self.max_reorder_packets = self.max_reorder_packets.clamp(4, 4096);
        self.initial_window_packets = self.initial_window_packets.max(1);
        self.max_window = self.max_window.max(4 * self.max_payload as u32);
        if self.min_rto > self.max_rto {
            self.min_rto = self.max_rto;
        }
        self.max_retransmits = self.max_retransmits.max(1);
        self
    }
}

/// Header bytes a data packet may carry besides its payload: fixed header, the SACK extension's two
/// bytes and the longest SACK bitmask.
pub(crate) const WIRE_OVERHEAD: usize = crate::packet::HEADER_LEN + 2 + crate::packet::MAX_SACK_BYTES;

/// Smallest step the path-MTU search resolves, bytes.
pub(crate) const MTU_RESOLUTION: usize = 16;

impl Config {
    /// Largest datagram (header + SACK + payload) this configuration can put on the wire or must
    /// be ready to receive: sizes every datagram buffer.
    ///
    /// A retransmitted probe keeps its size while its SACK extension may have grown, hence the
    /// slack of one maximal SACK extension on top of the largest probe.
    pub fn max_datagram(&self) -> usize {
        let base = self.max_payload + WIRE_OVERHEAD;
        let big = if self.mtu_discovery { base.max(self.mtu_loopback_datagram) } else { base };
        big + 2 + crate::packet::MAX_SACK_BYTES
    }

    /// Search ceiling for a path whose far end is on this host (`same_host`: loopback, or one of
    /// our own interface addresses, both of which travel over the 16 KiB loopback interface), in
    /// datagram bytes.
    pub(crate) fn path_ceiling(&self, same_host: bool) -> usize {
        let base = self.max_payload + WIRE_OVERHEAD;
        if self.mtu_discovery && same_host { base.max(self.mtu_loopback_datagram) } else { base }
    }

    /// Datagram size a connection starts with given its ceiling.
    pub(crate) fn initial_datagram(&self, ceiling: usize) -> usize {
        if self.mtu_discovery { ceiling.min(self.mtu_floor_datagram) } else { ceiling }
    }
}

/// Duration to wire microseconds, saturating.
pub(crate) fn micros(d: Duration) -> u64 {
    d.as_micros().min(u64::MAX as u128) as u64
}
