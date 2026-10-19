# trs-engine

The BitTorrent session engine of Seedwell: torrents, peers, picker, choker, rate limits, queue,
power tiers, tracker/DHT/PEX/magnet integration, a persistence boundary and the control API.
Design: `docs/ENGINE.md`. `#![forbid(unsafe_code)]`.

```
EngineHandle ──Cmd──▶ Session (1 task) ──TrackerCmd──▶ tracker_driver (1 task, TrackerHub)
   ▲ events/snapshot     │  table: id → ColdTorrent (+ Live)        ▲ TrackerPeers
   │                     │  DHT lookups, queue, persistence, goals  │
   │                     ├──TorrentMsg──▶ Torrent actor (Warm/Hot) ──┘  one task per awake torrent
   │                     │                  picker, choker, PEX, metadata, resume
   │                     └─ TCP listener     ▲▼ small commands / events (own bounded queue)
   └────────────────────────────────────  Peer task (per connection): framing, storage I/O, limits
```

## Modules

| Module | Role |
|---|---|
| `api` | Public data: `TorrentRow`, `Snapshot`, `TorrentDetail`, `EngineEvent`, options, errors |
| `config` | `EngineConfig` (updatable at runtime), `TierTimings`, dev-build detection |
| `persist` | `Store` trait, records, `Batch`, `MemStore`, the writer thread |
| `state_store` | `StateStore`: the `Store` over `trs-state` (redb + `torrents/`, `placement/`, `downloads/`) |
| `host` | `Host`: the whole stack from a state directory (settings files, hot reload, engine) |
| `config_map` | `trs_state::Config` to `EngineConfig`, and which settings need a restart |
| `policy` | `PolicyData` (rules + tracker profiles), the id table, `resolve` |
| `organize` | Content detection, location planning (flat), Trash/archive chores |
| `status` / `session/status.rs` | Organize by seed status: the four folders and the hourly check (SPEC 7A.3) |
| `watch` | Watch folders: one task, one `stat` per folder per 2 s; baseline + backlog |
| `choice` | Smart add (SPEC 7A.6): structure, preselection, library presence, reasons to ask |
| `moveplan`, `session/moves` | Moving files after a location changed (SPEC 7A.5): planning, queue, exec task, crash recovery |
| `portmap` | `trs-portmap` runner for the listen port |
| `session` | The session actor; `cold.rs` is the Cold-tier record (196 B); `add`, `policy_apply`, `settings`, `direct` are its parts |
| `torrent` | The torrent actor: `startup` (load, resume check, recheck), `events`, `maintenance`, `magnet` (BEP 9), `pex` (BEP 11) |
| `peer` | One connection: handshake, BEP 6/10 messages, direct storage reads/writes, buffer pool |
| `net` | `PeerIo` (TCP or uTP behind one type), `Dial` (how an outgoing connection is tried), the set of peers known to speak uTP |
| `picker`, `choker`, `queue`, `tier`, `ratelimit`, `budget`, `counters` | Pure logic with unit tests |
| `tracker_driver` | One task for every torrent's trackers, exactly per the `trs-tracker` README |
| `chan` | Small bounded queue: an idle inbox owns no heap (tokio's mpsc allocates 64 slots eagerly), plain wakers instead of `Notify`, pollable by hand |

## Deviations from ENGINE.md

* **Warm torrents have a task** (ENGINE.md 2 says only Hot ones do): a Warm torrent owns live
  sockets. What differs between the tiers is residency. A complete torrent's Hot state is a label
  that raises the peer cap; it holds the same data as Warm because a seeder needs no hashes or
  bitfield. An incomplete torrent is always Hot.
* **Partial pieces are not persisted.** `trs-storage` keeps unverified blocks in memory, so resume
  data is the verified bitfield only.
* The write path copies a block into the encoder instead of a vectored write.

## uTP and the shared UDP port

`network.utp` (`EngineConfig::utp`, default on) binds one UDP socket on the listen port's number; it is a
`trs_utp::UtpSocket`, so it speaks uTP (BEP 29, LEDBAT: seeding yields to the user's other traffic) and hands every
datagram that is not uTP to the DHT (`trs_dht::spawn_shared`, driven from the socket's foreign-datagram channel, answers go out of
the same socket). Port mapping already maps TCP and UDP for that port. The DHT keeps its own socket only when `dht_bind` names
another port, or when the UDP port could not be bound (then the engine runs TCP-only and says so on stderr).

* **Incoming** uTP connections go through the same path as TCP ones: connection budget, handshake deadline, `PeerIo::Utp`.
  A handshake that is not BitTorrent (an encrypted one) is answered with a RESET on uTP and a FIN after draining on TCP.
  `trs-utp`'s per-IP cap is raised to 256: a seeder of hundreds of torrents has one connection per torrent from a remote client.
* **Outgoing** (`net::Dial`, libtorrent-style): a peer that is known to speak uTP (it connected to us over uTP, `ut_pex` flagged it
  `0x04`, or a uTP dial worked before; at most 2,048 remembered) is dialled uTP-first with a 5 s probe, TCP as the fallback; any other
  peer TCP-first, uTP as the fallback when TCP does not connect. `EngineConfig::outgoing_tcp = false` makes every dial uTP (uTP-only
  swarms, tests). Our own `ut_pex` messages carry the flag for uTP peers.
* `PeerInfo::utp`, `Diagnostics::{utp_accepted, utp_dialed}` show what runs over uTP. Private torrents may use uTP (it is transport,
  not discovery); the DHT guarantees of `trs-dht` are unchanged.
* **Not shared: the UDP tracker transport.** `trs-tracker`'s `UdpTransport` binds its own ephemeral sockets and matches replies by
  transaction id. Nothing needs a well-known port there: the reply returns to the port the request left from, through the NAT mapping
  that request created. Sharing would save one socket and one task at the price of changing the tracker crate's transport API.
* **Encryption (MSE)** is implemented (`network.encryption`, `EncryptionPolicy`): Transmission, qBittorrent and most clients prefer encrypted
  connections and try them first, over uTP first; against an engine that answers only plain handshakes every first contact failed
  and fell back to TCP after a back-off of 20 s or more. `trs_wire::mse` has the primitives (768-bit Diffie-Hellman with a Montgomery
  multiplier, RC4 with the 1,024-byte discard); `mse.rs` runs the handshake on a socket, as responder and as initiator; `net::Rc4Io`
  puts RC4 on a TCP or uTP stream. `Prefer` (the default) answers an encrypted handshake, selects *plain* after it when the peer offers
  it (the stream is then as cheap as before) and dials encrypted first with a plain redial for peers that do not answer
  (remembered, 2,048 addresses); `Require` offers and selects RC4 only both ways and refuses plain handshakes; `Off` refuses encrypted
  handshakes. Concurrent handshakes are limited to two (each costs two modular exponentiations), the initial payload must be exactly a
  handshake and every length is bounded. `Diagnostics::{mse_accepted, mse_dialed, mse_fallbacks}` count them.

## Seeding performance (the "hot" benchmark)

Root causes of the failed hot campaign (`bench/scripts/tests/repro-seeder-reconnect.py`), all covered by `tests/seeding.rs`:

1. **A stopped torrent refuses handshakes.** The default rule is "ratio 1.0 or 7 days, then stop"; the first Transmission leecher took
   a torrent once, the goal was reached and the torrent was stopped (the tracker saw `stopped` 1.2 s after the leecher's `completed`),
   so the next leecher process was never answered. The engine is right; the benchmark seeder must not have a goal.
   `bench_client` writes `rules.toml` with the goal `forever` when the state directory has none.
2. **The first unchoke waited for the next rechoke (up to 10 s).** A torrent without interested peers holds 0 upload slots after its
   rechoke, and a new leecher was unchoked at once only `if unchoked < slots`. `refresh_slots` now asks the allocator on the first
   `interested` (and, for the second and third peer arriving together, again at once; later at most four times a second).
3. **One thread hand-off per 16 KiB block.** Requests that arrive in one batch are now grouped into runs of up to 128 KiB (consecutive
   blocks of one piece) read with one `pread`, finished reads are pumped into the write buffer in one go, and the write buffer keeps
   its allocation while the connection is active (it was freed after every drain).
4. A tracker-supplied address that is our own was dialled again and again ("connected to ourselves"); it is forgotten now.
5. A peer that sent a partial message and then stalled kept the peer task spinning (the 5 s buffer-release deadline stayed in the
   past and re-armed the timer to "now" on every pass) until the 240 s idle timeout. The deadline only counts when it can be acted on.
6. **"32 connections" while seeding.** A complete torrent kept dialling every address a tracker or the DHT handed it; most are
   seeds, NAT'd leechers or dead hosts, each holding a connection slot and a task for the connect and handshake timeouts, and the
   half-open cap (32) was always full. A seeding torrent now dials from an allowance (a burst of four, one more every 30 s, at most
   two connecting at once). `Snapshot::connections` / `Totals::connections` count connections that completed the handshake;
   `connecting` / `half_open` show the dials in flight (they hold a budget slot but are not connected).

7. **No MSE: the first contact of every default Transmission/qBittorrent leecher failed** (encrypted handshake first, then a plain
   redial after a back-off; over uTP 20+ s of fallback, and now and then one torrent of a leecher process that did not connect for
   minutes: the "no leecher progress for 60 s" rounds). See "Encryption" above.

Transmission itself takes about 10 s after connecting to say `interested`; that is not the seeder's doing.

Measured, same machine and hour (18:36 to 18:59), three Transmission 4.1.3 leecher processes, 10 torrents of 20 MB, N=200, 10 rounds
(`bench.py hot`, campaigns `final4-hot` and `final4-hot-tcp`; the load average was 4 to 10, other agents were compiling):

| Seeder | Upload mean | Upload peak (1 s) | CPU while serving | RSS peak while serving |
|---|---|---|---|---|
| Seedwell `bench_client`, defaults (uTP and encryption on; the leechers connect over uTP) | 97 MB/s | 132 MB/s | 93 % | 173 MB |
| Seedwell, TCP only (`BENCH_TRS_NO_UTP=1`) | 122 MB/s | 249 MB/s | 14 % | 108 MB |
| Transmission 4.1.3 | 152 MB/s | 482 MB/s | 38 % | 624 MB |
| qBittorrent 5.2.3 | 113 MB/s (45 to 113 in three runs) | 246 MB/s | 43 % | 265 MB |

The leechers are the limit on loopback (each Transmission process needs about 10 s to say `interested`, and the rounds are 5 to 9 s
long), so the means differ little; the peak shows the seeder. Before the fixes the run failed in round 1 or 2 (a stopped torrent),
with only the unchoke fix it served 48 MB/s, with the read coalescing 108 to 122 MB/s over TCP. **uTP is the expensive
transport here**: one `sendto` per 1,400-byte datagram is about 22 us of kernel time on loopback (`sample`: `sendto` 75 % of the active
time), so a seeder that Transmission reaches over uTP spends 93 % of a core for 97 MB/s. (Before this campaign's driver change the same
run reached 70 to 78 MB/s: the uTP driver sent 64 datagrams and read 64 datagrams *inside* the endpoint lock, which every stream
`read`/`write` takes; syscalls are outside the lock now.)

## Memory

* **Idle peer**: about 2.0 KB of engine heap (`tests/scale.rs`, slope between 14 and 56 peers, socket buffers and tokio's socket
  registrations excluded; 2.7 KB with them). What changed from 8.5 KB: the peer loop is a hand-written `poll_fn` instead of `select!`
  (no per-branch future inside the task), cold paths (`establish`, `on_piece`, `on_extended`, `on_readable`) are boxed, `Peer` lives
  on the heap, the spawn argument is boxed, `emit` completes synchronously unless the torrent's inbox is full, `PeerCmd` is 32 bytes,
  and after 5 s without traffic the read lease, the write buffer and the command queue give their allocations back. The task future
  is 248 bytes (was 2,040).
* **Idle torrent**: live heap is about 2.2 to 2.5 KiB per extra Cold torrent (`ColdTorrent` 216 B, `Entry` 224 B, one table slot 249 B at
  full load and up to twice that with slack, tracker hub 257 B per (torrent, tracker), counters 64 B, the rest small strings and the
  driver/hub tables). The footprint was several times that because of the allocator, and this is what was found (measured with
  `TRS_BENCH_DIAG`, `heap -addresses`, `vmmap`; macOS 26, fast tier timers, add scenario = the harness, restart scenario = the same state
  directory started again, so the torrents load from the store instead of being added):
  * Start-up churn is what the allocator keeps: 100 MB of the 130 MB allocated while 200 torrents start are 256 KiB hash scratch
    buffers, then 4 KiB redb page buffers (every add commits), then grown tables. Peak live heap is 3.9 MB, footprint was 8 to 12 MB.
  * **Kept: `trs_storage::MappedAlloc`** (global allocator of `trs-ffi` and the `seedwell` daemon): blocks of 128 KiB and more are
    an `mmap`, unmapped when freed. No new dependency. 3 MB less footprint at every N, and the footprint no longer depends on luck.
  * **Kept: follow-up trims.** `malloc_zone_pressure_relief` releases only memory that has been free for about 3 minutes (called
    every 10 s it released nothing for 150 s, then 3.4 of 10.9 MB). The session now trims 20 s after a burst and again 2.5, 5 and 7.5
    minutes later (`TRIM_FOLLOWUPS`); a call is one syscall off the actor.
  * Rejected, measured: mimalloc (19 to 26 MB footprint, C dependency); a second `malloc_create_zone` zone for blocks of 1 KiB and
    more, or for all blocks (no gain: 8.4 and 8.7 MB against 7.9 MB with mapping alone); running the commits on a short-lived thread
    (no change, so thread caches are not the cause); compaction (nothing to gain until relief can release the freed pages, which it
    does only after minutes; not implemented). Moving rare `ColdTorrent` fields behind a box would save about 40 B per slot (2 to 4 %): not done.
  * Results (footprint MB, fast tiers, 420 s; live heap MB in brackets). Add scenario: before (system allocator) 8.6 / 10.7 / 12.5
    typical (the 220 s runs; erratic 7 to 14 when relief happened to fire), after 5.6 / 6.8 / 9.5 (live 1.6 / 2.0 / 2.9), slope 8 to 9 KiB per
    torrent (7.1 KiB in the 220 s runs). Restart scenario: before 4.9 to 8.0 / 5.6 to 8.9 / 9.5, after 4.7 / 5.4 / about 6.8
    (live 1.5 / 1.85 / 2.5), slope 3.6 to 4.1 KiB. Default tiers, 13 min (`bench.py scale`): before 8.6 / 7.3 / 8.1, after 5.8 / 7.7 / 9.1.
    Wake (Cold to first block, 200 torrents): 1.5 to 5 ms before and after. Hot bench (3 leechers): 203 MB/s both, seeder RSS 111 MB to 78 MB.
    Idle CPU unchanged (0.02 / 0.2 / 0.35 % at N=50 / 200 / 500 with the fast tiers still demoting).
  * **4 KiB is met only after a restart (3.6 to 4.1 KiB). In a process that has added hundreds of torrents it is about 8 KiB**: each add
    commit leaves about 2.6 KiB of holes between the live records (4 KiB redb pages freed and refilled with small records). Reaching
    4 KiB there needs the long-lived per-torrent state to be rebuilt into fresh pages after the adds and every old copy dropped (so that
    whole pages free up for the delayed relief), or redb page buffers kept out of the shared heap; neither is cheap.
* **Idle CPU at N=500** (0.20 % against 0.08 %) was `bench_client`'s own once-a-second JSON line, built from a full `snapshot()`
  (a `TorrentRow` with ten strings per torrent plus a rule resolution). It uses `EngineHandle::totals()` now (one pass, no allocation):
  0.025 / 0.043 / 0.112 % over the 13-minute window for N=50 / 200 / 500 (0.016 / 0.024 / 0.100 % after the first two minutes; it was
  0.016 / 0.082 / 0.204 %), 0.01 to 0.3 wake-ups/s. What is left at N=500 is the minute tick (counters and statistics of every live
  torrent, one redb transaction) while the torrents are still Warm; it stops when they are Cold.

## Rules, profiles, categories (SPEC 7, 7E, 7A)

* The engine holds an `Arc<PolicyData>` (`rules.toml` + `trackers.toml`). `EngineHandle::apply_policy` publishes it for the
  adds that follow and makes the session re-evaluate every torrent; `Host` calls it on every reload.
* A torrent's tracker profile is matched from its announce URLs (`TrackerProfiles::match_announce`) when it is added, when
  its metadata arrives, at start-up and when `trackers.toml` changes (a background job re-reads the stored `.torrent`s).
  Cold keeps two small integers (interned profile id, interned forced-rule id), a category byte, an upload limit in KiB/s
  and the effective priority and goal. Classification results are not kept per torrent.
* `AddOptions` fields typed `Option` are overrides: `priority`, `seed_goal`, `category`, `rule`. A user choice is
  remembered (`G_PRIO_USER`, `G_GOAL_USER`, `G_CAT_USER`) and survives rule edits and restarts. A user goal keeps its
  kind (ratio / time / both); the hit-and-run floor and the action at goal always come from the rule.
* `SeedGoal` gained `stop_on_finish` ("stop when the download finishes"): the goal check runs the moment the torrent
  finishes, `completed` is queued first and `stopped` right behind it, and nothing is uploaded. If the tracker session had not
  even sent `started` yet (a tiny torrent), the tracker crate sends nothing at all, which is correct.
* Actions at goal: `AtGoal::{Stop, Remove (keep files), RemoveAndDelete}`.
* `EngineHandle::clear_seed_goal(ids)` undoes `set_seed_goal`: `G_GOAL_USER` is cleared and the rule's goal applies (and follows
  edits) again. `TorrentRow::seed_goal_user` says which one a row has, `TorrentRow::seeded_secs` is the total seeding time
  (all stretches), `SeedGoal::describe()` / `then_phrase()` and `ChoicePreview::summary()` are the words every front end shows.
* **Display title.** `ColdTorrent::title` / `TorrentRecord::title` hold the clean title ("Sintel (2010)") next to the category.
  It is computed by `trs_classify::torrent_title` (file-aware; a single-file torrent takes its file's name when that says more)
  when a torrent is planned (add, magnet metadata), again when its category changes (`titles.rs`: stored `.torrent` read off the
  actor), and once, a moment after start, for torrents stored without one. `TorrentRow::title` is empty until then;
  `TorrentRow::display_title()` falls back to the cleaned raw name.
* **Not enough space.** When a download starts (resume, re-add, start-up) and the pieces still wanted do not fit on the volume
  of the folder they are written to (staging when staged, else the save folder), the torrent faults with
  `Not enough space: X still to download, Y free` (row state `Error`, Resume tries again). `smart_add.free_space_override`
  is the test hook for the free space.
* Goals are checked for "finished" torrents (all pieces, or all wanted files when some are skipped). The next check is
  scheduled for the earliest moment something can be reached (end of a floor or time goal), at most a minute away.
* Statistics: `StatDelta` carries the profile id (`None` + `private` selects `@public` / `@other-private`), bytes,
  torrents added/completed/removed, wanted files and their bytes, and seed seconds. They are bucketed on the local day.

## Sharing groups (SPEC 8.3) and retry roles (SPEC 7C.4)

Every snapshot row of a finished, not stopped torrent has `seed_group`: Popular (>= 3 peers being uploaded to, or an upload
rate averaged over ~10 min above 250 KB/s while active in the last 10 min), Active (an upload or an interested peer in the last
24 h), Dormant, Error (an error younger than 24 h) or Dead (older, or the trackers all halted on a permanent reason such as
"unregistered"). Cold carries 12 bytes for it: `last_activity`, `error_since` (both unix seconds, persisted; the activity at
most every 10 minutes) and `avg_up`; everything else is computed. Errors are: files missing/unreadable or a disk fault
(`want == Error`), and *every* tracker of the torrent failing at the transport level (timeouts, DNS, 5xx, garbage). A tracker's
`warning message` or non-permanent `failure reason` is not an error. `error` carries the short reason ("Bad gateway",
"Rejected: “Unregistered torrent”", the disk text) and `next_retry_ms` the next announce attempt of a failing tracker.
`tracker_driver` aggregates the per-slot reports into `Health` and tells the session only when it changes; the session keeps a
map entry only for torrents that are failing right now. `Role` (Downloading / Seeding / SeedingDormant) is sent to the driver
whenever it changes (finish, group drift, checked every 10 minutes); the driver stores it and hands it to the hub, which
call `TrackerHub::set_role` (wired).

## Placement: flat, optional status folders (SPEC 7A.3)

`organize::plan` runs off the session actor (`spawn_blocking`) when a `.torrent` is added and when magnet metadata
arrives. It classifies with `trs-classify` (info-dict `source` as the tracker hint), decides the category in effect
(`Other` while unsure), resolves the rule and picks the finished location (rule `finished_dir` > category override >
app default). The category drives smart add, titles, icons and rules, **never a file name**: the torrent keeps its own
layout (`organize::source_path`: `<save>/<torrent name>/<path>`, or `<save>/<name>` for one file) inside that
location. Junk files get priority 0 when "skip junk" is on. Unfinished data is staged in `<incomplete_dir>/<info-hash>/`
(or `<save>/.incomplete` without `incomplete_dir`) and moved with trs-storage's journaled `finalize`; the finished
location never sees a half file. A name that is already taken by another file stops the finalize with "destination
already exists" and touches nothing.

* An explicit `save_path` is the torrent's finished location and **pins** it there.
* `Placed::Renamed` + `Store::load_placement` survive for torrents that earlier versions placed file by file (destination
  templates) and for a move that found a name taken: their recorded per-file paths are kept as they are. `Config` moves
  skip them. The `HardlinkPending` / `HardlinkDone` states are gone: old records read as `No`, and library links that
  were made stay as ordinary files.
* Removing a torrent with its files deletes its own folder; for a torrent with recorded per-file paths, those files and
  the empty folders above them, never above `Placement::library_root`.

### Organize by seed status

`EngineConfig::by_status` (`folders.by_status`). A new torrent that is not pinned gets `<finished>/Active` as its save
path (`Plan::save_path`; `Plan::finished_base` is the location itself). `session/status.rs` runs `Job::StatusCheck` every
`EngineConfig::status_interval` (one hour; tests shorten it): for every finished, not staged, unpinned torrent that is in
a status folder, the folder it belongs in is `StatusFolder::of(seed_group, stopped)` (`status.rs`: Popular and Active ->
`Active`, Dormant -> `Dormant`, paused or goal reached -> `Stopped`, Dead -> `Dead`, Error -> stay). It moves with
`MoveTarget::Status(folder)` (the ordinary `plan_move`, sibling folder, one volume = a rename) when two checks in a row
agree (Stopped and Dead at once) and `LiveStats::uploading` is 0. `MoveTarget::Config` (the settings changed) files an
unfiled finished torrent by its live state, keeps the folder of a filed one under a new base, and drops the folder when
the switch is off, so turning it on or off goes through the usual `LocationsChanged` -> `location_moves` ->
`move_torrents` confirmation; the hourly check never files an unfiled torrent behind the user's back.

## Locations (SPEC 7A.4)

Two locations per download: **incomplete** (staging, `<root>/<info-hash>/`) and **finished**. App defaults come from
`folders.incomplete_dir` / `folders.download_dir`; each category may override either one
(`EngineConfig::category_locations[Category::index()]`: `CategoryLocation { incomplete, finished: Option<PathBuf> }`,
built from `[folders.categories.<name>]`). The finished precedence is rule `finished_dir` > category override > the
download folder, all plain folders with the torrent's own layout below them. `Plan::stage_root` is decided at add time and **stored per torrent** (`ColdTorrent::stage_root`,
`TorrentRecord::stage_path`, `trs-state` record v2), so changing the incomplete setting never silently changes where a
running download's partial files are expected. Records from before keep following the configured folder.

`engine::same_volume`, `volume_id`, `cross_volume_pairs(&Config) -> Vec<CrossVolume>` (re-exports of `trs_state::volume`)
drive the Settings warning; they are a few `stat` calls, so the app can re-run them on every mount/unmount.

## Moving files (SPEC 7A.5)

API (`EngineHandle`):

```rust
async fn location_moves(&self) -> Result<MoveProposal, EngineError>;          // who sits elsewhere than the settings say (nothing moves)
async fn move_torrents(&self, ids: &[TorrentId]) -> Result<MoveReport, _>;     // queue: to where the settings say
async fn move_to(&self, ids: &[TorrentId], path: &Path) -> Result<MoveReport, _>;  // queue: below `path`, and pin
fn cancel_moves(&self, ids: &[TorrentId]);
```

Events: `LocationsChanged { torrents, bytes }` (a settings change left torrents out of place: ask "Move N torrents (X GB)?"),
`MoveStarted { id, from, to, bytes, cross_volume }`, `MoveProgress { id, done, total }` (about 1/s during a copy),
`MoveFinished { id, to }`, `MoveFailed { id, reason }`. `TorrentRow::moving: Option<f32>` is the progress for the grey
"Moving... 42%" line. Triggers: a settings change of a default/category location or of `by_status` (the event, then
the app calls `location_moves` + `move_torrents`), a **category change** of a torrent that is not pinned (queued at once,
fixing "re-applies the rule but does not move"), and `move_to`. A torrent the user placed (custom save path, `move_to`) is
**pinned** (`TorrentFlags::LOCATION_PINNED`): settings and category changes leave it alone.

How it works. A queued job is only `(torrent, target)`; its plan (`moveplan::plan_move`) is made when its turn comes, off the
actor, from the stored `.torrent` and placement (nothing is kept for Cold torrents, so hundreds of queued torrents cost a few
dozen bytes each). Up to 3 jobs plan/run at once, but **at most one cross-volume copy per destination volume**; others wait parked.

| Case | What happens |
|---|---|
| Seeding, same volume | `trs-storage::relocate_prepare` renames file by file (`renamex_np(RENAME_EXCL)`, directories first, never overwriting) on the **torrent's own storage handle**; reads that meet a file in transit wait for the switch (ms). Then the session persists the record and the actor switches its layout. |
| Seeding, other volume | A temporary handle copies with `F_NOCACHE`, 4 MiB buffers, hashing **every piece against the torrent's hashes while copying**, `fsync`, rate-limited (`advanced.move_limit_mib`, default unlimited, `0`). **The actor keeps serving from the source (reads only)**; when everything verified, the session persists a record pointing at the copy, the actor switches its layout (`SwitchLocation`, ms), and only then the sources are deleted (`relocate_commit`). |
| Downloading, incomplete location changes | The actor is stopped (peers dropped, resume data flushed), the partial files are relocated (`have` = verified pieces; holes stay holes), the record gets the new `stage_root`, and the torrent restarts and carries on. |
| Downloading, only the finished location changes (`Retarget`) | Nothing is copied: the record and the placement change, a running actor switches layout, and `finalize` later moves into the new place. A staged torrent keeps the file names of its staged copies. |
| Organize by status | `MoveTarget::Status(folder)`: a sibling folder of the same finished location, so a rename; the torrent's save path in the record changes with it. |

Safety: never overwrites (a name that exists gets ` (2)`; a torrent that collides becomes "placed" with per-file
destinations), refuses when the destination lacks `size + 1 GB` (`move_reserve_bytes`), a failed or cancelled move leaves the
torrent seeding from where it was (`MoveFailed`). Removing a moving torrent cancels the copy first. Under `cargo test` a
destination inside the real home is refused (`guard::check_path`).

Crash safety: every relocation is journaled in `<state dir>/moves/<info-hash>.journal` (the trs-storage relocate journal; see its
README for the recovery matrix). At start-up torrents with a journal stay stopped until it is resolved: the journal's plan is compared
with the record: record at the destination -> finish (delete sources), otherwise roll back. Empty folders left by an interrupted
*finish* are not pruned (no old root is known after a crash).

Known limits: xattrs/ACLs of a copied file are not preserved (a rename keeps them); a torrent that is still finishing
(`F_STAGED` and finished) is retried every 10 s (up to 30 times) for automatic triggers and refused for explicit ones.

## Smart add (SPEC 7A.6)

```rust
async fn preview(&self, src, opts) -> Result<AddPreview, _>;                  // AddPreview::choice: ChoicePreview
async fn add(&self, src, opts) -> Result<Added, _>;                           // Added::{needs_choice, choice}
async fn choice(&self, id) -> Result<ChoicePreview, _>;                       // structure + preselection (held: default; running: current ticks)
async fn reopen_choice(&self, id) -> Result<ChoicePreview, _>;                // Get Info > Files (same as choice)
async fn confirm_choice(&self, id, ChoiceSelection::{Default | Files(Vec<FilePriority>)}) -> Result<(), _>;
async fn cancel_choice(&self, id) -> Result<(), _>;                           // removes a held torrent; never deletes files
AddOptions::hold: Option<bool>   // None: settings decide; Some(false): start at once (the caller chose); Some(true): always hold
```

`TorrentState::AwaitingChoice` (new; `as_str` = `awaiting-choice`), events `ChoiceNeeded { id, name, reasons, origin }` (every hold:
watch folder, agent, magnet; show the orange dot and the notification) and `ChoiceResolved`. A watch folder in `ask` mode still emits
`AddRequest` (its `preview.choice` carries `ChoiceReason::FolderAsk`); a folder in `auto` mode adds a torrent that needs a choice in
state AwaitingChoice and emits `ChoiceNeeded` instead of `AutoAdded`.

**Grace period (SPEC 7A.7).** A torrent that would start without asking is added held for `EngineConfig::start_delay_secs` seconds
(0, the engine default, = start at once; the settings file's `transfers.start_delay_secs` defaults to 10). It is the silent held state
of `AwaitingChoice` plus a countdown the session owns (`session/pending.rs`, a `Job::PendingDue`): state `TorrentState::Pending`
(`pending`), `TorrentRow::pending: Option<PendingStatus { deadline_ms, remaining_ms, paused }>`, `Added::pending`,
events `Pending { id, change: Added | Paused | Resumed | Started | Chosen }`. `AddOptions::start_now` (and `paused`, `hold: Some(false)`,
non-empty `file_priorities`) skip it; a torrent that needs a choice never gets one; a magnet link is held before its metadata fetch
(ask mode `Always` skips it). Calls: `pending_pause`, `pending_resume` (at least 3 s left), `start_now`, `pending_choose` (cancels the countdown, the
torrent stays `AwaitingChoice`; a magnet fetches its metadata and `ChoiceNeeded` follows). The mark is persisted (`TorrentRecord::pending_start`,
in `torrent-extra.bin`); after a restart the torrent is held again with a fresh countdown. `tests/pending_start.rs` checks the silence
(MockTracker, DHT, a counting peer, a seeder dialling in), the deadline, pause, start now, restarts, setting 0, choose and the watch folder.

Settings: `transfers.ask_before_adding` = `never` | `multi_file` (the SPEC triggers; default) | `always`, `transfers.large_torrent_gb`
(default 50). `EngineConfig::smart_add` is `SmartAddConfig { mode: AskMode::{Never (default), Smart, Always}, large_bytes }`; the
engine's own default is **Never** so existing callers are unchanged, `config_map` turns it on from `config.toml`.

Triggers (`ChoiceReason`): `SeveralItems` (season pack, collection, discography, bundle of >= 3 books), `InLibrary`, `PlatformVariants`,
`VeryLarge`, `NotEnoughSpace` (preselection larger than the free space at the destination or the incomplete location), `Unsure`
(confidence below the threshold and `detection.ask_when_unsure`), `FolderAsk`, `Always`. Items come from `trs_classify::structure`;
`ChoicePreview { needs_choice, awaiting, reasons, structure, groups: Vec<ChoiceGroup{label, kind, items: Vec<ChoiceItem{label, identity,
bytes, files, selected, library: LibraryState, quality, this_mac, platform}>}>, files: Vec<bool> (per torrent file index; BEP 47
padding is `true`), selected_bytes, selected_files, total_bytes, free_bytes, destination, category, confidence, library_checked, rule_name, goal }`.
The structure follows the category the torrent has now (`set_category` changes it; items that would read as the same title get
the part of their file name that tells them apart, e.g. `Fallen Kingdom — S01E02`, never identical rows).
Groups end with "Other files" (kept whenever something is chosen) and "Extras" (junk, unticked).

**Library presence** (`choice.rs`, only for torrents with several items or platform variants; filesystem work off the actor, capped at
400 folders, 512 entries each, 1.5 s): an item is *in the library* when every primary file (>= a tenth of its largest) exists at its
place in the torrent's own layout below the save folder with the same size; *at another quality* when
a file in that destination folder yields the same identity key (`trs_classify::library_identities`: same show+season+episode, same title
and year, same book) or, for albums, the folder holds at least half as many audio files. Software is matched by destination only.
Preselection: everything the library does not have; software: only `this_mac_items()`; Extras unticked.

**A held torrent does nothing**: a `.torrent` is added without registering with the tracker driver, without a torrent actor (so no peer
connection, no `ensure_root`, no staging folder, no file), refuses incoming handshakes, ignores `add_peers`, takes no download slot and no
DHT lookup. A magnet fetches its metadata (that needs the tracker and peers), then the tracker session is closed (`stopped`), the metadata
actor ends and nothing is requested. The flag survives a restart (`TorrentFlags::AWAITING_CHOICE`). There is no timeout.

**Edge pieces.** A skipped file that shares a piece with a wanted file still receives that piece's bytes. For a placed torrent such a
file lives in a hidden `.seedwell-partial/` folder next to its clean destination (media apps ignore it; older builds called it
`.dispatch-partial/`, which is still recognised) instead of appearing in the library as a full-length, mostly empty episode; ticking it
later moves it to its clean destination (`apply_edge_policy`).

The move of a file that a running torrent may be writing is one storage operation, `TorrentHandle::switch_layout_renaming`: under an
exclusive per-torrent gate it renames the files and installs the layout that names the new paths; every `open` of a torrent file takes
the gate shared and resolves its path from the layout installed at that moment, so no write can create the old path after the rename.
The record is committed only after storage confirmed. A crash in between leaves the record naming one path and the data at the other;
`edgefix` repairs that at the next start (rename the sibling to the recorded path), and when both exist (builds before this fix could
leave the real data hidden next to a visible, mostly empty file) the torrent is rechecked against both candidates and the one with more
verifying pieces keeps the recorded path; the other is kept as `<name>.stale` in the hidden folder, never deleted.

## Watch folders (SPEC 7A.1)

One task ticks every 2 s. A folder costs one `stat`; the directory is listed when its mtime moved, while a candidate is
still settling, and once a minute as a safety net. A `.torrent` is taken after its size and mtime stayed the same for a
second. **Only files that arrive after the folder joined the watch list are looked at** (`WatchConfig::since`, persisted by `Host` as
`folders.watch[i].since`): a file's arrival is the latest of its birth, modification and status-change time, so a file copied or moved in
later counts, one that has been sitting there does not. The rest is the folder's *backlog*: `EngineHandle::watch_backlog(&Path) ->
Vec<PathBuf>` lists it (sorted, at most 1,000) so the app can offer "Add 5 existing torrents from Downloads?" once and call
`add_torrent_path` for the accepted ones. Without a persisted value (an engine used without `Host`) the baseline is the moment the folder
first appears in the process. Per folder: mode `auto` (add; ask when the confidence is below the threshold and `ask_when_unsure`) or `ask`
(`EngineEvent::AddRequest` with an `AddPreview`; the app answers with `add_torrent_path`), a forced category and rule,
and the after-add action (Trash = `~/.Trash` with a non-colliding name, Archive, Leave, Delete). Invalid files are
reported once (`WatchError`) and left alone; duplicates give a `Duplicate` event.

## Direct downloads (SPEC 7D)

`EngineHandle::add_download` creates a `trs-http` task. They share the "active downloads" slots with torrents (one queue
order), the global download limiter (`DownGate: RateGate` over the same `RateLimiter`) and the control API. Rows have
`kind: RowKind::Direct`, state `Completed` when done, never count as Sharing and have no tier. A record per download is
kept in `downloads/<id>.rec` (the resumable bytes are trs-http's `.part` and sidecar); progress is written every 10 s while
running and at shutdown, which pauses every transfer losslessly. A finished file goes to its category's finished folder when the category has one; a link that serves a `.torrent` becomes a torrent. The finished list keeps the latest 50.

## Listen port taken

If `network.listen_port` cannot be bound (another client still runs on it) `Engine::start` does not fail: it listens on a free
port (the same number for UDP, for uTP and the DHT), keeps the configured value in the configuration, and reports
`Snapshot::port_fallback` / `EngineHandle::port_fallback()` = `PortFallback { wanted, using, message }`, where `message` is ready to
show ("Port 55252 is in use by another app; using 61234 for now"). Port mapping and tracker announces use the port in force.
Only when no port at all can be bound does start-up fail.

`network.listen_port = 0` (or absent) means a random free port at every start; the engine never writes it back. Only a number
entered by the user is stored. A change at runtime (`update_config` / `apply_settings`) rebinds live: the new TCP+UDP pair is
bound first and only then swapped in (a taken port keeps the old one and sets `port_fallback`; `0` picks a fresh random port).
TCP peers stay connected, uTP peers drain on the old socket, the DHT restarts on the new socket with its saved state, port
mapping is redone and every tracker hears the new port with its next regular announce.

## Settings at runtime

`config_map::engine_config` maps `config.toml`; `EngineHandle::apply_settings` applies it. Limits, alt limits, the
**alt-speed schedule** (days, from/to in local time, windows across midnight; the session sleeps until the next edge,
re-checks at least every 15 minutes), queue size, connection caps, tiers, DHT/PEX switches, watch folders, locations, the
status-folder switch and port mapping take effect at once. Listen port, write-cache size and fd limit, and turning DHT on, need a
restart: `ConfigReloaded.restart_required` names them. Local time comes from `/bin/date` unless the app installs a
`ClockFn` / `HostOptions::time_zone` (the engine forbids `unsafe` and has no zone database).

`Host::start(dir, opts)` opens the state directory, loads the three documents (never failing: last good copy, else
defaults), rebuilds the torrent list from `torrents/*.torrent` when `state.redb` was damaged, starts the engine, writes a
random first-launch port back to `config.toml`, and runs one `spawn_watcher` poller per document.

## Outages

`EngineHandle::network_path_changed(satisfied)`, `system_will_sleep()` and `system_did_wake()` (the app feeds them from
`NWPathMonitor` and the workspace notifications; a wall-clock jump the monotonic clock did not see counts as a wake, which
covers the headless daemon) reach the tracker driver, which owns the decision (`trs_tracker::outage`, SPEC 7C.4c). Its
verdict is published to the rest of the engine in `netstate::NetFlags` (relaxed atomics): torrent actors stop dialling while
`held()`, the DHT is paused (`DhtHandle::set_paused`) and, at recovery, resumed with a fresh bootstrap backoff
(`network_restored`), torrents with nobody connected forget their dial backoff (`TorrentMsg::NetRestored`) and the port mapper
is refreshed once. A peer handshake, an accepted connection or a DHT datagram calls `NetFlags::alive()`: one clock read and
one relaxed store while online, a debounced probe request while an outage is declared. `Snapshot::online` /
`offline_since_ms` carry the state to the UI (a wake check is not an outage). Tests: `tests/outage.rs`.

## Port mapping

With `network.upnp` / `network.natpmp` the listen port is mapped TCP + UDP by `trs-portmap` (PCP and NAT-PMP, UPnP).
`network_changed()` is forwarded; at shutdown the mappings are deleted concurrently with the tracker `stopped` wait
(bounded by the portmap crate's own 3 s). `EngineEvent::PortMap(PortMapStatus)` reports the method, external address,
CGNAT and the external ports. Trackers are told the **external TCP port** when the router granted another one than the listen port:
the tracker driver holds its commands (at most 5 s) while a mapper is starting, so the torrents loaded at start-up are registered
with the mapped port; torrents added later use it too. A mapping that changes while torrents already announced is not re-announced
(the tracker hub has no per-torrent port setter; a `TrackerHub::set_port` in `trs-tracker` would close that gap).

## Counters and the tracker

`Counters` keeps lifetime totals and a per-`started` base. `uploaded`/`downloaded` in announces are
session deltas that restart at every `started` (also on resume). `downloaded` is the size of each
piece from peers that passed its hash check, added when it passes (one place: `torrent/maintenance.rs`,
`piece_verified`); hash-failed pieces, duplicates and pieces in progress never count. A failed piece adds
its size to `corrupt`, sent as `corrupt=` on HTTP announces when above zero. `uploaded` is the piece
payload the connection took from its send queue (`peer.rs`, `credit_written`). `left` is exact, from the
verified bitfield. `key` and `peer_id` are
generated once per process.

## Control-API behaviour worth knowing

* `reannounce(ids)` is async and answers per tracker (`ReannounceResult { id, url_redacted, sent, not_before_ms, waiting_after_failures, force_limited_until_ms, min_interval_until_ms }`; `reannounce_with(ids, force)` also goes past the tracker's minimum interval (once per tracker per 5 min); Seedwell's own backoff never holds a user's ask back) from the hub's
  real min-interval gate. `remove(ids, delete_files)` replies when the torrents are stopped and the files are deleted.
* `set_file_priorities` updates `wanted_bytes`; a Cold torrent's `torrent_detail` shows its real file priorities.
* Pausing remembers the connected peers (bounded) so that resume redials them at once.
* **Real-home guard** (`guard.rs`): in a cargo test binary (or with `TRS_FORBID_REAL_HOME=1`, which a test must set on child
  processes it starts) the engine refuses to start with, or hot-reload into, a download/incomplete/watch/archive/Trash/library
  folder inside `$HOME`. Set `HOME` to a temp dir or use explicit folders; `TRS_ALLOW_REAL_HOME=1` disables it.

## Known limits and open items

* `StateStore` cannot keep per-file Low/High priorities (trs-state stores only the skipped-files bitset), the queue position
  (a side file `torrent-extra.bin`, which also holds last activity and error start), or partial pieces; see `state_store.rs` for the field-by-field mapping.
* Direct downloads left in `.incomplete` without a record (the record was lost) are not adopted.
* "On battery" handling needs power status from the app and is not implemented here.
* LSD is not wired into the session; IPv6 settings are accepted and ignored.

## Dependencies

`trs-*` workspace crates (`trs-state` for the store adapter and settings types, `trs-classify`, `trs-http`, `trs-portmap`);
`trs-utp` (uTP on the shared UDP socket); `tokio` (rt, net, time, sync, macros, io-util, fs) for sockets and actors;
`bytes` for the zero-copy receive buffers `trs-wire` slices; `getrandom` for the tracker `key` and
RNG seeds. Dev: `tempfile`, `libc` (peak RSS in the network test), tokio `signal` (the `bench_client` example).

## Tests

`cargo test -p trs-engine` runs unit tests (picker, choker, rate limiter, tiers, queue, counters,
store, PEX log, chan, config mapping, schedule, policy, organizer helpers, watch scanner) and integration tests on
loopback (`tests/`): swarm, skipped files, pause/resume with limits, restart and resume, missing files, a corrupt peer,
magnets, private torrents, PEX, tier transitions, queue, staging, tracker audit, scale, `seeding` (raw-wire leechers in the manner of Transmission:
late `interested`, deep pipeline, a leecher that dies, a new one from the same IP; unchoke latency and throughput floors), `utp` (uTP-only and
mixed swarms, uTP-first dials of known peers, the DHT on the shared port, `network.utp` off), and
`state_store` (the same scenarios on redb, record/resume/placement round trips, statistics buckets, launch history,
database recovery), `policy` (rules, profiles, goals, stop-when-finished, statistics per profile), `organize` (flat placement of movie, TV,
music and book torrents, junk, collisions, restart seeding byte-exact), `status_folders` (enable/disable moves, pause and
goal -> Stopped, no move while uploading, downloads untouched, pinned and declined stay), `watch`, `direct` (a local range-capable HTTP server),
`portmap` (a NAT-PMP gateway on loopback) and `settings` (alt-speed schedule, `Host` hot reload, watch-folder baselines),
`smart_add` (every trigger, a held torrent makes no tracker request and no connection and creates nothing, magnets hold after
their metadata, confirm downloads only the chosen files, cancel leaves nothing, library presence with files in the torrent's own folder,
reopening the chooser), `locations` (per-category incomplete/finished locations, precedence, staging root stored per torrent)
and `moves` (same-volume move under load with a downloading peer, forced and **real** cross-volume copies - a temporary APFS disk
image made with `hdiutil`, always detached - collisions, free space, a corrupt source caught by the hash while copying, a crash
at every journal step on both methods, category change, incomplete/finished location changes of a download,
queue of 60 torrents, remove and cancel during a copy). `watch` also covers the baseline and the backlog.
Tracker tests use `DestinationPolicy::LOCAL` (the production default refuses loopback trackers).

`cargo build --release -p trs-engine --example bench_client` builds the headless client of the comparison harness
(`bench/README.md`, "Seedwell adapter binary"). The Debian download is
`#[ignore]`: `cargo test -p trs-engine --test network --release -- --ignored --nocapture`.
