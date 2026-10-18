# trs-dht

Mainline DHT (BEP 5) for Seedwell, with BEP 42 secure node ids and BEP 43 read-only mode.
A **sans-I/O core** (`DhtCore`) does all the protocol work deterministically; a **thin tokio UDP
driver** (`spawn`, `DhtHandle`) wires it to a socket and one timer. IPv4 only for now (see
"Not done"). `#![forbid(unsafe_code)]`.

## Private torrents never touch the DHT (SPEC 7B.4)

The only way to ask the DHT for a torrent is a `PublicInfoHash`, and its only constructor takes
the torrent's `private` flag:

```rust
let Some(h) = PublicInfoHash::new(info_hash, torrent.private) else { return /* private: skip */ };
```

A private torrent cannot be expressed as a `PublicInfoHash`, so no code path can announce or look
it up. Belt and braces: `DhtCore::forbid(hash)` / `DhtHandle::forbid(hash)` makes the DHT refuse
a hash even if it was wrapped (`Refused::Forbidden`), and also refuses incoming `announce_peer`
for it. A private hash is never sent in any datagram, as `get_peers`, `announce_peer` or anything else.

## API

Driver (what the engine uses):

```rust
let dht = trs_dht::spawn(DriverConfig { saved_state, ..Default::default() }).await?; // DhtHandle: Clone
let mut s = dht.get_peers(PublicInfoHash::new(ih, false).unwrap()).await?  // io::Result<Result<PeerStream, Refused>>
    .map_err(|why| /* Cooldown{retry_after} | InProgress | Busy | Forbidden */ why)?;
while let Some(u) = s.next().await {            // LookupUpdate::Peers(Vec<SocketAddrV4>) | Finished(LookupStats)
    /* feed peers to the torrent */              // the stream ends after Finished
}
dht.announce(hash, listen_port).await?;         // lookup + announce_peer to the 8 closest; also yields Peers;
                                                // ends with LookupUpdate::Announced { accepted, attempted }
dht.save_state().await?;                        // Vec<u8>, ~5 KiB max, store in redb `dht_table`
dht.set_read_only(true).await?;                 // BEP 43: no inbound traffic while no torrent needs the DHT
dht.info().await?;                              // node id, external IP, table size, Stats
dht.shutdown().await?;                          // returns the final state blob
```

Shared port (what the engine uses next to uTP): the socket is owned by someone else, the driver only
sees the datagrams that are not theirs.

```rust
let io = SharedIo { incoming: rx /* mpsc::Receiver<(SocketAddrV4, Box<[u8]>)> */,
                    send: Arc::new(|to, data| { /* best effort, never blocks */ }),
                    local: socket_addr };
let dht = trs_dht::spawn_shared(DriverConfig { saved_state, ..Default::default() }, io);   // DhtHandle, no bind
```

Sans-I/O core (what tests, simulations and the fuzzers drive):

```rust
let mut core = DhtCore::new(Config::default(), Instant::now());
let intents: Vec<Intent> = core.handle(Input::Packet { from, data }, now);   // or Tick / GetPeers / Announce / Bootstrap
// Intent::Send { to, data }  -> write the datagram
// Intent::Event(Event::{Peers, LookupFinished, Announced, Refused, NeedBootstrap, ExternalIp, NodeIdChanged})
let next: Option<Instant> = core.next_wakeup();   // None = fully idle, wake only for packets/commands
core.save_state(); core.load_state(&blob, now)?;  // compact persistence
```

`NeedBootstrap` is emitted when the table has fewer than 2 usable contacts (exponential back-off,
15 s up to 1 h). The driver answers it by resolving `router.bittorrent.com:6881`,
`dht.transmissionbt.com:6881`, `router.utorrent.com:6881` and `dht.libtorrent.org:25401` (one task
per host, IPv4 only) and feeding the addresses back. With persisted state the core instead seeds a
self-lookup from the saved nodes and never touches the routers unless that fails.

