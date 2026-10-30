# trs-fswatch

Watches one sync folder recursively and tells the sync scanner which paths to look at (SPEC §7G.2, "Watching").
Two parts:

* **`watch(root, opts) -> Watcher`**: a backend on its own std thread, sending `RawEvent`s on a bounded std
  `mpsc` channel. FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows, a metadata walk anywhere
  (or on request, or where the native one cannot start). No async
  runtime, so the daemon, the app and tests use it the same way.
* **`aggregate::Aggregator`**: a pure state machine on a virtual clock (`Millis`) that turns raw events into
  `Batch`es (`Paths`, `Dir`, `All`) for the scanner.

```rust
use trs_fswatch::{watch, WatchOptions, RawEvent, aggregate::{Aggregator, Batch}};

let w = watch(&root, WatchOptions { since: saved_token, ..Default::default() })?;   // before the initial scan
let mut agg = Aggregator::new();
loop {
    let timeout = agg.next_deadline().map_or(HOUR, |d| d.saturating_sub(now_ms()));
    if let Ok(ev) = w.events().recv_timeout(Duration::from_millis(timeout)) {
        // RootChanged / HistoryDone are also useful to the caller directly (pause the folder, end of replay).
        agg.push(ev, now_ms());
    }
    for batch in agg.poll(now_ms()) { scan(batch) }            // Paths: stat each; Dir: walk; All: walk all
    if let Some(t) = agg.take_resume() { persist(t.to_bytes()) } // only after those batches were scanned
}
```

## Events are hints

The consumer stats every path it is given and believes the disk. A wrong, duplicated or coalesced hint costs a
stat; it never decides what is synced. What must never happen is a change that is never reported, so every way
of losing events becomes a `RawEvent::Rescan`:

| Loss | Reported as |
|---|---|
| FSEvents `KernelDropped` / `UserDropped`, inotify `IN_Q_OVERFLOW`, ReadDirectoryChangesW overflow (a completion with no records, `ERROR_NOTIFY_ENUM_DIR`, a failed read) | `Rescan { rel: None, reason: Dropped }` |
| FSEvents `MustScanSubDirs`, a directory-level event without file detail, a mount/unmount inside the root | `Rescan { rel: Some(dir) or None, reason: MustScanSubDirs }` |
| FSEvents `EventIdsWrapped` | `Rescan { rel: None, reason: EventIdsWrapped }` |
| The consumer fell behind and the channel (8192 events) filled up | `Rescan { rel: None, reason: ChannelFull }`, sent before anything else once there is room (a backend timer retries even if no new event comes) |
| A resume token whose history cannot be replayed (other volume, reset store, ID from the future, no history on that volume, inotify, polling) | `Rescan { rel: None, reason: NoHistory }`, then `HistoryDone` |
| inotify: a new directory, watched only after it appeared; ReadDirectoryChangesW: a directory created or moved in (its contents are not reported) | `Rescan { rel: Some(dir), reason: NewDirectory }` |
| ReadDirectoryChangesW: a name in its 8.3 short form that is not there under that name | `Rescan { rel: Some(parent) or None, reason: MustScanSubDirs }` |
| inotify: a directory that could not be watched (watch limit, permissions) | `Rescan { rel: Some(dir), reason: WatchFailed }` |
| An FSEvents path that cannot be placed under the root | `Rescan { rel: None, reason: UnknownPath }` |
| The root moved, deleted or replaced | `RootChanged` (the aggregator treats it as `All`) |

## Guarantees

* **Order.** Events from one watcher arrive in the order the backend produced them. A `Resume(token)` covers
  every event sent before it on the channel.
* **Resume.** With `since: Some(token)`, history since the token is replayed first, then exactly one
  `HistoryDone`, then live events. With no usable history: `Rescan { rel: None }` then `HistoryDone`. With
  `since: None` there is no `HistoryDone`; only changes after `watch` returns are reported, so start the watcher
  before the initial scan.
