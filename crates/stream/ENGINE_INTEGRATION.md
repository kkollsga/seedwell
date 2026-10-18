# What the engine must add for streaming (SPEC 7F)

`trs-stream` is finished and tested against a fake swarm; nothing in `crates/engine` calls it yet. This
is the complete list of engine work, with the places to change (paths relative to `crates/`; line
numbers as of this writing). `trs-engine` gains a dependency on `trs-stream` (not the other way round).

## 0. Ownership

* One `trs_stream::StreamServer` per session, started lazily on the first `stream_url` (no listener
  and no tasks while nobody streams: idle budgets in SPEC 2 stay untouched). Keep it in the session
  struct (`engine/src/session/mod.rs`) next to the other long-lived services; `Cmd::Shutdown` calls
  `server.shutdown().await`.
* `OwnerId` = the torrent's `TorrentId` as `u64`.

## 1. A `PieceSource` per torrent (`engine/src/stream_source.rs`, new)

Everything needed already exists in the torrent's `DiskCtx` (`engine/src/peer.rs:116`, set once in
`Torrent.ctx.disk: OnceLock<DiskCtx>`):

| Trait method | Implement with |
|---|---|
| `geometry()` | `disk.piece_len`, `disk.total_len` |
| `has(p)` | `disk.have.get(p)` (`SharedHave`, lock-free atomic bits, already read by every peer task) |
| `subscribe()` | a `watch::Sender<u64>` owned by the torrent actor, see section 4 |
| `read(off, len, buf)` | split at piece boundaries; per piece `disk.handle.read_block(piece, begin, n, buf)` (`trs-storage`, goes through the fd cache; the `Vec` is reused as the storage API already allows; `peer.rs:924` is the existing upload-path example). Padding files read as zeros, which is correct |
| `piece_progress(p)` | optional. The picker knows partial pieces (`Picker.partial`, `picker.rs:40`); publish `received * 16 KiB` for open partials in a small `Arc<[AtomicU32]>`/map the adapter can read. Without it a response times out after 60 s on a piece that is merely slow |
| `set_urgent(id, ranges)` / `clear(id)` | `try_send` a `TorrentCmd::StreamHints { id, ranges: Box<[UrgentRange]> }` / `ClearHints(id)` to the torrent actor (`TorrentCmd`, `torrent/mod.rs:69`). Never block; the call comes from a stream task |

The adapter holds the `Arc<DiskCtx>` (make `DiskCtx` shared with `Arc` instead of the `OnceLock` value),
the epoch sender and the hint channel. A torrent without metadata or not yet `Hot` cannot stream:
`stream_url` returns an error (see section 5).

## 2. Deadline mode in the picker (`engine/src/picker.rs`)

`Picker::pick` is rarest-first (`best_new_piece`), partial pieces first (`take_from_partials`). Add:

* `Picker::set_stream_hints(stream: u64, ranges: &[UrgentRange], now: Instant)` and `clear_stream_hints(stream)`.
  Store per stream a short list `(first, end, priority, absolute_deadline_of_first = now + deadline, step)`.
  The slice is the stream's complete picture: replace it. Deadlines are relative to the call.
