# Build 70: "Read only" for your own devices (for the Windows app)

Build 70. The user's request: *"In the share menu add a read only toggle."* They share a
folder (a 600 GB folder with files on demand, say) to all their computers so that nothing can ever change the
hosting computer's files, nothing is downloaded to it, and the other computers cannot send files or changes to each
other. SPEC §7G.1 (*On all my devices*, *Read only*), §7G.5 (the Add, Edit and Share dialogs) say what the core does.

The core: the *On all my devices* statement (the group's Follow) has a new flag, `FOLLOW_READ_ONLY` (4). With it on,
an owner of the folder admits the user's other devices at Read only instead of Admin and makes the ones that have the
folder Read only (signed with the folder key), all but the device that turned it on; off again, it gives them back
Admin (not one the user had made Read only before). Members by link keep their link's role. Only an owner or admin of
the folder can switch it, and a sibling made Read only cannot lift it with a statement of its own. A new link from
the Share dialog (and `seedwell sync share` without `--role`) admits at Read only while it is on.

**No protocol change.** A 0.1.18 device keeps the flag and passes it on, but as an owner it still admits siblings as
admins: the owner needs this build for Read only to apply.

## What the Windows host must do

1. **Build with this core** (`app-windows\build.ps1` regenerates the C# bindings). The bindings change:
   - `FollowLine` has a new last field `ReadOnly` (`FollowReadOnly?`): a folder's toggle (`null` for a channel and
     while the line is not shown). `FollowReadOnly` has `Title` ("Read only"), `On`, `Enabled`, `Line`, `Help` and
     `Owner` (this computer is an owner; the host draws nothing from it, the line already says what waits for one).
   - `SyncAddForm` and `SyncEditForm` have a new field `ReadOnly` (`bool`), the form's choice, next to `AllDevices`.
   - `SyncShareWindowModel` has a new field `Devices` (`FollowLine?`), after `Sharing`.
   - Two new async calls: `Core.SyncShareSetDevices(SyncShareForm form, bool on, bool readOnly)` →
     `Task<SyncShareForm>`, and `Core.SyncSetFollowReadOnly(string id, bool readOnly)` → `Task` (the dialogs do not
     need the second one: Start and Save call it in the core).
   No C# code constructs these records (`app-windows` has no `new FollowLine`, `new SyncAddForm`, `new SyncEditForm`
   or `new SyncShareWindowModel`; it only uses `with`), so the app builds unchanged. The toggle is not drawn until
   the steps below are done.

2. **Add Folder and Edit Folder: draw "Read only" under "On all my devices".** `AddFolderDialog.Follow(...)` (used by
   both dialogs) gets a second toggle. Every word, `On` and `Enabled` comes from the model (the core already put the
   form's choice into `follow.ReadOnly.On` and disables it while `AllDevices` is off):
   ```csharp
   internal static StackPanel Follow(FollowLine follow, bool on, bool enabled, Action<bool> set, Action<bool>? setReadOnly = null)
   {
       // ... the toggle and its line as now, then:
       if (follow.ReadOnly is FollowReadOnly ro && setReadOnly is not null)
       {
           var t = new ToggleSwitch { Header = ro.Title, IsOn = ro.On, IsEnabled = ro.Enabled && enabled, Margin = new Thickness(16, 4, 0, 0) };
           Help(t, ro.Help);
           t.Toggled += (_, _) => { if (t.IsOn != ro.On) setReadOnly(t.IsOn); };
           s.Children.Add(t);
           if (ro.Line.Length > 0) s.Children.Add(Note(ro.Line));   // indented like the toggle
       }
       return s;
   }
   ```
   AddFolderDialog: `Follow(follow, form.AllDevices, !starting, on => Change(form with { AllDevices = on }), ro => Change(form with { ReadOnly = ro }))`.
   EditFolderDialog: the same with `!saving`. NewChannelDialog draws its own toggle and needs nothing (a channel's
   `ReadOnly` is `null`). Start and Save need no change: `SyncAddStart` / `SyncEditSave` apply it.

3. **Share dialog: draw `model.Devices`** after the sharing period (a `Rule()` above it), when it is not `null`:
   the "On all my devices" toggle (`Title`, `On`, `Enabled`, `Help`, `Line` under it) and, under it, `Devices.ReadOnly`
   as in step 2. Both apply at once, off the UI thread, like `ShareUntil`:
   ```csharp
   void SetDevices(bool on, bool readOnly) => _ = Task.Run(() => app.Core.SyncShareSetDevices(form, on, readOnly)).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
   {
       AppLog.Write($"share: devices {on} read-only {readOnly} {(t.IsCompletedSuccessfully ? "done" : "failed")}");
       if (t.IsCompletedSuccessfully) form = form with { Role = t.Result.Role };   // only the role: the user may have changed the rest meanwhile
       Draw();
       if (!t.IsCompletedSuccessfully) AddFolderDialog.Failed(failure, Message(t.Exception));
   }));
   ```
   The "On all my devices" toggle calls `SetDevices(isOn, devices.ReadOnly?.On ?? false)`; "Read only" calls
   `SetDevices(devices.On, isOn)`. Disable both while a call runs. The role switch then shows Read only (the core's
   choice); the user may still pick another role for one link.

4. **QA commands** (tool calls only), next to the existing ones in `App.xaml.cs`:
   - `add readonly on|off` → `af.QaReadOnly(on)` = `Change(form with { ReadOnly = on })`;
   - `edit readonly on|off` → `ef.QaReadOnly(on)`, the same;
   - `share devices on|off` and `share readonly on|off` → `sd.QaDevices(on, readOnly)` = `SetDevices(...)` with the
     other value as the model has it.
   Their checks in `app-windows\qa` read the drawn model (the toggle's `On`, `Enabled`, `Line`).

## What to check on the PC

- `app-windows\qa\run.ps1 all -Build -CoreProfile Debug` (the core's own tests cover the roles: `seedwell-sync-net`'s
  `read_only_*` tests, the dialog words in `seedwell-ffi`).
- With the PC paired to the Mac: Edit Folder on a folder of the PC's, *On all my devices* on, *Read only* on, Save;
  the Mac's members list of that folder shows the Mac at Read only, and a file made on the Mac does not reach the PC.
  Read only off: the Mac is an admin again.