* **Tokens.** Sent at most every `resume_interval` (5 s) and only when changed; never during a replay. While the
  root is quiet, the token is advanced to the host's current event ID every 12 intervals (after
  `FSEventStreamFlushSync`, so nothing for this root numbered before it is still undelivered); otherwise a
  restart would replay a long stretch of unrelated history. FSEvents only; the other backends send none.
* **Stop.** `Watcher::stop` (or drop) returns only when the backend thread has ended. For FSEvents: stop,
  invalidate, an empty `dispatch_sync_f` on the stream's serial queue (waits for a callback that was running or
  queued), release the stream, release the queue. No callback runs after `stop` returns (tested under a burst,
  with a callback counter).
* **Aggregator** (all tested, with proptest over random event streams on a virtual clock):
  * a path is ready after 2 s without a new event; one that keeps changing still goes out 60 s after its first
    pending event;
  * removes and renames are held until 10 s after the last remove or rename, then go out together (both halves
    of a rename in one batch), capped by the same 60 s;
  * more than 128 pending paths directly in one directory become `Dir(that dir)`; more than 512 pending
    entries become `All`;
  * a rescan absorbs the paths and smaller rescans it covers; `All` comes alone; `Paths` are sorted and
    unique;
  * `next_deadline` is exact (polling at it always emits something), every event is covered within 60 s, and
    `take_resume` releases a token only after every event before it has gone out.

## Backends

**FSEvents** (macOS). Our own `extern "C"` bindings: `FSEventStreamCreate`, `FSEventStreamSetDispatchQueue`,
`Start`/`FlushSync`/`Stop`/`Invalidate`/`Release`, `FSEventsGetCurrentEventId`, `FSEventsCopyUUIDForDevice`
(CoreServices), a few CoreFoundation calls (`CFStringCreateWithBytes`, `CFArrayCreate`, `CFUUIDGetUUIDBytes`,
`CFRelease`) and `dispatch_queue_create`/`dispatch_sync_f`/`dispatch_release` (libSystem). Flags:
`FileEvents | WatchRoot | NoDefer`, latency 0.5 s. The callback's context is an `Arc` whose count the stream
holds through the context's retain/release callbacks, so the pointer FSEvents passes back is always live.
The callback catches panics (an unwind into FSEvents would abort) and only copies paths and flags into the
channel. `UseExtendedData` (inode numbers) is not used: it needs `UseCFTypes` and CFDictionary parsing for
something the scanner gets from its own `lstat` anyway; plain `char **` paths are simpler to read safely.

**inotify** (Linux, `libc`). One watch per directory, added as directories appear (then that directory is
rescanned, since its first events may have been missed); a directory moved away loses its watches.
`IN_Q_OVERFLOW` means a full rescan. The thread waits in `poll(2)` on the inotify descriptor and a pipe that
stop closes. Records are parsed from bytes in safe code. When the root goes away it is retried every second and
reported with `RootChanged` again once it is back. Built and linted for Linux from this Mac with
`cargo +nightly clippy -Zbuild-std --target x86_64-unknown-linux-gnu`; its tests run only on Linux.

