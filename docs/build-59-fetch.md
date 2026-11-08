# Build 59: a fetch that shows how far it is, and the core's messages in a log (for the Windows app)

Branch `build-59-fetch-progress`, from master `d23d604` (build 58). Two problems the Windows agent found in build 58,
both fixed in Rust. Nothing in the UniFFI interface changed: no new types, functions or fields, so `app-windows/`
builds as it is. SPEC §7G.11 ("When a fetch gives up") and §9 (logging) say the same.

## 1. `sync_fetch(id, item, wait_ms)` returns within about `wait_ms`, with `done` rising

### What was wrong (two causes, each enough to give "never a partial `done`")

1. **The wait took the folder's core, which the pull holds for the whole download.** Every `sync_fetch` call made
   its request again (`od_fetch_wait` → `od_request` → `with_core`). The pull runs in rounds that hold the folder's core
   for a whole window of chunks (up to 64 MiB: the whole of a 3 or 12 MB file). Call 1 found the core free and came
   back at once; call 2 waited for the core, that is for the end of the round, that is for the whole file (4.7 s for
   3 MB, 23.6 s for 12 MB at 500 KB/s).
2. **The partial file grew only at the end of a window.** `done` is the size of the fetch's temp file in the cache.
   The puller asked a window of chunks of the network (`ChunkSource::fetch`) and wrote them only once *all* of them
   were in; with the download limit pacing the asking, that was the end of the file. So even a wait that came back in
   time could only have said `done 0`.

The in-process test reproduces both on build 58's code: `a wait of 250ms took 5.03 s (done so far [0])`, and with only
cause 1 fixed, `done` read `[0, 0, 0, …]` for every one of 20 waits.

### What changed

