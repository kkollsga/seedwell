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
| Power source (`power_changed`; the On battery rule is Rust's), network, sleep/wake | IOPS, NWPathMonitor, NSWorkspace | `RegisterPowerSettingNotification`, `NotifyIpInterfaceChange`, `WM_POWERBROADCAST` | UPower, NetworkManager, logind `PrepareForSleep` |
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
- New: a Trash seam, `fs::move_to_trash` (a given Trash folder; else the Mac's own Trash call (NSFileManager, each volume's
  Trash, Put Back), the Recycle Bin, the freedesktop.org home trash) with `trash!()` for its name in text, and a transport seam in `trs-control` (Unix socket, named pipe
  with an owner-only DACL).
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
- [x] A Windows name check as a per-path issue in the puller (`trs_platform::fs::name_problem`; a folder named
      `dir.` would otherwise come back as `dir`), and the same for symlinks without Developer Mode.
- [x] Deterministic conflict-copy times: the stamp is UTC with a `z` (`2026-10-25 14.32z`), shown in local time
      by the app; the device name is the loser's own signed profile in the folder's membership (0.1.9), so every
      member uses the same one.
- [ ] Symlink target format; ignore syntax, BOM and the shared default list; CRLF in chat; device name default.
- [ ] Portable torrent names on every system (with a layout version for torrents already on disk).
- [ ] Golden vectors for non-ASCII and hostile names; a Windows profile for the sync simulator's memory file
      system.

**Phase 2: Rust builds and runs on Windows and Linux**
- [x] `trs-platform` with the modules of §2 (`fs`, `dirs`, `proc`, `signal`, `net`, `ipc`); the `std::os::unix`, `libc`
      calls and errno matches of storage, state, http, sync, sync-net, engine, devlog, control and cli moved there
      or became `io::ErrorKind` matches. The workspace builds and passes clippy for `x86_64-pc-windows-gnu`.
      Still OS-specific in place: the fswatch backends, `trs-storage`'s mapped allocator (macOS), the sync flush
      barrier (macOS), uTP's socket options; and the app (`trs-ffi` builds for Windows, nothing calls it yet).
- [ ] Linux behaviour: run it there. Done: XDG state dir (`dirs::state_dir`), mount roots, local time offset
      (`localtime_r`, no more `/bin/date`), the freedesktop.org home trash (`fs::move_to_trash`: `files/` and
      `info/*.trashinfo`; only the home trash, so a folder on another volume is refused as on the Mac).
- [ ] Windows: ReadDirectoryChangesW watcher (the poll backend runs meanwhile), the Recycle Bin run on the PC
      (`fs::move_to_trash`: `SHFileOperationW` with undo and no UI; refuses what the bin would delete outright:
      a drive without one, set to delete at once, or an item larger than it holds). Done: named-pipe
      transport with an owner-only DACL (`trs-control::transport`), Windows `StdFs` (file ids and change times
      from the handle, no exec bit), Job Objects for wake commands, Ctrl+C/close/logoff/shutdown.
- [x] The test suite runs on a Windows 11 PC (MSVC), through the OneDrive bridge: from 434 failures at the first
      run to the few listed there. Found on the way and fixed for every system: a lost wake-up on encrypted
      connections, stale pull issues, the archive cap, half copies, temp files nobody finishes, a state folder
      made again, a stale sign of life in the outage check.
- [x] Windows timers: an async sleep under 16 ms takes about 16 ms (the default 15.6 ms tick). Measured: the rate
      limiter and uTP pacing stay within 0.3 % of their targets regardless, so no `timeBeginPeriod` (it costs power).
- [x] UniFFI held at 0.31 (2026-10-28): the Windows app's C# bindings come from uniffi-bindgen-cs, which supports
      0.31 at most, and the Rust side and every generator must share one UniFFI version. 0.32 brought nothing
      Seedwell uses. Dependabot ignores uniffi >= 0.32 until a uniffi-bindgen-cs release supports it; then both move.
