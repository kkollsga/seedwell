# Seedwell

A BitTorrent client for macOS that lives in the menu bar. Native SwiftUI on a Rust engine, built to stay
small in memory, to behave well toward trackers, and to keep your downloads tidy without you thinking about it.

- **Menu bar first.** Progress, speeds and the next thing that needs you, one click away. A main window for
  everything else (dark mode first).
- **Light.** A seeding torrent that nobody is downloading costs almost nothing: no idle connections, no
  timers, a few hundred bytes of state.
- **Tracker-safe.** Announces follow the tracker's intervals, failures back off, quitting says `stopped`.
  Seedwell identifies itself as Seedwell and never pretends to be another client.
- **Tidy.** Content is recognised (movie, TV, music, book, app, game) and filed into your folders when it
  finishes. Moves on the same disk are instant renames, so seeding goes on.
- **Choose files** before or during a download. Progress and time left count only what you picked.
- **Seeding goals.** Ratio, time, or "stop when it has been quiet": less than N GB uploaded over the last
  150 seeding hours.
- **Play while downloading** in IINA, VLC or QuickTime (streamed over localhost only).
- **Command line and agents.** A `seedwell` command with `--json` output, for scripts and local AI agents.
  Local only: nothing listens on the network for control.

Requires **macOS 15 or later** on **Apple silicon**.

## Install

Open Terminal and run:

```sh
curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash
```

The script downloads the latest release, checks its SHA-256 checksum against the one published with the
release, puts **Seedwell.app** in `/Applications` and opens it. It is short; read it first if you like:

```sh
curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | less
```

Why a terminal command? Seedwell is not signed with an Apple Developer ID yet. macOS blocks unsigned apps
downloaded in a browser. Files fetched with `curl` aren't marked as downloaded from the internet, so the app
opens normally. The app itself is still code-signed (ad hoc) as Apple silicon requires.

Options:

```sh
curl -fsSL …/install.sh | bash -s -- --cli        # also link the `seedwell` command into /usr/local/bin
curl -fsSL …/install.sh | bash -s -- --version 0.1.0
```

### Update

Seedwell checks for a new release once a day (Settings › General › Updates) and offers it in the menu bar and in
Settings; it can also install by itself when nothing is downloading. Or run the install command again. A running Seedwell is quit cleanly first (trackers are told `stopped`) and
reopened afterwards. Your torrents and settings are kept.

### Uninstall

```sh
curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash -s -- --uninstall
```

This removes the app and the `seedwell` link. It asks before deleting your data in
`~/Library/Application Support/Seedwell` (torrent list, resume data, settings). Downloaded files are never
touched.

## Command line

Settings › Agents › **Install Command-Line Tool…**, or the installer's `--cli` option, puts `seedwell` on
your PATH.

```sh
seedwell list                    # what's downloading and sharing
seedwell add ~/Downloads/x.torrent
seedwell files ubuntu --only 0-2 # choose files
seedwell info ubuntu --json      # machine-readable (versioned, v1)
seedwell help                    # every command
```

`debug` commands are read-only except `debug announce`, which asks the trackers again (the tracker's
minimum interval still applies).

## Build from source

You need Rust (stable) and the Xcode Command Line Tools.

```sh
bash app/build.sh                # builds app/build/Seedwell.app
bash tools/install-local.sh      # installs that build into /Applications and relaunches
```

Tests:

```sh
tools/check.sh           # clippy + tests for the crates a change affects (cargo nextest), the app when needed
tools/check.sh --full    # everything, as GitHub CI runs it
```

Each crate's integration tests build into one binary (`tests/it/`). Only tests that measure the process
itself (memory use) keep their own binary.

## Layout

| Path | What |
|---|---|
| `crates/engine` | Session, torrents, peers, piece picker, choker, queue, moves |
| `crates/tracker` | HTTP/UDP announces, backoff, outage handling |
| `crates/storage` | Disk I/O, verification, journaled moves |
| `crates/classify` | Content detection and naming |
| `crates/ffi` | Swift bindings (UniFFI) |
| `crates/cli` | The `seedwell` command |
| `app/` | The SwiftUI app |
| `SPEC.md` | The full behaviour specification |
| `docs/` | Engineering notes, releasing |

## Use it lawfully

Seedwell is a general-purpose file-sharing tool. Only download and share content you have the right to.

## License

MIT or Apache-2.0, at your option.
