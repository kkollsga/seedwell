# sysguard

This Mac has 16 GB of memory. Parallel Rust release builds plus a Linux VM once reached about 44 GB of process footprints, and macOS's watchdog then rebooted it (WindowServer stalled on page-ins). These two scripts keep agents and shells from doing that again.

| Script | What it does |
|---|---|
| `sysguard.sh` | Samples available memory (`kern.memorystatus_level`), swap and load every 5 s, logging to `$SYSGUARD_LOG` (default `$TMPDIR/seedwell-sysguard.log`; machine settings go in an untracked `local.env`). It prints a line whenever the level changes: ok, warn (≤ 25 % free, swap > 4 GB or load > 3× cores) or critical (≤ 12 % free or swap > 8 GB). At critical it stops this project's builds, tests, benches and daemons, newest first, until memory recovers. It never touches anything else. |
| `heavy.sh <cmd…>` | Runs one heavy command at a time, across all agents and shells, and only while more than 40 % of memory is available. It sets `CARGO_BUILD_JOBS=4` unless the caller already set it. |

Rules for agents:
- Wrap every `cargo build`/`cargo test`/`cargo clippy` of the workspace, every bench and every VM start in `tools/sysguard/heavy.sh`.
- Never run colima alongside builds.
