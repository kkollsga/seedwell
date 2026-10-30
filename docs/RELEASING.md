# Releasing Seedwell

On the Mac a release is **Developer ID signed and notarized** (from 0.1.11; the ones before were ad hoc): a disk image
to download and drag to Applications, and the same app as a zip for `install.sh` and the in-app updater. Windows
ships a zip and `install.ps1`. The release workflow builds both, with the signing certificate and the notarization
key from the repository's secrets ("One-time setup" below: the steps only you can do). Nothing here changes your
keychain or publishes anything until you run the commands yourself.

## Installing (what users get)

- **The disk image** (`Seedwell.dmg`, also as `Seedwell-X.Y.Z.dmg`): open it, drag Seedwell onto Applications.
  Opened straight from the disk image or from Downloads instead, the app offers to move itself to Applications
  (SPEC 12).
- **`install.sh`** (repo root): `curl … install.sh | bash` downloads a GitHub release, checks its SHA-256 against the
  release's `SHA256SUMS`, its code signature, that it is signed by Seedwell's Developer ID (`TEAM_ID` in install.sh)
  and that Gatekeeper accepts it (notarized), quits a running Seedwell cleanly, installs to `/Applications` (else
  `~/Applications`) and opens it. `--cli` links the `seedwell` command, `--version X` picks a release (one from
  before signing only this way), `--uninstall` removes the app (and asks before deleting
  `~/Library/Application Support/Seedwell`). `app/qa/install_signature.sh` tests the signature rule.

### Release with GitHub Actions (the usual way)

`.github/workflows/ci.yml` runs on every push to `main` and on pull requests, on the Mac (clippy, the Rust tests,
the app build, the Swift tests) and on Windows (clippy, the Rust tests, the app build and `app-windows\qa\run.ps1 all`,
the PowerShell scripts parsed). `.github/workflows/release.yml` runs when a `vX.Y.Z` tag is pushed, **all or nothing**:

1. *Tag and CI*: the tag matches `Cargo.toml`, and CI passed on the tagged commit (it waits up to an hour for CI to
   finish).
2. *Mac build* (`app/release-zip.sh --signed`, build number 100 + the run number: signed with the Developer ID,
   notarized and stapled, zip and disk image, each checked with Gatekeeper) and *Windows build*
   (`app-windows\release-zip.ps1`) side by side, each checking its own assets; a failure in one cancels the other.
   Without the signing secrets the Mac build fails at once: there is no unsigned release.
