# Releasing Seedwell

Two routes. **Now:** an ad-hoc signed app installed with `curl … install.sh | bash` (no Apple Developer ID needed; see
below). **Later:** a Developer ID-signed, notarized `.dmg` with Sparkle updates (SPEC §12 #19), from "One-time setup" on.
The scripts do the work; this page lists the steps only you can do. Nothing here changes your keychain or publishes
anything until you run the commands yourself.

## Now: the curl installer (no Developer ID)

macOS blocks unsigned apps downloaded in a browser. Files fetched with `curl` aren't marked as downloaded from the internet,
so an app installed by a script opens normally. `install.sh` (repo root) downloads a GitHub release, checks its SHA-256
against the release's `SHA256SUMS`, verifies the code signature, quits a running Seedwell cleanly, installs to
`/Applications` (else `~/Applications`) and opens it. `--cli` links the `seedwell` command, `--version X` picks a release,
`--uninstall` removes the app (and asks before deleting `~/Library/Application Support/Seedwell`).

### Release with GitHub Actions (the usual way)

`.github/workflows/ci.yml` runs clippy, the Rust tests, the app build and the Swift tests on every push to `main`
and on pull requests. `.github/workflows/release.yml` runs when a `vX.Y.Z` tag is pushed: it checks that the tag
matches `Cargo.toml`, builds the release (`app/release-zip.sh --draft`, build number 100 + the run number) and
creates a **draft** release with the zip, `install.sh`, `SHA256SUMS` and `latest.json`. Publishing stays manual.

```sh
# on master: bump [workspace.package] version in Cargo.toml, commit, then publish the tree (see below) and tag it
c=$(git commit-tree 'master^{tree}' -p public -m "Seedwell 0.2.0")
git ls-tree -r --name-only "$c" | wc -l                 # same count as `git ls-files | wc -l`; see AGENTS.md
git push origin "${c}:refs/heads/main" && git update-ref refs/heads/public "$c"
git tag v0.2.0 "$c" && git push origin v0.2.0           # starts the release workflow
gh run watch                                            # wait for CI and the release build
```

Then read the draft on GitHub and press **Publish**. The release notes list the public commits since the previous
tag, so give the public commit a message that says what changed.

Dependabot (`.github/dependabot.yml`) opens weekly pull requests for Cargo and Actions updates; CI tests each one.

### Release from this Mac (fallback)

```sh
git status                                              # must be clean
tools/sysguard/heavy.sh app/release-zip.sh              # full LTO build → app/build/release/
#   Seedwell-<version>.zip  install.sh  SHA256SUMS  latest.json  NOTES.md   (checked locally: checksum, signature, script syntax)
app/release-zip.sh --draft                              # optional: creates a DRAFT GitHub release with the assets (gh CLI)
```

Then open the draft on GitHub, read it, and press **Publish**. The install URL
(`https://github.com/kkollsga/seedwell/releases/latest/download/install.sh`) works from the moment it is published.
Updates: users run the same command again, or let the app do it (see "Updates" below).

Test the installer without touching your own install: `SEEDWELL_REPO_URL=http://127.0.0.1:8765 SEEDWELL_INSTALL_DIR=/tmp/x`
against a local `python3 -m http.server` that serves `releases/latest/download/` with the three assets.

### First publication: a fresh public history

The local history contains development notes that are not meant to be public. Publish a new repository whose first
commit is the current tree, and keep the full history only here:

```sh
c=$(git commit-tree 'master^{tree}' -m "Seedwell 0.1.0")    # master's files, no history
git push origin "${c}:refs/heads/main" && git update-ref refs/heads/public "$c"
```

Later releases: commit on `master` as usual, then publish its tree on top of the last public commit:
`c=$(git commit-tree 'master^{tree}' -p public -m "Seedwell <version>") && git push origin "${c}:refs/heads/main" && git update-ref refs/heads/public "$c"`.

Never use `git switch --orphan` here: it removes the tracked files from the working folder, and a following
`git add -A` commits only the ignored build output (`target/`, hundreds of GB).

## Version

One source: `version` under `[workspace.package]` in `Cargo.toml`. It becomes `CFBundleShortVersionString`,
`seedwell --version`, the User-Agent and the peer id. The build number (`CFBundleVersion`) is the git commit count.
Settings › General shows both ("Seedwell 0.1.0 (build 86)"). To release: bump the version, commit, run the script.

## One-time setup

1. **Apple Developer Program.** Enroll at <https://developer.apple.com/programs/enroll/> (99 USD/year). Wait for approval.
2. **Developer ID Application certificate.** Xcode › Settings › Accounts › your team › Manage Certificates › `+` ›
   *Developer ID Application*. (Without Xcode: developer.apple.com › Certificates › `+` › Developer ID Application, upload a
   CSR made in Keychain Access, download and open the `.cer`.) Check:
   ```sh
   security find-identity -v -p codesigning
   # 1) ABCD...  "Developer ID Application: Your Name (TEAMID)"
   ```
3. **Notarization credentials.** Create an app-specific password at <https://account.apple.com> › Sign-In and Security ›
   App-Specific Passwords, then store a keychain profile:
   ```sh
   xcrun notarytool store-credentials "seedwell-notary" \
       --apple-id "you@example.com" --team-id "TEAMID" --password "xxxx-xxxx-xxxx-xxxx"
   ```
4. **Sparkle EdDSA keys.** This writes a private key to your login keychain, so it is yours to run, once:
   ```sh
   app/fetch-sparkle.sh             # downloads the pinned Sparkle 2.9.6 into app/.sparkle (once)
   app/.sparkle/bin/generate_keys   # prints the public key; export a backup with: generate_keys -x seedwell-private.key
   ```
   Put the printed public key in `app/sparkle_public_key` (one line; add it to git, it is public) or in `SEEDWELL_SPARKLE_PUBLIC_KEY`.
   Losing the private key means installed copies can never update again. Builds without a public key never start Sparkle;
   the GitHub-release updater drives those.
5. **Environment** (for example in `~/.zshrc`):
   ```sh
   export SEEDWELL_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
   export SEEDWELL_NOTARY_PROFILE="seedwell-notary"
   ```

## Every release

```sh
git status                                  # must be clean; the script refuses a dirty tree (--allow-dirty for test builds)
tools/sysguard/heavy.sh app/release.sh      # full LTO build, signs, dmg, notarizes, staples, verifies
```

It prints version and build, then writes `app/build/release/Seedwell-<version>.dmg`. Without `SEEDWELL_SIGN_IDENTITY` it
signs ad-hoc and says clearly that the result is NOT distributable; without `SEEDWELL_NOTARY_PROFILE` it skips notarization.

What gets signed, inside-out, all with the hardened runtime (`--options runtime`) and a secure timestamp: the bundled CLI
(`Contents/MacOS/seedwell`, identifier `io.github.kkollsga.Seedwell.cli`), Sparkle's XPC services, `Autoupdate`,
`Updater.app` and `Sparkle.framework`, then `Seedwell.app`. Then the dmg itself is signed, submitted with
`notarytool submit --wait`, stapled, and checked with `spctl -a -vv -t install`, `stapler validate` and
`codesign --verify --deep --strict`.

**Entitlements** (`app/Seedwell.entitlements`): none. Seedwell is not sandboxed, so the network client/server and
user-selected-files entitlements, which only exist inside a sandbox, would do nothing. Peer/tracker/DHT/uTP sockets and file
access work under the hardened runtime as is. No JIT, no DYLD variables, no Apple events.

## Updates

Two back ends, chosen at launch (SPEC §12A): the GitHub-release updater below, used by every build until Sparkle is
configured; Sparkle, used only by a build that carries `SUPublicEDKey` and `SUFeedURL`.

### GitHub release updater (now)

- `release-zip.sh` writes `latest.json` next to the zip (`version`, `tag`, `notes_url`, `minimum_macos`, `zip`, `sha256`, all from
  the script's own values) and `--draft` uploads it with the other assets. It is not in `SHA256SUMS`; `install.sh` checks the zip itself.
- Installed copies read `<repo>/releases/latest/download/latest.json` once a day, which GitHub resolves to the release marked
  **Latest**. So **Publish is the moment users are notified** (within a day). A draft, or a release marked as pre-release,
  is never offered. A bad release is withdrawn by deleting it or by marking the previous release as Latest.
- Only release builds (`SEEDWELL_RELEASE=1`, set by `release-zip.sh`) check and install by themselves; your own
  development builds (`build.sh`, `tools/install-local.sh`) only check when you press Check Now.
- `build.sh` writes `SeedwellRepoURL` into Info.plist (default `https://github.com/kkollsga/seedwell`; `SEEDWELL_REPO_URL` overrides).
  For tests, launch the app with `SEEDWELL_UPDATE_REPO=http://127.0.0.1:PORT` and serve `/releases/latest/download/latest.json`
  and `/releases/download/v<version>/install.sh` (plus the installer's own files) from there.
- The install log is `~/Library/Logs/Seedwell/update.log`.

### Sparkle (when a Developer ID exists)

- `SUFeedURL` in Info.plist is `https://kkollsga.github.io/seedwell/appcast.xml` (a placeholder; override with
  `SEEDWELL_FEED_URL` when building). Publish it with GitHub Pages (repository `kkollsga/seedwell`, branch `gh-pages` or `/docs`).
- Automatic checks are off (`SUEnableAutomaticChecks` false) and the user can switch them on in Settings › General ›
  Updates, which also has **Check Now**. Nothing contacts the network at launch.
- After each release, build the feed over a folder holding every dmg you have shipped:
  ```sh
  cp app/build/release/Seedwell-<version>.dmg ~/seedwell-releases/
  SEEDWELL_DOWNLOAD_URL_PREFIX="https://github.com/kkollsga/seedwell/releases/download/v<version>/" \
      app/make-appcast.sh ~/seedwell-releases
  ```
  It runs Sparkle's `generate_appcast`, which signs each archive with the private key from step 4 and writes `appcast.xml`
  (plus delta updates). Upload `appcast.xml` to the feed location and attach the dmg to the GitHub release.
- First release order: ship one release with the public key and feed URL set, publish the appcast, then switch updates on.

## Homebrew cask (optional)

After the first notarized release, a cask in a tap (`kkollsga/homebrew-tap`) is a few lines: `url` pointing at the GitHub release dmg,
`sha256` from `shasum -a 256`, `app "Seedwell.app"`, `depends_on macos: ">= :sequoia"`, and `livecheck` on the appcast.

## Sparkle pin

Sparkle 2.9.6 is pinned by version and SHA-256 in `app/fetch-sparkle.sh` (used by `build.sh`, `make-appcast.sh`; run it once
before `swift test`/`swift build` in `app/`). Package.swift links the result as a binary target. A plain
`.package(url:, exact:)` dependency hung in "Downloading binary artifact" with the macOS 27 Command Line Tools SwiftPM; if yours
works, you can switch back, and `build.sh` only needs the framework path adjusted.
