# Build 71: an edit the scan recorded first is no conflict, and no placeholder holding the only copy is let go of

Branch `fix-unhashed-conflicts`, on master `54375f0e` (*Windows: a file the core calls stale is let go of safely*).
The Windows agent's report (master only, not in 0.1.18), in an on-demand folder kept in place (Cloud Files):

- `run.ps1 ondemandwrites`, every time: B (the PC, on demand) is stopped, its filled `one.txt` is overwritten, B
  starts. B's log says `one.txt changed here and on another device: kept as one (conflict, ...).txt`, then
  `one.txt not filled (hydrate: 0x80070185)`, and reading `one.txt` on B fails ("The cloud operation was
  unsuccessful"), while A already has the edit at the name.
- `ondemandstale` (edits on both devices while B is down): two identical conflict copies where one is expected, and
  once B's edit at the name on A while the file read as empty on B.

## The cause (in the core)

Two of the day's changes met:

1. **Lazy hashing (build 68).** B's core scans at start before its host looks. The placeholder it reads the file from
   (held, `crates/sync-net/src/placed.rs`) has changed, so the scan records the edit as B's own new version, and an
   on-demand folder's scan records it **unhashed** (no chunk list). It announces it, which is how A got the edit.
2. **The conflict-loop fix (build 69).** The host then reports the same edit from the version it started from (the
   note's, the version the placeholder was filled with). `od_write` treats the item's current content as no conflict
   only when `holds_content` finds it in the host's file, and `holds_content` is always false for an unhashed
   version (no hashes to check by). So B's own edit was a false conflict: a copy.
3. The host's next look (`od_local_change`) then found the placeholder's content in that copy and answered **Stale**,
   setting the note to not held. But the version at the name was B's own unhashed version, whose only bytes on B were
   in that very placeholder. The host dehydrated it, the refill had no source (0x80070185), and the name was
   unreadable on B.
4. `ondemandstale` is the same gap from the other side. B's scan records its edit unhashed and meets A's: the core
   makes a conflict copy itself, before the host looks (unhashed too when B's edit lost). The host's report of B's
   edit then looked for a copy holding it, found none (the copy is unhashed: `holds_content` false), and made a
   second copy. When B's edit won, the host's look made a copy of the version at the name, answered Stale for the
   placeholder that held it, and the refill had no source (the empty or unreadable file).

## The rule (SPEC §7G.11, *One edit, one version, whichever path sees it first*)

- **The scan is not held back for placeholders; the later path takes the earlier one's record.** The scan must see a
  changed placeholder: the core serves that file to other devices, and must never send changed bytes as the version
  it recorded before. It is also the only path for an edit the host never saw (made while the app was not running, a
  notification missed). Holding it back for placed items would mean hiding the file's content from the core until the
  host reports, and a host that never reports would leave the core serving the wrong bytes. So both paths stay, and
  one edit is one version:
- **An unhashed version is held by a file** (`placed::holds_version`) when the file this device has its bytes in (the
  cache's copy, or the held placeholder: the folder's `PlaceFs`) still has the size, time and file id it was recorded
  with, and holds the same bytes (`same_bytes`), or is that placeholder itself. A hashed version is checked by its
  hashes as before. `od_write` uses it for "the current content is no conflict" and for the copies, and
  `od_local_change` for "the same content as recorded" and for the copies. So the host's report of an edit the scan
  recorded first returns the item's path with no new version, `sync_local_change` answers Unchanged for it (and the
  note follows the file), and a copy recorded unhashed is found by its content: one copy per real conflict.
- **While the core reads the version at the name from the placeholder (held), a change to it starts from that
  version** (`Modified { base }` is the current content version), whatever this device recorded of it since. So a
  further edit after the scan's record is an edit at the name, never a conflict with the device's own version.
- **Never Stale for a placeholder that holds the only copy here of the version at the name** (held). The copy search
  no longer runs for it, so it can never be answered Stale.

The conflict loop stays fixed: one content is one copy, and a placeholder whose content is a copy held elsewhere is
still Stale (not held). The build 69 tests run unchanged.

## Tests (crates/sync-net/tests/it/ondemand.rs, each fails on master)

- `an_edit_made_while_stopped_that_the_scan_records_first_is_no_conflict_and_stays_readable`: `ondemandwrites`. B
  stopped, its placeholder overwritten, B started; the test waits for B's scan to record the edit and for A to have it
  (the Windows order), then the host's report from the old base, then the host's start and 30 rounds of its looks and
  feed. No copy on either device, no new version from the report, Unchanged, the placeholder never let go of, readable
  on B, A has the edit. On master: the report returns a conflict copy.
- `an_edit_on_both_devices_while_one_was_stopped_makes_one_copy_readable_everywhere`: `ondemandstale`, in both orders
  (A's edit later, so B's edit is the core's unhashed copy; B's edit later, so the placeholder holds the version at the
  name). Exactly one copy, the later edit at the name on both, the name and the copy readable on both. On master: two
  copies (A's edit later), and the name unreadable on B (B's later: the refill has no source).
- `an_edit_of_a_version_the_scan_recorded_from_the_placeholder_is_never_let_go_of`: after a settled conflict, an edit
  the scan records, then the copy's content put back at the name. On master the core answers Stale (taken for the
  copy) and the refill fails; now it is the version at the name, on A too, with one copy still.
- The build 69 tests (the hook case `a_write_meeting_another_devices_version_in_place_makes_one_conflict_copy_and_settles`
  and the rest) still give one copy and settle.

## What the Windows host needs (CloudHost.cs, the PC's)

**Nothing for this fix**: no call, binding or behaviour change. The answers the host gets are the same kinds; only
when they come changes. An overwritten file the scan recorded first is now Unchanged at `Overwritten`'s first look
(no `Report`, no `SyncHydrated`: the core already reads the version from that file, which `Convert` makes a
placeholder again), and Stale never comes for a held placeholder, so `Update`'s dehydrate only ever lets go of content
the core has elsewhere.

The patch on master (`54375f0e`) is right as it stands: (last write, size) noted before `SyncLocalChange`,
`CfSetInSyncState(1)`, the open check and the stamp again, then `CfUpdatePlaceholder(DEHYDRATE)`. A write after the
mark clears it and Windows refuses; one before it moves the stamp. One small hardening I would add:

- **Unmark after a refused dehydrate.** When `CfUpdatePlaceholder(DEHYDRATE)` itself fails (`hr < 0`, for instance a
  handle opened between the open check and the update), the file stays marked in sync from the step before, while
  it holds bytes other than the item's. Nothing is lost (Stale means the core holds those bytes elsewhere, as the
  copy), and the retry (`Place` again a second later, 30 at most) usually gets it, but after the last retry Explorer
  shows a file as in sync that differs from its item. Set it back as the guard's own refusal does:
  `if (dehydrate && hr < 0) SetInSync(full, false, item.Path);`.

QA on the PC, with this core: `app-windows\qa\run.ps1 ondemandwrites ondemandstale ondemand ondemandchanges -Build
-CoreProfile Debug` (bash: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File app-windows/qa/run.ps1
ondemandwrites ondemandstale ondemand ondemandchanges -Build -CoreProfile Debug`), then the full `all` run before the
batch goes back. `ondemandwrites` must pass the restart step with no `kept as` line in B's log and `one.txt` readable;
`ondemandstale` must end with one copy.
