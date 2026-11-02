# Windows app parity with the Mac app

Audit of 0.1.14 (2026-11-01). The rule is `docs/CROSS-PLATFORM.md` §1: Rust decides, the host draws. The Mac app has
~56k lines of Swift, the Windows app ~5k of C#, with 21 `NEED:` placeholders. **Rust ready?** says whether Windows
only has to draw: **FFI** (data, texts and actions exported), **Data only** (core calls exist; labels, validation or
the view model still in Swift: move them to Rust first), **Swift** (the logic itself is Swift). Placeholders open
`App.ShowPlaceholder` (an empty "Seedwell" window). Sizes: S < 1 day, M 1–3 days, L a week or more.

Work split: the Mac session moves logic into Rust/FFI and merges; the PC's agent writes, builds and screenshots the
C# (bundles in `Seedwell Sync/builds`, patches in `from-pc/`, messages in "Seedwell Development").

## Batches (most visible first, cheapest where the FFI is ready)

1. **Placeholder sweep and quick wins (S–M)** — Rust side ready (2026-11-01): the header, `Core::app_commands`,
   `HostWindow`, `move_failed`/`move_error`, the Agents failure title, `FollowLine.help`, `SidebarCommand::SetFollow`.
   - The "Settings / Settings" header (`library.rs`: Rust ready, the summary is empty; leave an empty one out), the Updates group title.
   - Mute/Unmute, Agents Yes/No, Save as Favorite.
   - Move To… (FolderPicker + `move_to`).
   - Add Torrent File… and Add a Download: tray menu, Ctrl+O / Ctrl+Shift+O, the sheet's Choose File, drag and drop onto the window.
   - ↑/↓/Ctrl+A.
   - "+N more" opens Downloads, and the Sharing line opens Sharing.
   - Pause All, Resume All, Limit Speed (Rust titles).
   - The backlog offer.
   - A command without a window yet is hidden, never a placeholder.
2. **Chat dialogs and links (M)** — Rust side ready (2026-11-01): `chat_dialogs.rs`, `links.rs`.
   - New Channel / Join with a Link, and New Topic (Rust texts and validation first).
   - Rename, Leave, Wake Command…, Delete Conversation….
   - `seedwell://` scheme registration and a Rust `link_route`.
