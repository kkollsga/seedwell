# trs-engine — design

How the crates combine into a session that stays fast and small with hundreds of active torrents. Read with `SPEC.md` §2, §5, §5A, §6, §7, §7B–§7E and `docs/ENGINEERING.md`.

## 1. Runtime and threads
| Thread group | Count | Work |
|---|---|---|
| tokio multi-thread runtime | `min(4, perf cores)` | networking, timers, actors |
| storage I/O pool (trs-storage) | 2–4 | `pread`/`pwrite`, fsync, moves |
| hashing pool (trs-storage) | perf cores − 1 | SHA-1/SHA-256 verification |
| persistence | 1 (inside the runtime via `spawn_blocking`-free channel to a writer thread) | redb transactions, debounced |

Nothing blocks a runtime worker. Timers go through one hierarchical wheel per session. Many torrents share wake-ups, and idle wake-ups average ≤ 1 per second (SPEC §2).

## 2. Ownership: actors with small messages
```
Session (actor)
 ├── TorrentTable: HashMap<InfoHash, TorrentSlot>
 ├── Listeners (TCP, and uTP on the same port number's UDP socket, which the DHT shares) ── incoming handshake → lookup → promote tier → hand socket to torrent
 ├── TrackerDriver (trs-tracker HostGate + transports + AnnounceMachines for ALL torrents)
 ├── RateLimiter (global ↓/↑ token buckets, priority weights 4:2:1, alt-speed schedule)
 ├── ConnectionBudget (global peer cap, per-torrent caps, half-open cap)
 ├── QueueManager (max active downloads; torrents + direct downloads)
 ├── Rules + TrackerProfiles (match on add / on edit; §7, §7E)
 ├── Stats (per tracker profile daily buckets; §7E.2)
 ├── Persistence (redb writer, debounced)
 └── EventBus → UI / agents (finished, error, state changes)
```
- `TorrentSlot` is an enum of power tiers (SPEC §5). **Only Hot torrents have a task.** Cold and Warm torrents are plain data owned by the session, and the session drives them.
- **Peer connections** are tokio tasks. They talk to their torrent through bounded channels carrying small fixed-size messages. Piece data goes peer task → storage directly (via a `Bytes` buffer from the pool), never through the torrent actor.

## 3. Power tiers in memory (targets)
| Tier | Resident | Target size |
|---|---|---|
| Cold | info-hash, name (`Box<str>`), save-root id (interned), sizes, counters, flags, tracker-profile id, rule id, private flag | **≤ 256 B** + name |
| Warm | + `Layout` from trs-storage (rebuilt from the stored `.torrent`), small peer set (≤ 8) | ≤ 4 KiB + 32 B/file |
| Hot | + piece hashes, bitfield, availability (`u16`/piece), picker state, full peer set | proportional to pieces/peers |

Tracker state for every torrent lives in trs-tracker's machines. It must stay compact (≤ 128 B per (torrent, tracker)) so 200 Cold torrents announcing cost about 50 KiB in total.

Transitions:
- **Cold → Warm** on an incoming handshake for the info-hash, or on a DHT/PEX/tracker peer when the torrent wants peers.
- **Warm → Hot** when downloading, when there are ≥ 2 interested peers, or when uploading > 0 for 60 s.
- **Hot → Warm → Cold** after N minutes (default 10) of no interest. Buffers, hashes and layout are dropped.

