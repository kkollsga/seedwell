# trs-cli: the `seedwell` command

One binary, three jobs. `#![forbid(unsafe_code)]`.

| Command | What |
|---|---|
| `seedwell <command>` | The command line for the running app: talks to the control socket (`trs-control`), `--json` on every command, `debug` and `sim` groups for diagnostics and testing |
| `seedwell skill show / install / uninstall` | The Agent Skill (SKILL.md + REFERENCE.md) that ships inside the binary, generated from the command table |
| `seedwell daemon` | The whole stack headless (engine + `trs-state` + control socket), for E2E tests, benchmarks and Linux CI |

There is no MCP server (user decision, 2026-10-16): one tailored CLI plus the embedded skill. SPEC §9A is the contract.

The socket is `~/Library/Application Support/Seedwell/control.sock` (macOS), or `$SEEDWELL_SOCKET`, or `--socket PATH`.
If nothing listens the CLI says "Seedwell is not running" and exits 5; `--launch` starts the installed app first.

## Source layout

| File | Role |
|---|---|
| `help.rs` | **The command table** (usage, summary, examples, JSON shape, permission) plus the texts of `seedwell help`, `<cmd> --help`, `help agents` |
| `cmd.rs` | Parsing words into a `Cmd` (no I/O): every command, every example is tested by parsing it |
| `select.rs` | Torrent selection: id prefix, name substring, `--all/--tracker/--state/--category`; ambiguity lists candidates |
| `commands.rs` | Running a `Cmd`: calls, `"v":1` JSON, exit codes, `--launch` |
| `debug.rs` | `seedwell debug …` and its text renderers |
| `sim/` | `seedwell sim …`: `content.rs` (layouts, SHA-1, deterministic content, torrents), `swarm.rs` (seeders, mock tracker, rate limiter), `host.rs` (the host process), `mod.rs` (commands, scenarios, manifest) |
| `skill.rs` | The skill text, `install` / `uninstall` |
| `render.rs` | Human text of results; engine texts (titles, goals, waiting notes) come from the engine |
| `daemon.rs` | `seedwell daemon` |

## Conventions

* Output: human text by default; `--json` prints one object with `"v":1` first. Errors: stderr, `{"v":1,"error":{"code","message","exit",..}}` with `--json`.
* Exit codes: 0 ok, 1 error, 2 usage, 3 not found/ambiguous, 4 permission denied or agent access off, 5 app not running, 6 timed out (`wait`).
* Selecting torrents: `<torrent>` is an id prefix (6+ hex), part of the name, or `--all`, `--tracker X`, `--state S`, `--category C`.
* Units: `5MB/s`, `500KB/s`, `1.5GB`, `unlimited`; JSON uses bytes.
* Held torrents: `seedwell start <t>` (Start) and `seedwell cancel <t> [--keep-file]` (Cancel: the `.torrent` goes to the Trash).

```
$ seedwell list downloading
$ seedwell add ~/Downloads/some.torrent && seedwell start some
$ seedwell debug health
$ seedwell debug trackers sintel ; seedwell debug peers sintel --watch
$ seedwell dev on ; seedwell sim add season-pack --seasons 2 --episodes 8 --speed 2MB/s
$ seedwell skill install
```

## Daemon

```
seedwell daemon --state-dir D --download-dir X [--listen-port P] [--bind 127.0.0.1]
                [--socket PATH] [--permission off|read_only|control|full|config] [--ask]
                [--no-dht] [--no-trackers] [--portmap] [--max-active N] [--shutdown-secs S]
                [--allow-lan-trackers] [--allow-loopback-trackers] [--dev-mode]
```

* Opens `D` like the app does: `state.redb`, `config.toml`, `rules.toml`, `trackers.toml` with hot reload,
  then the engine, then serves `D/control.sock` (or `--socket`). Prints
  `seedwell daemon ready socket=... port=<tcp> pid=...` on stdout and runs until SIGINT/SIGTERM.
* **The flags win over `config.toml` and are written into it** at start (download and incomplete
  folders, listen port, DHT, port mapping, queue size; watch folders cleared; `.torrent` never trashed).
  A `config.toml` with values that cannot be used, or a broken one, is never rewritten: the flags then apply
  in memory only (at start and at every hot reload, `HostOptions::adjust_config`), each warning is logged
  once on stderr at start, and the file stays as the user left it.
* `--dev-mode` switches developer mode on for the run: every tracker exchange is written, secrets masked, to
  `D/logs/trackers.log`; read it with `seedwell log trackers --socket D/control.sock`.
* **Tracker destinations** are strict by default (public addresses only, as in the app). `--allow-lan-trackers`
  admits private networks; `--allow-loopback-trackers` admits 127.0.0.1 for every torrent (tests). Without it only torrents that `seedwell sim` marks (internal `sim_allow`, needs Developer mode) may announce to a tracker on 127.0.0.1.
* Smart add is off in the daemon so test torrents start at once; `--ask` keeps the app's behaviour.
* Two daemons with different state dirs on loopback make a swarm: add a torrent on the seeder with
  `--save-path <dir with the data>`, then on the leecher `seedwell --socket L add file.torrent --peer 127.0.0.1:<seeder port>`.

## Dependencies

`trs-control` (protocol and client), `tokio` (current-thread runtime; with the `daemon` feature, `signal` and the
engine's multi-thread runtime), `serde`/`serde_json`, `trs-devlog`. `trs-engine` and `trs-state` only with the
`daemon` feature (default); `--no-default-features` gives the client-only binary that the app bundle ships (it
includes `sim` and `skill`). No argument-parsing crate: the grammar is small.

## Tests

`cargo test -p trs-cli`

* unit tests: argument scanner, selection, parsing of every command and every example in the help and the skill,
  the skill's frontmatter and install/uninstall (temp dir only), the sim generators (SHA-1 vectors, layouts, torrent bytes).
* `tests/e2e.rs`: real `seedwell daemon` processes on loopback driven by the CLI (download, pause, limits, removal,
  permission levels, settings, direct download, smart add, grace period).
* `tests/cli.rs`: exit codes, JSON shapes (`"v":1` and key sets), ambiguous selectors, held torrents (`start`/`cancel`),
  `dev`, `debug` (including that a passkey never appears in any output), `skill`, and `sim` end to end.
