# Seedwell QA harness

Real-window QA of the app against a real (local, legal) swarm. **Never touches the real home or the real
state**: the app runs with `--state-dir` and `--home-root` under a temp root (`env.sh` refuses anything else),
and the app itself refuses to start when only one of the two is given or when a folder of the isolated
state lies inside the real home.

**Never in your way.** A run is quiet (`--qa-control`, `Support/QuietRun.swift`): the app never becomes the active
or frontmost app, never takes the focus or a key you type, shows no window on any screen (they all sit far off the
screens), has no menu bar item or Dock icon and posts no notification. The shots are drawn in-process (`shot`), so a
run needs neither an awake display nor the Screen Recording permission: start it and keep working.

## Running

    app/qa/run.sh all                 # every capture script, one at a time
    app/qa/run.sh keys chooser        # some of them (capture_ / .sh may be left out)

Each script uses its own QA root and tears it down; `run.sh` prints per script PASS or FAIL, its time and its shots,
then a summary, and exits non-zero when one failed. Shots go to `app/build/qa/`, logs to `app/build/qa/logs/`. Build
the app first: `tools/sysguard/heavy.sh app/qa/rebuild.sh` (or `SEEDWELL_PROFILE=dev tools/sysguard/heavy.sh bash
app/build.sh`); the scripts with a local seeder use `target/release/seedwell` (`cargo build --release -p trs-cli`).

## Writing a script

`source env.sh`, then: `qa_fresh` (a clean root), `qa_write_config` (`QA_WATCH=~/Watch` adds a watch folder, `QA_START_DELAY`
the countdown), `qa_app_start [args]`, and

- `qa_cmd "<command>" ...`: returns once the app has done them **and laid its windows out**: a shot or a report right
  after sees the change. No `sleep` after a command.
- `qa_shot <name> [main|settings|info|chooser|popover|menu|alert|window <id>]`: draws it at 2x into
  `app/build/qa/<name>.png`. A missing shot fails the script.
- `qa_ask "<command>"`: runs a reporting command (`where`, `ui state`, `starting state`, `menu-titles`, `windows`,
  `getinfo-card`, …) and prints its report.
- `qa_wait "<command> ~ <text>" [s]` (or `!~`): the app answers once the command's report holds (no longer holds)
  the text, e.g. `qa_wait "where ~ section downloads" 10`, `qa_wait "windows ~ chooser"`. Prints the report; fails
  after the timeout (default 15 s).
- `qa_until <s> <shell command>`: for what only the outside sees (the CLI, files, ports).
- `qa_setting key=value` (through the CLI) and `qa_set_config key value` (a hand edit of config.toml): both return
  once the engine has read the file again (`config` reports its reload count).
- `check` / `lacks <label> <text> <wanted>` set `FAIL=1`; `qa_end <name>` tears the root down and exits with the
  verdict.

Nothing may go to the Trash: an isolated run's engine still uses the user's real Trash for "trash" (watch folders,
`torrent_after_add`), so QA configs say "leave" and `qa_app_start` refuses one that does not; Cancel's Trash is
`SEEDWELL_QA_TRASH_DIR` (a folder in the QA root).

A `sleep` stays only where time itself is the scenario: a countdown that must run, a seeder's throttled transfer, a
check that nothing happens for a while.

