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
- Releases are built by GitHub Actions when a `vX.Y.Z` tag is pushed (`docs/RELEASING.md`); the result is a draft
  that a person publishes.
- Agents do not commit unless the coordinator asks. Commit messages end with the attribution trailer the session
  specifies.

## Windows and Linux

- Windows and Linux apps with the same features are coming (`docs/CROSS-PLATFORM.md`). **Rust decides, the host
  draws:** user-visible text, formatting, ordering, state machines, validation and polling policy belong in Rust and
  reach the app as ready view models. Swift keeps drawing and OS services only; a rule moved to Rust is deleted in
  Swift. Nothing outside a platform seam names an operating system.

## Bugs

- **No bugs left behind.** Fix every bug as it appears, whether it is related to the current work or not: a test that
  fails only sometimes, an error in a log, wrong behaviour noticed in passing. Find the root cause, fix it with a test,
  and commit it on its own. Never wave a failure off as flaky or unrelated.

## Heavy work (16 GB Mac)

- Run builds, tests and benches through `tools/sysguard/heavy.sh`, one at a time and never nested.
  Machine-specific settings go in the untracked `tools/sysguard/local.env`.
- **Check with `tools/check.sh`.** It runs clippy and the tests (cargo nextest) for the crates a change touches
  plus the crates that depend on them, and the app build and Swift tests only when `app/` or `trs-ffi` is affected.
  It takes the heavy lock itself (do not wrap it in `heavy.sh`).
  - While iterating, narrow it further: `tools/check.sh -p trs-tracker -- -E 'test(/host_recovery/)'`.
  - Run the plain `tools/check.sh` once when the work is done, before handing it back.
  - `tools/check.sh --full` is the whole workspace, which is what GitHub CI runs on every push to `main`. Do not run
    it locally unless the coordinator asks; never run the whole suite more than once per task.
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
- Agents talk to each other (the Mac and Windows agents included) in **agent channels**: channels an agent made
  (`seedwell chat new`, then `chat share` / `chat join` by link), tagged "agents" in the user's app, where the user
  reads every message. Never start a channel or daemon the user can't see.
- **Agents manage the user's app through the `seedwell` CLI**: they may switch Settings › General › Updates ›
  Allow development builds on and off (`seedwell update dev-builds on|off`) and install development builds (SPEC
  12A), the one way a build reaches the user's app: build with `tools/sysguard/heavy.sh tools/dev-build.sh`, then
  `seedwell update install <zip>`, or put it in a folder the app watches (`tools/dev-build.sh --to <folder>` after
  `seedwell update watch <folder>`). After a release is published, switch development builds off so the app goes
  back to the release. Never replace, quit or signal the app any other way. `seedwell debug announce` is not
  read-only.
- **Chats are private unless the user opens them.** Agents see only agent channels and channels whose agent switch
  the user turned on (and private messages only with the Mac's switch on); only the user sets these switches, in the
  app, and nothing makes a user's own channel an agent channel. Never work
  around them: no reading the state folder's chat files or `sync.toml`, no QA control on the user's app (it refuses
  to run there).
- Never contact private trackers from tests or tools. Tests use only legal torrents and loopback trackers.
- Seedwell always identifies as itself (peer id, User-Agent). Never spoof another client.
- **Agents drive the app only through tool calls**: the `seedwell` CLI and the QA control commands
  (`app/qa/`, `QAControl.swift`), which call the same model and core methods the UI calls. No synthetic key presses,
  mouse movements or clicks (no posted NSEvent or CGEvent, no AppleScript or accessibility input), in QA, tests or
  tools. When a check needs a key or a click, add a QA command that calls what that key or click calls; the host's
  mapping from the event to that call is covered by unit tests.
