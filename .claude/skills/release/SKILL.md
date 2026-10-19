---
name: release
description: Cut a Seedwell release end to end - tidy the repo (open pull requests, stale branches, leftovers), publish master's tree to public main, wait for CI, tag, let the release workflow build the draft, check it, and publish it. Invoked while work is in progress, it pre-authorizes the release for when that work is done. Use when the user asks to release, ship, or make a new version.
---

# /release

Read `AGENTS.md` and `docs/RELEASING.md` first. Their rules win over anything here. The ones that matter most:

- Public `main` is a separate history. Each public commit is `git commit-tree 'master^{tree}' -p public`. Never
  `git switch --orphan` or `git checkout --orphan`. In zsh, write refspecs as `"${c}:refs/heads/main"`.
- Builds and tests go through `tools/sysguard/heavy.sh`, one at a time.
- Never touch the installed `/Applications/Seedwell.app` or its data. Do not run `tools/install-local.sh` unless the
  user asks.

Invoking `/release` is the user's go-ahead for the whole release, publishing included: merging Dependabot pull
requests, pushing `main`, pushing the tag and publishing the draft. Anything the steps below say to ask about
(a PR or branch we did not make, removals), ask about. Stop and report if a step fails, rather than working around
it. If the release is withdrawn or the user says to hold, stop where you are.

**Called mid-flight.** If `/release` arrives while work the user asked for is still open (a worker running, edits
not yet built, tested or committed, follow-ups the user just raised), it is a pre-authorization, not an interrupt:
finish that work first, to the usual standard (built, tested, reviewed, committed), then run the steps below without
asking again. Requests the user adds before the release is published join it. Tell the user in one line that the
release will follow once the work is done.

## 1. Version

- Releases are **patch** bumps (`0.1.1` → `0.1.2`) unless the user named another version.
- Read `version` under `[workspace.package]` in `Cargo.toml`, and the latest tag: `gh release list --limit 3`.
- If the tag for the current version exists as an **unpublished draft** and the user wants more changes in that same
  release, re-cut it in step 5 (delete the draft and tag, then tag again). A **published** version is never reused:
  bump it.
- Peer ids encode each version part as one character, `0`-`9` then `A`-`Z`, so a part above 35 fails the build.

## 2. Tidy pull requests

```sh
gh pr list --state open --json number,title,author,headRefName,statusCheckRollup,mergeable
```

- **Dependabot pull requests:** merge the ones whose checks pass (`gh pr merge N --squash`).
  - For one that fails, fix the code on its branch: use a scratch worktree, push a commit, wait for CI, then merge.
  - Never close a Dependabot PR or ignore a major version without the user.
  - Check major bumps for on-disk format changes. The redb 2 → 4 bump needed a converter
    (`crates/state/src/legacy.rs`).
- **Anything else, for example a PR we did not open or work on in this session:** list it (number, title, author,
  state) and **ask the user** before merging, closing or touching it.
- **After merging on GitHub:** bring the changes into master. The diff from `public` to `origin/main` applies cleanly
  when master's tree matched `public`.
  ```sh
  git fetch origin
  git diff public origin/main | git apply --index
  git commit -m "..."
  git fetch origin main:public
  ```
  Then check `master^{tree}` and `public^{tree}` match, unless master has newer commits of its own.

## 3. Tidy branches and leftovers

- **Remote branches:**
  ```sh
  git ls-remote --heads origin
  ```
  - Delete branches of merged or closed Dependabot PRs: `git push origin --delete <branch>`.
  - Any other branch except `main`: ask the user.
- **Tags:** list them. Never delete a tag of a published release.
- **Local:**
  - `git worktree list` and `git branch`: remove worktrees and branches this session created (scratch worktrees).
    Leave alone any you did not create, for example worktrees under the shared cache folder used by the soak
    daemon and the Linux checks. Ask if unsure.
  - `git status --short --ignored`: report untracked leftovers that are not build output (stray scripts, captures,
    `*.orig`, temp files). Delete only what this session made; ask about the rest.