- [ ] CI: `cargo check`/clippy for `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`, and the Linux test
      matrix extended to sync, sync-net, fswatch, stream, devlog and ffi.
- [ ] `tools/sync-compat.sh` against a daemon on another machine (Mac ↔ Linux ↔ Windows).

**Phase 3: the Mac app becomes a thin host** (Swift logic → Rust view models; Swift keeps drawing)
- [ ] One formatter (`human`): bytes, rates, durations, "ago", counts and plurals, used by the app and the CLI.
- [ ] File tree (window list, menu list) and chooser models; the starting countdown.
- [x] Menu bar model (sections, caps, signatures, Sharing and Folder Sync lines, icon state): `trs-ffi`'s `menu.rs`
      (`menu_model`, the `Arrivals` and `IconState` objects, `sync_summary`) and `row_text.rs` (the countdown phrase,
      the waiting and retry notes, category labels, the Play check); the Mac's menu and icon only draw them.
- [x] Library window, step 1 (sidebar, title bar, lists): `trs-ffi`'s `library.rs` (the `Library` object: place,
      search, category, folds and pages; its `LibraryViewModel` with the title and summary, the page, the refresh
      cadence and the fallbacks of a place that disappeared; `LocalClock`, the host's time and day styles),
      `library_rows.rs` (the Downloads, Sharing and tracker-tab groups, every row's lines in colour, rate slot,
      controls, category pill and filter chip, the empty notes), `sidebar.rs` (places, chats, private conversations,
      the trackers' order, the footer, the items' context menus) and `chat_text.rs`; the Mac draws them.
- [ ] Get Info sections; a tracker's header and Info page; the Folder Sync and conversation pages.
- [x] Row commands (a row's right-click menu in the window and the menu bar, the selection's menu): `trs-ffi`'s
      `row_commands.rs` (`row_commands`, `play_commands`: titles, order, checks, the red Remove, the shortcut, the
      Seeding Goal, Priority, Category and Rule submenus, Play and Play With with the episode labels and the capable
      players) and `row_text.rs` (held, Ask Tracker Again, may hold video). Each `RowCommand` is run by the host
      (core call, file manager, pasteboard, window, dialog); "Show in Finder" names `trs_platform::file_manager!()`.
- [x] Library window state, step 1b: `trs-ffi`'s `library_window.rs` (the selection with its anchor, ⇧-click ranges, ⌘A,
      ↑/↓ and ⇧↑/⇧↓, and the rows that leave the screen; the Remove… question, where it is drawn and when it goes; the window's keys as
      `LibraryKey` → `KeyOutcome`, with the file-list parts left to the host as `LibraryKeyAction`; the place Get Info
      shows and how it follows the arrows; the new-download takeover, a click in the notifier, and the arrival while the window is closed (`Library`
      lives with the app); the add sheet's texts, what is typed in it and the link check); the Mac maps its keys and
      clicks and draws the model.
- [x] Sync view model: `trs-ffi`'s `sync_text.rs` (folder lines and badges, roles and members, links, invitations and
      the sharing period, the add form and rename check, the Share dialog's choices, the file-manager badge, error and
      alert texts) and `sync_menu.rs` (`sync_menu_model`, the `SyncRemoval` confirmation); the Mac only draws them.
- [ ] Chat view model (text, mail, tags, markdown).
- [ ] Notices (which events notify, texts, bursts, click routing), `seedwell://` link routing, launch options.
- [ ] Settings parsing and save policy (ports, rates, times, goals, rules, folders, sync settings).
- [ ] Update checker; Play's player choice (the setting, Automatic, "needs update"; the episode labels and Play With
      are done with the row commands); the other context-menu models (a file's in the file list); the QA control
      protocol.
- [ ] Each step deletes the Swift copy, so no rule exists twice.

## 5. Status

Started 2026-10-26 from a four-part audit (Swift app, Rust OS-specific code, sync compatibility).
