# trs-wire

BitTorrent peer wire protocol for Seedwell. Sans-I/O: bytes in, messages out, no sockets, no clock, no async runtime.
`#![forbid(unsafe_code)]`. Depends on `trs-*` crates: none (info hash and peer id are plain `[u8; 20]`).

| BEP | What | Where |
|---|---|---|
| 3 | handshake, choke/unchoke/interested/not interested/have/bitfield/request/piece/cancel/port | `handshake`, `message`, `codec` |
| 6 | Fast: suggest, have all, have none, reject request, allowed fast + allowed-fast set generation | `message`, `fast` |
| 10 | extension protocol, extended handshake (`m`, `v`, `reqq`, `p`, `yourip`, `metadata_size`, `upload_only`), unknown-extension passthrough | `extended` |
| 9 | `ut_metadata` request/data/reject, 16 KiB pieces, assembly + SHA-1 verification, 16 MiB cap | `metadata` |
| 11 | `ut_pex` added/added.f/dropped, v4 + v6, send/receive caps | `pex` |
| 27 | PEX never advertised or parsed for private torrents; no DHT bit in the handshake | `ExtPolicy`, `Reserved::ours` |

## Usage sketch

```rust
use trs_wire::*;
use bytes::BytesMut;

let private = false;
let ours = Handshake::new(info_hash, peer_id, private);
socket.write_all(&ours.to_bytes());
// ...read into `rx: BytesMut` until Handshake::decode(&rx) returns Ok(Some(h)), then rx.advance(HANDSHAKE_LEN).
let dec = Decoder::new(Limits::for_pieces(num_pieces)).negotiated(&ours.reserved, &h.reserved);
let mut enc = Encoder::new();
let policy = ExtPolicy::for_torrent(private);
enc.push_extended(0, |b| LocalExtendedHandshake::new(env!("CARGO_PKG_VERSION"), policy).encode_payload(b));

while let Some(msg) = dec.decode(&mut rx)? {            // OwnedMessage: piece data is a Bytes slice of `rx`
    if let Message::Extended { id, payload } = &msg {
        match ExtendedMessage::parse(*id, payload, policy) { /* Handshake / Metadata / Pex / Unknown */ }
    }
}
socket.write_all(enc.pending()); enc.advance(n);
```

For the hottest path use `Decoder::decode_ref(&[u8])`, which returns `MessageRef<'_>` views and the frame length without touching `BytesMut`. For uploads use `piece_header()` plus a vectored write so a block is never copied into the encode buffer.

## Message Stream Encryption (`mse`)

The primitives of the obfuscated handshake that Transmission, libtorrent and others open connections with: 768-bit Diffie-Hellman in the
fixed MSE group (`public_key`, `shared_secret`; a Montgomery multiplier over twelve 64-bit limbs, checked against Python's `pow`;
degenerate public values 0, 1, p-1 and above p are refused), RC4 with the 1,024-byte discard (`Rc4`), and the SHA-1 helper (`hash`).
The protocol on a socket is in `trs-engine` (`mse.rs`). Not constant-time: the exponents are fresh per connection and the scheme
offers obfuscation, not authentication.

`Reserved::ours_with(private, dht)` sets the DHT bit of the handshake only when a DHT node runs (and never for a private torrent).

## Safety properties

- **No panics on untrusted input**: no `unwrap`/indexing on peer data; checked by unit tests, proptests, and `cargo fuzz` (`fuzz/`).
- **Lengths are capped before anything is buffered.** The 4-byte prefix is checked against 2^17 immediately; with the 5th byte the per-id rule applies (fixed sizes for choke/have/request/..., block <= 16 KiB + 9 for `piece`, extended <= 32 KiB, bitfield exactly `ceil(pieces / 8)` once the piece count is known (the only frame allowed to exceed 2^17), otherwise <= 2^17). A lying prefix is rejected before the body arrives; the receive buffer never grows beyond one capped frame. Unknown ids are skipped (and counted by the caller as `Message::Unknown`) but obey the block-sized cap.
- **Negotiation is enforced.** Fast and extended frames are errors unless both handshakes advertised them (`Decoder::negotiated`).
- **`request`/`cancel`/`reject` lengths are capped at the block limit**, so a peer cannot make the engine read an attacker-chosen amount from disk.
- **Honest identity.** The extended handshake `v` is always `"Seedwell <version>"`; the caller supplies only a version string, which is sanitised. There is no API to claim another client (SPEC 7B.1).
- **Private torrents.** `ExtPolicy::for_torrent(true)` removes `ut_pex` from our `m` dictionary and makes `ExtendedMessage::parse` refuse PEX frames. `Reserved::ours(true)` also drops the DHT bit.
- **Metadata**: size 1 B ..= 16 MiB, pieces length-checked, SHA-1 verified against the info hash; a mismatch resets the assembler so every piece is requested again.

## Per-connection memory

Target: <= 2 KiB of protocol state per idle peer, socket buffers excluded. Measured with `cargo run --example sizes`:

| Item | Bytes | Notes |
|---|---|---|
| `PeerState` (flags, reserved, ext ids, reqq, port, client `v` inline <= 32 B) | 56 | |
| `Decoder` (limits + 2 flags) | 24 | `Copy`; can be shared per torrent instead of per peer |
| `Encoder` (empty `BytesMut`) | 32 | call `release_if_idle()` when the queue drains so no 16 KiB buffer is pinned |
| `RequestWindow` | 8 | |
| Receive `BytesMut` header | 32 | allocate only while data is in flight |
| Peer `Bitfield` | 24 + ceil(pieces / 8) | e.g. 1,000 pieces = 149 B; 100,000 pieces = 12.5 KB (engine should drop it for peers we are not downloading from, or keep it only in the Hot tier) |
| **Total, idle, 1,000-piece torrent** | **~300 B** | well under the 2 KiB target |

Transient: `MetadataAssembler` (64 B + metadata size, only while fetching a magnet), `PexBuilder` (one message worth, built once a minute), `ExtendedHandshake` (80 B, borrows the received payload, dropped after use). Nothing in this crate allocates per message on the decode path.

## Dependencies

- `bytes` (no default features): `BytesMut`/`Bytes` give zero-copy slicing of the receive buffer, which is the core of the decoder design.
- `sha1` (no default features): `ut_metadata` verification and the allowed-fast set; uses the ARMv8 SHA extensions on Apple silicon.
- dev only: `proptest`.

Not used: `trs-bencode`. The extension dictionaries are tiny, flat and attacker-controlled, and `ut_metadata` data messages append raw bytes after the dictionary, so `src/bencode.rs` is a ~150-line allocation-free borrowed walker with the same strict integer rules and a depth limit of 8. This also keeps the crate free of a cross-crate API dependency. If the engine wants one bencode implementation, swapping it in is local to that file.

## Tests and tools

- `cargo test -p trs-wire`: unit tests next to the code (byte-exact vectors for every message, handshake edge cases, partial reads at every split point, oversized/garbage input, PEX caps, ut_metadata with a corrupt piece, BEP 6 example vectors), `tests/proptests.rs`, `tests/flow.rs` (two in-memory peers).
- `cargo bench -p trs-wire`: decode throughput (`benches/decode.rs`, plain harness).
- `cargo +nightly fuzz run decoder` / `extensions` from `crates/wire` (targets in `fuzz/`).
- `cargo test -p trs-wire --test network -- --ignored --nocapture`: live handshake + extended handshake + `ut_metadata` fetch against the Debian netinst swarm (one `started` and one `stopped` announce, at most 12 short connections, no payload data).
