# trs-sync-net

The folder-sync driver (SPEC §7G): devices talk to each other, folders run, and the app, the CLI and agents
get one API, `SyncHandle`. The rules for *what* to sync live in `trs-sync`; the wire format, keys and roles in
`trs-sync-proto`. This crate moves bytes and runs the loops. `#![forbid(unsafe_code)]`.

It is a compartment of its own: its own port, its own state (`sync.toml`, `sync/<folder>.redb`), its own
tasks. The torrent engine only lends it DHT lookups on public topics and a port mapping (through the
`PublicDiscovery` trait and the control crate's glue), and nothing in the torrent path depends on it.

## A session

```
dialer                                   acceptor
  TCP connect ─────────────────────────▶
  "SWSYNC1\n"  ────────────────────────▶ (magic: session, relay or punch)
  TLS 1.3, both sides present their Ed25519 device key as a raw public key (RFC 7250), ALPN seedwell-sync/1
  Hello ◀──────────────────────────────▶ Hello            (client "Seedwell x.y.z", device name)
  Join (folders it believes are shared,
        with its grants, or an invitation) ▶
                                       ◀ Join (only folders the dialer proved), or Close with the
                                         reason an invitation was refused
  per shared folder: Members, then Index (full, or the changes since what the peer holds)
  then, both ways: Index updates as files change, Request/Response for chunks, Ping when idle
```

- **Identity** is the TLS key. The verifiers accept any Ed25519 key and check only the handshake signatures;
  which devices are welcome is decided by each folder's membership (`roles.rs`): the peer's key must have a role,
  or it redeems an invitation (bound to the session by the TLS exporter value) with an owner or admin, which
  admits it and signs its grant.
- **Membership** lives beside the core (`FolderShared::membership`, kept in `sync/<folder>.members`), so the UI
  and the sessions read it without waiting for a pull. When it changes, `roles::changed` updates the core's
  writers and this device's access, cuts off a device that is no longer a member (after telling it), sends the
  statements on, re-signs admins' admissions on an owner, and gives the folder secret to new co-owners.
- **Privacy:** the dialer proves first. The acceptor answers only for folders the dialer proved, so a
  stranger who connects learns nothing about which folders this device has.
- **Serving a chunk:** only from a file our index holds as a valid file with exactly that chunk at that
  offset, and only if the bytes still hash right.
- **Two queues per session:** control (index, requests) before data (responses), so a big transfer never
  holds up index updates. At most 32 MiB of a peer's requests are read at once.
- **Compression:** index, membership, address and join messages over 1 KiB are deflated; file data is not.
  Inflating is capped at the frame limit (a compression bomb is refused).

## A folder

`runtime.rs`: the watcher (`trs-fswatch`) starts first, then a full stat scan (unchanged files are not
read), then a pull. After that: watcher batches → scans; a peer's index change → pull rounds until nothing
more can be done; paths that changed under the puller are scanned and pulled again; a jittered hourly
full scan; retries with backoff (10 s doubling to 5 min) while data is missing or steps fail. Every local
change wakes the sessions sharing the folder, which send the new entries.

Disk work runs on blocking threads, with the folder's `FolderCore` locked only there.

## Finding devices

- devices met before, at the addresses they had (`sync.toml` `[[device]]`), and addresses typed in;
- peer exchange: each device sends its own signed address record and passes on others' (signed, so
  passing them on cannot forge them);
- the DHT: each folder's daily topic is announced and looked up every 15 minutes (not for LAN-only or
  paused folders);
- LAN discovery (`lan.rs`, below).

Dial attempts back off per address (5 s doubling to about 5 min).

### LAN discovery

Every 30–60 s (jittered), at start and when a folder is added, the service sends one UDP packet to port
**27183**: to the IPv4 limited broadcast `255.255.255.255`, to the IPv4 group **239.192.83.87**
(organization-local scope) and, when the sync listener takes IPv6 (`bind` is `::`), to the link-local IPv6
group **ff12::5357**. A service bound to one address (tests, on loopback) sends only to `Options::lan_targets`.

```
offset  size  field
 0       8    magic "SWDISC1\0"
 8      32    device public key (Ed25519)
40       8    instance id, u64 BE (random per start, so a restart shows)
48       2    sync TCP port, u16 BE
50       4    Unix hour (UTC), u32 BE
54       8    nonce (random per packet)
62       1    tag count n (≤ 150)
63      8n    tags: lan_tag(hour, nonce) per folder (trs-sync-proto keys.rs)
63+8n   64    Ed25519 signature by the device key over everything before it
```

At most 1327 bytes. With more than 150 folders, each packet carries the next 150 in turn.

