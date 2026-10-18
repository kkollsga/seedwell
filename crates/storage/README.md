# trs-storage

The disk layer of Seedwell: piece/file mapping, a dedicated I/O + hashing thread pool, a shared
fd cache, a bounded write cache, resume validation, recheck/scrub and crash-safe finalize.
It depends on **no other workspace crate**: the engine converts metainfo into [`Layout`] input.

## Public API at a glance

| Item | Purpose |
|---|---|
| `Layout`, `FileSpec`, `FilePath` | Immutable piece ↔ file map. `segments(offset, len)` / `piece_segments(piece, begin, len)` return an allocation-free iterator of `Segment { file, file_offset, data_offset, len, padding }`. Per-file `Relative(components)` or `Absolute(path)` (per-file paths). BEP 47 padding files are flagged. |
| `DiskIo::new(DiskConfig)` | Starts the pools: `io_threads` (pread/pwrite, bounded queue), `hash_threads` (default = perf cores), `maintenance_threads` (recheck, scrub, finalize, stat). |
| `DiskIo::add_torrent(layout, TorrentConfig) -> TorrentHandle` | Cheap registration; nothing is opened or created. |
| `TorrentHandle::write_block(piece, begin, &[u8]) -> Result<WriteAck>` | `Buffered`, `Duplicate`, or `Complete(Pending<PieceOutcome>)` → `Verified(p)` / `HashFailed(p)`. |
| `TorrentHandle::read_block(piece, begin, len, Vec<u8>) -> Result<Pending<Vec<u8>>>` | Caller-owned buffer is reused (slab-friendly). Padding reads as zeros. |
| `TorrentHandle::{recheck, scrub}(RecheckOptions)` | Streaming full verification; progress callback, `CancelToken`, byte/s throttle (scrub defaults to 16 MiB/s at background I/O priority). |
| `TorrentHandle::{fingerprints, check_files}` | Size+mtime resume validation → `ResumeReport` (`Ok/Missing/SizeMismatch/Modified/NoRecord` per file). `FileFingerprint` has a fixed 20-byte encoding. |
| `TorrentHandle::finalize(journal_path, FinalizeOptions)` | Moves `.incomplete` → final paths (rename, or copy+fsync+remove across volumes) with an append-only journal; call again after a crash to resume. |
| `TorrentHandle::{drop_layout, rebuild_layout, resident_bytes}` | Cold-tier support (see Memory). |
| `TorrentHandle::relocate_{prepare, mark_switched, commit, rollback, recover}` | Journaled move of a torrent's files to new paths: rename on one volume, hashed streaming copy across volumes (see Relocation). |
| `TorrentHandle::{switch_layout, plan_paths, quiesce_writes, resume_writes}` | The instant switch to the new paths, the plan for a root change, and the write gate for a downloading torrent. |
| `Layout::{structure_digest, with_paths}` | Geometry-only fingerprint (paths excluded) and "same layout, other paths". |
| `rename_excl`, `same_volume`, `dev_of_nearest_existing`, `available_space` | `renamex_np(RENAME_EXCL)` and the volume helpers (device id of the nearest existing ancestor; free bytes). |
| `TorrentHandle::{set_mode, set_location, set_paths, ensure_root, attach_hashes, detach_hashes, abort_piece, sync}` | Lifecycle controls. |
| `raise_nofile_limit`, `perf_core_count`, `preallocate`, `release_free_memory` (gives the allocator's freed blocks back to the OS after a burst of work: `malloc_zone_pressure_relief` on macOS, `malloc_trim` on glibc), `hash::{sha1, sha256, Sha1Stream, Sha256Stream, sha1_hw_accelerated}` | Startup helpers and primitives. |
| `MappedAlloc`, `MAP_MIN` | `#[global_allocator]` of the apps (`trs-ffi`, the `seedwell` daemon): blocks of 128 KiB and more are an anonymous `mmap`, unmapped when freed, so a start-up burst of hash scratch, piece buffers and grown tables does not stay resident next to the small long-lived records. macOS only; elsewhere it is the system allocator. No new dependency (`libc` was already here); see the engine README, Memory. |
| `Pending<T>` | Request future: `.await` it, or `.wait()` from a plain thread. Backed by a hand-written oneshot. |
| `StorageError` | Typed: `MissingData { file }`, `Truncated { file }`, `MissingRoot`, `ReadOnly`, `Busy`, `Cancelled`, `DestinationExists { file }`, … |

Back-pressure: submissions use `try_send` on bounded queues and return `StorageError::Busy`
with **no side effects**; retry later.

## Behaviour that matters

* **Write cache** (default 32 MiB, global). A piece buffer is reserved when the piece's first block
  arrives; blocks are copied in, and when the piece is complete it is hashed (SHA-1) on the hash pool
  and, only if valid, written with one `pwrite` per file segment. Invalid pieces are dropped
  and reported as `HashFailed`; nothing reaches the disk. When the budget is exhausted, pieces that
  do not fit are **written through** block by block and, once complete, re-read in 256 KiB chunks
  and verified (bounded memory). Cache usage never exceeds the budget (tested). Pieces larger than
  the whole budget always write through.
* **Only serve a piece after its `Verified` future resolved.**
* **fd cache**: one LRU shared by all torrents (default 512 entries, `Arc<File>` so an eviction never
  yanks an fd from a running `pread`). Files open read-only; read-write only after the first write
  while `Mode::Download`. `set_mode` drops the torrent's cached fds so access mode follows the mode.
  A cached fd of a file that was *unlinked behind our back* keeps serving its old inode; deletion is
  detected by `check_files` (cold wake), `recheck` (drops cached fds first) or an fd eviction.
* **Missing data**: reading a file that does not exist returns `MissingData { file }`; in `Seed` mode
  nothing is ever created (`File::open`, no `create`). While downloading, the save root (or staging
  root) must already exist: writes return `MissingRoot` instead of rebuilding it, and
  directories below `/Volumes/<name>` are never created unless `/Volumes/<name>` is mounted. The
  engine calls `ensure_root()` once when a new download starts.
* **Sparse by default**; `TorrentConfig::preallocate` uses `fcntl(F_PREALLOCATE)` + `ftruncate`.
* **Staging**: `TorrentConfig::with_default_staging()` writes below `<root>/.incomplete`. Files with
  a custom absolute target are staged as `.incomplete/.custom/<index>-<name>`.
  `finalize` never overwrites an existing destination (`DestinationExists`).
* **Finalize journal**: header `TRSJ|v1|file count|layout digest` + 5-byte records. Rename moves need
  no records (rename is atomic; resume inspects the filesystem). Cross-volume copies record
  `Started` (fsync) before writing `<dst>.trs-part`, which is fsynced and atomically renamed, so a
  destination only ever appears complete. Tested by crashing at **every** step, on both paths, and
  resuming in a fresh `DiskIo`.

## Relocation (SPEC 7A.5)

Three calls, driven by the engine, so that **the source is deleted only after the engine's record points at the
destination**:

```rust
// 1. new locations: one Option<PathBuf> per file index (None / padding = leave alone; dst == current = skipped)
let dst = h.plan_paths(&new_root, new_staging.as_deref(), Location::Final)?;   // root change; or build absolute paths yourself
// downloading torrent: h.quiesce_writes()?.wait()?;   (write_block answers Busy; resolves when queued writes hit disk)
let rep = h.relocate_prepare(journal, dst, RelocateOptions { hashes: Some(hashes), ..Default::default() })?.wait()?;
// 2. the switch (milliseconds, no I/O): either of
h.set_paths(new_root, new_staging);                 // Relative layout paths follow the root
h.switch_layout(Arc::new(layout.with_paths(new_file_paths)?))?;  // absolute per-file paths (per-file paths)
// ...engine persists its record (placement paths / save root), flush_now()...
h.relocate_mark_switched(&journal)?;
h.relocate_commit(journal, Some(stop_at))?.wait()?;       // copy: delete sources + prune emptied dirs; drop journal
// or, instead of the switch, on any failure / user cancel: h.relocate_rollback(journal)?.wait()?;
// downloading torrent: h.resume_writes();
```

```rust
pub struct RelocateOptions {
    pub hashes: Option<Arc<[u8]>>,   // v1 piece hashes: verify every piece while copying
    pub have: Option<PieceSet>,      // partial download: verify only these pieces, skip never-created files, zero blocks become holes
    pub buffer_bytes: usize,         // 4 MiB default, clamped 64 KiB..=8 MiB
    pub nocache: bool,               // F_NOCACHE on both ends (default true)
    pub bandwidth: u64,              // bytes/s, 0 = unlimited
    pub force_copy: bool,            // use the copy path on one volume (tests)
    pub fsync: bool,                 // default true
    pub reserve_bytes: u64,          // free space that must remain on the destination volume, 1 GiB default
    pub cancel: CancelToken,
    pub progress: Option<Arc<dyn Fn(u64 /*done*/, u64 /*total*/) + Send + Sync>>,   // <= ~10 Hz
    pub fault: Option<FaultHook>,    // tests: simulated crash before every mutation
}
pub struct RelocateReport { method: RelocateMethod /*Renamed|Copied*/, files, unchanged, missing_skipped, bytes, verified_pieces }
pub enum RecoverState { Nothing, Incomplete, Prepared, Switched, Done }
pub struct Recovered { state: RecoverState, method: Option<RelocateMethod>, entries: Vec<RelocateEntry /* file, src, dst, create_empty */> }
```
New errors: `VerifyFailed { piece }`, `InsufficientSpace { needed, available }`, `InvalidState`; plus the existing
`DestinationExists { file }`, `MissingData`, `Truncated`, `Cancelled`, `Interrupted`.

**Method.** All files same device as their destination's nearest existing ancestor: `Renamed` (directories created first, then
`rename_excl` per file; an empty file whose source never existed is created). Otherwise `Copied` for the whole torrent: free space
per destination volume is checked first (`sum of sizes + reserve_bytes`, else `InsufficientSpace`, nothing written), then every
file is streamed to `<dst>.trs-part` with three stages that overlap (read, SHA-1 of every piece as it passes, write), pieces
spanning files and BEP 47 padding handled by one running hasher, then every part is fsynced (mtime and mode copied, xattrs/ACLs are
not), and only after the whole stream verified are the parts published with `rename_excl` (a half-verified file never appears at a
final name). A bad piece fails with `VerifyFailed { piece }`, everything this call created is removed, the source is untouched.
The sources are re-stat'ed after the copy: a file that changed under us fails the move. A destination that exists (or appears
meanwhile: `RENAME_EXCL`) is `DestinationExists`; for renames the files already moved are renamed back. Nothing is ever overwritten.

