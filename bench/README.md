# TorrentRS comparison benchmarks

Compares TorrentRS with Transmission and qBittorrent on the claims that matter (SPEC §2, ENGINE §9):
fast downloads, low memory with hundreds of seeding torrents, near-zero idle CPU and wake-ups.

Python 3 standard library only, run with `python3 -I`. Everything lives under `bench/`:

```
bench/
  scripts/bench.py            CLI (all commands below)
  scripts/benchlib/           adapters.py (client adapters), runners.py (the 3 benchmarks), torrents.py (synthetic
                              torrents + mock tracker wrapper), report.py, util.py (process supervision, sampling)
  torrents/                   official distro .torrent files (committed)
  reports/<campaign>-baseline.md        generated report (+ optional hand-written notes-<campaign>.md, appended)
  runs/        (gitignored)   results.jsonl (one record per run), csv/ (raw 1 s samples), scale/ (synthetic data),
                              profiles/ (isolated client profiles), trackerlogs/, footprint/
  downloads/   (gitignored)   per-run download dirs, deleted after each run
  capture/                    previous work: client builds / tracker captures (read-only for this harness)
```

## Prerequisites (already installed on the dev machine)

* Transmission 4.1.3: `brew install transmission-cli` (`transmission-daemon` on `PATH`, or `TR_DAEMON=...`).
* qBittorrent 5.2.3: a `qbittorrent-nox` binary on `PATH`, or `QBT=/path/to/qbittorrent-nox`.
* `target/release/tracker-probe` (mock tracker; built on demand with `cargo build --release -p tracker-probe`).
* TorrentRS: set `TRS_BENCH_BIN=/path/to/binary` (contract below). Without it the `torrentrs` client is skipped.

Isolation: every client runs with its own profile dir under `bench/runs/profiles/`. `~/Library/Preferences/org.m0k.transmission.plist`,
Transmission.app state and any other existing client state are never read or written. No private trackers are used.

## One command per benchmark

```sh
cd "$(git rev-parse --show-toplevel)"
python3 -I bench/scripts/bench.py fetch-torrents                  # once: official .torrent files (skips existing)
python3 -I bench/scripts/bench.py netcheck                        # ~200 MB speed sanity check

# 1. real-world download, interleaved A,B,A,B, 2 min seeding after completion, 15 GB budget guard
python3 -I bench/scripts/bench.py download --clients transmission,qbittorrent --plan debian:2,arch:1,ubuntu:1

# 2. scale / idle memory test (loopback, no Internet), 10 min idle window per run
python3 -I bench/scripts/bench.py scale --clients transmission,qbittorrent --sizes 50,200,500

# 3. hot variant: 3 local leechers fetch 10 torrents from a seeder holding N torrents
python3 -I bench/scripts/bench.py hot --clients transmission,qbittorrent --n 200

# report (writes bench/reports/<campaign>-baseline.md); --campaign defaults to today's date
python3 -I bench/scripts/bench.py report
python3 -I bench/scripts/bench.py cleanup                         # kill leftover bench client processes
```

The whole baseline (netcheck, downloads, scale, hot, report; about 3 hours) is `bench/scripts/run-baseline.sh [campaign]`; it adds
`torrentrs` automatically when `TRS_BENCH_BIN` is set. `bench/scripts/tests/` holds `fake-trs-bench` (a contract-conforming stand-in
binary for testing the adapter) and `repro-seeder-reconnect.py` (TorrentRS seeder refusing a second leecher process from the same IP).
A hand-written `bench/reports/notes-<campaign>.md` is appended to the generated report.

Useful flags: `--campaign ID` (group runs; default `YYYY-MM-DD`), `--resume` (skip runs already finished in the
campaign; makes a re-run idempotent), `download --budget-gb 15 --seed-secs 120 --max-secs 3600`,
`scale --idle-secs 600 --reps N`, `hot --rounds 10 --leecher transmission`.
`--clients transmission,qbittorrent,torrentrs` once `TRS_BENCH_BIN` is available. Only run **one** benchmark at a
time; the harness kills every client it started on exit, on error and on Ctrl-C/SIGTERM (process groups), deletes
download dirs in `finally`, and `cleanup` kills strays by profile path.

### Methodology in one paragraph

*Download:* fresh profile per run, no rate limits, DHT/PEX/LSD/uTP/port mapping at client defaults (on), the same fixed listen port
(51413) for every client (runs are sequential), peer limits equalised to 200 global / 100 per torrent, queueing off, tracker
URLs untouched. t0 = right after the torrent is added; every second the harness polls the client API and samples the process tree
(`ps -o rss`, `phys_footprint`, CPU user+sys and idle/interrupt wake-ups from `proc_pid_rusage`, which needs no sudo).
Peak rate is the best 10 s average of the byte counter, so it does not depend on each client's own rate smoothing.
*Scale:* N synthetic torrents (deterministic random data, 1–20 MB, one third multi-file) are added to one process with the data already
in place; the client verifies them; t0 = all seeding; the idle window then uses external sampling only (no API polling, which would
perturb the measurement); a mock tracker (interval 1800 s) counts announces. *Hot:* the seeder is measured while 3 fresh
Transmission leecher processes (default) download the 10 largest torrents from it, repeated for `--rounds` rounds; throughput is taken
from the leechers' byte counters.

