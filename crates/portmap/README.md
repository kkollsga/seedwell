# trs-portmap

Makes the listen port reachable from the Internet: **PCP** (RFC 6887), **NAT-PMP**
(RFC 6886) and **UPnP IGD** (`WANIPConnection:1/2`, `WANPPPConnection:1`) port mapping.
It maps TCP and UDP for the listen port (SPEC: Network settings, "port mapping").
IPv4 only. `#![forbid(unsafe_code)]`.

A **sans-I/O core** (`PortMapper`) does all protocol work, with time injected as
`Millis`; a **small tokio driver** (`driver::spawn`) wires it to one UDP socket, short TCP
exchanges and one timer.

```rust
use trs_portmap::driver::{spawn, DriverConfig};
use trs_portmap::{Config, Wanted};

let (portmap, mut events) = spawn(DriverConfig {
    core: Config { wanted: Wanted::tcp_and_udp(listen_port), ..Config::default() },
    fixed_net: None,                   // None = find the default gateway (see below)
});
let s = portmap.snapshot();            // method, external_ip, mappings[{protocol, internal_port, external_port, ..}]
// report the TCP mapping's external_port to trackers/peers; watch changes with portmap.watch()
portmap.network_changed().await;       // call on every network change (the engine already has this event)
portmap.shutdown().await;              // MUST be awaited at quit: deletes the mappings (<= 3 s)
```

`Event`s (`Mapped`, `Renewed`, `Lost`, `SlotFailed`, `GatewayRestarted`, `MethodSelected`,
`ExternalIp { public }`, `NoMethod`, ...) are for the UI and logs; the snapshot is the source of truth.
`ExternalIp.public == false` means the "external" address is private or CGNAT (100.64/10):
a mapping there is useless, so tell the user.

## Politeness rules (all enforced in the core, all tested on a virtual clock)

| Rule | Where / test |
|---|---|
| **One outstanding exchange** with the gateway at any time (mappings are done one after another) | `core::pump`; `natpmp_discovery_maps_tcp_and_udp_serially` |
| NAT-PMP retransmits at **250 ms doubling, at most 9 transmissions**, then gives up (RFC 6886 3.1). Discovery probes use fewer (4) so a silent router costs 3.75 s, not 128 s | `natpmp_retransmit_schedule_is_250ms_doubling_nine_tries` (0, 250, 750, 1750, 3750, 7750, 15750, 31750, 63750 ms; fails at 127750 ms) |
| PCP retransmits at 3 s doubling with +-10 % jitter (RFC 6887 8.1.1), 3 transmissions in discovery, 4 afterwards | `pcp_retransmit_schedule_is_3s_doubling_with_jitter` |
| **Renew at half the granted lifetime** (the grant, not what we asked for) | `renewal_happens_at_half_the_lifetime`, `shorter_grants_shorten_the_renewal_...` |
| A router that grants tiny lifetimes cannot make us spam it: **>= 60 s between requests for one mapping** (`min_renew_gap`) | same test, 10 s grant |
| **Back-off on errors**: 1 min doubling to 1 h, +0-10 % jitter, never shorter than a PCP `lifetime` retry hint; "network failure"/"out of resources" wait >= 5 min; "not authorized" waits >= 6 h | `network_failure_backs_off_exponentially_to_the_cap`, `refused_means_one_try_per_six_hours_not_a_storm`, `pcp_error_lifetime_is_a_retry_hint` |
| **At most one discovery per network change.** When every method fails the machine goes quiet (no timers at all) until the next `network_changed` with a *different* gateway/address, or an explicit `retry()`. Discoveries are >= 30 s apart, so flapping Wi-Fi is debounced | `everything_failing_stops_after_one_discovery_...`, `rapid_network_flaps_are_debounced_...`, `losing_the_network_silences_everything` |
| UPnP discovery is **4 datagrams total** (2 targets x 2 rounds 1.5 s apart), and stops at the first valid answer | `upnp_*`, `no_igd_on_the_network_gives_up_quietly` |
| The external address is re-read at most hourly (NAT-PMP/UPnP) | `renewal_happens_at_half_the_lifetime` |
| **Gateway reboot**: NAT-PMP/PCP epoch must advance at >= 7/8 of our clock (+2 s slack, RFC 6886 3.6); otherwise all mappings are re-created, asking for the same external ports | `gateway_reboot_is_detected_by_the_epoch_...`, `pcp_gateway_reboot_is_detected_too` |
| **Shutdown deletes** every mapping (NAT-PMP/PCP: lifetime 0; UPnP: `DeletePortMapping`) within 3 s, even if the gateway is gone; dropping all handles does the same | `shutdown_*` |
| Hostile gateway (random silence, error codes, reboots, short grants, garbage) for 3 simulated days x 20 seeds: **<= 12 packets in any 60 s, <= 300 in any hour**, <= 8 SSDP datagrams/hour | `hostile_gateway_cannot_make_us_exceed_the_packet_budget` |

Method order is PCP, then NAT-PMP, then UPnP (`Config::methods`). A NAT-PMP-only gateway
answers a PCP request with "unsupported version" immediately, so there is no waiting for a
PCP timeout there. NAT-PMP/PCP never fall back to UPnP unless the earlier methods fail.

## Security: every answer is untrusted input

* Datagrams count only from `gateway:5351` (the port is a config field for tests). Anything
  else is dropped. PCP replies must echo the 96-bit nonce, protocol and internal port; NAT-PMP
  replies must match opcode, protocol and internal port.
