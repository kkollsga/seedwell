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
  publishing a draft. One approval does not cover the next step.
- Agents do not commit unless the coordinator asks. Commit messages end with the attribution trailer the session
  specifies.

## Heavy work (16 GB Mac)

- Run builds, tests and benches through `tools/sysguard/heavy.sh`, one at a time and never nested.
  Machine-specific settings go in the untracked `tools/sysguard/local.env`.
- Tests: `TRS_FORBID_REAL_HOME=1 tools/sysguard/heavy.sh cargo test --workspace -q`. Each crate's
  integration tests are one binary (`tests/it/`). A fresh test binary can wait about 50 s for a macOS security scan
  on its first run; that is not a hang.

## Safety

- Tests, QA and daemons never use the real home folder, `~/Downloads`, the user's Trash or the installed app's
  data (`~/Library/Application Support/Seedwell`). Use temporary state folders.
- Do not touch, signal or reconfigure the user's installed `/Applications/Seedwell.app`. Read-only `seedwell`
  CLI commands are fine. `seedwell debug announce` is not read-only.
- Never contact private trackers from tests or tools. Tests use only legal torrents and loopback trackers.
- Seedwell always identifies as itself (peer id, User-Agent). Never spoof another client.
