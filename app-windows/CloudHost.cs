using System.Runtime.InteropServices;
using System.Text;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// On-demand folders on Windows (item 6, SPEC 7G.11, build 55's notes): the core's in-process host
/// (<c>sync_host_attach</c>) over Cloud Files. The core says which folders are on demand (<c>domains</c>), when a feed
/// moved, what each item is and holds, what a read fetches and what a removal needs; this registers a sync root per
/// folder (kept in place: <c>sync_set_in_place</c>), keeps its placeholders as the feed says (created, moved, updated,
/// deleted; in-sync state and pin from the item), and fills a placeholder on a read (FETCH_DATA: <c>sync_fetch</c> with
/// progress, the file transferred from the core's staging copy, then <c>sync_hydrated</c>). Every call into the core
/// blocks: they run on the core's and the system's threads, never the UI thread. Explorer's Always keep on this device and
/// Free up space reach the core through an attribute watcher (sync_pin, sync_evict), and the system's own dehydration
/// (Storage Sense) is allowed only for what the core frees. The user's own changes reach the core too: a rename or move
/// and a delete before they happen (NOTIFY_RENAME, NOTIFY_DELETE: the answer lets them happen or not), an edit of a
/// filled file when it is closed (sync_local_change), and anything new in the folder through the watcher
/// (sync_create_item, then made a placeholder).
/// </summary>
sealed class CloudHost : SyncHostListener
{
    const int Chunk = 4 << 20;
    static readonly Guid ProviderId = new("5eed0e11-0000-4000-8000-000000000002");

    readonly App app;
    readonly Notices notices;
    readonly object gate = new();
    readonly Dictionary<string, Root> roots = [];
    // Kept alive while any root is connected: the system calls through them.
    readonly CloudFiles.Callback fetch, cancel, dehydrate, rename, delete, close;

    sealed class Root(string id, string path)
    {
        public string Id { get; } = id;
        public string Path { get; } = path;
        public long Key { get; set; }
        /// <summary>Item id → its path in the folder ('/' between names), as last placed.</summary>
        public Dictionary<string, string> Placed { get; } = [];
        /// <summary>The other way: a path in the folder → its item id.</summary>
        public Dictionary<string, string> At { get; } = new(StringComparer.OrdinalIgnoreCase);
        /// <summary>One feed at a time per root, in order.</summary>
        public Task Work { get; set; } = Task.CompletedTask;
        /// <summary>The user's menu: attribute changes under the root (Always keep on this device, Free up space).</summary>
        public FileSystemWatcher? Watcher { get; set; }
        /// <summary>Path → the pin flags last acted on, so each choice of the user's is acted on once.</summary>
        public Dictionary<string, uint> Seen { get; } = new(StringComparer.OrdinalIgnoreCase);
        /// <summary>Item ids being filled because they are kept here (one fill each).</summary>
        public HashSet<string> Filling { get; } = [];

        /// <summary>The files looked at again a moment later (one wait per file, however many events come).</summary>
        public HashSet<string> Waiting { get; } = [];
        /// <summary>Paths the system was allowed to free (NOTIFY_DEHYDRATE), and when: the mark it leaves is not the user's choice.</summary>
        public Dictionary<string, DateTime> SystemAsked { get; } = new(StringComparer.OrdinalIgnoreCase);

        /// <summary>The root's path without its drive, in its full form: what a callback's path starts with.</summary>
        public string Normal { get; set; } = "";

        // Paths the user deleted or renamed away a moment ago: a new file there is a save by replace, looked at soon.
        readonly Dictionary<string, long> left = new(StringComparer.OrdinalIgnoreCase);

        public void NoteLeft(string path)
        {
            lock (left) left[path] = Environment.TickCount64;
        }

        public bool JustLeft(string path)
        {
            lock (left) return left.TryGetValue(path, out var at) && Environment.TickCount64 - at < 5000;
        }

        /// <summary>A folder or file the user moved: it and what is below it are placed at the new path from now on.</summary>
        public void Moved(string from, string to)
        {
            lock (Placed)
            {
                foreach (var (id, path) in Placed.ToArray())
                {
                    if (!path.Equals(from, StringComparison.OrdinalIgnoreCase) && !path.StartsWith(from + "/", StringComparison.OrdinalIgnoreCase)) continue;
                    At.Remove(path);
                    var moved = to + path[from.Length..];
                    Placed[id] = moved;
                    At[moved] = id;
                }
            }
        }

        /// <summary>An item and what is placed below it are no longer placed (gone from the folder).</summary>
        public void UnmarkTree(string id)
        {
            lock (Placed)
            {
                if (!Placed.TryGetValue(id, out var path)) return;
                foreach (var below in IdsBelow(path)) Unmark(below, out _);
                Unmark(id, out _);
            }
        }

        // Written on the feed's queue only; the system's callbacks read through PathOf (under the same lock).
        public void Mark(string id, string path)
        {
            lock (Placed)
            {
                if (Placed.TryGetValue(id, out var before)) At.Remove(before);
                Placed[id] = path;
                At[path] = id;
            }
        }

        /// <summary>An item's path as placed (its id when it is not placed), from any thread.</summary>
        public string PathOf(string id)
        {
            lock (Placed) return Placed.TryGetValue(id, out var path) ? path : id;
        }

        public int Count
        {
            get { lock (Placed) return Placed.Count; }
        }

        /// <summary>The item placed at a path, or null; from any thread.</summary>
        public string? IdAt(string path)
        {
            lock (Placed) return At.TryGetValue(path, out var id) ? id : null;
        }

        /// <summary>The ids placed below a folder's path (not the folder itself).</summary>
        public string[] IdsBelow(string path)
        {
            lock (Placed) return [.. Placed.Where(p => p.Value.StartsWith(path + "/", StringComparison.OrdinalIgnoreCase)).Select(p => p.Key)];
        }

        // Items the host itself is moving or deleting: their NOTIFY_RENAME / NOTIFY_DELETE are the host's, not the user's.
        readonly Dictionary<string, int> own = [];

        public IDisposable Own(params string[] ids)
        {
            lock (own) foreach (var id in ids) own[id] = own.GetValueOrDefault(id) + 1;
            return new Release(() =>
            {
                lock (own) foreach (var id in ids) if (--own[id] == 0) own.Remove(id);
            });
        }

        public bool IsOwn(string id)
        {
            lock (own) return own.ContainsKey(id);
        }

        sealed class Release(Action release) : IDisposable
        {
            public void Dispose() => release();
        }

        public bool Unmark(string id, out string path)
        {
            lock (Placed)
            {
                if (!Placed.Remove(id, out path!)) return false;
                At.Remove(path);
                return true;
            }
        }
    }

    public CloudHost(App app)
    {
        this.app = app;
        notices = new Notices(app.Core);
        fetch = OnFetch;
        cancel = OnCancel;
        dehydrate = OnDehydrate;
        rename = OnRename;
        delete = OnDelete;
        close = OnClose;
    }

    /// <summary>At start: the core hosts its on-demand folders here (in an isolated run, only those inside its state folder).</summary>
    internal void Start()
    {
        try
        {
            app.Core.SyncHostAttach(this);
            AppLog.Write("on-demand folders: hosted here (Cloud Files)");
        }
        catch (Exception e)
        {
            AppLog.Write($"on-demand folders: not hosted ({Words(e)})");
        }
    }