**What a reader sees.** A copy holds no lock and touches no source: reads keep being served from the source (own fds, `Arc<File>`
survive anything) until `set_paths` / `switch_layout`, which drops the torrent's cached fds and swaps the layout without
waiting for in-flight reads (they keep their `Arc<Layout>` and `Arc<File>`). For renames, files vanish from the old path a moment
before the engine switches: while a rename relocation is pending a read that finds its file missing re-resolves with the
current layout every 2 ms (up to 3 s) instead of failing; the flag clears at the switch, at `commit`/`rollback`, and on failure.
`relocate_commit` first waits (<= 5 s) for read jobs submitted before the switch, invalidates the fd cache once more and only then
unlinks sources. Fix for downloading torrents: `quiesce_writes()` before `prepare`, `resume_writes()` after the switch
(through-written blocks of open pieces are on disk and are copied with everything else).

**One maintenance thread** is occupied for the whole `prepare`. Buffers: 4 x `buffer_bytes` (16 MiB at the default) exist only
while a copy runs; nothing is kept per torrent when idle. Per torrent, relocation added about 24 bytes to `TorrentShared`
(now 232 B: structure digest, two in-flight counters, two flags).

### Journal and recovery matrix

File `TRSR`, v1: header (structure digest, file count; paths are not part of it, so the journal survives the switch), a
checksummed **Plan** record (method; per file: index, src, dst, "create empty"; the topmost directories the move will create),
then 5-byte records `Started` (fsync), `Publishing(file)` (copy; fsync *before* the publishing rename), `Done(file)`, `Prepared`
(fsync), `Switched` (fsync), `SourcesGone`. Torn tails are ignored; a foreign/garbled journal is `CorruptJournal`.
`prepare` refuses (`InvalidState`) while the journal holds an unfinished plan. `relocate_recover` is cheap and synchronous.

