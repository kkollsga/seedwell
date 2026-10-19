# trs-tracker

Tracker communication for Seedwell: **correct by construction** (SPEC 7B, 7C).
One bug here can get users banned from private trackers, so the design goal is
that hammering is impossible and every protocol rule is a pure, exhaustively
testable function.

```
 torrent events ─┐
 timers ─────────┼─▶ AnnounceMachine ──SendAnnounce──▶ HostGate ──▶ GatedRequest ──▶ HttpTransport / UdpTransport
 responses ──────┘   (pure, per torrent×tracker)        (only code that can           (take &GatedRequest,
                      ▲ TrackerSet: BEP 12 tiers        mint a request)                cannot make one)
                      └ TrackerHub: owns sets + gate + TimingStore, persists before releasing
```

Everything above the transports is **sans-I/O**: no sockets, no clock, no RNG.
Time is a `Millis(u64)` passed in by the caller; randomness is injected
(`rng::Rng`, `SplitMix64`). Tests and the 90-day simulation run on a virtual clock.

## Modules

| Module | Role |
|---|---|
| `machine` | `AnnounceMachine`: the state machine for one (torrent, tracker URL). `handle(Input, now) -> Vec<Intent>`. Every policy lives here: event order, intervals, backoff by role, counters, the 4.0.6 rules. Audits itself before emitting (fail-closed). 112 bytes while idle; the session state (~250 bytes) exists only while a session is open. |
| `set` | `TrackerSet`: BEP 12 tiers: seeded shuffle once per tier, **every tier independent** (SPEC 7C.4b), failover inside a tier, promotion of the working tracker, a failover that was passed over is retired (`Input::Retire`: silent if it never acknowledged `started`, else one `stopped` and no new `started` before its regular time), http/https twins folded into one tier, `set::fold_tiers` (the engine folds tiers that share a tracker profile, so alternate domains of one tracker fail over instead of announcing in parallel; hosts without a shared profile stay independent), dedupe and a 32-tracker cap. |
| `gate` | `HostGate`: the **only** producer of `GatedRequest` (private constructor, no `Clone`). 1 request per (torrent, tracker), ≤ 2 concurrent per host, per-host token bucket (1 req/s, burst 3, starting **empty** so restart loops cannot hand out bursts), host circuit breaker, registered-URL containment (private torrents), crash-loop hold, in-flight deadlines, independent protocol audit, fail-closed host trips. Hosts are keyed by their canonical spelling. |
| `audit` | A second, separately written check of the wire invariants, run by the gate on every release. |
| `hub` | `TrackerHub`: glue for the engine. Persists, then submits to the gate; `tick(now)` returns the requests to execute; `complete(ticket, result, now)` feeds answers back. A failed durable write of `started` or `completed` refuses the request (nothing is sent) and the machine retries with backoff. Owns the time model (below). |
| `store` | `TimingStore` trait + `MemoryStore`: last announce, next allowed, min-allowed, `completed_sent`, `completion_owed`, `session_open`. (The `tracker_id` field is obsolete: the tracker id is per session.) `Debug` of keys and the store never prints the passkey. |
| `policy` | `DestinationPolicy`: where the transports may connect (SSRF protection, below). |
| `url` | `TrackerUrl` (passkey-safe `Display`), exact RFC 3986 encoding, the single `PARAM_ORDER` table, `canonical_host`. |
| `response` | Defensive bencode response parsing via `trs-bencode`, the SPEC 7C.1 clamps, error classification. |
| `backoff` | 1 → 2 → 4 … min, capped at 1 h nominal, +0–20 % jitter, server hints win. |
| `udp_proto`, `scrape`, `retry_after` | Sans-I/O BEP 15/41 wire format, BEP 48, RFC 9110 `Retry-After`. All fuzzed. |
| `crash` | `CrashLoopGuard`: 3 launches in 10 min *that followed an unclean exit* ⇒ hold; `LaunchLog`: the persisted launch history with clean-shutdown markers. |
| `http`, `udp` *(feature `transport`, default)* | hyper 1.x + rustls and tokio UDP transports. |

## Driving it from the engine (event loop sketch)