    /// <summary>At quit: the roots stay registered (their placeholders stay); the connections go.</summary>
    internal void Stop()
    {
        try { app.Core.SyncHostDetach(); } catch { }
        lock (gate)
        {
            foreach (var r in roots.Values)
            {
                r.Watcher?.Dispose();
                if (r.Key != 0) CloudFiles.CfDisconnectSyncRoot(r.Key);
            }
            roots.Clear();
        }
    }

    // ---------------------------------------------------------------------------------------- the core's calls

    public void Domains(SyncHostDomain[] domains)
    {
        AppLog.Write($"on-demand folders: {domains.Length} ({string.Join(", ", domains.Select(d => d.Name))})");
        var folders = app.Core.SyncSnapshot().Folders;
        foreach (var d in domains)
        {
            if (folders.FirstOrDefault(f => f.Id == d.Id) is not SyncFolder f)
            {
                AppLog.Write($"on-demand folder {d.Name}: not in the snapshot");
                continue;
            }
            Root root;
            lock (gate)
            {
                if (roots.ContainsKey(d.Id)) continue;
                root = new Root(d.Id, f.Path);
                roots[d.Id] = root;
            }
            Queue(root, () => Connect(root));
        }
    }

    public void FeedMoved(string folderId)
    {
        Root? root;
        lock (gate) roots.TryGetValue(folderId, out root);
        if (root is not null) Queue(root, () => CatchUp(root));
    }

    /// <summary>
    /// Before a removal or a switch to full: filled placeholders become plain files (they keep their content), dehydrated
    /// ones are deleted, the root is disconnected and unregistered. The core does the rest.
    /// </summary>
    public SyncHostRemoval RemoveDomain(string folderId)
    {
        Root? root;
        lock (gate) { roots.TryGetValue(folderId, out root); roots.Remove(folderId); }
        if (root is null) return new SyncHostRemoval(null);
        try
        {
            root.Work.Wait(TimeSpan.FromSeconds(30));
            root.Watcher?.Dispose();
            if (root.Key != 0) CloudFiles.CfDisconnectSyncRoot(root.Key);
            foreach (var file in Directory.EnumerateFiles(root.Path, "*", SearchOption.AllDirectories))
            {
                if (((uint)File.GetAttributes(file) & RecallOnDataAccess) != 0)
                {
                    File.Delete(file);
                }
                else
                {
                    using var h = CloudFiles.Open(file);
                    CloudFiles.CfRevertPlaceholder(h, 0, IntPtr.Zero);
                }
            }
            foreach (var dir in Directory.EnumerateDirectories(root.Path, "*", SearchOption.AllDirectories).Reverse())
            {
                using var h = CloudFiles.Open(dir);
                CloudFiles.CfRevertPlaceholder(h, 0, IntPtr.Zero);
            }
            CloudFiles.Check(CloudFiles.CfUnregisterSyncRoot(root.Path), "unregister");
            AppLog.Write($"on-demand folder released: {Short(folderId)}");
            return new SyncHostRemoval(null);
        }
        catch (Exception e)
        {
            AppLog.Write($"on-demand folder not released: {e.Message}");
            return new SyncHostRemoval(e.Message);
        }
    }

    public void Notice(string title, string text) => app.Dispatch(() => app.Notify(title, text));

    /// <summary>
    /// Seedwell's entry in File Explorer's navigation pane (the core's SyncNavPane), at start and on each change: written
    /// under its CLSID, or removed when Show is false.
    /// </summary>
    public void NavPane(SyncNavPane entry)
    {
        AppLog.Write($"navigation pane entry: {(entry.Show ? entry.Target : "none")}");
        try
        {
            NavPaneEntry.Apply(entry, Environment.ProcessPath ?? Path.Combine(AppContext.BaseDirectory, "Seedwell.exe"));
        }
        catch (Exception e)
        {
            AppLog.Write($"navigation pane entry failed: {e.Message}");
        }
    }

    /// <summary>Never asked on Windows (no extension process: the host is in this app).</summary>
    public bool VerifyPeer(byte[] auditToken) => false;

    // ------------------------------------------------------------------------------------------- the roots

    const uint RecallOnDataAccess = 0x00400000;

    static void Queue(Root root, Action work)
    {
        lock (root)
        {
            root.Work = root.Work.ContinueWith(_ =>
            {
                try { work(); }
                catch (Exception e) { AppLog.Write($"on-demand {Short(root.Id)}: {Words(e)} ({e.StackTrace?.Split('\n')[0].Trim()})"); }
            }, TaskScheduler.Default);
        }
    }