- **Receiving:** wrong size or magic, our own device, over 32 packets/s, a bad signature or an hour more
  than one off our own: dropped. Then one HMAC per local folder (at most 256); a match records the device as
  a member of that folder at `source IP : announced port` and wakes the dialer. The dial expects that
  device's key in TLS, so a replayed or spoofed packet gets nothing. A new instance id clears the backoff.
- **Privacy:** without a folder's key a listener cannot tell which folders a packet names, whether two
  devices share one, or link one packet's tags to the next (new nonce each time). It does see the device
  key, the port and the number of folders announced.
- **Sockets** use SO_REUSEADDR and SO_REUSEPORT, so the app, a daemon and tests can all listen on the port.
  Multicast groups are joined again every round (the default interface changes with the network).
- **macOS:** broadcast and multicast need the Local Network permission; the app's Info.plist must carry
  `NSLocalNetworkUsageDescription`. Without it, sends fail (EHOSTUNREACH or EPERM): logged once per
  destination, retried at the next round, never in a loop.
- Not yet: one packet per interface (only the default interface's broadcast and multicast), and an announcement
  on network change (the next round covers it).

## Transports, hole punching and relays

A session runs over any byte stream (`session::BoxIo`), always with the magic and TLS end to end, so a
relay sees only ciphertext.

- **TCP and uTP:** the service binds TCP and its UDP twin on the same port number; the UDP socket speaks uTP
  (`trs-utp`, LEDBAT) and hands everything else (hole-punch probes) to `punch.rs`. A LAN address is dialed
  over TCP only; any other over TCP and uTP at once, and the first finished handshake wins.
- **Reaching a member that direct dials do not reach** (`relay::reach`, from the dial loop, 2 s after the
  direct attempts, backing off 3 s doubling to 5 min per device):
  - **relay servers** (`relay_server.rs`): `relay://host:port#<relay id>` in a folder's addresses makes the
    service keep a registration there (and name it in its address record); members are joined through it.
    The protocol is in the module doc; `run_relay` is the server behind `seedwell relay` (stateless, caps on
    waiting registrations, pairs, rate and idle time).
  - **hole punching** (`punch.rs`, BEP 55 style): `Rendezvous` to a connected peer C that shares a folder
    with the member; C sends `Introduce` to both with candidates and a common start time; both send
    `SWPUNCH1` probes, then the lower device id dials uTP.
  - **member relay** (`relay.rs`): if that brings no session, `RelayOpen` to C. C relays when
    `relay_own_devices` is on and both proved Write access to a folder C holds with Write access, or when
    `relay_everyone` is on and both share a folder with C; never a relayed session (no chains), at most 16
    pipes (2 per pair), 2 MiB/s per pipe and direction, a window of 64 × 16 KiB per direction with
    end-to-end acknowledgements, so C never buffers more than about 2 MiB per pipe. The UI shows such a
    session as `Relay`.

## Keys

`secrets.rs`: the device key, an owner's folder secret, a joining device's invitation and the links this device
made live in a `SecretStore`: the Keychain in an app build that may use it, else a 0600 file. Never in
`sync.toml`, the synced folder or a log.

## Dependencies

| Crate | Why |
|---|---|
| `trs-sync`, `trs-sync-proto`, `trs-fswatch` | the core, the wire format, the watcher |
| `trs-utp` | uTP on the sync port's UDP twin (Internet sessions, hole punching); Seedwell's own, already used by the engine |
| `tokio` | the driver (minimal features, as elsewhere) |
| `rustls`, `tokio-rustls` (ring) | TLS 1.3 with raw public keys; already in Seedwell for HTTPS trackers |
| `ring` | random ids, SHA-256 when serving; already linked through rustls |
| `flate2` (Rust backend) | deflate for metadata messages; already used by the tracker crate |
| `serde`, `toml` | `sync.toml`; already used by `trs-state` |
| `socket2` (`all`) | SO_REUSEPORT on the LAN discovery socket before it binds (std and tokio cannot set it, and `unsafe` is forbidden); already built with `all` through tokio's `net` |

## Tests

`tests/it`: whole services on loopback with temp state and temp folders: two devices syncing both ways
over TLS, a read-only device whose edits stay local, two devices pulling from each other at once. Roles
(`roles.rs`): a one-use link admits one device and tells the next why not; an admin invites but cannot change
roles or invite admins; a removed device is cut off and its changes ignored; a co-owner receives the folder key
and changes roles; joining waits while no owner or admin is online. LAN discovery
(`lan.rs`): two services that find each other only through announcements (unicast on loopback, own
ports) and sync; badly signed announcements, other folders' tags and stale hours lead to no dial. uTP
(`utp.rs`): two services with TCP dialing off sync both ways over uTP. Relays (`relay.rs`, with a test switch
that refuses direct sessions between two devices): A and B reach each other through C's member relay and
the snapshot says `Relay`; C refuses when the access rules are not met and relays once `relay_everyone` is
on; two devices registered at a `run_relay` server sync a file through it.