## Idle behaviour

With no commands, the core does exactly three things:

1. answers incoming queries (rate-limited, see below);
2. refreshes a bucket only after it has been unchanged for 15 minutes (BEP 5): a 3-query
   `find_node` chain per stale bucket, at most 3 in parallel, at most one maintenance pass per 30 s.
   Any packet from a node in the bucket counts as a change, so on a reachable node most buckets
   never go stale;
3. retries bootstrap with exponential back-off when the table is empty.

There are no periodic pings, no timers for token rotation (done lazily on the next packet) and no
timer for peer-store expiry (swept lazily). `next_wakeup()` returns `None` when nothing is pending
and no bucket is nonempty. The driver arms *one* sleep from `next_wakeup()` and none when it is `None`.

Simulation (2,000 nodes, isolated, no incoming traffic from outside the sim): 2.5 queries per
node per minute during an idle hour (about 150/h plus the same number of replies). For true
idle (no torrent wants the DHT) call `set_read_only(true)`: the node stops answering and tells
others to drop it, so inbound traffic fades out in hours.

Per-hash cooldowns stop callers from spamming: `get_peers` 60 s, `announce` 10 min
(`Config::get_peers_cooldown`, `announce_cooldown`). A request while a lookup for that hash is
queued or running is `Refused::InProgress` (an `announce` instead *upgrades* the running plain lookup).
At most 4 public lookups run at once; 64 more wait in a FIFO (`Busy` beyond that). Cooldown records
are capped at 1,024 hashes.

## Network citizenship

| Limit | Default | Why |
|---|---|---|
| Outgoing queries, global | burst 50, 10/s sustained | all queries, retransmits and announces go through one bucket |
| Background work (probes, refresh, bucket fill) | only while at least half of the burst is free | user-initiated lookups win |
| Query timeout / retransmits | 2 s / 1, only to contacts we already verified | contacts that only appeared in someone's `nodes` list are dead about half the time (measured), so no retry |
| Lookup caps | α=3, 128 queries, 45 s, 200 peers | stops runaway lookups |
| Responses per remote IP | burst 10, 5/s | amplification damping (a get_peers answer is ~9x its query) |
| Responses overall | burst 100, 100/s | caps what this node can reflect |
| Stored announces | 30 min TTL, 100/hash, 256 hashes, 4,096 total | bounded; evicts the oldest entry of a full hash, refuses new hashes at the caps |
| Tokens | 8-byte SipHash-2-4 of the IP, secret rotated every 5 min, previous accepted | stateless |

Other hygiene: one routing-table node per IP; nodes enter the table only after answering a query
(a spoofed source cannot get in); unsolicited queriers are verified with a ping first, only when
the bucket could use them; responses must come from the address and carry the id we queried;
private/loopback/CGNAT/multicast addresses are dropped (`allow_local_addrs` for tests);
`ro=1` queriers are never added; unknown methods get error 204, malformed queries 203, garbage is
dropped silently. The external IP is accepted only when 4 reporters from distinct /16 networks agree,
and then the node id is regenerated per BEP 42 (the table is re-keyed, `Event::NodeIdChanged`).
Incoming nodes are *not* required to have BEP 42 ids by default (`enforce_node_id`), because a good
part of the network does not.

## Memory

All numbers from tests in this crate (`cargo test -p trs-dht -- --nocapture`):

| Item | Size |
|---|---|
| `Node` (id, last_seen, addr, fails) | 32 bytes |
| Realistic full table (208 nodes, 26 buckets) | 8.8 KiB |
| Theoretical ceiling (145 full buckets, needs ~2^145 nodes) | 52 KiB (< 64 KiB; hard cap of 160 buckets) |
| Per-IP response limiter | 6 KiB fixed |
| Peer store at its caps (4,096 peers) | 64 KiB |
| Lookup candidates | 64 x 64 B = 4 KiB per running lookup (+ dedupe set, max 200 peers) |
| `DhtCore::memory_bytes()` of an idle node in the 2,000-node simulation (67 contacts) | 13.2 KiB avg, 13.8 KiB max |
| Persisted state | 27 bytes + 26 per node, max 200 nodes = 5.2 KiB |

