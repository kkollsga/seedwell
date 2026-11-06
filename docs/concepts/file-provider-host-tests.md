# On-demand folders in the Finder: checks in the user's app

What the unit tests cannot reach: a real, enabled File Provider domain. These run once in the user's own Seedwell
through a development build (`tools/dev-build.sh`, then `seedwell update install <zip>`), with a small test folder
shared from another device. Agents check through the CLI and the system's tools; the user does one short batch in the
Finder at the end. Nothing here registers a domain of its own: the one domain is the user's hub (*Seedwell* in the
Finder's sidebar, listing every synced folder, SPEC §7G.11). `<Hub>` below is its folder: `getUserVisibleURL` of its
root, expected at `~/Library/CloudStorage/Seedwell-Seedwell` (check H1 confirms the real path).

The app group's container (and so the socket) is reliably there only for a build signed with the team's Developer
ID, as the development builds for the user's app are; an ad-hoc build may be refused access to the container, and its
extension is then served by nothing (the socket also checks the extension's signature, which an ad-hoc build pins by
its cdhash).

The minute the app gives the system to hand over a folder's pending changes (`waitForChanges(below:)`) before it
lets go of it likely cannot cut the wait short (it is not cancellable); the real bound is the core's: it refuses the
removal or switch after two minutes, and a let-go that completes later is undone (the folder comes back in the hub).

## Agent checks (CLI, logs)

| # | Do | Expect |
|---|---|---|
| 1 | Start the development build with at least one synced folder | `fileproviderctl dump` lists one domain of Seedwell, `io.github.kkollsga.Seedwell.FileProvider/seedwell`, user-disabled until step U1; no `Seedwell-<Name>` domain is left |
| 2 | `log show --last 5m --predicate 'subsystem == "io.github.kkollsga.Seedwell" AND category == "file-provider"'` | "serving the File Provider extension at …/Group Containers/6GND8Z2C7Z.io.github.kkollsga.Seedwell/fp", "domain added" |
| 3 | After U1: `ls -lO <Hub>` and `ls -lO <Hub>/<Name>` | every synced folder once, in name order: on-demand ones as folders, full ones as links (`ls -l` shows `-> <their place>`); inside an on-demand one every file, `dataless` on the online-only ones; sizes and times as `seedwell sync items` |
| 4 | `cat <Hub>/<Name>/<file>` | the content; `seedwell sync items` shows it downloaded; no file left in the group container's `t/` |
| 5 | Change a file on the other device | the change shows in the Finder's copy within seconds (the feed signals the working set) |
| 6 | `echo x > <Hub>/<Name>/new.txt`, `mv`, `rm` there; `mv <Hub>/<Name>/new.txt <Hub>/<Other>/` | the other device gets the new file, the rename (same file id), the delete; `u/` is empty afterwards; the move into another folder is refused (an error in the Finder, nothing changes in either folder) |
| 7 | Edit a downloaded file here while the other device changes it (offline, then online) | a conflict copy next to it, the other version in place |
| 8 | Quit Seedwell (menu › Quit), then `cat` an online-only file | Seedwell starts by itself (menu bar only) and the read succeeds; with the other device offline: "No device that has this file is connected right now" after 30 s |
| 9 | A read-only member: `rm` a downloaded file | the file comes back online only (`dataless`); nothing is deleted on other devices |
| 10 | A rename on a read-write member while no device that holds the files is connected | the Finder shows the error "Renaming needs a device that has these files to be connected" |
| 11 | `fileproviderctl evict <Hub>/<Name>/<file>` | `seedwell sync items` shows it online only (the materialized set was reported); a file only this Mac has stays |
| 12 | Edit… › Files on demand off; Stop Syncing… of an on-demand folder; the same with `seedwell sync mode`/`sync remove` (write a file into `<Hub>/<Name>` just before) | the folder leaves `<Hub>` (switched: it comes back as a link) and the hub stays; the files are at the folder's place; the file written just before is in the folder or in `<name> (changes not synced)` next to it, with a notice; the other folders in `<Hub>` are untouched |
| 13 | Switch a small full folder to on demand, then `ls -lO <Hub>/<Name>` | no file `dataless` after a few seconds; `seedwell sync items` unchanged (nothing fetched from another device, nothing freed); `du -sh` about the same as before (clones). Quit and reopen Seedwell: nothing is asked for again |

| H1 | `fileproviderctl dump`, then `ls -l` of the domain's folder it names | one Seedwell domain; its folder's real path (put it for `<Hub>` above) |
| H2 | `ls -l <Hub>/<Full>` and `cat <Hub>/<Full>/<a file>` for a full folder | the link resolves to the folder's own place (also one outside `~/Seedwell`, on another disk); the file reads without any download and is the same inode as at its place (`stat -f %i`) |
| H3 | Migration: install the development build over 0.1.19 with an on-demand folder whose domain `Seedwell-<Name>` is enabled, after `echo x > ~/Library/CloudStorage/Seedwell-<Name>/pending.txt` with Seedwell quit | at its first start the old domain goes (`fileproviderctl dump`), `pending.txt` is in the folder (handed over) or in `<Name> (changes not synced)` with a notice; then the hub is added (one "Allow Seedwell" window). Quit and reopen: no domain removed or added again |

## The user's batch (Finder, once)

| # | Do | Expect |
|---|---|---|
| U1 | The "Allow Seedwell in the Finder" window: Open Settings, turn on Seedwell under File Providers | the window says it is on by itself; one *Seedwell* entry, with Seedwell's symbol, is in the Finder's sidebar under Locations, listing every synced folder |
| U1b | In the Finder: open a full folder's item in *Seedwell* | the Finder opens the folder at its own place (or shows the link); nothing downloads |
| U2 | Right-click a file: *Always Keep on This Mac*; then *Stop Keeping on This Mac* | the file downloads and stays; the action flips (the Finder's own *Remove Download* / *Download Now* work too) |
| U3 | Open a large online-only file | a progress ring in the Finder; cancelling it stops the fetch (`seedwell sync items`: not wanted any more) |
| U4 | Add Folder… › Shared with me, paste a link, tick *Files on demand*, Start; later Edit… › *Files on demand* off, Save | the note says what each does; the allow window comes up once; switched off, every file downloads into the folder's place |