## 4. Peer connections
- Per idle peer: the protocol state from trs-wire (≤ 2 KiB) and **no read buffer**. A buffer is leased from the session pool while bytes are flowing and returned when the connection goes idle for 5 s; the write buffer and the command inbox are released then too. Measured (`crates/engine/tests/scale.rs`): about 2.0 KB of engine heap per idle peer, plus about 0.7 KB of tokio's registration of the two socket ends in a loopback test.
- Encryption (MSE): an incoming connection whose first bytes are not "BitTorrent protocol" is answered as an encrypted handshake (policy `Prefer`, the default: plain after it when the peer allows, so the stream costs what it did; `Require`: RC4 both ways; `Off`: refused). Outgoing connections offer it first and redial plain for peers that do not answer.
- Peer transports: TCP and uTP (BEP 29, `trs-utp`) share one code path (`net::PeerIo`). Outgoing connections try TCP, or uTP first for peers that are known to speak it (they connected to us over uTP, `ut_pex` flagged them, or a uTP dial worked before); the other transport is the fallback. Both count against the same connection budget.
- Writes are vectored. Piece payloads are sent from storage read buffers without copying.
- Serving: we advertise `reqq` 1000 and close a peer only beyond twice that. The advertised value is the real ceiling of a leecher's speed: Transmission keeps at most `reqq` requests outstanding per connection and re-fills them about twice a second, so `reqq` x 16 KiB / 0.5 s is the most one connection can fetch (250 meant 8 MB/s per connection, which held the 3-leecher hot bench at ~122 MB/s with the seeder 90 % idle). A queued request costs 24 bytes; storage reads for it start only while less than 1 MiB of earlier reads is in flight or waiting for the socket (`READ_AHEAD`), so a deep queue does not pin 16 MiB per peer. The request queue and the read list are released with the other idle buffers.
- Request pipelining: queue depth = clamp(rate × RTT / 16 KiB, 4, peer's `reqq` or 250).
- Timeouts:
  - handshake 10 s;
  - an idle connection is dropped after 2 min if neither side is interested;
  - snubbed after 60 s without a block.

## 5. Picker and choker
- Rarest first on availability counts. **Partial pieces first**, then rarest, with random tie-break. Blocks are sequential within a piece. Endgame begins when all blocks are requested: duplicate requests, cancel on receipt.
- Files that aren't wanted get priority 0. Priority is per-file and per-torrent (High / Normal / Low from rules).
- **Choker:**
  - Downloading: tit-for-tat on received rate, plus 1 optimistic unchoke rotating every 30 s.
  - Seeding: unchoke the peers we upload to fastest, rotating every 30 s for fairness.
  - Upload slots are sized automatically from the upload limit.
  - **Demand-weighted across torrents** (SPEC §5.1): upload slots and bandwidth go first to torrents with the most interested peers, weighted by rule priority.

## 6. Correct stats (SPEC §7B.2)
- `uploaded`/`downloaded` count payload bytes only: piece message payloads.
- `downloaded` is the size of each piece that passed its hash check (Transmission's rule, verified against its source); hash-failed bytes go to `corrupt=` instead (HTTP, only when non-zero); `uploaded` is piece payload written to the socket. One place each: `piece_verified`, `piece_failed`, `credit_written`.
- `left` = bytes of the whole torrent not yet verified. Data for unwanted files still counts as left, matching reference clients; confirm against the captures.
- Session counters reset on each new `started`, as the tracker expects. Lifetime counters (for ratio and stats) are separate and persisted.

## 7. Persistence (`~/Library/Application Support/Seedwell/`)
- `state.redb` tables:
  - `torrents` (id → compact record)
  - `resume` (bitfield + partial-piece map, incomplete only)
  - `counters`
  - `tracker_timing` (implements trs-tracker `TimingStore`)
  - `stats_daily` (profile, day) → bucket
  - `peers_cache` (bounded)
  - `dht_table`
- `torrents/<infohash>.torrent`: the original bytes.
- `config.toml`, `rules.toml`, `trackers.toml`: human-editable, hot-reloaded, validated, with a backup of the previous version on every change.
- Writes are debounced (2 s) and batched into one transaction. Schema versioning and migrations come with a backup first (SPEC §5A).
- **Renaming a file a running torrent may write** (edge files of torrents that earlier versions placed file by file, ticked later or hidden; SPEC §7A.3): never "rename, persist, then tell storage". The actor hands the rename to storage together with the new layout (`switch_layout_renaming`): under a per-torrent gate that every file `open` takes shared, the files are renamed and the layout naming the new paths is installed, and opens resolve their path from the layout installed at that moment. Only after storage confirmed does the session commit the placement. A crash in between leaves the record naming one path and the data at its sibling (`.seedwell-partial/<name>` or the clean name; `.dispatch-partial/` from older builds is recognised too); `edgefix::reconcile` renames it back at the next start. If both names exist, the torrent is rechecked against both and the one with more verifying pieces keeps the recorded path; the other is kept as `<name>.stale`. Moves hold a running, incomplete torrent's writes (`quiesce_writes`) from before the first rename until the layout switch, and finishing from `.incomplete` holds them while the move runs.

## 8. Control API (shared by FFI and the CLI)
- `Command` / `Query` enums with serde. Responses are typed records.
- In-process: trs-ffi calls the API directly. Out-of-process: the `seedwell` CLI connects to `control.sock` (JSON lines, versioned).
- Permission levels are enforced at the socket boundary (SPEC §9A.3). The FFI/UI is trusted.
- Everything the UI shows comes from `Query::Snapshot` (cheap, ~1 Hz while visible) and the event bus.

## 9. Budgets to test continuously
- A `bench/` harness runs the engine with 500 synthetic torrents over 50 local peers (mock tracker + local seeders). It asserts the memory targets above and the idle wake-up rate.
- Real-world downloads (Linux distro torrents) are compared against Transmission and qBittorrent: time to complete, peak and steady memory, CPU time. Results go to `bench/reports/`.
