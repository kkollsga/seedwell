# Build 60: temp and junk files, and saves by replace, in on-demand folders (for the Windows app)

Branch `ondemand-ignore-replace`, from master `074bdc5` (after 0.1.18). The two things the Windows agent asked after step 4, decided
in Rust. SPEC §7G.2 (*Not synced*) and §7G.11 (*Names not synced*, *Save by replace*) say the same.

## What changed in the core

### 1. Names the folder does not sync

`sync_create_item`, and a rename or move through `sync_modify_item`, follow the folder's ignore rules exactly as a full
folder's scanner does: the versioned built-in list, the folder's `.seedwellignore` (read again first if it changed) and
this device's own patterns. The host keeps no list of its own.

- **A new file or folder at such a name** (`~$Report.docx`, `~WRD0000.tmp`, `Thumbs.db`, `desktop.ini`, anything a
  `.seedwellignore` line matches): fails with `CoreException.OnDemand` of kind **`SyncErrorKind.Ignored`**. Nothing is
  recorded, nothing is copied, nothing goes to other devices. The message is the reason for the log, e.g.
  `“~$Report.docx” is not synced: it matches “~$*” in Seedwell's built-in list`.
  The same for new content written at such a name (`sync_modify_item` with `contents`).
- **A rename or move of an item onto such a name** (Word renames the document aside to `~WRL0001.tmp`):
  - its content here (in its placeholder; the core checks): the item **leaves the folder** at once, deleted on every
    device as a full folder sees a file renamed so, and fails with **`Ignored`**: the file at the new name is now an
    ordinary file of this PC.
  - its content not here (online only): **nothing changes**, and it fails with **`IgnoredNotHere`** (the file would
    stay on this PC only, so it must be downloaded first).
  - The name checked is the one the rename lands at (on a disk that folds case, the folders above it as recorded), in
    the same step as the rename itself, so `Ignored` from `sync_modify_item` always means the item left. An ordinary
    rename that would still land on a name not synced fails as an ordinary refusal (`CoreException`, not `OnDemand`
    `Ignored`): refuse it as any failed rename.
- `Core.SyncIgnored(folderId, path, isDir)` → `SyncIgnored?` (`Reason`, `Deletable`): whether a path is ignored and
  whether its line is deletable (`(?d)`, built-in OS metadata). Never waits for the folder's core.
- Built-in list version 3 adds **`~*.tmp`** (Word's and many Windows programs' save temp files, as Dropbox leaves
  them out; not deletable). Not every `*.tmp`: people name their own files so, and a built-in line makes existing files
  in every folder newly ignored. A folder that wants all `*.tmp` puts it in its `.seedwellignore`, and on-demand
  follows that too.
- Newly ignored (patterns changed): an item already there stays an item, announced invalid, as in a full folder.

### 2. Save by replace

A **new file (or folder) at the path of an item of its kind that this device deleted through the host within 5 s**
(`REPLACE_WINDOW`) is that item, changed: `sync_create_item` returns **the deleted item's id**, with the new content;
its version follows the deleted one, so the history goes on and no conflict copy is made. "Deleted" covers both
`sync_delete_item` and a rename onto an ignored name (above).

- **The delete is announced late.** For 5 s after a host delete (or until the new file came) the folder's index
  sessions (before every batch) and the host's change feed wait (`FeedMoved` is not called meanwhile). So other devices
  see one edit, never a delete and a new file, and the host's `CatchUp` never sees the delete of an item that comes
  back. Each wait lasts one window at most: a run of deletes, one every few seconds, delays `FeedMoved` by 5 s, never
  longer. A delete with nothing after it reaches other devices about 5 s later than before.
