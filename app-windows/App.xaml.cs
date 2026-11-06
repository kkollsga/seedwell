using H.NotifyIcon;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The app: the core, the notification-area icon (drawn live from the core's icon model), the tray flyout (the
/// core's menu model) and the library window. Closing the library window keeps the app running in the tray; Quit
/// stops the core and ends it.
/// </summary>
public partial class App : Application
{
    public static new App? Current { get; private set; }

    internal Core Core { get; private set; } = null!;
    /// <summary>The library window's state, for the app's life (the core's Library: place, search, folds, selection).</summary>
    internal Library Library { get; private set; } = null!;
    string foldsFile = "";
    /// <summary>One for the app's life: which torrents just arrived (the menu lists them first).</summary>
    internal Arrivals Arrivals { get; } = new();
    internal IconState IconState { get; } = new();
    /// <summary>The Folder Sync menu's stop-syncing confirmation (one open at a time).</summary>
    internal SyncRemoval SyncRemoval { get; } = new();
    /// <summary>
    /// The windows Windows has (the core's HostWindow): commands that need another are not offered (row_commands, the
    /// sidebar's adds and menus). Not yet: the chooser, custom goal, rules editor, player, Tracker Info, favorite
    /// editing and a channel's Share dialog.
    /// </summary>
    /// <summary>What this host can do beyond its windows (the core's HostFeature): on-demand folders, over Cloud Files.</summary>
    internal static HostFeature[] HostFeatures { get; } = [HostFeature.OnDemandFolders];

    internal static HostWindow[] HostWindows { get; } = [HostWindow.GetInfo, HostWindow.MoveTo, HostWindow.NewChannel, HostWindow.ChatDialogs, HostWindow.AddFolder, HostWindow.FolderEditor, HostWindow.FolderShare, HostWindow.ChannelShare, HostWindow.FileList, HostWindow.Chooser, HostWindow.Player, HostWindow.TrackerInfo, HostWindow.FavoriteEditing, HostWindow.RulesEditor];
    /// <summary>Notifications (the core's Notices), the core's questions, and the settings banner.</summary>
    internal Notifier? Notifier { get; private set; }
    /// <summary>Whether other devices can reach this PC (the core's, one per run; Folder Sync's page draws it).</summary>
    internal SyncReach Reach => reach ??= new SyncReach(Core);
    SyncReach? reach;
    /// <summary>The torrent proxy, as its views share it.</summary>
    internal ProxyUi Proxy => proxy ??= new ProxyUi(this);
    ProxyUi? proxy;
    internal Prompts Prompts { get; private set; } = null!;
    internal SettingsBannerView Banner { get; } = new();
    /// <summary>The small chat dialogs (New Topic…, Rename…, Leave…, ...).</summary>
    internal ChatDialogs ChatDialogs { get; private set; } = null!;
    readonly SleepGuard sleep = new();
    /// <summary>The app's updates (the core's Updates): its timer, checks and installs.</summary>
    internal Updater Updater { get; private set; } = null!;
    bool wasDownloading;

    DispatcherQueue ui = null!;
    DispatcherQueueTimer iconTimer = null!;
    TaskbarIcon? tray;
    /// <summary>QA (a --state-dir run with SEEDWELL_QA_NO_TRAY=1): the first icon fails as with no notification area.</summary>
    bool failTrayOnce = Program.StateDir is not null && Environment.GetEnvironmentVariable("SEEDWELL_QA_NO_TRAY") == "1";
    IconLook? drawn;
    string? iconError;
    MainWindow? main;
    FlyoutWindow? flyout;
    bool quitting;
    /// <summary>The core is there (it starts off the UI thread); until then what arrives waits in <see cref="pending"/>.</summary>
    bool started;
    bool quitBeforeCore;
    readonly List<Action> pending = [];
    /// <summary>Kept alive for its ColorValuesChanged event (the light/dark taskbar changes the icon's grey).</summary>
    readonly Windows.UI.ViewManagement.UISettings uiSettings = new();

    public App()
    {
        Current = this;
        // The visual QA run draws one theme per run (the app's theme is fixed once it starts).
        if (Program.Render is { } render)
        {
            RequestedTheme = render.Light ? ApplicationTheme.Light : ApplicationTheme.Dark;
        }
        // A QA run's pictures, likewise (qa\run.ps1 proxy).
        else if (Program.QaControl && Environment.GetEnvironmentVariable("SEEDWELL_QA_THEME") is string qaTheme)
        {
            RequestedTheme = qaTheme == "light" ? ApplicationTheme.Light : ApplicationTheme.Dark;
        }
        InitializeComponent();
        // The app ends only by Exit, after the core's shutdown: by default it ends when its last window closes, which
        // Quit does first, and the process could end before the core had saved and sent its "stopped" announces.
        DispatcherShutdownMode = DispatcherShutdownMode.OnExplicitShutdown;
    }

    internal static ulong Now() => (ulong)DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

    /// <summary>The core's snapshot; every one goes through <see cref="Arrivals"/>.</summary>
    internal Snapshot TakeSnapshot()
    {
        var s = Core.Snapshot();
        // A new download goes to the library window, which shows it, flags Downloads or remembers it (Rust decides).
        foreach (var id in Arrivals.Observe(s, Now()))
        {
            var outcome = Library.NewDownload(id, main?.Pane.Context() ?? new WindowContext(false, [], null, 0));
            AppLog.Write($"new download {id}: {outcome}");
            main?.Pane.Refresh();
        }
        return s;
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        ui = DispatcherQueue.GetForCurrentThread();
        if (Program.Render is { } render)
        {
            RenderSnapshots(render.Dir, render.Light);
            return;
        }
        // The app's real data folder, as the core picks it on Windows (%LOCALAPPDATA%\Seedwell), unless --state-dir.
        var dir = Program.StateDir ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Seedwell");
        AppLog.Open(dir);
        AppLog.Write($"launch: state {dir}, {Program.Routes.Length} to open");
        if (GoBackAfterFailedLaunches(dir))
        {
            return;
        }
        // The UI thread is watched from the start; in a QA run a stall of fail_ms fails it (qa/run.ps1 reads the log).
        _ = new HangWatch(ui, () => started ? Core : null, Program.QaControl);
        // The icon at once, in its plain look; its menu and the flyout come with the core.
        CreateTray();
        StartCore(dir, 0);
    }

    /// <summary>
    /// Starts the core off the UI thread (docs/CROSS-PLATFORM.md §1: it opens the state, loads every torrent and reads
    /// the library once, which takes a while for a big library), then <see cref="CoreStarted"/> on the UI thread. The
    /// previous run may still be quitting and hold the state (up to 120 s while trackers hear "stopped"): it is waited
    /// for quietly, as the Mac's startCore does.
    /// </summary>
    void StartCore(string dir, int waitedSecs)
    {
        // A --state-dir run (QA, tests) is isolated: the core refuses it when a configured folder lies in the user's
        // profile outside the temp folder, and it trashes into <state>\Trash, never the Recycle Bin.
        var forbidden = Program.StateDir is null ? null : Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        // QA only (an isolated run): a core that takes this long to start (SEEDWELL_QA_SLOW_CORE_MS).
        var slow = Program.StateDir is not null && int.TryParse(Environment.GetEnvironmentVariable("SEEDWELL_QA_SLOW_CORE_MS"), out var ms) ? ms : 0;
        var clock = System.Diagnostics.Stopwatch.StartNew();
        _ = Task.Run(() =>
        {
            if (slow > 0 && waitedSecs == 0) Thread.Sleep(slow);
            var core = new Core(new CoreOptions(dir, false, false, forbidden));
            // The first library read may wait for the engine: done here, so the UI thread's reads find it.
            _ = core.Snapshot();
            return core;
        }).ContinueWith(t => ui.TryEnqueue(() => CoreStarted(t, dir, waitedSecs, clock.ElapsedMilliseconds)));
    }

    void CoreStarted(Task<Core> t, string dir, int waitedSecs, long ms)
    {
        if (!t.IsCompletedSuccessfully)
        {
            // The core's sentence (a CoreException's own message carries its record's fields).
            var error = t.Exception is { } ex ? CoreCalls.Message(ex) : "";
            // The control socket says "another instance"; the state database, opened first, says it is locked.
            bool busy = new[] { "another instance", "Database already open", "Cannot acquire lock", "being used by another process" }.Any(b => error.Contains(b, StringComparison.OrdinalIgnoreCase));
            // Quit while waiting for a previous run: there is no core to shut down yet.
            if (quitBeforeCore)
            {
                AppLog.Write("quit: before the core started");
                Environment.Exit(0);
            }
            if (busy && waitedSecs < 150)
            {
                if (waitedSecs % 10 == 0) AppLog.Write($"launch: the previous run is still quitting; waiting ({waitedSecs} s so far)");
                var timer = ui.CreateTimer();
                timer.Interval = TimeSpan.FromSeconds(1);
                timer.IsRepeating = false;
                timer.Tick += (_, _) => StartCore(dir, waitedSecs + 1);
                timer.Start();
                return;
            }
            Fatal(dir, error);
            return;
        }
        Core = t.Result;
        started = true;
        AppLog.Write($"launch: core ready after {ms} ms");
        // All of this is one turn of the UI thread: each step is named for a stall's line and timed for the log.
        var steps = new List<string>();
        void Step(string doing, Action work)
        {
            var c = System.Diagnostics.Stopwatch.StartNew();
            HangWatch.Doing = doing;
            work();
            HangWatch.Doing = "";
            steps.Add($"{doing} {c.ElapsedMilliseconds}");
        }
        Notifier = new Notifier(this, Core, ui);
        Prompts = new Prompts(this);
        Banner.Changed += () => LibraryWindowIfCreated?.Pane.Refresh();
        Core.SetListener(new Listener(this));
        PowerWatch.Start(Core);
        NetWatch.Start(Core);
        Step("the library window", () => CreateLibrary(dir));
        Updater = new Updater(this, Core);
        Updater.Changed += () => flyout?.UpdateChanged();
        // On-demand folders over Cloud Files (item 6); in an isolated run the core hosts only the folders inside its own
        // state folder.
        Step("the cloud host", () => { Cloud = new CloudHost(this); Cloud.Start(); });
        Step("the settings", ApplySettings);
        Step("the flyout", () => flyout = new FlyoutWindow(this));
        TaskbarWatch.Start(flyout!.Hwnd, TaskbarCreated);
        // The plain icon of the start gives way to the full one (its menu and flyout): one icon, never two.
        tray?.Dispose();
        tray = null;
        Step("the tray icon", CreateTray);
        iconTimer = ui.CreateTimer();
        iconTimer.IsRepeating = false;
        iconTimer.Tick += (_, _) => UpdateIcon();
        Step("the icon", UpdateIcon);
        // A theme change redraws the icon at once (it fires off the UI thread).
        uiSettings.ColorValuesChanged += (_, _) => ui.TryEnqueue(() =>
        {
            drawn = null;
            iconTimer.Stop();
            UpdateIcon();
        });
        // What arrived while the core started (a second launch's links, QA commands, Show), in order.
        var waiting = pending.ToArray();
        pending.Clear();
        if (waiting.Length > 0) AppLog.Write($"launch: {waiting.Length} waited for the core");
        foreach (var run in waiting)
        {
            run();
        }
        if (Program.Routes.Length > 0)
        {
            Open(Program.Routes);
        }
        // Started by a click on a notification while the app was not running: what it opens.
        if (Program.NoticeClick is string click)
        {
            Notifier.Clicked(click);
        }
        // The files already in the watch folders, offered once ever.
        Prompts.Backlog();
        AppLog.Write($"launch: on the UI thread, ms: {string.Join(", ", steps)}");
    }

    /// <summary>The window on a transfer: its place, the row selected, Get Info on it (a notice's click, the Mac's showMain(nil, id)).</summary>
    internal void ShowTransfer(string id)
    {
        if (!Ready(() => ShowTransfer(id))) return;
        ShowMainWindow();
        Library.RevealRow(id);
        LibraryWindow.Pane.Refresh();
        OpenInfo(id);
    }

    /// <summary>
    /// The core could not start (a damaged state, a folder it refuses, …): the app never runs on without it. As the Mac's
    /// fatal: an alert with the core's sentence, Quit, and the core's ways out (<c>recovery_actions</c>: Go Back to the
    /// kept version, Install the Release; neither touches the data); the chosen one is started, then the app ends. An
    /// isolated run (QA, tests) shows no window nobody could answer: it logs what it would offer and ends.
    /// </summary>
    void Fatal(string dir, string error)
    {
        // The window's words need no core (it is what failed).
        var texts = TrsFfiMethods.StartFailureTexts();
        AppLog.Write($"launch refused: {error}");
        RecoveryAction[] actions;
        try { actions = TrsFfiMethods.RecoveryActions(dir, Updater.InstallDir, Updater.Repo, Updater.RelaunchArgs); }
        catch (Exception e) { AppLog.Write($"recovery actions failed: {e.Message}"); actions = []; }
        AppLog.Write($"launch: recovery offered: {(actions.Length == 0 ? "none" : string.Join(", ", actions.Select(a => a.Title)))}");
        if (Program.StateDir is not null)
        {
            Console.Error.WriteLine($"{texts.Title}: {error}");
            Environment.Exit(2);
        }
        tray?.Dispose();
        tray = null;
        var w = new Window { Title = texts.Title };
        var panel = new StackPanel { Margin = new Thickness(24), Spacing = 12, MaxWidth = 520 };
        panel.Children.Add(new TextBlock { Text = texts.Title, FontSize = 18, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(new TextBlock { Text = error, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        var quit = new Button { Content = texts.QuitTitle, MinWidth = 80, Style = (Style)Resources["AccentButtonStyle"] };
        ToolTipService.SetToolTip(quit, texts.QuitHelp);
        quit.Click += (_, _) => Environment.Exit(1);
        buttons.Children.Add(quit);
        foreach (var a in actions)
        {
            var b = new Button { Content = a.Title, MinWidth = 80 };
            ToolTipService.SetToolTip(b, a.Help);
            var command = a.Command;
            b.Click += (_, _) =>
            {
                AppLog.Write($"launch: recovery chosen: {a.Title}");
                Updater.Start(command);
                Environment.Exit(1);
            };
            buttons.Children.Add(b);
        }
        panel.Children.Add(buttons);
        w.Content = panel;
        w.Closed += (_, _) => Environment.Exit(1);
        w.AppWindow.Resize(new Windows.Graphics.SizeInt32(600, 280));
        w.Activate();
    }

    /// <summary>True when the core is there; else <paramref name="run"/> waits until it is (the UI thread never waits).</summary>
    bool Ready(Action run)
    {
        if (started)
        {
            return true;
        }
        pending.Add(run);
        return false;
    }

    /// <summary>
    /// What a command line or a clicked link opens (the core's LinkRoute, from command_line_routes or route_link): things
    /// to add go to <see cref="Add"/> together; each other route opens its window.
    /// </summary>
    internal void Open(LinkRoute[] routes)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Open(routes));
            return;
        }
        if (!Ready(() => Open(routes)))
        {
            return;
        }
        var sources = routes.OfType<LinkRoute.AddSource>().Select(r => r.Source).ToArray();
        if (sources.Length > 0)
        {
            Add(sources);
        }
        foreach (var route in routes.Where(r => r is not LinkRoute.AddSource))
        {
            AppLog.Write($"open: {route.GetType().Name}");
            switch (route)
            {
                case LinkRoute.JoinChannel j:
                    ShowMainWindow();
                    NewChannelDialog.Show(this, j.Link);
                    break;
                case LinkRoute.PairDevice p:
                    // Settings › Devices, Pair with a Link… filled in.
                    ShowPlace(new LibraryPlace.Settings());
                    // On the Devices tab, where its section is.
                    LibraryWindow.Pane.SettingsPageShown?.Show(SettingsTab.Devices);
                    LibraryWindow.Pane.SettingsPageShown?.Devices.ShowPaste(p.Link);
                    break;
                case LinkRoute.ShowFolder f:
                    ShowPlace(new LibraryPlace.FolderSync());
                    LibraryWindow.Pane.SyncPage?.Choose(f.FolderId);
                    break;
                case LinkRoute.ShareFolder sf:
                    ShowPlace(new LibraryPlace.FolderSync());
                    LibraryWindow.Pane.SyncPage?.Choose(sf.FolderId);
                    ShareDialog.Show(this, sf.FolderId);
                    break;
                case LinkRoute.SharePath sp:
                    // A folder not synced yet: its Share dialog syncs it with the first link.
                    ShareDialog.ShowNew(this, sp.Path);
                    break;
                case LinkRoute.AddFolder af:
                    ShowPlace(new LibraryPlace.FolderSync());
                    AddFolderDialog.Show(this, af.Link);
                    break;
                case LinkRoute.Problem problem:
                    Alert(new SyncAlert(problem.Title, problem.Text));
                    break;
            }
        }
    }

    /// <summary>
    /// A development build launched three times in a row without starting fine goes back to the app kept before it
    /// (SPEC 12A, Recovery): the core says so before it starts, since it may be what fails. The command it returns is
    /// started detached (it quits nothing: this launch ends instead of starting) and puts the kept app back. A release
    /// passes no version of its own: the guard acts only for a development build.
    /// </summary>
    static bool GoBackAfterFailedLaunches(string dir)
    {
        InstallCommand? c;
        try
        {
            c = TrsFfiMethods.LaunchGuard(dir, Updater.DevVersion ?? "", Updater.InstallDir, Updater.RelaunchArgs);
        }
        catch (Exception e)
        {
            AppLog.Write($"launch guard failed: {e.Message}");
            return false;
        }
        if (c is null)
        {
            return false;
        }
        AppLog.Write("launch: three launches without starting fine; going back to the kept app");
        if (!Updater.Start(c))
        {
            return false;
        }
        Environment.Exit(0);
        return true;
    }

    /// <summary>
    /// Adds opened .torrent files and clicked links (the Mac's application(_:open:)): each through the core, which holds
    /// it for its countdown or a choice as the settings say; the menu opens to show it.
    /// </summary>
    internal void Add(string[] sources)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Add(sources));
            return;
        }
        // In order, off the UI thread; the menu shows what was added (nothing added, nothing to show: the alert said why).
        foreach (var source in sources)
        {
            bool added = true;
            CoreCalls.InOrder("add", () => Core.AddSource(source),
                () => { if (added) flyout?.ShowOpen(); },
                e => { added = false; Alert(TrsFfiMethods.AddFailed(e)); });
        }
    }

    /// <summary>
    /// The visual QA run: the simulated library (never the user's data) in a temporary state folder, every view drawn
    /// to <paramref name="dir"/>, then the app ends.
    /// </summary>
    void RenderSnapshots(string dir, bool light)
    {
        var state = Path.Combine(Path.GetTempPath(), $"seedwell-render-{Environment.ProcessId}");
        Core = new Core(new CoreOptions(state, true, false, null));
        // Beside the pictures (the state folder goes at the end): every stall is noted there, also those short of failing.
        AppLog.Open(dir);
        // A render that hangs fails (the core's fail_ms), as a QA run does.
        _ = new HangWatch(ui, () => Core, testRun: true);
        CreateLibrary(state);
        // The menu reads its update row from it; a development build never checks by itself, so nothing goes out.
        Updater = new Updater(this, Core);
        flyout = new FlyoutWindow(this);
        ui.TryEnqueue(async () =>
        {
            int code = 0;
            try
            {
                await new SnapshotRenderer(this, flyout, dir, light ? "light" : "dark").RenderAll();
            }
            catch (Exception e)
            {
                Console.Error.WriteLine(e);
                File.WriteAllText(Path.Combine(dir, $"error-{(light ? "light" : "dark")}.txt"), e.ToString());
                code = 1;
            }
            flyout.Close();
            await Task.Run(() => Core.Shutdown(10));
            try { Directory.Delete(state, true); } catch { }
            if (HangWatch.Failed && code == 0)
            {
                File.WriteAllText(Path.Combine(dir, $"error-{(light ? "light" : "dark")}.txt"), HangWatch.FailedLine ?? "the UI thread stalled");
                code = 3;
            }
            Environment.Exit(code);
        });
    }

    /// <summary>
    /// The library with the folds remembered from last time (a convenience: losing the file only unfolds the groups).
    /// </summary>
    void CreateLibrary(string stateDir)
    {
        foldsFile = Path.Combine(stateDir, "app-windows-folds.txt");
        string[] folded = [];
        try { folded = File.ReadAllLines(foldsFile).Where(l => l.Length > 0).ToArray(); } catch { }
        Library = new Library(Core, folded, new Clock());
        Library.SetHostWindows(HostWindows);
        ChatDialogs = new ChatDialogs(this);
        Files = new FilesStore(this, FileListPlace.Window);
        MenuFiles = new FilesStore(this, FileListPlace.Menu);
        Files.Changed += id => LibraryWindowIfCreated?.Pane.FilesChanged(id);
        Files.Edited += () => LibraryWindowIfCreated?.Pane.Refresh();
        MenuFiles.Changed += id => flyout?.FilesChanged(id);
        MenuFiles.Edited += () => LibraryWindowIfCreated?.Pane.Refresh();
        // The file rows' Play follows the host set (with HostWindow.Player: Play and Play With).
        Files.Lists.SetHostWindows(HostWindows);
        MenuFiles.Lists.SetHostWindows(HostWindows);
        Favorites = new FavoriteEditing(this);
        Favorites.Changed += () => LibraryWindowIfCreated?.Pane.FavoritesChanged();
        Choices = new ChoiceStore(this);
        Choices.Changed += () => LibraryWindowIfCreated?.Pane.ChoicesChanged();
        StartingNotifier = new StartingWindow(this, Choices);
        // The first look after start: torrents an earlier run left waiting.
        StartingNotifier.Refresh(launch: true);
    }

    /// <summary>The on-demand folders' host (Cloud Files), when it runs.</summary>
    internal CloudHost? Cloud { get; private set; }

    /// <summary>Favorite trackers: the one inline edit and the Remove from Favorites question (Part F).</summary>
    internal FavoriteEditing Favorites { get; private set; } = null!;

    /// <summary>The one chooser per torrent every surface shares, and the starting notifier at the tray (Part E).</summary>
    internal ChoiceStore Choices { get; private set; } = null!;
    internal StartingWindow? StartingNotifier { get; private set; }

    /// <summary>Held torrents whose checklist is open under their row (the window's list in selection mode).</summary>
    internal HashSet<string> HeldOpen { get; } = [];

    /// <summary>A held row's triangle: its checklist opens under it (the shared chooser, loaded off the UI thread), or closes.</summary>
    internal void ToggleHeld(string id)
    {
        if (!HeldOpen.Remove(id))
        {
            HeldOpen.Add(id);
            _ = Choices.Load(id);
        }
        LibraryWindowIfCreated?.Pane.ChoicesChanged();
    }

    /// <summary>The window on a torrent, selected and in view, its list open (the notifier's title).</summary>
    internal void ShowTorrent(string id)
    {
        if (!Ready(() => ShowTorrent(id))) return;
        ShowMainWindow();
        Library.RevealRow(id);
        if (TakeSnapshot().Downloading.Any(r => r.Id == id && r.AwaitingChoice))
        {
            if (HeldOpen.Add(id)) _ = Choices.Load(id);
        }
        else
        {
            Files.Expand(id);
        }
        LibraryWindow.Pane.Refresh();
    }

    /// <summary>
    /// A torrent waits for a choice (the core's event): the notifier looks first; when it shows, it is the notice and
    /// holds the checklist (SPEC 8.6); a choice someone asked for opens the chooser.
    /// </summary>
    internal void NeedsChoice(TransferRow row)
    {
        var requested = StartingNotifier?.ConsumeRequest(row.Id) == true;
        StartingNotifier?.Refresh();
        Notifier?.Event(new NoticeEvent.NeedsChoice(row, requested, StartingNotifier?.IsShown == true));
    }

    /// <summary>The file lists under the window's rows, and the tray's checklist (the core's FileLists, one per place).</summary>
    internal FilesStore Files { get; private set; } = null!;
    internal FilesStore MenuFiles { get; private set; } = null!;

    /// <summary>The QA command files: what it does is what a click does; it answers with the lists as drawn.</summary>
    async Task<string> QaFiles(string[] w, Func<int, string> arg, RowFinder row, LibraryPane pane)
    {
        string? Torrent() => row(arg(2), out _)?.Id;
        uint? Node(string id) => Files.Lists.Find(id, arg(3)) ?? MenuFiles.Lists.Find(id, arg(3));
        string done = "";
        switch (arg(1))
        {
            case "toggle" when Torrent() is string t: Files.Toggle(t); await Task.Delay(800); break;
            case "click" when Torrent() is string t && Node(t) is uint n:
                if (pane.ListShown is LibraryList l) l.Files.Click(t, n); else done = "no list; ";
                break;
            case "check" when Torrent() is string t && Node(t) is uint n: Files.ToggleCheck(t, n); break;
            case "priority" when Torrent() is string t && Node(t) is uint n && Enum.TryParse<FilePriority>(arg(4), out var p): Files.SetPriority(t, n, p); break;
            case "all" when Torrent() is string t: Files.SelectAll(t, arg(3) == "on"); break;
            case "filter" when Torrent() is string t: Files.SetFilter(t, string.Join(' ', w.Skip(3))); break;
            case "flush" when Torrent() is string t: Files.Flush(t); await Task.Delay(800); break;
            case "menu" when Torrent() is string t:
                if (flyout is null) { done = "no tray; "; break; }
                flyout.QaToggleFiles(t);
                break;
            case "menu-check" when Torrent() is string t && Node(t) is uint n: MenuFiles.ToggleCheck(t, n); break;
            case "describe" or "": break;
            default: done = "no such row or file; "; break;
        }
        string Rows(FileListView v) => string.Join(", ", v.Rows.Take(30).Select(r => $"{r.File.Name}:{r.File.Check}:{r.File.PriorityLabel}{(r.Selected ? "*" : "")}"));
        var lists = string.Join(" | ", Files.Views.Values.Select(v =>
            $"{v.Status ?? v.Header?.Summary ?? ""} (filter {(v.Header?.Filter is null ? "none" : $"\"{v.Header.Filter.Text}\"")}, pending {Files.Lists.HasPending(v.TorrentId)}) rows {v.Rows.Length} [{Rows(v)}]"));
        var menu = string.Join(" | ", MenuFiles.Expanded.Select(id => MenuFiles.Lists.MenuView(id) is FileMenuView m ? $"{m.Footer} rows {m.Rows.Length} [{string.Join(", ", m.Rows.Select(r => $"{r.Name}:{r.Check}"))}]" : "not read"));
        return $"{done}open {Files.Expanded.Length}, selected {Files.Selection?.Nodes.Length ?? 0}; lists {lists}; menu {menu}";
    }

    internal delegate ListRow? RowFinder(string part, out string group);

    /// <summary>The QA commands favorite and tracker-info: what each does is what a click does; they answer with what is drawn.</summary>
    async Task<string> QaFavorite(string verb, string[] w, Func<int, string> arg)
    {
        var f = Favorites;
        string? Tracker(string part) => TakeSnapshot().Trackers.FirstOrDefault(t => t.Name.Contains(part, StringComparison.OrdinalIgnoreCase))?.Id;
        // Types into the one field as a user does, then commits (Return).
        async Task Type(FavoriteTarget target, string text)
        {
            // The field drawn for it (the sidebar's, the page's) once the redraw placed it.
            await Task.Delay(500);
            if (!Equals(f.View.Target, target) || !f.QaType(text)) return;
            await Task.Delay(200);
            f.Commit();
            await Task.Delay(1000);
        }
        // favorite <verb> <tracker> …, tracker-info <tracker> [<verb> …].
        var id = Tracker(verb == "tracker-info" ? arg(1) : arg(2));
        switch (verb, arg(1))
        {
            case ("favorite", "save") when id is string t: f.SaveAsFavorite(t); await Task.Delay(1200); break;
            case ("favorite", "rename") when id is string t:
                f.BeginRename(t);
                await Type(new FavoriteTarget.SidebarName(t), string.Join(' ', w.Skip(3)));
                break;
            case ("favorite", "remove") when id is string t: f.AskRemove(t); await Task.Delay(500); break;
            case ("favorite", "confirm"): f.ConfirmRemoval(); await Task.Delay(1200); break;
            case ("favorite", "cancel"): f.CancelRemoval(); break;
            case ("tracker-info", _) when id is string t:
                Library.SelectPlace(new LibraryPlace.Tracker(t));
                Library.SetTrackerInfo(true);
                LibraryWindow.Pane.Refresh();
                await Task.Delay(1200);
                var page = LibraryWindow.Pane.TrackerPage;
                switch (arg(2))
                {
                    case "limits" when ulong.TryParse(arg(3), out var down) && ulong.TryParse(arg(4), out var up): f.SetLimits(t, down, up); await Task.Delay(1200); break;
                    case "pex": f.Run(t, new TrackerInfoCommand.SetPex(arg(3) == "on")); await Task.Delay(1000); break;
                    case "add":
                        f.Run(t, new TrackerInfoCommand.AddAddress());
                        await Task.Delay(500);
                        if (f.View.Target is FavoriteTarget target) await Type(target, string.Join(' ', w.Skip(3)));
                        break;
                }
                page?.Load();
                await Task.Delay(800);
                break;
            case (_, "status"): break;
            default: return "no such command or tracker";
        }
        var v = f.View;
        var m = LibraryWindow.Pane.TrackerPage?.Model;
        var info = m is null ? "none" : $"{m.Title} favorite {m.Favorite}, header {(m.Header is null ? "none" : m.Header.Stats.Length.ToString())}, now [{string.Join("; ", m.Now.Select(x => $"{x.Label}: {x.Value}"))}], limits [{string.Join("; ", m.Limits.Rows.Select(x => $"{x.Label} {x.Limit} {x.Line}"))}], folders {(m.Location.Record is null ? "none" : "editable")}, addresses [{string.Join("; ", m.Addresses.Rows.Select(x => x.Text))}]";
        var trackers = string.Join(", ", TakeSnapshot().Trackers.Select(x => $"{x.Name}{(x.Favorite ? "*" : "")}"));
        return $"editor target {v.Target?.GetType().Name ?? "none"}, hint \"{v.Hint ?? ""}\", note \"{v.Note ?? ""}\", removal \"{v.Removal?.Message ?? ""}\", busy {v.Busy}; trackers [{trackers}]; info {info}";
    }

    /// <summary>The QA commands chooser, held and starting: what each does is what a click does; they answer with what is drawn.</summary>
    async Task<string> QaChoice(string verb, string[] w, Func<int, string> arg, RowFinder row)
    {
        // A torrent by part of its name: the window's rows, else the notifier's blocks.
        string? Torrent(string part) => row(part, out _)?.Id
            ?? StartingNotifier?.Page.Blocks.FirstOrDefault(b => b.Title.Contains(part, StringComparison.OrdinalIgnoreCase))?.Id;
        string Page(string? id) => id is not null && Choices.Page(id) is ChooserPage p
            ? $"checked {p.CheckedCount}, rows {p.Rows.Length} [{string.Join(", ", p.Rows.Take(12).Select(r => $"{r.Title}:{r.Check}"))}], selected \"{p.SelectedLine}\", awaiting {p.Awaiting}, can start {p.CanConfirm}"
            : "no chooser";
        var rest = string.Join(' ', w.Skip(3));
        switch (verb, arg(1))
        {
            case ("chooser", "show") when Torrent(arg(2)) is string t:
                ChooserDialog.Show(this, t);
                for (int i = 0; i < 40 && ChooserDialog.Open?.TorrentId != t; i++) await Task.Delay(100);
                break;
            case ("chooser", "close"): ChooserDialog.Open?.Close(); await Task.Delay(300); break;
            case ("chooser", _) when ChooserDialog.Open is not ChooserDialog: return "no chooser open";
            case ("chooser", "toggle"):
                var cid = ChooserDialog.Open!.TorrentId;
                if (Choices.Loaded(cid)?.FindRow(string.Join(' ', w.Skip(2))) is string r) Choices.Edit(cid, c => c.Toggle(r)); else return "no such row";
                break;
            case ("chooser", "pick") when Enum.TryParse<ChooserPick>(arg(2), out var pick): Choices.Edit(ChooserDialog.Open!.TorrentId, c => c.ApplyPick(pick)); break;
            case ("chooser", "category") when TrsFfiMethods.CategoryChoices(Category.Other).FirstOrDefault(c => string.Equals(c.Title, arg(2), StringComparison.OrdinalIgnoreCase)) is CategoryChoice cc:
                Choices.ChangeCategory(ChooserDialog.Open!.TorrentId, cc.Category); await Task.Delay(800); break;
            case ("chooser", "start"): Choices.Confirm(ChooserDialog.Open!.TorrentId); await Task.Delay(1000); break;
            case ("chooser", "cancel"): Choices.Cancel(ChooserDialog.Open!.TorrentId, arg(2) == "keep"); await Task.Delay(1000); break;
            case ("held", "toggle") when Torrent(arg(2)) is string t:
                ToggleHeld(t);
                for (int i = 0; i < 30 && Choices.Page(t) is null; i++) await Task.Delay(100);
                LibraryWindow.Pane.ChoicesChanged();
                break;
            case ("starting", "show"): StartingNotifier?.QaShow(); break;
            case ("starting", "close"): StartingNotifier?.QaClose(); break;
            case ("starting", "shift"): Choosing.QaShift = arg(2) switch { "on" => true, "off" => false, _ => null }; break;
            case ("starting", "chevron") when Torrent(arg(2)) is string t: StartingNotifier?.QaChevron(t); await Task.Delay(800); break;
            case ("starting", var action) when action is "ok" or "start-now" or "cancel" or "title" or "category" or "all" or "none" or "open" or "toggle" && Torrent(arg(2)) is string t:
                StartingNotifier?.QaAction(action, t, rest);
                await Task.Delay(800);
                break;
            case (_, "status" or ""): break;
            default: return "no such command or row";
        }
        StartingNotifier?.Refresh();
        var open = ChooserDialog.Open?.TorrentId;
        var held = string.Join(" | ", HeldOpen.Select(id => $"{id[..8]}: {Page(id)}"));
        return $"chooser {(open is null ? "none" : $"{open[..8]}: {Page(open)}")}; held [{held}]; {StartingNotifier?.QaSummary ?? "no notifier"}";
    }

    /// <summary>The window on a torrent with its file list open (the tray checklist's "Show All in Seedwell…").</summary>
    internal void ShowFiles(string id)
    {
        if (!Ready(() => ShowFiles(id))) return;
        ShowMainWindow();
        Library.RevealRow(id);
        Files.Expand(id);
        LibraryWindow.Pane.Refresh();
    }

    /// <summary>Folds or unfolds a group and remembers the folds.</summary>
    internal void ToggleFold(string key)
    {
        var folded = Library.ToggleFold(key);
        try { File.WriteAllLines(foldsFile, folded); } catch { }
    }

    /// <summary>
    /// The settings the host implements itself (the Mac's apply(_:)): read at start and again whenever config.toml
    /// changes (<see cref="SettingsChanged"/>).
    /// </summary>
    void ApplySettings()
    {
        // A value the core could not use took its backup or safe value: the banner says which.
        try
        {
            var notes = Core.SettingsProblems();
            foreach (var problem in notes)
            {
                AppLog.Write($"settings problem: {problem}");
            }
            Banner.Notes(notes);
        }
        catch { }
        var general = Core.SettingsLoad().General;
        sleep.WhileDownloading = general.PreventSleepWhileDownloading;
        sleep.ForAgents = general.KeepAwakeForAgents;
        if (Program.StateDir is null)
        {
            try { LoginItem.Apply(general.LaunchAtLogin); } catch { }
        }
        // The update switches may have changed outside Settings (the CLI, an agent, a hand edit): the next check again.
        Updater?.Schedule();
    }

    /// <summary>config.toml changed (the app, an agent, the CLI or a hand edit): apply its settings again.</summary>
    internal void SettingsChanged() => ui.TryEnqueue(() =>
    {
        ApplySettings();
        // Settings open: read again (an agent, the CLI or a hand edit changed a value).
        LibraryWindowIfCreated?.Pane.SettingsPageShown?.Reload();
    });

    void CreateTray()
    {
        // The right-click menu, made anew each time it opens: the core's app commands (File, then Transfers), then Open
        // and Quit, worded by the menu model.
        var menu = new MenuFlyout();
        menu.Opening += (_, _) =>
        {
            menu.Items.Clear();
            if (!started) return;
            foreach (var item in TrayMenuItems())
            {
                menu.Items.Add(item);
            }
        };
        if (started)
        {
            foreach (var item in TrayMenuItems())
            {
                menu.Items.Add(item);
            }
        }

        var icon = new TaskbarIcon
        {
            ToolTipText = "Seedwell",
            Icon = new System.Drawing.Icon(Path.Combine(AppContext.BaseDirectory, "Seedwell.ico")),
            ContextFlyout = menu,
            ContextMenuMode = ContextMenuMode.SecondWindow,
            NoLeftClickDelay = true,
            LeftClickCommand = new Command(() => { if (Ready(() => flyout?.Toggle())) flyout?.Toggle(); }),
        };
        // No notification area (Explorer not running or restarting, a session without a shell): the app runs on
        // without its icon (the core, the control socket and the window all work) and makes it when the taskbar comes.
        try
        {
            if (failTrayOnce)
            {
                failTrayOnce = false;
                throw new InvalidOperationException("simulated (SEEDWELL_QA_NO_TRAY)");
            }
            icon.ForceCreate();
            tray = icon;
            drawn = null;
            AppLog.Write("tray: icon created");
        }
        catch (Exception e)
        {
            AppLog.Write($"tray: no icon ({e.GetType().Name}: {e.Message}); running without it until the taskbar is created");
            try { icon.Dispose(); } catch { }
            tray = null;
        }
    }

    /// <summary>The tray's right-click menu: each app menu's entries, a separator between them, then Open and Quit.</summary>
    internal List<MenuFlyoutItemBase> TrayMenuItems()
    {
        var items = new List<MenuFlyoutItemBase>();
        AppMenu[] menus;
        try { menus = Core.AppCommands(); } catch (Exception e) { AppLog.Write($"app commands failed: {e.Message}"); menus = []; }
        foreach (var m in menus)
        {
            if (items.Count > 0)
            {
                items.Add(new MenuFlyoutSeparator());
            }
            foreach (var e in m.Entries)
            {
                if (e is AppMenuEntry.Item it)
                {
                    MenuFlyoutItem mi = it.Checked ? new ToggleMenuFlyoutItem { IsChecked = true } : new MenuFlyoutItem();
                    mi.Text = it.Title;
                    mi.IsEnabled = it.Enabled;
                    if (it.Shortcut is KeyShortcut k)
                    {
                        mi.KeyboardAcceleratorTextOverride = k.Label;
                    }
                    var command = it.Command;
                    mi.Click += (_, _) => RunAppCommand(command);
                    items.Add(mi);
                }
                else
                {
                    items.Add(new MenuFlyoutSeparator());
                }
            }
        }
        MenuModel model;
        try { model = TrsFfiMethods.MenuModel(TakeSnapshot(), Core.SyncSnapshot(), Arrivals, Now(), Updater?.Updates.AvailableVersion(), HostWindows); }
        catch { model = new MenuModel(null, [], null, null, null, "Open Seedwell…", "Quit", 1000, ""); }
        var open = new MenuFlyoutItem { Text = model.OpenTitle };
        open.Click += (_, _) => ShowMainWindow();
        var quit = new MenuFlyoutItem { Text = model.QuitTitle };
        quit.Click += (_, _) => Quit();
        if (items.Count > 0)
        {
            items.Add(new MenuFlyoutSeparator());
        }
        items.Add(open);
        items.Add(new MenuFlyoutSeparator());
        items.Add(quit);
        return items;
    }

    /// <summary>What an app command does (the tray menu, the window's shortcuts, QA): a core call or a window of the app's.</summary>
    internal void RunAppCommand(AppCommand command)
    {
        AppLog.Write($"app command {command}");
        switch (command)
        {
            case AppCommand.AddTorrentFile:
                ShowMainWindow();
                ChooseTorrentFile();
                break;
            case AppCommand.AddLink:
                ShowMainWindow();
                Library.ShowAddSheet();
                LibraryWindow.Pane.Refresh();
                break;
            case AppCommand.PauseAll: CoreCalls.InOrder("pause all", Core.PauseAll, Refreshed); return;
            case AppCommand.ResumeAll: CoreCalls.InOrder("resume all", Core.ResumeAll, Refreshed); return;
            case AppCommand.SetLimitSpeed limit: CoreCalls.InOrder("limit speed", () => Core.SetAltSpeed(limit.On), Refreshed); return;
        }
        LibraryWindowIfCreated?.Pane.Refresh();
    }

    /// <summary>
    /// The taskbar was created (Explorer started late or restarted; its TaskbarCreated message, or the QA command):
    /// an app without its icon makes it now. An icon that exists is put back by H.NotifyIcon itself.
    /// </summary>
    internal void TaskbarCreated()
    {
        if (tray is null && !quitting)
        {
            CreateTray();
            if (started) UpdateIcon();
        }
    }

    /// <summary>
    /// The tray icon from the core's icon model: redrawn only when its look changes, asked again after the model's
    /// <c>next_refresh_ms</c> (sooner while the flyout is open).
    /// </summary>
    void UpdateIcon()
    {
        if (quitting)
        {
            return;
        }
        uint next = 5000;
        try
        {
            var snapshot = TakeSnapshot();
            // The icon's refresh is also when the power needs are asked (the Mac's statusMenu.onRefresh).
            bool downloading = TrsFfiMethods.PowerNeeds(snapshot).Downloading;
            sleep.Update(downloading, Core.AgentsAwake() is not null);
            // Downloads stopped: an update that waited for that may install now (the core decides).
            if (wasDownloading && !downloading)
            {
                Updater.InstallIfDue();
            }
            wasDownloading = downloading;
            var model = IconState.Icon(snapshot, Now(), flyout?.IsOpen == true);
            // Without an icon (no taskbar yet) only the drawing waits; the power needs above go on.
            if (tray is not null)
            {
                if (drawn is null || !drawn.Equals(model.Look))
                {
                    var old = tray.Icon;
                    tray.Icon = TrayIcon.Draw(model.Look, TrayIcon.Size());
                    old?.Dispose();
                    drawn = model.Look;
                }
                tray.ToolTipText = model.AccessibilityLabel;
            }
            next = model.NextRefreshMs;
        }
        catch (Exception e)
        {
            // The core is stopping or busy: try again later. Said once per kind, so a lasting failure is seen.
            if (iconError != e.GetType().Name)
            {
                iconError = e.GetType().Name;
                AppLog.Write($"icon refresh failed: {e.GetType().Name}: {e.Message}");
            }
        }
        iconTimer.Interval = TimeSpan.FromMilliseconds(next);
        iconTimer.Start();
    }

    /// <summary>An agent's activity: the icon (its dot while a request waits) at once, and the request toasts no longer waiting go.</summary>
    internal void AgentActivity()
    {
        iconTimer?.Stop();
        UpdateIcon();
        Notifier?.WithdrawStale();
    }

    /// <summary>The flyout opened or closed: the icon's refresh time depends on it.</summary>
    internal void FlyoutChanged()
    {
        iconTimer?.Stop();
        UpdateIcon();
    }

    /// <summary>Get Info for a row: the library window shows it on that row (the Library holds which one is open).</summary>
    internal void OpenInfo(string rowId)
    {
        ShowMainWindow();
        Library.SetInfo(rowId);
        LibraryWindow.Pane.Refresh();
    }

    /// <summary>The library window if it exists (the visual QA run lays it out without creating it).</summary>
    internal MainWindow? LibraryWindowIfCreated => main;

    /// <summary>The library window, created on first use (the visual QA run draws it too).</summary>
    internal MainWindow LibraryWindow => main ??= new MainWindow(new LibraryPane(this, Library));

    /// <summary>Shows the library window (from the menu, or when a second launch asks for it).</summary>
    public void ShowMainWindow()
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(ShowMainWindow);
            return;
        }
        // The window needs the library (the core's): before the core is there, it opens once it is.
        if (!Ready(ShowMainWindow))
        {
            return;
        }
        flyout?.Hide();
        main = LibraryWindow;
        main.ShowAndActivate();
        // A download that arrived while the window was closed.
        Library.TakeArrival(true, main.Pane.Context());
        main.Pane.Refresh();
        AppLog.Write($"window shown, in front: {main.IsInFront}");
    }

    /// <summary>The library window on a place (the flyout's "+N more" shows Downloads, its Sharing line Sharing).</summary>
    internal void ShowPlace(LibraryPlace place)
    {
        ShowMainWindow();
        Library.SelectPlace(place);
        LibraryWindow.Pane.Refresh();
    }

    /// <summary>
    /// Add Torrent File… (Ctrl+O, the add sheet's Choose .torrent File…): the Open dialog for .torrent files, each added
    /// as an opened one (the Mac's chooseTorrentFile). A QA run opens no dialog: the log says it would have.
    /// </summary>
    internal async void ChooseTorrentFile()
    {
        if (Quiet.On)
        {
            AppLog.Write("open dialog: .torrent files");
            return;
        }
        var picker = new Windows.Storage.Pickers.FileOpenPicker();
        picker.FileTypeFilter.Add(".torrent");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(LibraryWindow));
        try
        {
            var files = await picker.PickMultipleFilesAsync();
            if (files.Count > 0)
            {
                Add(files.Select(f => f.Path).ToArray());
            }
        }
        catch (Exception e)
        {
            AppLog.Write($"open dialog failed: {e.Message}");
        }
    }

    /// <summary>
    /// A folder the user picks (Move To…, the folder dialogs' Choose… with the core's <paramref name="asked"/>: its prompt
    /// as the button; Windows' picker has no message or start path), or null; a QA run takes <see cref="QaFolder"/>.
    /// </summary>
    internal async Task<string?> PickFolder(SyncFolderPicker? asked = null)
    {
        if (Quiet.On)
        {
            AppLog.Write($"folder dialog{(asked is null ? "" : $" \"{asked.Prompt}\"")}: {QaFolder ?? "(none)"}");
            return QaFolder;
        }
        var picker = new Windows.Storage.Pickers.FolderPicker();
        picker.FileTypeFilter.Add("*");
        if (asked is not null && asked.Prompt.Length > 0)
        {
            picker.CommitButtonText = asked.Prompt;
        }
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(LibraryWindow));
        try
        {
            return (await picker.PickSingleFolderAsync())?.Path;
        }
        catch (Exception e)
        {
            AppLog.Write($"folder dialog failed: {e.Message}");
            return null;
        }
    }

    /// <summary>The library window drawn again (after a command returned).</summary>
    internal void Refreshed() => LibraryWindowIfCreated?.Pane.Refresh();

    /// <summary>Runs <paramref name="action"/> on the UI thread (now when already there).</summary>
    internal void Dispatch(Action action)
    {
        if (ui.HasThreadAccess) action(); else ui.TryEnqueue(() => action());
    }

    /// <summary>Where to save a file named <paramref name="suggested"/> (Save As…), or null; a QA run takes <see cref="QaSaveFile"/>.</summary>
    internal async Task<string?> PickSaveFile(string suggested)
    {
        if (Quiet.On)
        {
            AppLog.Write($"save dialog \"{suggested}\": {QaSaveFile ?? "(none)"}");
            return QaSaveFile;
        }
        var picker = new Windows.Storage.Pickers.FileSavePicker { SuggestedFileName = Path.GetFileNameWithoutExtension(suggested) };
        var ext = Path.GetExtension(suggested);
        picker.FileTypeChoices.Add(ext.Length > 0 ? ext : "File", [ext.Length > 0 ? ext : "."]);
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(LibraryWindow));
        try { return (await picker.PickSaveFileAsync())?.Path; }
        catch (Exception e) { AppLog.Write($"save dialog failed: {e.Message}"); return null; }
    }

    /// <summary>Files the user picks to attach (the core's image extensions first, any file allowed), or none in a QA run.</summary>
    internal async Task<string[]> PickFiles(IEnumerable<string> extensions)
    {
        if (Quiet.On)
        {
            AppLog.Write("open dialog: attachments");
            return [];
        }
        var picker = new Windows.Storage.Pickers.FileOpenPicker();
        picker.FileTypeFilter.Add("*");
        foreach (var e in extensions) picker.FileTypeFilter.Add(e.StartsWith('.') ? e : $".{e}");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(LibraryWindow));
        try { return (await picker.PickMultipleFilesAsync()).Select(f => f.Path).ToArray(); }
        catch (Exception e) { AppLog.Write($"open dialog failed: {e.Message}"); return []; }
    }

    /// <summary>QA: the path the next save dialog answers with.</summary>
    internal string? QaSaveFile { get; set; }

    /// <summary>QA: the folder the next folder dialog answers with (the QA command pick-folder).</summary>
    internal string? QaFolder { get; set; }

    /// <summary>
    /// A quiet notification from the tray icon (no sound, held back in Do Not Disturb); logged, and not shown in a QA or
    /// render run. A balloon the shell refuses is logged: it must not end the app (it runs from the dispatcher's queue).
    /// </summary>
    internal void Notify(string title, string body)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Notify(title, body));
            return;
        }
        AppLog.Write($"notify: {title}: {body}");
        if (Quiet.On)
        {
            return;
        }
        try
        {
            tray?.ShowNotification(title, body, H.NotifyIcon.Core.NotificationIcon.None, sound: false, respectQuietTime: true);
        }
        catch (Exception e)
        {
            AppLog.Write($"notify: not shown ({e.Message})");
        }
    }

    /// <summary>
    /// An alert (the Mac's NSAlert): over the library window when it is open, otherwise in a small window of its own;
    /// its button is the core's alert_ok_title.
    /// </summary>
    internal void Alert(SyncAlert alert)
    {
        AppLog.Write($"alert: {alert.Title}: {alert.Text}");
        if (Quiet.On)
        {
            return;
        }
        if (main?.AppWindow.IsVisible == true && main.Content.XamlRoot is XamlRoot root)
        {
            var d = new ContentDialog
            {
                XamlRoot = root,
                Title = alert.Title,
                Content = new TextBlock { Text = alert.Text, TextWrapping = TextWrapping.Wrap },
                CloseButtonText = TrsFfiMethods.AlertOkTitle(),
                DefaultButton = ContentDialogButton.Close,
            };
            _ = d.ShowAsync();
            return;
        }
        var w = new Window { Title = alert.Title };
        var ok = new Button { Content = TrsFfiMethods.AlertOkTitle(), HorizontalAlignment = HorizontalAlignment.Right, MinWidth = 80, Style = (Style)Resources["AccentButtonStyle"] };
        ok.Click += (_, _) => w.Close();
        var panel = new StackPanel { Margin = new Thickness(24), Spacing = 12 };
        panel.Children.Add(new TextBlock { Text = alert.Title, FontSize = 18, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(new TextBlock { Text = alert.Text, TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(ok);
        Chrome.Apply(w, panel, alert.Title);
        w.SystemBackdrop = new Microsoft.UI.Xaml.Media.MicaBackdrop();
        w.AppWindow.Resize(new Windows.Graphics.SizeInt32(460, 250));
        w.Activate();
    }

    /// <summary>Stops the core (it sends its "stopped" announces and saves its state; at most 120 s) and ends.</summary>
    internal void Quit()
    {
        if (quitting)
        {
            return;
        }
        quitting = true;
        AppLog.Write("quit: begins");
        // A clean quit: this launch does not count toward a crash loop (SPEC 12A, Recovery).
        Updater?.Quitting();
        var clock = System.Diagnostics.Stopwatch.StartNew();
        iconTimer?.Stop();
        Cloud?.Stop();
        flyout?.Close();
        main?.CloseForQuit();
        tray?.Dispose();
        // On a thread of its own, never the pool's (docs/CROSS-PLATFORM.md §1): the engine's deadline bounds it.
        new Thread(() =>
        {
            var r = Core.Shutdown(120);
            AppLog.Write($"quit: shutdown confirmed {r.Confirmed}, abandoned {r.Abandoned}, finished {r.Finished}, {clock.ElapsedMilliseconds} ms");
            ui.TryEnqueue(Exit);
        }) { IsBackground = true, Name = "core shutdown" }.Start();
    }

    /// <summary>
    /// QA commands (<c>Seedwell.exe [--state-dir D] --qa ...</c> through the second-launch hand-off, run only when this
    /// instance was started with <c>--qa-control</c>, see <see cref="Program.QaControl"/>): each calls what the
    /// click or key it stands for calls, so tests drive the app without synthetic input; each is logged with the state
    /// after it. window | menu | place downloads|sharing|foldersync|settings|tracker &lt;id&gt; | rowclick &lt;title
    /// part&gt; [command] [shift] | key delete|return|escape|space|left|right|up|down|i|a [shift] [command] [list] |
    /// remove &lt;title part&gt; [files] | info &lt;title part&gt; | control &lt;title part&gt; &lt;command&gt; | confirm |
    /// cancel | add-sheet | add-file | move &lt;title part&gt; &lt;folder&gt; | menu-line more|sharing | sidebar
    /// &lt;channel&gt; &lt;command&gt; | agents &lt;channel&gt; yes|no | rendezvous use &lt;address&gt;|test|off|status |
    /// follow on|off | search &lt;text&gt; | update-check | update-install | update-status | taskbar-created | shot
    /// &lt;name&gt; [rendezvous] (the library window, or the Cloudflare section, to &lt;state&gt;\qa\&lt;name&gt;.png).
    /// </summary>
    internal void Qa(string[] words)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Qa(words));
            return;
        }
        if (!Ready(() => Qa(words)))
        {
            AppLog.Write($"qa {QaSaid(words)}: waits for the core");
            return;
        }
        _ = QaRun(words).ContinueWith(t =>
        {
            // A command that throws answers with its error (else the run waits for an answer that never comes).
            var e = t.Exception?.InnerException;
            AppLog.Write($"qa {QaSaid(words)}: failed: {e?.GetType().Name}: {e?.Message} {e?.StackTrace?.Split(Environment.NewLine).FirstOrDefault()?.Trim()}");
        }, TaskContinuationOptions.OnlyOnFaulted);
    }

    /// <summary>"On all my devices" and its "Read only" as a dialog's model draws them (QA).</summary>
    static string QaFollow(FollowLine? f, ReadOnlyLine? r) => $"all devices {(f is null ? "none" : $"{f.On} enabled {f.Enabled}")}, read only {(r is null ? "none" : $"{r.On} enabled {r.Enabled} line \"{r.Line}\"")}";

    /// <summary>A QA command as the log may say it: never what a credential field is given (qa\run.ps1's Qa says the same).</summary>
    static string QaSaid(string[] w) => w is ["proxy-window", "fill" or "custom" or "copied" or "last", ..] ? $"proxy-window {w[1]} ***" : string.Join(' ', w);

    async Task QaRun(string[] w)
    {
        string verb = w.Length > 0 ? w[0] : "";
        HangWatch.LastAction = $"qa {QaSaid(w)}";
        string Arg(int i) => w.Length > i ? w[i] : "";
        // A folder (or channel) named by its name or its id.
        string FolderId(string name) => Core.SyncSnapshot().Folders.FirstOrDefault(f => f.Name == name)?.Id ?? Core.ChatChannels().FirstOrDefault(c => c.Name == name)?.Id ?? name;
        bool Has(string flag) => w.Skip(1).Contains(flag);
        var pane = LibraryWindow.Pane;
        ListRow? Row(string part, out string group)
        {
            group = "";
            foreach (var g in pane.Model?.List?.Groups ?? [])
            {
                foreach (var r in g.Rows)
                {
                    if (r.Title.Contains(part, StringComparison.OrdinalIgnoreCase))
                    {
                        group = g.Key;
                        return r;
                    }
                }
            }
            return null;
        }
        string result = "done";
        try
        {
            switch (verb)
            {
                case "window": ShowMainWindow(); break;
                case "menu": flyout?.Toggle(); break;
                case "place":
                    Library.SelectPlace(Arg(1) switch
                    {
                        "sharing" => new LibraryPlace.Sharing(),
                        "foldersync" => new LibraryPlace.FolderSync(),
                        "settings" => new LibraryPlace.Settings(),
                        "tracker" => new LibraryPlace.Tracker(Arg(2)),
                        // A channel by name or id (its General), or a private conversation by device id.
                        "chat" => new LibraryPlace.Chat(FolderId(Arg(2)), null),
                        "direct" => new LibraryPlace.Direct(Arg(2)),
                        _ => new LibraryPlace.Downloads(),
                    });
                    break;
                // The file lists (the core's FileLists): describe | toggle <row> | click <row> <file> | check <row> <file> |
                // priority <row> <file> High|Normal|Low | all <row> on|off | filter <row> <text> | flush <row> | menu <row>
                // (the tray's chevron) | menu-check <row> <file>. A row by part of its title, a file by part of its name.
                case "files":
                    result = await QaFiles(w, Arg, Row, pane);
                    break;
                // The chooser, the held list and the starting notifier (Part E): chooser show <row> | status | toggle <text>
                // | pick All|Missing|Latest | category <name> | start | cancel [keep] | close; held toggle <row> | status;
                // starting status | show | close | chevron <row> | <ok|start-now|cancel|title|category|all|none|open|toggle>
                // <row> [arg] | shift on|off|auto. A row by part of its title.
                // Favorites and the Info page (Part F): favorite save|rename <tracker> <name>|remove|confirm|cancel|status <tracker>;
                // tracker-info <tracker> [limits <down> <up> | pex on|off | add <url>]. A tracker by part of its name.
                case "favorite" or "tracker-info":
                    result = await QaFavorite(verb, w, Arg);
                    break;
                case "chooser" or "held" or "starting":
                    result = await QaChoice(verb, w, Arg, Row);
                    break;
                case "rowclick":
                    if (Row(Arg(1), out var group) is ListRow clicked) Library.ClickRow(group, clicked.Id, Has("command"), Has("shift"));
                    else result = "no such row";
                    break;
                case "key":
                    var key = Arg(1) switch
                    {
                        "delete" => LibraryKey.Delete,
                        "return" => LibraryKey.Return,
                        "escape" => LibraryKey.Escape,
                        "space" => LibraryKey.Space,
                        "left" => LibraryKey.Left,
                        "right" => LibraryKey.Right,
                        "up" => LibraryKey.Up,
                        "down" => LibraryKey.Down,
                        "i" => LibraryKey.I,
                        "a" => LibraryKey.A,
                        _ => (LibraryKey?)null,
                    };
                    if (key is LibraryKey k) result = $"handled {pane.PressKey(k, new KeyModifiers(Has("shift"), Has("command"), false, false), Has("list"))}";
                    else result = "no such key";
                    break;
                case "remove":
                    if (Row(Arg(1), out _) is ListRow target) Library.AskRemoval(Library.MenuRows(target.Id), Has("files"));
                    else result = "no such row";
                    break;
                case "info":
                    if (Row(Arg(1), out _) is ListRow infoRow) OpenInfo(infoRow.Id);
                    else result = "no such row";
                    break;
                // What a click on one of the row's own controls runs, named by its command (CancelHeld, Pause, ...).
                case "control":
                    if (Row(Arg(1), out _) is not ListRow controlRow) result = "no such row";
                    else if (controlRow.Controls.Select(c => c.Command).Concat(RowCommands.Held(controlRow.Transfer).Select(i => i.Command))
                        .FirstOrDefault(c => c?.GetType().Name == Arg(2)) is not RowCommand rc) result = $"no such control (the row has {string.Join(", ", controlRow.Controls.Select(c => c.Command?.GetType().Name ?? c.Kind.ToString()))})";
                    else if (main?.Content.XamlRoot is not XamlRoot controlRoot) result = "no window";
                    else RowCommands.Run(Core, rc, [controlRow.Transfer], controlRoot);
                    break;
                case "confirm": Library.ConfirmRemoval(); break;
                case "cancel": Library.CancelRemoval(); break;
                case "add-sheet": Library.ShowAddSheet(); break;
                // Ctrl+O and the add sheet's Choose .torrent File… (a QA run logs the dialog instead of opening it).
                case "add-file": ChooseTorrentFile(); break;
                // Move to… on a row, with the folder the dialog would have answered.
                case "move":
                    if (Row(Arg(1), out _) is not ListRow moveRow) result = "no such row";
                    else { QaFolder = string.Join(' ', w.Skip(2)); await RowCommands.MoveTo(Core, [moveRow.Id]); QaFolder = null; }
                    break;
                // The flyout's "+N more" line and its Sharing line.
                case "menu-line": ShowPlace(Arg(1) == "sharing" ? new LibraryPlace.Sharing() : new LibraryPlace.Downloads()); break;
                // A channel's sidebar menu entry (Mute, Unmute, ...), or its Agents choice (agents <channel> yes|no).
                // A channel's sidebar menu entry, by its command's name (Mute, Unmute, NewTopic, RenameChannel,
                // LeaveChannel, ChannelAgents, SetFollow), as its click runs it.
                case "sidebar":
                    var channel = Core.ChatChannels().FirstOrDefault(c => c.Name.Contains(Arg(1), StringComparison.OrdinalIgnoreCase));
                    if (channel is null) { result = "no such channel"; break; }
                    var chatPlace = new LibraryPlace.Chat(channel.Id, null);
                    var menuItems = Library.SidebarMenu(chatPlace).OfType<SidebarMenuEntry.Item>().ToArray();
                    if (menuItems.FirstOrDefault(i => i.Command.GetType().Name.Equals(Arg(2), StringComparison.OrdinalIgnoreCase)) is SidebarMenuEntry.Item entry)
                    {
                        pane.RunSidebarCommand(entry.Command, chatPlace);
                        // Mute and the like are queued: what they did shows once they returned.
                        await CoreCalls.Idle();
                    }
                    else { result = $"no such entry (the menu has {string.Join(", ", menuItems.Select(i => i.Command.GetType().Name))})"; break; }
                    var now = Core.ChatChannels().FirstOrDefault(c => c.Id == channel.Id);
                    pane.Refresh();
                    result = now is null ? "channel gone" : $"channel muted {now.Muted}, agents {now.Agents}, drawn muted {pane.SidebarDrawn.Contains(nameof(SidebarIcon.MutedChannel))}";
                    break;
                // The open chat dialog: type <text> | confirm | cancel | status.
                case "dialog":
                    switch (Arg(1))
                    {
                        case "type": ChatDialogs.QaType(string.Join(' ', w.Skip(2))); break;
                        case "confirm": if (!ChatDialogs.QaConfirm()) result = "not confirmed (none open, or its action is disabled)"; break;
                        case "cancel": ChatDialogs.QaCancel(); break;
                    }
                    if (result == "done") result = $"dialog {(ChatDialogs.Shown is ChatDialog sd ? $"{sd.Kind} \"{sd.Title}\"" : "none")}";
                    break;
                // A private conversation's Delete Conversation… (it has no menu on Windows until the conversation page).
                case "delete-conversation":
                    var direct = Core.ChatDirects().FirstOrDefault(d => d.Name.Contains(Arg(1), StringComparison.OrdinalIgnoreCase));
                    if (direct is null) { result = "no such conversation"; break; }
                    ChatDialogs.Show(TrsFfiMethods.ChatDeleteConversationDialog(direct.Name), null, direct.DeviceId);
                    break;
                // A channel's Wake Command… (in the conversation page's ⋯ menu on the Mac; no menu on Windows yet).
                case "wake-command":
                    var wakeChannel = Core.ChatChannels().FirstOrDefault(c => c.Name.Contains(Arg(1), StringComparison.OrdinalIgnoreCase));
                    if (wakeChannel is null) { result = "no such channel"; break; }
                    ChatDialogs.Show(TrsFfiMethods.ChatWakeCommandDialog(wakeChannel), wakeChannel);
                    break;
                // The New Channel window: open [link] | mode new|join | name <text> | link <text> | agents yes|no |
                // start | cancel | status.
                case "new-channel":
                    if (Arg(1) == "open") { NewChannelDialog.Show(this, w.Length > 2 ? w[2] : null); await Quiet.Settle(LibraryWindow.Content); }
                    else if (NewChannelDialog.Open is not NewChannelDialog nc) { result = "not open"; break; }
                    else switch (Arg(1))
                    {
                        case "mode": nc.QaMode(Arg(2) == "join" ? ChatNewMode.Join : ChatNewMode.New); break;
                        case "name": nc.QaName(string.Join(' ', w.Skip(2))); break;
                        case "link": nc.QaLink(string.Join(' ', w.Skip(2))); break;
                        case "agents": nc.QaAgents(Arg(2) == "yes"); break;
                        case "start": result = await nc.QaStart() ? "started" : "not started"; break;
                        case "cancel": nc.QaCancel(); break;
                    }
                    if (NewChannelDialog.Open is NewChannelDialog open)
                    {
                        result += $"; new channel {open.Form.Mode}, can start {open.Model.CanStart}, link line \"{open.Model.LinkLine}\"";
                    }
                    break;
                // Settings: tab <General|Transfers|Folders|Network|FolderSync|Devices|Agents|Advanced> | rows | row <id> |
                // set <row id> <value> [unit] (a switch on|off, an option id, a field's text, a button's title) | describe.
                case "settings":
                    if (pane.Model?.Page != LibraryPage.Settings) { Library.SelectPlace(new LibraryPlace.Settings()); pane.Refresh(); }
                    if (pane.SettingsPageShown is not SettingsPage sp2) { result = "no settings page"; break; }
                    switch (Arg(1))
                    {
                        case "tab": if (Enum.TryParse<SettingsTab>(Arg(2), true, out var st)) sp2.Show(st); else result = "no such tab; "; break;
                        case "rows": result = sp2.QaRows() + "; "; break;
                        case "row": result = sp2.QaRow(Arg(2)) + "; "; break;
                        case "set": result = sp2.QaSet(Arg(2), w.Skip(3).ToArray()) + "; "; await Task.Delay(700); await CoreCalls.Idle(); break;
                        // The rules editor: rules [status | section rules|trackers | select <title> | add | remove | save | revert |
                        // move <from> <to> | new-tracker <host>]; its rows are set with set <row> as Settings' are.
                        case "rules": result = sp2.QaRulesCommand(w.Skip(2).ToArray()) + "; "; await Task.Delay(900); await CoreCalls.Idle(); result += sp2.QaRules() + "; "; break;
                    }
                    await Quiet.Settle(LibraryWindow.Content);
                    result += sp2.QaDescribe();
                    break;
                // Notifications (a QA run posts none: it keeps them): notices (the last ones posted) | notice-click <click>
                // (what a click on a notice with that route string opens).
                case "notices":
                    result = Notifier is null ? "no notifier" : string.Join(" || ", Notifier.Posted.TakeLast(5).Select(n => $"{n.Kind}: {n.Title} | {n.Body} [{n.Key ?? "-"}] {{{Notifier.ClickArg(n.Click)}}}"));
                    if (result.Length == 0) result = "none";
                    break;
                case "notice-click":
                    Notifier?.Clicked(string.Join(' ', w.Skip(1)));
                    await Quiet.Settle(LibraryWindow.Content);
                    break;
                // A click on a notice's button: notice-action <action id> (the last notice that has buttons).
                case "notice-action":
                    if (Notifier?.Posted.LastOrDefault(n => n.Actions.Length > 0) is not Notice withButtons) { result = "no notice with buttons; "; break; }
                    Notifier.Clicked(Notifier.ClickArg(withButtons.Click), Arg(1));
                    await Task.Delay(500);
                    await CoreCalls.Idle();
                    result = $"clicked {Arg(1)} on \"{withButtons.Title}\" [{string.Join(", ", withButtons.Actions.Select(a => a.Id))}]";
                    break;
                // Agents waiting for access: requests (the tray's line and the rows, as the core gives them).
                case "requests":
                    var rq = Core.AgentRequests(Now());
                    result = $"line \"{rq.Line ?? ""}\", rows {rq.Rows.Length} [{string.Join(" | ", rq.Rows.Select(r => r.Title))}], icon \"{tray?.ToolTipText ?? ""}\" dot {drawn?.Attention} for {drawn?.NeedsAnswer?.ToString() ?? "none"}";
                    break;
                // The question the core asks, in a QA run waiting for its answer: prompt status | yes | no.
                case "prompt":
                    if (Arg(1) is "yes" or "no") result = Prompts.QaAnswer(Arg(1) == "yes") ? "answered; " : "nothing asked; ";
                    await Task.Delay(500);
                    result += $"waiting \"{Prompts.Waiting ?? ""}\"";
                    break;
                // The settings banner as the core gives it, outside Settings.
                case "banner":
                    result = Banner.QaText(false);
                    break;
                // Play on a row as its Play button does (a QA run starts no player: it logs play-url): the note shown after.
                case "play":
                    if (Row(Arg(1), out _) is not ListRow played) { result = "no such row"; break; }
                    Player.Play(this, played.Transfer);
                    await Task.Delay(1500);
                    result = $"note \"{pane.NoteShown ?? ""}\"";
                    break;
                // The on-demand host (item 6): its roots and each file's Cloud Files state; read <folder-relative path> reads a file
                // (off the UI thread: a placeholder's read is filled by the host's callback).
                case "ondemand":
                    if (Cloud is null) { result = "no host"; break; }
                    if (Arg(1) == "read") { var rel = string.Join(' ', w.Skip(2)); result = await Task.Run(() => Cloud.QaRead(rel)) + "; "; }
                    // placeholder <full path>: whether a file or folder is still a Cloud Files placeholder (after a switch to full, none is).
                    if (Arg(1) == "placeholder") { var at = string.Join(' ', w.Skip(2)); result = $"placeholder {await Task.Run(() => CloudFiles.IsPlaceholder(at))}; "; }
                    // write-during-update <relative path> <text>: a program's write lands while the feed's item is placed.
                    if (Arg(1) == "write-during-update") { var wrel = Arg(2); var wtext = string.Join(' ', w.Skip(3)); result = await Task.Run(() => Cloud.QaWriteDuringUpdate(wrel, wtext)) + "; "; }
                    // write-when-stale <relative path> <text>: armed; the write lands when another device's change reaches the file, before it is let go of.
                    if (Arg(1) == "write-when-stale") { var srel = Arg(2); var stext = string.Join(' ', w.Skip(3)); result = Cloud.QaWriteWhenStale(srel, stext) + "; "; }
                    if (Arg(1) == "wrote-when-stale") result = $"wrote {Cloud.QaWroteWhenStale}; ";
                    result += Cloud.QaStatus();
                    break;
                // The Cloud Files plumbing's own check (item 6): a sync root in this run's temp state, no core involved.
                case "cloudfiles" when Arg(1) == "selftest":
                    var cfState = Core.StateDir();
                    result = await Task.Run(() => CloudSelfTest.Run(cfState));
                    break;
                // The players the core found on this PC (Windows: the registry and PATH), each with what Automatic plays.
                case "play-players":
                    result = string.Join(" | ", Core.PlayPlayersReport());
                    break;
                // Another Seedwell on this PC (Settings › Advanced's warning line): the process list, read off the UI thread.
                case "others":
                    result = await Task.Run(() => Core.OtherSeedwells()) ?? "none";
                    break;
                // The conversation shown: describe | type <text> | send | reply <card> | attach <path> | menu <title> | menu-text |
                // members | member-rows | member <device id> <title> (a member's menu item) | members-shot <name> (the panel's content, as a picture) |
                // file <card> <file> <action> | image <card> <image> open|fetch | tag <card> <tag> | open-answer yes|no |
                // hover <card> on|off (its time, Tag and Reply as the pointer shows them) | quote <card> (a click on its reply quote) |
                // show-image <card> <image> (an image reference's click) | escape (Escape in the field) | slow-read <ms> (each read of the
                // page takes that much longer) | focus <caret> (the field has the focus, as someone typing) | redraw (the whole page) |
                // click-send (Send takes the focus as a click gives it, then its click's call).
                // Cards count from 0; a file action is the core's kind (Download, SaveAs, Retry, Cancel, Open, ShowInFolder,
                // StopSharing).
                case "chatpage":
                    pane.Refresh();
                    if (pane.ChatPageShown is not ChatPage cp) { result = "no conversation"; break; }
                    await cp.Read();
                    var cards = cp.Model?.Messages ?? [];
                    ChatMessageCard? CardAt(string n) => int.TryParse(n, out var i) && i >= 0 && i < cards.Length ? cards[i] : null;
                    switch (Arg(1))
                    {
                        case "type": cp.QaType(string.Join(' ', w.Skip(2))); break;
                        case "send": result = await cp.Send() ? "sent; " : "not sent; "; await Task.Delay(300); break;
                        case "reply": cp.QaReply(int.TryParse(Arg(2), out var rn) ? rn : -1); break;
                        case "attach": cp.Attach(new ChatImageInput(Path.GetFileName(string.Join(' ', w.Skip(2))), string.Join(' ', w.Skip(2)), null)); await Task.Delay(1000); break;
                        case "menu": result = cp.QaMenu(Arg(2)) + "; "; await Quiet.Settle(LibraryWindow.Content); break;
                        case "menu-text": result = cp.QaMenuText() + "; "; break;
                        case "members": cp.QaMembers(); await Task.Delay(1500); break;
                        case "member-rows": result = $"rows {cp.QaMemberRows()}; "; break;
                        case "member": result = cp.QaMemberPress(Arg(2), string.Join(' ', w.Skip(3))) + "; "; await Task.Delay(1000); break;
                        case "members-shot":
                            var panelDir = Path.Combine(Path.GetDirectoryName(foldsFile) ?? "", "qa");
                            Directory.CreateDirectory(panelDir);
                            result = await cp.QaMembersShot(Path.Combine(panelDir, $"{Arg(2)}.png")) + "; ";
                            break;
                        case "hover": result = (cp.QaHover(int.TryParse(Arg(2), out var hn) ? hn : -1, Arg(3) != "off") ? "hover" : "no such card") + "; "; break;
                        case "quote": result = cp.QaQuote(int.TryParse(Arg(2), out var qn) ? qn : -1) + "; "; break;
                        case "show-image":
                            var sn = int.TryParse(Arg(2), out var sv) ? sv : -1;
                            cp.QaShowImage(sn, int.TryParse(Arg(3), out var si) ? si : 0);
                            result = $"gallery shows {cp.GalleryShown(sn)}; ";
                            break;
                        case "escape": result = (cp.Escape() ? "reply cancelled" : "passed on") + "; "; break;
                        case "drop-over" when int.TryParse(Arg(2), out var overChar): var hit = await cp.QaDropOver(overChar, string.Join(' ', w.Skip(3))); result = $"hit {hit}, draft \"{cp.Draft}\"; "; break;
                        case "drop-at" when uint.TryParse(Arg(2), out var dropAt): await cp.QaDropAt(string.Join(' ', w.Skip(3)), dropAt); result = $"draft \"{cp.Draft}\"; "; break;
                        case "open-answer": ChatAttachments.QaOpenAnswer = Arg(2) == "yes"; break;
                        case "file":
                            if (CardAt(Arg(2)) is not ChatMessageCard fc || !int.TryParse(Arg(3), out var fi) || fi < 0 || fi >= fc.Files.Length) { result = "no such file; "; break; }
                            var cardFile = fc.Files[fi];
                            if (!Enum.TryParse<ChatFileActionKind>(Arg(4), out var kind) || cardFile.Actions.All(a => a.Kind != kind)) { result = $"no {Arg(4)} on it ({string.Join(",", cardFile.Actions.Select(a => a.Kind))}); "; break; }
                            cp.Attachments.RunFile(fc.Place, cardFile, kind, cp.Update);
                            await Task.Delay(1500);
                            result = $"asked \"{ChatAttachments.Asked ?? ""}\"; ";
                            break;
                        case "image":
                            if (CardAt(Arg(2)) is not ChatMessageCard ic || !int.TryParse(Arg(3), out var ii) || ii < 0 || ii >= ic.Images.Length) { result = "no such image; "; break; }
                            if (Arg(4) == "fetch") cp.Attachments.Fetch(ic.Place, ic.Images[ii].Hash, cp.Update); else cp.Attachments.OpenImage(ic.Place, ic.Images[ii].Hash);
                            await Task.Delay(1000);
                            break;
                        case "tag":
                            if (CardAt(Arg(2))?.TagMenu?.Entries.OfType<ChatPageMenuEntry.Item>().FirstOrDefault(t => t.Title.StartsWith(Arg(3))) is not ChatPageMenuEntry.Item tagItem) { result = "no such tag; "; break; }
                            cp.Run(tagItem.Command);
                            await Task.Delay(800);
                            break;
                        case "slow-read": ChatPage.QaSlowReadMs = int.TryParse(Arg(2), out var slowMs) ? slowMs : 0; break;
                        case "focus": cp.QaFocus(int.TryParse(Arg(2), out var caretAt) ? caretAt : 0); break;
                        case "redraw": await cp.QaRedraw(); break;
                        case "click-send": result = await cp.QaClickSend() ? "sent; " : "not sent; "; await Task.Delay(300); break;
                    }
                    await cp.Read();
                    var cm = cp.Model;
                    if (cm is null) { result += "no model"; break; }
                    var lastCard = cm.Messages.LastOrDefault();
                    string BodyText(ChatMessageCard c) => c.Body switch
                    {
                        ChatMessageBody.Markdown md => string.Join(" / ", md.Blocks.Select(ChatMarkdown.Plain)),
                        ChatMessageBody.Mail ml => "mail " + string.Join(" / ", ml.Blocks.Select(ChatMarkdown.Plain)),
                        ChatMessageBody.Json js => "json " + js.Text,
                        ChatMessageBody.Unsupported un => "unsupported " + un.Text,
                        _ => "",
                    };
                    result += $"cards {cm.Messages.Length}, empty \"{cm.Empty ?? ""}\", last \"{(lastCard is null ? "" : BodyText(lastCard))}\", last images {lastCard?.Images.Length ?? 0} [{string.Join(",", lastCard?.Images.Select(i => i.State) ?? [])}], last files {lastCard?.Files.Length ?? 0} [{string.Join(",", lastCard?.Files.Select(f => $"{f.State}:{string.Join("+", f.Actions.Select(a => a.Kind))}") ?? [])}], last tags \"{string.Join("; ", lastCard?.Tags.Select(t => t.Text) ?? [])}\", tags total {cm.Messages.Sum(m => m.Tags.Length)}, reply quote \"{(lastCard?.ReplyQuote is ChatQuote quoted ? $"{quoted.Title}: {quoted.Text}" : "")}\", bubbles {cm.Bubbles}, last who {lastCard?.Who.ToString() ?? "none"}, last meta \"{(lastCard?.Meta is ChatMessageMeta lm ? $"{lm.Name} · {lm.Detail}" : "")}\", runs {cm.Messages.Count(m => m.StartsRun)}, metas {cm.Messages.Count(m => m.Meta is not null)}, toolbar [{(cm.Toolbar.Tag is null ? "" : "tag ")}{(cm.Toolbar.Members is null ? "" : "members ")}{(cm.Toolbar.Share is null ? "" : "share ")}menu], can post {cm.Composer.CanPost} \"{cm.Composer.CannotPost ?? ""}\", attach {cm.Composer.Attach}, attached {cp.Attached}, send enabled {cp.SendEnabled}, pending {cm.Composer.Pending.Length}, replying {cp.ReplyingTo ?? "none"}, members panel {(cm.Members is null ? "none" : $"{cm.Members.Rows.Length} rows")}, open {cp.MembersOpen}, {cp.QaField}";
                    break;
                // The Folder Sync page: describe | press <title> [<folder>] (a button, or a list row's menu item; its question
                // counts as confirmed) | members (the member rows too) | member <device id> <title> (a member's menu item, its
                // question counted as confirmed) | trash on|off | remove (the Stop Syncing question's Remove) | choose <folder>. A folder is named by its name or id.
                case "syncpage":
                    if (pane.Model?.Page != LibraryPage.FolderSync) { Library.SelectPlace(new LibraryPlace.FolderSync()); pane.Refresh(); }
                    if (pane.SyncPage is not FolderSyncPage page) { result = "no page"; break; }
                    switch (Arg(1))
                    {
                        case "press": result = page.QaPress(Arg(2), w.Length > 3 ? FolderId(Arg(3)) : null) + "; "; break;
                        case "member": result = page.QaMemberPress(Arg(2), string.Join(' ', w.Skip(3))) + "; "; await Task.Delay(1000); break;
                        case "members": await Task.Delay(300); break;
                        case "trash": page.QaTrash(Arg(2) == "on"); break;
                        case "remove": page.Run(new SyncPageCommand.Remove(), page.Chosen); await Task.Delay(1000); break;
                        case "choose": page.Choose(FolderId(Arg(2))); break;
                    }
                    await Quiet.Settle(LibraryWindow.Content);
                    page.Update();
                    result += page.QaDescribe();
                    if (Arg(1) is "member" or "members") result += $"; rows {page.QaMembers()}";
                    break;
                // Add Folder: open [link] | mode local|shared | folder <path> | link <text> | location <path> | parent <path> |
                // devices on|off | readonly on|off | ondemand on|off | start | cancel | status.
                case "add-folder":
                    if (Arg(1) == "open") { AddFolderDialog.Show(this, w.Length > 2 ? w[2] : null); await Quiet.Settle(LibraryWindow.Content); }
                    else if (AddFolderDialog.Open is not AddFolderDialog af) { result = "not open"; break; }
                    else switch (Arg(1))
                    {
                        case "mode": af.QaMode(Arg(2) == "shared" ? SyncAddMode.Shared : SyncAddMode.Local); break;
                        case "folder": af.QaFolder(string.Join(' ', w.Skip(2))); break;
                        case "link": af.QaLink(string.Join(' ', w.Skip(2))); break;
                        case "location": af.QaLocation(string.Join(' ', w.Skip(2))); break;
                        case "parent": af.QaParent(string.Join(' ', w.Skip(2))); break;
                        case "devices": af.QaAllDevices(Arg(2) == "on"); break;
                        case "readonly": af.QaReadOnly(Arg(2) == "on"); break;
                        case "ondemand": af.QaOnDemand(Arg(2) == "on"); break;
                        case "start": result = await af.QaStart() ? "started" : "not started"; await Quiet.Settle(LibraryWindow.Content); break;
                        case "cancel": af.QaCancel(); break;
                    }
                    if (AddFolderDialog.Open is AddFolderDialog afOpen)
                    {
                        var am = afOpen.Model;
                        result += $"; add folder {afOpen.Form.Mode}, can start {am.CanStart}, folder \"{am.FolderText}\", link line \"{am.LinkLine}\", location \"{am.Location}\", on demand \"{am.OnDemandLabel ?? "none"}\" {afOpen.Form.OnDemand} note \"{am.OnDemandNote ?? ""}\", {QaFollow(am.Follow, am.ReadOnly)}, drawn [{afOpen.Switches}], failure \"{afOpen.Failure}\"";
                    }
                    if (ShareDialog.Open is not null) result += "; share open";
                    break;
                // A folder's Edit dialog: open <folder> | name <text> | pause on|off | lan on|off | devices on|off | readonly on|off | ondemand on|off |
                // options on|off | parent <path> | share | save | cancel | status.
                case "edit-folder":
                    if (Arg(1) == "open") { EditFolderDialog.Show(this, FolderId(Arg(2))); await Quiet.Settle(LibraryWindow.Content); }
                    else if (EditFolderDialog.Open is not EditFolderDialog ef) { result = "not open"; break; }
                    else switch (Arg(1))
                    {
                        case "name": ef.QaName(string.Join(' ', w.Skip(2))); break;
                        case "pause": ef.QaPaused(Arg(2) == "on"); break;
                        case "lan": ef.QaLanOnly(Arg(2) == "on"); break;
                        case "devices": ef.QaAllDevices(Arg(2) == "on"); break;
                        case "readonly": ef.QaReadOnly(Arg(2) == "on"); break;
                        case "ondemand": ef.QaOnDemand(Arg(2) == "on"); break;
                        case "options": ef.QaOptions(Arg(2) == "on"); break;
                        case "parent": ef.QaParent(string.Join(' ', w.Skip(2))); break;
                        case "share": ef.QaShare(); await Quiet.Settle(LibraryWindow.Content); break;
                        case "save": result = await ef.QaSave() ? "saved" : "not saved"; break;
                        case "cancel": ef.QaCancel(); break;
                    }
                    if (EditFolderDialog.Open is EditFolderDialog efOpen)
                    {
                        var em = efOpen.Model;
                        result += $"; edit {em.Title}, changed {em.Changed}, can save {em.CanSave}, location \"{em.Location}\", share {(em.ShareTitle ?? "none")}, follow {(em.Follow is null ? "none" : em.Follow.On.ToString())}, on demand \"{em.OnDemandLabel ?? "none"}\" {efOpen.Form.OnDemand} note \"{em.OnDemandNote ?? ""}\", {QaFollow(em.Follow, em.ReadOnly)}, drawn [{efOpen.Switches}], failure \"{efOpen.Failure}\"";
                    }
                    if (ShareDialog.Open is not null) result += "; share open";
                    break;
                // The Share dialog: open <folder or channel> | role read_only|read_write|admin | single on|off | expires on|off
                // [days] | chat on|off [channel id] | devices on|off | readonly on|off | scroll-end | copy | revoke (the first link) | for <days> |
                // noend | cancel | status. The link copied is never printed.
                case "share":
                    if (Arg(1) == "open") { ShareDialog.Show(this, FolderId(Arg(2))); await Quiet.Settle(LibraryWindow.Content); }
                    else if (ShareDialog.Open is not ShareDialog sd) { result = "not open"; break; }
                    else switch (Arg(1))
                    {
                        case "role": sd.QaRole(Arg(2) switch { "admin" => SyncRole.Admin, "read_write" => SyncRole.ReadWrite, _ => SyncRole.ReadOnly }); break;
                        case "single": sd.QaSingleUse(Arg(2) == "on"); break;
                        case "expires": sd.QaExpires(Arg(2) == "on", uint.TryParse(Arg(3), out var days) ? days : 0); break;
                        case "chat": sd.QaChat(Arg(2) == "on", Arg(3)); break;
                        case "devices": sd.QaDevices(Arg(2) == "on"); break;
                        case "readonly": sd.QaReadOnly(Arg(2) == "on"); break;
                        case "scroll-end": sd.QaScrollEnd(); await Quiet.Settle(LibraryWindow.Content); break;
                        case "copy": result = await sd.Copy() ? "copied" : "not copied"; break;
                        case "revoke": sd.QaRevokeFirst(); await Task.Delay(500); break;
                        case "for": sd.QaShareFor(uint.TryParse(Arg(2), out var forDays) ? forDays : 7); await Task.Delay(500); break;
                        case "noend": sd.QaNoEnd(); await Task.Delay(500); break;
                        case "cancel": sd.QaCancel(); break;
                    }
                    if (ShareDialog.Open is ShareDialog sdOpen)
                    {
                        var sm = sdOpen.Model;
                        result += $"; share {sm.Title}, role {sm.Role}, summary \"{sm.Summary}\", links {sm.Links.Length}, sharing \"{sm.Sharing?.Countdown ?? sm.Sharing?.Line ?? "none"}\", copy {(sm.CopyTitle ?? "none")}, {QaFollow(sm.Devices, sm.ReadOnly)}, drawn [{sdOpen.Switches}], setting devices {sdOpen.SettingDevices}, refusal \"{sm.Gone ?? sm.Refusal ?? ""}\", failure \"{sdOpen.Failure}\"";
                    }
                    break;
                // The tray's right-click menu as drawn (its entries' titles, [x] when checked, (off) when disabled), or one
                // of its app commands run by name (AddTorrentFile, AddLink, PauseAll, ResumeAll, SetLimitSpeed).
                case "tray-menu":
                    result = string.Join(" | ", TrayMenuItems().Select(i => i switch
                    {
                        ToggleMenuFlyoutItem t => $"{t.Text} [x]{(t.IsEnabled ? "" : " (off)")}",
                        MenuFlyoutItem m => $"{m.Text}{(m.IsEnabled ? "" : " (off)")}",
                        _ => "-",
                    }));
                    break;
                case "app-command":
                    var appCommand = Core.AppCommands().SelectMany(m => m.Entries).OfType<AppMenuEntry.Item>()
                        .FirstOrDefault(i => i.Command.GetType().Name.Equals(Arg(1), StringComparison.OrdinalIgnoreCase))?.Command;
                    if (appCommand is null) { result = "no such app command"; break; }
                    RunAppCommand(appCommand);
                    break;
                // A link as the system would hand it over (route_link), and the window it opens.
                case "link":
                    Open([Core.RouteLink(Arg(1))]);
                    break;
                // Settings › Folder Sync › Cloudflare: use <address> | test | off | status (the section as drawn).
                case "rendezvous":
                    if (pane.Model?.Page != LibraryPage.Settings) { Library.SelectPlace(new LibraryPlace.Settings()); pane.Refresh(); }
                    if (pane.SettingsPageShown?.Rendezvous is not RendezvousSection rs) { result = "no settings page"; break; }
                    switch (Arg(1))
                    {
                        case "use": rs.QaAddress(string.Join(' ', w.Skip(2))); await rs.UseAddress(); break;
                        case "test": await rs.Run(() => Core.SyncRendezvousTest()); break;
                        case "off": await rs.Run(() => Core.SyncRendezvousTurnOff()); break;
                    }
                    var rv = Core.SyncRendezvous();
                    result = $"rendezvous {rv.State}, refused \"{rs.Refused}\", result \"{rv.Result}\", problem {rv.ResultIsProblem}";
                    break;
                // "On all my devices" for the folder the Folder Sync page shows: follow on|off, through its Edit dialog and Save.
                case "follow":
                    if (pane.Model?.Page != LibraryPage.FolderSync) { Library.SelectPlace(new LibraryPlace.FolderSync()); pane.Refresh(); }
                    if (pane.SyncPage is not FolderSyncPage sp || sp.Chosen is not string folderId) { result = "no folder"; break; }
                    EditFolderDialog.Show(this, folderId);
                    await Quiet.Settle(LibraryWindow.Content);
                    if (EditFolderDialog.Open is not EditFolderDialog fe) { result = "no edit dialog"; break; }
                    fe.QaAllDevices(Arg(1) == "on");
                    if (!await fe.QaSave()) { result = $"not saved: {fe.Failure}; "; fe.QaCancel(); }
                    var fl = Core.SyncFollowLine(folderId);
                    result = $"follow shown {fl.Shown}, on {fl.On}, enabled {fl.Enabled}";
                    break;
                // The torrent proxy, by the calls its controls make: proxy-model | proxy-toggle | proxy-click | proxy-find-faster |
                // proxy-country <code> | proxy-menu (the right click's menu) | proxy-flyout (the flyout's shield and sentence)
                // | proxy-sheet (the contact sheet to the qa folder).
                case "proxy-model": Proxy.Poll(); result = Proxy.QaDescribe(); break;
                case "proxy-toggle": await Proxy.Toggle(); result = Proxy.QaDescribe(); break;
                case "proxy-click": Proxy.Click(); await Task.Delay(300); await Quiet.Settle(LibraryWindow.Content); result = Proxy.QaDescribe(); break;
                case "proxy-find-faster": await Proxy.FindFaster(); result = Proxy.QaDescribe(); break;
                case "proxy-country": await Proxy.SetCountry(Arg(1)); result = Proxy.QaDescribe(); break;
                case "proxy-sheet":
                    var sheetDir = Path.Combine(Path.GetDirectoryName(foldsFile) ?? "", "qa");
                    Directory.CreateDirectory(sheetDir);
                    result = await ProxySheet.Save(LibraryWindow.Content.XamlRoot, Path.Combine(sheetDir, "proxy-shield-win.png"));
                    break;
                // The tray flyout's shield: open | close | shot <name> | status (it stays open while a QA run has no focus to lose).
                case "proxy-flyout":
                    if (flyout is null) { result = "no flyout"; break; }
                    if (Arg(1) == "open") { flyout.OpenPinned(false); await Quiet.Settle(flyout.RootElement); }
                    if (Arg(1) == "close") flyout.Hide();
                    if (Arg(1) == "shot")
                    {
                        var flyoutDir = Path.Combine(Path.GetDirectoryName(foldsFile) ?? "", "qa");
                        Directory.CreateDirectory(flyoutDir);
                        await SnapshotRenderer.SaveImage(flyout.RootElement, Path.Combine(flyoutDir, $"{Arg(2)}.png"), flyout.RootElement.ActualTheme != ElementTheme.Dark);
                    }
                    result = flyout.QaProxy();
                    break;
                // The SOCKS5 window: open | close | status | pane nord|custom | fill <user> <password> | enable | custom <host> <port> [<user> <password>]
                // | use-custom | replace on|off | guide on|off | copied <text> (as a new text on the clipboard: QA never reads the real one) |
                // remove | faster | hover on|off.
                case "proxy-window":
                    if (Arg(1) == "open") { Proxy.OpenWindow(); await Task.Delay(300); await Quiet.Settle(LibraryWindow.Content); }
                    // A window that failed to open or was closed: what it does with a new copy (nothing).
                    else if (Arg(1) == "last") { ProxyWindow.QaLast?.QaCopied(string.Join(' ', w.Skip(2))); result = ProxyWindow.QaLast?.QaDescribe() ?? "no window yet"; break; }
                    else if (ProxyWindow.Open is not ProxyWindow pw) { result = "not open"; break; }
                    else switch (Arg(1))
                    {
                        case "close": pw.QaClose(); break;
                        case "pane": pw.QaPane(Arg(2) == "custom"); break;
                        case "fill": pw.QaFill(Arg(2), Arg(3)); break;
                        case "enable": await pw.EnableNordVpn(); break;
                        case "custom": pw.QaFillCustom(Arg(2), Arg(3), Arg(4), Arg(5)); break;
                        case "use-custom": await pw.UseCustom(); break;
                        case "replace": pw.QaReplace(Arg(2) == "on"); break;
                        case "guide": pw.QaGuide(Arg(2) == "on"); break;
                        case "copied": pw.QaCopied(string.Join(' ', w.Skip(2))); break;
                        case "remove": await pw.RemoveKey(); break;
                        case "faster": await pw.FindFaster(); break;
                        case "hover": pw.QaHover(Arg(2) == "on"); break;
                    }
                    await Quiet.Settle(LibraryWindow.Content);
                    Proxy.Poll();
                    result = ProxyWindow.Open?.QaDescribe() ?? "closed";
                    break;
                // The Country list, as the window's button opens it: open | key <letter or Enter or Down> | choose <index> | shot <name> | status.
                case "proxy-countries":
                    if (Arg(1) == "open") { ProxyWindow.Open?.QaOpenCountries(); await Task.Delay(300); await Quiet.Settle(LibraryWindow.Content); }
                    if (Proxy.QaCountries is not (ListView cl, var ckey, var cchoose)) { result = "no list"; break; }
                    if (Arg(1) == "key" && Enum.TryParse<Windows.System.VirtualKey>(Arg(2), out var ck)) ckey(ck);
                    if (Arg(1) == "choose") cchoose(int.Parse(Arg(2)));
                    if (Arg(1) == "shot")
                    {
                        var listDir = Path.Combine(Path.GetDirectoryName(foldsFile) ?? "", "qa");
                        Directory.CreateDirectory(listDir);
                        await SnapshotRenderer.SaveImage(cl, Path.Combine(listDir, $"{Arg(2)}.png"), RequestedTheme == ApplicationTheme.Light);
                    }
                    await Quiet.Settle(LibraryWindow.Content);
                    result = $"list height {cl.ActualHeight} rows {cl.Items.Count} row height {(cl.ContainerFromIndex(cl.SelectedIndex < 0 ? 0 : cl.SelectedIndex) as FrameworkElement)?.ActualHeight}, selected {cl.SelectedIndex}, country \"{Proxy.Model.Country}\", open {Proxy.QaCountries is not null}";
                    break;
                // The navigation-pane entry, under the test seam only (a QA run's core gives none, and Explorer never sees those keys):
                // nav-pane <clsid> <marker> on|off <name> <target>.
                case "nav-pane":
                    if (Environment.GetEnvironmentVariable("SEEDWELL_INSTALL_TEST") != "1") { result = "refused: only under SEEDWELL_INSTALL_TEST=1"; break; }
                    NavPaneEntry.Apply(new SyncNavPane(Arg(1), Arg(4), string.Join(' ', w.Skip(5)), Arg(2), Arg(3) == "on"), Environment.ProcessPath ?? "");
                    result = NavPaneEntry.QaDescribe(Arg(1));
                    break;
                // Other devices can't reach this PC: reach (the core's model and the banner) | reach-recheck (as the button does after
                // netsh) | reach-allow no|ran (the button with UAC's answer given: the real command never runs in QA).
                case "reach" or "reach-recheck" or "reach-allow":
                    if (pane.Model?.Page != LibraryPage.FolderSync) { Library.SelectPlace(new LibraryPlace.FolderSync()); pane.Refresh(); }
                    if (pane.SyncPage is not FolderSyncPage rp) { result = "no page"; break; }
                    if (verb == "reach-recheck") await Task.Run(() => Reach.Recheck());
                    if (verb == "reach-allow") { rp.QaAllow(Arg(1) == "no"); await Task.Delay(500); }
                    rp.PollReach();
                    await Quiet.Settle(LibraryWindow.Content);
                    var rm = Reach.Model();
                    var ra = rm.Notice?.Action;
                    result = $"notice {(rm.Notice is null ? "none" : $"\"{rm.Notice.Title}\" body \"{rm.Notice.Body}\"")}, action {(ra is null ? "none" : $"\"{ra.Title}\" program \"{ra.Program}\" arguments {ra.Arguments}")}, refresh {rm.RefreshMs}, {rp.QaReachDrawn()}";
                    break;
                // Settings › Devices' calls, between two temp states: link (a temp state's, so it may be printed) | join <link> |
                // confirm (the first asking) | status.
                case "pair":
                    switch (Arg(1))
                    {
                        case "link": result = $"link {(Core.DevicesCurrentLink() ?? await Core.DevicesPairLink()).Link} ;"; break;
                        case "join": await Core.DevicesJoin(Arg(2)); break;
                        case "confirm":
                            if (Core.DevicesView().Requests.FirstOrDefault() is PairRequestRow asking) await Core.DevicesConfirm(asking.Id);
                            else result = "nobody asking; ";
                            break;
                    }
                    var pairing = Core.DevicesView();
                    result += $"devices {pairing.Kind}, rows {pairing.Rows.Length}, requests {pairing.Requests.Length}, joining \"{pairing.JoiningLine}\", problem \"{pairing.JoiningProblem}\"";
                    break;
                case "search": Library.SetSearch(string.Join(' ', w.Skip(1))); break;
                case "taskbar-created": TaskbarCreated(); result = $"tray {(tray is null ? "none" : "created")}"; break;
                case "update-check": Updater.Check(false); break;
                case "update-install": Updater.Install(); break;
                case "update-status": result = $"status \"{Updater.Updates.Model(Now()).Status}\", on offer {Updater.Updates.AvailableVersion() ?? "none"}, version {Updater.Updates.CurrentVersion()}"; break;
                case "shot":
                    pane.Refresh();
                    await Quiet.Settle(LibraryWindow.Content);
                    var dir = Path.Combine(Path.GetDirectoryName(foldsFile) ?? "", "qa");
                    Directory.CreateDirectory(dir);
                    var file = Path.Combine(dir, $"{Arg(1)}.png");
                    // The whole window's content (the Get Info card and the title-bar question are part of it), or with
                    // "rendezvous" the Settings page's Cloudflare section alone (it lies below the window's fold).
                    // With "dialog" the open dialog (it lives in the popup layer, not under the window's content).
                    var dialogPopup = Arg(2) == "dialog" && LibraryWindow.Content.XamlRoot is XamlRoot xr
                        ? Microsoft.UI.Xaml.Media.VisualTreeHelper.GetOpenPopupsForXamlRoot(xr).Select(p => p.Child).OfType<UIElement>().FirstOrDefault()
                        : null;
                    // With "starting" the starting notifier (a window of its own).
                    UIElement shown = dialogPopup ?? (Arg(2) == "starting" && StartingNotifier?.RootElement is UIElement notifier ? notifier
                        : Arg(2) == "rendezvous" && pane.SettingsPageShown?.Rendezvous.Root is UIElement section ? section : LibraryWindow.Content);
                    await SnapshotRenderer.SaveImage(shown, file, RequestedTheme == ApplicationTheme.Light);
                    result = file;
                    break;
                default: result = "unknown"; break;
            }
        }
        catch (Exception e)
        {
            result = $"failed: {e.GetType().Name}: {e.Message}";
        }
        pane.Refresh();
        var m = pane.Model;
        AppLog.Write($"qa {QaSaid(w)}: {result}; place {m?.Place}, selection [{string.Join(", ", m?.Selection ?? [])}], removal {(m?.Removal is PendingRemoval pr ? $"{pr.RowId ?? "(title bar)"}: {pr.Question.Message}" : "none")}");
    }

    /// <summary>Quit, asked from another thread (a second launch with --quit).</summary>
    internal void QuitFromAnyThread() => ui.TryEnqueue(() =>
    {
        // Before the core is there, Quit waits for it (a core being made must be shut down cleanly), unless the start
        // only waits for a previous run: then the next retry ends the app.
        if (!started) quitBeforeCore = true;
        if (Ready(Quit)) Quit();
    });

    /// <summary>A command for the tray icon's left click.</summary>
    sealed class Command(Action run) : System.Windows.Input.ICommand
    {
        public event EventHandler? CanExecuteChanged { add { } remove { } }
        public bool CanExecute(object? parameter) => true;
        public void Execute(object? parameter) => run();
    }
}