* Ordering inside `pick`, before the existing logic, for pieces the peer has and that we do not:
  1. **Urgent** pieces from all streams ordered by absolute deadline; ties broken round-robin by stream
     id (keep a `last_served` stream and prefer another stream on ties). This is the fairness rule of 7F.3:
     urgent windows of all live streams share the urgent bandwidth fairly, because every stream's first
     piece has deadline 0 and the deadlines then advance by `step`.
  2. **ReadAhead** pieces in piece order (per stream, so it never starves other streams' urgent pieces).
  3. Everything else exactly as today (partial first, rarest first, random tie-break). **Never** switch
     the whole torrent to sequential.
* Urgent pieces bypass `max_open` (the cap on partial pieces, `picker.rs` `Picker::new`), otherwise a
  window of 8 MiB / 256 KiB pieces cannot open.
* Pieces with priority 0 (unselected files) stay excluded: select the file first (section 5).
* A hint that disappears (`clear`, or a replaced picture without that piece) only changes ordering:
  already requested blocks stay requested (SPEC: "become normal priority, not cancelled unless they are
  duplicates"), so `request_lost`/cancel paths are untouched.
* The hint list is tiny (<= ~10 ranges per stream), the check per piece is a range comparison: no
  per-piece storage, nothing for non-streaming torrents (`Vec::new()`, 24 bytes).

## 3. Duplicate requests for urgent blocks across fast peers (`engine/src/torrent/events.rs`)

`fill_requests` (`events.rs:496`) calls `pk.pick(...)` once per peer; duplicates exist only in endgame
(`pk.in_endgame()`, `events.rs:577`, which already cancels the other peers' copies with
`PeerCmd::Cancel` when a block arrives).

* Let `pick` hand a block of an **Urgent** piece to up to `urgent_dup` (2 or 3) distinct peers even when
  it is already requested, provided the peer's recent rate is among the fastest few (`Peer` already tracks
  rate for the choker/`depth`). Do not duplicate ReadAhead.
* Reuse the endgame cancel code for the arrival: generalise the condition at `events.rs:577` from
  `pk.in_endgame()` to `pk.in_endgame() || pk.is_urgent_piece(piece)` so the other peers' in-flight copies
  of that block are cancelled the moment one copy arrives.
* Hash-failed bytes still count as downloaded (ENGINE.md 6); duplicates that arrive before the cancel are
  counted as received payload like endgame duplicates today (stats rules unchanged).
* Keep the total duplicate overhead bounded: only blocks of pieces within the urgent window of a live
  stream, so at 6 MB/s it is a few MB per seek, not a permanent 2x.

## 4. The hash-verified notification (`engine/src/torrent/maintenance.rs`)

In `piece_verified` (`maintenance.rs:349`), right after `d.have.set(piece)` add the epoch bump:

```rust
d.have.set(piece);                                   // 1. bit first
stream_epoch.send_modify(|e| *e = e.wrapping_add(1)); // 2. then the counter (ordering contract of PieceSource)
```

Also bump the epoch after `set_have` from recheck/resume. The sender lives in the torrent actor next to the
picker; drop it when the torrent task ends (the stream server then ends every response with
`EndReason::SourceGone`). `send_modify` on a `watch` with no receivers is a few atomics: no cost when nobody
streams.

## 5. Selecting the file on stream, and the tier/queue

`stream_url(id, file)` in the session (`engine/src/session/cmds.rs`, next to `set_file_priorities`, line 474):

1. Torrent must have metadata; else `EngineError` ("metadata not yet available").
2. Ensure the file is selected: if its priority is 0 set it to Normal via the existing path
   (`TorrentCmd::SetFilePriorities`, `maintenance.rs:591` -> `piece_priorities` -> `Picker::set_priorities`).
   Selecting for streaming selects it for download (SPEC 7F.2). Remember which files were selected only for
   streaming only if the UI wants to offer undo; the SPEC does not require it.
3. If paused/queued: resume and bypass the queue (`QueueManager`, `engine/src/queue.rs`); streaming is an explicit
   user action. If the torrent is Cold/Warm (`tier.rs`) promote it to Hot; the `DiskCtx` exists only then.
4. Give a streaming torrent the High weight in the global `RateLimiter`/demand weighting (4:2:1, `ratelimit.rs`,
   `choker.rs`) while it has live cursors, so it wins bandwidth against background torrents. Cross-torrent fairness
   is bandwidth weighting; deadlines only order pieces inside one torrent.
5. Build the `FileRange { index, name: last path component, offset: layout.file_start(i), len: layout.file_len(i) }`
   (`Layout` in trs-storage; skip padding files) and call `server.open(source, file, id as u64)`.
6. If the file is already complete (all its pieces present) return no URL: the app opens the file itself
   (SPEC 7F.3, finished files need no server). Expose `complete: bool` in the reply.

## 6. Web seeds

`url-list` (BEP 19) is already parsed (`metainfo` `url_list`, `magnet.rs` `web_seeds`) but the engine has no web
seed client yet. For streaming:

* A web seed fetches **only Urgent pieces** (SPEC 7F.2: "and from web seeds when present"): when the picker holds an
  urgent piece with no peer able to serve it soon (or always, for the first piece of a window), issue
  `GET <seed>/<file path> Range: bytes=<piece start - file start>-<end>` with `trs-http` (it already does ranged,
  resumable segment downloads, `crates/http`), write the bytes with `TorrentHandle::write_block(piece, begin, data)`
  as 16 KiB blocks so the normal hashing/`Verified` path and the epoch bump apply. Treat it as one more "peer"
  in the picker so duplicates are cancelled the same way (a cancelled HTTP range is just dropped).
* For multi-file torrents the web seed URL is `<url-list base>/<torrent name>/<path>`; a piece spanning two files needs
  two ranged requests. Verify with the piece hash like any other source; a bad web seed is dropped after one failure.

## 7. Closing streams on removal (and pause)

* `remove_torrent` / `remove_waiting` (`session/cmds.rs:505-531`): call `streams.close_owner(id as u64)` **first**,
  before `stop_live`, so responses end before storage is torn down (otherwise `read` could see `MissingData`).
* `Cmd::Pause`: leave streams open; responses stall and end after 60 s by themselves, and resume works. If product
  wants pause to end playback, call `close_owner` there too.
* App quit: `Cmd::Shutdown` -> `server.shutdown().await`. A finished/seeding torrent needs nothing.
* Move/relocate (`session/moves.rs`): the read path goes through `TorrentHandle`, which follows `switch_layout`, so
  streams survive a move; if the move fails with `MissingData` the response ends with `EndReason::Error`.

## 8. Control API and FFI

Add to the control protocol (`control/src/proto.rs`, the request table around lines 814-837, same macro style,
permission `Control`):

```text
StreamUrl(StreamParams { id, file })   = "stream_url",  Control   -> StreamInfo
StopStream(StopStreamParams { token }) = "stop_stream", Control   -> ()
```

`StreamInfo` (serde, also the FFI record): `url`, `token`, `file_name`, `file_len`, `content_type`,
`container` (`mp4_family | matroska | avi | mpeg_ts | other_video | audio | unknown`), `is_mp4_family: bool`,
`complete: bool`. All of it is already in `trs_stream::StreamHandle` (`container`, `is_mp4_family`,
`content_type`); `complete` comes from step 6 above. Handlers go in `control/src/ops.rs`/`engine_backend.rs`
(`Backend` trait: `stream_url`, `stop_stream`). The agent tools (`stream_url(id, file)`, `stop_stream(token)`,
SPEC 7F.3) are these two requests; log them in `agent_activity` like other control calls.

FFI (`ffi/src/lib.rs`, UniFFI, next to `pause`/`remove` at lines ~124-140; mock in `ffi/src/mock.rs`):

```rust
pub fn stream_url(&self, id: String, file: u32) -> Result<StreamInfo, CoreError>
pub fn stop_stream(&self, token: String)
```

## 9. What the app (Swift) needs from the crate

Player detection (LaunchServices) and launching (URL schemes, `open -a`) are app-side. Per SPEC 7F.3 the app needs
only the `StreamInfo` above:

* `url`: what to hand to the player (`iina://weblink?url=<percent-encoded url>`, `open -a VLC <url>`, `open -a
  "QuickTime Player" <url>`, `infuse://x-callback-url/play?url=...`).
* `container` / `file_name` extension / `content_type`: ranking and filtering of "Play With" entries.
* `is_mp4_family`: hide QuickTime Player (AVFoundation) for MKV, AVI, TS, WebM. AVFoundation only opens
  MP4/M4V/MOV (M4A for audio).
* `token`: for `stop_stream`. The app cannot see a player window close, so it should stop the stream when the
  user stops it in Seedwell, on Play of another file in the same player, and at quit (the engine also closes it on removal). A stream
  with no live connection costs one `Arc` and (until the index is present) a few urgent hints; it is safe to
  leave open.

## 10. Verified player behaviour that shapes the engine work

Measured with the fake swarm at 6 MB/s, 296 MiB 1080p H.264 file (600 s):

* AVFoundation opens with `bytes=0-1`, then **`bytes=0-<last>` (a full-file open-ended range)** and a tail range for
  `moov`, aborts and re-requests (`bytes=48996-...`) repeatedly, then seeks with a fresh open-ended range. All of it is
  plain open-ended ranges here, so per-request cursors with hang-up detection matter more than tiny-range coalescing.
* libavformat (mpv/IINA engine) does head, tail (`moov`), then `bytes=48-`; a seek is one new open-ended request.
* **Piece size dominates seek latency**: a seek needs a whole new piece verified before the first byte. 4 MiB pieces
  at 6 MB/s: AVFoundation seek-to-playing 1.8 s, libavformat 0.7 s. 16 MiB pieces: AVFoundation 6.0 s (over the 4 s
  budget), first frame 3.8 s. The engine should therefore, for streaming only, prefer sub-piece parallelism for the
  urgent piece (all blocks of the first urgent piece from all fast peers at once, section 3) and accept that
  torrents with 16 MiB pieces cannot meet the seek budget below ~7 MB/s.