    /// <summary>Kept in place, registered (identity: the folder's id), connected; what is placed is read back; the feed caught up.</summary>
    void Connect(Root root)
    {
        app.Core.SyncSetInPlace(root.Id, true);
        Directory.CreateDirectory(root.Path);
        root.Normal = CloudFiles.WithoutVolume(root.Path);
        var identity = Encoding.UTF8.GetBytes(root.Id);
        var h = GCHandle.Alloc(identity, GCHandleType.Pinned);
        try
        {
            var reg = new CloudFiles.SyncRegistration
            {
                StructSize = (uint)Marshal.SizeOf<CloudFiles.SyncRegistration>(),
                ProviderName = "Seedwell",
                ProviderVersion = "1",
                SyncRootIdentity = h.AddrOfPinnedObject(),
                SyncRootIdentityLength = (uint)identity.Length,
                ProviderId = ProviderId,
            };
            var policies = new CloudFiles.SyncPolicies
            {
                StructSize = (uint)Marshal.SizeOf<CloudFiles.SyncPolicies>(),
                HydrationPrimary = CloudFiles.HydrationFull,
                PopulationPrimary = CloudFiles.PopulationAlwaysFull,
                InSync = CloudFiles.InSyncTrackAll,
            };
            // An earlier run's registration is updated, not refused.
            CloudFiles.Check(CloudFiles.CfRegisterSyncRoot(root.Path, ref reg, ref policies, CloudFiles.RegisterUpdate), "register");
        }
        finally { h.Free(); }
        var table = new[]
        {
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.FetchData, Callback = Marshal.GetFunctionPointerForDelegate(fetch) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.CancelFetchData, Callback = Marshal.GetFunctionPointerForDelegate(cancel) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.NotifyDehydrate, Callback = Marshal.GetFunctionPointerForDelegate(dehydrate) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.NotifyRename, Callback = Marshal.GetFunctionPointerForDelegate(rename) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.NotifyDelete, Callback = Marshal.GetFunctionPointerForDelegate(delete) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.NotifyFileCloseCompletion, Callback = Marshal.GetFunctionPointerForDelegate(close) },
            new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.None, Callback = IntPtr.Zero },
        };
        CloudFiles.Check(CloudFiles.CfConnectSyncRoot(root.Path, table, IntPtr.Zero, CloudFiles.ConnectRequireFullPath, out var key), "connect");
        root.Key = key;
        AppLog.Write($"on-demand folder connected: {Short(root.Id)} at {root.Path}");
        // What earlier runs placed (the core's tree as it is), so a deletion or a move from the feed finds its file.
        Walk(root, "root");
        CatchUp(root);
        Watch(root);
        // What the user put in the folder while the app was not running (not a placeholder): the core takes it.
        Sweep(root, root.Path);
    }

    /// <summary>Adopts what is new in a folder, and looks inside the folders already placed.</summary>
    void Sweep(Root root, string folder)
    {
        foreach (var entry in Directory.EnumerateFileSystemEntries(folder))
        {
            if (RelativeTo(root, entry) is string rel && root.IdAt(rel) is string id)
            {
                if (Directory.Exists(entry)) Sweep(root, entry);
                // Saved to while the app was not running: no notification told of it.
                else LookAt(root, id);
            }
            else
            {
                Adopt(root, entry);
            }
        }
    }

    void Walk(Root root, string parent)
    {
        string? cursor = null;
        do
        {
            var page = app.Core.SyncChildren(root.Id, parent, cursor, 1000);
            foreach (var item in page.Items)
            {
                if (File.Exists(Full(root, item.Path)) || Directory.Exists(Full(root, item.Path))) root.Mark(item.Id, item.Path);
                if (item.Kind == SyncItemKind.Dir) Walk(root, item.Id);
            }
            cursor = page.Next;
        } while (cursor is not null);
    }

    static string Full(Root root, string path) => Path.Combine(root.Path, path.Replace('/', '\\'));

    /// <summary>The feed from the anchor the core keeps, a page at a time: placeholders as the items say, then the anchor.</summary>
    void CatchUp(Root root)
    {
        ulong anchor = app.Core.SyncHostAnchor(root.Id) ?? 0;
        while (true)
        {
            SyncChangePage page;
            try
            {
                page = app.Core.SyncChanges(root.Id, anchor, 1000);
            }
            catch (CoreException.OnDemand e) when (e.kind == SyncErrorKind.AnchorExpired)
            {
                // The folder's index was made anew: list everything from 0 (placeholders no longer listed stay until a
                // later change names them).
                anchor = 0;
                continue;
            }
            foreach (var item in page.Items) Place(root, item);
            foreach (var id in page.Deleted) Remove(root, id);
            app.Core.SyncSetHostAnchor(root.Id, page.Anchor);
            anchor = page.Anchor;
            if (!page.More) break;
        }
    }

    /// <summary>One item: its placeholder created, moved or updated; its in-sync state and pin as the item says.</summary>
    void Place(Root root, SyncItem item, int refusals = 0)
    {
        if (item.Id == "root") return;
        var full = Full(root, item.Path);
        var dir = item.Kind == SyncItemKind.Dir;
        if (root.Placed.TryGetValue(item.Id, out var before) && before != item.Path && (File.Exists(Full(root, before)) || Directory.Exists(Full(root, before))))
        {
            // Renamed or moved elsewhere (another device): the placeholder moves with it (the host's own move: its
            // NOTIFY_RENAME is not the user's).
            Directory.CreateDirectory(Path.GetDirectoryName(full)!);
            using (root.Own(item.Id))
            {
                if (dir) Directory.Move(Full(root, before), full); else File.Move(Full(root, before), full);
            }
        }
        var refill = false;
        if (!File.Exists(full) && !Directory.Exists(full))
        {
            Create(Path.GetDirectoryName(full)!, Path.GetFileName(full), item, dir);
        }
        else
        {
            refill = Update(root, full, item, dir, out var refused);
            if (refused)
            {
                // Its in-sync state stays: the file may hold what the core has not seen. A new write is reported now, and
                // the item as it is then placed again.
                root.Mark(item.Id, item.Path);
                LookAt(root, item.Id);
                if (refusals < 30) Later(root, full, 1000, () => { if (app.Core.SyncItem(root.Id, item.Id) is SyncItem now) Place(root, now, refusals + 1); });
                return;
            }
        }
        root.Mark(item.Id, item.Path);
        // Overwritten and still being written: nothing to set on a plain file; it becomes a placeholder when it closes.
        if (!CloudFiles.IsPlaceholder(full)) return;
        using var h = CloudFiles.Open(full);
        // In sync: online only, or here as the core has it; not: on its way, only here, unavailable.
        var inSync = item.State is SyncItemState.Downloaded or SyncItemState.OnlineOnly;
        Refused(CloudFiles.CfSetInSyncState(h, inSync ? 1u : 0u, 0, IntPtr.Zero), "in-sync state", item.Path);
        // The pin as the core has it, set only when it differs (each change is an event for the watcher); the user's
        // "Free up space" mark stays on a file the core does not keep.
        var a = (uint)File.GetAttributes(full);
        if (item.Pinned != ((a & PinnedAttribute) != 0))
        {
            Refused(CloudFiles.CfSetPinState(h, item.Pinned ? CloudFiles.PinState.Pinned : CloudFiles.PinState.Unspecified, 0, IntPtr.Zero), "pin", item.Path);
        }
        // Kept here, or an earlier version just let go of that is wanted here: filled (through FETCH_DATA, as a read would).
        if (!dir && (item.Pinned || refill) && (a & RecallOnDataAccess) != 0) Fill(root, item.Id, full);
    }

    /// <summary>A call on a placeholder the system refused: said in the log, as the placeholder now differs from the item.</summary>
    static void Refused(int hr, string what, string path)
    {
        if (hr < 0) AppLog.Write($"on-demand: {path}: {what} refused (0x{hr:X8})");
    }

    /// <summary>Fills a placeholder kept on this PC, off the feed's queue (a fetch can take long); one fill per item.</summary>
    void Fill(Root root, string id, string full)
    {
        lock (root.Filling) if (!root.Filling.Add(id)) return;
        Task.Run(() =>
        {
            try
            {
                using var h = CloudFiles.OpenForData(full);
                CloudFiles.Check(CloudFiles.CfHydratePlaceholder(h, 0, -1, 0, IntPtr.Zero), "hydrate");
            }
            catch (Exception e) { AppLog.Write($"on-demand: {Path.GetFileName(full)} not filled ({e.Message})"); }
            finally { lock (root.Filling) root.Filling.Remove(id); }
        });
    }

    // --------------------------------------------------------------------------------------- the user's menu

    const uint PinnedAttribute = 0x00080000, UnpinnedAttribute = 0x00100000;

    /// <summary>Watches the root for attribute changes: Explorer's Always keep on this device and Free up space set them.</summary>
    void Watch(Root root)
    {
        var w = new FileSystemWatcher(root.Path)
        {
            IncludeSubdirectories = true,
            NotifyFilter = NotifyFilters.Attributes | NotifyFilters.FileName | NotifyFilters.DirectoryName | NotifyFilters.LastWrite,
            InternalBufferSize = 64 * 1024,
        };
        w.Changed += (_, e) =>
        {
            Queue(root, () => Attributes(root, e.FullPath));
            Queue(root, () => Written(root, e.FullPath));
        };
        // Something new (made, copied or moved in): the core takes it unless it is a placeholder.
        w.Created += (_, e) => Queue(root, () => Adopt(root, e.FullPath));
        w.Renamed += (_, e) => Queue(root, () => Adopt(root, e.FullPath));
        w.Error += (_, e) => AppLog.Write($"on-demand {Short(root.Id)}: the watcher missed changes ({e.GetException().Message})");
        w.EnableRaisingEvents = true;
        root.Watcher = w;
    }

    /// <summary>
    /// An item's pin flags changed. PINNED (Always keep on this device): kept, and the feed fills it. PINNED taken away:
    /// no longer kept. UNPINNED (Free up space): no longer kept, and freed as far as the core allows (the feed then says
    /// online only and the placeholder is dehydrated). Each choice is acted on once; the core's own pin changes match it
    /// already and do nothing.
    /// </summary>
    void Attributes(Root root, string full)
    {
        var rel = Path.GetRelativePath(root.Path, full).Replace('\\', '/');
        if (root.IdAt(rel) is not string id || !(File.Exists(full) || Directory.Exists(full))) return;
        var attributes = (uint)File.GetAttributes(full);
        var flags = attributes & (PinnedAttribute | UnpinnedAttribute);
        if (root.Seen.TryGetValue(rel, out var seen) && seen == flags) return;
        root.Seen[rel] = flags;
        bool system;
        lock (root.SystemAsked) system = root.SystemAsked.Remove(rel, out var asked) && DateTime.UtcNow - asked < TimeSpan.FromSeconds(30);
        if (system && (flags & UnpinnedAttribute) != 0)
        {
            // The system's own clean-up marked it (NOTIFY_DEHYDRATE allowed it already): not the user's Free up space.
            // Should the file be here after all, the mark goes, so Explorer shows it as here.
            if ((attributes & RecallOnDataAccess) == 0) ClearFreeMark(root, rel);
            return;
        }
        if (app.Core.SyncItem(root.Id, id) is not SyncItem item) return;
        var keep = (flags & PinnedAttribute) != 0;
        if (keep != item.Pinned)
        {
            app.Core.SyncPin(root.Id, id, keep);
            AppLog.Write($"on-demand: {rel} {(keep ? "kept on this PC" : "no longer kept")}");
        }
        if ((flags & UnpinnedAttribute) != 0 && !keep)
        {
            var freed = app.Core.SyncEvict(root.Id, id);
            AppLog.Write($"on-demand: {rel} freed {freed.Files} files, {freed.Bytes} bytes; kept {freed.Kept}"
                + string.Concat(freed.KeptWhy.Select(k => $"; {k.Path}: {k.Error}"))
                + string.Concat(freed.Failed.Select(f => $"; not freed {f.Path}: {f.Error}")));
            // What stays here loses the mark, so Explorer shows it as here (a pinned file below keeps its pin).
            foreach (var kept in freed.KeptWhy.Concat(freed.Failed)) ClearFreeMark(root, kept.Path);
            // Explorer's menu has no error to show: the core says what stayed and why (nothing when all was freed).
            if (notices.SyncEvictNotice(root.Id, freed, true) is Notice n) app.Dispatch(() => app.Notifier?.Post(n));
        }
    }

    /// <summary>Takes the Free up space mark off a file that stays here (only that mark; its pin, if any, stays).</summary>
    void ClearFreeMark(Root root, string rel)
    {
        var full = Full(root, rel);
        if (!File.Exists(full) || ((uint)File.GetAttributes(full) & UnpinnedAttribute) == 0) return;
        using (var h = CloudFiles.Open(full)) CloudFiles.CfSetPinState(h, CloudFiles.PinState.Unspecified, 0, IntPtr.Zero);
        root.Seen[rel] = (uint)File.GetAttributes(full) & (PinnedAttribute | UnpinnedAttribute);
    }

    static void Create(string parent, string name, SyncItem item, bool dir)
    {
        Directory.CreateDirectory(parent);
        var id = Encoding.UTF8.GetBytes(item.Id);
        var h = GCHandle.Alloc(id, GCHandleType.Pinned);
        try
        {
            var info = new[]
            {
                new CloudFiles.PlaceholderCreateInfo
                {
                    RelativeFileName = name,
                    FsMetadata = CloudFiles.Metadata((long)item.Size, item.MtimeNs, dir),
                    FileIdentity = h.AddrOfPinnedObject(),
                    FileIdentityLength = (uint)id.Length,
                    // The provider fills folders itself (always full).
                    Flags = CloudFiles.CreateMarkInSync | (dir ? 0x1u : 0u),
                },
            };
            CloudFiles.Check(CloudFiles.CfCreatePlaceholders(parent, info, 1, 0, out _), $"create {name}");
            CloudFiles.Check(info[0].Result, $"create {name}");
        }
        finally { h.Free(); }
    }

    /// <summary>
    /// A placeholder's metadata as the item has it now. A filled file asks the core what it holds first
    /// (sync_local_change): the user's change not reported yet is reported (the next feed brings the item as changed);
    /// an earlier version, or content this PC let go of, is dehydrated, only if nothing wrote to it since the core looked
    /// (the core has its content then, as a conflict copy perhaps).
    /// True when a dehydrated file is wanted here again (downloaded, only here, on its way), so the caller fills it.
    /// <paramref name="refused"/>: the system refused, so the file may hold what the core has not seen.
    /// </summary>
    bool Update(Root root, string full, SyncItem item, bool dir, out bool refused)
    {
        refused = false;
        // Overwritten: its change first, and it is a placeholder again (or will be, when it closes).
        if (!dir && !CloudFiles.IsPlaceholder(full))
        {
            Overwritten(root, full);
            if (!CloudFiles.IsPlaceholder(full)) return false;
        }
        var dehydrate = false;
        if (!dir && ((uint)File.GetAttributes(full) & RecallOnDataAccess) == 0)
        {
            var saved = Stamp(full);
            switch (app.Core.SyncLocalChange(root.Id, item.Id))
            {
                case SyncLocalChange.Modified:
                    LookAt(root, item.Id);
                    return false;
                case SyncLocalChange.Stale:
                    dehydrate = true;
                    break;
            }
            qaDuringUpdate?.Invoke(full, dehydrate);
            // A filled file's size is its own: the update would set it to the item's and cut off what a program has just
            // written. Only a placeholder with no content, or one let go of here, takes the item's metadata.
            if (!dehydrate) return false;
            // Windows dehydrates only a file in sync, and a write clears the mark, so one after this is refused. One before it:
            // still being written, or a save since the core looked. (The USN can't tell: it moves at each close of a handle.)
            SetInSync(full, true, item.Path);
            if (!Closed(full) || Stamp(full) != saved)
            {
                SetInSync(full, false, item.Path);
                refused = true;
                return false;
            }
        }
        var meta = CloudFiles.Metadata((long)item.Size, item.MtimeNs, dir);
        var ptr = Marshal.AllocHGlobal(Marshal.SizeOf<CloudFiles.FsMetadata>());
        try
        {
            Marshal.StructureToPtr(meta, ptr, false);
            using var h = CloudFiles.Open(full);
            var hr = CloudFiles.CfUpdatePlaceholder(h, ptr, IntPtr.Zero, 0, IntPtr.Zero, 0, dehydrate ? CloudFiles.UpdateDehydrate : 0, IntPtr.Zero, IntPtr.Zero);
            Refused(hr, "update", item.Path);
            refused = hr < 0;
            // Marked in sync for this: refused, the file still holds bytes other than the item's.
            if (dehydrate && refused)
            {
                SetInSync(full, false, item.Path);
            }
        }
        finally { Marshal.FreeHGlobal(ptr); }
        return !refused && dehydrate && item.State is SyncItemState.Downloaded or SyncItemState.ModifiedHere or SyncItemState.Downloading;
    }

    /// <summary>What a file's last save looks like, for telling whether another came after it.</summary>
    static (DateTime, long) Stamp(string full)
    {
        var f = new FileInfo(full);
        return (f.LastWriteTimeUtc, f.Length);
    }

    static void SetInSync(string full, bool inSync, string path)
    {
        using var h = CloudFiles.Open(full);
        Refused(CloudFiles.CfSetInSyncState(h, inSync ? 1u : 0u, 0, IntPtr.Zero), "in-sync state", path);
    }

    void Remove(Root root, string id)
    {
        if (!root.Unmark(id, out var path)) return;
        var full = Full(root, path);
        // Only the placeholder of this item: a plain file there is the user's new one, not adopted yet.
        if (!(File.Exists(full) || Directory.Exists(full)) || CloudFiles.IdentityOf(full) != id) return;
        // The host's own delete (the item and what was placed below it): their NOTIFY_DELETEs are not the user's.
        using (root.Own([id, .. root.IdsBelow(path)]))
        {
            if (Directory.Exists(full)) RemoveFolder(root, full, path);
            else File.Delete(full);
        }
    }

    /// <summary>
    /// A folder the feed deleted: its placeholders go. A plain file stays unless the core says its ignore line lets it go
    /// (a full folder keeps a folder that holds what is not synced, as it does here), and the folder stays with it, plain.
    /// True when the folder went.
    /// </summary>
    bool RemoveFolder(Root root, string full, string rel)
    {
        var empty = true;
        foreach (var entry in Directory.EnumerateFileSystemEntries(full))
        {
            var below = $"{rel}/{Path.GetFileName(entry)}";
            if (Directory.Exists(entry))
            {
                if (!RemoveFolder(root, entry, below)) empty = false;
            }
            else if (CloudFiles.IsPlaceholder(entry) || app.Core.SyncIgnored(root.Id, below, false) is { Deletable: true })
            {
                File.Delete(entry);
            }
            else
            {
                empty = false;
            }
        }
        if (empty)
        {
            Directory.Delete(full);
        }
        else if (CloudFiles.IsPlaceholder(full))
        {
            using var h = CloudFiles.Open(full);
            CloudFiles.CfRevertPlaceholder(h, 0, IntPtr.Zero);
        }
        return empty;
    }

    /// <summary>Whether a callback's path is where the item is placed: a file renamed aside carries the identity of an item that is elsewhere now.</summary>
    static bool PlacedAt(Root root, in CloudFiles.CallbackInfo info, string item) =>
        string.Equals(Marshal.PtrToStringUni(info.NormalizedPath), $"{root.Normal}\\{root.PathOf(item).Replace('/', '\\')}", StringComparison.OrdinalIgnoreCase);

    /// <summary>A notice the core has for a failure of this kind, if the user is to be told (once per file and problem in ten minutes).</summary>
    void Tell(Root root, SyncErrorKind kind, string path)
    {
        try
        {
            if (notices.SyncOndemandNotice(root.Id, kind, path) is Notice n) app.Dispatch(() => app.Notifier?.Post(n));
        }
        catch { }
    }

    /// <summary>
    /// A file or folder renamed onto a name not synced: it left the folder, and what is at the new name (once the rename
    /// has happened) goes back to plain files. Word deletes it in a moment; then there is nothing to do.
    /// </summary>
    void LetGo(Root root, string full, int tries = 0)
    {
        if (!File.Exists(full) && !Directory.Exists(full))
        {
            if (tries < 40) _ = Task.Delay(50).ContinueWith(_ => Queue(root, () => LetGo(root, full, tries + 1)));
            return;
        }
        // The deepest first.
        var all = Directory.Exists(full) ? Directory.EnumerateFileSystemEntries(full, "*", SearchOption.AllDirectories).Reverse().Append(full) : [full];
        foreach (var entry in all)
        {
            if (!CloudFiles.IsPlaceholder(entry)) continue;
            using var h = CloudFiles.Open(entry);
            CloudFiles.CfRevertPlaceholder(h, 0, IntPtr.Zero);
        }
    }

    // ------------------------------------------------------------------------------------------- callbacks

    Root? RootOf(in CloudFiles.CallbackInfo info)
    {
        var id = Encoding.UTF8.GetString(Bytes(info.SyncRootIdentity, (int)info.SyncRootIdentityLength));
        lock (gate) return roots.TryGetValue(id, out var r) ? r : null;
    }

    static byte[] Bytes(IntPtr p, int length)
    {
        var b = new byte[length];
        if (length > 0) Marshal.Copy(p, b, 0, length);
        return b;
    }

    /// <summary>
    /// A read of a dehydrated placeholder: the core fetches (progress shown), then the whole file goes into the
    /// placeholder from the core's staging copy, and the core adopts it (its copy goes). The core decides when a fetch
    /// gives up (no connected device has it, or no data for a while). A failure answers with the status for its kind, and
    /// the core decides whether the user is told.
    /// </summary>
    void OnFetch(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        var root = RootOf(info);
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        var (want, wantLength) = CloudFiles.FetchRange(parameters);
        if (root is null)
        {
            CloudFiles.Transfer(info, null, want, CloudFiles.StatusCloudFileProviderNotRunning, wantLength);
            return;
        }
        long sent = 0;
        try
        {
            SyncFetch f;
            ulong last = ulong.MaxValue;
            var reports = 0;
            while (true)
            {
                // The core gives up by itself (NotHereYet, its request taken back): the host keeps no timer.
                f = app.Core.SyncFetch(root.Id, item, 1000);
                if (f.LocalPath is not null) break;
                CloudFiles.CfReportProviderProgress(info.ConnectionKey, info.TransferKey, (long)f.Total, (long)f.Done);
                if (f.Done != last) { last = f.Done; reports++; }
            }
            using (var file = new FileStream(f.LocalPath, FileMode.Open, FileAccess.Read, FileShare.Read))
            {
                var buffer = new byte[Chunk];
                long offset = 0;
                int n;
                while ((n = file.Read(buffer, 0, Chunk)) > 0)
                {
                    var data = n == Chunk ? buffer : buffer[..n];
                    CloudFiles.Check(CloudFiles.Transfer(info, data, offset), "transfer");
                    offset += n;
                    sent = offset;
                    CloudFiles.CfReportProviderProgress(info.ConnectionKey, info.TransferKey, file.Length, offset);
                }
            }
            // The placeholder holds exactly this version now: the core checks it and lets its copy go.
            var adopted = app.Core.SyncHydrated(root.Id, item, f.Item.ContentVersion);
            AppLog.Write($"on-demand: {adopted.Path} filled ({adopted.State}; progress shown {reports} times while fetching)");
        }
        catch (Exception e)
        {
            var kind = (e as CoreException.OnDemand)?.kind;
            // The part of the asked range not filled yet fails (the system takes a failure only for that); a fill that
            // reached it already stands, and only the core's adoption failed.
            var from = Math.Max(want, sent);
            var answer = from < want + wantLength ? CloudFiles.Transfer(info, null, from, Status(kind), want + wantLength - from) : 0;
            AppLog.Write($"on-demand fetch of {item} failed ({kind?.ToString() ?? e.GetType().Name}): {Words(e)}{(answer < 0 ? $"; the system refused the failure ({answer:X8})" : "")}");
            if (kind is SyncErrorKind k)
            {
                Tell(root, k, root.PathOf(item));
            }
        }
    }

    void OnCancel(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        if (RootOf(info) is not Root root) return;
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        try { app.Core.SyncCancelFetch(root.Id, item); } catch { }
    }

    /// <summary>
    /// The system is about to dehydrate a placeholder (Storage Sense, another app): allowed only when the core frees it
    /// (not kept, and another device confirms it holds it); denied otherwise, silently (the core's rule for a clean-up
    /// the user did not start).
    /// </summary>
    void OnDehydrate(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        var allow = false;
        try
        {
            if (RootOf(info) is Root root)
            {
                var path = root.PathOf(item);
                var freed = app.Core.SyncEvict(root.Id, item);
                allow = freed.Kept == 0 && freed.Failed.Length == 0;
                // An allowed dehydration leaves the Free up space mark (a denied one leaves none): noted before the
                // answer lets it happen, so the watcher does not take it for the user's choice.
                if (allow) lock (root.SystemAsked) root.SystemAsked[path] = DateTime.UtcNow;
                // A clean-up the user did not start: the core keeps it silent (asked all the same, the rule is its).
                if (notices.SyncEvictNotice(root.Id, freed, false) is Notice n) app.Dispatch(() => app.Notifier?.Post(n));
                AppLog.Write($"on-demand: the system asked to free {path}: {(allow ? "allowed" : "denied")}"
                    + string.Concat(freed.KeptWhy.Select(k => $"; {k.Error}")) + string.Concat(freed.Failed.Select(f => $"; {f.Error}")));
            }
        }
        catch (Exception e) { AppLog.Write($"on-demand: the system asked to free {item}: denied ({Words(e)})"); }
        CloudFiles.Ack(info, CloudFiles.OperationType.AckDehydrate, allow ? CloudFiles.StatusSuccess : CloudFiles.StatusCloudFileUnsuccessful);
    }

    /// <summary>The status a callback answers with, for the core's kind of failure (the notes' table).</summary>
    static int Status(SyncErrorKind? kind) => kind switch
    {
        SyncErrorKind.NotHereYet => CloudFiles.StatusCloudFileNetworkUnavailable,
        SyncErrorKind.NotRunning => CloudFiles.StatusCloudFileProviderNotRunning,
        SyncErrorKind.NoItem or SyncErrorKind.Gone => CloudFiles.StatusObjectNameNotFound,
        SyncErrorKind.Changed => CloudFiles.StatusCloudFileNotInSync,
        _ => CloudFiles.StatusCloudFileUnsuccessful,
    };

    // ----------------------------------------------------------------------------------- the user's changes

    /// <summary>A full path's place in the folder ('/' between names; "" for the root), or null outside it.</summary>
    static string? RelativeTo(Root root, string full)
    {
        var rel = Path.GetRelativePath(root.Path, full);
        if (rel == ".") return "";
        return rel.StartsWith("..") || Path.IsPathRooted(rel) ? null : rel.Replace('\\', '/');
    }

    static string ParentOf(string rel) => rel.LastIndexOf('/') is var i and >= 0 ? rel[..i] : "";

    static string NameOf(string rel) => rel[(rel.LastIndexOf('/') + 1)..];

    /// <summary>A folder's item by its path ("root" for the folder itself), or null when it is not placed.</summary>
    static string? FolderId(Root root, string rel) => rel.Length == 0 ? "root" : root.IdAt(rel);

    /// <summary>
    /// The user renames or moves an item, before it happens: the core records it (sync_modify_item: the new parent and
    /// name) and the answer lets it happen, or refuses it with the status for the core's reason. Moved out of the folder,
    /// it leaves the folder on every device, as a delete does. Onto a name not synced (Word's save renames the document
    /// aside), the core takes the item out and the rename must happen; with its content not here the core refuses and
    /// says so. The host's own moves pass, and so does a file that only carries the identity of an item now elsewhere.
    /// </summary>
    void OnRename(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        var status = CloudFiles.StatusSuccess;
        if (RootOf(info) is Root root && !root.IsOwn(item) && PlacedAt(root, info, item))
        {
            var from = root.PathOf(item);
            try
            {
                var inScope = (CloudFiles.NotifyFlags(parameters) & CloudFiles.RenameTargetInScope) != 0;
                if ((inScope ? RelativeTo(root, CloudFiles.RenameTarget(info, parameters)) : null) is not string to)
                {
                    status = Deleted(root, item, from, app.Core.SyncDeleteItem(root.Id, item));
                }
                else
                {
                    string? parent = null;
                    if (ParentOf(to) != ParentOf(from))
                    {
                        // Into a folder made a moment ago: the queue takes it to the core first.
                        if (FolderId(root, ParentOf(to)) is null) root.Work.Wait(TimeSpan.FromSeconds(5));
                        parent = FolderId(root, ParentOf(to)) ?? throw new InvalidOperationException($"{ParentOf(to)} is not in the folder yet");
                    }
                    try
                    {
                        var moved = app.Core.SyncModifyItem(root.Id, item, parent, NameOf(to) == NameOf(from) ? null : NameOf(to), null, null);
                        // Placed there from now on, what is below it too: a callback for one of them comes with the new path.
                        root.Moved(from, moved.Path);
                        AppLog.Write($"on-demand: {from} moved here to {moved.Path}");
                    }
                    catch (CoreException.OnDemand e) when (e.kind == SyncErrorKind.Ignored)
                    {
                        // The core took the item out: at once no longer placed, so no feed step touches the old path, where
                        // the saved file is about to appear.
                        root.UnmarkTree(item);
                        root.NoteLeft(from);
                        AppLog.Write($"on-demand: {from} left the folder as {to} ({Words(e)})");
                        Queue(root, () => LetGo(root, Full(root, to)));
                    }
                }
            }
            catch (Exception e)
            {
                var kind = (e as CoreException.OnDemand)?.kind;
                status = Status(kind);
                AppLog.Write($"on-demand: {from} not moved ({Words(e)})");
                if (kind == SyncErrorKind.IgnoredNotHere) Tell(root, kind.Value, from);
            }
        }
        CloudFiles.Ack(info, CloudFiles.OperationType.AckRename, status);
    }

    /// <summary>
    /// The user deletes an item, before it happens: deleted on every device (sync_delete_item). On a read-only member
    /// nothing is deleted and the content is freed instead: the delete is refused and the feed dehydrates it. Something
    /// that could not go (open elsewhere) refuses it too. The host's own deletes pass, and so does a file that only carries
    /// the identity of an item now elsewhere (Word's renamed-aside document).
    /// </summary>
    void OnDelete(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        var status = CloudFiles.StatusSuccess;
        if (RootOf(info) is Root root && !root.IsOwn(item) && (CloudFiles.NotifyFlags(parameters) & CloudFiles.DeleteIsUndelete) == 0 && PlacedAt(root, info, item))
        {
            var path = root.PathOf(item);
            try
            {
                status = Deleted(root, item, path, app.Core.SyncDeleteItem(root.Id, item));
            }
            catch (CoreException.OnDemand e) when (e.kind is SyncErrorKind.NoItem or SyncErrorKind.Gone)
            {
                // Gone from the core already: nothing to keep.
            }
            catch (Exception e)
            {
                status = Status((e as CoreException.OnDemand)?.kind);
                AppLog.Write($"on-demand: {path} not deleted ({Words(e)})");
            }
        }
        CloudFiles.Ack(info, CloudFiles.OperationType.AckDelete, status);
    }

    /// <summary>
    /// What a delete answers: done, or refused (a read-only member freed it instead; something stayed). Done, the item is at
    /// once no longer placed: the feed tells of it seconds later, and a new file at that path (a save) is not this placeholder.
    /// </summary>
    static int Deleted(Root root, string item, string path, SyncDeleted gone)
    {
        if (gone.Freed is not null)
        {
            AppLog.Write($"on-demand: {path} not deleted (read-only here): freed instead");
            return CloudFiles.StatusCloudFileAccessDenied;
        }
        if (gone.Failed.Length > 0)
        {
            AppLog.Write($"on-demand: {path} not deleted" + string.Concat(gone.Failed.Select(f => $"; {f.Path}: {f.Error}")));
            return CloudFiles.StatusCloudFileInUse;
        }
        root.UnmarkTree(item);
        root.NoteLeft(path);
        AppLog.Write($"on-demand: {path} deleted here ({gone.Deleted} items)");
        return CloudFiles.StatusSuccess;
    }

    /// <summary>A placeholder closed (not deleted): on the feed's queue, a filled file's change of the user's is reported.</summary>
    void OnClose(IntPtr infoPtr, IntPtr parameters)
    {
        if ((CloudFiles.NotifyFlags(parameters) & CloudFiles.CloseDeleted) != 0) return;
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        if (RootOf(info) is not Root root) return;
        var item = Encoding.UTF8.GetString(Bytes(info.FileIdentity, (int)info.FileIdentityLength));
        if (!PlacedAt(root, info, item)) return;
        Queue(root, () => LookAt(root, item));
    }

    /// <summary>
    /// What a filled file holds against the core: a change of the user's is reported. A report the core could not take
    /// (the file was written while it was copied, or is still changing) is offered again a moment later: nothing else
    /// would tell of it, and the save would stay on this PC.
    /// </summary>
    void LookAt(Root root, string item, int tries = 0)
    {
        var path = root.PathOf(item);
        var full = Full(root, path);
        if (!File.Exists(full) || ((uint)File.GetAttributes(full) & RecallOnDataAccess) != 0) return;
        if (!CloudFiles.IsPlaceholder(full))
        {
            Overwritten(root, full);
            return;
        }
        try
        {
            if (app.Core.SyncLocalChange(root.Id, item) is SyncLocalChange.Modified m) Report(root, item, path, m.Base);
        }
        // A busy copy is a plain core error (no on-demand kind of its own).
        catch (CoreException e) when (tries < 120)
        {
            if (tries == 0) AppLog.Write($"on-demand: {path} offered again ({Words(e)})");
            Later(root, full, 500, () => LookAt(root, item, tries + 1));
        }
    }

    /// <summary>
    /// The user's change of a file's content: the core takes it (copied, from the version it started from) and the
    /// placeholder, which holds exactly that, is adopted. When another device's version came first the change becomes a
    /// conflict copy, which the feed places; the feed names this item again too, the core then says it is stale, and Update
    /// lets this placeholder go. However often it is reported, one content is one copy.
    /// </summary>
    void Report(Root root, string item, string path, string? @base)
    {
        var changed = app.Core.SyncModifyItem(root.Id, item, null, null, Full(root, path), @base);
        if (changed.Id == item)
        {
            app.Core.SyncHydrated(root.Id, item, changed.ContentVersion);
            AppLog.Write($"on-demand: {path} changed here");
        }
        else
        {
            AppLog.Write($"on-demand: {path} changed here and on another device: kept as {changed.Path}");
        }
    }

    /// <summary>
    /// A placeholder a program overwrote: CREATE_ALWAYS takes the placeholder away, so it is a plain file, and a plain file
    /// sends no close notification: without this the save would never leave this PC. Once no one has it open, its change is
    /// reported and it is a placeholder again. Looked at once more after that: a write that came between the two told no one.
    /// </summary>
    void Overwritten(Root root, string full, int tries = 0)
    {
        if (RelativeTo(root, full) is not string rel || rel.Length == 0 || root.IdAt(rel) is not string id) return;
        if (!File.Exists(full) || CloudFiles.IsPlaceholder(full)) return;
        if (!Closed(full))
        {
            Later(root, full, 500, () => Overwritten(root, full));
            return;
        }
        try
        {
            if (app.Core.SyncLocalChange(root.Id, id) is SyncLocalChange.Modified m) Report(root, id, rel, m.Base);
            CloudFiles.Convert(full, id, false);
            if (app.Core.SyncLocalChange(root.Id, id) is SyncLocalChange.Modified again) Report(root, id, rel, again.Base);
        }
        // Written again meanwhile, or the system not done with it: looked at again while it comes to rest.
        catch (Exception e) when (tries < 120)
        {
            if (tries == 0) AppLog.Write($"on-demand: {rel} overwritten, looked at again ({Words(e)})");
            Later(root, full, 500, () => Overwritten(root, full, tries + 1));
        }
    }

    /// <summary>
    /// The watcher saw a write to a file the host knows. Not only a close notification tells of a change: none comes for a
    /// plain file, and a save must not hang on one signal. The core says first whether it changed (this event may be only
    /// an attribute, and looking whether a file is closed takes it for a moment, in the way of the host's own calls on it);
    /// a change is reported once no one has it open, so a file still being written is not copied again and again.
    /// </summary>
    void Written(Root root, string full)
    {
        if (RelativeTo(root, full) is not string rel || rel.Length == 0 || root.IdAt(rel) is not string id || !File.Exists(full)) return;
        if (!CloudFiles.IsPlaceholder(full))
        {
            Overwritten(root, full);
            return;
        }
        if (((uint)File.GetAttributes(full) & RecallOnDataAccess) != 0 || app.Core.SyncLocalChange(root.Id, id) is not SyncLocalChange.Modified) return;
        if (Closed(full)) LookAt(root, id);
        else Later(root, full, 500, () => Written(root, full));
    }

    /// <summary>Looks at a file again a moment later, off the queue while it waits; one wait per file.</summary>
    void Later(Root root, string full, int ms, Action look)
    {
        lock (root.Waiting) if (!root.Waiting.Add(full)) return;
        _ = Task.Delay(ms).ContinueWith(_ =>
        {
            lock (root.Waiting) root.Waiting.Remove(full);
            Queue(root, look);
        });
    }

    /// <summary>
    /// Something not a placeholder appeared in the folder (made here, copied or moved in): the core takes it
    /// (sync_create_item: a file with its content copied; a folder, then what is in it) and it becomes a placeholder in
    /// sync. A name the folder does not sync stays a plain file of this PC. A file still being written is looked at again
    /// a moment later (sooner where a file just left: a save by replace, and the core waits only so long for it). Where the
    /// core names it otherwise (a conflict copy: something else is at that name now), it is renamed so.
    /// </summary>
    void Adopt(Root root, string full, int failures = 0)
    {
        if (RelativeTo(root, full) is not string rel || rel.Length == 0 || root.IdAt(rel) is not null) return;
        var dir = Directory.Exists(full);
        if (!dir && !File.Exists(full)) return;
        // A placeholder renamed or moved: its NOTIFY_RENAME told the core already.
        if (CloudFiles.IsPlaceholder(full)) return;
        // Asked first: Word holds its owner file open for as long as the document is, so it would never count as closed.
        if (app.Core.SyncIgnored(root.Id, rel, dir) is SyncIgnored ignored)
        {
            AppLog.Write($"on-demand: {rel} not synced ({ignored.Reason})");
            return;
        }
        if (FolderId(root, ParentOf(rel)) is null) Adopt(root, Full(root, ParentOf(rel)));
        // The folder's adoption took what is in it, this one too.
        if (root.IdAt(rel) is not null) return;
        if (FolderId(root, ParentOf(rel)) is not string parent) return;
        if (!dir && !Closed(full))
        {
            Later(root, full, root.JustLeft(rel) ? 50 : 500, () => Adopt(root, full, failures));
            return;
        }
        SyncItem item;
        try
        {
            item = app.Core.SyncCreateItem(root.Id, parent, NameOf(rel), dir, dir ? null : full);
        }
        catch (CoreException.OnDemand e) when (e.kind == SyncErrorKind.Ignored)
        {
            AppLog.Write($"on-demand: {rel} not synced ({Words(e)})");
            return;
        }
        // Written while the core copied it (a plain core error): offered again, or a new file would stay on this PC.
        catch (CoreException e) when (e is not CoreException.OnDemand && failures < 120)
        {
            if (failures == 0) AppLog.Write($"on-demand: {rel} offered again ({Words(e)})");
            Later(root, full, 500, () => Adopt(root, full, failures + 1));
            return;
        }
        if (item.Path != rel)
        {
            var named = Full(root, item.Path);
            if (dir) Directory.Move(full, named); else File.Move(full, named);
            full = named;
        }
        CloudFiles.Convert(full, item.Id, dir);
        root.Mark(item.Id, item.Path);
        if (!dir) app.Core.SyncHydrated(root.Id, item.Id, item.ContentVersion);
        AppLog.Write($"on-demand: {item.Path} added here");
        if (dir) foreach (var child in Directory.EnumerateFileSystemEntries(full)) Adopt(root, child);
    }

    /// <summary>Whether no one has the file open any more; looking never stands in the way of another program's write.</summary>
    static bool Closed(string full) => CloudFiles.NobodyHasOpen(full);

    // ------------------------------------------------------------------------------------------------------ QA

    /// <summary>QA: each root (its folder, connected or not, items placed) and each file's state (p: placeholder not filled, f: filled, P: pinned, U: marked to free).</summary>
    internal string QaStatus()
    {
        Root[] all;
        lock (gate) all = [.. roots.Values];
        return "roots " + all.Length + " [" + string.Join(" | ", all.Select(r =>
        {
            var files = Directory.Exists(r.Path)
                ? Directory.EnumerateFiles(r.Path, "*", SearchOption.AllDirectories).Select(f =>
                {
                    var a = (uint)File.GetAttributes(f);
                    return $"{Path.GetRelativePath(r.Path, f).Replace('\\', '/')}:{((a & RecallOnDataAccess) != 0 ? "p" : "f")}{((a & PinnedAttribute) != 0 ? "P" : "")}{((a & UnpinnedAttribute) != 0 ? "U" : "")}";
                })
                : [];
            return $"{Short(r.Id)} connected {r.Key != 0} placed {r.Count} files [{string.Join(", ", files)}]";
        })) + "]";
    }

    /// <summary>
    /// QA: a file of the first root read as an app reads it (a placeholder is filled first, within 90 s): a short text
    /// file's text, another file's size and SHA-256, or why not.
    /// </summary>
    /// <summary>QA: runs after the core said what a filled file holds and before its placeholder is updated.</summary>
    Action<string, bool>? qaDuringUpdate;
    volatile bool qaWroteWhenStale;

    /// <summary>
    /// QA: a program's write at the worst moment of an update from the feed: the feed's item, of the size the file had
    /// before, is placed while the write lands (in place, so the file stays a placeholder). Answers what the file holds then.
    /// </summary>
    internal string QaWriteDuringUpdate(string relative, string text)
    {
        Root? r;
        lock (gate) r = roots.Values.FirstOrDefault();
        if (r is null || r.IdAt(relative) is not string id || app.Core.SyncItem(r.Id, id) is not SyncItem item) return "write: no such item";
        var bytes = Encoding.ASCII.GetBytes(text);
        qaDuringUpdate = (full, _) =>
        {
            using var f = new FileStream(full, FileMode.Open, FileAccess.Write, FileShare.Read);
            f.Write(bytes);
        };
        var done = new TaskCompletionSource();
        Queue(r, () =>
        {
            try { Place(r, item); }
            finally { qaDuringUpdate = null; done.SetResult(); }
        });
        done.Task.Wait(30_000);
        return $"holds \"{File.ReadAllText(Full(r, relative))}\"";
    }

    /// <summary>
    /// QA: a program's write at the moment the core has said a filled file is an earlier version, before it is let go of:
    /// armed now, it lands when another device's change reaches the file. A dehydrate from here on would throw it away.
    /// </summary>
    internal string QaWriteWhenStale(string relative, string text)
    {
        Root? r;
        lock (gate) r = roots.Values.FirstOrDefault();
        if (r is null) return "write: no root";
        var target = Full(r, relative);
        var bytes = Encoding.ASCII.GetBytes(text);
        qaWroteWhenStale = false;
        qaDuringUpdate = (full, stale) =>
        {
            if (!stale || !string.Equals(full, target, StringComparison.OrdinalIgnoreCase)) return;
            qaDuringUpdate = null;
            using (var f = new FileStream(full, FileMode.Open, FileAccess.Write, FileShare.Read)) f.Write(bytes);
            qaWroteWhenStale = true;
        };
        return "armed";
    }

    internal bool QaWroteWhenStale => qaWroteWhenStale;

    internal string QaRead(string relative)
    {
        Root? r;
        lock (gate) r = roots.Values.FirstOrDefault();
        if (r is null) return "read: no root";
        try
        {
            var t = Task.Run(() => File.ReadAllBytes(Path.Combine(r.Path, relative.Replace('/', '\\'))));
            // Waited on through its handle: Wait would run a task not started yet on this thread, past any time limit.
            var done = ((IAsyncResult)t).AsyncWaitHandle.WaitOne(90_000);
            if (!done) return "read: timed out";
            var b = t.Result;
            return b.Length <= 200 && b.All(c => c is >= 0x20 and < 0x7f)
                ? $"read \"{Encoding.ASCII.GetString(b)}\""
                : $"read {b.Length} bytes, sha256 {Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(b)).ToLowerInvariant()}";
        }
        catch (Exception e) { return $"read failed: {e.InnerException?.Message ?? e.Message}"; }
    }

    static string Words(Exception e) => e is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : e.Message;

    static string Short(string id) => id[..Math.Min(8, id.Length)];
}
