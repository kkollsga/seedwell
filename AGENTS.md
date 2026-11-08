# Working on Seedwell (agents)

Rules for AI agents working in this repo. `SPEC.md` is the behaviour specification; `docs/RELEASING.md` is the
release procedure.

## Git and publishing

- **Never `git switch --orphan` (or `git checkout --orphan`) in this working folder.** `switch --orphan`
  removes every tracked file from disk, `.gitignore` included, and a following `git add -A` then commits only the
  ignored build output (`target/` alone is hundreds of GB). It happened once: the folder lost its sources, `.git`
  grew to 71 GB and the push started uploading build output.
- **Make the public commit with `git commit-tree`.** It builds a commit from master's tree without touching the
  working folder or the index:
  ```sh
  c=$(git commit-tree 'master^{tree}' -p public -m "Seedwell <version>")   # first time: no -p
  ```
- **Check a commit before you push it.** File count and size must match master, and no build output:
  ```sh
  git ls-tree -r --name-only "$c" | wc -l                                  # same as: git ls-files | wc -l
  git ls-tree -r -l "$c" | awk '{s+=$4} END {print s/1048576 " MB"}'       # tens of MB, not GB
  git ls-tree -r --name-only "$c" | grep -E '(^|/)target/|\.build/|Generated/|local\.env'   # must print nothing
  ```
  A push that takes more than a minute means something is wrong. Stop it and look.
- **Never `git add -A` or `git add .` when the working folder may be missing tracked files.** Check
  `git status` first: it should list only the changes you meant to make.
- **Outward-facing steps need the user's explicit go-ahead each time:** creating repos, pushing, tags, releases,
  publishing a draft. One approval does not cover the next step. The exception is the `/release` skill: invoking it is
  the go-ahead for that whole release (push, tag, publish), also when it is invoked while other work is still open.
