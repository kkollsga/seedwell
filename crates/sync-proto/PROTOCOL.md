# Seedwell folder sync: the wire protocol

This is the protocol two Seedwell devices speak to sync folders (SPEC §7G). It covers the framing, every
message, membership statement, invitation and proof, the stored formats, and the rules for changing any of
them. The code is the reference: `src/codec.rs` (framing), `src/msg.rs` (messages), `src/members.rs`
(statements, invitations, links), `src/sign.rs` (signed file versions), `src/keys.rs` (folder keys, proofs).

**0.1.6 is the baseline.** It is the first release with folder sync, and every later version must still
sync with it until a major version change drops it (see [Evolution](#evolution)). The bytes 0.1.6 writes are
frozen in `tests/it/golden.rs`: each message type, statement kind, invitation link and proof, decoded and
encoded back byte for byte. That table only ever gets new entries.

## Contents

- [Transport](#transport)
- [Encoding](#encoding)
- [The session](#the-session)
- [Messages](#messages)
- [Index entries and signed versions](#index-entries-and-signed-versions)
- [Membership statements](#membership-statements)
- [Invitations, links and proofs](#invitations-links-and-proofs)
- [Other formats](#other-formats)
- [Stored formats](#stored-formats)
- [Evolution](#evolution)
- [Checking compatibility](#checking-compatibility)

## Transport

- A device listens on one TCP port (and runs uTP on the same UDP port). Before anything else, the connecting
  side sends 8 bytes of magic:
  - `SWSYNC1\n`: a sync session; TLS follows;
  - `SWRELAY1`: the relay protocol (`crates/sync-net/src/relay_server.rs`);
  - `SWPUNCH1`: a hole-punch probe.
- **TLS 1.3**, mutual, with Ed25519 raw public keys (each device's key; its device id is the SHA-256 of the
  public key) and ALPN `seedwell-sync/1`.
- Both sides take a 32-byte TLS exporter value with the label `EXPORTER-seedwell-sync-v1`. Folder proofs are
  bound to it, so a proof cannot be replayed in another session.
- Inside TLS everything is a **frame**:

  ```text
  frame = len u32 BE (of what follows, 2 ..= 16 MiB + 64) ‖ type u8 ‖ flags u8 ‖ payload
  ```

  Flag bit 0 says the payload is deflate-compressed (the net layer inflates it before decoding). The other
  flag bits are reserved and ignored.

## Encoding

- Integers are LEB128 varints (at most 10 bytes, 64 bits). Signed integers (mtimes) are zigzag varints. A
  field that must fit 32 bits is refused when it does not.
- Keys, hashes, ids and signatures are fixed-size raw bytes.
- A string is `varint length ‖ UTF-8`; a byte string is `varint length ‖ bytes`; a bool is one byte, 0 or 1.
- A list is `varint count ‖ items`. Every count and length is checked against its cap and against the bytes
  left before anything is allocated.
- **A list of records** (anything that may grow) is `varint count ‖ n × (varint length ‖ record)`. Each record
  is read from its own bytes: what follows the fields a version knows is ignored, and a record holding a value
  it does not know (a file kind, a role, file flags) is skipped instead of failing its message. A record that
  is cut short still fails the message.
- Encodings are canonical: the shortest varint, bools 0 or 1. A device passes signed bytes on as it got them,
  so only canonical signed bytes survive a round trip through an older device.

## The session

1. **Hello** both ways, at once. Each side works out what to speak from the two Hellos:

   ```text
   agreed protocol = min(my protocol, their protocol)
   it must be >= my min_protocol and >= their min_protocol, else the session ends
   agreed features = my features AND their features
   ```

   When the ranges do not overlap, each side sends a Close and says why: "Mac X runs an older Seedwell that
   cannot sync with this one: update it" on the newer side, "Mac X runs a newer Seedwell that this one
   cannot sync with: update this Mac" on the older one. A joining device shows that as the reason it waits;
   a folder the other device is a member of lists it with its issues until a session with it succeeds.

2. **Join.** The dialing side proves first, for the folders it believes the other device shares. The other
   side answers only for folders the dialer proved, so a stranger who connects learns nothing about which
   folders a device has. A listener with nothing to share sends a Close instead (with an invitation's refusal,
   `link: <reason>`).
3. Then, per shared folder: Members (the statements), the Index (full or the changes since what the peer
   holds), and from then on index updates, Requests and Responses, Progress and Addresses, both ways. A quiet
   session sends a Ping every 90 s; one silent for three times that is dead.
4. Either side ends it with a Close (a reason for the other side's log or, for a joining device, its UI).

## Messages

`type`: the frame type. Fields are in order. Every message ignores bytes after its last field.

| type | message | fields |
|---|---|---|
| 1 | Hello | `protocol` varint (highest spoken) ‖ `client` str (`Seedwell 0.1.6`, ≤ 128 B) ‖ `device_name` str (≤ 64 characters) ‖ `min_protocol` varint ‖ `features` varint. Without the last two: `min_protocol = protocol`, `features = 0`. |
| 2 | Join | records of folders, each: `folder_id` (32) ‖ proofs (count ‖ n × proof, ≤ 8) ‖ `my_index_id` varint ‖ `your_index_id` varint ‖ `your_max_seq` varint |
| 3 | Members | `folder_id` (32) ‖ records of statements (≤ 10 000). A statement this version cannot frame (a signer kind it does not know) is left out. |
| 4 | Index | `folder_id` (32) ‖ `index_id` varint ‖ `full` bool ‖ records of index entries (≤ 1000, about 1 MiB) ‖ `done` bool (the last batch of a full send) |
| 5 | Request | `id` varint (u32) ‖ `folder_id` (32) ‖ `path` str ‖ `offset` varint ‖ `len` varint (u32, one chunk) ‖ `hash` (32, what the bytes must hash to) ‖ `from_temp` bool (read from the sender's download in progress) |
| 6 | Response | `id` varint ‖ `code` u8 (0 ok, 1 failed, 2 no such file, 3 invalid, 4 busy; any other code means failed) ‖ `data` bytes (≤ 16 MiB) |
| 7 | Progress | `folder_id` (32) ‖ records of updates (≤ 1000), each: `path` str ‖ version ‖ `have` (count ‖ chunk indexes, varint each; empty: forget this file) |
| 8 | Addresses | records of address records (≤ 256), each: `device_pk` (32) ‖ `addrs` (count ‖ str, ≤ 32) ‖ `relay` str ‖ `issued_s` varint ‖ `sig` (64) |
| 9 | Ping | (empty) |
| 10 | Close | `reason` str (≤ 256 B) |
| 11 | Rendezvous | `target` device id (32) ‖ `candidates` (count ‖ str): asks a common peer to introduce the sender to `target` |
| 12 | Introduce | `from` device id (32) ‖ `candidates` (count ‖ str) ‖ `punch_at_ms` varint: both sides punch at that time |
| 13 | RelayOpen | `target` device id (32) ‖ `session` varint: a member relay pipe to `target` |
| 14 | RelayData | `session` varint ‖ `data` bytes (≤ 16 MiB) |
| 15 | RelayClose | `session` varint |
| 16 | OwnerKey | `folder_id` (32) ‖ `secret` (32, the folder secret `S`): from an owner to a device whose grant says Owner, only inside TLS |
| other | (unknown) | skipped |

- **Version** (in entries and progress): `count ‖ n × (short device id, 8 bytes BE ‖ counter varint)`, sorted by
  id, no repeats, no zero counters, at most 256.
- **Address record** signature: Ed25519 by `device_pk` over
  `"seedwell-sync/v1/addresses" ‖ device_pk ‖ u32 BE n ‖ n × (u32 BE len ‖ addr) ‖ u32 BE len ‖ relay ‖ u64 BE issued_s`.
  A newer record replaces an older one.

## Index entries and signed versions

An index entry, inside its record:

```text
sequence varint ‖ entry flags u8 (bit 0: invalid, the announcer's copy differs; other bits ignored)
‖ path str ‖ kind u8 (0 file, 1 directory, 2 symlink) ‖ size varint ‖ mtime_ns zigzag varint
‖ file flags u8 (bit 0 executable, bit 1 deleted; others refused: the record is skipped)
‖ version ‖ modified_by (8, BE) ‖ file_id (16) ‖ symlink_target str ‖ has_conflict_of bool [‖ conflict_of str]
‖ chunks (count ≤ 400 000 ‖ n × (len varint ≤ 16 MiB ‖ SHA-256 (32))) ‖ signature (64)
```

The signature is the writer's (the device `modified_by` names) Ed25519 signature over a fixed encoding of its
own, so the wire form can change without invalidating signatures:

```text
"seedwell-sync/v1/file" ‖ folder_id ‖ u32 BE len ‖ path ‖ kind u8 ‖ size u64 BE ‖ mtime_ns i64 BE ‖ file flags u8
‖ u16 BE n ‖ n × (short id u64 BE ‖ counter u64 BE) ‖ modified_by u64 BE ‖ file_id ‖ u32 BE len ‖ symlink_target
‖ (0 | 1 ‖ u32 BE len ‖ conflict_of) ‖ SHA-256(u32 BE n ‖ n × (u32 BE len ‖ hash))
```

`sequence` and the entry flags belong to the announcing device and are not signed. Stores keep entries in the
bare form above (without the record length): see [Stored formats](#stored-formats).

## Membership statements

A folder's membership is the set of signed statements its devices know; each device works out the same roles
from them (`Membership` in `src/members.rs`).

```text
statement = kind u8 ‖ body (varint length ‖ bytes) ‖ signer ‖ signature (64)
signer    = 0 (the folder key: an owner) | 1 ‖ device public key (32) (an admin)
signed    = "seedwell-sync/v2/statement" ‖ folder_id ‖ kind ‖ body (with its length) ‖ signer
```

| kind | statement | body | who may sign |
|---|---|---|---|
| 1 | grant | `device` pk (32) ‖ `role` u8 (1 owner, 2 admin, 3 read & write, 4 read only) ‖ `serial` varint ‖ `at_s` varint ‖ `has_invite` bool [‖ invitation (varint length ‖ bytes)] | owners; an admin only with the invitation it admits through |
| 2 | remove | `device` pk (32) ‖ `serial` varint ‖ `at_s` varint | owners |
| 3 | invite | invitation (published, so every owner and admin can show and count it) | its issuer |
| 4 | revoke | invitation id (16) ‖ `at_s` varint | owners and admins |
| 5 | share until | `until_s` varint (0 = no end) ‖ `serial` varint ‖ `at_s` varint | owners |

- A kind a device does not know, or a body it cannot read (a role it does not know), is kept as it came,
  checked by its signature only, stored, passed on unchanged, and gives no role.
- Bytes after a body a device knows are kept with the statement, so its signature still checks and it is
  passed on unchanged; the fields the device knows count.
- Each device judges times (`at_s`, expiries, the share end) by its own clock.

## Invitations, links and proofs

```text
invitation = version u8 (1) ‖ folder pk (32) ‖ id (16) ‖ role u8 ‖ max_uses varint (0 = unlimited, ≤ 1000)
             ‖ expires_s varint (0 = never) ‖ at_s varint ‖ issuer (signer) ‖ token pk (32) ‖ signature (64)
signed     = "seedwell-sync/v2/invite" ‖ everything before the signature
link       = seedwell://sync/<base32(invitation without token pk ‖ token seed (32) ‖ check (2))>?name=<name>
check      = SHA-256("seedwell-sync/v2/link" ‖ what comes before it)[..2]
```

- An invitation never grows: a new form is a new invitation version. A device refuses a link of a version it
  does not know ("this link was made by a newer Seedwell: update Seedwell to use it"), a statement holding
  one is kept as unknown, and an owner or admin that cannot read a redeemed one tells the joining device that
  it runs an older Seedwell.
- Query fields other than `name` are ignored.

**Proofs** (in a Join, per folder): `kind u8 ‖ bytes`.

| kind | proof | bytes |
|---|---|---|
| 4 | grant | a statement (the sender's grant, or the grant of the admin that signed it) |
| 5 | redeem | invitation ‖ the token key's signature of `"seedwell-sync/v2/redeem" ‖ folder_id ‖ the joining device's pk ‖ TLS exporter` |
| other | (unknown) | carried, never trusted |

A member also sends the owners' latest share-until statement as a grant proof, so a device that was cut off
when the share ended learns of an extension when it reconnects.

## Other formats

- **LAN announcements** (`crates/sync-net/src/lan.rs`): a fixed-size signed UDP packet that starts with the magic
  `SWDISC1\0`. A new layout is a new magic; a newer device sends both for as long as it supports the older
  version.
- **DHT topics and LAN tags** (`src/keys.rs`): `SHA-1("seedwell-sync/v1/dht" ‖ folder pk ‖ day u32 BE)` and
  `HMAC-SHA256(lan key, "seedwell-sync/v1/lan-tag" ‖ hour u32 BE ‖ nonce)[..8]`.
- **The relay protocol** (`crates/sync-net/src/relay_server.rs`): its own magic `SWRELAY1`, versioned the same
  way.

## Stored formats

- **The membership file** (`sync/<folder id>.members`): `"SWMB" ‖ format varint (1) ‖ n × (varint length ‖
  statement)`, every statement as it came (unknown kinds too). A file of a later format is refused ("written by
  a newer Seedwell"). A folder of the test builds before 0.1.6 (a `SWM2` membership file, or a stored link
  without the invitation version) is forgotten at start; its files stay.
- **`sync.toml`**: `format = 1` at the top (a file without it is format 1). Keys a version does not know are
  dropped when it rewrites the file, so a later version raises `format` only when an older one would misread the
  file; the older one then refuses it.
- **The index store** (redb, `crates/sync/src/store_redb.rs`): its own schema version and migration rule, as
  before. Entries are kept in the bare entry form.

## Evolution

What a later version may change, and how:

1. **Append fields.** A message, a record in a list, or a statement body may get new fields at its end. Older
   devices ignore them (statements keep them, so their signatures still check). A new field must be safe to
   ignore; if it is not, it needs a feature bit or a new type or kind.
2. **New message types, statement kinds and proof kinds.** Older devices skip unknown message types, keep
   and pass on unknown statement kinds without giving them any meaning, and carry unknown proofs without
   trusting them.
3. **New values of an existing enum** (a file kind, a role, a file flag): a record holding one is skipped by
   older devices, so a newer device sends it only to peers that understand it, by an agreed feature bit or
   protocol version. Response codes are the exception: an unknown one means failed.
4. **Feature bits** (`FEATURES` in `src/msg.rs`) are for optional behaviour both sides must support: a session
   uses `my features AND theirs`. A new behaviour takes the next free bit, documented in `src/msg.rs` and in a
   table here; bits are never reused. None is defined in 0.1.6.

   | bit | feature | since |
   |---|---|---|
   | (none yet) | | |

5. **Breaking changes** raise `PROTOCOL`, the highest version spoken. The new build keeps `MIN_PROTOCOL` one
   behind for at least one release (N-1), so devices a release apart still sync at the older version, and
   raises it later. Two devices whose ranges do not overlap refuse each other and say which needs an update.
6. **Invitations and links** never grow: a new form is a new invitation version (the first byte of an
   invitation and of a link's payload).
7. **Signed bytes** never change for an existing type: a new signed form needs a new domain string
   (`seedwell-sync/v3/...`).
8. **Stored formats** change by raising their format number, with a migration where the old data must be kept.

## Checking compatibility

- `tests/it/golden.rs`: the frozen 0.1.6 vectors, and the rules above (appended fields, grown records, unknown
  kinds and values, newer links, a peer with a newer protocol and features).
- `crates/sync-net/tests/it/versions.rs`: two services on loopback, one speaking a newer protocol range with
  unknown features (they sync), and one that only speaks a newer version (refused, with the reason shown).
- `tools/sync-compat.sh OTHER`: this build's daemon against another `seedwell` binary, on loopback with
  temporary state: share both ways (each side the owner in turn), sync files, change a role, remove a device.
  The CLI tests run it with this build on both sides. From 0.1.7 on, CI should also run it against the latest
  release's CLI (`Seedwell.app/Contents/MacOS/seedwell`).
