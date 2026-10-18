# trs-utp

uTP (BEP 29, "Micro Transport Protocol") with LEDBAT congestion control (RFC 6817) for Seedwell.
Lets the engine talk to the many peers that prefer uTP, and makes background seeding yield to the
user's other traffic (the sender backs off as soon as its own queue builds a standing delay above
100 ms).

* **Sans-I/O core.** `Connection` (one connection) and `Endpoint` (many connections behind one
  UDP socket) are pure state machines: datagrams and an injected `u64` microsecond clock in,
  datagrams out. No sockets, no timers, no threads. All simulator tests run on virtual time.
* **tokio driver.** `UtpSocket` (bind, accept, connect) and `UtpStream` (`AsyncRead + AsyncWrite`,
  usable like a `TcpStream` for the peer wire protocol). One task, one timer and one lock serve
  every connection.
* `#![deny(unsafe_code)]` except one `setsockopt` wrapper (`sockopt.rs`, Don't-Fragment for path-MTU probing); no panics on untrusted packets (fuzzed, see below).

## API

```rust
// --- tokio driver (what the engine uses) -----------------------------------------------------
impl UtpSocket {                       // Clone; cheap handle to the shared socket + driver task
    pub async fn bind(addr: SocketAddr, cfg: Config) -> io::Result<UtpSocket>;
    pub async fn bind_with_foreign(addr: SocketAddr, cfg: Config,
                                   foreign: mpsc::Sender<ForeignDatagram>) -> io::Result<UtpSocket>;
    pub fn from_udp(socket: UdpSocket, cfg: Config,
                    foreign: Option<mpsc::Sender<ForeignDatagram>>) -> io::Result<UtpSocket>;
    pub async fn accept(&self) -> io::Result<(UtpStream, SocketAddr)>;
    pub async fn connect(&self, remote: SocketAddr) -> io::Result<UtpStream>; // TimedOut after ~15 s
    pub async fn send_foreign(&self, to: SocketAddr, data: &[u8]) -> io::Result<usize>;
    pub fn set_foreign_sink(&self, tx: Option<mpsc::Sender<ForeignDatagram>>);
    pub fn set_accepting(&self, on: bool);
    pub fn local_addr(&self) -> SocketAddr;
    pub fn stats(&self) -> SocketStats;  // live/half-open connections, datagram counters, wake-ups
}
pub struct ForeignDatagram { pub from: SocketAddr, pub data: Box<[u8]> }

impl AsyncRead + AsyncWrite for UtpStream {}   // poll_shutdown = half-close (FIN after queued data)
impl UtpStream {
    // readiness style, like TcpStream: lets the engine lease a read buffer only while bytes flow
    pub async fn readable(&self) -> io::Result<()>;   pub fn try_read(&self, out: &mut [u8]) -> io::Result<usize>;
    pub async fn writable(&self) -> io::Result<()>;   pub fn try_write(&self, data: &[u8]) -> io::Result<usize>;
    pub fn peer_addr(&self) -> SocketAddr;  pub fn local_addr(&self) -> SocketAddr;
    pub fn stats(&self) -> Option<Stats>;   // rtt, rto, cwnd, in_flight, queuing delay, retransmits
    pub fn reset(&self);                    // RESET instead of graceful close
    pub fn buffered_heap_bytes(&self) -> usize;
}                                           // Drop = flush buffered data, FIN, linger, free

// --- sans-I/O (drive it yourself, or test it) ---------------------------------------------------
impl Endpoint {
    pub fn new(cfg: Config, seed: u64) -> Endpoint;
    pub fn handle_datagram(&mut self, now: u64, from: SocketAddr, data: &[u8]) -> Disposition; // Utp | Foreign
    pub fn poll_transmit(&mut self, now: u64, buf: &mut [u8]) -> Option<(SocketAddr, usize)>;
    pub fn poll_timeout(&self) -> Option<u64>;      pub fn handle_timeout(&mut self, now: u64);
    pub fn connect(&mut self, now: u64, remote: SocketAddr) -> io::Result<ConnHandle>;
    pub fn poll_accept(&mut self) -> Option<ConnHandle>;
    pub fn poll_event(&mut self) -> Option<(ConnHandle, u8 /* EV_READ | EV_WRITE */)>;
    pub fn read(&mut self, h: ConnHandle, now: u64, out: &mut [u8]) -> io::Result<usize>;  // Ok(0)=EOF, WouldBlock
    pub fn write(&mut self, h: ConnHandle, now: u64, data: &[u8]) -> io::Result<usize>;
    pub fn shutdown_write(&mut self, h: ConnHandle, now: u64);
    pub fn close(&mut self, h: ConnHandle, now: u64);   pub fn reset(&mut self, h: ConnHandle, now: u64);
}
impl Connection { /* handle_packet, poll_transmit, poll_timeout, handle_timeout, read, write, ... */ }
pub mod packet { /* Header, Packet::parse, encode, looks_like_utp, sack_offsets */ }
```

Typical engine use:

```rust
let (dht_tx, dht_rx) = mpsc::channel(1024);
let sock = UtpSocket::bind("0.0.0.0:6881".parse()?, Config::default()).await?; // or bind_with_foreign(.., dht_tx)
let mut inbound = sock.accept().await?.0;                     // then run the BEP 3 handshake on it
let outbound = sock.connect(peer).await?;                     // same code path as TcpStream
```

## Sharing the port with the DHT and UDP trackers

Mainstream clients use one UDP port for uTP, the DHT and UDP-tracker replies. `Endpoint::handle_datagram`
decides from the first byte: a datagram is uTP iff it is >= 20 bytes, the low nibble of byte 0 is `1`
(version) and the high nibble is `0..=4` (type). Bencoded KRPC starts with `d` (0x64), UDP-tracker
replies with `0x00`, NAT-PMP with `0x00`, so none can match. Everything else is returned as
`Disposition::Foreign`; the driver forwards it into the `mpsc::Sender<ForeignDatagram>` given at
bind time (bounded 1024-ish by the caller; dropped and counted in `foreign_dropped` when full) and
`send_foreign` / `try_send_foreign` (never waits) send replies from the same socket. A uTP-shaped but
malformed datagram is consumed and dropped, never forwarded. `trs-engine` uses exactly this: its UDP
socket on the listen port is a `UtpSocket`, and `trs_dht::spawn_shared` drives `DhtCore` from the foreign
channel (see the engine README, "uTP and the shared UDP port").

## Protocol details (libutp-compatible)

| Item | Behaviour |
|---|---|
| Header | version 1, 20 bytes, big-endian; extension chain walked, unknown extensions skipped, SACK = ext 1 |
| Connection ids | initiator: SYN carries `recv_id`, then sends with `recv_id + 1`; acceptor: `recv_id = c + 1`, `send_id = c` |
| SYN | `seq_nr = 1`, `ack_nr = 0`, consumes seq 1; first data packet is seq 2 |
| SYN-ACK | STATE with `ack_nr` = SYN's seq; its `seq_nr` is the acceptor's random initial `S` and consumes nothing; the acceptor's first data packet also carries `S`; the initiator therefore starts with `ack_nr = S - 1` |
| Selective ACK | bit `i` of the mask (LSB first) acks `ack_nr + 2 + i`; emitted whenever a gap exists, 4-byte multiples, up to 32 bytes (256 packets). Loss = 3 SACKed packets beyond a hole. Bare duplicate acks are ignored (a real gap always carries SACK; a bare dup is what a duplicated ack looks like) |
| RESET | accepted on `(addr, id)` matching our recv id, or our send id (libutp's `id`, `id+1`, `id-1` lookup); a RESET never provokes a RESET |
| Timestamps | `timestamp_difference` echoes `their_timestamp - our_receive_time`; LEDBAT uses it as the one-way queuing-delay signal (offset cancels against the base delay) |
| Handshake completion | the initiator sends a STATE immediately after the SYN-ACK. libutp would stay silent until it has data, but our acceptor only hands a connection to the application (and sends data) after the first packet that acks its STATE, so a silent initiator would otherwise never be accepted |
| Not supported | uTP version 0 (23-byte header, pre-2010 clients) |

### LEDBAT (RFC 6817, libutp constants)

* target queuing delay 100 ms; `our_delay = min(last 3 samples) - min(per-minute minima over 10 min)`
  (wrap-aware 32-bit arithmetic, so unknown clock offset and 71-minute wrap are harmless);
* window update per ack: `cwnd += 3000 B * (target - delay)/target * acked/cwnd` (negative above
  target), floor 2 packets, never above `flight + 2 packets` while application- or receiver-limited;
* slow start from 3 packets, exits at 90 % of target delay or on loss; loss halves once per RTT;
  RTO collapses to 1 packet and slow-starts back to half;
* RTO per RFC 6298 (`srtt + 4 rttvar`, Karn's rule, floor 500 ms, cap 16 s, doubles per timeout).
  Default: give up after 8 timeouts without progress, i.e. about 80 s of total silence; SYN gives up
  after 3 retransmits (about 15 s);
* receive window = free receive-buffer bytes; a closed window triggers persist probes (1 byte) and
  never counts toward the timeout; the reader reopening it sends an immediate window update.

### Path-MTU discovery (libutp style)

* A connection starts at a 1232 B datagram (the IPv6 minimum MTU) and probes upward towards
  `max_payload + 54` (1454 B: what a 1500 B path carries), or, for a peer on this host (loopback, or
  one of our own interface addresses, which the kernel also routes over the 16 KiB `lo0`), towards
  `mtu_loopback_datagram` (4000 B; Transmission reads UDP into a 4096 B buffer). The ceiling is tried
  first; after a lost probe the search bisects `[confirmed, lost - 1]` down to 16 B.
* A probe is ordinary data: one new packet bigger than anything confirmed, sent once with the DF bit
  (`IP_DONTFRAG` on macOS, `IP_MTU_DISCOVER=IP_PMTUDISC_PROBE` on Linux, the IPv6 equivalents; set per
  datagram around the send). Acked => confirmed, the payload per packet becomes `datagram - 54` and the
  LEDBAT MSS follows. Lost (3 later SACKs, or the RTO while it is the oldest packet) => ceiling lowered,
  the packet is retransmitted as it is **without DF**, so a smaller path just fragments it. Probe loss
  never touches the congestion window, the RTO back-off or the retry budget.
* A settled search starts over every 30 minutes (`mtu_reprobe`). Confirmed sizes never shrink.
* Padding probes inside an unknown BEP 29 extension were tried first and are **not** an option:
  libutp (Transmission) drops any packet carrying an extension it does not know.
* A receiver cannot tell a truncated datagram from a short one (uTP has no checksum): our receive slots
  are bigger than anything we send and a datagram that fills a slot is dropped.
* `Config::mtu_discovery = false` restores the fixed `max_payload` datagram size.

### Acks and timers

In-order data is acked every second packet, or after `clamp(2 x last inter-arrival gap, 1 ms, 25 ms)`
(a fixed 25 ms stalled a window-limited libutp sender on every odd-length burst; found against
Transmission). Gaps, duplicates, FIN and window re-opens are acked immediately. A STATE keep-alive goes
out after 29 s of send silence; 180 s without any incoming packet drops the connection. A cleanly
finished connection lingers 2 s to re-ack a retransmitted FIN.

## Multiplexer, caps and abuse limits

| Limit (`Config`) | Default | What it bounds |
|---|---|---|
| `max_connections` | 2048 | live connections per endpoint |
| `max_half_open` | 128 | inbound connections that have not yet proven their address |
| `max_per_ip` | 32 | live connections per remote IP |
| `accept_backlog` | 64 | confirmed inbound connections waiting for `accept` |
| `resets_per_sec` | 100 (queue <= 16) | RESET replies to packets for unknown connections; never to a RESET |
| `half_open_timeout` | 10 s | unconfirmed inbound connection lifetime |
| `max_inflight_packets` / `max_reorder_packets` | 512 / 512 | unacked and out-of-order packets per connection |
| `send_buffer` / `recv_buffer` | 512 KiB each | per-connection buffers (allocated on demand) |
| `max_payload` | 1400 | payload per packet: 1400 + 20 + 28 B IP/UDP fits 1500 B on IPv4 and IPv6, no fragmentation |

* **Anti-amplification.** An accepted connection is *unconfirmed* until a packet arrives whose
  `ack_nr` equals `S - 1`, which only a peer that received our STATE can know. Until then it sends
  nothing except STATEs, capped at 3x the bytes received (the SYN-ACK is the same size as the SYN, so
  a flood is reflected at <= 1x), the application cannot see it, and it is dropped after 10 s with no
  traffic. SYNs over any cap are ignored silently.
* Forged acks (`ack_nr` outside `[snd_una - 1, snd_nxt - 1]`) and SACK bits past the flight are ignored.
* Out-of-order buffering is bounded by packet count and by the advertised window; a sender that
  overruns the window is dropped and re-acked.
* Far-future timers (keep-alive, idle, linger, > 5 s) are rounded to whole seconds, so any number of
  idle connections wake the driver at most about once per second in total.

Known weakness shared with libutp: a RESET needs only the peer address and a 16-bit connection id.

## Memory per idle connection

Static structures (`// size:` comments in the source; `struct_sizes_stay_within_budget` fails the
build past 400 B):

| Structure | Bytes |
|---|---|
| `Connection` (state machine, LEDBAT 76, RTT 16, 8 deadlines, counters; boxed) | 344 (+16 allocator) |
| endpoint `Slot` (box ptr, remote address, timer, epoch, flags) | 56 |
| `by_key` hash entry `(SocketAddr, id) -> slot` | about 56 |
| timer set entry `(deadline, slot)` | about 24 |
| driver waker pair | 32 |
| `UtpStream` handle (application side) | 48 |
| **Library total per connection end** | **about 530 + 48 in the stream handle** |

Buffers (`send`, `recv`, in-flight metadata, reorder list) are empty `Vec`s at creation, grow on
demand, and are freed one second after a connection goes idle (`buffered_heap_bytes() == 0`).

Measured RSS (`cargo run --release -p trs-utp --example idle_mem -- N`, two sockets in one process,
each connection exchanged 20 bytes, then idle, N = 500 / 1000 / 3000): **1065 / 999 / 816 bytes of
RSS per connection end**, including tokio's per-task bookkeeping and hash-table slack, with the
buffer heap back to 0 B. With 1000 idle connections the driver task woke **0 times in 20 s**
(keep-alives are 29 s apart and coalesced to whole-second ticks).

## Test results

All simulator numbers are deterministic (virtual time, seeded) and reproduce with
`cargo test -p trs-utp --test sim_basic --test sim_ledbat --release -- --nocapture`.
Simulated path: 10 Mbit/s each way, 40 ms RTT, 200-300 KB drop-tail queue, 1400 B payload.

| Scenario | Result |
|---|---|
| Clean path, 8 MiB one way | 7.11 s, **9.43 Mbit/s = 94.3 % of the link** (0 drops, 0 retransmits) |
| 4 MiB each way, clean 20 Mbit/s | byte-exact both directions; connections close and free completely |
| 1 % loss, 2 MiB each way | 3.06 s, 5.48 Mbit/s per direction, 84 retransmits, 0 RTOs |
| 5 % loss | 10.1 s, 1.66 Mbit/s, 148 retransmits, 0 RTOs |
| 10 % loss | 19.8 s, 0.85 Mbit/s, 345 retransmits, 8 RTOs |
| 10 % reordering (+30 ms) | 9.4 s, 1.79 Mbit/s |
| 5 % duplication | 2.2 s, 7.50 Mbit/s |
| 0-40 ms jitter (FIFO) | 3.1 s, 5.35 Mbit/s |
| 3 % loss + reorder + dup + jitter together | 15.4 s, 1.09 Mbit/s, byte-exact |
| 30 s of 100 % loss mid-transfer | survives; finishes 37.4 s after start (outage 3-33 s), no error |
| Permanent 100 % loss | `TimedOut` 82 s after the outage began; after `close` both endpoints have 0 connections and 0 timers |
| 24 MiB in 256 B packets (~107k packets, both seq spaces wrap) at 1 % loss | byte-exact |
| 60 concurrent connections on one endpoint pair | every stream byte-exact |
| Zero window: reader stalls 20 s / window update lost | recovers, 0 timeouts counted |
| SYN lost / peer absent | connects after 1 s retry / `TimedOut` after 15 s |
| Single LEDBAT flow on a 4 Mbit/s, 500 ms-buffer link | queue delay **median 99 ms, p95 105 ms** (target 100), goodput 3.86 Mbit/s, 0 drops |
| Same link plus a Reno-like TCP flow (rwnd 100 pkts) | uTP **0.07 Mbit/s**, TCP 3.92 Mbit/s, cwnd 69.9 KB -> 2.8 KB; after TCP stops uTP is back at 3.87 Mbit/s |
| Two LEDBAT flows | both progress (7.9 / 21.0 MB in 60 s): the known late-comer advantage, not fixed |

Other suites: `wire.rs` (hand-built libutp-style peers on both sides of the handshake, SACK wire
bytes, fast retransmit of exactly the hole, forged acks/RESETs, RESET rate limit, SYN flood,
per-IP cap, amplification bound, spoofed confirmation, reorder bound, DHT/tracker datagrams, 300k
random/mutated datagrams), 28 unit tests (header codec, demux byte, seq wrapping, RTT/RTO,
LEDBAT window response and delay filter, buffers, idle release, struct sizes).

### Loopback through tokio (macOS, Apple Silicon, release build, 4 worker threads)

`cargo run --release -p trs-utp --example loopback_bench -- <payload> <MiB> <streams>`, CPU from
`/usr/bin/time -l` (user + sys; sys is the UDP syscalls, one per datagram):

| Payload | Streams | Data | Wall | Throughput | CPU (user + sys) |
|---|---|---|---|---|---|
| 1400 B (default) | 1 | 256 MiB | 0.84 s | **321 MB/s** (2.6 Gbit/s) | 0.19 + 1.38 s = 5.9 ms/MB |
| 1400 B | 8 | 256 MiB | 0.83 s | 322 MB/s | 0.15 + 1.26 s |
| 8000 B | 1 | 512 MiB | 0.32 s | **1675 MB/s** (13.4 Gbit/s) | 0.09 + 0.50 s |
| 1400 B, 200 concurrent streams x 256 KiB each way | 200 | 100 MiB | 1.1-1.7 s | 65-99 MB/s aggregate; about 6 % of datagrams are dropped by the default kernel UDP receive buffer during the simultaneous slow starts and recovered by retransmission (an `SO_RCVBUF` knob would help, see gaps) | |

`tests/loopback.rs` also covers echo + half-close, drop-flushes-then-FIN, RESET propagation, connect
failure, the shared port with DHT-style traffic, and byte-exact 128 MiB / 256 MiB transfers.

## Interop (Transmission 4.1.3, libutp)

`crates/utp/interop/transmission.sh` (also `cargo test -p trs-utp --test interop_transmission -- --ignored`)
runs two throw-away `transmission-daemon`s with their own `-g` config dirs under
`bench/runs/utp-interop/` (git-ignored; DHT, LPD, PEX, port-mapping off; never touches
Transmission.app preferences) on a generated 16 MiB torrent:

* **A. Transmission seeds, we connect.** `UtpSocket::connect` to its peer port, BEP 3 handshake
  accepted, `interested`, wait for `unchoke`, request every block. Result: **byte-exact 16 MiB over
  uTP**; Transmission lists the peer with flag `T` (uTP); 176-226 MB/s once unchoked (Transmission
  itself takes ~10 s to unchoke, its rechoke period).
* **B. We seed (UDP only, no TCP listener), Transmission leeches.** A tiny HTTP tracker in the
  example returns our LAN address (Transmission discards loopback peers from trackers);
  Transmission dials us over uTP (libutp initiator, our acceptor path), downloads the whole torrent
  and `cmp` confirms it is byte-identical. Transmission's peer view shows flag `T`.

Findings that changed the implementation: the delayed-ack stall above, and the initiator needing to
send a STATE right after the SYN-ACK. Transmission's encryption setting must be 0 (plain) for B because
the example speaks no MSE.

## Dependencies

* `tokio` (features `net`, `time`, `sync`, `rt`, `macros`; `default-features = false`): UDP socket,
  the timer sleep, `Notify`/`mpsc`, task spawn, `select!`. Needed for the driver; the sans-I/O core
  uses nothing but `std`.
* `libc` (already in the build through tokio/mio): one `setsockopt` for the DF bit, in `sockopt.rs`, the
  only `unsafe` in the crate.
* Nothing else. No `rand` (a 20-line splitmix generator seeded from `RandomState` is enough for
  connection ids), no `bytes` (a `Vec` with a head offset gives contiguous slices for packetizing),
  no `socket2` (default OS UDP buffers are sufficient so far, see gaps).

## Diagnostics: `TRS_UTP_TRACE`

`TRS_UTP_TRACE=1` (any value but `0`, read when a socket is created) makes the library print to stderr, stamped
with wall-clock milliseconds so it lines up with a `netstat -I lo0 -b` timeline (`bench/scripts/lo0-timeline.py`):
inbound SYNs, retransmission timeouts (with the full state line), fast-retransmit losses, path-MTU probe results,
zero-window periods and persist probes, FIN / app drop / lingering / close, plus every 500 ms a state line
(cwnd, in flight, queued, peer window, RTO, SRTT, queuing delay, datagram size, recovery, whether it wants to
transmit and is queued) for each connection with data pending and one line for the endpoint and driver.
Off by default; the cost when off is one relaxed atomic load at each (rare) event. `bench/scripts/utp-stall-loop.sh`
runs the hot bench in a bounded loop with it on and keeps the trace of the first slow round.

## CPU on loopback (what was measured, what was rejected)

Seeding three Transmission leechers over uTP at 200 MB/s, `sample` of the seeder: the driver thread spends 71 % of
its samples inside `sendto` (kernel UDP loopback path, about 4.6 us for a 4000 B datagram), 7 % in `recvfrom`, the
rest in the state machine; the peer threads' RC4 `try_write` is the other visible user-space cost. In the in-process
`loopback_bench` (1400 B payload, 3 streams, 512 MiB, user+sys of both ends) system time is 87 % of the total.

* Per-packet changes made: the retransmission scan over the whole flight is skipped unless a packet is marked for
  retransmission (`pending_tx`); the endpoint writes each datagram straight into its slot of the send batch (one
  4 KB copy less per datagram). `loopback_bench` user time 0.20 s -> 0.17 s, system time unchanged (1.15 s).
* `sendmsg_x` (macOS batching syscall, private SPI, no entitlement needed) was measured with a stand-alone
  sender: 5.9 us per 4000 B datagram with `sendto`, 5.5 us per datagram at batch 8-32, i.e. 7 % at best, because
  the kernel's per-datagram loopback work dominates the syscall entry. Not worth a private API; not adopted.
  Batching `recvmsg_x` would save less (receive is 7 % of the driver).
* The remaining lever is fewer, larger datagrams. 4000 B is the ceiling because Transmission reads UDP into a
  4096 B buffer.

## Fuzzing

`fuzz/` (cargo-fuzz, nightly): `packet_parse` (parse, round-trip, SACK iteration) and
`endpoint_datagrams` (arbitrary datagram/timer/app-call sequences through an `Endpoint`).
Smoke runs: 1.86 M and 1.31 M executions with no findings. `packet::tests::exhaustive_garbage_never_panics`
and `wire::random_garbage_and_mutations_never_panic` give the same coverage under plain `cargo test`.

## Gaps and notes for the engine

* Path-MTU discovery exists (see above) but does not learn interface MTUs: non-loopback paths are
  capped at `max_payload + 54`, so jumbo-frame LANs are not exploited.
* No pacing: a window-opening ack releases a burst. Fine on loopback and typical links.
* Engine integration choices: `max_per_ip` is raised from 32 to 256 there (a seeder of hundreds of
  torrents has one connection per torrent from the same remote client). Transmission and libtorrent
  prefer *encrypted* connections (MSE, BEP 8 style) and try uTP first; the engine has no MSE yet, so those
  handshakes fail and the peer falls back to TCP after a back-off. See the engine README.
* Dual-stack sockets report IPv4 peers as v4-mapped IPv6 addresses; use one socket per family.
* LEDBAT late-comer advantage and the fixed 100 ms target are not tuned; the engine's own rate
  limiter should sit above the stream (back-pressure through the receive window works).
* Only macOS was tested. SO_RCVBUF/SO_SNDBUF are left at OS defaults because tokio's `UdpSocket` cannot
  set them and `socket2` was not worth a dependency yet; 200 simultaneous slow starts on loopback lose
  about 6 % of datagrams to the default receive buffer. A pre-configured socket can be passed to
  `UtpSocket::from_udp`.
* No MSE/PE: encryption would wrap the stream above this crate.