- **The rendezvous Worker** (`tools/rendezvous-worker`, SPEC §7G.3) is the user's: they set it up in the app with
  their own Cloudflare API token. Agents never ask for, handle or store that token and never set the rendezvous up
  through the CLI. An agent may deploy the Worker with the README's developer path (`npx wrangler login`,
  `npx wrangler deploy` in that folder) only when the user explicitly allows that deploy: it is an outward-facing
  step like a push. Tests use the loopback fakes (`seedwell-rendezvous`'s `testkit`, `node --test`), never Cloudflare.
- Releases are built by GitHub Actions when a `vX.Y.Z` tag is pushed (`docs/RELEASING.md`); the result is a draft
  that a person publishes.
- Agents do not commit unless the coordinator asks. Commit messages end with the attribution trailer the session
  specifies.

## Windows and Linux

- Windows and Linux apps with the same features are coming (`docs/CROSS-PLATFORM.md`). **Rust decides, the host
  draws:** user-visible text, formatting, ordering, state machines, validation and polling policy belong in Rust and
  reach the app as ready view models. Swift keeps drawing and OS services only; a rule moved to Rust is deleted in
  Swift. Nothing outside a platform seam names an operating system.
- **Edit `app-windows/` files with tools that keep their line endings** (CRLF in the PC's checkout): the editor's own
  edit tool, or PowerShell. Never `sed -i` (or another tool that writes the file anew): it changes every line ending
  and the whole file shows as changed.
- **Commands for a person on Windows come in both forms**: help, skill text, docs and chat messages that give a
  command to type on Windows give the PowerShell form and the bash form (Git Bash, which agents on the PC run), or
  only the bash form when it is for an agent. Example: `app-windows\dev-build.ps1 -To <folder>` in PowerShell is
  `powershell.exe -NoProfile -ExecutionPolicy Bypass -File app-windows/dev-build.ps1 -To <folder>` from bash.
- **The Mac and the PC work together as the `windows-bridge` skill says** (`.claude/skills/windows-bridge/SKILL.md`):
  git bundles from the Mac, patches from the PC, both as attachments in "Seedwell Development"; the Mac owns Rust and
  merges, the PC owns `app-windows/` and never commits.
- **The PC tests the shortest way first, then fully before a hand-back** (the user's rule, 2026-11-04: the goal is faster development overall and as little waiting for CI as possible). While working it runs
  the checks its change touches (`app-windows\qa\run.ps1 <check>...`); before every batch it sends, the full
  `app-windows\qa\run.ps1 all -Build -CoreProfile Debug` on that code, and a failure is fixed before it goes. On every
  bundle from the Mac, and always on the final candidate before the coordinator pushes, it also runs
  `app-windows\qa\rust-tests.ps1` (the Rust tests as CI's Windows job runs them, about 2 minutes): Windows-only Rust
  failures are found there, not on CI (a save-by-replace gap reached CI on 2026-11-05 because this run was skipped). Local
  Rust builds on the PC are debug, `CARGO_BUILD_JOBS=4`, one build per `target` folder; never a local release core (it
  ran the PC out of memory). Tests and QA may overlap other work there (no quiet machine, see Heavy work). Every batch says what ran.

## Simple, owner-decided designs

**One device decides; the others ask** (the user's rule, 2026-11-06: "This simplistic approach should be the rule. This is
how we make it robust"). Shared state between devices (who is a member, a folder's settings such as Read only) has one
writer: the owner's device.
- Other devices send requests. An owner's device applies them automatically when the rules allow (an admin's admission or
  removal of a user, never of an owner or another admin) and signs the result as its own decision. A later change to the
  requester (a demoted admin) changes nothing already decided.
- A requester acts locally at once where that is safe (an admin's device disconnects a user it asked to remove) and keeps
  its request, re-sent until the owner's decision arrives.
- **No multi-writer rules**: no ordering by device clocks, no deciding "who was what when" from signatures, no tenure,
  backdating or serial-reach logic. Read only went through three such designs (clock dating, run identities, admin
  tenure) and every review found new edge cases; the owner-decided version had none of them.
- When a design needs rules about concurrent writers, stop and propose the owner-decided version to the user first.
- A device that is away catches up from the owner's signed decisions; it never decides alone what the owner would.

## Bugs

- **No bugs left behind.** Fix every bug as it appears, whether it is related to the current work or not: a test that
  fails only sometimes, an error in a log, wrong behaviour noticed in passing. Find the root cause, fix it with a test,
  and commit it on its own. Never wave a failure off as flaky or unrelated.

## Catching bugs early

**Shortest tests first, a full local check before code is handed back or pushed, and GitHub CI as the last check,
not where bugs are found** (the user's rule, 2026-11-04: the goal is faster development overall and as little waiting for CI as possible). A local check of 15–20 minutes is faster than a CI round trip of
about an hour; the shortest test that can catch a failure is faster still. A bug the PC or CI finds costs a round
trip; most can be caught in the first minutes on the Mac.

- **A test targets a specific way the code can fail** (the user's rule). It names that failure in its doc comment
  (a deadlock, a delete reaching a peer before the save, a mark drawn in its row's own colour) and fails when it
  happens. Tests are not there to restrict change: a test that only pins today's output (exact pixels, exact
  wording, exact timings, values copied from one machine) blocks harmless changes and machine differences without
  catching a break. Change it to target the failure, or delete it.
- **Every core call a host uses gets a test that acts like the host,** in the same change. It calls it the way
  Finder, Explorer or the app will (repeated calls, the `wait_ms` they pass, the threads they call from) and asserts
  the promise the host relies on: "answers within `wait_ms`", "`done` rises while data comes", "gives up within N s".
  Write the promise in the call's doc comment, and also what each host's system does with it (Cloud Files cancels a
  fetch that reports nothing for 60 s; a Finder-started Mac app's standard error goes to /dev/null; a Windows app
  has no console). When a host reports a bug, first write this test and see it fail on the old code.
- **Compile for Windows on the Mac:** `tools/check.sh --quick` runs `cargo check --target x86_64-pc-windows-gnu`
  over the affected crates, which catches Windows-only compile errors before a bundle goes to the PC.
- **A review that builds nothing:** before a worker's branch goes into a bundle, the coordinator has its diff read by
  a reviewer agent (the `code-review` skill, medium) against the call's promise and the platform differences above.
  It takes a few minutes and can run while something else builds.
- **While working, test what you changed** with `tools/check.sh --quick` (see *Heavy work*); before handing back,
  the full local check.
- **Every CI failure is a bug that escaped.** Fix it, then find why the targeted tests missed it and add the
  cheapest guard that catches that kind locally next time: a host-like test, a line in `tools/timing-tests.txt`, a
  rule in `tools/test-portability.py` or `tools/ffi-names.py`, a step in `tools/check.sh --quick`. Say in the fix's
  commit which guard was added.

## Heavy work (16 GB Mac)

- **Don't wait for a quiet machine** (the user's rule, 2026-11-05: "We should limit waiting"). Load doesn't let an
  error go uncaught, and that is the only thing the tests guard against. Tests, QA and renders run alongside other
  work. A failure under load is still a finding: rerun it to tell a timing false alarm from a bug, and fix a test
  that fails only under load (it targets a race) instead of explaining it away. Only memory, disk and a shared
  `target` folder still put work in line.
- Run builds, tests and benches through `tools/sysguard/heavy.sh`, one at a time and never nested. This is for memory
  on the 16 GB Mac (parallel Rust builds crashed it twice), not for quiet: work that isn't a Rust build or test
  (QA scripts, renders, reviews, git) doesn't wait for it.
  Machine-specific settings go in the untracked `tools/sysguard/local.env`.
- **Check with `tools/check.sh`.** It runs clippy and the tests (cargo nextest) for the crates a change touches
  plus the crates that depend on them, and the app build and Swift tests only when `app/` or `seedwell-ffi` is affected.
  It takes the heavy lock itself (do not wrap it in `heavy.sh`).
  - On macOS it runs the timing tests (`tools/timing-tests.txt`) a second time at utility priority, where timers
    are late as on CI's Mac runner, and it scans the affected crates' tests for Mac assumptions
    (`tools/test-portability.py`). docs/ENGINEERING.md §5 says how a test joins the one and passes the other.
  - While iterating, narrow it further: `tools/check.sh --quick -p seedwell-tracker -- -E 'test(/host_recovery/)'`.
  - **While working: `tools/check.sh --quick`.** It runs the tests of the crates that changed only (not of the
    crates that depend on them), clippy over those and their dependents (which compiles them, no tests), the app and
    Swift tests only when `app/` or `seedwell-ffi` itself changed, and the Windows compile check.
  - **A worker hands back once `tools/check.sh --base master` passes:** every crate the branch changed and every
    crate that depends on them, the app and Swift tests when they are affected, the timing pass at utility priority,
    the portability and FFI-name scans. That is what CI would run for that change: crates it cannot reach cannot
    break. About 15–20 minutes on this Mac, one heavy job at a time. Its result goes in the hand-back.
  - **Before a push to GitHub** (a release's public commit), the coordinator runs `tools/check.sh --base public` on
    master, unless every commit since the last push was already checked that way and nothing was merged by hand.
  - `tools/check.sh --full` (the whole workspace) only when the user asks, or after a toolchain or dependency
    change (which `check.sh` turns into a full run by itself).
- **Clean up temp folders as you go** (the user's rule, 2026-11-04). Whatever a run, a test, a QA check or a worker
  makes outside the repo's tracked files goes when it is done with: QA state folders, render output, a worker's own
  `CARGO_TARGET_DIR` or scratch copies, downloaded CI logs and artifacts. A worker's worktree is removed once its
  branch is merged (its build folder alone is 10–35 GB; 33 of them filled 496 GB on 2026-11-04), and the coordinator
  removes them after each merge. `tools/check.sh` sweeps `target/tmp` (the tests' own temp root) of anything older
  than a day; the PC's `run.ps1` sweeps `seedwell-qa-*` and `seedwell-render-*` older than an hour at its start. Never
  sweep a folder other programs share (the system temp folder, `~/Library`, the Seedwell Sync folder): delete only
  what you made.
- **A worker's logs and scratch files go under a name of its own** (its worktree or branch name, for example
  `<scratchpad>/<branch>/check.log`), never a shared name like `check.log`: workers share the coordinator's scratchpad,
  and one run's log overwrote another's on 2026-11-05.
- **Clear the build cache as you go.** `tools/check.sh` removes, once a day, the build artifacts no build has
  used for 3 days (`cargo sweep --time 3`, plus the `target/*/incremental` folders of that age, which cargo sweep
  leaves; it says so if cargo-sweep is missing). After a toolchain or dependency change, or when `target/` grows
  past about 50 GB, sweep by hand: `tools/sysguard/heavy.sh cargo sweep --time 1 .` and delete the incremental
  folders older than a day. Do not `cargo clean`: `target/` also holds other sessions'
  files (`target/two-mac/`). An oversized cache breaks things, not only the disk: with 860 000 files in
  `target/debug/deps`, a test binary started from there waited 21 s for FSEvents and the file watcher tests failed.
- Each crate's integration tests are one binary (`tests/it/`). A fresh test binary can wait for a macOS security
  scan on its first run; that is not a hang.

## Safety

- Tests and QA never use the real home folder, `~/Downloads`, the user's Trash or the installed app's data
  (`~/Library/Application Support/Seedwell`). Use temporary state folders.
- **One Seedwell: the user's.** There is no test copy: never run a second, long-lived Seedwell (a daemon, a test
  app, a test bundle id) for development, for trying a feature, or for talking to other agents. Automated test runs
  (cargo tests, `tools/check.sh`, QA) are not copies: they use temporary state and quit when done.
- Agents talk to each other (the Mac and Windows agents included) in channels open to agents (**Agents: Yes**):
  one an agent made (`seedwell chat new`, then `chat share` / `chat join` by link, usable at once on the joining
  side) or one the user set to Yes, tagged "agents" in the user's app, where the user reads every message. Never
  start a channel or daemon the user can't see. Seedwell development talk goes in the channel **Seedwell
  Development** only (follow it with `seedwell chat watch "Seedwell Development"` in the background); the old
  OneDrive files (mac*.md, windows.md), `BRIDGE.txt` notes and the old "Bridge" channel are retired. Run every
  `seedwell chat watch` (and `mail watch`) as the harness's background task and read its output when it ends; never
  detach it (`&`, nohup) or throw its output away: what it printed is no longer unread, so an agent that does not
  read it loses those messages. One `watch --unread` per agent name and place: a second is refused while one runs. Files (bundles,
  patches, screenshots) go as attachments in that channel; the shared Seedwell Sync folder is the fallback.
- **Agents manage the user's app through the `seedwell` CLI**: they may switch Settings › General › Updates ›
  Allow development builds on and off (`seedwell update dev-builds on|off`) and install development builds (SPEC
  12A), the one way a build reaches the user's app: build with `tools/sysguard/heavy.sh tools/dev-build.sh`, then
  `seedwell update install <zip>`, or publish it to every device through the shared builds folder: each device
  (its own agent) runs `seedwell update watch "<Seedwell Sync folder>/builds"` once, and
  `tools/sysguard/heavy.sh tools/dev-build.sh --to "<Seedwell Sync folder>/builds"` puts the Mac's build there signed
  with the build key (the Mac's Keychain). A zip built on the PC is never signed or put there (the user's decision):
  the PC tests its own builds with `seedwell update install <zip>` (`app-windows\dev-build.ps1`), and Windows builds
  reach the devices through releases. Devices install only builds signed with the build key (and on a Mac, Developer
  ID signed). After a release is published, switch development builds off on every device so each goes back to the
  release. Never replace, quit or signal the app any other way. `seedwell debug announce` is not read-only.
- **Chats are private unless the user opens them.** Agents see only channels set to Agents: Yes (one setting per
  channel, for every member device; the user picks it in the app, agents never turn a No into a Yes) and private
  messages only with the Mac's switch on, which only the user sets. Never work around them: no reading the state
  folder's chat files or `sync.toml`, no QA control on the user's app (it refuses to run there).
- Never contact private trackers from tests or tools. Tests use only legal torrents and loopback trackers.
- Seedwell always identifies as itself (peer id, User-Agent). Never spoof another client.
- **Agents drive the app only through tool calls**: the `seedwell` CLI and the QA control commands
  (`app/qa/`, `QAControl.swift`), which call the same model and core methods the UI calls. No synthetic key presses,
  mouse movements or clicks (no posted NSEvent or CGEvent, no AppleScript or accessibility input), in QA, tests or
  tools. When a check needs a key or a click, add a QA command that calls what that key or click calls; the host's
  mapping from the event to that call is covered by unit tests.
