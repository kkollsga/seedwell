# Seedwell folder sync: the wire protocol

This is the protocol two Seedwell devices speak to sync folders and to chat in channels (SPEC §7G). It covers the
framing, every message, membership statement, invitation, proof and channel message, the stored formats, and the
rules for changing any of them. The code is the reference: `src/codec.rs` (framing), `src/msg.rs` (messages),
`src/members.rs` (statements, invitations, links), `src/sign.rs` (signed file versions), `src/keys.rs` (folder
keys, proofs), `src/chat.rs` (channel messages and topics), `src/direct.rs` (private messages).

**0.1.6 is the baseline.** It is the first release with folder sync, and every later version must still
sync with it until a major version change drops it (see [Evolution](#evolution)). The bytes 0.1.6 writes are
frozen in `tests/it/golden.rs`: each message type, statement kind, invitation link (a link for a folder and its
channel too), proof, channel message and private message, decoded and encoded back byte for byte. That table only ever gets new entries.

## Contents

- [Transport](#transport)
- [Encoding](#encoding)
- [The session](#the-session)
- [Messages](#messages)
- [Index entries and signed versions](#index-entries-and-signed-versions)
- [Membership statements](#membership-statements)
- [Invitations, links and proofs](#invitations-links-and-proofs)
- [Channels](#channels)
- [Private messages](#private-messages)
- [Your devices](#your-devices)
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

2. **Join**, then, when both have the chat feature bit, **ChatJoin**, then, when both have the direct bit,
   **DirectJoin** (a private conversation the session may stand for, see [Private messages](#private-messages)),
   then, when both have the devices bit, **DevicesJoin** (the user's own devices, see [Your devices](#your-devices)).
   The dialing side proves first, for the folders and channels it believes the other device shares. The other side answers only for those the dialer
   proved, so a stranger who connects learns nothing about which folders or channels a device has. A listener
   with nothing to share sends a Close instead (with an invitation's refusal, `link: <reason>`).
3. Then, per shared folder: Members (the statements), the Index (full or the changes since what the peer
   holds), and from then on index updates, Requests and Responses, Progress and Addresses, both ways. Per shared
   channel: Members, ChatHave, the ChatMessages the other lacks, and new messages as they come. A quiet
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
| 17 | ChatJoin (chat bit) | records of channels, each: `channel_id` (32) ‖ proofs (as in Join) |
| 18 | ChatHave (chat bit) | `channel_id` (32) ‖ `floor_at_ms` varint (older messages are not wanted: the sender dropped them; 0 = all) ‖ records of authors (≤ 1024), each: `author` pk (32) ‖ ranges (count ≤ 4096 ‖ n × (`start` varint ‖ `len` varint ≥ 1)), sorted |
| 19 | ChatMessages (chat bit) | `channel_id` (32) ‖ records of channel messages (≤ 1000, about 1 MiB) |
| 20 | DirectKeys (chat bit) | records of key records for private messages (≤ 1024) |
| 21 | DirectHave (chat bit) | records of authors as in ChatHave: what the sender holds of the private messages to it |
| 22 | DirectMessages (chat bit) | records of private messages (≤ 1000, about 1 MiB) |
| 23 | DirectJoin (direct bit) | `conversation` bool ‖ `user_name` str (≤ 64 characters): sent once while a session starts, after ChatJoin (the dialer first), and again when the user renames itself |
| 24 | HoldQuery (holds bit) | `id` varint (u32) ‖ `folder_id` (32) ‖ files (count ≤ 1000 ‖ n × (`path` str ‖ version ‖ `content` (32, a digest of kind, deleted, exec, size, link target and chunks))): does the receiver hold these versions now (an on-demand device about to free its copies) |
| 25 | HoldReply (holds bit) | `id` varint ‖ held (count ‖ n × bool), in the query's order |
| 26 | RenameRequest (renames bit) | `id` varint (u32) ‖ `folder_id` (32) ‖ `from` str ‖ `to` str ‖ `seen` (32, a digest of the paths and versions at and below `from` the sender has) ‖ `ttl_ms` varint (u32): rename for the sender, as the receiver's own change, only if it can start within `ttl_ms` of receiving it |
| 27 | RenameReply (renames bit) | `id` varint ‖ `outcome` u8 (0 done, 1 refused, 2 busy: could not start in time, and never will) ‖ `reason` str (≤ 512 B) |
| 28 | DevicesJoin (devices bit, 0.1.13) | records of devices groups, each: `group_id` (32) ‖ proofs (as in Join): the sender's group, and, to a device of its current group, each group it moved on from (its grant there and the Moved statements, as grant proofs); sent once while a session starts, after DirectJoin (the dialer first, only to a device it believes is one of its own or, joining, to whoever it dialed for the group; the other side answers only for a group the dialer proved), and later for a group made, joined, confirmed or moved to meanwhile; a device moving to a new group proves nothing there (no proofs: its members know its key); empty when there is nothing to prove |
| 29 | BlobWant (blobs bit, 0.1.15) | `id` varint (u32) ‖ `scope` u8 (0: ‖ `channel_id` (32), a channel's message names it; 1: a private message between the two devices of this session names it, asked only over a direct session) ‖ `hash` (32: an image's SHA-256, a file's piece table's SHA-256, or a file's SHA-256 for one of its pieces) ‖ `offset` varint ‖ `len` varint (u32, ≤ 1 MiB): a part of an image or piece table, or one piece of a file (`offset` a multiple of 1 MiB, `len` the piece's length) |
| 30 | BlobData (blobs bit, 0.1.15) | `id` varint ‖ `code` u8 (0 ok, 1 not held, 2 refused, 3 gone: the sender's file changed, moved or is no longer shared, for good) ‖ `total` varint (the blob's or file's size) ‖ `data` bytes (≤ 1 MiB; empty unless ok) |
| other | (unknown) | skipped |

- **Version** (in entries and progress): `count ‖ n × (short device id, 8 bytes BE ‖ counter varint)`, sorted by
  id, no repeats, no zero counters, at most 256.
- **Address record** signature: Ed25519 by `device_pk` over
  `"seedwell-sync/v1/addresses" ‖ device_pk ‖ u32 BE n ‖ n × (u32 BE len ‖ addr) ‖ u32 BE len ‖ relay ‖ u64 BE issued_s`.
  A newer record replaces an older one.

## Index entries and signed versions

An index entry, inside its record:

```text
sequence varint ‖ entry flags u8 (bit 0: invalid, the announcer's copy differs; bit 1: dataless, the announcer has this
version without its content; bit 2: on demand, the announcer may let go of its copy later; other bits ignored)
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
| 6 | profile (0.1.9) | `device_name` str (1 to 64 characters, no control characters) ‖ `user_name` str (empty = none, else as `device_name`) ‖ `serial` varint ‖ `at_s` varint | the device it names, while a member; its latest (by `serial`, then signature bytes) replaces the older ones |
| 7 | channel (0.1.12) | `flags` varint (bit 0: open to agents, 0.1.12's agent channel; other bits: none yet, kept and ignored) ‖ `at_s` varint | owners; no longer written since 0.1.13 |
| 8 | agents (0.1.13) | `open` u8 (0 = No, 1 = Yes; another value: the body is unknown) ‖ `serial` varint ‖ `at_s` varint | owners (channel key) and admins (device key); the latest of the owners' and current admins' (by `serial`, then `at_s`, then signature bytes) counts |
| 9 | follow (0.1.13) | `target` u8 (1 folder, 2 channel; another value: unknown) ‖ `target_pk` (32) ‖ `name` str (≤ 255 B, no control characters) ‖ `on` u8 (0 or 1; another value: unknown) ‖ `serial` varint ‖ `at_s` varint [‖ `flags` varint, 0.1.14: written only when not 0; bit 0 on demand, bit 1 removed] | in a devices group only: a member, with its device key; per target the latest of a current member's (by `serial`, then `at_s`, then signature bytes) counts. Anywhere else it means nothing. 0.1.13 reads a Follow with flags as the same Follow with bytes after it (kept, signed, passed on) |
| 10 | moved (0.1.13) | `target_pk` (32) ‖ `at_s` varint | in a devices group only: a member, with its device key: the group moved to the group with this key (its key changed after a removal). Of the current members' moved statements the lowest target group id counts |
| 11 | rendezvous (0.1.13) | `url` str (≤ 200 B, no spaces or control characters; empty: none) ‖ `serial` varint ‖ `at_s` varint | in a devices group only: the device it is about, with its device key, while a member: the rendezvous (§7G.3) it publishes to; its latest (by `serial`, then signature bytes) counts. A device that never said one takes the newest non-empty one of another member; one that turned it off says empty and takes none |

- A kind a device does not know, or a body it cannot read (a role it does not know), is kept as it came,
  checked by its signature only, stored, passed on unchanged, and gives no role.
- Bytes after a body a device knows are kept with the statement, so its signature still checks and it is
  passed on unchanged; the fields the device knows count.
- Each device judges times (`at_s`, expiries, the share end) by its own clock.
- **Names.** A device's name, as every member shows it (member lists, chat authors, conflict copies), is its latest
  profile in the folder's or channel's membership; a name a device learned another way (a session's Hello) only
  stands in until a profile arrives. A profile lives in its folder's or channel's membership, so it goes when that
  goes. Devices before 0.1.9 keep a profile from an admin or owner as unknown and pass it on; others' they leave
  out, and newer devices pass those on themselves.
- **Agents: Yes/No** (0.1.13). Whether a channel is open to agents on every member device is the latest agents
  statement (kind 8) of an owner or a current admin; the next one takes a `serial` above every one known. An
  owner's channel statement with flag bit 0 (0.1.12's agent channel, signed with the channel key when an agent made
  it) counts as a Yes at serial 0, so any agents statement comes after it. No statement: No. A channel made with
  Yes gets an owner's agents statement (serial 1); one made with No gets none.
  - 0.1.12 read flag bit 0 as set for good and opened such a channel to its agents unless its user closed it; it
    keeps an agents statement as an unknown kind and passes it on (an admin's too, while that admin is one there),
    so to it a channel is open exactly while it carries the kind 7 mark: closed to its agents for a channel made or
    switched on since, open even after a later No for one it marked. Nothing breaks either way.
  - Devices before 0.1.12 keep both as unknown kinds, store them, pass them on, and see an ordinary channel.

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
- Query fields other than `name` (and, in a folder's link, `chat`, `chat_name` and `chat_agents`; in a channel's,
  `agents`; in all, `a`) are ignored.
- **A link to pair a device** (0.1.13): `seedwell://devices/<the same>?name=<the device's name>[&a=…]`, an invitation
  of the devices group made by a device (issuer: its device key, role Read & write, one use, 15 minutes). A folder's
  or a channel's reader refuses it, and its reader refuses theirs; older versions do not know the prefix ("not a
  Seedwell link").
- **`agents=yes`** (0.1.13) in a channel's link (`chat_agents=yes` for the channel of a folder's link) says the
  channel was open to agents when the link was made, so the joining device lets its agents use the channel while
  it waits to be admitted. Only a hint: once admitted, the membership decides. Older versions ignore it.
- **A link for a folder and its channel** is one folder link that also carries the channel's invitation:
  `seedwell://sync/<the folder's>?name=<name>&chat=<the channel's base32, as in its own link>&chat_name=<its name>`.
  The two invitations are separate (each in its own membership, with the same role, uses and expiry); the joining
  device checks both and joins both, or only the folder when the channel is there already. A damaged `chat` part,
  or one naming the folder itself, makes the whole link damaged. A channel's link ignores a `chat` field.
- **`a=<base32>`** (0.1.13, last in the link): where the device that made the link can be reached, sealed so that
  only someone with the whole link can read it (`src/reach.rs`):

  ```text
  sealed  = version u8 (1) ‖ nonce (12) ‖ ChaCha20-Poly1305(key, nonce, aad = invite id, plain)
  prk     = HKDF-SHA256-Extract(salt = "seedwell-sync/v2/addresses", ikm = token seed (32))
  key     = HKDF-Expand(prk, "key" ‖ invite id, 32)
  nonce   = HMAC-SHA256(HKDF-Expand(prk, "nonce" ‖ invite id, 32), plain)[..12]   (deterministic, per plaintext)
  plain   = device id (32) ‖ name str (≤ 40 bytes) ‖ count u8 ‖ count × address ‖ field*
  address = kind u8 ‖ ip (4 | 16) ‖ port u16 BE    kind 4 / 6: IPv4 / IPv6 on a local network, 0x84 / 0x86: public
  field   = tag u8 ‖ str                           tag 1: the maker's rendezvous URL (≤ 200 bytes)
  ```

  At most 4 local addresses and 1 public one are read; an unknown address kind ends the list, and with it the
  fields. After a whole list, the fields are read: a known tag is kept, an unknown one skipped by its length, a
  damaged one ends them (0.1.12 reads no field: it ignores everything after the list). The rendezvous URL says where
  the maker publishes its address record (below, "Rendezvous records"): the joining device looks it up there when the
  addresses do not answer. A link for a folder and its channel has one block, sealed under the folder's token, for both. A block
  that does not open (damaged, another link's, another version) is left out and the link joins as one without it;
  older versions ignore the field. The joining device dials these addresses, expecting that device, alongside LAN
  discovery and the DHT.

**Proofs** (in a Join, per folder): `kind u8 ‖ bytes`.

| kind | proof | bytes |
|---|---|---|
| 4 | grant | a statement (the sender's grant, or the grant of the admin that signed it) |
| 5 | redeem | invitation ‖ the token key's signature of `"seedwell-sync/v2/redeem" ‖ folder_id ‖ the joining device's pk ‖ TLS exporter` |
| 6 | sibling (devices bit, 0.1.13) | the sender's devices group id (32): it joins a folder or channel that one of its own devices put on all of them. Its identity is the session's TLS key; an owner or admin admits it only when that key is a member of its own devices group with this id and the folder or channel follows there |
| other | (unknown) | carried, never trusted |

A member also sends the owners' latest share-until statement as a grant proof, so a device that was cut off
when the share ended learns of an extension when it reconnects.

## Channels

A channel is a folder without files: its own key pair (its id is the SHA-256 of the public key; owners hold the
secret), its own membership with the same statements, roles, invitations and sharing period, and links of their
own: `seedwell://chat/<the same payload as a folder link>?name=<name>`. Owners, admins and Read & write members
post; Read only members read. Channels need the chat feature bit (bit 0): a device without it never gets types
17 to 22, and one that has it ignores them over a session without it.

```text
message = fields (varint length ‖ bytes) ‖ signature (64)
  fields = id (16, random) ‖ author pk (32) ‖ seq varint ‖ at_ms varint ‖ has_reply bool [‖ reply_to id (16)]
           ‖ has_topic bool [‖ topic id (16)] ‖ agent str (empty = none, ≤ 64 characters) ‖ kind u8 ‖ body bytes
           ‖ (later fields)
  signed = "seedwell-chat/v1/message" ‖ channel id ‖ fields (with their length)
  kind   = 1 text (UTF-8, ≤ 16 KiB) | 2 json (UTF-8, ≤ 64 KiB) | 3 topic (its title: UTF-8, not blank, ≤ 200
           characters) | other: kept and passed on, not shown (≤ 64 KiB)
```

- The author's device signs; the agent label (the control client that posted it, set by the posting device's
  service from the client's `hello`, never by the client) is inside the signed bytes.
- `seq` counts an author's messages in the channel (1, 2, …), so a device's holdings are a few ranges per author
  (ChatHave). `(author, seq)` names one message: a second one with the same pair is not taken. A device that
  sees a higher `seq` of its own than it knows (it lost its store) continues after it.
- A message counts only if its author may post when it arrives (Owner, Admin or Read & write, while the sharing
  lasts): a removed or expired member's later messages are ignored; what devices took before stays.
- Order: `at_ms` (a message more than ten minutes ahead of the receiver's clock is placed at the time it arrived,
  and marked), then the author's `seq`, then the id.
- Every member keeps every message up to its cap (10 000 messages or 50 MB per channel, the oldest dropped
  first), and sends any member what it lacks: a device that was away catches up from whoever is online.
- A text message or JSON body that is too long or not UTF-8 makes its record unreadable: it is skipped.
- **Images** (0.1.15). A text message names its images in its later fields, signed with the rest; the bytes are
  blobs fetched by hash (BlobWant, BlobData; the blobs bit, 5):

  ```text
  later fields = n × (tag varint ‖ varint length ‖ bytes)             (a tag a version does not know is skipped)
  tag 1        = caption_len varint ‖ count varint (1..=10) ‖ count × (varint length ‖ image)
  image        = sha256 (32) ‖ kind u8 ‖ size varint ‖ width varint ‖ height varint ‖ name str (≤ 120 characters)
                 ‖ preview bytes ‖ (later fields)
    kind       = 1 PNG | 2 JPEG | 3 GIF | 4 WebP | 5 SVG | other: shown as an image this version cannot show
    preview    = a JPEG of at most 160 px a side and 12 KiB, or empty
  ```

  The body is the caption (its first `caption_len` bytes), then one line per image for versions without images
  (`[image: shot.png, 240 KB]`): a device of 0.1.14 or earlier shows that text and keeps and passes on the message
  as it came. The later fields are at most 48 KiB, so a message with a full text still fits every version's limit
  on a message's fields. Caps, on sending and on receiving: a raster image (PNG, JPEG, GIF, WebP) ≤ 10 MiB and 64
  megapixels, an SVG ≤ 1 MiB; the format is found from the bytes, never the name. Before sending, metadata (EXIF and
  GPS, XMP, IPTC, comments, text chunks, trailing data) is stripped by keeping only the parts of the file that draw
  the picture; a photo turned by its EXIF is turned and encoded again. An image that arrives must hash to its
  `sha256`, be the kind its message says, and decode; a peer that sends other bytes is passed over, an image that
  does not decode is not shown. A member serves an image only to a member of the channel, and only one that a
  message it keeps names. Members fetch by themselves the images of the newest messages, up to 512 MiB a channel;
  older ones when the user asks. An SVG is drawn on the device in Rust (no scripts, `foreignObject`, files, URLs,
  entities or nested SVG; bounded size, elements and time): hosts draw only the PNG it makes.
- **Image references** (0.1.18). A message's text (its caption) may point at one of its own images where the image
  belongs in the text, as a Markdown image whose destination is `att:` and the image's number, counted from 1 in the
  order the record lists the message's attachments: `![Image 2](att:2)` (1 to 99; anything else after `att:` is no
  reference). Nothing else on the wire changes: it is text. A version that knows it draws a chip there that shows
  that image; a reference to a number the message does not carry is drawn as its description, as plain text; 0.1.17
  and earlier show the description ("Image 2") as for any Markdown image. Senders add one only where the user put an
  image inside the text (pasting it there); an attachment added any other way, and `--attach`, add none.
- **Files** (0.1.15). Any other file, of any size, is an attachment of kind 16 in the same record:
  `sha256` is the whole file's, `width` and `height` are 0, there is no preview, and after the preview comes
  `pieces` (32), the SHA-256 of its piece table. The fallback line is `[file: report.pdf, 1.2 GB]`. Private
  messages carry the same record in the later fields of their sealed body (after the text; a version without them
  stops reading at the text), so its names and hashes are seen only by the two devices.

  ```text
  piece i     = bytes [i × 1 MiB, min((i + 1) × 1 MiB, size)) of the file      (an empty file has no pieces)
  piece table = SHA-256(piece 0) ‖ SHA-256(piece 1) ‖ …                         (32 × ceil(size / 1 MiB) bytes)
  pieces      = SHA-256(piece table)
  ```

  - **Sending.** The sender reads the file once (whole hash and piece table), keeps its path, size and modification
    time, and posts the message after that. It never copies the file.
  - **Serving.** A BlobWant for `pieces` is answered with the piece table; one for `sha256` must ask for exactly one
    piece (offset a multiple of 1 MiB, length the piece's), and is answered from the file where it is, only while the
    file has its recorded size and modification time and the piece read hashes to its entry in the table. If not,
    the sender answers 3 (gone) from then on, and so it does once its user stops sharing the file. A device that
    downloaded the file serves its copy the same way while it is unchanged, answering 1 (not held) once it is not.
  - **Downloading** (only when the user asks): the piece table first, from any device that has it, checked against
    `pieces` (and at most 256 MiB, a file of 8 TiB); then the missing pieces, four at a time from one device (the
    sender first), each checked against its entry and written at its offset in a partial file beside the
    destination. A piece that does not match is asked for again alone; a device that sends three is passed over.
    The verified pieces are recorded (*On disk*), so a download that stopped (the service quit, the devices parted)
    resumes from them. With nobody that has the file connected the download pauses until one connects; when the
    sender answered gone and nobody else sent the rest, the file is gone. A finished file is renamed into place
    (never over another file unless the user chose that file with Save As) and marked as downloaded for the system.
  - **Names.** The name is the sender's; a receiver uses its last part only, without control characters, `..`,
    leading dots or trailing dots and spaces, at most 120 characters (the extension kept), `file` when nothing is
    left; never a path.
- **Topics.** A topic message (kind 3) makes a topic: its id is the topic's id, its body the title; anyone who
  may post makes one. A message whose `topic` names it belongs to it; one without, or naming a topic this device
  does not have (yet), is shown under General, and moves into the topic when the topic message arrives. A topic
  message has no `topic` of its own (one that has is still a topic).

## Private messages

Two devices write to each other privately (`src/direct.rs`). Since 0.1.9 (feature bit 1, *direct*) a private
message travels only between its two devices, over a session between them that no other device relays; no device
keeps or passes on another's private messages or key records. A device of 0.1.8 (chat bit only) passes them on to
other members, so a device with the direct bit does not write to one.

```text
direct key = device pk (32) ‖ x25519 pk (32) ‖ at_ms varint ‖ signature (64)
  signed   = "seedwell-chat/v1/direct-key" ‖ device pk ‖ x25519 pk ‖ at_ms (u64 BE)
direct     = fields (varint length ‖ bytes) ‖ signature (64)
  fields   = id (16) ‖ author pk (32) ‖ recipient pk (32) ‖ seq varint ‖ at_ms varint ‖ ephemeral x25519 pk (32)
             ‖ sealed bytes ‖ (later fields)
  signed   = "seedwell-chat/v1/direct" ‖ fields (with their length)
  sealed   = ChaCha20-Poly1305(key, nonce 0, aad = "seedwell-chat/v1/direct" ‖ id ‖ author ‖ recipient, body)
  key      = HKDF-SHA256(salt "seedwell-chat/v1/direct", ikm X25519(ephemeral secret, recipient's x25519 pk),
                         info = ephemeral pk ‖ recipient's x25519 pk ‖ author ‖ recipient ‖ id), 32 bytes
  body     = has_reply bool [‖ reply_to id (16)] ‖ agent str ‖ kind u8 (as in channels) ‖ body bytes ‖ (later)
```

- **Keys.** A device's X25519 key is derived from its device key (`HMAC-SHA256(device key, "seedwell-chat/v1/x25519")`
  as the secret), so nothing more is stored; the record above carries its public half, signed by the device. When
  a session with the direct bit starts and no device relays it, each side sends its own record (DirectKeys with
  one record). A record of another device in DirectKeys is ignored.
- **Sealing.** Each message has a fresh ephemeral X25519 key, so a key is never used twice and the nonce can be
  fixed. Only the recipient can open it; the author keeps its own copy. A message that does not open for its
  recipient (changed, or sealed to another key) is dropped; so is one whose signature does not check.
- **Delivery.** A device takes a private message only from its author, to itself, over such a session; the author
  sends it only to the recipient. When the session starts, each side that keeps a conversation with the other says
  what it holds of the other's messages (DirectHave: ranges of `seq` per author); the other sends what of its own it
  lacks (a message written as an earlier session ended).
- **Conversations.** A device takes the first message of a conversation only from a device that is a member of a
  channel it is in; after that, the conversation stands on its own. DirectJoin's `conversation` says the sender
  keeps one with the receiver; when both say so, the session stands even with no folder or channel in common. The
  side that answers says so only when the dialer did, so a device that connects learns nothing it did not claim.
  Over a relayed session `conversation` is false.
- `seq` counts the author's private messages, to anyone (1, 2, …). Order and the cap are as for a channel (10 000
  messages or 50 MB per conversation, the oldest dropped first).

## Your devices

0.1.13, feature bit 4 (`devices`). The user's own devices form a **devices group**: a membership like a channel's
with no messages, whose key pair is the **user key** (every member is an owner and holds its secret, received with
OwnerKey). Its id, discovery topic and LAN tag come from its public key as for any folder.

- **Pairing.** The joining device redeems a pairing link in a DevicesJoin. Only the device that issued the
  invitation admits, and only once its user confirms (the two screens show the joining device's id): then it signs an
  owner grant carrying the invitation (one use) and sends the secret. Until then the session stays up with the group
  not shared over it. Refusing revokes the invitation.
- **Removal is final** in a devices group: a device an owner removed is never a member again, whatever grant comes
  later. Leaving signs the leaving device's own removal.
- **The key changes after a removal.** The removing device makes a new group (a new key): owner grants for the
  current members but the removed ones, the Follow statements signed again, and a **moved** statement (kind 10) in the
  old group naming the new key; it deletes the old key. A member that sees a current member's moved statement moves
  too (saying so in the old group), deletes the old key, and joins the new group with no proofs: the new group's
  members admit it by its TLS key, which their grants name, and send the new key with OwnerKey. A device that was
  away names the old group in its DevicesJoin; a device of the new group whose current group lists it answers with
  its grant there and the moved statements, and the away device moves. Groups a device moved on from are kept without
  their key (at most 8), never announced or looked up, and answered for only to devices of its current group. Two
  moves at once settle on the lowest target group id; a device that moved to another one moves to the winner (saying
  so in the group it leaves) and, holding the winner's key, removes there the devices removed in the group where they
  split, which changes the key again. Of a group a device moved on from, only moves made by devices of its current
  group count. A device that sees a removal in its group and no move changes the key itself (the others after a
  device left).
- **Follow** statements (kind 9) say which folders and channels are on all the user's devices. A member that does
  not have one joins it with no invitation and proves `sibling` (proof 6) to the members that are its siblings. An
  owner sibling signs `grant{device, admin}` with the folder key; an admin sibling makes a one-use invitation of its
  own (Read & write, one hour) and signs `grant{device, read & write, invitation}`, exactly an admin's admission. They
  do so when the group or the Follow changes (push) and when a sibling proves itself (pull); an owner raises a sibling
  it finds at Read & write to Admin. These are ordinary kind 1 statements: every member accepts them, also one without
  the devices bit, which never sees the group, proof 6 or type 28.
- A Follow's **flags** (0.1.14) come after `at_s`, only when not 0 (so a Follow without them is 0.1.13's bytes; a
  zero or a varint written longer than needed is not read as flags but kept after the body). **On demand** (bit 0):
  the folder is on demand on the device that put it on all devices, so a sibling joins it on demand where it can
  (a device that keeps no folders on demand joins it in full); it is the default at joining only, and each device
  switches its own afterwards. **Removed** (bit 1, with `on` 0): the folder's owner removed it from Seedwell, so each
  sibling removes it too, its files kept where they are (an on-demand folder's become an ordinary folder at its
  place). An off Follow without it only stops the folder coming to more devices. 0.1.13 reads either as the Follow
  without flags: it joins in full and keeps a removed folder.

## Other formats

- **LAN announcements** (`crates/sync-net/src/lan.rs`): a fixed-size signed UDP packet that starts with the magic
  `SWDISC1\0`. A new layout is a new magic; a newer device sends both for as long as it supports the older
  version.
- **DHT topics and LAN tags** (`src/keys.rs`): `SHA-1("seedwell-sync/v1/dht" ‖ folder pk ‖ day u32 BE)` and
  `HMAC-SHA256(lan key, "seedwell-sync/v1/lan-tag" ‖ hour u32 BE ‖ nonce)[..8]`.
- **The relay protocol** (`crates/sync-net/src/relay_server.rs`): its own magic `SWRELAY1`, versioned the same
  way.

## Rendezvous records

A device that uses a rendezvous (SPEC §7G.3: the user's own Worker on Cloudflare, `tools/rendezvous-worker`) keeps
one record there of where it can be reached (`src/rendezvous.rs`). Any device that knows its device id can find
and open it; the rendezvous, which sees only the record id, cannot.

```text
id        = SHA-256("seedwell-sync/v2/rendezvous/id" ‖ device id)          URL: /v1/r/<64 lower-case hex digits>
prk       = HKDF-SHA256-Extract(salt = "seedwell-sync/v2/rendezvous", ikm = device id)
key       = HKDF-Expand(prk, "key", 32)
record    = version u8 (1) ‖ nonce (12, random) ‖ ChaCha20-Poly1305(key, nonce, aad = version ‖ id, plain)
plain     = body ‖ signature (64)
body      = device pk (32) ‖ issued u64 BE (Unix ms) ‖ count u8 ‖ count × address (as in the link block) ‖ field*
signature = Ed25519 by the device key over "seedwell-sync/v2/rendezvous/record" ‖ id ‖ body
```

A reader checks that the key in the body is the device's (its SHA-256 is the device id) and that the signature
verifies, keeps the newest record it has seen of each device (an older one served again changes nothing) and uses
none older than 7 days. At most 8 local addresses and 1 public one; a record is at most 1024 bytes; fields of a later
version (`tag u8 ‖ str`, none yet) are skipped.

The Worker's API (v1; `tools/rendezvous-worker/README.md`): `GET /v1`, `GET /v1/r/<id>` (anyone; the records are
sealed), and `PUT` / `DELETE /v1/r/<id>` with `Seedwell-Key` (the device's **write key** for that rendezvous: a random
Ed25519 key in its secret store, never the device key), `Seedwell-Time` (Unix ms) and `Seedwell-Signature` (by the
write key over `"seedwell-rendezvous/v1\n" ‖ method ‖ "\n" ‖ id ‖ "\n" ‖ time ‖ "\n" ‖ hex(SHA-256(body))`). The
first write registers the write key (trust on first use); a write with another key is refused, and so is one whose
time is more than 5 minutes off or not later than the last write's. Records live 7 days after their last write.

Members learn where a device publishes from its signed address record (`Addresses`): an entry
`rendezvous+https://…` among its addresses, which versions before 0.1.13 skip (it does not parse as an address).

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
- **A channel's messages** (`sync/<channel id>.chat`): `"SWCH" ‖ format varint (2) ‖ n × (varint length ‖ sealed
  record)`, a record being `received_ms varint ‖ message`, sealed with the channel's own key on this device
  (ChaCha20-Poly1305, a random nonce first, the magic as associated data; the key is deleted with the channel).
  Appended to as messages arrive and written anew when old ones are dropped. A file of format 1 (0.1.8: the
  records in the clear) is sealed when opened; one of a later format is refused; a cut-off last record, or one
  that does not open, is dropped. Its membership is a `.members` file as
  for folders; its settings on this device (name, muted, the wake-up command, what was read) are a `[[channel]]`
  entry in `sync.toml` (`topic_read` keeps what was read in each topic). 0.1.12 kept a per-device agent switch there
  (`agents`: `off`, `read`, `post`); since 0.1.13 the channel's membership decides (kind 8), and the key is ignored
  when read and dropped when the file is next written.
- **Files of a channel or conversation** (0.1.15, `sync/<channel id>.files/`, `sync/dm-<device>.files/`): `state`,
  sealed JSON (`shares`: each file served from here, its path, size, modification time, whole-file and piece-table
  hashes, whether it was sent from here and why it is no longer served; `downloads`: each download, its destination,
  its partial file, the bitmap of verified pieces in hex, its state), and one file per piece table named by the
  table's hash, sealed. Both with the conversation's key and `SWFL` as associated data, written to a temporary name
  and renamed into place; the bitmap is saved at most every half second while a download runs, after the partial
  file is flushed, and when the download stops. The folder goes with the conversation.
- **A channel's images** (0.1.15, `sync/<channel id>.blobs/<sha256 hex>`): each file is the image sealed whole with
  the channel's own key (ChaCha20-Poly1305, a random nonce first, `SWBL` as associated data), written to a
  temporary name and renamed into place. The folder goes with the channel. Images neither within the channel's
  budget (the newest 512 MiB) nor asked for since the service started are deleted.
- **Private messages** (`sync/dm-<device id>.chat`, one per conversation): `"SWDM" ‖ format varint (2) ‖ n ×
  (varint length ‖ sealed record)`, a record being `received_ms varint ‖ has_body u8 [‖ body bytes] ‖ direct
  message` (the body only in the author's own copies; those to this device are opened again when read), sealed
  with the conversation's own key (ChaCha20-Poly1305, a random nonce first, the magic as associated data). The
  conversation is a `[[conversation]]` entry of `sync.toml` (the device, its names and last addresses, what was
  read); the agent switch and the next `seq` are its `[direct]` table. 0.1.8's single `sync/direct.chat` (format 1,
  in the clear, with messages carried for others) and `sync/direct.keys` are split into conversations when read,
  the carried messages dropped, and both deleted.
- **Your devices** (0.1.13): the group's membership is `sync/<group id>.members` (as a folder's); the group is the
  `[my_devices]` table of `sync.toml` (`id`, `group_pk`, its devices as `[[my_devices.device]]`, `skip`: what the user
  took off this device, `mode`: `full`, `previous`: the groups it moved on from, `pending_remove`: devices removed in
  one of those that it still takes out of the current group). The user key is the secret store's `folder-<group id>`,
  never in `sync.toml`; the groups moved on from keep their `.members` files, not their keys.
  A version before 0.1.13 drops the table when it writes the file: that device forgets the group (pair it again after
  updating); its folders keep syncing.

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
   table here; bits are never reused.

   | bit | feature | since |
   |---|---|---|
   | 0 | channels, their topics and private messages: types 17 to 22 | 0.1.6 |
   | 1 | private messages only between their two devices, over a session no device relays; DirectJoin (type 23) | 0.1.9 |
   | 2 | holds: answers HoldQuery (type 24) with HoldReply (type 25); only a device with it is asked | 0.1.12 |
   | 3 | renames: answers RenameRequest (type 26) with RenameReply (type 27); only a device with it is asked | 0.1.12 |
   | 4 | devices: the user's own devices (DevicesJoin, type 28; the sibling proof, kind 6); only a device with it is sent either | 0.1.13 |
   | 5 | blobs: images and files of channels and private messages by hash (BlobWant, type 29; BlobData, type 30); only a device with it is asked | 0.1.15 |

5. **Breaking changes** raise `PROTOCOL`, the highest version spoken. The new build keeps `MIN_PROTOCOL` one
   behind for at least one release (N-1), so devices a release apart still sync at the older version, and
   raises it later. Two devices whose ranges do not overlap refuse each other and say which needs an update.
6. **Invitations and links** never grow: a new form is a new invitation version (the first byte of an
   invitation and of a link's payload).
7. **Signed bytes** never change for an existing type: a new signed form needs a new domain string
   (`seedwell-sync/v3/...`).
8. **Stored formats** change by raising their format number, with a migration where the old data must be kept.

## Checking compatibility

- `tests/it/golden.rs`: the frozen 0.1.6 vectors (and those added since, such as the 0.1.12 channel statement), and
  the rules above (appended fields, grown records, unknown
  kinds and values, newer links, a peer with a newer protocol and features).
- `crates/sync-net/tests/it/versions.rs`: two services on loopback, one speaking a newer protocol range with
  unknown features (they sync), and one that only speaks a newer version (refused, with the reason shown).
- `crates/sync-net/tests/it/chat.rs`: channels and topics on loopback, including a device without the chat bit,
  which gets no channel messages while its folders still sync; `direct.rs`: private messages (sealed to the
  recipient, carried by a third member, delivered later through it, tampered with).
- `crates/sync-net/tests/it/devices.rs`: the user's own devices on loopback: pairing confirmed (or refused) on the
  device that made the link, folders and channels on all devices, the admin path and the owner raising it, a member
  without the devices bit accepting a sibling from the pushed grant, final removal, the skip list, Start Over.
- `tools/sync-compat.sh OTHER`: this build's daemon against another `seedwell` binary, on loopback with
  temporary state: share both ways (each side the owner in turn), sync files, change a role, chat in a channel,
  remove a device; and that the CLI has no pairing command (pairing is the app's alone).
  The CLI tests run it with this build on both sides. From 0.1.7 on, CI should also run it against the latest
  release's CLI (`Seedwell.app/Contents/MacOS/seedwell`).