| File | What |
|---|---|
| `run.sh` | the runner: `run.sh [script ...\|all]`, PASS/FAIL, times and shots per script, a summary |
| `env.sh` | paths and helpers (above), `qa_seeder`/`qa_seed` (a rate-limited local seedwell daemon), local trackers, `qa_port_free` (a fixed seeder port still held by another QA scene stops the script), `qa_teardown` |
| `make_torrents.py` | synthetic torrents with data in place (random bytes, release-like names, any tracker URL) |
| `qa_tracker.py` | tiny loopback tracker: `ok`, `unregistered` (Dead group) or `down` |
| `scenario.sh [N]` | the standard scene: N seeds (resting, active, error, dead), 3 downloads from a throttled `seedwell daemon`, a queue, held torrents |
| `popular.sh` | three headless leechers download a big torrent from the app (Popular group); run after the app's last start |
| `moving.sh` | moves a finished torrent to a throwaway disk image under the QA root with the copy throttled to 1 MiB/s (`advanced.move_limit_mib`): the "Moving… 42%" row |
| `nospace.sh` | a torrent bigger than its 24 MB destination volume (disk image under the QA root): the "Not enough space: … still to download, … free · Resume to try again" row; `scenario.sh` runs it |
| `teardown.sh` | quits the app, ends every process whose command line names the QA root (seeders, leechers, trackers) and detaches its disk images |
| `perf.sh [N]` | N seeding torrents, window closed: RSS, footprint, CPU and wake-ups; then snapshot cost and scrolling |
| `capture_all.sh` | every view and tab of the standard scene (built by `scenario.sh` when it does not run), dark and light, into `app/build/qa/` (Popular, Active with the Moving row, Error, Dead, choosers incl. Change ▸ category in place, menus, all settings tabs); `capture_changes.sh` and `capture_play.sh` share the scene |
| `capture_play.sh` | Play while downloading (SPEC 7F.3): Play button row, context menus, Play With (single and pack), menu bar right-click menu, Get Info, the "streaming" note, Settings > General > Play videos with (closed and open), dark and light, into `app/build/qa/play-*.png`; `capture_play.sh launch-test <App.app> <video>` is the one real launch |
| `capture_keys.sh` | the main window's keys in the running app (own QA root `seedwell-qa-keys`): ↓/⇧↓/⇧↑ move and extend the selection, ⌘A selects every row, ⌘I opens Get Info of one row only, Escape clears; a new download while the Add sheet is open only flags Downloads. QA commands: `key delete\|return\|escape\|space\|left\|right\|up\|down\|a\|i [shift] [cmd] [list]` (the core's answer to that key, no key event posted), `add-sheet open\|close`; `where` also says whether the sheet is open |
| `capture_files.sh` | the file lists under the rows (own QA root `seedwell-qa-files`): collapsed list, a season pack expanded with mixed selection and Play on each episode, file and Priority menus, a finished torrent in Sharing, a 2,000-file archive expanded and scrolled, the filter, and a scroll benchmark; `files-*.png`, dark and light. QA commands: `files expand\|folders\|collapse\|filter\|select\|skip\|high\|menu <torrent> …` |
| `capture_starting.sh` | the "Starting in 7 s" notifier (SPEC 8.6; own QA root `seedwell-qa-start`, `start_delay_secs = 12`): popover with one torrent, three torrents and paused, dark and light, the menu bar row, a watch-folder add that raises the popover, a frame sequence of the countdown and the restart that brings it back; `starting-*.png`. QA commands: `starting show\|close\|state\|pause\|resume\|now\|choose [name]` |
| `capture_devlog.sh` | developer mode (SPEC 8.7; own QA root `seedwell-qa-dev`): a private-style local tracker with a passkey in its announce URL, Settings > Advanced with the Developer group, the tracker log viewer with entries (failed exchange in red, one entry expanded to its request and answer), dark and light, into `devlog-*.png`; then `grep`s `logs/trackers.log` for the real passkey and fails if it is there. QA commands: `devlog open\|close\|filter <text>\|errors on\|off\|event <name>\|expand all\|none\|first\|last\|state` |
| `capture_offline.sh` | the quiet "Offline" state (SPEC 7C.4c; own QA root `seedwell-qa-offline`): `net offline` (the path monitor's verdict, no debounce) shows "Offline" in the sidebar's rate line and the menu bar menu, `net online` brings the rates back; dark and light, `offline-*.png` |
| `capture_new_download.sh` | a new download takes the window and the menu (SPEC 8.2, 8.3; own QA root `seedwell-qa-newdl`, local rate-limited seeder): a finished multi-file torrent selected in Sharing, then a torrent added by the socket (as an opened .torrent / magnet), by the watch folder, with the menu open, with the window closed, with Settings open (dot on Downloads), through the popover (countdown expiry, Start Now, Download/OK on a multi-file torrent) and after a restart; asserts section / selection / badge (`where`) and the menu titles (`menu-titles`); `newdl-before\|after\|badge\|menu-new\|menu-live.png` |
| `capture_menu_files.sh` | the menu bar menu's inline file list (SPEC 8.2; own QA root `seedwell-qa-menufiles`, rate-limited local seeder; a season pack and a 2,000-file archive downloading, Big Buck Bunny in Recent): the REAL menu is opened, the row chevron and the list's rows are clicked through their handlers (`menu-files expand <torrent>`, `press <name\|all\|none>`, `fold <folder>`, `state <torrent>`, `context <row>`); asserts expand/collapse, one list at a time, the engine's wanted files after a file, a folder and Select None / All, the menu still tracking, collapsed on reopen; `menufiles-inline-collapsed\|expanded\|after-toggle\|folder-open\|archive\|recent.png` |
| `capture_clicks.sh` | a multi-file row, its disclosure triangle and its file tree "clicked on and off" in every order (own QA root `seedwell-qa-clicks`, rate-limited local seeder): asserts that the row and file selection follow, Escape clears a file selection, the run loop answers within 250 ms; `clicks-*.png`. QA commands: `rowclick <name> [command\|shift]`, `files toggle <torrent>` (the triangle), `files select <torrent> <file> [command] [shift]` (a click on a file row), `ui state`, `ui ping` |
| `capture_chooser.sh` | "deselect everything, then tick one" in the chooser window and the popover's checklist, one shared model (own QA root `seedwell-qa-chooser`): the ticks after every step, every fold changes only the expanded set, the run loop and CPU stay idle; `chooser-*.png`. QA commands: `choice tick <title>` (a box or a row's title), `choice fold <title>` (a group's chevron), `choice all` (the "Select all / none" box), `choice pick <All\|Latest season\|What I don't have>`, `choice state`, `starting fold` (the block's chevron) |
| `capture_catpill.sh` | the category pill of the add popover (own QA root `seedwell-qa-catpill`): TV -> Other and back, and the pill's menu under the pill; `catpill-*.png`. QA commands: `starting category <label>`, `starting pill [close]` (the menu as a native menu, the same entries) |
| `capture_addpop2.sh` | the add popover's Cancel + Start, the title that opens the main window in selection mode, a tick in the window's list that shows in the popover, ⇧ captions, a magnet link (own QA root `seedwell-qa-addpop2`, fake Trash); `addpop2-*.png`, `selmode-*.png` |
| `capture_agents.sh` | Settings > Agents (SPEC 9A.4; own QA root `seedwell-qa-agents`): agent access, the Command line group (Copy CLI Path, Install Command-Line Tool…, Install Skill for Claude Code) and the activity list, dark and light; no button is pressed (nothing is written to the real `~/.claude` or PATH folders); `agents-dark\|light.png` |
| `capture_cancel.sh` | Cancel on a held torrent (SPEC 7A.6; own QA root `seedwell-qa-cancel`): `.torrent` files in the QA home's Downloads and a watch folder, `SEEDWELL_QA_TRASH_DIR` as a fake Trash inside the QA root (the real Trash is never used); Cancel trashes the file, `starting cancel keep` (⇧-Cancel) keeps it, a replaced file is never trashed, a magnet link has no Shift variant; asserts the files, `cancel-normal\|shift\|expanded\|expanded-shift\|watch\|magnet.png`. QA commands: `open-file <path>`, `starting shift on\|off`, `starting cancel [name] keep` |
| `age.py` | rewrites last-activity so most torrents are dormant (the engine needs 24 h otherwise) |
| `rebuild.sh`, `sb.sh` | fast (no LTO) rebuild of the app; Swift-only rebuild with error filtering |

The app is driven through `--qa-control <file>` (see `Support/QAControl.swift`): one command per line
(`section sharing`, `tracker tracker-z`, `settings rules`, `appearance light`, `menu open`, `contextmenu <name>`,
`shot <name> [what]`, `wait <command> ~ <text>`, `windows`, `starting show`, `quit`, ...); `<file>.ack` holds the number
of lines done once their effects are applied and laid out, reports go to `<file>.bench`.

**No synthetic input.** No command posts a key, mouse or scroll event, and no script uses AppleScript or accessibility
input (AGENTS.md, Safety). A click or a key is the QA command that makes the call it makes (`rowclick`, `files select`,
`choice tick`, `key escape`, `menu-press quit`, ...); `ui dump`, `ui state` and `ui ping [win=main|chooser|popover]` only
read the window. That a click lands on the control drawn under it (an unticked box, a chevron beside its glyph) is
checked by `app/Tests/SeedwellAppTests/HitAreaTests.swift` through SwiftUI's accessibility hit test, not in the app.

**Shots** (`shot`, `Support/QAShots.swift`) draw the app's own windows in their active look (accent colours, window
buttons in colour), every 50 ms until two drawings match. Materials draw as with Reduce transparency (the window
server composites them from what is behind a window on screen) and vibrant layers (the sidebar) through their own
colour matrix. Menus and popovers cannot be drawn off screen: a quiet run opens and refreshes the menu bar menu
without showing it and keeps context menus "open" for drawing; `shot … menu` draws their real item views and
stand-ins of what NSMenu draws itself (MenuPanel); the "Starting in …" popover's content lives in a window off screen
and `shot … popover` draws it on a stand-in of its background; `shot … info` is Get Info's popover. The menu bar, the
Dock and other apps are never drawn: `policy` reports the Dock icon the app would show.

**Play QA.** `play-players` writes the detection result (host CPU, each player, its Mach-O slices, runs or CANNOT RUN, what Play would use for MKV and MP4) to `<file>.bench`. `contextmenu <name> playwith` shows the Play With submenu on its own, `settings-popup` the "Play videos with" choices, `play <id>` presses Play on an unfinished row. The QA build never launches a player on its own; only `play-app <App.app> <file>` does.

**Release QA.** `app/release.sh --allow-dirty` (inside `tools/sysguard/heavy.sh`) builds, signs ad-hoc and writes `app/build/release/Seedwell-<version>.dmg`; mount it read-only on a mountpoint under the QA root (`hdiutil attach … -mountpoint <root>/mnt -nobrowse -readonly`), start `<root>/mnt/Seedwell.app` with the usual `--state-dir`/`--home-root`/`--qa-control` flags and capture `settings general` (`settings-version-dark|light.png`: the version footer; `settings-scroll bottom` shows the Updates group). Never install the dmg's app into /Applications.


**Follow-up (not done).** Several scripts here build torrents with `make_torrents.py` / `qa_tracker.py` and a throttled `seedwell daemon` seeder. `seedwell sim add <kind> --speed R --tracker MODE ...` (SPEC 9A.6) does the same from the CLI with no Python (season packs, archives, private two-season packs, unregistered or down trackers, stalls, corrupt pieces, `sim scenario redownload|outage|storm-300|private-unregistered`); the scripts can switch to it later. It needs Developer mode (`seedwell dev on`); the sim torrents alone may use the loopback tracker (the app marks them through the internal `sim_allow` method), and the QA launch flag `--qa-loopback-trackers` that these scripts already pass opens it for every torrent.
