# trs-stream

Seedwell's streaming server (SPEC 7F, "play while downloading"). It serves one file of a
downloading torrent over a loopback HTTP/1.1 server with `Range` support so the user's own player
(VLC, IINA, mpv, QuickTime Player/AVFoundation, Infuse...) can play it, and it tells the engine's
picker which pieces the player will need next.

It depends on **no other workspace crate** (only `tokio` and `getrandom`). The engine implements
[`PieceSource`] and calls [`StreamServer`]; see `ENGINE_INTEGRATION.md` for exactly what the engine
must add. The server holds no file data: one read chunk (256 KiB) per live response, nothing else.

## Public API

```rust
let server = StreamServer::start(Config::default()).await?;      // 127.0.0.1:<random port>
let h: StreamHandle = server.open(source /* Arc<impl PieceSource> */, FileRange { index, name, offset, len }, owner_id)?;
// h.url  = http://127.0.0.1:<port>/<128-bit hex token>/<percent-encoded file name>
// h.token, h.id, h.content_type, h.container, h.is_mp4_family, h.file_len
server.close(&h.token);          // one stream
server.close_owner(owner_id);    // every stream of a removed torrent
server.shutdown().await;         // stop accepting, close all, drain connections
```

`Config` (all defaults match the SPEC): `stall_timeout` 60 s, `chunk_size` 256 KiB, `linger` 3 s,
`stale_after` 8 s, `max_connections` 64, `plan` (urgent window 30 s / min 8 MiB, read-ahead 120 s,
head and tail 2 MiB, fallback bitrate 8 Mbit/s), `on_event` (observer for requests, chunks and ends:
used by the tests and handy for the app's debug log).

## HTTP behaviour

* Binds loopback only (`start` refuses any other address). Each accepted peer is checked with
  `peer_allowed` (loopback only; IPv4-mapped IPv6 loopback counts) and dropped otherwise. A `Host:`
  that is not `127.0.0.1`/`localhost`/`[::1]` gets 403 (DNS-rebinding guard).
* Unknown token (or `/`): 404. The name after the token is cosmetic. Methods other than GET/HEAD: 405.
  Malformed head: 400, oversized head (> 16 KiB): 431, HTTP/2+ or `Transfer-Encoding`: refused.
* `Range`: `bytes=N-`, `bytes=N-M` (M clamped), `bytes=-N` (suffix; N larger than the file gives the whole
  file), unsatisfiable gives 416 with `Content-Range: bytes */len`. Several ranges: the first
  satisfiable one is served as a single-part 206. Unknown units and invalid syntax (`5-2`) are
  ignored (200). `If-Range` is honoured against the stream's `ETag` (`"<token prefix>-<len>"`).
* Every 200/206/416 carries exact `Content-Length`, `Content-Range` (206/416), `Accept-Ranges: bytes`,
  `Content-Type` from the extension, `ETag`, `Cache-Control: no-store`. `bytes=0-1` probes
  (AVFoundation) get exactly 2 bytes with `Content-Range: bytes 0-1/<len>`.
* Keep-alive and pipelining; `Connection: close` honoured. HEAD never creates a cursor.
* Offsets are `u64` throughout (tested with a 5.5 GiB file inside a 6 GiB torrent).

### Never unverified data

A response is a loop: wait until the piece under the cursor is verified (`has`), read at most one
chunk that lies entirely inside the verified run, write it, repeat. It never sends zeros and never
reads a piece `has` has not reported. A wait with no progress for `stall_timeout` ends the response
(the connection is closed after a truncated body; players treat that as a dropped connection and
retry). "Progress" is growth of `PieceSource::piece_progress(piece)` (partial piece bytes), so a
slow piece on a slow swarm is not cut off, only a stuck one.

While parked (waiting for a piece) or writing, the server watches the socket: hang-up ends the
response at once and its urgency is dropped. Closing the stream, removing the torrent (`close_owner`),
or the engine dropping the source (the `watch` sender) also end responses immediately.

## Urgency (SPEC 7F.2)

Each response is a **cursor** (file offset of its next byte). A stream's cursors are merged by
`urgency::plan` into one picture, pushed with `PieceSource::set_urgent(stream_id, &[UrgentRange])`:

| Contribution | Pieces | Priority | Deadlines |
|---|---|---|---|
| index (head, tail, `moov`, `Cues`) | >= 2 MiB each end | Urgent | 0 |
| per cursor: urgent window | ~30 s of playback (bitrate = size / duration; 8 Mbit/s until known), at least 8 MiB | Urgent | `(piece start - cursor) / bitrate`, nearest first |
| per cursor: read-ahead | the following ~2 min | ReadAhead | continuing |

* **One picture per stream.** Overlapping windows are merged so no piece appears twice, with its
  earliest deadline. Several small ranged requests (AVFoundation) are one window, not a series of
  jumps: a request that completes normally lingers 3 s (`linger`) as a cursor at its end offset, and
  an update is only pushed when a cursor crosses into a new piece, a request starts/ends, or the index
  set changes (and only if the result differs from the last push).
* **Seeking.** The new request's cursor appears at once; a hang-up of the old connection removes its
  window at once (VLC and mpv close the old connection; AVFoundation also cancels). A cursor whose
  socket has not drained for `stale_after` (8 s: paused player) stops claiming urgency and
  comes back when it moves again.
* **Several streams.** Each `open` has its own `StreamId`, token and cursors. Deadlines are relative to
  "now" for every stream, so a deadline-ordered picker interleaves them (A0 B0 A1 B1...) and ties
  go round-robin by stream id; read-ahead stays per stream. The fake swarm in the tests implements
  exactly that and `two_streams_share_the_swarm_fairly` checks both finish together.

## Index first (`container.rs`, `index.rs`)

At `open` the head and tail (2 MiB each) are urgent. A task then runs a pure probe over the bytes as
they arrive:

* **MP4/MOV**: walk top-level atoms using only their 8/16-byte headers (`size == 1` 64-bit sizes and
  `size == 0` handled); after a big `mdat` the next header is read at `offset + size`; when `moov` is found
  (head or tail) its whole range becomes urgent and `mvhd` gives the duration (v0 and v1).
* **MKV/WebM**: EBML header, Segment, SeekHead -> positions of `Info` (read `TimecodeScale`, `Duration`)
  and `Cues` (header read at its position to get the range).

The probes (`probe_mp4`, `probe_mkv`) are synchronous state machines: `Probe::Need { offset, len }` or
`Probe::Done(ContainerInfo { duration, index })`. They never panic on garbage. The duration feeds the
bitrate for the windows. Index ranges stay urgent until present, then are released.

## `PieceSource` contract

```rust
pub trait PieceSource: Send + Sync + 'static {
    fn geometry(&self) -> Geometry;                      // piece_length, total_len (torrent)
    fn has(&self, piece: u32) -> bool;                   // O(1); verified and readable
    fn subscribe(&self) -> watch::Receiver<u64>;         // changes whenever any piece verifies
    fn piece_progress(&self, piece: u32) -> u32 { 0 }    // optional: partial bytes (resets stall timer)
    fn read(&self, offset: u64, len: usize, buf: Vec<u8>)  // torrent offset; buf reused; len <= 1 MiB
        -> impl Future<Output = io::Result<Vec<u8>>> + Send;
    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]); // non-blocking; replaces the picture
    fn clear(&self, stream: StreamId);                              // non-blocking
}
```

* Offsets in the trait are torrent offsets (storage `Layout` coordinates); `FileRange` places the file.
* Set the `has` bit **before** bumping the `watch` counter; dropping the sender = torrent gone.
* `read` is only called for byte ranges whose pieces `has` reported true. It may be split across pieces
  by the implementation. No per-byte allocation: the `Vec` goes in and comes back.
* `set_urgent` receives piece ranges `(first, end)` with `deadline` for the first piece and `step` per
  following piece, and a `Priority` (`Urgent` or `ReadAhead`). Treat deadlines as relative to the time of
  the call. Replace, do not accumulate.

## Tests

`cargo test -p trs-stream` (no network, no real home/Downloads; ffmpeg fixtures are cached in
`target/tmp/trs-stream-fixtures`, generated with `/opt/homebrew/bin/ffmpeg`, tests skip if it is missing):

* `src` unit tests: ranges, request parsing, planner, container probes (hand-built atoms and EBML), MIME.
* `tests/protocol.rs`: range forms, errors, non-loopback refusal, keep-alive/pipelining, verification gating,
  disconnect drops urgency, 50-seek storm (no leaks; RSS flat), stalled piece, slow-but-progressing piece,
  > 4 GiB offsets, two streams, removal mid-play, AVFoundation-style small parallel ranges, stale cursors.
* `tests/real_files.rs`: probes against real ffmpeg MP4 (moov at end / faststart) and MKV (Cues at end), and the
  stream finding the index first against a fake swarm.
* `tests/players.rs` (`#[ignore]`): real players; see the header of the file. `cargo test -p trs-stream --test players
  -- --ignored --nocapture --test-threads=1`. `TRS_PIECE_MIB` and `TRS_RATE_MBS` tune the fake swarm.
  `tests/players/avf.swift` is the AVFoundation probe (AVURLAsset + AVPlayer + seek), built with `swiftc`.

The fake swarm (`tests/common/fake.rs`) delivers pieces at a fixed rate: urgent hints by deadline (round-robin
between streams), then read-ahead, then random order; pieces can be stalled.