## TorrentRS adapter contract

The harness drives TorrentRS through a small foreground binary (a headless mode of the engine, e.g. `trs bench` or a
`crates/engine` example). The coordinator provides it; `TRS_BENCH_BIN` must point to it.

```
$TRS_BENCH_BIN add <file.torrent> [--add <file.torrent>]... \
      --download-dir <dir> --state-dir <dir> --listen-port <port> \
      [--no-dht] [--no-pex] [--no-lsd] [--no-portmap] [--no-utp]
```

* Runs in the **foreground**, adds the given torrents (the first positional one plus every `--add`, up to 500) with the same
  `--download-dir` (data for already-complete torrents is found there and must be verified or resumed, then seeded), and keeps running
  (seeding) until it receives **SIGTERM**.
* `--state-dir` is a fresh, empty directory for all persistent state (resume data, DHT, settings). No other path may be touched.
* `--listen-port` is the TCP (and uTP/UDP) listen port. No rate limits, no queueing: every added torrent is active.
* `--no-utp` (optional) turns uTP off. `--no-dht --no-pex --no-lsd --no-portmap` disable those peer sources and UPnP/NAT-PMP; the harness passes all four for the local
  (scale/hot) benchmarks so the mock tracker is the only peer source, and none of them for real-swarm downloads.
* **stdout:** exactly one JSON object per line, once per second, nothing else on stdout (logs go to stderr), aggregated over **all**
  torrents of the process:

  ```json
  {"done_bytes":123456,"total_bytes":792723456,"down_rate":5242880,"up_rate":0,"peers":42,"state":"downloading"}
  ```

  | key | meaning |
  |---|---|
  | `done_bytes` / `total_bytes` | verified bytes present / total bytes wanted, summed over all torrents |
  | `down_rate` / `up_rate` | current payload throughput in bytes per second (any smoothing window ≤ 5 s) |
  | `peers` | currently connected peers, summed over torrents |
  | `state` | `"checking"` while any torrent is verifying, `"downloading"` while any is incomplete, `"seeding"` when all torrents are complete and verified, `"error"` otherwise |

  Optional extra keys the harness uses when present: `"torrents"` (count), `"seeding"` (number of complete torrents), `"checking"`
  (number being verified). Without them, `state` alone is used.
* **Exit:** on SIGTERM it shuts down gracefully (persist resume data, send `stopped` announces, flush) and exits with code **0** within 30 s.
  Anything else (non-zero, hang, killed by the harness after 30 s) is recorded as a failure in the report.
* `--version` (optional) should print one line, used in the report.
* Memory is measured externally (RSS and `phys_footprint` of the process tree), so the binary should be the real release engine with
  production defaults (same cache and peer limits it would ship with, except where the flags above say otherwise).

## Adding another client

Subclass `Adapter` in `bench/scripts/benchlib/adapters.py` (`available`, `version`, `launch`, `poll`, `stop`), add it to `ADAPTERS`.
`launch(cfg, [(torrent_path, download_dir)])` starts the process with an isolated `cfg.profile_dir` and adds the torrents;
`poll()` returns the aggregated stats dict (`n`, `n_seeding`, `n_checking`, `done_bytes`, `total_bytes`, `down_rate`, `up_rate`, `peers`,
`state`, `complete`).

## Measuring notes

* RSS (`ps`) counts file-backed pages; libtorrent's mmap disk I/O keeps verified data resident after a recheck, so `phys_footprint`
  (what Activity Monitor and SPEC §2 use) is the headline memory number and RSS is reported next to it.
* `powermetrics` needs sudo; wake-up counts use the same kernel counters via `proc_pid_rusage` (`ri_pkg_idle_wkups`, `ri_interrupt_wkups`).
  `top -stats pid,cpu,idlew,power` is run for 20 s at the end of each idle window as a cross-check.
* Sampling the clients' APIs costs a little CPU, identically for every client (1 Hz during downloads, 0.5 Hz while loading, none while idle).
* Other agents compile on the same machine: the load average is stored per run (CSV column `load1`, `loadavg_start` in `results.jsonl`)
  and printed in the report; re-run outliers with `--resume` after deleting their records, or use a new `--campaign`.

## TorrentRS adapter binary

`TRS_BENCH_BIN` for the contract above is the engine example `bench_client` (`crates/engine/examples/bench_client.rs`):

```sh
cd "$(git rev-parse --show-toplevel)"
CARGO_BUILD_JOBS=4 cargo build --release -p trs-engine --example bench_client
export TRS_BENCH_BIN="$PWD/target/release/examples/bench_client"
```

