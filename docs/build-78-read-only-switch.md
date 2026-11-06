# Build 78: Read only on its own switch, and admins remove users (for the Windows app)

Build 78 (branch `readonly-flag`, 0.1.20). *Read only* is no longer part of *On all my devices*: it is the folder's
own setting, which only an owner of the folder signs (SPEC §7G.1 *Read only*), so it is drawn as a switch of its own,
for every folder of this computer, paired or not. An owner whose PC is paired with nothing has it too (before, it was
hidden with *On all my devices*). Everywhere else it shows the state and who can switch it. Separately, an admin may
now ask the folder's owner to remove a user (a member at Read & write or Read only): its PC cuts the user off at once
and the owner's device carries the removal out by itself (SPEC §7G.1 *Removing a device*); the member menus say so.

**Rust decides, the host draws.** Every word, every state and whether the switch may be used come from the core.

## What changes in the bindings

Rebuild with this core (`app-windows\build.ps1` regenerates the C# bindings). The changes:

- **Record `FollowReadOnly` is now `ReadOnlyLine`**, with the same fields: `Title`, `On`, `Enabled` (this PC may switch
  it: an owner of the folder), `Line`, `Help`, `Owner`.
- **`FollowLine.ReadOnly` is gone.** `FollowLine` is only "On all my devices" now.
- **New fields, each `ReadOnlyLine?`:**
  - `SyncAddWindowModel.ReadOnly`: a folder of this PC (From this PC): always there, enabled; `On` is the form's
    choice (`SyncAddForm.ReadOnly`, as before). `null` for Shared with me.
  - `SyncEditWindowModel.ReadOnly`: every folder; `On` is the form's choice (`SyncEditForm.ReadOnly`); disabled on
    anyone but an owner (Save never changes it there).
  - `SyncEditForm.ReadOnlyLine` (new field, `ReadOnlyLine?`): the switch as the core has it, filled by
    `SyncEditForm(id)`; keep it as it comes, like `Follow`.
  - `SyncShareWindowModel.ReadOnly`: a synced folder this PC may share (not a channel, not a folder not synced yet),
    as it is now; switching applies at once.
- **Calls:**

  | before | now |
  |---|---|
  | `SyncShareSetDevices(form, on, readOnly)` | `SyncShareSetDevices(form, on)`: "On all my devices" only |
  | (none) | `SyncShareSetReadOnly(form, on)`: Read only, at once; returns the form with the next link's role (Read only once it comes on, Read & write again when it goes off); throws `CoreException.Failed` with the words to show for anyone but an owner |
  | `SyncSetFollowReadOnly(id, readOnly)` | `SyncSetReadOnly(id, on)` (and its async wrapper) |
  | (none) | `SyncReadOnlyLine(id)`: `ReadOnlyLine?`, the switch as it is (`null`: not a folder synced here) |

## What the Windows host must do

1. **Add Folder and Edit** (`AddFolderDialog.cs`, `EditFolderDialog.cs`): draw `model.ReadOnly` as its own
   `ToggleSwitch` (header `Title`, tooltip `Help`, `IsEnabled = Enabled && !busy`, the `Line` as a note under it)
   below the "On all my devices" block, not inside it and not indented (the Mac draws it with the same leading edge as
   "On all my devices", separated by the sheet's divider in Edit). It is drawn whether or not `model.Follow` is there.
   Toggling it changes `form with { ReadOnly = on }` as today. `AddFolderDialog.Follow` loses its `setReadOnly`
   parameter and the nested Read only block.
2. **Share dialog** (`ShareDialog.cs`): `model.Devices` draws "On all my devices" alone, switching it calls
   `SyncShareSetDevices(form, on)`. `model.ReadOnly`, when there, is its own switch after it (a rule between them, as
   between the dialog's sections), switching it calls `SyncShareSetReadOnly(form, on)` off the UI thread and keeps only
   the returned `Role`, as `SetDevices` does now. Both are disabled while either call runs.
3. **Members** (folder page and channel members panel): nothing to change in the host: the menus come from the core.
   An admin now gets a role menu with "Remove from Folder…" (or "Remove from Channel") alone on each user (Read & write,
   Read only), with its own question ("The folder's owner removes it, as soon as its device hears from this PC…");
   owners' and admins' rows stay plain text for it. The menu calls `SyncRemoveDevice` as for an owner. `SyncMember`
   gains `Removal` (`string?`, already part of the member's line from `SyncMemberLine`: "Removal asked: waiting for
   the owner", or why an owner's device refused) and `RemovalWaiting` (`bool`, the item is disabled meanwhile).
4. **QA** (`App.xaml.cs`, `qa\run.ps1`):
   - `QaFollow` prints Read only from the dialog model's `ReadOnly`, not `FollowLine.ReadOnly`;
     `share readonly on|off` calls the new `SetReadOnly`, `share devices on|off` only the devices switch.
   - `$checks.readonly` describes the old design (B's role lowered and given back): since 0.1.20 Read only changes no
     role. What it should check now: A (owner) turns Read only on in the Edit dialog (paired or not), B (paired, admin)
     shows the switch disabled with "Read only: only the folder's owner changes it…", `seedwell sync read-only Docs
     status --json` on B says `"read_only":true,"may_switch":false,"held":true`, a file B writes does not reach A,
     the Share dialog's next link starts at Read only, and once A turns it off B's file reaches A and B's role is the
     same as before (`sync members`).
   - An unpaired owner's Add Folder shows Read only (a new render: Add Folder on a PC paired with nothing).

### The channel members panel's removal line

An admin's removal of a channel member now has a line in the members panel, as the folder page's member row has.
`ChatMemberRow` gains **`Removal`** (`string?`): "Removal asked: waiting for the owner" while the request waits, "The
owner did not remove it: …" once an owner's device refused it, `null` otherwise. It is the same text as
`SyncMember.Removal`, from the same source. Draw it under the member's name in the panel (`ChatPage.cs`), in the
caption size and the secondary colour, as the folder page draws a member's line; the Mac does the same. The waiting
state needs nothing more: "Remove from Channel" in the row's menu is already disabled meanwhile.

The folder and channel status no longer wait for a device this PC cut off pending its removal: "Waiting for …"
(`SyncFolder.Waiting`, `SyncChannel.Waiting`) leaves it out until an owner's device refuses the request. Nothing to
change in the host.

## Renders to compare with the Mac

`sync-add-local`, `sync-edit` and `sync-share` (light and dark) from the Mac's `--render-snapshots`: the Read only switch
sits under "On all my devices" with the same leading edge, its line in the secondary colour under it.