```rust
let mut hub = TrackerHub::new(GateConfig::default(), store /* impl TimingStore */, seed);
let http = HttpTransport::with_config(env!("CARGO_PKG_VERSION"), HttpConfig {
    policy: DestinationPolicy { allow_lan_trackers: user_setting, ..Default::default() },
    ..HttpConfig::default()
});                                                           // User-Agent: Seedwell/<version>
let udp  = UdpTransport::bind(UdpConfig {
    policy: DestinationPolicy { allow_lan_trackers: user_setting, ..Default::default() },
    ..UdpConfig::default()
}).await?;
hub.add_torrent(TorrentSpec::new(info_hash, peer_id, key, port, tiers), clock())?;
hub.set_stats(&ih, stats, clock());                // push counters before they matter
hub.torrent_event(&ih, TorrentEvent::Start, clock());
loop {
    // 1. let the hub run timers and release whatever the gate allows
    for req in hub.tick(clock()) {
        let (http, udp, tx) = (http.clone(), udp.clone(), done_tx.clone());
        tokio::spawn(async move {
            let ticket = req.ticket();
            let result = match req.url().scheme() {
                Scheme::Udp => udp.announce(&req).await,
                _ => { let o = http.announce(&req).await; extra.send((req.url().host().to_owned(), o.extra_hops)); o.result }
            };
            tx.send((ticket, result));
        });
    }
    for r in hub.take_reports() { /* UI: Announced / Warning / Failed / Halted ... */ }
    // 2. wait for the next thing: an answer, a user/engine command, or the hub's wake-up
    select! {
        (ticket, result) = done_rx.recv() => hub.complete(ticket, result, clock()),
        (host, hops) = extra_rx.recv()    => hub.charge_extra(&host, hops),
        cmd = commands.recv()             => apply(cmd),   // Start/Stop/Pause/Resume/Complete, set_stats,
                                                           // manual_reannounce, network_changed, wake
        _ = sleep_until(hub.next_wake(clock())) => {}
    }
}
```

Outages (SPEC 7C.4c): the engine tells the hub what the OS knows (`path_changed(satisfied)`, `did_wake()`), gives it signs of
life from outside the tracker code (`note_alive(at)`, `net_hint()`), and drains `take_net_events()` (`Offline`, `Probe`,
`ProbeSkipped`, `Online`). The hub also infers an outage from unreachable-class failures on 2 distinct hosts. Offline, it holds
every machine through one shared flag, freezes backoff (`Input::Unreachable`, `Input::Forgive`), and releases a single probe
(`Input::Probe`) on the schedule in `outage.rs`; on recovery `Input::Release` spreads the announces that came due. `next_wake`
includes the probe time, so the single timer is the one the loop already has.

Rules for the engine (also in the type docs):

* **Clock.** Pass **Unix milliseconds from the wall clock** (`SystemTime`) to every
  `now`: persisted timings use that epoch in every launch. Do not smooth or
  fabricate values. The hub does not trust the clock to be monotonic:
  * a **backward step** (NTP, a corrected wrong reading) raises an internal
    offset, so the internal time line never goes back and every running wait
    continues unchanged: nothing becomes early and nothing stays pinned in the
    future. A forward excursion that is later corrected therefore costs nothing
    after the correction. Everything handed back (`next_wake`, report times,
    persisted records) is converted back to the wall time line;
  * a **forward jump** (sleep, hibernation, clock set forward) is taken at face
    value: that much time may really have passed. After a genuine sleep what is
    due is announced, spread by the token bucket and the wake jitter, never
    earlier than `interval` / `min interval` in elapsed wall time. A bogus
    forward reading can make announces due early *once* per tracker (rate-limited
    by the bucket);
  * `Millis` near `u64::MAX` saturates.
