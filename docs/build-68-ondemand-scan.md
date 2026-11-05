# Build 68: on-demand folders listed in seconds, hashed when first fetched (for the Windows app)

Branch `ondemand-listing-first` (build 68), on master `d00f5f0` (*files on demand never moves a holder's files*). The
user's report (0.1.18): a 914 GB folder of 407 films added on demand sat in *Scanning* for 28 minutes while the core
hashed every byte, the other devices' file managers showed an endless spinner, and `ls` of the Mac's own File Provider
location timed out. The user's design: **an on-demand folder's files are listed, not read; a file is hashed only when
a device first downloads it, as part of that download.** SPEC §7G.11 (*Listed first, hashed when first fetched*) and
§7G.2 (*Scanning in steps*, names first and progress) say what the core does; `crates/sync-proto/PROTOCOL.md`
(*Unhashed files*) has the wire.

**Every device must be updated together.** The protocol is 3 and only 3 (`PROTOCOL = MIN_PROTOCOL = 3`, the user's
decision): a 0.1.18 device and a build-68 device refuse each other at the Hello, and the folder shows *"… runs a newer
Seedwell that this one cannot sync with: update this PC"* (or *older … update it*) instead of syncing. Install build
68 on the PC before testing it against the Mac.

**Review fixes (after the first build 68 bundle).** The last piece of a stream now carries only the maker's
signature, not the whole chunk list, so a device with the first build-68 core and one with these fixes cannot stream
to each other (the session closes on the frame it cannot read). Take the PC's core from the same commit as the Mac's.
Nothing else changes for the host: a fetch over a slow link may pause on one piece for up to about 9 minutes (the
host's `SyncFetch` loop keeps reporting progress every second, so Cloud Files never cancels it), and a fetch cut off
goes on from where it stopped instead of from the first byte.

## What the Windows host must do

1. **Build with this core** (`app-windows\build.ps1`, which regenerates the C# bindings from trs-ffi). The bindings
   change: `SyncFolder` has a new field `Scan` (`SyncScan?`, last, with a default), and there are two new types,
   `SyncScan` and `SyncScanPhase`. No C# code constructs a `SyncFolder` (checked: `app-windows` has no
   `new SyncFolder`), so **no C# source changes**: the folder rows already draw `f.Subtitle` and `f.Badge`
   (`FolderSyncPage.cs`), which now carry the scan's words and ring fraction.
2. **No call changes.** Everything the host calls keeps its name, arguments and promises: `SyncAddStart` (the Add
   dialog: with *Files on demand* on it now marks the folder before it opens, in Rust), `SyncChildren`, `SyncItem`,
   `SyncChanges`, `SyncHostAnchor`/`SyncSetHostAnchor`, `SyncFetch`, `SyncHydrated`, `SyncLocalChange`, the snapshot.
3. **Run the QA checks that touch this** with the full run before the batch goes back, as always:
   `app-windows\qa\run.ps1 all -Build -CoreProfile Debug` (bash: `powershell.exe -NoProfile -ExecutionPolicy Bypass
   -File app-windows/qa/run.ps1 all -Build -CoreProfile Debug`). The ones that exercise the new paths are `folders`
   (the Add and Edit dialogs' *Files on demand*: the folder stays full here, marked for the others),
   `ondemand` (`--free-space`: this PC's files go into its cache and are recorded unhashed; reading `a.txt` fills its
   placeholder from the cache, and `SyncHydrated` then compares the placeholder byte for byte with the cache's copy
   before deleting that copy, since an unhashed version has no hashes), `ondemandfetch`, `ondemandchanges` and
   `ondemandwrites`. A failure there is a bug in this build: report it with the check's output.
4. **Confirm on the PC, with two devices on build 68** (no code to write; these are the promises the core tests make
   on the Mac, which the PC's Explorer must show):
   - **A just-joined folder lists at once, empty, and fills by itself.** Join a folder on demand whose owner has not
     sent its list (the owner quit right after the invitation, or still starting). Explorer opens the folder at once,
     empty, with no spinner and no *"Working on it…"*; the flyout's grey line says *"Waiting for Kristian's Mac mini
     to list Movies"* (Rust's words, `SyncFolder.Waiting`, drawn as `Subtitle` already). When the owner sends its list,
     the change feed moves (`FeedMoved`), `CatchUp` places the placeholders and they appear without a refresh. The
     sync root is registered `CF_POPULATION_POLICY_ALWAYS_FULL` and the provider creates every placeholder itself from
     the feed, so Explorer never waits on a population callback. If Explorer shows a spinner on the root: `Connect`
     must not wait for anything but `CfConnectSyncRoot`; `Walk` and `CatchUp` are calls that answer at once (empty).
   - **No call waits for a scan or a stream.** While the Mac scans a big folder, or the PC streams a big file from it,
     the flyout, `SyncChildren` and Explorer answer at once. If anything stalls, that is a core bug: report the call
     and its time from `AppLog`.
   - **A first download works with progress.** Opening a file the Mac has not hashed yet: `SyncFetch` reports `Done`
     as each 8 MiB piece arrives (the host already reports progress every second it waits, so Cloud Files' 60 s
     cancel never fires), and the placeholder is filled as before. A second device fetching the same file afterwards
     gets it from either holder, chunk by chunk.
   Commands (bash, as an agent runs them; a person types the same in PowerShell): on the Mac
   `seedwell sync add ~/Movies --on-demand` and `seedwell sync share Movies`; on the PC
   `seedwell sync join <link> --on-demand`, then `seedwell sync status Movies` (the Mac's 914 GB must list within
   seconds) and `seedwell sync fetch Movies "<a film>.mkv"` (progress, then the file; the Mac's status shows no
   scan meanwhile).

## What changed in the core (nothing to do on Windows unless point 1 to 4 above says so)

1. **Adding a folder with *Files on demand* reads nothing.** Since d00f5f0 such a folder stays full on the device that
   holds its files (at its path, nothing moved), marked on demand for the others (`on_demand_others`). Its scans now
   list, as an on-demand folder's do: every file it finds is recorded as an *unhashed* version (path, kind, size,
   time, permissions, no chunk list). The Add dialog (`sync_add_start`) and `seedwell sync add <path> --on-demand`
   mark the folder before it opens (`SyncHandle::add_folder_with`), so its first scan reads no file; marking a folder
   later (the Edit dialog, `sync mode <folder> on-demand`) reaches a scan already under way at its next step, and the
   big files it has not read yet are listed instead. The same holds for a folder whose copies are freed
   (`--free-space`) or joined on demand. A host's own writes (`sync_create_item`, `sync_modify_item` with contents)
   are hashed as before. Nothing is hashed in the background. 914 GB lists on every device in about a second.
2. **The first download hashes the file.** A device that needs an unhashed file's content (Explorer opening it, a pin,
   a full device) asks its maker for it in 8 MiB pieces, in order; the maker reads each piece once, hashing it as it
   goes, and the last piece brings the version signed with its hashes. The downloading device checks every chunk as it
   arrives and records the hashed version; later downloads, from the maker or from that copy, are the ordinary chunk by
   chunk transfer. For the host nothing changes: `sync_fetch` works as before (progress moves with each piece), and the
   item a fetch returns carries the hashed content version, which `SyncHydrated` is called with.
3. **The maker's own items do not change** when another device downloads them: their content version stays, so the
   host is never asked to re-download, dehydrate or replace a placeholder because of it.
4. **A file changed while it streams** is never recorded: the maker answers *changed*, the download fails and is tried
   again in the next round (after the maker's scan has found the new version). A changed file is unhashed again until
   someone downloads it. A stream cut off in the middle (the session closed) starts again from the first byte.
5. **Kept in place (a folder whose copies were freed on the PC):** its files are this PC's own unhashed versions. When
   the host fills a placeholder from the cache and calls `sync_hydrated`, the core checks the placeholder byte for byte
   against the cache's copy (there are no hashes yet) before it deletes that copy; the call and its errors are as
   before. An unhashed item's `content_version` names the version itself (two unhashed versions of one size differ),
   and it does not change when another device downloads the file. A placeholder whose size, time or file id moved
   since it was filled is the user's change (`sync_local_change` says *modified*), as today.
6. **Real progress while a folder scans.** `SyncFolder.scan` (phase `listing` or `checkingContents`, files and bytes
   seen, files and bytes to read, read so far). `sync_folder_subtitle` says it ("Listing · 1234 files (56 GB)",
   "Checking contents · 3 of 407 files · 457 of 914 GB") and `sync_folder_badge` gives the ring its fraction.
7. **A member waiting for its owner's list** says so: a folder whose connected members have not sent their first index
   yet has `waiting` = "Waiting for Kristian's Mac mini to list Movies", shown as its grey line while it is otherwise up
   to date.
8. **Full folders (not marked): names first.** A full folder's scan walks the whole tree before it reads its big files
   (4 MiB and more), so directories and small files reach the other devices in the first seconds and the reading that
   follows knows its total. What is recorded is unchanged (every file is still hashed before it is announced).

## Tests on the Mac (each names its failure)

- trs-sync: `lazy::an_on_demand_folders_scan_reads_no_file` (both modes),
  `lazy::a_folder_marked_on_demand_while_its_first_scan_walks_reads_none_of_its_big_files`,
  `lazy::the_first_download_reads_the_file_once_and_its_copy_serves_it_by_those_hashes`,
  `lazy::a_file_changed_while_it_streams_is_never_recorded_mixed`, and the unit tests in `src/lazy.rs`.
- trs-sync-net (timed, in `tools/timing-tests.txt`): `lazy::a_big_on_demand_folder_is_listed_on_a_peer_within_seconds_of_being_added`
  (64 GiB: 1.6 s), `lazy::listings_and_status_answer_at_once_while_a_file_is_hashed_for_another_device`,
  `lazy::listings_and_status_answer_at_once_while_a_scan_holds_the_folder`,
  `lazy::a_member_whose_owner_has_not_listed_the_folder_yet_answers_at_once_and_says_whom_it_waits_for`; and
  `lazy::a_file_not_hashed_yet_is_fetched_verified_and_its_copy_then_serves_it_by_its_hashes`,
  `versions::a_device_of_0_1_18_is_refused_and_told_to_update`.
- trs-ffi (over the real socket, as the File Provider extension asks):
  `sync_host::tests::a_member_lists_at_once_while_its_owner_has_sent_nothing_and_is_signalled_when_it_does`.

## Recommendation for full folders that are not marked (not done)

A full folder could record its big files unhashed too, so a joining device sees the whole tree at once. That changes
what a full folder announces (every new big file would first stream from its maker), so it is left for a decision; the
names-first order above already gives the tree's directories and small files in the first seconds.