* UPnP: `LOCATION` comes from a multicast reply that any LAN device can forge. It is used
  only if it is a literal IPv4 `http://` URL (no DNS, no https, no userinfo), **the host is the
  default gateway** and equals the answering address (`allow_foreign_igd` is off by default; if
  enabled the host must still be private/link-local). The `controlURL` must stay on the same
  host. The service type is matched against three exact URNs before it goes into a header.
* Size caps everywhere: HTTP response 128 KiB (headers + body), XML 256 KiB / depth 32 /
  20 000 events / 4 KiB text, 32 services kept, datagrams 64 B (NAT-PMP) / 1100 B (PCP) / 4 KiB (SSDP),
  `route` output 16 KiB.
* Parsers return `Result`/`Option`; no `unwrap`, no unchecked indexing on untrusted data. Fuzz
  targets in `fuzz/` (`udp_packets`, `upnp_xml_soap`, `http_gateway_misc`, and `core_inputs`, which
  scripts datagrams, HTTP answers, ticks, network changes and shutdown into the whole machine).
  Each ran 40 s clean when this crate was written (10 M, 1.3 M, 2.0 M and 0.8 M executions).

## Default gateway on macOS without unsafe code

`gateway::detect` runs `/sbin/route -n get default` (absolute path, cleared environment,
3 s timeout, 16 KiB output cap) and parses `gateway: a.b.c.d`; on Linux it runs `ip -4 route show
default` so CI works. The local address comes from `connect()`ing a UDP socket to the gateway,
which only consults the routing table and sends nothing. Why not a crate: the options
(`default-net`, `netdev`, `if-addrs`, `system-configuration`) pull in `libc`/`core-foundation`
FFI layers and a lot of code for one fact, and a pure text parser is trivially fuzzed. The
cost is one process spawn per network change, which is rare. Tools that print no `gateway:`
(a VPN default route on a `utun`, an on-link route) yield "no gateway" and the driver reports
`NetworkLost`; PCP/NAT-PMP/UPnP are not attempted through tunnels.
The engine should call `network_changed()` from its existing reachability/wake events; this
crate has no polling timer by design.

## Dependencies (each justified)

| Crate | Why |
|---|---|
| `tokio` (`rt`, `net`, `time`, `sync`, `io-util`, `macros`), *driver feature only* | UDP/TCP sockets, timers, channels. The sans-I/O core and all parsers build with `--no-default-features` and have no dependencies. |

No XML crate: the UPnP subset (element names, text, entity decoding, bounded) is ~150 lines
in `xml.rs`, never expands DTD entities (a `DOCTYPE` with an internal subset is refused) and is
fuzzed; a general XML library adds a large API surface for no benefit here. No HTTP crate: IGDs
answer with bare `\n` line ends, wrong `Content-Length`, chunked bodies and unclosed connections,
which strict clients reject; `http.rs` is a lenient, capped response parser plus two request
builders, and the driver just does connect/write/read-until-complete.

## Tests

| Layer | File | What |
|---|---|---|
| Unit | `src/*` | byte-exact NAT-PMP/PCP layouts (RFC examples), every result code, truncation and garbage, epoch rule, SSDP, description + SOAP parsing, XML limits and hostile input, HTTP quirks (bare LF, chunked, truncated), `route` output |
| Virtual time | `tests/core_virtual.rs` | the core against pure mock gateways (`tests/common`): schedules, renewals over 25 h, back-off, error codes, reboots (NAT-PMP and PCP), spoofed sources, nonce checks, fall-through, quiet-after-failure, shutdown (also with a dead gateway), UPnP quirks (725 permanent leases, 718 conflicts, 724, foreign LOCATION/controlURL, PPP-only, chunked), packet budget under a hostile gateway |
| Loopback | `tests/{natpmp,pcp,upnp}_loopback.rs` | the tokio driver against real UDP/TCP servers on 127.0.0.1: mapping, error codes, renewal, reboot, shutdown deletes, dead-gateway shutdown, SSDP responder + SOAP server |
| Real router (`#[ignore]`) | `tests/real_router.rs` | read-only: NAT-PMP external-address request, PCP ANNOUNCE (opcode 0), SSDP + `GetExternalIPAddress`. **Creates no mappings.** `cargo test -p trs-portmap --test real_router -- --ignored --nocapture` |
| Fuzz | `fuzz/` | see above (`cargo +nightly fuzz run <target>`) |

## Not done / limits

* IPv4 only. PCP PEER/third-party/FILTER options, PCP over IPv6 and IPv6 pinholes (UPnP
  `WANIPv6FirewallControl`, PCP v6) are not implemented.
* NAT-PMP's unsolicited external-address announcements (multicast `224.0.0.1:5350`) are not
  listened to; an external IP change is noticed at the hourly re-read or the next renewal.
* Mappings made on a network we have left are not deleted (that gateway may be unreachable);
  they expire after the lease (<= 2 h; UPnP "permanent" leases are the router's business).
* A mapping that may be in flight at the moment of a shutdown during *discovery* is not
  deleted; it lapses with its lease.
* Double NAT: the default gateway's IGD is used; an upstream IGD needs
  `Config::allow_foreign_igd`.
* macOS may require the "Local Network" privacy permission for SSDP multicast and LAN TCP;
  without it discovery simply times out (`NoMethod`).
