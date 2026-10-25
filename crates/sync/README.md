# trs-sync

The folder-sync core of Seedwell ([SPEC §7G](../../SPEC.md), the sync model in §7G.2): what is on disk, how files are
cut into chunks, what is ignored, and how names compare. It is sans-I/O in the network sense: no sockets and no
async. The driver (`trs-sync-net`) runs it on blocking threads and moves the messages. The filesystem is injected
(`fs::Fs`), so the same code runs on disk and in the simulator.

## Modules

| Module | What it holds |
|---|---|
| `fs` | `trait Fs` / `trait FileHandle` (blocking), `StdFs` (the disk, `std` only), `MemFs` (in memory, deterministic, crash snapshots and fault injection), `components` (the path-escape guard), `read_all` / `write_all` |
| `path` | `wire_name` (NFC), `key` (the comparison key: NFC, plus case folding on case-insensitive folders), `check_wire_path`, `probe_case_insensitive` |
| `ignore` | `Ignore`: the default ignores and the `.seedwellignore` matcher (Syncthing syntax, own NFA glob) |
| `chunk` | FastCDC v2020 (`Chunker`, `chunk_bytes`, `chunk_file`), size `Tier`s and `choose_tier` |

### Filesystem

* `StdFs` joins every path below its root through `fs::components`, which refuses absolute paths, `.`, `..`, empty
  components and NUL: the second line of defense behind `validate_path`. Writes and `open_read` also refuse a path
  with a symlink among its parents (a directory swapped for a link must not lead outside the folder). `lstat` and
  `read_dir` skip that check: they are the scanner's hot path, and the scanner never follows links.
* `lstat` gives inode, ctime and mode through `MetadataExt`, and on macOS `dataless` from `st_flags & SF_DATALESS`.
  `copy` is `std::fs::copy` (a clone on APFS), `set_mtime` is `File::set_modified`.
* `MemFs` keeps a live tree and a durable image. `snapshot()` makes the live tree durable; `restore()` is a crash:
  back to the image, plus the file data that `sync_all` wrote into it since. Namespace changes (create, rename,
  link, remove) are durable only at the next snapshot, the pessimistic end of real filesystems. It can be
  case-insensitive and normalization-insensitive like APFS (lookups fold, names keep their spelling), and
  `fail_next(path, n, kind)` makes the next `n` operations on a path fail.

### Names

The disk name is never rewritten. A listing on a Mac can hold NFD names (HFS+ stored NFD; APFS keeps whatever the
app wrote), the wire is NFC, and the two are matched through `path::key`. Case folding is per character: the
lowercase form, plus the few characters whose Unicode simple case folding differs (final sigma, long s, micro sign,
Greek symbol forms). Full folding (`ß` → `ss`) is the documented gap; it errs towards "two files", never towards
overwriting.

## Index storage

`index::IndexStore` is the folder's index: local records, remote entries, peers, writers, and the derived need
list and chunk map. `MemStore` (in memory) is the reference semantics; `store_redb::RedbStore` is the same thing in
one redb file per folder (`<state>/sync/<folder-id>.redb`), checked against `MemStore` by a property test that
compares every read after random op sequences.

* **Tables:** `local` (path → record), `by_seq` (sequence → path), `remote` ((path, device) → entry) with the reverse
  `remote_by_device` ((device, path) → ()), `peers`, `writers`, `need` (path → record), `chunks`
  ((hash, path, offset) → ()), and `meta` (schema version, index id and sequence, need and local totals). Paths
  below a directory are read as the key range `[dir/, dir0)`, so `local_paths` never scans; `need_totals` and
  `local_totals` are counters kept in the same transaction as the records they count.
* **Records** use the wire encoding of `trs-sync-proto` (`msg::encode_entry`), so the checked decoder that reads
  peers also reads the disk. A record that does not decode is an `InvalidData` error naming its table and key, and a
  record the decoder would refuse (a path over `MAX_PATH`) is refused on write.
* **Atomic:** every `apply` is one write transaction. After a crash each batch is there whole or not at all, and the
  derived tables always match their records.
* **Durability:** `Immediate` (default) fsyncs before `apply` returns. `Eventual` commits without fsync; a crash
  rolls back to the last durable commit, losing those batches whole (for recomputable work: scans, remote batches,
  whose `max_seq` rolls back with them). The puller commits finished steps in batches (every 64 steps and at the end
  of a round), after `Fs::sync_dir` made the directories they changed durable, so a record is never ahead of the
  disk; if a crash comes between a file landing and its record, the scanner adopts the finished file as the version
  it was pulled for. `local_since` (what peers are sent from) flushes first, so a sequence a peer has seen is never
  reused.