Usage (exactly the contract; the first torrents may be positional after `add`, more via `--add`):

```sh
$TRS_BENCH_BIN add a.torrent b.torrent [--add c.torrent]... \
    --download-dir <dir> --state-dir <dir> --listen-port <port> \
    [--no-dht] [--no-pex] [--no-lsd] [--no-portmap] [--no-utp]
$TRS_BENCH_BIN --version
```

* One JSON line per second on stdout, aggregated over all torrents:
  `{"done_bytes","total_bytes","down_rate","up_rate","peers","state"}` plus the optional extras `"torrents"`, `"seeding"`,
  `"checking"`. `state` is `checking` / `downloading` / `seeding` / `error` as specified above. Logs go to stderr.
* The binary runs the real stack (`trs_engine::Host`) on `--state-dir` (a fresh `state.redb`, `torrents/`, default
  `config.toml` / `rules.toml` / `trackers.toml` are created there; nothing else is touched). Engine settings: no rate limits,
  no queueing (`max_active_downloads` 100000), 200 connections globally and 100 per torrent (the harness's equalised limits),
  DHT / PEX / port mapping on unless disabled, no staging, no organizing, no watch folders (data is written straight into
  `--download-dir`; complete data found there is verified once and then seeded). Loopback trackers are allowed (the mock
  tracker of the local benchmarks); the production engine refuses them.
* SIGTERM (or SIGINT) performs `Engine::shutdown` with a 20 s deadline (`stopped` announces for every torrent, resume data and
  statistics flushed, port mappings deleted) and exits 0; it normally returns in well under a second on loopback.
* The seeder holds every torrent forever: the binary writes a `rules.toml` with the goal `forever` into a fresh state directory
  (the shipped default stops a torrent at ratio 1.0, and a stopped torrent refuses new leechers).
* Measurement aids, off by default: `TRS_BENCH_DIAG=1` counts live heap bytes (a counting global allocator wrapped around `trs_storage::MappedAlloc`, the
  allocator the apps install) and prints, every 10 s on stderr, the heap, connections, handshakes, an allocation histogram by size class,
  the peak heap and the cumulative KiB allocated per size class (the churn of start-up); `TRS_BENCH_PROF=2048-8192` (with `TRS_BENCH_DIAG=1`)
  prints a source-line stack for every 8th allocation whose size is in that range, which is how the 4 KiB redb page buffers were found to be
  the start-up churn (slow: every sample symbolizes a backtrace, use it with 100 torrents); `TRS_BENCH_TIER_SECS=15` shortens the
  Hot to Warm and Warm to Cold timers, so that an idle seeder reaches its steady Cold state in half a minute. The status line uses
  `EngineHandle::totals()`, not `snapshot()`, so it costs nothing measurable with 500 idle torrents.
* `--no-utp` turns uTP off (`BENCH_TRS_NO_UTP=1` makes the harness pass it, for a TCP-only comparison). The shipped defaults, uTP and
  encryption on, are what the harness runs: Transmission prefers encrypted connections and tries uTP first, which only works because the
  engine answers the encrypted handshake (MSE) on TCP and uTP.
* `--no-lsd` is accepted and ignored (the engine has no LSD yet). `--no-portmap` disables UPnP / NAT-PMP / PCP; without it the
  listen port (TCP and UDP) is mapped like in the shipped app, so pass it for anything that must not talk to the router. The DHT
  node uses the same UDP port number.
* Hot rounds are timed from the leechers' byte counters polled once a second, so a 2.5 s transfer reads as 3 or 4 s (609 MB shows as 203, 152 or 122 MB/s). Compare whole rounds and the 1 s peak, and for a true timeline sample the loopback byte counter (`netstat -I lo0 -b`, column Obytes, every 100 ms) next to the run. The Transmission leechers re-fill their request queue about every 500 ms up to the seeder's advertised `reqq`, so a seeder's `reqq` bounds the hot result (see ENGINE.md section 4).

## uTP stall hunting

`bench/scripts/utp-stall-loop.sh OUTDIR [max_runs] [rounds] [stall_s]` loops the hot bench with `TRS_UTP_TRACE=1`
(see `crates/utp/README.md`) and `BENCH_KEEP_PROFILE=1` (keeps the seeder's `stderr.log` instead of deleting the
profile), next to `bench/scripts/lo0-timeline.py OUT.tsv [SECONDS]`, a 100 ms `netstat -I lo0 -b` sampler (wall ms,
Obytes). Per run it keeps `bench.log`, `lo0.tsv` and `stderr.log` until a round takes at least `stall_s` seconds.
Wrap it in `tools/sysguard/heavy.sh`. A round that is slow on the whole (30-50 MB/s, SRTT 100-270 ms in the trace)
is the machine being loaded; a round that is fast and then idle for 8-10 s is a choked leecher waiting for a rechoke.
