# trs-metainfo

`.torrent` and magnet parsing, plus the shared identity types (`InfoHash`, `PeerId`).

## API sketch

```rust
use trs_metainfo::{Metainfo, Magnet, InfoHash, PeerId};

let m = Metainfo::from_bytes(&bytes)?;      // or Metainfo::from_info_bytes(info) for BEP 9
m.info_hash                                  // InfoHash { v1: Option<[u8;20]>, v2: Option<[u8;32]> }
m.name; m.piece_length; m.total_length; m.wanted_length(); m.private;
m.files()                                    // FileRef { index, path, length, offset } + is_padding()...
m.piece_count(); m.piece_hash(i); m.piece_size(i); m.piece_hashes()   // one Box<[u8]>
m.announce_tiers; m.url_list; m.source; m.comment; m.created_by; m.creation_date;
m.info_range                                 // byte range of raw `info` in the input (BEP 9 serving)
m.warnings                                   // Vec<Warning>: ignored URLs, repaired paths, ...

let mg = Magnet::parse("magnet:?xt=urn:btih:...&dn=...&tr=...")?;
mg.to_uri(false);                            // re-render without trackers (no passkey leak)

let id = PeerId::generate();                 // PEER_ID_PREFIX + 12 random alphanumerics
info_hash.verifies(&metadata_bytes);         // validate BEP 9 metadata against a magnet
```

## Safety and limits

- Info-hash is SHA-1 (v1) / SHA-256 (v2) over the **original** `info` bytes, never a re-encoding.
- Paths: absolute, `..`, `.`, empty, NUL, `/` inside a component and components over 255 bytes
  are rejected (the whole torrent is refused, not repaired). Invalid UTF-8 becomes U+FFFD and
  `:` becomes `_`; both are reported in `warnings`. Duplicate resulting paths are rejected.
  `name.utf-8` / `path.utf-8` are preferred when present.
- BEP 47: `attr` flags are exposed (`p`adding, e`x`ecutable, `h`idden, `l` symlink). Symlink
  entries are listed but must never be created on disk.
- Caps: 32 MiB / 2 Mi bencode items per `.torrent`, 1 Mi files, 256 MiB piece length,
  256 trackers in 64 tiers, 128 web seeds, 16 KiB magnets.
- v2-only torrents are parsed provisionally (file list with piece-aligned offsets, no piece
  verification data); hybrids use the v1 layout. Both set `Warning::V2PieceLayersIgnored`.

## Dependencies

| crate | why |
| --- | --- |
| `trs-bencode` | the decoder; raw spans for the info-hash |
| `sha1` 0.11, `sha2` 0.11 (no default features) | RustCrypto; uses the ARM SHA intrinsics on aarch64 via runtime feature detection, so no asm or linking |
| `getrandom` 0.3 (no default features) | OS entropy for `PeerId::generate` (smaller than pulling in `rand`) |

Dev only: `proptest`, `sha1`.

## Tests

`cargo test -p trs-metainfo` runs unit tests with hand-built torrents plus real distro torrents in
`tests/fixtures/` (sources and expected hashes documented in `tests/fixtures.rs`).
`cargo test -p trs-metainfo -- --ignored` additionally downloads piece 0 of each payload from
official mirrors and checks it against the torrent's piece table. Fuzz targets: `fuzz/`
(`cargo +nightly fuzz run metainfo|magnet`).