3. *Draft release*, only when both succeeded: `tools/release-merge.py` makes one `SHA256SUMS`, one `latest.json` (the
   Mac's fields unchanged, plus `"windows"`) and one set of notes (both installers, all checksums), and the draft gets
   `Seedwell-X.Y.Z.zip`, `install.sh`, `Seedwell-X.Y.Z-windows-x64.zip`, `install.ps1`, `SHA256SUMS` and `latest.json`.
4. *Withdraw the tag*, when anything failed or was cancelled: the tag (and a draft, if one was made) is deleted. Fix
   the failure on `master`, publish the tree again and tag the same version again.

Publishing the draft stays manual.

```sh
# on master: bump [workspace.package] version in Cargo.toml, commit, then publish the tree (see below) and tag it
c=$(git commit-tree 'master^{tree}' -p public -m "Seedwell 0.2.0")
git ls-tree -r --name-only "$c" | wc -l                 # same count as `git ls-files | wc -l`; see AGENTS.md
git push origin "${c}:refs/heads/main" && git update-ref refs/heads/public "$c"
git tag v0.2.0 "$c" && git push origin v0.2.0           # starts the release workflow
gh run watch                                            # wait for CI and the release build
```

Then read the draft on GitHub and press **Publish**. The release notes' Changes are the bullets of the public
commit's message (`- …` lines; a title alone is listed as one), so write what changed for users there. When a
release is cut again, only the newest "Seedwell X.Y.Z" commit counts; Dependabot merges since the previous tag are
listed by their titles.

Dependabot (`.github/dependabot.yml`) opens weekly pull requests for Cargo and Actions updates; CI tests each one.

### Build the Mac's assets on this Mac (a check, not a release)

```sh
git status                                              # must be clean
tools/sysguard/heavy.sh app/release-zip.sh              # full LTO build → app/build/release/
#   Seedwell-<version>.zip  Seedwell-<version>.dmg  Seedwell.dmg  install.sh  SHA256SUMS  latest.json  NOTES.md
#   (checked locally: checksums, signature, the disk image's contents, script syntax)
```

Without `--signed` it signs ad hoc and does not notarize, whatever your environment says: a check of the build, the
zip and the disk image's layout. `--signed` (what the workflow runs) needs the identity and a notary profile in
your keychain (`SEEDWELL_SIGN_IDENTITY`, `SEEDWELL_NOTARY_PROFILE`, see `app/release-zip.sh --help`).

This builds and checks the Mac's assets only; it never creates a release. A release is always the Mac's and the
Windows build together, made by the release workflow when both succeeded: a Mac-only release would leave every
Windows copy without an update.
Updates: users run the same command again, or let the app do it (see "Updates" below).

Test the installer without touching your own install: `SEEDWELL_REPO_URL=http://127.0.0.1:8765 SEEDWELL_INSTALL_DIR=/tmp/x`
against a local `python3 -m http.server` that serves `releases/latest/download/` with the three assets.

### Windows: the PowerShell installer (no code-signing certificate)

Without a certificate an MSIX does not install and an MSI or .exe from a browser gets a SmartScreen warning on every
download, so Windows takes the Mac's route: a script installer, per user, no administrator rights.

```powershell
irm https://github.com/kkollsga/seedwell/releases/latest/download/install.ps1 | iex
& ([scriptblock]::Create((irm https://github.com/kkollsga/seedwell/releases/latest/download/install.ps1))) -Version 0.2.0 -Cli
```

`install.ps1` (repo root, Windows PowerShell 5.1) downloads `SHA256SUMS` and `Seedwell-X.Y.Z-windows-x64.zip` from
the release, checks the zip's SHA-256, unpacks it next to the target and swaps it in (a failure leaves the old
version), quits a running Seedwell cleanly first (`Seedwell.exe --quit`), installs to `%LOCALAPPDATA%\Programs\Seedwell`
and starts it. Per user it adds a Start menu shortcut, the `.torrent` and `magnet:` handlers (`Seedwell.exe --register`,
the app's own rule) and an Apps & features entry whose Uninstall runs the kept `install.ps1 -Uninstall`. `-Cli` puts
`cli\seedwell.exe` on the user's PATH (the CLI lives in `cli\`: Windows file names ignore case, so `seedwell.exe` and
`Seedwell.exe` cannot share a folder), `-Version X` picks a release, `-NoLaunch` skips the start, `-Uninstall` removes
everything and asks before deleting `%LOCALAPPDATA%\Seedwell`. Running it again updates in place.

The release: the *Windows build* job of `.github/workflows/release.yml` builds with `app-windows\release-zip.ps1` (release Rust with `SEEDWELL_RELEASE=1` and full LTO, the app published
self-contained for win-x64 with the Windows App SDK inside, so nothing else needs installing), which also checks the
zip (hash, unzipped, the CLI's version, the unzipped app starts and draws a quiet render, `install.ps1` parses). The
*Draft release* job merges its `SHA256SUMS-windows` and `latest-windows.json` into the Mac's (`tools/release-merge.py`:
`latest.json` gains `"windows": {zip, sha256, minimum_windows}`, the Mac's fields unchanged; `install.sh` takes the Mac
zip by its exact name). Review the draft as usual: both platforms are in it.

**The gate is CI, never a person or a machine.** A release needs CI passing on the Mac and Windows (Rust tests,
clippy, the app's QA checks on both) and the release workflow passing (both builds, and on Windows the installer test
`app-windows\qa\install-test.ps1` against the built zip). A Windows PC with an agent on it speeds up Windows work and
may test more by hand, but a release never waits for it: what it finds is fixed like any other bug, in the next
release if this one is out. So the Windows app stays a thin shell over the core (Rust decides, the host draws), and
a rule that matters is tested in Rust, where both CIs run it.

