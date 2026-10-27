# Seedwell on macOS, Windows and Linux

Seedwell gets Windows and Linux apps with the same features as the Mac app, and devices on all three systems sync,
chat and share with each other. This file sets the rules that make that possible and tracks the work. `SPEC.md`
stays the behaviour specification; `docs/ENGINEERING.md` the crate contract.

## 1. The rule: Rust decides, the host draws

Everything a second app would otherwise have to write again lives in Rust and is shared:
- what is shown: every user-visible sentence built from data, number/size/rate/time formatting, plural forms;
- how it is arranged: which sections and rows a list or menu has, their order, caps ("+ 3 more"), what changed
  since the last refresh (signatures), which controls a row offers;
- what happens: state machines (countdowns, removal prompts, edits in progress, update checks), validation and
  parsing of what the user types (ports, rates, times, links), polling cadence, notification decisions and texts,
  link routing (`seedwell://…`), the file tree and the chooser.

The host app (Swift on macOS, later one app for Windows and one for Linux) keeps only:
- drawing and input: views, menus, windows, popovers, keyboard and mouse events, accessibility;
- OS services behind narrow seams (§2), each a small interface that Rust calls or that calls Rust.

A host asks Rust for a **view model** (a record with ready text, flags and rows) and draws it. A rule found in
Swift that is not drawing or OS integration is a bug in this sense and moves to Rust (§4).

## 2. Seams

**Host seams** (the app implements them; Rust holds the policy):

| Seam | macOS | Windows | Linux |
|---|---|---|---|
| Secrets (`SecretVault`) | Keychain | Credential Manager / DPAPI | Secret Service (libsecret); file when headless |
| Keep awake (lease → assertion; `power_needs`) | IOPMAssertion, App Nap activity | `PowerCreateRequest` / `SetThreadExecutionState` | logind `Inhibit` |
| Power source, network, sleep/wake | IOPS, NWPathMonitor, NSWorkspace | `RegisterPowerSettingNotification`, `NotifyIpInterfaceChange`, `WM_POWERBROADCAST` | UPower, NetworkManager, logind `PrepareForSleep` |
| Notifications | UNUserNotificationCenter | App notifications (toast) | `org.freedesktop.Notifications` |
| Menu bar / tray | NSStatusItem + NSMenu | Notification-area icon + menu | StatusNotifierItem |
| File manager badges and Share | Finder Sync + Share extension | Shell overlays / Cloud Files API, Explorer verb | Nautilus/Dolphin extensions, `.desktop` actions |
| Open, reveal, play | NSWorkspace, LaunchServices | ShellExecute, `explorer /select,`, App Paths | xdg-open, FileManager1 `ShowItems`, `.desktop` MimeType |
| Pickers, clipboard, launch at login | NSOpenPanel, NSPasteboard, SMAppService | IFileOpenDialog, Win32 clipboard, Run key / StartupTask | xdg-desktop-portal, arboard, XDG autostart |
| App updates | curl installer (Sparkle for Developer ID) | installer/updater of the package | the package manager / AppImage |

**Rust seams** (`cfg`-split modules; nothing outside them names an OS):
- `trs-platform` (new): `dirs` (home, state, downloads, IPC name, sensitive roots, mount roots), `fs` (read/write
  at offset, preallocate, rename without replace, directory sync, volume and file ids, free space, private files,
  metadata copy), `names` (one portable file-name policy), `proc` (shell, process tree, device name, stats,
  utc offset), `signal` (shutdown), `net` (default gateway).
- Existing seams kept: `trs-fswatch` backends (FSEvents, inotify, poll; ReadDirectoryChangesW to add), the
  `trs_sync::fs::Fs` trait (a Windows `StdFs`), `SecretStore`, `Awake`.
- New: a `Trash` seam (freedesktop Trash, Recycle Bin), and a transport seam in `trs-control` (Unix socket, named
  pipe with an owner-only DACL).
- errno numbers are never matched; `io::ErrorKind` is.

**Bindings.** `trs-ffi` stays the one API for every host. It is plain UniFFI (no Swift-specific code); it builds as a
`cdylib` too for C#/Kotlin/Python bindings, or a Rust UI links it as a library.

## 3. Compatibility decisions (fixed before any Windows or Linux device exists)

Devices of every system must agree on these. They are decided here so that no later change breaks a mixed
group of devices.

1. **Wire paths** are NFC, `/`-separated, and `validate_path` only guarantees that a path stays inside the folder.
   It is frozen: whether a name can be stored on *this* disk is the receiver's local decision. A name the local
   disk cannot store (Windows: `\ : * ? " < > |`, control characters, trailing dot or space, `CON`, `NUL`, `COM1`…)
   becomes a per-path issue there ("Not a valid name on Windows"); it is never written under another name, never
   recorded as a delete, and the other devices keep it. One bad entry in a batch is skipped, never the session.
2. **Conflict copies** are named from signed data only, so every device names the same losing version the same way
   and none makes a second copy: the UTC time of the losing version (`2026-10-25 14.32z`; apps show local time)
   and the losing device's name as it signed it, reduced to characters legal on all three systems.
