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
        AppLog.Write($"launch: state {dir}, {Program.Sources.Length} to add");
        // A --state-dir run (QA, tests) is isolated: the core refuses it when a configured folder lies in the user's
        // profile outside the temp folder, and it trashes into <state>\Trash, never the Recycle Bin.
        var forbidden = Program.StateDir is null ? null : Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        try
        {
            Core = new Core(new CoreOptions(dir, false, false, forbidden));
        }
        catch (Exception e) when (forbidden is not null)
        {
            AppLog.Write($"launch refused: {e.Message}");
            Environment.Exit(2);
        }
        Core.SetListener(new Listener(this));
        PowerWatch.Start(Core);
        CreateLibrary(dir);
        Updater = new Updater(this, Core);
        Updater.Changed += () => flyout?.UpdateChanged();
        ApplySettings();
        flyout = new FlyoutWindow(this);
        TaskbarWatch.Start(flyout.Hwnd, TaskbarCreated);
        CreateTray();
        iconTimer = ui.CreateTimer();
        iconTimer.IsRepeating = false;
        iconTimer.Tick += (_, _) => UpdateIcon();
        UpdateIcon();
        // A theme change redraws the icon at once (it fires off the UI thread).
        uiSettings.ColorValuesChanged += (_, _) => ui.TryEnqueue(() =>
        {
            drawn = null;
            iconTimer.Stop();
            UpdateIcon();
        });
        if (Program.Sources.Length > 0)
        {
            Add(Program.Sources);
        }
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
        bool added = false;
        foreach (var source in sources)
        {
            try
            {
                Core.AddSource(source);
                added = true;
            }
            catch (CoreException e)
            {
                Alert(TrsFfiMethods.AddFailed(TrsFfiMethods.CoreErrorMessage(e)));
            }
            catch (Exception e)
            {
                Alert(TrsFfiMethods.AddFailed(e.Message));
            }
        }
        // The menu shows what was added; nothing added, nothing to show (the alert said why).
        if (added)
        {
            flyout?.ShowOpen();
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
        // A value the core could not use took its backup or safe value: say which (NEED: the host showing them).
        try
        {
            foreach (var problem in Core.SettingsProblems())
            {
                AppLog.Write($"settings problem: {problem}");
            }
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
    internal void SettingsChanged() => ui.TryEnqueue(ApplySettings);

    void CreateTray()
    {
        // The right-click menu: Open and Quit, worded by the menu model.
        MenuModel m;
        try { m = TrsFfiMethods.MenuModel(TakeSnapshot(), Core.SyncSnapshot(), Arrivals, Now(), Updater?.Updates.AvailableVersion()); }
        catch { m = new MenuModel(null, [], null, null, null, "Open Seedwell…", "Quit", 1000, ""); }
        var open = new MenuFlyoutItem { Text = m.OpenTitle };
        open.Click += (_, _) => ShowMainWindow();
        var quit = new MenuFlyoutItem { Text = m.QuitTitle };
        quit.Click += (_, _) => Quit();
        var menu = new MenuFlyout();
        menu.Items.Add(open);
        menu.Items.Add(new MenuFlyoutSeparator());
        menu.Items.Add(quit);

        var icon = new TaskbarIcon
        {
            ToolTipText = "Seedwell",
            Icon = new System.Drawing.Icon(Path.Combine(AppContext.BaseDirectory, "Seedwell.ico")),
            ContextFlyout = menu,
            ContextMenuMode = ContextMenuMode.SecondWindow,
            NoLeftClickDelay = true,
            LeftClickCommand = new Command(() => flyout?.Toggle()),
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

    /// <summary>
    /// The taskbar was created (Explorer started late or restarted; its TaskbarCreated message, or the QA command):
    /// an app without its icon makes it now. An icon that exists is put back by H.NotifyIcon itself.
    /// </summary>
    internal void TaskbarCreated()
    {
        if (tray is null && !quitting)
        {
            CreateTray();
            UpdateIcon();
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
        flyout?.Hide();
        main = LibraryWindow;
        main.ShowAndActivate();
        // A download that arrived while the window was closed.
        Library.TakeArrival(true, main.Pane.Context());
        main.Pane.Refresh();
        AppLog.Write($"window shown, in front: {main.IsInFront}");
    }

    /// <summary>
    /// A command whose window the Windows app does not have yet. NEED: each such window (Get Info, Move To, the
    /// chooser, rules, Folder Sync's Edit / Add Folder, the updater).
    /// </summary>
    internal void ShowPlaceholder(string what)
    {
        if (Quiet.On)
        {
            // A QA run opens no window that would take the foreground: the log says what it would have shown.
            AppLog.Write($"placeholder: {what}");
            return;
        }
        var w = new Window { Title = "Seedwell" };
        Chrome.Apply(w, new TextBlock { Text = what, Margin = new Thickness(24), TextWrapping = TextWrapping.Wrap }, "Seedwell");
        w.SystemBackdrop = new Microsoft.UI.Xaml.Media.MicaBackdrop();
        w.AppWindow.Resize(new Windows.Graphics.SizeInt32(420, 200));
        w.Activate();
    }

    /// <summary>A quiet notification from the tray icon (no sound, held back in Do Not Disturb).</summary>
    internal void Notify(string title, string body)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Notify(title, body));
            return;
        }
        tray?.ShowNotification(title, body, H.NotifyIcon.Core.NotificationIcon.None, sound: false, respectQuietTime: true);
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
        var clock = System.Diagnostics.Stopwatch.StartNew();
        iconTimer?.Stop();
        flyout?.Close();
        main?.CloseForQuit();
        tray?.Dispose();
        Task.Run(() =>
        {
            var r = Core.Shutdown(120);
            AppLog.Write($"quit: shutdown confirmed {r.Confirmed}, abandoned {r.Abandoned}, finished {r.Finished}, {clock.ElapsedMilliseconds} ms");
            ui.TryEnqueue(Exit);
        });
    }

    /// <summary>
    /// QA commands (<c>Seedwell.exe [--state-dir D] --qa ...</c> through the second-launch hand-off, run only when this
    /// instance was started with <c>--qa-control</c>, see <see cref="Program.QaControl"/>): each calls what the
    /// click or key it stands for calls, so tests drive the app without synthetic input; each is logged with the state
    /// after it. window | menu | place downloads|sharing|foldersync|settings|tracker &lt;id&gt; | rowclick &lt;title
    /// part&gt; [command] [shift] | key delete|return|escape|space|left|right|i [shift] [command] | remove &lt;title part&gt;
    /// [files] | info &lt;title part&gt; | control &lt;title part&gt; &lt;command&gt; | confirm | cancel | add-sheet |
    /// search &lt;text&gt; | update-check | update-install | update-status | taskbar-created | shot &lt;name&gt; (the
    /// library window to &lt;state&gt;\qa\&lt;name&gt;.png).
    /// </summary>
    internal void Qa(string[] words)
    {
        if (!ui.HasThreadAccess)
        {
            ui.TryEnqueue(() => Qa(words));
            return;
        }
        _ = QaRun(words);
    }

    async Task QaRun(string[] w)
    {
        string verb = w.Length > 0 ? w[0] : "";
        string Arg(int i) => w.Length > i ? w[i] : "";
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
                        _ => new LibraryPlace.Downloads(),
                    });
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
                        "i" => LibraryKey.I,
                        _ => (LibraryKey?)null,
                    };
                    if (key is LibraryKey k) result = $"handled {pane.PressKey(k, new KeyModifiers(Has("shift"), Has("command"), false, false))}";
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
                    // The whole window's content: the Get Info card and the title-bar question are part of it.
                    await SnapshotRenderer.SaveImage(LibraryWindow.Content, file, RequestedTheme == ApplicationTheme.Light);
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
        AppLog.Write($"qa {string.Join(' ', w)}: {result}; place {m?.Place}, selection [{string.Join(", ", m?.Selection ?? [])}], removal {(m?.Removal is PendingRemoval pr ? $"{pr.RowId ?? "(title bar)"}: {pr.Question.Message}" : "none")}");
    }

    /// <summary>Quit, asked from another thread (a second launch with --quit).</summary>
    internal void QuitFromAnyThread() => ui.TryEnqueue(Quit);

    /// <summary>A command for the tray icon's left click.</summary>
    sealed class Command(Action run) : System.Windows.Input.ICommand
    {
        public event EventHandler? CanExecuteChanged { add { } remove { } }
        public bool CanExecute(object? parameter) => true;
        public void Execute(object? parameter) => run();
    }
}