* **Schema:** version 1, with `trs-state`'s migration pattern: an older file is copied to `<file>.v<N>.<n>.bak` and
  migrated in one transaction, a newer one is refused untouched.

Scale (Apple Silicon, release, `redb_scale`: 200k local records of 4 chunks, plus 2 peers × 200k entries, batches
of 1000 through `Batch`): inserting takes 1.0 s for the local records and 1.7 s for the remote entries with
`Eventual` and a flush (1.6 s and 3.1 s with `Immediate` per batch); an `Immediate` batch of 100 changes 4–5 ms. A
`local_since` page of 1000 takes 0.7 ms, a `local_paths` page 0.2 ms, a chunk lookup 4 µs. Dropping a peer
(200k paths re-evaluated) 1.0 s. The file is 514 MiB (redb grows it in large steps; 271 MiB after dropping one peer
and `compact`). Open 3 ms, with `verify` 150 ms. RSS 31 MiB with the 16 MiB page cache.

```sh
tools/sysguard/heavy.sh cargo test --release -p trs-sync --test it redb_scale -- --ignored --nocapture
```

## FastCDC contract

The cut points are a wire-level contract: two devices must cut the same bytes the same way, or renamed and copied
files would be sent again. Never change any of this:

* **Gear table:** 256 outputs of splitmix64 started at `GEAR_SEED = 0x5365_6564_7765_6c6c` ("Seedwell"). The first two
  are `0x32164987A8EC18CF`, `0x135273D2344BE8F4`.
* **Hash:** `h = (h << 1) + GEAR[b]`, from 0 at `min` bytes into a chunk; the chunk ends after the byte where
  `h & mask == 0`, or at `max`.
* **Normalized chunking, level 2:** `log2(avg) + 2` mask bits below `avg`, `log2(avg) - 2` from `avg` to `max`. Mask
  bits are spread evenly over bits 16–62 (`chunk::mask`).
* **Two bytes per step** (`GEAR_LS = GEAR << 1`, paper §3.7) gives exactly the per-byte hash (a property test checks
  this against a per-byte loop).
* **Tiers:** avg = clamp(pow2_ceil(size / 2048), 64 KiB, 4 MiB) with integer division, min = avg / 4, max = avg × 4
  (at most `MAX_CHUNK`). A file keeps its previous tier while its size stays within 0.5×–2× of the size that chose
  it. The caller stores that pair (`choose_tier(size, Some((tier, chosen_at)))`); it is not on the wire, and it is
  not guessed from chunk lengths.
* **Golden vector:** 1 MiB of splitmix64 output (seed `0x12345678`, little-endian) at the 64 KiB tier cuts into
  `153497, 73544, 68951, 78583, 33840, 68110, 72600, 111700, 66901, 69123, 67140, 77634, 67772, 39181`
  (`chunk::tests::golden_cut_points`).

Throughput (Apple Silicon, release, 256 MiB in memory, `chunk_bench`): chunk + SHA-256 about 1.5 GB/s on one
thread and 2.9 GB/s on two (two files at once); gear alone 2.5 GB/s, SHA-256 alone 2.8 GB/s.

```sh
tools/sysguard/heavy.sh cargo test --release -p trs-sync chunk_bench -- --ignored --nocapture
```

## Dependencies

| Crate | Why |
|---|---|
| `trs-sync-proto` | The shared model: `FileInfo`, `ChunkRef`, `MAX_CHUNK`, `validate_path`, `STATE_DIR`, and the entry codec the index stores reuse. |
| `redb` | The index file (`store_redb`). Already Seedwell's database (`trs-state`, same major version): pure Rust, ACID transactions, crash-safe commits, ordered keys with range reads, no C library or server. |
| `sha2` | SHA-256 per chunk. Already in the workspace (`trs-storage`); it uses the ARMv8 SHA instructions, chosen at run time. |
| `unicode-normalization` | NFC for wire names and keys. The tables are not in `std`, and the Linux daemon cannot use CFString. No default features beyond `std`; pulls only `tinyvec`. |

Dev: `proptest` (properties: chunk stability, streaming, the two-byte step), `tempfile` (temp folders for the
`StdFs` tests; never the home folder).