| Crash point | On disk | `relocate_recover` | Engine's record | Engine does |
|---|---|---|---|---|
| before the plan is durable / during `Started` | nothing changed (a stray journal is removed by `recover`) | `Nothing` | old | retry the move if still wanted |
| after `Started`, dirs created / some files renamed / mid-copy (parts) / mid-publish | partial: created dirs, `.trs-part`, some published copies or some renamed files | `Incomplete` | old | `relocate_rollback` (deletes parts and our copies; renames files back; prunes dirs it created; removes journal), then retry |
| copy verified and published, `Prepared` not yet written | all copies in place, journal says `Incomplete` | `Incomplete` | old | `relocate_rollback` (safe) |
| after `Prepared`, engine not yet switched/persisted | rename: files at `dst`; copy: both copies | `Prepared` | old (`src`) | `relocate_rollback`, or redo the switch + persist + `mark_switched` + `commit` |
| after `Prepared`, engine persisted but `Switched` not yet recorded | as above | `Prepared` | new (`dst`) | `relocate_commit` (valid from `Prepared`) |
| after `Switched`, before/during source deletion | `dst` complete; sources partly or fully there | `Switched` | new | `relocate_commit` (rollback refuses with `InvalidState`) |
| `SourcesGone` written, before the journal is removed | `dst` only | `Done` | new | `relocate_commit` (finishes pruning, removes journal) |
| journal gone | final state | (`Nothing`) | new | nothing; `commit`/`rollback` on a missing journal are no-ops |