- `crates/sync-net/src/service_ondemand.rs`, `od_fetch_wait`: a request that **stands already** (every file not here
  is wanted, read from the index's read-only handle) is not made again, so a wait never needs the core while the file
  downloads. A request that must be made goes in on a task of its own and is waited for only until `wait_ms` ends;
  if the core is busy (a round for another file) the call returns *coming* (`waiting_for` `None`) and the request goes
  in by itself once the core is free. Later calls wait on that request and never give up meanwhile; `sync_cancel_fetch`
  stops it from going in.
- `crates/sync/src/pull.rs` + `crates/sync-net/src/source.rs`: new `ChunkSource::fetch_each`: the network source hands
  each chunk over **in order as soon as it is in** (the next chunk is asked while earlier ones are on their way, as
  before), and the puller verifies and writes it at once. `done` is therefore the bytes here so far, contiguous from
  the start, rising chunk by chunk (64 KiB chunks for files below 128 MiB, larger for larger files). A chunk that
  failed at a device is asked of another holder at once (before: after the whole window); each request has its 60 s
  from when it was asked.
- The NotHereYet give-up from build 58 is unchanged: 3 s grace with no connected holder, 30 s with no data from the
  holders while waited on (`FETCH_GRACE`, `FETCH_STALL`). A transfer that is slow but flowing never trips it, since the
  stall watches the bytes received.

### What the host does

Nothing new. Keep the loop as it is: `sync_fetch(id, item, 1000)`, then `CfReportProviderProgress(total, done)` after
each return until `local_path` is set. Each call now comes back after about 1 s (a little more on a busy machine) with
`done` where the download has got. (If `CloudHost.OnFetch` still has its own 30 s "no progress" timer, it no longer
fires on a slow link that delivers, but build 58 made it unneeded: the core gives up by itself.) The Mac's File
Provider gets the same, through the same call.

### Which QA assertions should now pass

`app-windows\qa\run.ps1 ondemandfetch` with the throttled read (`SEEDWELL_QA_LIMIT_LAN=1`, B's down limit 500 KB/s,
3 MB `big.bin`): the read takes about 6 s and the `on-demand: big.bin filled (…; progress shown N times while
fetching)` line in B's `app.log` has **N ≥ 2** (expect about 6 to 8: one report per 1 s wait, each with a new `done`).
Every `sync_fetch` call returns within about 1 s; none waits for the whole file. The 12 MB read the same way: about
25 s, about 25 reports. The rest of `ondemandfetch` (A gone: NotHereYet on `small.txt`, one notice, the failed
fetch in the log) is unchanged.

### Rust tests (run by `rust-tests.ps1`)

- `seedwell-sync-net` `ondemand::a_slow_fetch_answers_each_wait_in_time_and_shows_how_far_it_is`: B's download limited to
  256 KiB/s on the LAN (`test_limit_lan`, the same switch as `SEEDWELL_QA_LIMIT_LAN`), a 1.5 MB file of noise; waits
  of 250 ms each return within 250 ms + 1.5 s, `done` never goes back and takes at least two partial values, and the
  file arrives whole.
- `seedwell-sync-net` `ondemand::a_fetch_while_the_core_is_busy_answers_in_time_and_asks_once_it_is_free`: the folder's
  core held for 2 s and then 3 s (`test_lock_core`); each wait returns in time, none gives up (grace 0, stall 0.5 s),
  a cancel meanwhile keeps the request from going in, and the file comes once the core is free.

Both are in `tools/timing-tests.txt`.

## 2. The core's messages reach a log on Windows (and on the Mac)

### What was wrong

The core's messages are `eprintln!`s: folder sync's (`seedwell sync: …`, including "QA: the speed limits apply to LAN
sessions too" and "… ignored …" at start, and every one in `service.rs`, `devices.rs`, `direct.rs`, `chat.rs`, `lan.rs`,
`netchange.rs`, `shared.rs`), and those of the other crates (`seedwell: file provider: …`, `seedwell chat: …`), and a
panic's message. A WinUI app has no console: standard error is no handle, and Rust drops what is written there. The
Mac is no better: an app started from the Finder or at login writes standard error to `/dev/null` (the Mac logs only
its own lines, through `os_log`), so the guess that it sent standard error to a file was not right.

### What changed

`Core::new` (`crates/ffi/src/lib.rs`, first thing in `Core::open`) calls `seedwell_platform::proc::stderr_to_log(<state>\logs\core.log)`.
When standard error goes nowhere (Windows: `GetStdHandle(STD_ERROR_HANDLE)` is null, `INVALID_HANDLE_VALUE` or of an
unknown type; Unix: fd 2 is closed or is `/dev/null`), it opens `core.log` for appending and makes it the process's
standard error (Windows: `SetStdHandle(STD_ERROR_HANDLE, handle)`, the handle kept open for the life of the process;
Rust's standard error asks for the handle at each write, so every later `eprintln!` and panic goes there; Unix:
`dup2` onto fd 2). Once per process. A `core.log` larger than 2 MB at start is kept as `core.log.1` (the one before
goes) and begun anew. Standard error that goes somewhere (a terminal, a test run, a script that redirects it) is left
alone. The first line after the redirect is

```
seedwell: 2026-11-03T14:05:09.123Z Seedwell 0.1.17 started (process 4242)
```

`core.log` is a file of its own beside `app.log`, not `app.log` itself: C#'s `AppLog` opens `app.log` per line with
`FileShare.Read`, which a handle the core kept open for writing would refuse (and two appenders to one file could
overwrite each other's lines).

### What the host does

Nothing: no C# change is needed. To see the core's lines, read `<state>\logs\core.log`. A QA check that asserts on
them needs a helper beside `Log` in `run.ps1`, for example
`function CoreLog($dir) { $f = "$dir\state\logs\core.log"; if (Test-Path $f) { @(Get-Content $f -Encoding utf8) } else { @() } }`.
(C#'s own `Console.Error.WriteLine`, in `App.xaml.cs` and `HangWatch.cs`, may land there too: .NET takes the
handle the first time `Console.Error` is used, so a line written after the core started goes to `core.log`, one
written before goes nowhere as it did.)

### Which checks should pass

- A QA run started with `SEEDWELL_QA_LIMIT_LAN=1` in an isolated state: B's `logs\core.log` has the started line and
  `seedwell sync: QA: the speed limits apply to LAN sessions too (SEEDWELL_QA_LIMIT_LAN)`.
- The user's app (state not in a temporary folder) with the variable set: `seedwell sync: SEEDWELL_QA_LIMIT_LAN ignored: …`.
- A run with `Start-Process … -RedirectStandardError <file>`: the lines go to that file instead and no `core.log` is
  made.

### Rust tests (run by `rust-tests.ps1`)

- `seedwell-platform` `proc::tests::standard_error_that_goes_nowhere_goes_to_the_log`: a copy of the test binary with no
  standard error handle (Windows: `SetStdHandle(…, null)`; Unix: `/dev/null`) redirects once, and both an `eprintln!`
  and a panic's message are in the log; a log over 2 MB is kept as `.1`; with standard error piped, nothing is
  redirected and the lines arrive on the pipe.
- `seedwell-ffi` `sync::ondemand::tests::the_cores_messages_reach_its_log_in_an_app_with_no_console`: a copy of the test
  binary with no standard error starts a real `Core` with `SEEDWELL_QA_LIMIT_LAN=1`; its `logs/core.log` has the
  started line and folder sync's QA line.

## 3. Also in this build (found by the check, fixed at the root)

- **Engine:** removing a torrent with its files right after it finished could leave some files behind: the torrent's
  actor answered the stop at once while its files were still being moved out of `.incomplete`, and the removal
  deleted what was at either place at that moment. A stop during that move is now answered when the move is over.
  `seedwell-engine` `organize::removing_a_torrent_with_its_files_deletes_its_folder_only` failed 3 of 8 stress runs at
  utility priority before, 0 of 10 after; if `rust-tests.ps1` showed it failing now and then on the PC, that was this.
- **Test:** `seedwell-cli` `e2e::download_pause_resume_info_stats_and_two_step_remove` walked the download folder a second
  time while the engine moved a file and could miss it; the redundant walk is gone.
