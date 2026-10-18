# Seedwell — Engineering guide

Read `SPEC.md` first. This file is the contract between crates and the quality bar for every change.

## 1. Crate graph

```
trs-bencode ─▶ trs-metainfo ─┬─▶ trs-tracker ─┐
                             ├─▶ trs-wire ────┤
                             └─▶ trs-storage ─┤
trs-dht, trs-utp, trs-portmap ────────────────┼─▶ trs-engine ─┬─▶ trs-ffi  ─▶ app/ (Swift)
trs-http (direct downloads) ──────────────────┤               └─▶ trs-cli (`seedwell`)
trs-classify ─▶ trs-state (config, rules, ────┘
                 profiles, stats, redb)
tools/tracker-probe   (test tool; standalone)
```

A crate may depend only on crates to its left. Shared primitive types (`InfoHash`, `PeerId`) live in `trs-metainfo`.
To build in parallel, `trs-tracker`, `trs-wire` and `trs-storage` do **not** depend on `trs-metainfo`:
- They take plain `[u8; 20]` info-hash and peer-id bytes, and their own small input structs (e.g. a file layout).
- `trs-tracker` may depend on `trs-bencode`.
- `trs-engine` converts between the types.

## 2. Quality bar (every crate, every change)

- `cargo build`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` are clean for the crate you touched.
- Rust 2024 edition. Each crate has `#![forbid(unsafe_code)]`, except `trs-storage` and `trs-ffi`, where every `unsafe` block carries a `// SAFETY:` comment.
- **No panics on untrusted input.** Anything parsed from the network, from `.torrent` files or from disk returns `Result`. Never call `unwrap`/`expect`/indexing on untrusted data. Fuzz every parser (`fuzz/` directory per crate, `cargo fuzz`).
- **Memory discipline:**
  - No per-item heap allocation in hot paths. Prefer `Box<[u8]>` over `Vec` for fixed data, and use `SmallVec`/arrays where sizes are bounded.
  - Every collection that grows from external input has an explicit cap.
  - Document any struct that is held per torrent or per peer with its size in bytes (`// size: N bytes`).
- Keep dependencies small. Use `default-features = false` and enable only what you use. Justify each new dependency in the crate README.
- Comments: explain *why*, not *what*. Every public item gets a one-line doc comment.
- Tests live next to code (`#[cfg(test)]`). Integration tests go in `tests/`. Fixtures go in `tests/fixtures/`.
- **Do not commit.** The coordinator reviews and commits. Do not edit files outside the crate(s) you were assigned, except to add your crate's dependencies.

## 3. Crate contracts

### trs-bencode
- `Value<'a>`: zero-copy (`Int(i64)`, `Bytes(&'a [u8])`, `List(Vec<Value>)`, `Dict(Vec<(&'a [u8], Value)>)`) in original key order.
- `decode(&[u8]) -> Result<Value, Error>` must consume the entire input. `decode_prefix` returns `(Value, rest)`.
- **Raw spans:** callers must be able to get the exact original bytes of any dict value. The info-hash is SHA-1 over the *original* `info` bytes, never a re-encoding.
- Integer rules: canonical only (no leading zeros, no `-0`), with i64 overflow returning an error. Accept unsorted dict keys when decoding, because real torrents have them, but expose `is_canonical()`.
- Limits: depth ≤ 64, and `DecodeLimits { max_items, max_bytes }`.
- Encoder: `Encoder` writes into `Vec<u8>`. Dicts are emitted with sorted keys.

### trs-metainfo
- `InfoHash { v1: Option<[u8;20]>, v2: Option<[u8;32]> }` with hex/base32 helpers. `PeerId([u8;20])`. The client prefix is one constant, `PEER_ID_PREFIX` (dev builds: `-SX0001-`, final code chosen before 1.0).
- `Metainfo::from_bytes` gives: info hash, name, piece length, total length, files (sanitized path components, length, offset, padding flag), `private`, announce tiers (`announce-list` with fallback to `announce`), `url-list`, `source`, creation date, comment, created-by. Piece hashes are kept as one `Box<[u8]>`. v2/hybrid are detected (`meta version`), with v1 fully supported.
- **Path sanitization is security-critical.** Reject absolute paths, `..`, empty components, NUL and `/` inside components, and over-long names. Map names that are invalid on macOS.
- `Magnet::parse`: `xt` (`urn:btih` hex/base32, `urn:btmh`), `dn`, repeated `tr`, `ws`, `xl`, `so`.

### trs-tracker (safety-critical — SPEC §7B, §7C)
- **Sans-I/O core:** `AnnounceMachine` per (torrent, tracker URL) and `TrackerSet` per torrent (BEP 12 tiers). They are pure: `handle(input, now) -> Vec<Intent>`, with no clock or sockets inside.
- **`HostGate`:** the only producer of wire requests. The request type has a private constructor. It enforces 1 in-flight request per (torrent, tracker), ≤ 2 concurrent requests per host, and a per-host token bucket. It also contains the host circuit breaker.
- Persisted timing goes through a `TimingStore` trait: last announce, next allowed time, `completed_sent`.
- Transports: HTTP(S) (rustls, gzip, per-host keep-alive, ≤ 3 redirects, timeouts) and UDP (BEP 15/41: connection-ID reuse, retransmit). URL building percent-encodes every byte except RFC 3986 unreserved characters, and preserves the existing query (passkeys).
- All §7C.3 invariants are covered by property tests. There is a deterministic simulation harness with a virtual clock, and fuzzing of response parsers.

### trs-wire
- Handshake (reserved bits: extension protocol, Fast extension, DHT). All BEP 3 messages, BEP 6 Fast messages, BEP 10 extended handshake (`m`, `v`, `reqq`, `p`, `yourip`, `metadata_size`), BEP 9 `ut_metadata`, BEP 11 `ut_pex`.
- Incremental, zero-copy decoder over a byte buffer with a hard cap on message length. Compact `Bitfield`.

### trs-storage
- Piece ↔ file mapping (including padding files), positional I/O (`pread`/`pwrite`) on a dedicated thread pool, an fd LRU cache, sparse files, a bounded write cache flushed per verified piece, and hardware SHA-1/SHA-256.
- Resume validation by size+mtime, full recheck, and atomic move/rename for organize.

### trs-engine (later wave)
Session, torrent tiers (Cold/Warm/Hot, SPEC §5), peers, picker, choker, rate limits, queue, rules, statistics, persistence (`redb`), and the control API used by `trs-ffi`/`trs-cli`.

## 4. Testing against the real world
- Use only legal public torrents for network tests: official Linux distro torrents (Ubuntu, Debian, Fedora, Arch, etc.).
- Downloads go to `bench/downloads/`. Never point anything at a private tracker.
- Network tests are `#[ignore]` by default and run explicitly (`cargo test -- --ignored`).