* Call `set_stats` whenever counters change materially and always before
  `Complete`/`Stop`. Counters are **session** counters (zero at `Start`). A `started`
  carries the session counters pushed by `set_stats` before it is built: 0/0 when
  none were pushed yet (the usual case for a first `started`), but a `started`
  that goes out a little after a resume, or one that is retried after a failure,
  carries the real session totals so far (`uploaded`/`downloaded` since `Start`;
  they never decrease within the session). That is what Transmission sends on a
  retried `started` (its counters are cleared only by a successful announce) and
  what SPEC 7B.2 asks for ("the per-session reporting matches what trackers
  expect after a new `started`"); the next session starts from 0 again. `left` is exact; `size`
  must be the torrent length (`size == 0` means "metadata unknown" and `left`
  is reported as ≥ 1 so a magnet never claims to be complete, unless the torrent
  was `added_complete`).
* `Complete` may arrive at any time, also while a stop is in progress; it is
  honoured once per torrent (`completed` is then sent in the next session if
  needed). It is ignored for torrents the tracker never saw as leeching.
* **`set_role(&ih, role, now)`** tells the hub what the torrent is doing (retry
  pace, below): `Downloading` (default, incl. metadata), `Seeding`,
  `SeedingDormant`. Call it after `add_torrent`, when a download finishes, and
  when a seed becomes dormant or popular again. It is not persisted.
* `Start`/`Resume` are *user actions*: over a halt or a slow-retry streak they
  retry, but never sooner than 60 s after the last attempt (same floor as
  `manual_reannounce`). Nothing else clears a halt.
* **Quit** (SPEC §12 #16): call `hub.begin_quit()` once, then `hub.quit(&ih, now)` for every
  open torrent, then wait for `is_quiescent` until your own deadline and drop what is left
  (`stopped` is best effort). `begin_quit` switches the gate to the quit rules for `stopped`
  only (libtorrent's way: all stops at once, a short wait for the answer, which is irrelevant):
  at most `GateConfig::quit_max_in_flight` (50) in flight over all hosts together, so also per
  host; the per-host cap of 2 and the token bucket do not apply to them. An open circuit
  breaker still holds a host back, and a quit-time `stopped` that fails never feeds the breaker
  (`quit_stop_deadline`, 5 s, frees a slot whose request never finished). Regular announces
  keep the cap, the bucket and every interval. A `stopped` from a pause or remove while the app
  runs is not a quit-time stop and keeps the regular rules. Over HTTP use
  `HttpTransport::announce_quit(req, QuitLimits { connect, answer })`: one attempt on one address
  (no second family, `completed`-style at-most-once), `connect` (3 s in the engine) for the
  connection and `answer` (2 s) for the response head once the connection is up, which is when
  the short GET is written; a timeout is the normal end for a slow tracker and the request
  has been sent. Keep-alive connections are reused as usual. Machines that tripped count as
  quiescent once told to stop.
* **Crash-loop breaker.** Keep a `crash::LaunchLog` (text form: `parse`/`render`). At start call
  `log.record_launch(now)` and feed the returned times to `CrashLoopGuard::evaluate`, then
  `hub.hold_until(t)` if it returns a time. At the end of an orderly quit call
  `log.mark_clean_exit()` and persist it. Only launches whose run left no marker (a crash, a
  kill, a power cut) count, so relaunching the app any number of times never holds an announce,
  while three launches in a row after crashes still do. (The engine does this in
  `StateStore::record_launch` / `record_clean_exit`, `launches.log`.)
* After an HTTP transport returns, pass `extra_hops` to `TrackerHub::charge_extra(host, hops)` so redirect hops are paid for out of the token bucket. The host may be spelled in any form.
* **Destinations.** Both transports refuse to connect to loopback, link-local
  (incl. `169.254.169.254`), unspecified, multicast and cloud-metadata
  addresses, and to RFC 1918 / CGNAT / ULA addresses unless
  `DestinationPolicy::allow_lan_trackers` is set (a user opt-in, default off).
  The check runs on the *resolved* address on every connection and every
  redirect hop, and the connection goes to the vetted address (no DNS
  rebinding). A refusal is `TransportError::Forbidden`. Tests use
  `DestinationPolicy::LOCAL` (adds loopback).
* A `HostTripped` gate event (an invariant check failed) stops that host fail-closed; the affected machines stop announcing. Recovery without a restart: `reset_host(host)`, `Stop` the torrent, wait for `is_quiescent`, `remove_torrent`, `add_torrent`, `Start`. `trip_host(host, reason)` is the same switch pulled by the engine.
* Late or duplicate answers for a ticket are fine to forward (`complete` handles them): that is exactly the 4.0.6 case.
* A transport reports `Dns`, `Connect`, `ConnectTimeout`, `Tls` or `Forbidden` **only if the request was not written** (see "`completed` delivery" below); the shipped transports keep that promise.
* **Dual-stack hosts** (the transports handle it inside one released request; the gate sees one announce
  and the machine one answer, so the token bucket and the backoff are charged once):
  * *HTTP.* hyper races the addresses of a name only for the TCP connect. If the first family accepts
    the connection and then resets it, closes it without a response, or does not answer within
    `HttpConfig::address_timeout` (15 s), the request is sent **once** to the other family in the rest of
    `total_timeout` (30 s). Only a failure *before the response head* triggers it; a 4xx/5xx or a broken
    body is an answer and is never retried. The family that worked is remembered per host for
    `HttpConfig::family_memory` (1 h, not refreshed by later successes, so the natural order is probed
    again once an hour); a remembered family that fails is tried the same way, the other family once.
    A host with a single family, or an IP literal, gets the whole `total_timeout` and no second try.
  * *`completed` is never sent to a second address.* After a failure on an established connection the
    request may have been delivered, and the machine keeps `completed` set in that case (at-most-once,
    "`completed` delivery" below), so a second send could be counted twice. `started`, `stopped`,
    regular announces and scrapes are idempotent on the tracker and do fall back; a duplicate `started`
    or `stopped` is what the retry schedule would send a minute later anyway. If the second try only fails
    to connect, nothing was sent and the first error stands; if it connects and fails, its error is returned.
    Either way a failed announce is reported once.
  * *UDP.* IPv4 is tried first (a remembered fallback family first for `FAMILY_MEMORY`). If the BEP 15
    `connect` handshake with that address gets no answer in its first three transmissions (15 + 30 + 60 s),
    the other family is tried in the same call. Only the handshake is ever abandoned this way (nothing
    announce-shaped was sent), so this is safe for every event including `completed`. An announce that was
    sent and not answered is never re-sent to another address.

## Invariants: where each is enforced and tested

Enforcement is layered: (1) the machine's pure logic, (2) the machine's own
pre-send audit, (3) the gate's independent `Auditor`, token-bucket window
cross-check and registry. Tests have a fourth, independent oracle
(`tests/common/oracle.rs`) that sees only what a tracker would see.

| # | Invariant (SPEC 7C.3) | Enforced in | Tested by |
|---|---|---|---|
| 1 | No non-event announce before `max(interval, min interval)` after a success; manual ≥ `min interval` (300 s floor if absent) | `machine::drive`/`apply_success`, `machine::audit`, `audit::Auditor::on_release` | `machine::tests` (`started_then_interval`, `min_interval_beats_interval`, `manual_waits_for_min_interval`), `machine_props::success_wins_in_any_order_batched`, `hub_props::invariants_hold` (oracle rule 1), `model_check` (witness), `simulation` (oracle + tracker-side spam detector) |
| 2 | `started` first per session; `stopped` only after `started`; nothing after `stopped` but a new `started` | `machine` phases (`started_dispatched`, `want_stop`, `stopped_dispatched`), audit, `Auditor` | `machine::tests` (`stop_before_started_dispatch_sends_nothing`, `nothing_after_stopped`, …), `machine_props::arbitrary_inputs_are_safe`, `hub_props`, `model_check` |
| 3 | `completed` at most once that the tracker could have received (persisted), never for torrents added complete | `completed_sent` persisted **at dispatch, before the request leaves** (`hub::mark_dispatched` applies `Persist` first and refuses the request if the write fails); `completion_owed` survives crashes; `leech_seen`, `added_complete`; `Auditor`; the one exception is a `completed` that provably never reached the tracker (below) | `machine::tests` (`completed_once_…`, `never_completed_for_added_complete`, `completed_flag_is_persisted_…`), `set_hub::{completed_flag_is_durable_before_the_request_is_released, failed_persist_of_completed_fails_closed, crash_and_restart_never_sends_completed_twice}`, `hub_props`, `model_check` (crashes interleaved everywhere) |
| 4 | `uploaded`/`downloaded` non-decreasing within a session; `0 ≤ left ≤ size` | `machine::effective_stats` (high-water marks, clamping), audit | `machine::tests::counters_never_decrease_and_left_is_clamped`, `hub_props` with `sloppy_engine` (the harness feeds decreasing counters and `left > size`), `model_check` |
| 5 | Per-host ≤ token bucket; ≤ 2 concurrent per host; ≤ 1 in flight per (torrent, tracker) | `HostGate` (bucket, concurrency, one-per-key), sliding-window self-check, host trip | `tests/gate.rs`, oracle windows in `hub_props` and `simulation` (≤ burst + W requests in any W-second window, W ∈ 1,2,5,30,300) |
| 6 | `key`, `peer_id`, tracker-id echo stable within a session (the tracker id is per session, like Transmission and libtorrent: never persisted) | immutable `MachineConfig`; tracker id latched per session; audit | `machine::tests::tracker_id_is_echoed_and_latched`, `hub_props` (the fake tracker changes its id on purpose) |
| 7 | Private torrents contact only their own announce URLs | `HostGate` registry (exact URL match, all torrents); transports never follow cross-host redirects | `gate::unregistered_urls_and_torrents_are_refused`, oracle rule 7 in `hub_props`/`simulation`, `transport::http_refuses_cross_host_redirects_and_loops` |

§7C.4 and §7C.4a:

| Rule | Where | Tests |
|---|---|---|
| Backoff by role: downloading 1 → 32 min (cap 1 h), seeding 15 min → 4 h (cap 6 h), dormant seed 30 min → 8 h (cap 12 h), +0-20 % jitter | `backoff::{nominal_delay_for, retry_delay_for}`, `machine::apply_failure` | `backoff::tests`, `fixes::{transient_failure_schedules_follow_the_role, role_is_read_when_each_retry_is_scheduled, role_change_never_makes_a_retry_earlier_except_to_downloading, seeding_to_downloading_pulls_the_pending_retry_in}`, oracle backoff rule everywhere |
| `Retry-After` (429/503) and BEP 31 `retry in`: downloading waits **exactly** the hint (never under 1 min), seeding roles the later of hint and schedule | `response::AnnounceError::class`, `backoff::retry_delay_for`, `http` | `fixes::{retry_after_downloading_waits_exactly_seeding_waits_for_the_later, pulling_a_retry_in_still_honours_the_server_hint}`, `machine::tests::{retry_after_is_honoured, bep31_retry_in_minutes}`, `transport::http_error_statuses`, `retry_after::tests` |
| `failure reason` that looks **permanent** halts until a user action; every other `failure reason` is retried 30 min → 1 h → 2 h → 4 h → 8 h → every 24 h (all roles) and surfaced as a warning | `response::{is_permanent_reason, PERMANENT_REASONS}`, `FailureClass::{Halt, Slow}`, `Report::{Halted, Warning, Failed}` | `fixes::{permanent_*, maintenance_*, failure_reason_schedule_does_not_depend_on_the_role, start_resume_and_manual_over_a_halt_or_a_slow_failure_keep_the_floor}`, `machine::tests::failure_reason_halts_until_user_acts`, oracle "did not halt", `machine_props`, `model_check` |
| One announce per (torrent, tracker) per interval; no per-family announces | `TrackerSet`/`HostGate` design | `dual_stack::ipv6_blackholed_500_torrents_24h` (every tracker-side announce counted: exactly `ceil(24h/interval)` per torrent) |
| Success wins over a concurrent failure, any order, individually or batched | `machine::on_response` (`resolved` + `Undo`), gate `finish` treats a late success as proof the host is alive | `machine::tests::transmission_406_success_wins_any_order`, `machine_props::{success_wins_in_any_order_batched, late_success_wins_only_while_nothing_newer_was_sent}`, `dual_stack`, `model_check` (`DupAnswer`) |
| Re-announce time derives only from the latest successful response or from backoff | same | same, plus `dual_stack::detector_flags_a_client_with_the_4_0_6_flaw` (positive control: the tracker-side detector flags a client that lets the failure overwrite the success) |
| Crash-loop breaker; fail closed | `crash`, `HostGate::hold_until`, `trip_host`, machine `tripped` | `crash::tests`, `set_hub::crash_loop_breaker_hold_delays_everything`, `gate::{crash_loop_hold…, audit_violation_trips_the_host…, auditor_catches_each_protocol_violation}` |

## Test layers

| Layer | File | What |
|---|---|---|
| Unit | `src/*` | URL encoding (all 256 bytes, passkey URLs, byte-exact full URL, Transmission capture comparison), parsing of every key / garbage / huge-negative-zero intervals, clamp table, backoff math, UDP packet layout, scrape, Retry-After |
| Property | `tests/hub_props.rs`, `tests/machine_props.rs` | proptest over random events, faults, crashes, sleeps, network changes, many torrents on shared hosts; 4.0.6 interleavings; arbitrary garbage inputs |
| Model check | `tests/model_check.rs` | `stateright` BFS, every interleaving up to a depth bound of start / stop / complete / crash / timer / dispatch / 8 response classes (incl. maintenance `failure reason` and DNS failure) / duplicate answers / manual / network / suspend. Depth 9 by default (release); depth 10 (10.7 M unique states) is clean. `MODEL_DEPTH=n` overrides. |
| Simulation | `tests/simulation.rs` | 1000 torrents × 50 hosts × 90 days with faults; see below |
| Regression | `tests/dual_stack.rs` | Transmission 4.0.6 scenario + detector positive control |
| Regression | `tests/dual_stack_transport.rs` | dual-stack host whose IPv6 accepts and closes / accepts and never answers (real transport, fixed name table, loopback mocks): one fallback to IPv4, remembered family and expiry, no second send of `completed`, both families broken = two tries, UDP handshake fallback; then the real transport behind the real hub and gate on a virtual clock (60/30, 300/120): normal cadence at IPv4, no hammering |
| Audit regressions | `tests/adversarial_*.rs` | an independent adversarial audit: the hostile-input, scheduling, gate, tier, private-torrent and transport tests; the `bug_*` tests there reproduce the defects the audit found and are permanent regressions now |
| Design changes | `tests/fixes.rs` | failure-reason policy, `completed` delivery, retry schedules per role and role transitions, hints, clock model, host keys, scrape gap |
| Memory | `tests/memory.rs` | counting allocator: heap per (torrent, tracker) pair (budget below) |
| Engine API | `tests/engine_compat.rs` | the calls `tracker_driver.rs` makes, spelled as it spells them |
| Transport | `tests/transport.rs`, `tests/adversarial_transport.rs` | local mock HTTP and UDP trackers: exact request bytes, gzip, redirects, errors, keep-alive, timeouts, size limits, TLS fail-closed, UDP retransmit / connection-id reuse and expiry / spoofing / BEP 41, destination policy (every spelling of a local address), "was the request written" classification, transaction-id randomness |
| Fuzz | `fuzz/` | `response_parse`, `udp_decode`, `url_and_misc` (`cargo +nightly fuzz run <target>`) |
| Network (ignored) | `tests/network.rs` | one started + one stopped to Debian's public tracker for the current netinst info-hash |

### Simulation (seed 0, `cargo test --release -p trs-tracker --test simulation -- --nocapture`)

1000 torrents × 50 hosts × 90 simulated days, ~8 s wall time in release
(`SIM_SCALE=TxHxD` and `SIM_SEED=n` explore other sizes and worlds):

* 4,843,864 requests released (0.61/s overall); per host: min 28,653, max
  429,589, mean 96,877; at most **300 requests to one host in any hour**; at most
  **4 requests to one host in any 1 s window** (burst 3 + the one refilled token).
* Faults injected: 1,513 outage hits (host outages of hours-to-days plus a 3 h
  global outage), 298 permanent `failure reason` rejections, 2,600 maintenance
  `failure reason`s (slow schedule), 23,736 timeouts (incl. hangs), 22,024 × 5xx,
  13,095 × 429 + Retry-After, 13,202 garbage bodies, 8,604 DNS failures, 4,436
  BEP 31 `retry in`; 6 sleep/wake jumps (20 min – 40 h), weekly network changes,
  3,000 manual re-announces, 333 pause/resume cycles, 16 crashes including a
  6-crash loop (with the crash-loop hold applied), 1,355 circuit-breaker
  openings. (Seeds 0-4 are clean.)
* Per (torrent, tracker) pair (1,461 pairs) the announce count stayed below
  `ceil(active_time / interval) + events + user licences + 7·(failure streaks +
  crashes) + failure_time/1h + 2` for every pair; worst pair at 89.9 % of that
  bound, e.g. 4,140 announces against a bound of 4,605.
* The simulated trackers' own announce-spam detector (more than one non-event
  announce per min interval, the detector that flags Transmission 4.0.6)
  fired **0** times; every torrent was quiet after `Stop`; the independent
  oracle (invariants 1-7 plus backoff and halt rules) never fired.

## Parameter order and capture alignment

The announce request is aligned with what the mainstream clients (qBittorrent, Transmission) actually send
(captured announces;
SPEC §12 decision 18). We copy Transmission 4.x's shape because it is the
leanest: no extras that need an exact meaning.

`url::PARAM_ORDER` is the only place the order is defined:

```
info_hash, peer_id, port, uploaded, downloaded, left, numwant, key, compact,
[event], [trackerid], [ipv4], [ipv6]
```

Example (`started` for the Debian netinst torrent; the line breaks are ours):

```
GET /announce?info_hash=z%CF%8F%B5%90%B2%06%0D%D9%C3%14n%F7p%16%9DY43%B0
    &peer_id=-SX0001-abcdefghijkl&port=6881&uploaded=0&downloaded=0&left=792723456
    &numwant=80&key=0A1B2C3D&compact=1&event=started HTTP/1.1
Host: bttracker.debian.org:6969
User-Agent: Seedwell/<version>
Accept: */*
Accept-Encoding: gzip
```

* **`event` is omitted on regular announces.** `numwant=0` on `stopped`
  (`machine::build_params`); other announces use the configured numwant
  (default 50, SPEC 7B.3).
* **`trackerid` goes after `event`**, as in Transmission's `announce_url_new`
  (`libtransmission/announcer-http.cc`: `...&compact=1&supportcrypto=1`, then
  `requirecrypto`, `corrupt`, `event`, `trackerid`). The 4.1.3 capture shows
  `ipv4` after `event`, so the opt-in `ipv4`/`ipv6` are last.
* **Encryption (`Crypto`, from `network.encryption`):** `prefer` adds
  `supportcrypto=1`, `require` adds `supportcrypto=1&requirecrypto=1`, `off`
  adds nothing, always right after `compact` and before `event`
  (`...&compact=1&supportcrypto=1&requirecrypto=1&event=started`). The
  capture only shows `supportcrypto`; `requirecrypto` follows it as in
  libtransmission's `announce_url_new` (written when its encryption mode is
  "required"). UDP announces (BEP 15) have no such field. The value is part of
  the hub's shared config: `TrackerHub::set_crypto` applies a settings change
  from each torrent's next announce (no extra announce, no schedule change,
  like `set_port`).
