# On-demand folders in the Finder: checks in the user's app

What the unit tests cannot reach: a real, enabled File Provider domain. These run once in the user's own Seedwell
through a development build (`tools/dev-build.sh`, then `seedwell update install <zip>`), with a small test folder
shared from another device. Agents check through the CLI and the system's tools; the user does one short batch in the
Finder at the end. Nothing here registers a domain of its own: the domain is the user's folder's.

The app group's container (and so the socket) is reliably there only for a build signed with the team's Developer
ID, as the development builds for the user's app are; an ad-hoc build may be refused access to the container, and its
extension is then served by nothing (the socket also checks the extension's signature, which an ad-hoc build pins by
its cdhash).

The minute the app gives the system to hand over a domain's pending changes before removing it likely cannot cut
`waitForChanges` short (it is not cancellable); the real bound is the core's: it refuses the removal or switch after
two minutes, and a removal that completes later then leaves the domain (or adds it back).

## Agent checks (CLI, logs)

| # | Do | Expect |
|---|---|---|
| 1 | `seedwell sync join --on-demand <link>` (or switch a small folder: Edit… › Files on demand) | `fileproviderctl dump` lists a domain `io.github.kkollsga.Seedwell.FileProvider/<folder id>`, user-disabled until step U1 |
| 2 | `log show --last 5m --predicate 'subsystem == "io.github.kkollsga.Seedwell" AND category == "file-provider"'` | "serving the File Provider extension at …/Group Containers/6GND8Z2C7Z.io.github.kkollsga.Seedwell/fp", "domain added" |
| 3 | After U1: `ls -lO ~/Library/CloudStorage/Seedwell-<Name>` | every file listed, `dataless` on the online-only ones; sizes and times as `seedwell sync items` |
| 4 | `cat ~/Library/CloudStorage/Seedwell-<Name>/<file>` | the content; `seedwell sync items` shows it downloaded; no file left in the group container's `t/` |
| 5 | Change a file on the other device | the change shows in the Finder's copy within seconds (the feed signals the working set) |
| 6 | `echo x > ~/Library/CloudStorage/Seedwell-<Name>/new.txt`, `mv`, `rm` there | the other device gets the new file, the rename (same file id), the delete; `u/` is empty afterwards |
| 7 | Edit a downloaded file here while the other device changes it (offline, then online) | a conflict copy next to it, the other version in place |
| 8 | Quit Seedwell (menu › Quit), then `cat` an online-only file | Seedwell starts by itself (menu bar only) and the read succeeds; with the other device offline: "No device that has this file is connected right now" after 30 s |
| 9 | A read-only member: `rm` a downloaded file | the file comes back online only (`dataless`); nothing is deleted on other devices |
| 10 | A rename on a read-write member while no device that holds the files is connected | the Finder shows the error "Renaming needs a device that has these files to be connected" |
| 11 | `fileproviderctl evict ~/Library/CloudStorage/Seedwell-<Name>/<file>` | `seedwell sync items` shows it online only (the materialized set was reported); a file only this Mac has stays |
| 12 | Edit… › Files on demand off; Stop Syncing… of an on-demand folder; the same with `seedwell sync mode`/`sync remove` | the domain is gone from `fileproviderctl dump` before the folder moves; the files are at the folder's place; a change made in the Finder just before is in the folder or in `<name> (changes not synced)` next to it, with a notice. Quit and reopen Seedwell: no domain is removed or added twice |
| 13 | Switch a small full folder to on demand, allow it (U1), then `ls -lO ~/Library/CloudStorage/Seedwell-<Name>` | no file `dataless` after a few seconds; `seedwell sync items` unchanged (nothing fetched from another device, nothing freed); `du -sh` of the domain about the same as before (clones). Quit and reopen Seedwell: nothing is asked for again |

## The user's batch (Finder, once)

| # | Do | Expect |
|---|---|---|
| U1 | The "Allow Seedwell in the Finder" window: Open Settings, turn on Seedwell under File Providers | the window says it is on by itself; the folder is in the Finder's sidebar under Locations |
| U2 | Right-click a file: *Always Keep on This Mac*; then *Stop Keeping on This Mac* | the file downloads and stays; the action flips (the Finder's own *Remove Download* / *Download Now* work too) |
| U3 | Open a large online-only file | a progress ring in the Finder; cancelling it stops the fetch (`seedwell sync items`: not wanted any more) |
| U4 | Add Folder… › Shared with me, paste a link, tick *Files on demand*, Start; later Edit… › *Files on demand* off, Save | the note says what each does; the allow window comes up once; switched off, every file downloads into the folder's place |