Build it on a Windows PC (a check; it never creates a release): `app-windows\release-zip.ps1` (refuses a dirty tree; `-AllowDirty` for a test
build) writes `app-windows\bin\release\` with the zip, `install.ps1`, `SHA256SUMS-windows` and `latest-windows.json`.

Test the installer without touching your own install: `app-windows\qa\install-test.ps1` serves those assets from a
local web server laid out like GitHub's, and installs, updates, installs a pinned version and uninstalls with the
test seam on (`SEEDWELL_INSTALL_TEST=1`: every registry key under `HKCU\Software\SeedwellInstallTest`, the shortcut in
a temporary folder, the app never started), then checks that nothing is left and that the real places did not change.

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

## One-time setup (the Mac's signing secrets)

Done once, by you: the certificate and the key are tied to your Apple account. Never paste a password or a key into a
chat or a command line; the commands below read them from files or ask for them without echoing.

1. **Apple Developer Program.** Done (individual, Team ID `6GND8Z2C7Z`, `TEAM_ID` in `install.sh`).
2. **Developer ID Application certificate.** Xcode › Settings › Accounts › your Apple ID › Manage Certificates › `+` ›
   *Developer ID Application*. It is made in this Mac's login keychain. Check:
   ```sh
   security find-identity -v -p codesigning
   #   1) ABCD…  "Developer ID Application: <your name> (6GND8Z2C7Z)"
   ```
3. **Export it for CI.** Keychain Access › login › My Certificates › *Developer ID Application: … (6GND8Z2C7Z)* ›
   right-click › Export… › format *Personal Information Exchange (.p12)*, saved as `~/Desktop/seedwell-developer-id.p12`
   with a new, strong password (it protects the private key inside).
4. **Notarization key.** <https://appstoreconnect.apple.com> › Users and Access › Integrations › App Store Connect API ›
   Team Keys › `+`, name "Seedwell notarization", access *Developer*. Download the `AuthKey_<KEY ID>.p8` (only once
   possible) and note the **Key ID** and, above the list, the **Issuer ID**.
5. **The five secrets** (GitHub CLI; `gh secret set NAME` without a value asks for it, hidden):
   ```sh
   base64 -i ~/Desktop/seedwell-developer-id.p12 | gh secret set MACOS_CERT_P12 --repo kkollsga/seedwell
   gh secret set MACOS_CERT_PASSWORD --repo kkollsga/seedwell      # the .p12's password
   base64 -i ~/Downloads/AuthKey_<KEY ID>.p8 | gh secret set NOTARY_KEY_P8 --repo kkollsga/seedwell
   gh secret set NOTARY_KEY_ID --repo kkollsga/seedwell            # the Key ID
   gh secret set NOTARY_ISSUER_ID --repo kkollsga/seedwell         # the Issuer ID
   gh secret list --repo kkollsga/seedwell                         # the five names (values are never shown)
   ```
   Then keep the `.p12` (with its password) and the `.p8` somewhere safe, such as a password manager, and delete
   them from Desktop and Downloads. A lost `.p8` is replaced by a new key in App Store Connect; a leaked one is
   revoked there.

The release workflow's Mac job imports the certificate into a keychain of its own, stores the notarization key as a
`notarytool` profile in it, runs `app/release-zip.sh --signed` and deletes the keychain again. A certificate expires
after five years; renew it in Xcode and repeat steps 3 and 5 for `MACOS_CERT_P12` and `MACOS_CERT_PASSWORD`.

**Signing on this Mac instead** (a test of `--signed`, never a release): with the certificate from step 2 in your
keychain, store the key as a profile once and run the script:
```sh
xcrun notarytool store-credentials seedwell-notary --key ~/path/AuthKey_<KEY ID>.p8 --key-id <KEY ID> --issuer <ISSUER ID>
SEEDWELL_SIGN_IDENTITY="Developer ID Application: <your name> (6GND8Z2C7Z)" SEEDWELL_NOTARY_PROFILE=seedwell-notary \
    tools/sysguard/heavy.sh app/release-zip.sh --signed
