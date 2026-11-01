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
    internal static HostWindow[] HostWindows { get; } = [HostWindow.GetInfo, HostWindow.MoveTo, HostWindow.NewChannel, HostWindow.ChatDialogs];
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
        AppLog.Write($"launch: state {dir}, {Program.Routes.Length} to open");
        if (GoBackAfterFailedLaunches(dir))
        {
            return;
        }
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
        NetWatch.Start(Core);
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
        if (Program.Routes.Length > 0)
        {
            Open(Program.Routes);
        }
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
                    LibraryWindow.Pane.SettingsPageShown?.Devices.ShowPaste(p.Link);
                    break;
                case LinkRoute.ShowFolder f:
                    ShowPlace(new LibraryPlace.FolderSync());
                    LibraryWindow.Pane.SyncPage?.Choose(f.FolderId);
                    break;
                // NEED: Add Folder and the folder's Share dialog (batch W3). Until they exist these open the Folder Sync
                // page, the folder chosen where the link names one.
                case LinkRoute.ShareFolder sf:
                    ShowPlace(new LibraryPlace.FolderSync());
                    LibraryWindow.Pane.SyncPage?.Choose(sf.FolderId);
                    break;
                case LinkRoute.AddFolder or LinkRoute.SharePath:
                    ShowPlace(new LibraryPlace.FolderSync());
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
        Library.SetHostWindows(HostWindows);
        ChatDialogs = new ChatDialogs(this);
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
        // The right-click menu, made anew each time it opens: the core's app commands (File, then Transfers), then Open
        // and Quit, worded by the menu model.
        var menu = new MenuFlyout();
        menu.Opening += (_, _) =>
        {
            menu.Items.Clear();
            foreach (var item in TrayMenuItems())
            {
                menu.Items.Add(item);
            }
        };
        foreach (var item in TrayMenuItems())
        {
            menu.Items.Add(item);
        }

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
            case AppCommand.PauseAll: Core.PauseAll(); break;
            case AppCommand.ResumeAll: Core.ResumeAll(); break;
            case AppCommand.SetLimitSpeed limit: Core.SetAltSpeed(limit.On); break;
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

    /// <summary>A folder the user picks (Move To…), or null; a QA run takes <see cref="QaFolder"/> instead of asking.</summary>
    internal async Task<string?> PickFolder()
    {
        if (Quiet.On)
        {
            AppLog.Write($"folder dialog: {QaFolder ?? "(none)"}");
            return QaFolder;
        }
        var picker = new Windows.Storage.Pickers.FolderPicker();
        picker.FileTypeFilter.Add("*");
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

    /// <summary>QA: the folder the next folder dialog answers with (the QA command pick-folder).</summary>
    internal string? QaFolder { get; set; }

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
        // A clean quit: this launch does not count toward a crash loop (SPEC 12A, Recovery).
        Updater?.Quitting();
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
                        case "use": rs.QaAddress(string.Join(' ', w.Skip(2))); rs.UseAddress(); break;
                        case "test": Core.SyncRendezvousTest(); break;
                        case "off": Core.SyncRendezvousTurnOff(); break;
                    }
                    var rv = Core.SyncRendezvous();
                    result = $"rendezvous {rv.State}, refused \"{rs.Refused}\", result \"{rv.Result}\", problem {rv.ResultIsProblem}";
                    break;
                // The Folder Sync page's "On all my devices" for its folder: follow on|off.
                case "follow":
                    if (pane.Model?.Page != LibraryPage.FolderSync) { Library.SelectPlace(new LibraryPlace.FolderSync()); pane.Refresh(); }
                    if (pane.SyncPage is not FolderSyncPage sp || sp.Chosen is not string folderId) { result = "no folder"; break; }
                    sp.SetFollow(folderId, Arg(1) == "on");
                    var fl = Core.SyncFollowLine(folderId);
                    result = $"follow shown {fl.Shown}, on {fl.On}, enabled {fl.Enabled}";
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
                    UIElement shown = dialogPopup ?? (Arg(2) == "rendezvous" && pane.SettingsPageShown?.Rendezvous.Root is UIElement section ? section : LibraryWindow.Content);
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