* **Encoding:** every byte except RFC 3986 unreserved `A-Za-z0-9-._~` is
  `%XX` with **upper-case** hex; never `+`; `*!'()` are escaped. Tested over all
  256 bytes. `key` is 8 upper-case hex digits.
* **Deliberately not sent:** `corrupt`, `redundant`,
  `no_peer_id` (trackers ignore them and they are not needed; the
  fields no longer exist, so they cannot be sent by mistake), and `ip`.
  `ipv4=`/`ipv6=` are sent only when the user configured an address
  (`TorrentSpec::ipv4/ipv6`, default off).
* **Headers** (HTTP transport): `Host`, `User-Agent: Seedwell/<ver>`,
  `Accept: */*`, `Accept-Encoding: gzip`, in that order, title-cased, and no
  `Connection` header (HTTP/1.1 keep-alive). `transport::http_announce_parses_and_sends_exact_request`
  asserts the exact set and order. We advertise only `gzip` (not `deflate`)
  because that is all we decode.
* **Capture test:** `url::tests::matches_the_transmission_capture_modulo_identity_and_dropped_params`
  builds the full URL for the Debian info-hash and asserts it equals the
  Transmission 4.1.3 capture except `peer_id`, `key`, `port` and the dropped
  `ipv4`. `url::tests::crypto_parameters_follow_the_policy_right_after_compact`
  covers the three encryption policies.