**ReadDirectoryChangesW** (Windows, `windows-sys`). One directory handle on the root (`FILE_LIST_DIRECTORY`,
every share mode, so the user can still rename or delete the root), recursive, 64 KiB reads (the network-share
maximum), overlapped with two buffers: the next read is posted before the completed one is parsed. Records map
to hints directly (renames come as old/new name pairs, so no inode pairing); a directory's own "modified"
record is left out (its entries' records say what changed). A directory created or moved in gets a
`NewDirectory` rescan. Overflow is a full rescan. Junctions and symlinks inside the root are reported as entries
and never followed (the system does not report changes inside their targets). Once a second the thread checks
that the root path still names the watched directory (volume serial and file index): `RootChanged` when it is
renamed, deleted or replaced, and again when it is back. No history: a resume is a full rescan. If the handle
or the first read cannot be had (a share that refuses change notifications), `watch` falls back to polling.
Stop sets an event, cancels the pending read and waits for it, then joins the thread. `debug::hold_reads` (tests
only) stops re-posting reads so the system's buffer overflows on purpose.

**Polling** (`Backend::Poll { interval }`, default 10 s; the native backend elsewhere). `read_dir` + `lstat`
of the whole tree each interval, compared with the previous walk (size, mtime, ctime, mode, inode; a
directory's own size and times are ignored, its entries are reported themselves). A path that disappeared and
one that appeared with the same inode are reported as the two halves of a rename. Never opens a file and never
follows symlinks. Costs one `lstat` per entry per interval, which is why it is the fallback.

## Unsafe code

`#![deny(unsafe_code)]`; `#[allow(unsafe_code)]` only on `fsevents` (18 `unsafe` blocks), `inotify` (8) and
`windows` (Win32 calls and one `Send` for its read slots),
each with a `// SAFETY:` comment. The aggregator, polling, the channel and everything else are safe code.

## Dependencies (each justified)

| Crate | Why |
|---|---|
| `libc` | inotify, `poll`, `pipe2`, `read` on Linux, and `dev_t` for `FSEventsCopyUUIDForDevice`. Already in the workspace. |
| `windows-sys` *(Windows)* | `ReadDirectoryChangesW`, `CreateFileW`, overlapped I/O and events. Already in the workspace (trs-platform). |
| `proptest` *(dev)* | Properties of the aggregator over random event streams. |
| `tempfile` *(dev)* | Temporary folders for the integration tests; never the home folder. |

No `notify`/`fsevent-sys`/`inotify` crates: `notify` hides the FSEvents event IDs and the volume UUID that
resume needs, and brings its own threads and channel types; the bindings needed here are a few dozen lines.
No `core-foundation` crate for five CF calls.

## FSEvents quirks handled

* **Flags accumulate.** An event carries every flag seen for the path recently (`Created | Modified |
  Removed` for a file created and deleted inside the latency, and sometimes for longer). The hint picks the most
  important flag (Removed, Renamed, Created, Modified, Meta) and the scanner stats anyway.
* **Both halves of a rename are `ItemRenamed`**, with nothing pairing them; the aggregator's hold keeps them in
  one batch.
* **History replay is per path and in path order**, not time order, coalesced per path, file-level when the
  stream asked for `FileEvents` (seen in the replay test: a rename, a new directory and its file come back
  sorted by path). Hence "replay until `HistoryDone`", and no tokens before it.
* **Paths are canonical** (`/private/var/...` for `/var/...`), so the root is canonicalized first; a case
  mismatch on a case-insensitive volume is matched ASCII case-insensitively.
* **`FSEventStreamFlushSync` waits for the callbacks it triggers**, so calling it on the stream's queue, or
  while holding a lock the callback takes, would deadlock; the timer thread calls it without the state lock.
* **Event IDs are host-wide**, so a quiet root still sees the global counter move; the idle advance uses it.

## Limits

* **FSEvents history can be disabled or missing**, typically on external volumes (`.fseventsd/no_log`,
  read-only or non-Apple file systems, or a store reset). `FSEventsCopyUUIDForDevice` then returns no UUID, no
  tokens are sent, and a later `since` gets `Rescan { rel: None, reason: NoHistory }`. The live stream still
  works. A volume moved to another Mac has another event store: its UUID differs, again a full rescan.
* inotify has no history (every resume is a full rescan), is limited by `fs.inotify.max_user_watches` (past it:
  `WatchFailed` rescans, and that subtree is only seen by the periodic full scan), and does not see changes made
  through other mounts of the same files (NFS clients, bind mounts) or inside a symlinked directory.
* Symlinks inside the root are reported as entries, never followed.
* Polling misses a change that restores size and mtime exactly between two walks; it is a fallback, and the
  sync core's hourly stat-only scan (SPEC §7G.2) is the safety net for every backend.
* Event paths that are not valid UTF-8 are passed through as bytes; a root that is not UTF-8 cannot be watched
  with FSEvents (CFString), which is not possible on APFS/HFS+ anyway.
