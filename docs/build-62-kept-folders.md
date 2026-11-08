# Build 62: folders another device deleted that the host kept (for the Windows app)

Branch `build-62-kept-folders`, from master `0267086`. The Windows agent found it: in an on-demand folder, another
device deletes a folder that still holds a file this PC does not sync and may not delete (`~keep.tmp` under the
built-in `~*.tmp`, no `(?d)`). The host does what build 60 asked (placeholders go, that file and the folder stay, the
folder a plain folder again), but at the next start its sweep adopted the plain folder with `sync_create_item`, and
the folder came back, empty, on every other device. SPEC §7G.11 (*Kept folders*) says what the core does now.

## What changed in the core

1. **`sync_create_item` for a folder** (`is_dir: true`), in a folder kept in place, fails with
   `CoreException.OnDemand` of kind **`SyncErrorKind.Ignored`** when
   - the record at that path is **another device's delete of a folder**,
   - the folder is **the one that was there when the delete was made**: created before the second of the delete (the
     deleting device's version counter, a time; the creation time is the system's, read by the core), and
   - the folder on the PC's disk (the core reads the sync root, nothing is passed) holds **no synced file at any
     depth**: only names the folder does not sync, or nothing. A folder below it counts as part of the kept folder
     when the same holds for it; any other folder below it is new, and takes the folder up.

   Nothing is recorded, nothing is sent, the delete stays the record. The message is the reason, for the log:
   `kept: "docs/old/~keep.tmp" is ignored on this device; a (?d) in front of its line in .seedwellignore lets it go with the folder`
   (the full folder's words), or, when nothing in it is kept by a line (empty, or only deletable files),
   `"docs/old" was deleted on another device and holds nothing that is synced; it stays on this device only`.
2. **The issue.** With a kept file, the folder's status shows that issue at the folder's path, the same one a full
   folder shows, once however often the folder is offered (each start's sweep offers it again). It goes when the
   folder is taken up (3.), when a pull makes it live again (another device made it anew), when the folder is gone
   from the PC's disk, and when the file it names no longer keeps it (Word closed its `~$` file, or its line got a
   `(?d)`). The disk is looked at off the caller's thread: on settling, and at most every 2 s while a snapshot shows
   the issue (the snapshot itself never touches the disk, so the status may show it one snapshot longer). Only the
   kept issue goes, never another issue at that path.
3. **A synced file in it later.** `sync_create_item` for the folder then succeeds (it holds a synced file): the folder
   is recorded again, its version following the delete, and the issue goes. A file written below a folder that has no
   record (`sync_create_item` of the file first) records the folder too, as before.
4. **A file adopted twice.** `sync_create_item` of a file that is its own placeholder, filled with the item's content
   (`sync_hydrated` was called) and unchanged since, returns that item as it is: no conflict copy, nothing written.
   `Adopt` does this today when a file appears in a plain folder without an id: it adopts the folder, whose adoption
   adopts the file, then goes on to create the file itself.
5. **Not kept:** a folder made after the delete (Explorer's empty "New folder" at a name another device deleted
   earlier) is new: recorded, also empty, its version following the delete. So is one whose creation time the system
   does not give. (NTFS may give a folder made again within 15 s of removing one at that name the old creation time,
   "tunneling": such a folder counts as the kept one until a file is put in it. The QA step below waits past that.) A kept folder, by contrast, stays out also once empty (its temp file gone), with no issue. A folder
   at a path with no record (new), or where this PC deleted it itself, holding only ignored files, is a new folder,
   recorded empty, as a full folder's scan records it. A folder kept in the Mac's cache is
   unchanged too (its folder is the system's, which the core does not see).

Nothing in the UniFFI interface changed (only doc comments): no new kind, no new call.

## What the Windows host must do (`app-windows/CloudHost.cs`)

**Nothing** (also after the follow-up fixes: the creation time and the issue's end are the core's). `Adopt` already does the right thing with `Ignored` from `SyncCreateItem`: it logs
`on-demand: docs/old not synced (kept: …)`, leaves the folder plain and does not adopt what is in it. `RemoveFolder`
(build 60's step 5) stays as it is.

Recommended, not needed: in `Adopt`, right after `if (FolderId(root, ParentOf(rel)) is null) Adopt(root, Full(root, ParentOf(rel)));`,
add `if (root.IdAt(rel) is not null) return;` (the folder's adoption took this file already). Without it the core
answers the second `SyncCreateItem` with the same item (4. above), and `CloudFiles.Convert` is called on a file that
is a placeholder already, which may log a `convert` error once; nothing else happens.

Unchanged, for the record: the kept folder stays the PC's plain folder when its ignored files are gone later (Word
deletes its `~$` owner file when the document closes); a full folder would delete it then. It is never sent, and its
issue goes. Moving a
placeholder into the kept folder is refused as a move into any folder without an id is today.

## The `ondemandchanges` QA check could assert

On B (the PC, in place), with A full:

- Prepare: A makes `docs\kept\k.txt`; on B wait for the placeholder, then `'w' > "$bd\kept\~keep.tmp"` (a plain file;
  QaStatus lists no placeholder for it).
- A deletes `docs\kept` (whole folder). On B within 15 s: `kept\k.txt` gone, `kept\~keep.tmp` still there, `kept` a
  plain folder (no placeholder, not in QaStatus's items).
- **Restart B's app** (the sweep runs at start). Then, after 15 s: A has no `docs\kept` (`Test-Path` false, polled a
  few times over those 15 s); B's log has `on-demand: kept not synced (kept: "kept/~keep.tmp" is ignored on this device; …)`;
  B's folder status lists that issue for `kept` once. Restart B again: still no `kept` on A, still one issue.
- `'n' > "$bd\kept\new.txt"`: within 15 s A has `docs\kept\new.txt` reading `n`, and no `~keep.tmp` and no conflict
  file in `docs\kept` on A or B; on B `kept` and `new.txt` are placeholders in sync, `~keep.tmp` plain; the issue is
  gone from B's status. (With the recommended line above, B's log has no `convert` error for `new.txt`.)
- A new folder nobody deleted: `mkdir "$bd\fresh"; 'x' > "$bd\fresh\~x.tmp"`: A has an empty `docs\fresh` within 15 s.
- **The issue's end**: with the kept folder's issue showing, `Remove-Item "$bd\kept\~keep.tmp"`: within 10 s B's
  status no longer lists it, and A still has no `docs\kept` (also after a restart of B).
- **A new empty folder at a name deleted earlier**: A makes `docs\gone\g.txt`; B waits for it; A deletes `docs\gone`;
  B's `gone` disappears. Wait 20 s (past NTFS's 15 s tunneling), then on B `mkdir "$bd\gone"` (empty, as Explorer's "New folder" renamed): within
  15 s A has an empty `docs\gone`, and B's `gone` is a placeholder in sync.

## Tests (Mac, in-process)

- `seedwell-sync-net` `ondemand::a_folder_deleted_elsewhere_that_the_host_kept_stays_out_until_a_synced_file_comes`: the
  host's steps in its order (placeholders filled, the temp file in a folder below, A's delete, the placeholders
  removed, a restart, the sweep's offer twice, a later change that reaches A while the kept folder never does, a
  synced file adopted with its folder and then again, the folder below still kept with its issue, a new folder and a
  re-made one of this device's own, the folder below made anew by A, whose issue goes, then deleted for real, whose
  issue does not come back). It fails on master (the folder is recorded), without the second-adoption rule (a
  conflict copy), and with the issue pruned only in the snapshot's copy (a stale issue).
- `seedwell-sync-net` `ondemand::a_kept_folder_stays_out_empty_and_a_new_one_at_a_deleted_name_is_synced`: the issue goes
  once the temp file does (no offer from the host), the emptied kept folder still stays out, and an empty folder made
  later at a deleted name reaches A. Fails before the follow-up fixes (the issue stays; the new folder is `Ignored`).
- `seedwell-sync-net` `shared::tests::a_kept_folders_issue_is_one_and_goes_alone`: another issue at the same path stays.