- Why 5 s: programs do the whole dance within a fraction of a second; the host adds its own lag (the watcher, the
  queue, `Adopt`'s check every 0.5 s that the new file is closed). A new file at that name within 5 s that is not a
  save is taken as an edit too, which costs nothing (versions go by path).
- **A rename of another item over that name** (a temp file of an ordinary name, made and adopted first, then renamed
  over the deleted document): the renamed item keeps *its own* id (a host cannot change an item's id in a rename), and
  its version follows the delete, so other devices see an edit of that name.
- Outside the window, at another name, or of another kind: a new item, as before.

## What the Windows host must do (`app-windows/CloudHost.cs`)

Nothing in the UniFFI interface was removed or renamed. New: the two `SyncErrorKind` values, `SyncIgnored`,
`Core.SyncIgnored`. `Status()` already maps unknown kinds to `StatusCloudFileUnsuccessful`, so it compiles as it is,
but these steps are needed; 4 and 5 fix data-loss races that exist today, independent of this build.

1. **`Adopt`**: catch `CoreException.OnDemand` with `kind == SyncErrorKind.Ignored` around `SyncCreateItem`: leave the
   file or folder as it is (a plain file: no `Convert`, no `SyncHydrated`), log `on-demand: {rel} not synced ({message})`,
   do not retry, and do not adopt what is inside an ignored folder (later events inside it are ignored again by the
   core; that is fine).
2. **`OnRename`**, `SyncModifyItem` failing with:
   - `Ignored`: **ack success** (the rename must happen, or Word's save fails), `Unmark` the item and everything placed
     below it at once (so no feed step touches the old path, where Word's new file is about to appear), log the
     message, and queue: once the rename is done, take the placeholder(s) at the new path back to plain files
     (`CfRevertPlaceholder`; they are hydrated, the core checked). Do not call `SyncDeleteItem` for it: the core deleted
     the item already.
   - `IgnoredNotHere`: **refuse** (`StatusCloudFileUnsuccessful` is fine), log it, and ask
     `notices.SyncOndemandNotice(root.Id, kind, path)`: the core has a notice for this kind ("“x” wasn't renamed …
     Download it first"); post it.
3. **The renamed-aside file still carries the item's identity** until it is reverted, and Word deletes it within
   milliseconds. In **`OnDelete`, `OnRename` and `OnClose`**: when the callback's path
   (`Marshal.PtrToStringUni(info.NormalizedPath)`: the full path without the drive, so compare it with the root's path
   without its drive, case-insensitively) is not where the item is placed (`root.PathOf(item)`, which after step 2's `Unmark` is not a path), it is
   not that item's: ack success and do not call the core. Without this, Word's delete of `~WRL0001.tmp` deletes the
   saved document (the item is the saved document again by then).
4. **`Remove(root, id)`**: delete only a placeholder whose identity is `id` (`CfGetPlaceholderInfo`, or at least
   `CloudFiles.IsPlaceholder`; `CfGetPlaceholderInfo` needs a binding in `CloudFiles.cs`). A plain file at that path is the user's new file not adopted yet. Today, a delete then
   a new file at the same name lets `CatchUp` delete the new file if it runs before `Adopt`.
5. **Folders the feed deletes** (`Remove` of a directory): delete the placeholders below it, not plain files. For each
   plain file left, ask `app.Core.SyncIgnored(root.Id, rel, isDir)`: `Deletable` → delete it; otherwise keep it, and the
   folder with it (as a full folder keeps a folder that holds ignored files that are not deletable: Office owner files,
   `~*.tmp`, `.seedwellignore` lines without `(?d)`).
6. Recommended: in `Adopt`, a file at a path the host deleted or renamed away a moment ago need not wait for `Closed()`
   (a save by replace writes the temp file completely before the rename); every wait counts against the 5 s.

Nothing changes for the host when a file is not a temp file: `sync_create_item` simply returns an existing id now and
then, which `Adopt` already handles (`Convert(full, item.Id)`, `Mark`).

## What the Mac does (no Swift change)

The File Provider server is Rust (`crates/ffi/src/sync_host.rs`): `Ignored` and `IgnoredNotHere` answer the
extension with `excluded` (`NSFileProviderError.excludedFromSync`). For a create the system keeps the file and stops
asking. For a rename onto an ignored name of a file the system has not reported on disk, the server does not ask the
core: excluded, the system downloads it and then deletes the item (`deleteItem`). A **folder** renamed onto an
ignored name is excluded only when every file below it is on the system's disk; otherwise the rename is refused
(`failed`, "download it first"), because the system deletes an excluded folder with whatever it did not download. When
the system later deletes the item it excluded, as it saw it then (`base` = the old content version), while the saved
document holds the name again (its version follows the delete the file left by), the core answers it done and deletes
nothing, also when the saved document has the same content (a save that changed nothing, an editor's `name~`). The
feed signal waits for the save, as on Windows, one window at most. Not checked against a real File Provider and Word
yet.

## The `ondemandchanges` QA check could assert

- Existing steps: `Remove-Item "$bd\del.txt"` and `Remove-Item -Recurse "$bd\dd"` now reach A about 5 s later (both
  wait 15 s: fine).
- **Temp and junk files** on B: `'owner' > "$bd\~`$edit.txt"`, `'t' > "$bd\~WRD0001.tmp"`, `"$bd\Thumbs.db"`,
  `"$bd\desktop.ini"`, and a `.seedwellignore`-matched name (write `*.bak` into `$docs\.seedwellignore` on A first,
  wait for it on B, then `"$bd\x.bak"`): each stays a plain file on B (QaStatus lists no placeholder for it, the
  attribute check shows no `RecallOnDataAccess`/in-sync state), never appears on A, and the log has
  `not synced (… matches …)` once for each.
- **Save by replace, Word's way**, on a filled `edit.txt` (B): note A's `edit.txt` and the item id (from `sync items`
  or the QA status); then in one go `Rename-Item "$bd\edit.txt" '~WRL0001.tmp'`;
  `'saved by replace' | Set-Content -NoNewline "$bd\~WRD0000.tmp"`; `Rename-Item "$bd\~WRD0000.tmp" 'edit.txt'`;
  `Remove-Item "$bd\~WRL0001.tmp"`. Assert: A's `edit.txt` reads `saved by replace` within 15 s; a watcher job on A that
  polls `Test-Path "$docs\edit.txt"` every few ms never saw it missing; no `conflict` file on A or B; B's `edit.txt` is
  a placeholder in sync with the **same item id** as before; no `~WRL0001.tmp`/`~WRD0000.tmp` on A or B afterwards.
- **Save by replace, delete then create**: `Remove-Item "$bd\new.txt"`; `'recreated' | Set-Content -NoNewline
  "$bd\new.txt"` at once: same id, A never misses `new.txt`, reads `recreated`.
- **Save by replace with nothing changed**: as Word's way above, but write the same text back: same id, A never
  misses the file, its text unchanged, nothing deleted on A after 10 s.
- **Not a save**: `Remove-Item "$bd\renamed.txt"`, wait 7 s, recreate it: a new item id (and A deleted it in between).
- **Online-only file renamed to an ignored name**: `Rename-Item "$bd\farther.txt" 'farther.tmp'` on a dehydrated
  placeholder fails (refused), the file stays `farther.txt`, A unchanged, and a notice "“farther.txt” wasn't renamed"
  was posted.

## Tests (Mac, in-process)

- `seedwell-sync` `ondemand::a_new_file_where_the_host_just_deleted_one_is_that_item_changed`,
  `a_new_file_elsewhere_or_later_is_a_new_item`, `a_rename_to_a_name_not_synced_takes_the_item_out_of_the_folder`,
  `a_temp_file_renamed_over_a_just_deleted_name_continues_its_history`,
  `a_save_by_replace_with_the_same_bytes_is_never_taken_for_a_delete`, `a_large_tree_deleted_through_the_host_is_quick`
  (20 000 files), `a_rename_onto_a_name_not_synced_is_checked_as_it_lands` (a disk that folds case);
  `ignore::tests::why_names_the_deciding_line`.
- `seedwell-sync-net` `ondemand::a_save_by_replace_reaches_the_other_device_as_one_edit` (a full peer's file polled every
  2 ms is never missing; it fails without the announcement wait),
  `words_save_and_a_plain_delete_reach_the_other_device_as_they_should`,
  `a_delete_made_while_a_session_announces_still_waits_for_the_save` (a session already at its batches).
- `seedwell-ffi` `a_save_by_replace_through_the_app_keeps_the_item_and_leaves_temp_files_alone` (the Windows host's calls
  in its order, through `Core`), `sync_host` `a_save_by_replace_through_the_extension_keeps_the_item` (the Mac
  extension's requests over the socket, with a second save of the same bytes and a plain delete whose feed signal comes
  after the window), `a_rename_to_a_name_not_synced_waits_for_the_systems_download`,
  `a_folder_renamed_to_a_name_not_synced_needs_its_files_on_disk`, `a_run_of_deletes_never_keeps_the_feed_from_the_system`.