The decision when the state is `Prepared` is the engine's: compare its persisted record with `Recovered::entries` (`src`/`dst` per
file). `commit` never deletes a source unless its destination exists with the right size (`MissingData` otherwise, nothing deleted).
Directory pruning in `commit` stops at `stop_at` (never removed, nothing above it); `None` prunes nothing.
`rollback` and `commit` are idempotent.

## Memory

Per-torrent resident cost (measured with `TorrentHandle::resident_bytes`, excludes malloc overhead):

| State | Cost |
|---|---|
| `TorrentShared` struct | 168 B (+48 B `Paths`, + root path string) |
| `Layout` | 48 B + **16 B per file** + path bytes (one blob, no per-file allocation) |
| 1-file torrent, Warm | ≈ 320 B |
| 5-file torrent, Warm | ≈ 1.1 KB |
| **Cold** (`drop_layout()`) | ≈ 230 B + root path; only an 8-byte layout digest remains |
| Hashes (Hot only) | 20 B/piece in an `Arc<[u8]>` shared with the engine; `detach_hashes()` frees the reference |
| Partial piece | 72 B + bitmap (1 bit/block) + the piece buffer (counted in the global 32 MiB budget) |

`drop_layout()` refuses with `Busy` while pieces are in flight. `rebuild_layout(layout)` accepts
only a layout with the same digest (offsets, flags, paths), so a stale `.torrent` cannot be
swapped in. Global fixed cost: 4 I/O + N hash + 3 maintenance threads (idle, parked), 512-entry
fd table, ≤ 32 MiB write cache (only while downloading; zero when idle).

## Dependencies (justification)

| Crate | Why | Notes |
|---|---|---|
| `sha1` 0.11, `sha2` 0.11 | SHA-1 piece hashing, SHA-256 for v2 | **Must be 0.11.** 0.10.x has no aarch64 SHA backend for SHA-1 (portable code, 1.28 GB/s measured) and needs the `asm` feature for SHA-256. 0.11 detects the ARMv8 SHA instructions at run time (`cpufeatures`): 2.7–2.8 GB/s. `default-features = false`. |
| `crossbeam-channel` 0.5 | bounded MPMC queue for the explicit thread pools, `default-features = false` | std `mpsc` has no bounded MPMC receiver. |
| `libc` 0.2 | `setrlimit`, `fcntl(F_PREALLOCATE)`, `fcntl(F_NOCACHE)`, `sysctlbyname`, `setpriority`, `renamex_np(RENAME_EXCL)`, `statvfs`, `EXDEV` | unsafe, each call has a `// SAFETY:` comment (`src/fsutil.rs`). |
| dev: `proptest`, `tempfile` | layout property tests, temp dirs | |

No async runtime and no `futures` crate: the oneshot future is 60 lines in `src/oneshot.rs`.

## Benchmarks

`cargo bench -p trs-storage --bench storage_bench` (env `TRS_BENCH_MB`, `TRS_BENCH_DIRS`). It writes
under `bench/downloads/storage-bench/` and `$TMPDIR`, and deletes everything afterwards.
Numbers from an Apple M4 (4 perf cores), macOS, release+LTO:

