# File Provider spike (on-demand folders, phase 1)

2026-10-31, macOS 27.0.1, Xcode 27.2. The question: can a Developer ID-signed, notarized Seedwell carry a File
Provider extension that puts an on-demand folder in Finder (SPEC §7G.6), built the way Seedwell already builds its
extensions? The spike app (`spikes/fileprovider/`, bundle id `io.github.kkollsga.Seedwell.FPSpike`, domain
"Seedwell Test") was built, signed, notarized and registered once, and then removed again. The user's rule since
then is that nothing but the installed Seedwell runs on this Mac, so the live checks that need the domain enabled
were not done; they move to development builds of the real app (below).

## Confirmed

| | Finding |
|---|---|
| Build | A File Provider extension builds like `SeedwellFinderSync`/`SeedwellShare`: a SwiftPM `executableTarget` with `-application-extension`, linked with `-Xlinker -application_extension` and `FileProvider`, its `main.swift` calling `NSExtensionMain`, wrapped by the build script into `Contents/PlugIns/<Name>.appex` with `NSExtensionPointIdentifier` `com.apple.fileprovider-nonui`, `NSExtensionPrincipalClass` (an `@objc(Name)` class), `NSExtensionFileProviderDocumentGroup` = the app group and `NSExtensionFileProviderSupportsEnumeration` = true. No Xcode project needed. |
| Signing identity | Pick the identity by its SHA-1, not its name: "Developer ID Application: Kristian de Figueiredo Kollsgård (6GND8Z2C7Z)" has an "å" that `security find-identity` prints in a form `codesign --sign "<name>"` does not match ("no identity found"). `security find-identity -v -p codesigning \| grep 'Developer ID Application' \| grep '(6GND8Z2C7Z)' \| awk '{print $2; exit}'`. |
| Signing order | Inside out, no `--deep`: the `.appex` with its entitlements, then the app with its own; both `--options runtime --timestamp`. Both show `TeamIdentifier=6GND8Z2C7Z`, `Authority=Developer ID Application: …`, a secure timestamp; `codesign --verify --deep --strict` passes. |
| Keychain prompt | The first local `codesign` with a key Xcode made asks the login keychain once ("codesign wants to sign using key …"): *Always Allow* ends that. A `codesign` left waiting on the prompt for hours then fails with "timestamps differ by 17444 seconds - check your system clock" (it took the signing time when it started): just sign again. CI is unaffected (its own keychain with the partition list set). |
| Entitlements | Extension: `com.apple.security.app-sandbox` = true and `com.apple.security.application-groups` = [`6GND8Z2C7Z.<group>`]. App: the same group. **No provisioning profile**: the Team-prefixed group is accepted on a Developer ID build (the group container was made, with no consent prompt), the extension loads and registers. (A `group.`-prefixed id would need a profile; don't use one.) |
| App group container | `~/Library/Group Containers/6GND8Z2C7Z.<group>/`, made by the system on first use (`containerURL(forSecurityApplicationGroupIdentifier:)`); it also holds `File Provider Storage/`. A Unix socket there works from the non-sandboxed app (bound and listened). |
| Socket path length | `sun_path` takes 103 bytes. Here (`$HOME` = `/Volumes/EksternalHome/KristianEX`, 33 bytes) the spike's socket `…/Group Containers/6GND8Z2C7Z.seedwell.fpspike/s` was 88 bytes; Seedwell's real group `6GND8Z2C7Z.io.github.kkollsga.Seedwell` makes it 99 with a one-letter name: fits, barely. Keep the socket name one or two letters, and fall back (`chdir` + relative bind/connect, or a shorter group) when `$HOME` is longer. |
| Notarization | The app with the extension: `notarytool submit … --wait` "Accepted" in 18 s, `stapler staple` OK, `spctl --assess --type execute`: "accepted, source=Notarized Developer ID". |
| Registration | `NSFileProviderManager.add(domain)` from the app works (the app run from a build folder, after `lsregister -f` and `pluginkit -a` of the bundle). The domain's folder is `~/Library/CloudStorage/<CFBundleName>-<domain display name without spaces>` (here `FPSpike-SeedwellTest`). `getUserVisibleURL(for: .rootContainer)` returns it. |
| **Disabled by default** | A new domain starts **user-disabled**: `fileproviderctl dump` shows `⏹ user-disabled` / `enabled: no`, every request fails with `NSFileProviderErrorDomainDisabled` (-2011, "Syncing is not enabled"), the extension is never started and reading the folder times out (`ls: fts_read: Operation timed out`). Neither `pluginkit -e use` nor removing and adding the domain changes that; the API has only the read-only `NSFileProviderDomain.userEnabled`. The user turns it on in **System Settings › General › Login Items & Extensions › Extensions › File Providers** (or from Finder's sidebar). |
| Clean-up | `removeAllDomains`, `pluginkit -r` of the `.appex`, `lsregister -u` of the app, deleting the app and the test group container leave nothing: `~/Library/CloudStorage` has no entry for it and `fileproviderctl dump` none. |

## Not tested here (in the real app, via development builds)

Everything that needs the domain enabled: enumeration and placeholders (`ls -lO` "dataless"), fetch on open through
the socket and its latency, behaviour with the app not running, eviction (`evictItem`), pinning (`contentPolicy`
`.downloadEagerlyAndKeepDownloaded`) and what Finder's menu offers, and changes back (create, modify, rename,
delete). The spike's extension and app implement all of these (`spikes/fileprovider/Sources`), so the same code
shapes carry over.

## Recommended architecture (phases 2 and 3)

- **App group** `6GND8Z2C7Z.io.github.kkollsga.Seedwell`, Team-prefixed, no provisioning profile. The app and the
  extension carry it; the extension is sandboxed, the app stays unsandboxed.
- **Extension = thin Swift**, an `NSFileProviderReplicatedExtension`: it keeps no state. `item`, `enumerator`
  (items and changes since an anchor), `fetchContents`, `createItem`/`modifyItem`/`deleteItem` each become one
  request to the running app over a Unix socket in the group container (a short name, e.g. `fp`), answered by the
  core. Items carry the core's ids; versions come from the sync index's version vectors, sync anchors from its
  change sequence; content arrives as a file written into the domain's `temporaryDirectoryURL` (or streamed over
  the socket for small files). App not running: answer `serverUnreachable`, so the system retries; the app starts
  at login, and a "Seedwell isn't running" notice is the host's.
- **Core (Rust decides)**: an on-demand folder mode (SPEC §7G.6) in trs-sync: metadata always, contents only on a
  fetch or a pin; the request/answer protocol and its decisions in trs-ffi, so the Windows Cloud Files shim reuses
  them.
- **Enablement**: after `add(domain)`, read `userEnabled`; while false, the host shows "Turn on Seedwell in System
  Settings" with a button that opens the Extensions pane (verify the exact `x-apple.systempreferences:` URL for
  Login Items & Extensions on macOS 15 to 27 when building it), and watches for the domain to become enabled.
- **Build and signing**: `app/build.sh` adds the `.appex` (as above); `app/sign.sh` signs it with its sandbox and
  group entitlements before the app, and the app's entitlements gain the group; `release-zip.sh --signed` and
  `release.yml` need no change beyond that (identity by SHA-1 already, notarization covers the extension). Local
  dev builds sign with the Developer ID after the one-time keychain *Always Allow*.
- **Test path**: development builds of the real app (the dev-builds switch), with the user enabling the domain once
  in System Settings; the checks listed under "Not tested here".
