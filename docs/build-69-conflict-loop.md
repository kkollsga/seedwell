# Build 69: one conflict, one copy (the Windows conflict loop of 0.1.18)

Branch `fix/ondemand-conflict-loop`, on master `c2f7f919`. The Windows agent's report (0.1.18, also without its QA
hook): in an on-demand folder kept in place, a file changed on this PC whose item moved on meanwhile (a program's write
landing between `sync_local_change` answering Stale and the host's update; or simply a file edited on both devices
while the PC was off) made a conflict copy at every look of the host — 4 in the first second, then one every 10 s on
every device, without end — and the file at the name stayed different on the two devices.

## The cause (in the core)

1. `sync_local_change` compared a changed placeholder only with the item's current version. After `sync_modify_item`
   wrote the user's edit as a conflict copy (the base had moved on), nothing told it that the placeholder's content
   was now recorded, as the copy: every look answered `Modified { base: <the earlier version> }` again.
2. `sync_modify_item` made a copy for every report: `host_target` only compares the base, and picks the next free
   name (`… 2`, `… 3`), so each repeat of the same content was one more full-size copy, synced to every device.
3. Nothing named the item in the host's feed again, so the host's `Update` (the only path that dehydrates a stale
   placeholder) never ran for it: the placeholder kept the user's bytes, and the name held two versions for good.

## What the core does now

The rule (SPEC §7G.11, unchanged, now written out): **the version that reached this device first stays at the name on
every device; the edit made from an earlier version is the conflict copy on every device.** One content, one copy:

- `sync_modify_item` from an earlier version whose content is the item's current content: no conflict, no new
  version, the item is returned. Whose content a conflict copy of the item already holds: that copy is returned and
  nothing is written. However often a host reports it.
- `sync_local_change`, for a placeholder whose edit started from an earlier version and whose content is that of a
  conflict copy of the item held here: **Stale** (never Modified again). The note follows the file, so the next look
  reads nothing.
- The conflict copy's record names the original item in the host's feed again (`Batch::shown`), so the host's next
  `CatchUp` places the item, asks `sync_local_change` (Stale), lets go of the placeholder and fills it again (the item
  is Downloaded: the version at the name is in the cache).

Tests: `crates/sync-net/tests/it/ondemand.rs`
`a_write_meeting_another_devices_version_in_place_makes_one_conflict_copy_and_settles` (the host's call order: the
write between Stale and the update, the four looks of the first second, 30 s of looks and feed pages; one copy, the
same content at the name on both devices, nothing more in the feed) and
`the_same_content_reported_again_makes_no_second_copy_and_the_current_content_no_copy` (a host that reports the same
edit every look). Both fail on master. `crates/sync/tests/it/ondemand.rs`
`the_copy_a_hosts_edit_was_written_as_is_found_by_its_content` covers long names, whose copies are cut to fit.

## What the Windows host must change (CloudHost.cs, the PC's)

No call or binding changes. The core now answers **Stale** for a placeholder that is **not in sync** (the user wrote
to it, and its content is now the conflict copy's), and Cloud Files refuses `CfUpdatePlaceholder(… DEHYDRATE …)` on
such a file with `0x80070179` (`ERROR_CLOUD_FILE_NOT_IN_SYNC`): the protection that kept the write safe in the first
place. So, in `Update`:

1. **Read the file's USN before `SyncLocalChange`** (`FSCTL_READ_FILE_USN_DATA` on a handle opened with
   `FILE_READ_ATTRIBUTES`; the `Usn` of the returned `USN_RECORD`).
2. **On Stale, dehydrate with `CF_UPDATE_FLAG_DEHYDRATE | CF_UPDATE_FLAG_MARK_IN_SYNC`** (`UpdateDehydrate |
   UpdateMarkInSync`), **passing that USN as `UpdateUsn`**, so the update happens only if nothing wrote to the file
   since the core looked. Without the USN guard, a write in that window would be thrown away (the bug's own window).
3. **A refused update is not in sync.** Today `Place` sets the in-sync state from the item's state after `Update`
   even when the update was refused, which marks a file holding unreported bytes as in sync. After a refused update
   (any `hr < 0`, a USN mismatch included), leave the in-sync state as it is and `LookAt` the item again (its next
   answer is Modified for a new write, Stale otherwise, and the feed or the retry brings it back to `Update`).
4. **On a conflict in `Report`** (`changed.Id != item`) nothing more is needed: the feed names the item again at
   once. If the host lets go of the placeholder there itself (the PC's plan: dehydrate with the item's metadata,
   refill if wanted), ask `SyncLocalChange` first and do it only on Stale, with the same USN guard as above.

QA for the PC: `QaWriteDuringUpdate` on a filled file while another device's version arrives must end with one
conflict copy on each device, the other device's version at the name on both, and no `kept as` line after the first.
A write made inside the USN window (a second `qaDuringUpdate` write after `SyncLocalChange`) must make the update fail
and be reported, never lost.