## Persistence format

`[version=1][own id 20][external ip 4][count u16 BE][count x (id 20, ipv4 4, port u16 BE)]`.
Nodes are best first (freshest `last_seen`). On load the saved id is adopted (an unspecified IP is
"unknown"), and the saved nodes seed a bootstrap self-lookup; they enter the table only after
they answer. Wrong version, wrong length or more than 200 nodes is `StateError::Invalid`.

## Testing

* `cargo test -p trs-dht` runs 95 tests (+1 ignored): BEP 5 byte-exact vectors for every message and BEP 42
  vectors (unit tests), proptests (routing-table invariants, `closest()` equals brute force, codec vs
  `trs-bencode` in both directions), core behaviour with hand-driven clocks (27 tests), a
  deterministic virtual-time simulation (2,000 nodes, loss, dead nodes, idle hour, transcript hash
  equality across runs) and the tokio driver over loopback UDP.
* Real network, `#[ignore]`: `cargo test -p trs-dht --test real_world -- --ignored --nocapture` joins
  the real DHT and does a single `get_peers` for the current Debian netinst amd64 info hash (taken
  from the official `.torrent`, SHA-1 of the original `info` bytes). No `announce_peer`.
  `cargo run -p trs-dht --example lookup -- <hex>` does the same for any hash.
* Fuzz: `fuzz/` (own `[workspace]`) has `krpc_parse` (parser) and `core_packets` (whole state
  machine, with well-formed fuzz-derived responses to its own queries). `cargo +nightly fuzz run
  krpc_parse fuzz/seeds`; seeds are the BEP 5 examples.

The simulation (`tests/sim/mod.rs`) is a reusable harness: nodes are plain `DhtCore`s, delivery is
a deterministic event queue with 20-200 ms latency, optional loss and dead nodes.

## Dependencies

* `tokio` (`net`, `time`, `sync`, `rt`, `macros`; default features off): UDP socket, one timer, channels.
  Only `driver.rs` uses it; the core compiles without any I/O.
* dev: `proptest` (properties), `sha1` (real-world test derives the info hash), `trs-bencode`
  (interop tests).
* No `rand`: a 40-line xoshiro256** (`rng.rs`) keeps the core reproducible; production seeds it from
  the OS-seeded `RandomState`. No `siphash`/crc crates: SipHash-2-4 (reference vectors tested) and
  CRC-32C (check value tested) are ~40 lines each.
* The codec (`codec.rs`) does not use `trs-bencode`: KRPC needs allocation-free, tolerant (unknown
  keys, unsorted dicts, trailing bytes) parsing of tiny flat dicts from hostile UDP, and a writer
  that emits sorted keys in a fixed order. Interop is enforced by tests instead.

## Not done / limits

* IPv6 (BEP 32) is not implemented; `nodes6`, `want` and IPv6 `values` are ignored. All types use
  `SocketAddrV4`. TODO: it is not a small patch. It needs a second routing table (a v6 node id
  derived per BEP 42 from the v6 address), `SocketAddr` instead of `SocketAddrV4` in the codec,
  lookup, store and event types, `want`/`nodes6`/v6 `values` encode and decode, a second UDP
  socket in the driver, and a dual-stack simulation to test the table separation. It was left out
  rather than half-tested.
* BEP 33 (scrape), BEP 44 (storage) and `sample_infohashes` (BEP 51) are not implemented; the
  methods are answered with error 204.
* No port mapping in this crate (see `trs-portmap`): whether the node is reachable from outside is the engine's business. A node
  behind NAT with no mapping still works as a client but receives no queries.
* After a bootstrap the sparse far buckets get up to 3 rounds of filling lookups (≤12 queries each);
  that is the only unsolicited traffic besides stale refreshes.