3. **Symlink targets** are `/`-separated and NFC on the wire. A device that cannot make symlinks (Windows without
   the right) records a per-path issue.
4. **The executable bit** is only read where the disk keeps it. On a disk without Unix permissions (NTFS, FAT,
   exFAT, many network shares) the bit is unknown: the device keeps the bit of the version it has and never
   reports a change of it. No wire field.
5. **`.seedwellignore`** has one syntax and one default list on every system (`\` escapes; matching is
   case-sensitive unless `(?i)`; a UTF-8 BOM is ignored). The defaults cover every system's litter (`.DS_Store`,
   `._*`, `Thumbs.db`, `desktop.ini`, `$RECYCLE.BIN`, `.directory`, `.Trash-*`, `.nfs*`…).
6. **Chat text** is signed with LF line ends (CRLF is turned into LF when a message is written).
7. **Device names and clients**: a device without a name uses the computer's name on every system; the client
   string may name the system (`Seedwell 0.2 (Windows)`).
8. **Torrent file names** are made safe for every system on every system (the same mapping everywhere, so a
   torrent lands on the same paths on a Mac, a PC and a Linux box), and names that differ only by case or Unicode
   normalization inside one torrent get distinct names.

## 4. Work

Each step is its own commit with tests; `tools/check.sh` passes after each.

**Phase 0: bugs that hit Macs today** (found by the audit)
- [x] Sync: replacing a file on a disk without hard links (exFAT, FAT, some network shares) fails for ever:
      the archive copy falls back to a copy.
- [x] Sync: exFAT/FAT mounts report every file executable, so every file gets a new version: the exec bit
      becomes "unknown" there (§3.4).
- [x] Sync: the case-clash check uses our folding, not the disk's: a name the disk treats as equal (ß, ligatures)
      is overwritten by the rename. The disk's own answer (`lstat` hits a name that is not in the listing) counts.
- [x] Sync: one bad path in a peer's batch drops the whole batch and the session (§3.1).
- [x] Torrents: two files differing only by case or Unicode normalization become one file on APFS (endless hash
      failures) (§3.8).
- [x] Swift and Rust disagree: the mail header parser and the tag notes (app vs `seedwell mail`): one
      implementation in `trs_control::mail`. The link problem wording (dead Swift fallback), the default ignores of
      the Finder badges and the video extensions are still copied by hand: they go with the sync and Play steps of
      Phase 3.
- [x] Found on the way: a folder-and-channel link's two invitations could expire a second apart.

**Phase 1: compatibility decisions** (§3)
- [ ] (Skip, don't drop: done in Phase 0.) A Windows name check as a per-path issue in the puller.
- [x] Deterministic conflict-copy times: the stamp is UTC with a `z` (`2026-10-25 14.32z`), shown in local time
      by the app; the device name is the loser's own signed profile in the folder's membership (0.1.9), so every
      member uses the same one.
- [ ] Symlink target format; ignore syntax, BOM and the shared default list; CRLF in chat; device name default.
- [ ] Portable torrent names on every system (with a layout version for torrents already on disk).
- [ ] Golden vectors for non-ASCII and hostile names; a Windows profile for the sync simulator's memory file
      system.

**Phase 2: Rust builds and runs on Windows and Linux**
- [ ] `trs-platform` with the modules of §2; every `std::os::unix`, `libc` call and errno match moves there.
- [ ] Linux behaviour: XDG state dir, freedesktop Trash, mount roots, local time offset (`/bin/date -r` means a
      file on GNU: Linux falls back to UTC today).
- [ ] Windows: named-pipe transport, Windows `StdFs`, ReadDirectoryChangesW watcher, Recycle Bin, Job Objects
      for wake commands, `ctrl_c`/close for shutdown.
- [ ] CI: `cargo check`/clippy for `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`, and the Linux test
      matrix extended to sync, sync-net, fswatch, stream, devlog and ffi.
- [ ] `tools/sync-compat.sh` against a daemon on another machine (Mac ↔ Linux ↔ Windows).

**Phase 3: the Mac app becomes a thin host** (Swift logic → Rust view models; Swift keeps drawing)
- [ ] One formatter (`human`): bytes, rates, durations, "ago", counts and plurals, used by the app and the CLI.
- [ ] File tree (window list, menu list) and chooser models; the starting countdown.
- [ ] Menu bar model (sections, caps, signatures, Sharing and Folder Sync lines, icon state).
- [ ] Row model (status lines, rate slot, controls) and list groups; Get Info sections; tracker order and pages.
- [ ] Library window state (polling cadence, section fallbacks, summaries, new-download takeover).
- [ ] Sync view model (SyncText, share, add/edit, Finder status file), chat view model (text, mail, tags, markdown).
- [ ] Notices (which events notify, texts, bursts, click routing), `seedwell://` link routing, launch options.
- [ ] Settings parsing and save policy (ports, rates, times, goals, rules, folders, sync settings).
- [ ] Update checker; Play (player choice, episode labels); context-menu models; the QA control protocol.
- [ ] Each step deletes the Swift copy, so no rule exists twice.

## 5. Status

Started 2026-10-26 from a four-part audit (Swift app, Rust OS-specific code, sync compatibility).