```
`app/release.sh` (a disk image only, the older local route) uses the same two variables.

**What signing changes for users.** The first signed release (0.1.11) is a new code identity to macOS: things it
remembered for the ad-hoc app may be asked for once more (opening at login, notifications, access to folders that
needed permission). Updates after that keep the identity.

**Entitlements** (`app/Seedwell.entitlements`): none. Seedwell is not sandboxed, so the network client/server and
user-selected-files entitlements, which only exist inside a sandbox, would do nothing. Peer/tracker/DHT/uTP sockets and
file access work under the hardened runtime as is. No JIT, no DYLD variables, no Apple events. The data protection
keychain for Folder Sync's keys needs a keychain entitlement with a provisioning profile, which this build does not
have yet, so the keys stay in their 0600 file (SPEC 7G.1).

## Updates

Two back ends, chosen at launch (SPEC §12A): the GitHub-release updater below, used by every build until Sparkle is
configured; Sparkle, used only by a build that carries `SUPublicEDKey` and `SUFeedURL`.

### GitHub release updater (now)

- `release-zip.sh` writes `latest.json` next to the zip (`version`, `tag`, `notes_url`, `minimum_macos`, `zip`, `sha256`, all from
  the script's own values); the release workflow adds the Windows entry and uploads it with the other assets. It is not in `SHA256SUMS`; `install.sh` checks the zip itself.
- Installed copies read `<repo>/releases/latest/download/latest.json` once a day, which GitHub resolves to the release marked
  **Latest**. So **Publish is the moment users are notified** (within a day). For a minute or two after publishing, GitHub still
  resolves `latest` to the previous release on some requests, so an immediate Check Now can report "up to date". A draft, or a release marked as pre-release,
  is never offered. A bad release is withdrawn by deleting it or by marking the previous release as Latest.
- Only release builds (`SEEDWELL_RELEASE=1`, set by `release-zip.sh`) check and install by themselves; your own
  local builds (`build.sh`, `tools/install-local.sh`) only check when you press Check Now.
- **Development builds** (SPEC 12A) are how the installed app runs work in progress without a release: with
  Settings › General › Updates › Allow development builds on, `tools/sysguard/heavy.sh tools/dev-build.sh` makes
  `app/build/dev/Seedwell-X.Y.Z.N.zip` (the next number for Cargo.toml's version) and `seedwell update install <zip>`
  installs it; `tools/dev-build.sh --to <folder>` puts it in a folder the app watches (`seedwell update watch
  <folder>`), which installs it as it appears. The app keeps the build it replaces in `previous/` in its state folder
  and goes back to it when a build does not start within two minutes. Turning the switch off installs the latest
  release. A release is never `X.Y.Z.N`: the release workflow builds without `SEEDWELL_DEV_VERSION`.
- `build.sh` writes `SeedwellRepoURL` into Info.plist (default `https://github.com/kkollsga/seedwell`; `SEEDWELL_REPO_URL` overrides).
  For tests, launch the app with `SEEDWELL_UPDATE_REPO=http://127.0.0.1:PORT` and serve `/releases/latest/download/latest.json`
  and `/releases/download/v<version>/install.sh` (plus the installer's own files) from there.
- The install log is `logs/update.log` in the state folder (`~/Library/Application Support/Seedwell` on the Mac, `%LOCALAPPDATA%\Seedwell` on Windows); the updater's own record is `updates.json` next to it.

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

Sparkle 2.10.1 is pinned by version and SHA-256 in `app/fetch-sparkle.sh` (used by `build.sh`, `make-appcast.sh`; run it once
before `swift test`/`swift build` in `app/`). Package.swift links the result as a binary target. A plain
`.package(url:, exact:)` dependency hung in "Downloading binary artifact" with the macOS 27 Command Line Tools SwiftPM; if yours
works, you can switch back, and `build.sh` only needs the framework path adjusted.