| Test | External volume (bench/downloads) | `$TMPDIR` (internal SSD) |
|---|---|---|
| SHA-1, 1 thread, 4 MiB pieces | 2.70 GB/s (HW) vs 0.35 GB/s portable, 1.28 GB/s sha1 0.10 | same |
| SHA-256, 1 thread | 2.47 GB/s | same |
| SHA-1, 4 threads aggregate | 9.7 GB/s | same |
| Verified piece writes, 2 GiB, 6 pieces in flight, to page cache | 5.5 GB/s | 6.4 GB/s |
| … including `fsync` of the whole file | 0.47 GB/s (drive-limited) | 1.27 GB/s |
| Verified writes with cache overflow (write-through path) | 2.9 GB/s / 0.48 incl. fsync | 4.8 GB/s / 1.35 incl. fsync |
| Random 16 KiB reads, raw `pread` 1 thread | 8.9 GB/s (544 kIOPS) | 12.8 GB/s |
| Random 16 KiB reads via `read_block`, window 64 | 11.3 GB/s (691 kIOPS) | 10.8 GB/s |
| … 4 peers x 32 outstanding | 10.6 GB/s | 10.9 GB/s |

Targets: ≥ 1 Gbit/s (0.125 GB/s) per core budget is exceeded by orders of magnitude; hashing a
1 Gbit/s stream costs ≈ 5% of one core.

## Tests

`cargo test -p trs-storage`: layout property tests (random file sizes incl. empty and padding files,
piece sizes; tiling, round trips, boundaries), write→read round trip, hash-failure path,
write-cache budget pressure, write-through verification, concurrent writers, recheck over a generated
multi-file torrent (corruption, missing, truncated, progress, cancel, scrub throttle), resume
fingerprints, missing file/directory/root/volume behaviour, fd LRU with 2,000 files and limit 64,
Cold-tier layout drop/rebuild, finalize (full flow, crash-at-every-step on both move paths, foreign
destination, missing source, unplugged destination volume, foreign journal).
`tests/relocate.rs`: same-volume rename under a concurrent reader (with and without fd-cache churn, which exercises the
read retry), collisions (before and midway: renames are undone), hashed forced copy over odd piece sizes / padding / empty
files, corrupt source (`VerifyFailed` with the right piece, destination cleaned), truncated/missing sources, cancel,
free-space refusal, bandwidth pacing, unchanged/`None` destinations, crash at **every** step on both paths followed by
recovery in a fresh `DiskIo` (+ a fresh relocation), switch-then-crash during commit, commit refusing to delete without a
complete destination, `switch_layout` (geometry check, Cold round trip), `quiesce_writes`, a partial download relocated
and then finished at the new place, and a **real cross-device copy onto a temporary APFS disk image**
(`hdiutil create/attach`, always force-detached by a drop guard; the test skips itself when hdiutil is unavailable).
`cargo test --release -p trs-storage --test relocate -- --ignored --nocapture` prints throughput (`RELOC_MB` sets the size).

### Relocation throughput (Apple M4, internal SSD, release; single runs, noisy on a shared machine)

| Case | Result |
|---|---|
| same volume, 2,000 files of 4 KiB (`rename_excl` + journal) | ~14,000 files/s (about 145 ms), commit 9-12 ms |
| copy 1 GiB, internal SSD to internal SSD, hash + `F_NOCACHE` | 1.3-1.4 GB/s |
| ... no hashing, `F_NOCACHE` | 1.6-1.7 GB/s |
| ... hash, page cache on / no hash, page cache on | 1.6 / 1.8 GB/s |
| copy 1 GiB onto an APFS disk image on the same SSD, hash + `F_NOCACHE` | 0.5-1.4 GB/s (an image on the same SSD is not representative) |

Hashing runs on its own thread, so on a fast SSD it costs 0-30 % of throughput and on slower targets nothing (read, hash and write overlap).

## Not included / follow-ups

* BEP 52 merkle (per-file 16 KiB-block SHA-256 trees): the SHA-256 primitives are exposed, the
  verification pipeline is v1 only.
* `fsync` uses `sync_all` (not `F_FULLFSYNC`).
* Storage only offers `Absolute` per-file targets; which torrents use them is the engine's business.
* Relocation copies mtime and mode but not extended attributes/ACLs (a quarantine xattr or Finder tags do not follow a cross-volume copy; renames keep them). No `copyfile(3)`/clonefile: the point is to hash while copying.
* `RENAME_EXCL` is not supported by every filesystem (some exFAT/SMB volumes): there `rename_excl` falls back to check-then-rename, which is racy.
* A relocation occupies one of the (default 3) maintenance threads for its whole duration; the engine limits copies to one per destination volume.
