# trs-sync-proto

Seedwell Sync (protocol 2): everything two devices must agree on to sync a folder (SPEC §7G). The folder key and
what is derived from it, roles and membership, invitations, device ids, the signed file model and the wire
messages. **Sans-I/O**: no sockets, no files, no clocks;
randomness comes in through a trait (`Random`, implemented for ring's `SystemRandom`) so simulations are
deterministic. `#![forbid(unsafe_code)]`. The sessions, TLS and compression live in `trs-sync-net`; the scanner,
index and puller in `trs-sync`.

| Module | What |
|---|---|
| `model` | version vectors, `FileInfo`, `SignedFile`, `IndexEntry`, `validate_path` (Syncthing's rules) |
| `ids` | `DeviceId`, `FolderId`, `ShortId`; Syncthing's checked text form |
| `keys` | `FolderSecret`, `FolderKey` (owner or member), `Access`, `Proof`, DHT topics and LAN tags |
| `members` | `Role`, signed `Statement`s, `Membership` (the roles they give), `Invite`, `InviteLink`, redeem proofs |
| `device` | `DeviceKey` (Ed25519), `verify` |
| `sign` | `file_payload`, `chunks_hash`, `sign_file`, `verify_file`, `WriterTable` |
| `codec`, `msg` | frames, magics, the primitive encodings, every message |
| `base32`, `random` | RFC 4648 base32 (lenient decoding), the randomness trait |

## Identities and keys

**Device id** (Syncthing's format, so it reads the same): SHA-256 of the device's Ed25519 public key, base32 (52
characters), four groups of 13 each followed by a Luhn mod-32 check character, shown as 8 groups of 7:
`MFZWI3D-BONSGYC-YLTMRWG-C43ENR5-QXGZDMM-FZWI3DP-BONSGYY-LTMRWAD`. The Luhn variant is a port of Syncthing's
`luhn.go` (it is not textbook Luhn: the factor starts at 1 on the left). Parsing ignores case, spaces and `-`, reads
`0 1 8` as `O I B`, and requires every check character; the old 52-character form is refused. `Debug` prints the
first group only. Folder ids use the same form.

**Folder authority:** a folder's secret `S` gives its Ed25519 signing key; only owner devices hold it. Every member
knows the public half `pk`; the folder id is `SHA-256(pk)`. No key is shared: devices join with invitation links.

**Invitation link:** `seedwell://sync/<base32(invite without its token key ‖ token seed 32 ‖ check 2)>?name=<name>`,
`check = SHA-256("seedwell-sync/v2/link" ‖ payload)[..2]` (so a typo is caught; the name is only a hint, ≤ 255
bytes). Parsing takes surrounding whitespace, `<…>` quotes from mail and `+` for a space, ignores other query fields,
and refuses the bare `SWW-`/`SWR-` keys of the test builds (`KeyError::OldKey`).

Links, invitations and secrets have hand-written `Debug` that prints the invite id or folder id, never secret material.
The link text comes only from `format()`, never from `Display`, so a link cannot end up in a log line by accident.

## Derivation

`H(k, label)` = HKDF-SHA256, empty salt, ikm `k`, info `label`, 32 bytes.

| Value | Derivation | Who can compute it |
|---|---|---|
| S | 32 random bytes | owners |
| folder signing key | Ed25519 from seed `H(S, "seedwell-sync/v1/sign")`; `pk` its public half | owners (`pk`: all) |
| folder id | `SHA-256(pk)` | all |
| lan_key | `H(pk, "seedwell-sync/v1/lan")` | all |
| `dht_topic(day)` | `SHA-1("seedwell-sync/v1/dht" ‖ pk ‖ day u32 BE)`, day = Unix days UTC | all |
| `lan_tag(hour, nonce8)` | `HMAC-SHA256(lan_key, "seedwell-sync/v1/lan-tag" ‖ hour u32 BE ‖ nonce8)[..8]` | all |

## Membership

Roles: **Owner** (holds `S`: changes roles, removes devices, invites at any role), **Admin** (invites at Read & write or
Read only), **Read & write**, **Read only** (receives, never sends its own). A folder's membership is a set of signed
statements (wire forms in `members.rs`):

| Statement | Says | Signed by |
|---|---|---|
| grant | device `pk` has a role; `serial` orders an owner's statements; may carry the invitation it came from | the folder key, or an admin (only with an invitation, for a device no owner decided on) |
| remove | the device is out | the folder key |
| invite | an invitation, published so owners and admins can count and show it | its issuer |
| revoke | an invitation admits nobody from now on | the folder key or an admin |

`Membership::add` checks every signature; the roles follow deterministically from the statements, in any order:
owners' latest statement per device (higher serial, then greater signature) decides; admins' grants count for devices
no owner decided on (earliest by time, then signature); an invitation used more often than its limit keeps its
earliest admissions and the rest count as removed. An owner signs admins' admissions again (`to_confirm`), so they
stay if that admin is later demoted.

## Proofs (after TLS)

TLS authenticates each device's key. In `Join`, each side sends `Proof::Grant`s (the statements that make it a
member: its grant, and an admin's own grant if an admin signed it), or, when joining, one `Proof::Invite`: the
invitation and the token's signature of `"seedwell-sync/v2/redeem" ‖ folder_id ‖ device_pk ‖ X`, where `X` is the
32-byte TLS exporter value with label `EXPORTER-seedwell-sync-v1` (`keys::EXPORTER_LABEL`). So a redeem proof is
bound to the device and the session. Only an owner or admin admits (`Membership::check_admit`): issuer still an
owner or admin, not expired, not revoked, uses left.

## Signed file versions

```text
payload = "seedwell-sync/v1/file" ‖ folder_id(32)
        ‖ str(path) ‖ u8 kind ‖ u64 size ‖ i64 mtime_ns ‖ u8 flags(bit0 exec, bit1 deleted)
        ‖ u16 n ‖ n × (u64 id, u64 value)          (version)
        ‖ u64 modified_by ‖ file_id(16) ‖ str(symlink_target) ‖ u8 has_conflict_of ‖ [str(conflict_of)]
        ‖ chunks_hash(32)    where chunks_hash = SHA-256(u32 n ‖ n × (u32 len ‖ hash32))
str(s) = u32 BE length ‖ UTF-8 bytes; every integer big-endian; kind 0 file, 1 dir, 2 symlink.
```

`sign_file(device_key, folder_id, info)` and `verify_file(folder_id, device_pk, &signed)`. A `WriterTable` maps
`ShortId → Writer { device_pk, cert }`; `insert` checks the certificate against the folder `pk` and computes the
short id from the key itself, so `verify(&signed)` is one signature check against the writer that `modified_by`
names. `IndexEntry::sequence` and `IndexEntry::invalid` belong to the announcing device and are not signed.

## Wire format

Before TLS the connecting side sends 8 bytes: `SWSYNC1\n` (session), `SWRELAY1` (relay protocol) or `SWPUNCH1`
(hole-punch probe), `codec::Magic`.

Inside TLS: frames of `u32 BE len ‖ u8 type ‖ u8 flags ‖ payload`, `len` counting everything after itself, at most
16 MiB + 64. Flag bit 0 = deflate-compressed payload (the net layer inflates; `decode` refuses a frame still
flagged); other bits are reserved and ignored. An unknown type decodes to `Message::Unknown` and is skipped. Bytes
after a message's last field are ignored, so later versions can append fields.

Payload encoding: LEB128 varints (signed: zigzag); hashes, keys, signatures and short ids fixed-size; strings and
byte strings `varint len ‖ bytes`; lists `varint count ‖ items`; bools one byte, 0 or 1.

| type | message | fields |
|---|---|---|
| 1 | Hello | protocol, client str (≤ 128 B), device_name str (≤ 64 chars) |
| 2 | Join | folders ≤ 1024: [folder_id 32, proofs ≤ 8: [u8 kind ‖ bytes], my_index_id, your_index_id, your_max_seq] |
| 3 | Members | folder_id 32, statements ≤ 10 000: [bytes ≤ 1024 B: a statement] |
| 4 | Index | folder_id 32, index_id, full bool, entries ≤ 1000: [IndexEntry], done bool |
| 5 | Request | id, folder_id 32, path str, offset, len, hash 32, from_temp bool |
| 6 | Response | id, code u8 (0 ok, 1 generic, 2 no such file, 3 invalid, 4 busy; unknown → generic), data ≤ 16 MiB |
| 7 | Progress | folder_id 32, updates ≤ 1000: [path str, version, have ≤ 400 000: [chunk index]] |
| 8 | Addresses | records ≤ 256: [device_pk 32, addrs ≤ 32: [str ≤ 256 B], relay str, issued_s, sig 64] |
| 9 | Ping | (empty) |
| 10 | Close | reason str (≤ 256 B) |
| 11 | Rendezvous | target device_id 32, candidates ≤ 32: [str] |
| 12 | Introduce | from device_id 32, candidates ≤ 32: [str], punch_at_ms |
| 13 | RelayOpen | target device_id 32, session |
| 14 | RelayData | session, data ≤ 16 MiB |
| 15 | RelayClose | session |
| 16 | OwnerKey | folder_id 32, S 32 (an owner to a co-owner, inside TLS) |

An `IndexEntry` is `sequence ‖ u8 entry flags (bit0 invalid) ‖ path str ‖ u8 kind ‖ size ‖ mtime_ns (zigzag) ‖
u8 file flags ‖ version ‖ modified_by 8 ‖ file_id 16 ‖ symlink_target str ‖ bool [conflict_of str] ‖ chunks ≤
400 000: [len ‖ hash 32] ‖ sig 64`. A version is `count ≤ 256 ‖ n × (short id 8 BE ‖ value)`, and must be sorted,
unique and non-zero. Paths are ≤ 4096 bytes, chunks ≤ 16 MiB. Unknown file-flag bits are refused (the signature
could not cover them); unknown entry-flag bits are ignored. Proof kinds are 4 grant and 5 invite (≤ 1024 bytes);
other kinds are carried as `Proof::Unknown` and never trusted. `Hello.protocol` is 2; a peer with another one is
refused.

Every cap is checked while decoding, before allocating, and so is the claim itself: a count of `n` items needs at
least `n ×` (smallest item) bytes left in the frame, so no frame makes the decoder reserve more than the frame
already holds. The decoder checks structure only; paths (`validate_path`), signatures and proofs are the
receiver's, so one bad entry can be skipped without dropping its batch. The address record signature covers
`"seedwell-sync/v1/addresses" ‖ device_pk ‖ u32 n ‖ n × str(addr) ‖ str(relay) ‖ u64 issued_s` (same style as
the file payload).

## Testing

`tools/check.sh -p trs-sync-proto`. Unit tests sit next to the code; integration tests are one binary
(`tests/it/`).

* Test vectors: Syncthing's Luhn vectors (`luhn_test.go`) and two Syncthing device ids (the documentation example
  and `deviceid_test.go`), RFC 4648 base32, RFC 5869 HKDF (case 3, empty salt), RFC 8032 Ed25519 (test 1); a
  byte-exact `Hello` frame and `file_payload` layout.
* Keys and links: the derivation matches the published labels; folder signatures verify only with the folder's
  key; Debug never shows secrets; links roundtrip, survive mail quoting, and refuse typos, damage and bare keys.
* Ids: every single-character change of a device id is caught.
* Membership: statements roundtrip and verify; only owners remove or grant without an invitation; every order of the
  same statements gives the same roles; admins invite below Admin and cannot change roles; a removed device is out
  and its later versions do not count; limits, expiry and revocation hold; two admins over a limit keep the same
  earliest device everywhere; a redeem proof is bound to the device and the session.
* Signatures: a one-bit change in any signed field (and in the signature) breaks it; the writer table refuses a
  forged `modified_by` and drops a removed writer.
* Wire: proptest roundtrips for every message, payload/frame agreement, frame streams, and random or damaged input
  (bit flips, truncation) decoded without panics; huge claimed counts refused without allocating.
* Fuzz: `fuzz/` (own `[workspace]`, not part of the Seedwell workspace) has `msg_decode`: frame splitting plus
  decoding of every type, and decode → encode → decode must give the same message.
  `cd crates/sync-proto/fuzz && cargo +nightly fuzz run msg_decode`.

## Dependencies

* `ring` 0.17 (default features off, `std`): Ed25519 (device and folder keys), HKDF-SHA256 (the derivation),
  HMAC-SHA256 (proofs, LAN tags, constant-time compare), SHA-256 (ids, key checks, chunk lists) and SHA-1 (DHT
  topics, which must be 20 bytes like an info hash). It is already linked through rustls, so sync adds no crypto
  crate.
* dev: `proptest` (properties).
* No `base32`, `data-encoding` or `percent-encoding` crates: base32 is ~60 lines and checked against RFC 4648,
  percent-encoding of one query field is ~30. No `zeroize`: secrets live in ordinary arrays (the Keychain is where
  they are protected at rest).
