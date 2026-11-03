using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The open file lists of a place, as the core keeps them (<c>FileLists</c>, trs-ffi file_list.rs: which are open, the
/// trees, folders, filters, the selection, the edits on their way and when to read again); the Mac's FilesStore. This
/// only runs what the core asks for, off the UI thread where it reads the engine, and keeps the drawn lists: the
/// window's (<see cref="App.Files"/>) and the tray's own (<see cref="App.MenuFiles"/>).
/// </summary>
sealed class FilesStore
{
    readonly App app;
    readonly Dictionary<string, Microsoft.UI.Dispatching.DispatcherQueueTimer> flushTimers = [];

    internal FileLists Lists { get; }

    /// <summary>The window's lists as drawn, by torrent.</summary>
    internal Dictionary<string, FileListView> Views { get; private set; } = [];

    /// <summary>Torrents with their list open, and the selected file rows (both as the core has them).</summary>
    internal string[] Expanded { get; private set; } = [];
    internal FileSelection? Selection { get; private set; }

    /// <summary>Told when anything drawn may have changed (a list read, an edit, the engine's answer); the torrent.</summary>
    internal event Action<string?>? Changed;

    /// <summary>Told after an edit reached the engine, so the rows pick up the new wanted sizes.</summary>
    internal event Action? Edited;

    public FilesStore(App app, FileListPlace place)
    {
        this.app = app;
        Lists = new FileLists(place);
    }

    /// <summary>
    /// "Show All": the window's lists grow to their full height instead of scrolling inside (one choice for every list),
    /// remembered as the Mac's UserDefaults fileListShowAll is: a marker file in the state folder (so a QA run, on its
    /// own state, never changes the user's).
    /// </summary>
    internal bool ShowAll
    {
        get => showAll ??= File.Exists(ShowAllPath);
        private set
        {
            showAll = value;
            try
            {
                if (value) { Directory.CreateDirectory(Path.GetDirectoryName(ShowAllPath)!); File.WriteAllText(ShowAllPath, ""); }
                else File.Delete(ShowAllPath);
            }
            catch (Exception e) { AppLog.Write($"show all not remembered: {e.Message}"); }
        }
    }

    bool? showAll;

    string ShowAllPath => Path.Combine(app.Core.StateDir(), "windows", "file-list-show-all");

    internal bool IsExpanded(string id) => Expanded.Contains(id);

    internal bool IsSelected(string torrent, uint node) => Selection is { } s && s.TorrentId == torrent && s.Nodes.Contains(node);

    internal void Toggle(string id)
    {
        if (Lists.Toggle(id)) Load(id);
        Redraw(id);
    }

    internal void Expand(string id)
    {
        if (Lists.Expand(id)) Load(id);
        Redraw(id);
    }

    internal void Collapse(string id)
    {
        Lists.Collapse(id);
        Redraw(id);
    }

    /// <summary>A page or tab change: every torrent opens closed again.</summary>
    internal void CollapseAll()
    {
        Lists.CollapseAll();
        Redraw(null);
    }

    /// <summary>The window shows again: lists that were open are read again.</summary>
    internal void Resume()
    {
        foreach (var id in Lists.Resume()) Load(id);
        Redraw(null);
    }

    /// <summary>The window hides: nothing is read until it shows again.</summary>
    internal void Suspend()
    {
        Lists.Suspend();
        Redraw(null);
    }

    /// <summary>From the window's refresh: the lists the core says are due are read again.</summary>
    internal void Tick()
    {
        foreach (var id in Lists.Due(App.Now())) Read(id);
    }

    // ------------------------------------------------------------------------------------------------ reading

    /// <summary>Reads the files and builds the tree off the UI thread (thousands of files take a moment to sort).</summary>
    internal void Load(string id)
    {
        _ = Task.Run(async () =>
        {
            TorrentFile[]? files = null;
            try { files = await app.Core.Files(id); } catch (Exception e) { AppLog.Write($"files of {Short(id)} not read: {e.Message}"); }
            Lists.Loaded(id, files, App.Now());
        }).ContinueWith(_ => app.Dispatch(() => Redraw(id)));
    }

    /// <summary>The tray's version of <see cref="Load"/>: the list is in place when this returns (the flyout is about to show it).</summary>
    internal void LoadNow(string id)
    {
        Lists.Loaded(id, app.Core.FilesShown(id), App.Now());
        Redraw(id);
    }

    void Read(string id)
    {
        _ = Task.Run(async () =>
        {
            TorrentFile[]? files = null;
            try { files = await app.Core.Files(id); } catch (Exception e) { AppLog.Write($"files of {Short(id)} not read: {e.Message}"); }
            return Lists.Read(id, files, App.Now());
        }).ContinueWith(t => { if (t.IsCompletedSuccessfully && t.Result) app.Dispatch(() => Redraw(id)); });
    }

    // ---------------------------------------------------------------------------------------------- selection

    internal void ClearSelection()
    {
        Lists.ClearSelection();
        Redraw(null);
    }

    /// <summary>A click on a file or folder row.</summary>
    internal void Select(string torrent, uint node, bool command, bool shift)
    {
        Lists.Click(torrent, node, command, shift);
        Redraw(torrent);
    }

    /// <summary>Space: check what is unchecked, else skip.</summary>
    internal void ToggleSelected()
    {
        if (Lists.Space() is string id) EditedNow(id);
    }

    /// <summary>→ on the selected row: opens a folder.</summary>
    internal bool OpenSelected()
    {
        var done = Lists.Right();
        Redraw(Selection?.TorrentId);
        return done;
    }

    /// <summary>← on the selected row: closes a folder, else selects the folder it is in (or the torrent's row).</summary>
    internal FileLeft CloseSelected()
    {
        var left = Lists.Left();
        Redraw(Selection?.TorrentId);
        return left;
    }

    // ------------------------------------------------------------------------------------- folders and edits

    internal void ToggleFolder(string id, uint node)
    {
        Lists.ToggleFolder(id, node);
        Redraw(id);
    }

    internal void SetFilter(string id, string text)
    {
        Lists.SetFilter(id, text);
        Redraw(id);
    }

    /// <summary>The check of a row (and of the selection it is in).</summary>
    internal void ToggleCheck(string id, uint node)
    {
        if (Lists.ToggleCheck(id, node)) EditedNow(id);
    }

    internal void SetPriority(string id, uint node, FilePriority p)
    {
        if (Lists.SetPriority(id, node, p)) EditedNow(id);
    }

    /// <summary>Select All / Select None on what the filter shows (everything without one).</summary>
    internal void SelectAll(string id, bool on)
    {
        if (Lists.SelectAll(id, on)) EditedNow(id);
    }

    /// <summary>Shown at once; sent to the engine after the core's quiet moment, so a run of clicks is one change.</summary>
    void EditedNow(string id)
    {
        Redraw(id);
        if (!flushTimers.TryGetValue(id, out var t))
        {
            t = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
            t.IsRepeating = false;
            t.Tick += (_, _) => Flush(id);
            flushTimers[id] = t;
        }
        t.Stop();
        t.Interval = TimeSpan.FromMilliseconds(Views.TryGetValue(id, out var v) ? v.FlushMs : Lists.MenuView(id)?.FlushMs ?? 250);
        t.Start();
    }

    /// <summary>Sends the edits now (the engine call starts at once, off the UI thread).</summary>
    internal void Flush(string id)
    {
        if (flushTimers.Remove(id, out var t)) t.Stop();
        if (Lists.TakeEdits(id) is not FileEdits edits) return;
        // Files were ticked: a finished torrent may become a download again (SPEC 8.3); it is not announced as new.
        if (edits.Ticked) app.Arrivals.UserReselected(id, (ulong)App.Now());
        _ = Task.Run(() => app.Core.SetFilePriorities(id, edits.Changes)).ContinueWith(task => app.Dispatch(() =>
        {
            if (!task.IsCompletedSuccessfully) AppLog.Write($"file priorities of {Short(id)} not set: {task.Exception?.InnerException?.Message}");
            var next = Lists.Sent(id, task.IsCompletedSuccessfully);
            Edited?.Invoke();
            switch (next)
            {
                case FileListRead.Reload: Load(id); break;
                case FileListRead.Read: Read(id); break;
                default: Redraw(id); break;
            }
        }));
    }

    // ---------------------------------------------------------------------------------------------- drawing

    /// <summary>Everything drawn, again from the core; then <see cref="Changed"/>.</summary>
    void Redraw(string? id)
    {
        Expanded = Lists.Expanded();
        Selection = Lists.Selection();
        var show = ShowAll;
        var next = new Dictionary<string, FileListView>();
        foreach (var open in Expanded)
        {
            if (Lists.View(open, show) is FileListView v) next[open] = v;
        }
        Views = next;
        Changed?.Invoke(id);
    }

    /// <summary>"Show All" pressed: every list again at its new height.</summary>
    internal void SetShowAll(bool on)
    {
        ShowAll = on;
        Redraw(null);
    }

    static string Short(string id) => id[..Math.Min(8, id.Length)];
}
