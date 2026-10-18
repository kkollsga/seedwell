# Seedwell QA harness

Real-window QA of the app against a real (local, legal) swarm. **Never touches the real home or the real
state**: the app runs with `--state-dir` and `--home-root` under a temp root (`env.sh` refuses anything else),
and the app itself refuses to start when only one of the two is given or when a folder of the isolated
state lies inside the real home.

| File | What |
|---|---|
| `env.sh` | paths and helpers: `qa_app_start`, `qa_cmd`, `qa_shot`, `qa_add_all`, local trackers |
| `make_torrents.py` | synthetic torrents with data in place (random bytes, release-like names, any tracker URL) |
| `qa_tracker.py` | tiny loopback tracker: `ok`, `unregistered` (Dead group) or `down` |
| `scenario.sh [N]` | the standard scene: N seeds (resting, active, error, dead), 3 downloads from a throttled `seedwell daemon`, a queue, held torrents |
| `popular.sh` | three headless leechers download a big torrent from the app (Popular group); run after the app's last start |
| `moving.sh` | moves a finished torrent to a throwaway disk image under the QA root with the copy throttled to 1 MiB/s (`advanced.move_limit_mib`): the "Moving… 42%" row |
| `nospace.sh` | a torrent bigger than its 24 MB destination volume (disk image under the QA root): the "Not enough space: … still to download, … free · Resume to try again" row; `scenario.sh` runs it |
| `teardown.sh` | quits the app, kills the seeders/trackers/caffeinate (by PID from `pgrep`, no `pkill`) and detaches the disk images |
| `perf.sh [N]` | N seeding torrents, window closed: RSS, footprint, CPU and wake-ups; then snapshot cost and scrolling |
| `capture_all.sh` | every view and tab, dark and light, into `app/build/qa/` (Popular, Active with the Moving row, Error, Dead, choosers incl. Change ▸ category in place, menus, all settings tabs) |
| `capture_play.sh` | Play while downloading (SPEC 7F.3): Play button row, context menus, Play With (single and pack), menu bar right-click menu, Get Info, the "streaming" note, Settings > General > Play videos with (closed and open), dark and light, into `app/build/qa/play-*.png`; `capture_play.sh launch-test <App.app> <video>` is the one real launch |
| `followups.sh` | own QA root (`seedwell-qa-fu`): Transmission's port offered on the first launch from a FIXTURE plist (`--qa-transmission-prefs`; Settings > Network). Shots `followup-*.png`, dark and light; tear down with `QA_ROOT=… teardown.sh` |
| `capture_files.sh` | the file lists under the rows (own QA root `seedwell-qa-files`): collapsed list, a season pack expanded with mixed selection and Play on each episode, file and Priority menus, a finished torrent in Sharing, a 2,000-file archive expanded and scrolled, the filter, and a scroll benchmark; `files-*.png`, dark and light. QA commands: `files expand\|folders\|collapse\|filter\|select\|skip\|high\|menu <torrent> …` |
| `capture_starting.sh` | the "Starting in 7 s" notifier (SPEC 8.6; own QA root `seedwell-qa-start`, `start_delay_secs = 12`): popover with one torrent, three torrents and paused, dark and light, the menu bar row, a watch-folder add that raises the popover, a frame sequence of the countdown and the restart that brings it back; `starting-*.png`. QA commands: `starting show\|close\|state\|pause\|resume\|now\|choose [name]` |
| `capture_devlog.sh` | developer mode (SPEC 8.7; own QA root `seedwell-qa-dev`): a private-style local tracker with a passkey in its announce URL, Settings > Advanced with the Developer group, the tracker log viewer with entries (failed exchange in red, one entry expanded to its request and answer), dark and light, into `devlog-*.png`; then `grep`s `logs/trackers.log` for the real passkey and fails if it is there. QA commands: `devlog open\|close\|filter <text>\|errors on\|off\|event <name>\|expand all\|none\|first\|last\|state` |
| `capture_offline.sh` | the quiet "Offline" state (SPEC 7C.4c; own QA root `seedwell-qa-offline`): `net offline` (the path monitor's verdict, no debounce) shows "Offline" in the sidebar's rate line and the menu bar menu, `net online` brings the rates back; dark and light, `offline-*.png` |
| `capture_new_download.sh` | a new download takes the window and the menu (SPEC 8.2, 8.3; own QA root `seedwell-qa-newdl`, local rate-limited seeder): a finished multi-file torrent selected in Sharing, then a torrent added by the socket (as an opened .torrent / magnet), by the watch folder, with the menu open, with the window closed, with Settings open (dot on Downloads), through the popover (countdown expiry, Start Now, Download/OK on a multi-file torrent) and after a restart; asserts section / selection / badge (`where`) and the menu titles (`menu-titles`); `newdl-before\|after\|badge\|menu-new\|menu-live.png` |
| `capture_menu_files.sh` | the menu bar menu's inline file list (SPEC 8.2; own QA root `seedwell-qa-menufiles`, rate-limited local seeder; a season pack and a 2,000-file archive downloading, Big Buck Bunny in Recent): the REAL menu is opened, the row chevron and the list's rows are clicked through their handlers (`menu-files expand <torrent>`, `press <name\|all\|none>`, `fold <folder>`, `state <torrent>`, `context <row>`); asserts expand/collapse, one list at a time, the engine's wanted files after a file, a folder and Select None / All, the menu still tracking, collapsed on reopen; `menufiles-inline-collapsed\|expanded\|after-toggle\|folder-open\|archive\|recent.png` |
| `capture_agents.sh` | Settings > Agents (SPEC 9A.4; own QA root `seedwell-qa-agents`): agent access, the Command line group (Copy CLI Path, Install Command-Line Tool…, Install Skill for Claude Code) and the activity list, dark and light; no button is pressed (nothing is written to the real `~/.claude` or PATH folders); `agents-dark\|light.png` |
| `capture_cancel.sh` | Cancel on a held torrent (SPEC 7A.6; own QA root `seedwell-qa-cancel`): `.torrent` files in the QA home's Downloads and a watch folder, `SEEDWELL_QA_TRASH_DIR` as a fake Trash inside the QA root (the real Trash is never used); Cancel trashes the file, `starting cancel keep` (⇧-Cancel) keeps it, a replaced file is never trashed, a magnet link has no Shift variant; asserts the files, `cancel-normal\|shift\|expanded\|expanded-shift\|watch\|magnet.png`. QA commands: `open-file <path>`, `starting shift on\|off`, `starting cancel [name] keep` |
| `age.py` | rewrites last-activity so most torrents are dormant (the engine needs 24 h otherwise) |
| `winid.swift` | window ids of the app for `screencapture -x -o -l <id>` |
| `rebuild.sh`, `sb.sh` | fast (no LTO) rebuild of the app; Swift-only rebuild with error filtering |

The app is driven through `--qa-control <file>` (see `Support/QAControl.swift`): one command per line
(`section sharing`, `tracker tracker-z`, `settings rules`, `appearance light`, `menu open`,
`contextmenu <name>`, `bench-snapshot`, `bench-scroll`, `starting show`, `quit`, ...), acknowledged in `<file>.ack`.

**Active windows.** macOS refuses to activate any app while the display sleeps or is locked, and the windows then
draw in the grey inactive state (and `screencapture -l` fails). `env.sh` keeps the display awake for the run
(`qa_wake`: `caffeinate -d -u`), the app (QA mode only) becomes a regular app and `activate [main|settings|chooser]`
brings the app to the front and makes that window key before every capture. `scroll top|bottom|<0..1>` shows the lower
Sharing groups, `chooser-category <name>` drives Change ▸ category.

**Play QA.** `play-players` writes the detection result (host CPU, each player, its Mach-O slices, runs or CANNOT RUN, what Play would use for MKV and MP4) to `<file>.bench`. `contextmenu <name> playwith` shows the Play With submenu on its own, `settings-popup` the "Play videos with" choices, `play <id>` presses Play on an unfinished row. The QA build never launches a player on its own; only `play-app <App.app> <file>` does.

**Release QA.** `app/release.sh --allow-dirty` (inside `tools/sysguard/heavy.sh`) builds, signs ad-hoc and writes `app/build/release/Seedwell-<version>.dmg`; mount it read-only on a mountpoint under the QA root (`hdiutil attach … -mountpoint <root>/mnt -nobrowse -readonly`), start `<root>/mnt/Seedwell.app` with the usual `--state-dir`/`--home-root`/`--qa-control` flags and capture `settings general` (`settings-version-dark|light.png`: the version footer; `settings-scroll bottom` shows the Updates group). Never install the dmg's app into /Applications.


**Follow-up (not done).** Several scripts here build torrents with `make_torrents.py` / `qa_tracker.py` and a throttled `seedwell daemon` seeder. `seedwell sim add <kind> --speed R --tracker MODE ...` (SPEC 9A.6) does the same from the CLI with no Python (season packs, archives, private two-season packs, unregistered or down trackers, stalls, corrupt pieces, `sim scenario redownload|outage|storm-300|private-unregistered`); the scripts can switch to it later. It needs Developer mode (`seedwell dev on`); the sim torrents alone may use the loopback tracker (the app marks them through the internal `sim_allow` method), and the QA launch flag `--qa-loopback-trackers` that these scripts already pass opens it for every torrent.