3. **Folder Sync dialogs (M–L)** — Rust side ready (build 38): `sync_page.rs`, `sync_dialogs.rs`, `sync_share.rs`; the
   `HostWindow`s `FileList`, `AddFolder`, `FolderEditor`, `FolderShare`.
   - Add Folder (this PC's folder or join), Edit, Share (roles, uses, days, links, invitations, sharing period).
   - The page buttons, the role menu, Remove from Folder…, conflicts, gone and re-share prompts.
   - "On all my devices".
   - The page model (`sync_page_model`).
4. **Notifications (M)** — Rust side ready (build 42): `notify.rs`, `prompts.rs`. See `docs/build-42-notify-ffi.md`.
   - Windows App notifications from `Notices` (`event` per listener event, `chat` every `chat_poll_ms`), the click
     through `notice_route`.
   - The settings problems banner (`settings_banner`), the add request, location move and backlog prompts.
5. **Settings (L, Rust first):** a Rust settings view model (groups, rows, labels, hints, choices) and parsers (rate, port, time, size, goal), with one generic C# renderer.
   - First: the Cloudflare RendezvousSection (FFI ready) and Agents (permission, activity).
   - Last: the Rules and Trackers editors.
6. **Chat conversation page (L)** — Rust side ready (build 41): `chat_page.rs`, `chat_markdown.rs`; `Core::chat_page_read`,
   `chat_page_model` (toolbar and ⋯ menu, message cards with Markdown runs, mail banners, tags, images and files, the
   members panel, the composer). See `docs/build-41-ffi.md`.
7. **File list, chooser and starting notifier (L, Rust first):** the file list's data (`FileTreeModel`) is FFI, but about
   25 of its texts and its selection and polling rules are still Swift (FileTreeViews.swift, FilesStore.swift): a file-list view
   model first; the chooser and the countdown models move to Rust too.
8. **Tracker Info page and favorite editing (M)** — Rust side ready (build 42): `tracker_page.rs`, `favorite_editor.rs`;
   `Core::tracker_info_read`, `FavoriteEditor` (its `tracker_info_page` and `tracker_info_run`), `tracker_header_model`.
   See `docs/build-42-tracker-play-ffi.md`.
9. **Play (M)** — Rust side ready (build 42): `trs_platform::players` (Windows: App Paths, registered handlers, `PATH`;
   unverified until Windows CI and the PC run it), `player.rs`: `Core::play` → `PlayStart`, `play_offer`, `play_players`,
   `play_with_choices`.
10. **Platform (L–XL):**
    - The key vault: done in Rust (build 42). Without a vault passed, `secret_store` uses DPAPI-sealed files in
      `<state>/sync/vault` (`trs_sync_net::secrets::open`, `trs_platform::secrets::Dpapi`) and copies the 0600 file
      `sync/secrets` into it (each key put, read back and compared; a key the vault holds keeps the vault's value,
      a different plain one is kept aside). The plain file is erased only once all match and this release has
      started fine before (`launch-ok`), so an older build the installer goes back to still has its keys. When DPAPI
      or the copy fails it keeps the plain file first with the vault under it and says so in
      `Core::settings_problems`. DPAPI files rather than the Credential Manager: a credential holds at most 2,560
      bytes and is one list per user, while the files stay in their state folder (a QA run never sees the user's
      keys). The Windows app passes no `SecretVault`. Verified by Windows CI only.
    - An Explorer Share verb.
    - Cloud Files on-demand folders (CfRegisterSyncRoot; `sync_ondemand.rs` is host-neutral) and their states.
    - Overlays.
11. **QA protocol:** `--qa` verbs for every new window, so `qa/run.ps1` covers it.

## Gaps by area

### Library window
| Feature | Windows | Rust ready? | Notes |
|---|---|---|---|
| Sidebar, lists, Remove… question, tracker header | Done | FFI | Screen reader "Online"/"Offline" from Rust missing (LibraryPane.cs:360) |
| Selection keys | Partial | FFI `LibraryKey` | ↑/↓, ⇧↑/⇧↓, Ctrl+A not mapped (LibraryPane.cs:153) |
| Row right-click menu | Partial | FFI `row_commands` | Move To, Choose Files, Custom Goal…, Edit Rules…, Play → placeholder (RowCommands.cs:250) |
| Get Info | Partial | FFI `GetInfoCard` | No chooser, no play offer (InfoCard.cs:36, 296) |
| File list under a row | Placeholder | FFI `FileTreeModel`, `set_file_priorities` | LibraryList.cs:328, LibraryPane.cs:122 |
| Chooser window | Missing | Data only | ChooserModel.swift → Rust first |
| Starting notifier | Missing | Swift | StartingModel.swift → Rust |
| Add a Download sheet / Add Torrent File… / drag and drop | Partial / Missing | FFI `add_source`, `add_link_source`, `Core::app_commands` | LibraryPane.cs:585, 623 |
| Move To…, custom seeding goal | Missing | FFI / Data only | Commands.swift texts |
| Play / Play With | Missing | FFI `play_commands`, `Core::play`, `play_offer` | Discovery in Rust (build 42); start `PlayLaunch::Run` |
| Tracker Info page, favorite editing | Placeholder | FFI `tracker_info_page`, `FavoriteEditor` | LibraryPane.cs:652, 459 |
| Pause All, Resume All, Limit Speed | Missing | FFI `Core::app_commands` | Titles, shortcuts, enabled, check mark |
| Move To… failure | Missing | FFI `move_failed`, `move_error` | |
| Commands without a window | Placeholder | FFI `HostWindow`, `row_commands(…, host)`, `Library::set_host_windows` | Pass the windows Windows has |

### Tray flyout (menu bar)
| Feature | Windows | Rust ready? | Notes |
|---|---|---|---|
| Icon, sections, rows, Folder Sync submenu, updates | Done | FFI | |
| "+N more" / Sharing line select their place | Partial | FFI | FlyoutWindow.xaml.cs:256, 270 |
| Inline file checklist, Play button | Missing | FFI `FileTreeModel.menu_rows` | |
| Folder Sync Edit, Add Folder… | Placeholder | FFI | `sync_menu_model(…, host)` offers them only with `FolderEditor` / `AddFolder` |

### Folder Sync
| Feature | Windows | Rust ready? | Notes |
|---|---|---|---|
| Folder page | Partial | FFI `sync_page_model` | FolderSyncPage.cs:14 draws its own; the model has the buttons, menus and empty state |
| Add Folder, Edit, Share dialogs | Placeholder / Missing | FFI `Core::sync_add_*`, `sync_edit_*`, `sync_share_*` | Drawing only |
| Members' roles, Remove from Folder…, gone / re-share prompts, conflicts, activity | Missing | FFI `sync_page_model` (activity: none yet) | |
| On all my devices | Missing | FFI `sync_follow_line` (with `help`), `sync_set_follow`; the channel menu's `SidebarCommand::SetFollow` | DevicesSection.cs:12 |
| `seedwell://` links | Missing | FFI `link_route`, `Core::route_link`, `command_line_routes` | Register the scheme; route what the command line brings |
| On-demand folders | Missing | FFI host-neutral | Cloud Files API (batch 10) |

### Chats
| Feature | Windows | Rust ready? | Notes |
|---|---|---|---|
| Sidebar (channels, topics, private, unread, agents) | Done | FFI | |
| Conversation page | Placeholder | FFI `Core::chat_page_read`, `chat_page_model` | Markdown as runs (`chat_blocks`); links say Open or Route |
| Mail, tags, members, toolbar | Missing | FFI `ChatPageModel` | Members read with `Core::sync_members` when the panel opens |
| New Channel / Join, New Topic | Placeholder | FFI `Core::chat_new_form`, `chat_new_set_link`, `chat_new_window`, `chat_new_start`; `chat_new_topic_dialog` | |
| Mute, Agents Yes/No | Placeholder | FFI | `chat_agents_menu` (with `failure_title`), `chat_agents_dialog` |
| Rename, Leave, Wake Command, Delete Conversation, Share | Missing / Placeholder | FFI `chat_*_dialog`, `chat_dialog_check` | Share: FFI |

### Settings (Windows has Updates and Devices)
| Mac group | Windows | Rust ready? |
|---|---|---|
| General (Startup, Notifications, Playback, Power); Updates' development rows | Missing / Partial | Data only / FFI |
| Transfers, Folders, Rules and trackers, Network | Missing | Data only (parsers in Swift) |
| Folder Sync (names, port, discovery, location) | Missing | Data only |
| Folder Sync › Cloudflare (Rendezvous) | Missing | FFI (`RendezvousView`, `sync_rendezvous_*`) |
| Agents (allow, permission, activity, notify); command-line tool and skill | Missing | Data only / Swift |
| Advanced (cache, verify, diagnostics, developer mode, tracker log, state folder) | Missing | Data only |
| Problems banner, rejected settings | Missing | FFI `settings_banner` with `settings_problems` (App.xaml.cs:252) |

### Notifications and prompts
| Feature | Windows | Rust ready? |
|---|---|---|
| Finished, error, goal, choice, added, duplicate, agent activity, chat banners, click routing | Missing (Listener.cs stubs) | FFI `Notices`, `notice_route` |
| Add request (watch folder "Ask first"), location move, backlog offer | Missing | FFI `add_request_prompt`, `location_move_prompt`, `location_move_result`, `Core::backlog_prompt` |

### Platform-only
| Mac | Windows | Size |
|---|---|---|
| File Provider on-demand folders | Cloud Files API | L–XL |
| Finder badges | Cloud Files states; overlays | L |
| Share extension, "Show in Seedwell" | Explorer verb | M |
| Keychain | DPAPI-sealed key files in the state folder (Rust, build 42) | Done |
| Player discovery | App Paths / registry (Rust, build 42) | M |
| FSEvents | ReadDirectoryChangesW | Done (`crates/fswatch/src/windows.rs`) |