- **Trash code:** look for leftovers the release should not carry, and propose removals rather than making them
  silently:
  - commented-out blocks, `dbg!` / `println!` debugging, `TODO remove`;
  - files not referenced by any build (old scripts, unused QA captures);
  - dependencies no crate uses (`cargo machete` if installed, otherwise a grep for each dependency's crate name).

  Removing code is a change like any other: tests and clippy must pass, and it goes in its own commit.

## 4. Bump, check, publish the tree

1. **Bump** `Cargo.toml` (`Cargo.lock` follows on the next cargo command) and commit on master: `Seedwell X.Y.Z`,
   with the session's attribution trailer.
2. **Clean tree:** `git status --short` must be empty.
3. **Check locally** what changed since the last release (GitHub CI runs the full suite, so don't duplicate it):
   ```sh
   tools/check.sh --base public
   ```
4. **Build the public commit.** Its message becomes the release notes, so write what changed for users: a title
   plus a short bullet list.
   ```sh
   c=$(git commit-tree 'master^{tree}' -p public -m "Seedwell X.Y.Z: <summary>" -m "- change one
   - change two")
   ```
5. **Verify it before pushing:**
   ```sh
   git ls-tree -r --name-only "$c" | wc -l        # equals: git ls-files | wc -l
   git ls-tree -r -l "$c" | awk '{s+=$4} END {print s/1048576 " MB"}'      # tens of MB, never GB
   git ls-tree -r --name-only "$c" | grep -E '(^|/)target/|\.build/|Generated/|local\.env'   # prints nothing
   ```
6. **Push and move the ref:**
   ```sh
   git push origin "${c}:refs/heads/main"
   git fetch origin main:public
   ```
   A push that takes more than a minute is wrong: stop it and look.

## 5. CI, then tag

- Wait for CI on the pushed commit:
  ```sh
  gh run list --branch main --limit 1
  gh run watch <id> --exit-status
  ```
  If it fails, read the log (`gh run view <id> --log-failed`), fix on master, and go back to step 4.
- **Re-cutting an unpublished draft of the same version** (step 1):
  ```sh
  gh release delete vX.Y.Z --yes
  git push origin --delete vX.Y.Z
  git tag -d vX.Y.Z
  ```
- **Tag the public commit and push it:**
  ```sh
  git tag -a vX.Y.Z "$c" -m "Seedwell X.Y.Z"
  git push origin vX.Y.Z
  ```
  This starts `.github/workflows/release.yml`.

## 6. The draft

- **Wait for the release workflow:**
  ```sh
  gh run list --workflow release.yml --limit 1
  gh run watch <id> --exit-status
  ```
- **Check the draft:**
  ```sh
  gh release view vX.Y.Z --json isDraft,assets,body
  ```
  - It is a draft.
  - Its assets are `Seedwell-X.Y.Z.zip`, `install.sh`, `SHA256SUMS` and `latest.json`.
  - `latest.json` names X.Y.Z and the zip's SHA-256 matches `SHA256SUMS`. Download them into a scratch folder to
    compare.
  - The notes read sensibly.

## 7. Publish

- Covered by `/release`. If the notes need tidying (CI plumbing, lines that are no longer true), edit them first
  (`gh release edit vX.Y.Z --notes-file …`). Then:
  ```sh
  gh release edit vX.Y.Z --draft=false --latest
  ```
- Then confirm that the public URLs serve the new version:
  ```sh
  curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/latest.json
  ```
  For a minute or two after publishing, GitHub's `releases/latest` still answers with the previous release on some
  requests. Repeat the check until several answers in a row name X.Y.Z before telling the user to press **Check Now**
  (Settings › General › Updates, or the menu's Update row). Installed copies otherwise notice within a day.

## 8. Report

Report the version, the public commit, the CI and release runs, the PRs merged and the branches deleted, anything you
asked about and the answers, and whether it is published.
