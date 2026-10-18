# trs-bencode

Strict, zero-copy bencode (BEP 3) decoder and encoder. No `unsafe`, no dependencies.

## API sketch

```rust
use trs_bencode::{decode, decode_prefix, encode, raw_dict_get, Value, DecodeLimits};

let v = decode(b"d3:cow3:mooe")?;                  // must consume all input
v.get(b"cow").and_then(Value::as_str);             // Some("moo")
let (v, rest) = decode_prefix(b"i1ei2e")?;         // rest == b"i2e"
v.is_canonical();                                  // sorted keys everywhere?
encode(&v);                                        // always sorted keys

// Exact original bytes of a nested value (info-hash input):
let info: Option<&[u8]> = raw_dict_get(torrent_bytes, b"info")?;
```

- `Value<'a>` = `Int(i64) | Bytes(&[u8]) | List(Vec<_>) | Dict(Vec<(&[u8], _)>)`, dict entries
  kept in original order.
- `decode`, `decode_with_limits`, `decode_prefix`, `decode_prefix_with_limits`.
- Raw spans: `raw_dict_get`, `raw_dict_entries`, `raw_list_items` return subslices of the input
  (they validate with the same grammar as `decode` and allocate nothing per value). The
  returned slice is itself a complete bencode value, so calls compose.
- `Encoder` (streaming into `Vec<u8>`) and `encode(&Value)`.

## Rules enforced

- Integers: canonical only (no `-0`, no leading zeros, no `+`), `i64` overflow is an error.
- String lengths: no leading zeros, overflow-safe.
- Nesting <= `MAX_DEPTH` (64). `DecodeLimits { max_items, max_bytes }` (defaults 4 Mi items,
  64 MiB) bound memory on hostile input.
- Unsorted dict keys are accepted (real torrents have them); `Value::is_canonical()` reports it.
- **Duplicate dict keys are rejected**: two parsers picking different winners is an info-hash
  confusion vector.
- Decoding never panics (property-tested and fuzzed: `fuzz/`, `cargo +nightly fuzz run decode`).

## Dependencies

None at runtime. Dev only: `proptest` (round-trip and never-panic properties).