Changing the wire format means editing that array (and, if the captures differ,
`build_announce_url`'s per-parameter formatting), and the tests above.

### Intervals

We schedule from the moment the response was received and never early. The SPEC
7C.1 clamps (`response::clamp_timing`, table-tested in `response::tests::clamp_table`,
idempotent): missing or non-positive `interval` becomes 30 min; `min interval`
is kept within 60 s to 24 h (non-positive means absent); without a `min interval`
the interval is raised to at least 5 min (the same 300 s floor libtorrent
applies; Transmission honours a smaller `interval` exactly, we deliberately do
not go below 300 s); with a `min interval` the interval is `max(interval, min
interval)`; the interval never exceeds 24 h. Manual re-announce waits for `min
interval` (300 s when the tracker gave none).

## Deliberate decisions and deviations

* **`completed` is at-most-once for everything the tracker could have received.**
  The flag is persisted when the request is *dispatched* (and the request is
  refused if that write fails). A `completed` that provably **never reached the
  tracker** is sent once more with the retry: `Dns`, `Connect`, `ConnectTimeout`
  (TCP connect or the UDP connection-id handshake), `Tls` (handshake, before the
  request is written), `Forbidden` (destination policy) and `NotSent` (the
  durable write failed, so it was never released). The machine rolls the flag back
  (durably, and the gate's auditor forgets it) and the retry carries `completed`
  again. **Ambiguous** failures do not: a timeout after connecting, a reset after
  the request was written, a 5xx, garbage: the tracker may have counted it, so
  the next announce just carries `left=0`. A double `completed` is a ban risk, a
  missed one is not. A `completed` that was *owed* but never dispatched (crash,
  stop) is sent in the next session, still at most once. The transports only
  return the "never reached" classes for errors that happened *while
  connecting* (`hyper`'s `is_connect()`, UDP before the announce datagram), and
  a late success for the same request cancels the resend.
* **`failure reason` policy.** Permanent-looking reasons halt until the user acts
  (`response::PERMANENT_REASONS`, matched case-insensitively as substrings:
  "unregistered torrent", "torrent not registered", "not registered",
  "torrent not found", "not found", "unknown torrent", "info hash not found",
  "banned client", "client is banned", "client banned", "banned", "invalid
  passkey", "passkey invalid", "passkey is invalid", "bad passkey", "unknown
  passkey", and BEP 31 `retry in: never`). Every other reason (maintenance,
  "announce too frequently", "try again later") is retried on the slow schedule
  30 min → 1 h → 2 h → 4 h → 8 h → every 24 h (+0-20 % jitter, all roles) and
  surfaced as a `Warning` (and a `Failed` with the retry time). A user action
  (`Start`, `Resume`, `manual_reannounce`) over a halt or a slow streak retries,
  never sooner than 60 s after the last attempt. The table is short on purpose:
  a wrongly halted torrent costs a click, a wrongly retried one costs one
  request per 30 min to 24 h.
* **`trackerid` is per session**, never persisted (Transmission and libtorrent
  keep it in memory only); the first `started` of a process carries none.
* **Redirects:** only to the same host (case-insensitive), same scheme or
  http→https, ≤ 3. Stronger than SPEC (which allows other hosts of the same
  scheme): a cross-host redirect would bypass the per-host budget and, for
  private torrents, invariant 7. Redirect hops are paid for via `charge_extra`.
* **UDP retransmission** is `15·2ⁿ s` with `n` capped at 4 (not 8): 465 s per
  exchange, 540 s total per announce, below the gate's 600 s UDP deadline. A
  longer wait would pin one of a host's two request slots for an hour.
* **Host circuit breaker:** opens when ≥ 3 *different* torrents fail on a host
  with no success in between within 10 minutes (pause 60 s, doubling, ≤ 1 h
  nominal + 20 % jitter; then a single half-open probe). `failure reason`
  rejections do not count: the host is alive.
* **Empty bucket at start** (`GateConfig::initial_tokens = 0`): the burst of 3
  exists only after the host has been idle; a process that restarts in a loop
  therefore cannot exceed the sustained rate. Cost: the first announce of a run
  waits ≤ 1 s.
* **Restart after a crash** waits for the persisted `min_allowed_at` (capped at
  1 h) before `started`, but only when the previous run did not end with a
  confirmed `stopped`.
* **Quit-time `stopped` for hundreds of torrents on one host** goes out together (50 at a
  time over all hosts, 2 s answer wait, whole quit bounded by the engine's 10 s), because a
  `stopped` sent at quit is the one request that must not wait in line behind a pacing rule
  made for a long-running client: the process is leaving, and trackers expire stale peers
  anyway. Before this the token bucket (burst 3 + 1/s) and the 120 s linger delivered about
  one `stopped` per second and abandoned the rest (92 of ~250 on the 500-torrent storm test).
  The regular cap, bucket, intervals and backoff are untouched.
* **Peers larger than the cap** (200) are truncated, not discarded.
* **Late success after a timeout** is accepted while nothing newer was sent for
  that (torrent, tracker) and the session is still open; otherwise ignored.
* Wake/network-change **never** causes an early regular announce, not even
  after a "manual-like" delay: a healthy machine ignores them. Only announces
  that already failed are rescheduled: a *network-class* backoff (DNS, connect,
  timeout) is shortened to ~1–16 s but never below 30 s after the last answer,
  and the failure counter is kept. A storm of events cannot postpone announces
  forever: the jitter window they open is not extended by further events and a
  new one opens at most once per `wake_jitter`.
* **A halt after an accepted `started`** (the tracker rejects a later announce)
  still gets one best-effort, gated `stopped` when the torrent is stopped, so the
  next session's `started` is not a repeat. A halt on the very first `started`
  sends nothing.
* **A tripped machine** (`Health::Tripped`: a self-audit or the gate refused)
  never sends again but ends its session on `Stop`/`Pause`, so the torrent can be
  removed and re-added.
* **Destination policy** (SSRF): see "Rules for the engine". HTTP IP literals are
  rewritten to their canonical dotted-quad / bracketed form before connecting,
  so no legacy spelling (`0177.0.0.1`, `2130706433`) is interpreted differently by
  the OS resolver than by our policy check.
* **Scrape gap:** no more often than the tracker's last announce interval and
  never below 30 min; before the first answered announce, 1 h.
* **Memory.** One idle (added, not started) pair costs **257 B** of heap, down
  from 2,851 B (`crates/engine/tests/scale.rs` "trs-tracker hub alone"; asserted
  by `tests/memory.rs`). A pair of a 3-tracker torrent costs 189 B, a seeding
  pair about 680 B. How: exact-size slices instead of growing vectors; the
  machine splits into a 104-byte core and a session block that exists only while
  a session is open; one shared configuration object and one shared URL object
  per distinct value; the gate's auditor lives in the registry entry (16 bytes
  idle); no per-torrent store keys, scratch buffers or random generators; the
  gate gives its queue capacity back after a start-up burst. A distinct passkey
  URL adds the URL text itself (~150 B).
* **Cleanups visible in the API:** `AnnounceMachine::config()` returns a
  `MachineConfig` by value (its `seed` is reported as 0; jitter comes from a
  per-draw generator seeded by the info-hash, the request counter and the time),
  `AnnounceMachine::last_success()`/`regular_at()` are `None` when no session is
  open, `TrackerSet::tiers()` returns a `Tiers` view (`iter()` yields `&[u16]`),
  `FailureClass` has a `Slow` variant, `TransportError` gained `ConnectTimeout`,
  `Forbidden` and `NotSent`.

## Developer log hooks (SPEC 8.7)

The crate writes no log itself; it only exposes what the engine's developer log needs, all of it free when unused:

* `HttpOutcome::trace` (`HttpTrace`: the address connected to, the last status, whether the other address family was tried) and `HttpTransport::request_headers(url)` (the exact header lines that go on the wire); `UdpTransport::announce_traced` (`UdpTrace`: address, fallback, connection id and whether it came from the cache, transaction id). A few `Copy` fields, filled either way.
* `TrackerHub::set_trace(true)` makes the hub record decisions that send nothing as `HubNote`s (`take_notes()`): `SessionOpened` (with the tier and position), `Retire` / `RetireSilent` (BEP 12 failover), `Suspend` (quit), `KeptSchedule` / `RetryPulled` (network change or wake), `Deferred` / `Released` (the rate gate held a request back, and for how long), `Refused`. Off, the hub takes no snapshots and keeps nothing; `tests/hub_trace.rs` covers both.

## Dependencies (each justified)

| Crate | Why |
|---|---|
| `trs-bencode` | response parsing (workspace crate) |
| `tokio` (`rt`, `net`, `time`, `sync`, `io-util`) | UDP sockets, timeouts, lookup (transports only) |
| `hyper`, `hyper-util`, `http-body-util`, `bytes` | the HTTP client: connection pool, `HttpConnector` (connect timeout, happy eyeballs), body size limit |
| `hyper-rustls` (`ring`, `webpki-roots`, `tls12`) | TLS via rustls with full certificate verification (webpki roots); the `ring` provider avoids a C toolchain dependency |
| `flate2` (`rust_backend`) | gzip responses, size-bounded |
| `tower-service` | the `Service` trait of hyper's connector layer, for our vetting resolver (already in the tree through hyper-util) |
| `getrandom` | UDP transaction ids from the OS RNG |
| dev: `proptest`, `stateright`, `sha1` | property tests, model checking, info-hash for the network test |

The sans-I/O core builds with `--no-default-features` (only `trs-bencode`).
`#![forbid(unsafe_code)]`.
