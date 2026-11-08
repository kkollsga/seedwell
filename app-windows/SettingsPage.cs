using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The library window's Settings page (the Mac's SettingsRenderer): one generic renderer for the core's
/// <c>settings_view_model</c> (seedwell-ffi settings_view.rs). The tab bar, then the tab's blocks: titled groups of rows (or two
/// side by side), each row drawn by its style and its controls (switches, menus, segmented choices, fields with units,
/// folders, buttons, links, labels, checklists, chips); Folder Sync's Cloudflare section and the Devices tab are their
/// own sections. What is there and every word is the core's. Every change goes through <c>settings_apply</c>, whose
/// answer names what to save (settings and folder sync a moment after the last change, rules at once), a port to
/// move to and a question to ask; the host reads the live part (ports, reachability, activity, the updater, the
/// command-line tool) for the tab shown, again every <c>refresh_ms</c>.
/// </summary>
sealed partial class SettingsPage
{
    readonly App app;
    readonly StackPanel tabs = new() { Orientation = Orientation.Horizontal, Spacing = 2, Padding = new Thickness(24, 4, 24, 8) };
    /// <summary>The tab shown's controls.</summary>
    StackPanel content = NewPanel();
    /// <summary>
    /// Each tab's controls as last drawn, with what they were drawn from. Controls made anew are what holds the UI thread: a
    /// whole tab's tree, and the full collections its garbage brings, 100-500 ms for each tab shown and each refresh (the check
    /// interactive). So a tab or a refresh that would draw the same shows what is there.
    /// </summary>
    readonly Dictionary<SettingsTab, (StackPanel Panel, string Signature)> drawn = [];
    /// <summary>A draw that must make its controls anew waits out a field being typed in or an open drop-down: it stays owed.</summary>
    bool anewOwed;
    readonly List<(SettingsTab Tab, Button Button, FrameworkElement Title)> tabButtons = [];
    string tabsSignature = "";
    readonly ScrollViewer scroll = new();
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer refresh;
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer saveSettings;
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer saveSync;
    /// <summary>
    /// The field last typed in sends 600 ms after its last key. One timer for the page, not one for each field of each draw: a
    /// DispatcherQueueTimer holds its Tick handler in native code, out of the collector's sight, so one whose handler reaches
    /// the timer is never let go, with the draw's controls: 0.1.20 grew to 15.8 GB and 232,000 handles in 22 hours (check leaks).
    /// </summary>
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer typed;
    Action? typedSend;
    SettingsTab tab = SettingsTab.General;
    SettingsState state;
    SettingsPageModel? model;
    // The live part the host reads for the tab shown.
    VolumeWarning[] volumeWarnings = [];
    AgentActivity[] activity = [];
    ushort? syncPortMovingTo;

    public FrameworkElement Root { get; }

    /// <summary>How many pages this run made: one (the check interactive; each more would stay alive for good).</summary>
    internal static int Made { get; private set; }

    /// <summary>The Cloudflare rendezvous section (QA presses its buttons).</summary>
    internal RendezvousSection Rendezvous { get; }

    /// <summary>Settings › Devices (a pairing link opens its Pair with a Link…).</summary>
    internal DevicesSection Devices { get; }

    /// <summary>The model last drawn (QA).</summary>
    internal SettingsPageModel? Model => model;

    public SettingsPage(App app)
    {
        this.app = app;
        Devices = new DevicesSection(app);
        Rendezvous = new RendezvousSection(app);
        state = Load();
        var q = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        refresh = q.CreateTimer();
        refresh.IsRepeating = false;
        refresh.Tick += (_, _) => Refreshed();
        // Settings and folder sync are written a moment after the last change (the core's 450 ms).
        saveSettings = q.CreateTimer();
        saveSettings.IsRepeating = false;
        saveSettings.Interval = TimeSpan.FromMilliseconds(450);
        saveSettings.Tick += (_, _) => SaveSettings();
        saveSync = q.CreateTimer();
        saveSync.IsRepeating = false;
        saveSync.Interval = TimeSpan.FromMilliseconds(450);
        saveSync.Tick += (_, _) => SaveSync();
        typed = q.CreateTimer();
        typed.IsRepeating = false;
        typed.Interval = TimeSpan.FromMilliseconds(600);
        typed.Tick += (_, _) => { var send = typedSend; typedSend = null; send?.Invoke(); };
        var g = new Grid();
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        g.RowDefinitions.Add(new RowDefinition());
        g.Children.Add(tabs);
        scroll.Content = content;
        Grid.SetRow(scroll, 1);
        g.Children.Add(scroll);
        Root = g;
        Made++;
        g.Loaded += (_, _) => Render();
        g.Unloaded += (_, _) => refresh.Stop();
        app.Updater.Changed += Refreshed;
        app.Banner.Changed += Refreshed;
        RefreshCli();
        ReadVolumeWarnings();
        Draw();
    }

    SettingsState Load()
    {
        SyncSettings? sync = null;
        try { sync = app.Core.SyncSettings(); } catch { }
        return new SettingsState(app.Core.SettingsLoad(), app.Core.RulesLoad(), sync, SeedwellFfiMethods.SettingsViewState());
    }

    /// <summary>The settings changed on disk (an agent, the CLI, a hand edit): read again, the window's own state kept.</summary>
    internal void Reload()
    {
        var view = state.View;
        state = Load() with { View = view };
        ReadVolumeWarnings();
        Draw();
    }

    void ReadVolumeWarnings()
    {
        _ = Task.Run(() => app.Core.VolumeWarnings()).ContinueWith(t => app.Dispatch(() =>
        {
            if (t.IsCompletedSuccessfully) { volumeWarnings = t.Result; Refreshed(); }
        }));
    }

    /// <summary>
    /// The core looks at this computer's agent tools again (where the command-line tool is, whether the skill is
    /// installed, the version line: <c>SettingsHostFacts</c>), off the UI thread; drawn again when it has.
    /// </summary>
    void RefreshCli()
    {
        _ = Task.Run(() => app.Core.RefreshSettingsHostFacts()).ContinueWith(t => app.Dispatch(() => { if (t.IsCompletedSuccessfully) Refreshed(); }));
    }

    static string Bundled => Path.Combine(Updater.InstallDir, "cli", "seedwell.exe");

    SettingsLive Live()
    {
        var core = app.Core;
        SettingsUpdates updates;
        try { updates = new SettingsUpdates.Core(app.Updater.Updates.Model(App.Now())); } catch { updates = new SettingsUpdates.Off(); }
        DiagnosticsInfo diagnostics = tab == SettingsTab.Advanced ? core.Diagnostics() : new DiagnosticsInfo(0, 0, 0, 0, 0, 0, 0);
        if (tab == SettingsTab.Agents)
        {
            try { activity = core.AgentActivity(50); } catch { }
        }
        return new SettingsLive(
            app.Banner.Problems, app.Banner.Notes_, app.Banner.Restart,
            volumeWarnings, core.ListenPort(), core.PortFallback(), core.Reachability(),
            state.Sync is null ? null : core.SyncReachability(), syncPortMovingTo, state.Sync is not null,
            diagnostics, activity, Requests(), updates, PlayWith(),
            new SettingsCli(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), Bundled),
            core.SettingsHostFacts(), false, false, core.StateDir());
    }

    /// <summary>Agents waiting for access (Settings › Agents › Requests).</summary>
    AgentRequestRow[] Requests()
    {
        try { return app.Core.AgentRequests(App.Now()).Rows; } catch { return []; }
    }

    /// <summary>"Play videos with"'s choices for the setting (drawn only with HostWindow.Player).</summary>
    PlayWithChoice[] PlayWith()
    {
        try { return app.Core.PlayWithChoices(state.Settings.General.PlayWith); } catch { return []; }
    }

    // ----------------------------------------------------------------------------------------------- drawing

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    /// <summary>
    /// Draws the tab, unless a field is being typed in (its text stays until it is sent) or a drop-down is open (drawing makes
    /// the controls anew, which would close it). The keyboard stays on the control it was on. The controls are made anew,
    /// here and, once shown, in every other tab: what the user or an agent changed may be in them without being in the model
    /// (a field's refused text, the rules).
    /// </summary>
    void Draw()
    {
        anewOwed = true;
        Render();
    }

    /// <summary>
    /// What the page's timer and the app's changes ask: nothing while the page is not shown. It stays for the pane's life, with
    /// its timer and its place in the app's events: a page made for each visit never left them, and each went on drawing a tree
    /// nobody saw, every collection's pause growing with the visits (83 to 242 ms in a minute of the check interactive).
    /// </summary>
    void Refreshed()
    {
        if (model is null || Root.IsLoaded) Render();
    }

    /// <summary>The page opens again, as it did when each visit made one: at its first tab, at the top. What it had drawn stays.</summary>
    internal void Opened()
    {
        tab = SettingsTab.General;
        Render();
        scroll.ChangeView(null, 0, null, true);
    }

    /// <summary>
    /// Draws, for what only reads: the same model keeps the controls it was drawn with (<see cref="Draw"/> makes them anew). No walk over the tree
    /// here or in the focus keeper for that: each element a walk looks at gets a wrapper with memory pressure of its own, and
    /// a few walks of a tab brought the app a full collection every second, 60-380 ms of every thread (the check interactive).
    /// </summary>
    void Render()
    {
        refresh.Stop();
        // Not yet in the window (no XamlRoot): nothing can have focus. Over the page: a drop-down, a menu, a dialog.
        if (Root.XamlRoot is XamlRoot xr && (FocusManager.GetFocusedElement(xr) is TextBox or PasswordBox && IsInside((DependencyObject)FocusManager.GetFocusedElement(xr), content) || VisualTreeHelper.GetOpenPopupsForXamlRoot(xr).Any(p => p.IsOpen && p.Child is not ToolTip)))
        {
            refresh.Interval = TimeSpan.FromMilliseconds(500);
            refresh.Start();
            return;
        }
        SettingsPageModel m;
        try
        {
            m = SeedwellFfiMethods.SettingsViewModel(new SettingsViewInput(tab, state, Live()), App.HostWindows, App.HostFeatures, new Clock());
        }
        catch (Exception e)
        {
            AppLog.Write($"settings page failed: {e.Message}");
            return;
        }
        model = m;
        tab = m.Tab;
        var signature = ModelSignature.Of(m);
        if (!anewOwed && ModelSignature.Of(m.Tabs) == tabsSignature && drawn.TryGetValue(tab, out var same) && same.Signature == signature)
        {
            DrawTabs(m);
            content = same.Panel;
            if (!ReferenceEquals(scroll.Content, content)) scroll.Content = content;
        }
        else
        {
            FocusKeeper.Redraw(Root, Root.XamlRoot, () => DrawAnew(m, signature));
        }
        if (m.RefreshMs > 0)
        {
            refresh.Interval = TimeSpan.FromMilliseconds(m.RefreshMs);
            refresh.Start();
        }
    }

    void DrawAnew(SettingsPageModel m, string signature)
    {
        DrawTabs(m);
        if (anewOwed)
        {
            anewOwed = false;
            // Out of their panels: a section kept across draws finds no parent left to leave.
            foreach (var (_, (panel, _)) in drawn) panel.Children.Clear();
            drawn.Clear();
        }
        drawn.TryGetValue(tab, out var was);
        content = was.Panel ?? NewPanel();
        content.Children.Clear();
        foreach (var block in m.Blocks)
        {
            content.Children.Add(Block(block));
        }
        if (m.Footer is string footer)
        {
            content.Children.Add(new TextBlock { Text = footer, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), HorizontalAlignment = HorizontalAlignment.Center, IsTextSelectionEnabled = true });
        }
        if (m.Banner is SettingsBanner banner && SettingsBannerView.Draw(banner, () => { }) is UIElement b)
        {
            content.Children.Add(b);
        }
        drawn[tab] = (content, signature);
        if (!ReferenceEquals(scroll.Content, content)) scroll.Content = content;
    }

    static bool IsInside(DependencyObject? d, DependencyObject root)
    {
        while (d is not null)
        {
            if (d == root) return true;
            d = VisualTreeHelper.GetParent(d);
        }
        return false;
    }

    static string TabGlyph(SettingsTab t) => t switch
    {
        SettingsTab.General => "",
        SettingsTab.Transfers => "",
        SettingsTab.Folders => "",
        SettingsTab.Rules => "",
        SettingsTab.Network => "",
        SettingsTab.FolderSync => "",
        SettingsTab.Devices => "",
        SettingsTab.Agents => "",
        _ => "",
    };

    static StackPanel NewPanel() => new() { Padding = new Thickness(28, 4, 28, 28), Spacing = 18, MaxWidth = 860, HorizontalAlignment = HorizontalAlignment.Left };

    void DrawTabs(SettingsPageModel m)
    {
        var signature = ModelSignature.Of(m.Tabs);
        if (signature != tabsSignature)
        {
            tabsSignature = signature;
            tabs.Children.Clear();
            tabButtons.Clear();
            foreach (var t in m.Tabs)
            {
                var label = new StackPanel { Spacing = 2, HorizontalAlignment = HorizontalAlignment.Center };
                label.Children.Add(new FontIcon { Glyph = TabGlyph(t.Tab), FontSize = 16, HorizontalAlignment = HorizontalAlignment.Center });
                var title = new TextBlock { Text = t.Title, FontSize = 11.5, HorizontalAlignment = HorizontalAlignment.Center };
                label.Children.Add(title);
                var b = new Button { Content = label, MinWidth = 72, Padding = new Thickness(8, 6, 8, 6), BorderThickness = new Thickness(0) };
                if (t.Help.Length > 0) ToolTipService.SetToolTip(b, t.Help);
                var which = t.Tab;
                b.Click += (_, _) => Show(which);
                tabs.Children.Add(b);
                tabButtons.Add((which, b, title));
            }
        }
        foreach (var (t, b, title) in tabButtons)
        {
            b.Background = t == m.Tab ? Res("SubtleFillColorSecondaryBrush") : transparent;
            title.SetValue(TextBlock.FontWeightProperty, t == m.Tab ? FontWeights.SemiBold : FontWeights.Normal);
        }
    }

    static readonly SolidColorBrush transparent = new(Microsoft.UI.Colors.Transparent);

    /// <summary>QA: every tab walked (<see cref="FocusKeeper.QaWalk"/>), each redraw as a refresh and the sections' own changes make it.</summary>
    internal async Task<string> QaWalk()
    {
        var said = new List<string>();
        foreach (var t in Enum.GetValues<SettingsTab>())
        {
            if (!(model?.Tabs.Any(x => x.Tab == t) ?? false)) continue;
            Show(t);
            await Quiet.Settle(Root);
            try { said.Add($"{t}: {await FocusKeeper.QaWalk(Root, Root.XamlRoot, () => { Rendezvous.QaRedraw(); Devices.QaRedraw(); Draw(); })}"); }
            catch (Exception e) { said.Add($"{t}: walk threw {e.GetType().Name}: {e.Message.ReplaceLineEndings(" ")} at {string.Join(" < ", (e.StackTrace ?? "").Split('\n').Where(l => l.Contains("Seedwell.")).Take(4).Select(l => l.Trim()))}"); }
        }
        return string.Join("; ", said);
    }

    /// <summary>QA: this tab's first drop-down opened or closed as a click leaves it; whether it is open.</summary>
    internal bool QaDropDown(bool open)
    {
        if (RowCommands.Tree(content).OfType<ComboBox>().FirstOrDefault() is not ComboBox c) return false;
        c.IsDropDownOpen = open;
        return c.IsDropDownOpen;
    }
    internal bool QaDropDownOpen => RowCommands.Tree(content).OfType<ComboBox>().Any(c => c.IsDropDownOpen);
    internal void QaDraw() => Draw();

    /// <summary>Shows tab <paramref name="t"/> (the tab bar, and QA).</summary>
    internal void Show(SettingsTab t)
    {
        tab = t;
        if (t is SettingsTab.Transfers or SettingsTab.Folders) ReadVolumeWarnings();
        Render();
        scroll.ChangeView(null, 0, null, true);
    }

    UIElement Block(SettingsBlock b) => b switch
    {
        SettingsBlock.Single s => Group(s.Group),
        SettingsBlock.Pair p => Pair(p.Left, p.Right),
        SettingsBlock.Rendezvous => Detach(Rendezvous.Root),
        SettingsBlock.Devices => Detach(Devices.Root),
        SettingsBlock.RulesEditor => RulesEditor(),
        SettingsBlock.TorrentProxy t => new ProxySection(app, t.Title).Root,
        _ => new TextBlock(),
    };

    /// <summary>A section kept across redraws: out of its old place first.</summary>
    static UIElement Detach(UIElement e)
    {
        if (e is FrameworkElement { Parent: Panel p }) p.Children.Remove(e);
        else if (e is FrameworkElement { Parent: Border border }) border.Child = null;
        return e;
    }

    UIElement Pair(SettingsGroup left, SettingsGroup right)
    {
        var g = new Grid { ColumnSpacing = 16 };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition());
        var l = Group(left);
        var r = Group(right);
        Grid.SetColumn(r, 1);
        g.Children.Add(l);
        g.Children.Add(r);
        return g;
    }

    FrameworkElement Group(SettingsGroup group)
    {
        // Title, box, list buttons: the box takes the rest of the height (two groups side by side are the same height).
        var s = new Grid { RowSpacing = 6 };
        s.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        s.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        s.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        if (group.Title.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = group.Title.ToUpperInvariant(), FontSize = 11, FontWeight = FontWeights.SemiBold, CharacterSpacing = 40, Foreground = Res("TextFillColorSecondaryBrush"), Margin = new Thickness(4, 0, 0, 0) });
        }
        var rows = new StackPanel();
        bool first = true;
        foreach (var row in group.Rows)
        {
            if (!first) rows.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush"), Margin = new Thickness(12, 0, 12, 0) });
            first = false;
            rows.Children.Add(Row(row, 0));
        }
        var box = new Border { Child = rows, CornerRadius = new CornerRadius(8), Background = Res("CardBackgroundFillColorDefaultBrush"), BorderBrush = Res("CardStrokeColorDefaultBrush"), BorderThickness = new Thickness(1), VerticalAlignment = VerticalAlignment.Stretch };
        Grid.SetRow(box, 1);
        s.Children.Add(box);
        if (group.ListButtons is SettingsListButtons lb)
        {
            var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
            buttons.Children.Add(SymbolButton("", lb.Add));
            buttons.Children.Add(SymbolButton("", lb.Remove));
            Grid.SetRow(buttons, 2);
            s.Children.Add(buttons);
        }
        return s;
    }

    Button SymbolButton(string glyph, SettingsButton b)
    {
        var button = new Button { Content = new FontIcon { Glyph = glyph, FontSize = 12 }, Padding = new Thickness(8, 4, 8, 4), IsEnabled = b.Enabled };
        ToolTipService.SetToolTip(button, b.Title);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, b.Title);
        button.Click += (_, _) => Run(b.Command);
        return button;
    }

    FrameworkElement Row(SettingsRow row, int depth)
    {
        var quiet = row.Quiet ? 0.55 : 1.0;
        var pad = new Thickness(12 + depth * 20, 8, 12, 8);
        TextBlock Label(bool bold = false) => new() { Text = row.Label, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center, FontWeight = bold ? FontWeights.SemiBold : FontWeights.Normal };
        TextBlock Hint(string h) => new() { Text = h, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        var controls = Controls(row.Controls);
        FrameworkElement drawn;
        switch (row.Style)
        {
            case SettingsRowStyle.Note:
                drawn = Hint(row.Label);
                break;
            case SettingsRowStyle.Empty:
                var empty = Hint(row.Label);
                empty.HorizontalAlignment = HorizontalAlignment.Center;
                drawn = empty;
                break;
            case SettingsRowStyle.Warning:
                var warn = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
                warn.Children.Add(new FontIcon { Glyph = "", FontSize = 12, Foreground = Res("SystemFillColorCautionBrush") });
                warn.Children.Add(new TextBlock { Text = row.Label, Foreground = Res("SystemFillColorCautionBrush"), TextWrapping = TextWrapping.Wrap });
                drawn = warn;
                break;
            case SettingsRowStyle.Described:
                var dg = Columns(new GridLength(1, GridUnitType.Star), GridLength.Auto);
                var left = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
                left.Children.Add(Label());
                if (row.Hint is string dh) left.Children.Add(Hint(dh));
                dg.Children.Add(left);
                Place(dg, controls, 1);
                drawn = dg;
                break;
            case SettingsRowStyle.Wide:
                var wg = Columns(new GridLength(1, GridUnitType.Star), GridLength.Auto);
                if (row.Label.Length > 0)
                {
                    wg.Children.Add(Label());
                    Place(wg, controls, 1);
                }
                else
                {
                    controls.HorizontalAlignment = HorizontalAlignment.Left;
                    wg.Children.Add(controls);
                    if (row.Hint is string wh) Place(wg, Hint(wh), 1);
                }
                drawn = wg;
                if (row.Quiet) quiet = 0.5;
                break;
            case SettingsRowStyle.Item item:
                var ig = Columns(GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto);
                ig.Children.Add(new FontIcon { Glyph = Icon(item.Icon), FontSize = 16, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
                var it = new StackPanel { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
                it.Children.Add(Label(true));
                if (item.Detail.Length > 0) it.Children.Add(Hint(item.Detail));
                Place(ig, it, 1);
                Place(ig, controls, 2);
                var border = new Border { Child = ig, CornerRadius = new CornerRadius(6), Background = item.Selected ? Res("SubtleFillColorSecondaryBrush") : new SolidColorBrush(Microsoft.UI.Colors.Transparent) };
                if (item.Select is SettingsCommand select) border.Tapped += (_, _) => Run(select);
                if (item.Menus.Length > 0)
                {
                    var menu = new MenuFlyout();
                    foreach (var m in item.Menus)
                    {
                        var sub = new MenuFlyoutSubItem { Text = m.Title };
                        foreach (var e in m.Entries)
                        {
                            if (e is SettingsMenuEntry.Item mi)
                            {
                                MenuFlyoutItem x = mi.Checked ? new ToggleMenuFlyoutItem { IsChecked = true } : new MenuFlyoutItem();
                                x.Text = mi.Title;
                                var command = mi.Command;
                                x.Click += (_, _) => Run(command);
                                sub.Items.Add(x);
                            }
                            else sub.Items.Add(new MenuFlyoutSeparator());
                        }
                        menu.Items.Add(sub);
                    }
                    border.ContextFlyout = menu;
                }
                drawn = border;
                break;
            case SettingsRowStyle.Activity act:
                var ag = Columns(GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star));
                ag.Children.Add(Hint(act.Time));
                Place(ag, new TextBlock { Text = act.Client, FontWeight = FontWeights.SemiBold, VerticalAlignment = VerticalAlignment.Center }, 1);
                var what = new TextBlock { TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
                what.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = row.Label });
                if (act.Outcome is string outcome)
                {
                    what.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = $"  {outcome}", Foreground = Res("SystemFillColorCriticalBrush") });
                }
                Place(ag, what, 2);
                drawn = ag;
                break;
            case SettingsRowStyle.Disclosure dis:
                var outer = new StackPanel();
                var hg = Columns(GridLength.Auto, GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto, GridLength.Auto);
                var chevron = new Button { Content = new FontIcon { Glyph = dis.Expanded ? "" : "", FontSize = 10 }, Padding = new Thickness(4), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
                if (dis.ToggleHelp.Length > 0) ToolTipService.SetToolTip(chevron, dis.ToggleHelp);
                chevron.Click += (_, _) => Run(dis.Toggle);
                hg.Children.Add(chevron);
                Place(hg, new FontIcon { Glyph = Icon(dis.Icon), FontSize = 14, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center }, 1);
                Place(hg, Label(), 2);
                Place(hg, new TextBlock { Text = dis.Status, FontSize = 12, Foreground = dis.StatusAccent ? Res("AccentTextFillColorPrimaryBrush") : Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center }, 3);
                if (dis.WarningHelp is string wh2)
                {
                    var w = new FontIcon { Glyph = "", FontSize = 12, Foreground = Res("SystemFillColorCautionBrush"), VerticalAlignment = VerticalAlignment.Center };
                    ToolTipService.SetToolTip(w, wh2);
                    Place(hg, w, 4);
                }
                outer.Children.Add(new Border { Child = hg, Padding = pad });
                if (dis.Expanded)
                {
                    foreach (var child in dis.Children)
                    {
                        outer.Children.Add(Row(child, depth + 1));
                    }
                }
                outer.Opacity = quiet;
                return outer;
            default:
                // Standard: the label, the controls, the hint on the right.
                var sg = Columns(new GridLength(155), new GridLength(1, GridUnitType.Star), GridLength.Auto);
                sg.Children.Add(Label());
                controls.HorizontalAlignment = HorizontalAlignment.Left;
                Place(sg, controls, 1);
                if (row.Hint is string sh)
                {
                    var hint = Hint(sh);
                    if (row.Controls.Length >= 4 || drawingRules)
                    {
                        // Many controls, or the rules editor (drawn compact): the hint under them, so neither is cut.
                        sg.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
                        sg.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
                        Grid.SetRow(hint, 1);
                        Grid.SetColumnSpan(hint, 2);
                        Place(sg, hint, 1);
                    }
                    else
                    {
                        hint.TextAlignment = TextAlignment.Right;
                        hint.MaxWidth = 260;
                        Place(sg, hint, 2);
                    }
                }
                drawn = sg;
                break;
        }
        var wrap = new Border { Child = drawn, Padding = pad, Opacity = quiet };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(wrap, row.Id);
        return wrap;
    }

    static Grid Columns(params GridLength[] widths)
    {
        var g = new Grid { ColumnSpacing = 10 };
        foreach (var w in widths) g.ColumnDefinitions.Add(new ColumnDefinition { Width = w });
        return g;
    }

    static void Place(Grid g, FrameworkElement e, int column)
    {
        Grid.SetColumn(e, column);
        g.Children.Add(e);
    }

    static string Icon(SettingsIcon icon) => icon switch
    {
        SettingsIcon.Content c => c.Category switch
        {
            Category.Movie => "",
            Category.Tv => "",
            Category.Music => "",
            Category.Book => "",
            Category.Software => "",
            Category.Game => "",
            _ => "",
        },
        _ => "",
    };

    // ---------------------------------------------------------------------------------------------- controls

    StackPanel Controls(SettingsControl[] controls)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center, HorizontalAlignment = HorizontalAlignment.Right };
        foreach (var c in controls)
        {
            s.Children.Add(Control(c));
        }
        return s;
    }

    static void Help(FrameworkElement e, string? help)
    {
        if (!string.IsNullOrEmpty(help)) ToolTipService.SetToolTip(e, help);
    }

    FrameworkElement Control(SettingsControl c)
    {
        // A control of the rules editor sends its change there (RulesChange.Set), not to settings_apply.
        var rules = drawingRules;
        switch (c)
        {
            case SettingsControl.Toggle t:
                if (t.Checkbox)
                {
                    var box = new CheckBox { IsChecked = t.On, IsEnabled = t.Enabled, MinWidth = 0 };
                    Help(box, t.Help);
                    box.Click += (_, _) => Deliver(rules, t.Key, new SettingsValue.Flag(box.IsChecked == true));
                    return box;
                }
                var sw = new ToggleSwitch { IsOn = t.On, IsEnabled = t.Enabled, OnContent = "", OffContent = "", MinWidth = 0 };
                Help(sw, t.Help);
                sw.Toggled += (_, _) => { if (sw.IsOn != t.On) Deliver(rules, t.Key, new SettingsValue.Flag(sw.IsOn)); };
                return sw;
            case SettingsControl.Choice ch when ch.Style == SettingsChoiceStyle.Segmented:
                var seg = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2 };
                foreach (var o in ch.Options)
                {
                    var b = new Button { Content = o.Title, IsEnabled = ch.Enabled && o.Enabled, Padding = new Thickness(10, 4, 10, 5) };
                    if (o.Id == ch.Selected) b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
                    var id = o.Id;
                    b.Click += (_, _) => Deliver(rules, ch.Key, new SettingsValue.Choice(id));
                    seg.Children.Add(b);
                }
                Help(seg, ch.Help);
                return seg;
            case SettingsControl.Choice ch:
                var combo = new ComboBox { IsEnabled = ch.Enabled, MinWidth = 140 };
                foreach (var o in ch.Options) combo.Items.Add(new ComboBoxItem { Content = o.Title, IsEnabled = o.Enabled, Tag = o.Id });
                combo.SelectedIndex = Array.FindIndex(ch.Options, o => o.Id == ch.Selected);
                combo.SelectionChanged += (_, _) =>
                {
                    if (combo.SelectedItem is ComboBoxItem { Tag: string id } && id != ch.Selected) Deliver(rules, ch.Key, new SettingsValue.Choice(id));
                };
                Help(combo, ch.Help);
                return combo;
            case SettingsControl.Field f:
                return Field(f);
            case SettingsControl.Folder fo:
                var fp = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
                var path = new TextBox { Text = fo.Path, PlaceholderText = fo.Placeholder, Width = fo.Width > 0 ? fo.Width : 280 };
                Help(path, fo.Help);
                path.TextChanged += (_, _) => { if (path.Text != fo.Path) Deliver(rules, fo.Key, new SettingsValue.Typed(path.Text), redraw: false); };
                path.LostFocus += (_, _) => Draw();
                fp.Children.Add(path);
                fp.Children.Add(ButtonControl(fo.Choose));
                if (fo.Clearable && fo.Path.Length > 0)
                {
                    var clear = new Button { Content = new FontIcon { Glyph = "", FontSize = 10 }, Padding = new Thickness(6), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
                    clear.Click += (_, _) => Deliver(rules, fo.Key, new SettingsValue.Typed(""));
                    fp.Children.Add(clear);
                }
                return fp;
            case SettingsControl.Button bt:
                return ButtonControl(bt.Action);
            case SettingsControl.Link l:
                var link = new HyperlinkButton { Content = l.Title, Padding = new Thickness(0) };
                link.Click += (_, _) => OpenUrl(l.Url);
                return link;
            case SettingsControl.Label lb:
                var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 5, VerticalAlignment = VerticalAlignment.Center };
                if (lb.Symbol is SettingsSymbol sym) line.Children.Add(new FontIcon { Glyph = Glyph(sym), FontSize = lb.Small ? 11 : 13, Foreground = Tone(lb.Tone) });
                line.Children.Add(new TextBlock { Text = lb.Text, FontSize = lb.Small ? 12 : 14, Foreground = Tone(lb.Tone), TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true, VerticalAlignment = VerticalAlignment.Center });
                if (lb.Identifier is string lid) Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(line, lid);
                return line;
            case SettingsControl.Checklist cl:
                var dd = new DropDownButton { Content = cl.Title };
                var flyout = new MenuFlyout();
                foreach (var ck in cl.Checks)
                {
                    var item = new ToggleMenuFlyoutItem { Text = ck.Title, IsChecked = ck.Checked };
                    var id = ck.Id;
                    item.Click += (_, _) => Deliver(rules, cl.Key, new SettingsValue.Choice(id));
                    flyout.Items.Add(item);
                }
                dd.Flyout = flyout;
                return dd;
            case SettingsControl.Chips chips:
                // In the rules editor (drawn compact) the chips wrap, four to a line.
                var chipLines = new StackPanel { Spacing = 4 };
                var cp = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
                chipLines.Children.Add(cp);
                foreach (var ck in chips.Checks)
                {
                    if (rules && cp.Children.Count == 4)
                    {
                        cp = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
                        chipLines.Children.Add(cp);
                    }
                    var chip = new ToggleButton { Content = ck.Title, IsChecked = ck.Checked, Padding = new Thickness(10, 3, 10, 4), CornerRadius = new CornerRadius(12) };
                    ToolTipService.SetToolTip(chip, (ck.Checked ? chips.HelpOn : chips.HelpOff).Replace("{}", ck.Title));
                    var id = ck.Id;
                    chip.Click += (_, _) => Deliver(rules, chips.Key, new SettingsValue.Choice(id));
                    cp.Children.Add(chip);
                }
                return chipLines;
            default:
                return new TextBlock();
        }
    }

    /// <summary>
    /// A field: what is typed (and the unit picked) stays while it has focus; it is sent on Enter, on leaving it, when
    /// the unit changes and, for live fields, after 600 ms without typing; text the core would refuse is drawn red.
    /// </summary>
    FrameworkElement Field(SettingsControl.Field f)
    {
        var rules = drawingRules;
        var box = new TextBox
        {
            Text = f.Text,
            PlaceholderText = f.Placeholder,
            IsEnabled = f.Enabled,
            Width = f.Width > 0 ? f.Width : double.NaN,
            MinWidth = f.Width > 0 ? 0 : 220,
            TextAlignment = f.Trailing ? TextAlignment.Right : TextAlignment.Left,
            FontFamily = f.Kind == SettingsFieldKind.Address ? new FontFamily("Consolas") : FontFamily.XamlAutoFontFamily,
        };
        Help(box, f.Help);
        if (f.Identifier is string fid) Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(box, fid);
        ComboBox? units = null;
        var unit = f.Unit;
        var normal = box.Foreground;
        void Send()
        {
            var text = box.Text;
            if (text == f.Text && unit == f.Unit) return;
            Deliver(rules, f.Key, f.Units.Length > 0 ? new SettingsValue.Measure(text, unit) : new SettingsValue.Typed(text));
        }
        Action send = Send;
        box.TextChanged += (_, _) =>
        {
            box.Foreground = SeedwellFfiMethods.SettingsFieldValid(f.Kind, box.Text, unit) ? normal : Res("SystemFillColorCriticalBrush");
            if (f.Live) Typed(send);
        };
        box.KeyDown += (_, e) =>
        {
            if (e.Key == Windows.System.VirtualKey.Enter) { e.Handled = true; Untyped(send); Send(); }
        };
        box.LostFocus += (_, _) => { Untyped(send); Send(); Draw(); };
        if (f.Units.Length == 0)
        {
            return box;
        }
        units = new ComboBox { IsEnabled = f.Enabled, MinWidth = 0 };
        foreach (var u in f.Units) units.Items.Add(u);
        units.SelectedItem = f.Unit;
        units.SelectionChanged += (_, _) =>
        {
            if (units.SelectedItem is not string next || next == unit) return;
            // A new unit keeps the value: the number converted (the core's settings_field_convert).
            box.Text = SeedwellFfiMethods.SettingsFieldConvert(f.Kind, box.Text, unit, next);
            unit = next;
            Send();
        };
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        s.Children.Add(box);
        s.Children.Add(units);
        return s;
    }

    ButtonBase ButtonControl(SettingsButton b)
    {
        var rules = drawingRules;
        ButtonBase button = b.Style switch
        {
            SettingsButtonStyle.Link => new HyperlinkButton { Content = b.Title, Padding = new Thickness(0) },
            SettingsButtonStyle.Remove => new Button { Content = new FontIcon { Glyph = "", FontSize = 12 }, Padding = new Thickness(6) },
            SettingsButtonStyle.Add => new Button { Content = AddContent(b.Title), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0), Foreground = Res("AccentTextFillColorPrimaryBrush") },
            _ => new Button { Content = b.Title },
        };
        if (b.Style == SettingsButtonStyle.Prominent && button is Button) button.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
        button.IsEnabled = b.Enabled;
        Help(button, b.Style == SettingsButtonStyle.Remove ? b.Title : b.Help);
        if (b.Identifier is string id) Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(button, id);
        button.Click += (_, _) => Run(b.Command, rules);
        return button;
    }

    static StackPanel AddContent(string title)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        s.Children.Add(new FontIcon { Glyph = "", FontSize = 11 });
        s.Children.Add(new TextBlock { Text = title });
        return s;
    }

    static Brush Tone(SettingsTone t) => t switch
    {
        SettingsTone.Secondary => Res("TextFillColorSecondaryBrush"),
        SettingsTone.Accent => Res("AccentTextFillColorPrimaryBrush"),
        SettingsTone.Warning => Res("SystemFillColorCautionBrush"),
        SettingsTone.Problem => Res("SystemFillColorCriticalBrush"),
        SettingsTone.Good => Res("SystemFillColorSuccessBrush"),
        _ => Res("TextFillColorPrimaryBrush"),
    };

    static string Glyph(SettingsSymbol s) => s switch
    {
        SettingsSymbol.Check => "",
        SettingsSymbol.Mapped => "",
        SettingsSymbol.Checking => "",
        SettingsSymbol.Closed => "",
        SettingsSymbol.Unknown => "",
        _ => "",
    };

    // ------------------------------------------------------------------------------------------------ changes

    void Set(SettingKey key, SettingsValue value, bool redraw = true) => Apply(new SettingsChange.Set(key, value), redraw);

    /// <summary>A control's change: the rules editor's own (its controls), else Settings'.</summary>
    /// <summary>A key in a live field: its send is due in a moment, and another field's that was still waiting goes now.</summary>
    void Typed(Action send)
    {
        if (typedSend is not null && typedSend != send) typedSend();
        typedSend = send;
        typed.Stop();
        typed.Start();
    }

    /// <summary>The field sends now (Enter, or the focus left it): nothing more is due from it.</summary>
    void Untyped(Action send)
    {
        if (typedSend != send) return;
        typedSend = null;
        typed.Stop();
    }

    void Deliver(bool rules, SettingKey key, SettingsValue value, bool redraw = true)
    {
        if (rules) RulesApply(new RulesChange.Set(key, value), redraw); else Set(key, value, redraw);
    }

    /// <summary>A change: the core's rule, then the saves, the port and the question it names.</summary>
    internal void Apply(SettingsChange change, bool redraw = true)
    {
        SettingsApplied r;
        try { r = SeedwellFfiMethods.SettingsApply(state, change); }
        catch (Exception e) { AppLog.Write($"settings apply failed: {e.Message}"); return; }
        state = r.State;
        AppLog.Write($"settings: {change.GetType().Name}{(r.Refused ? " (refused)" : "")} -> {string.Join(",", r.Saves)}");
        foreach (var save in r.Saves)
        {
            switch (save)
            {
                case SettingsStore.Settings:
                    saveSettings.Stop();
                    if (r.SaveNow) SaveSettings(); else saveSettings.Start();
                    break;
                case SettingsStore.Sync:
                    saveSync.Stop();
                    saveSync.Start();
                    break;
                case SettingsStore.Rules:
                    var rules = state.Rules;
                    CoreCalls.InOrder("rules save", () => app.Core.RulesSave(rules), () => { }, e => app.Banner.Problem(e));
                    break;
            }
        }
        if (r.SyncPort is ushort port)
        {
            syncPortMovingTo = port;
            CoreCalls.InOrder("sync port", () => app.Core.SetSyncPort(port), Draw, e => { syncPortMovingTo = null; Apply(new SettingsChange.SyncPortRefused(e)); });
        }
        if (r.Ask is SettingsQuestion q) Ask(q);
        if (redraw || r.Refused) Draw();
    }

    void SaveSettings()
    {
        saveSettings.Stop();
        var settings = state.Settings;
        bool saved = true;
        CoreCalls.InOrder("settings save", () => app.Core.SettingsSave(settings), () => { if (saved) app.Banner.Saved(); app.Updater.Schedule(); }, e => { saved = false; app.Banner.Problem(e); });
    }

    void SaveSync()
    {
        saveSync.Stop();
        if (state.Sync is not SyncSettings sync) return;
        CoreCalls.InOrder("sync settings save", () => app.Core.SetSyncSettings(sync), () => { }, e => app.Banner.Problem(e));
    }

    /// <summary>What a control's command does: a change, or what the system does (pickers, the clipboard, Explorer, the updater, the tool).</summary>
    internal async void Run(SettingsCommand command, bool rules = false)
    {
        AppLog.Write($"settings command: {command.GetType().Name}");
        switch (command)
        {
            // The rules editor's controls change its draft; its commands are its own (RulesChange).
            case SettingsCommand.Apply { Change: SettingsChange.Set rs } when rules: RulesApply(new RulesChange.Set(rs.Key, rs.Value)); break;
            case SettingsCommand.Rules r: RulesApply(r.Change); break;
            case SettingsCommand.Apply a: Apply(a.Change); break;
            case SettingsCommand.ChooseFolder cf:
                if (await app.PickFolder() is string folder) Deliver(rules, cf.Key, new SettingsValue.Typed(folder));
                break;
            case SettingsCommand.ChooseWatchFolder:
                if (await app.PickFolder() is string watch) Apply(new SettingsChange.AddWatchFolder(watch));
                break;
            case SettingsCommand.CheckForUpdates: app.Updater.Check(false); break;
            case SettingsCommand.InstallUpdate: app.Updater.Install(); break;
            case SettingsCommand.ShowUpdateLog: app.Updater.ShowLog(); break;
            case SettingsCommand.InstallBuild:
                if ((await app.PickFiles([".zip"])).FirstOrDefault() is string zip)
                {
                    CoreCalls.InOrder("install build", () => app.Updater.Updates.InstallBuild(zip, null, App.Now()), _ => Draw(), e => app.Alert(new SyncAlert(e, "")));
                }
                break;
            case SettingsCommand.StopWatchingBuilds: app.Updater.Updates.StopWatching(); Draw(); break;
            case SettingsCommand.OpenUrl u: OpenUrl(u.Url); break;
            case SettingsCommand.CopyCliPath cp:
                if (!Quiet.On)
                {
                    var data = new Windows.ApplicationModel.DataTransfer.DataPackage();
                    data.SetText(cp.Text);
                    Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(data);
                }
                Apply(new SettingsChange.Copied(true));
                await Task.Delay((int)(model?.CopiedMs ?? 1500));
                Apply(new SettingsChange.Copied(false));
                break;
            case SettingsCommand.Ask a: Ask(a.Question); break;
            case SettingsCommand.InstallSkill s:
                var (ok, err) = await Tool(["skill", "install", "--dir", s.Dir, .. (s.Force ? new[] { "--force" } : [])]);
                Apply(ok ? new SettingsChange.SkillInstalled(s.Dir) : new SettingsChange.SkillFailed(s.Dir, err));
                RefreshCli();
                Draw();
                break;
            case SettingsCommand.RemoveSkill rs:
                var (removed, rerr) = await Tool(["skill", "uninstall", "--dir", rs.Dir]);
                Apply(removed ? new SettingsChange.SkillRemoved(rs.Dir) : new SettingsChange.SkillRemoveFailed(rs.Dir, rerr));
                RefreshCli();
                Draw();
                break;
            case SettingsCommand.ShowInFileManager sm:
                if (!Quiet.On) Shell.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"/select,\"{sm.Path}\"") { UseShellExecute = true });
                break;
            case SettingsCommand.ShowTrackerLog:
                var log = app.Core.TrackerLogPath();
                var stateDir = app.Core.StateDir();
                // (What is there is looked at off the UI thread too.)
                _ = Task.Run(() =>
                {
                var shown = File.Exists(log) ? $"/select,\"{log}\"" : $"\"{(Directory.Exists(Path.GetDirectoryName(log)) ? Path.GetDirectoryName(log) : stateDir)}\"";
                if (!Quiet.On) Shell.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", shown) { UseShellExecute = true });
                });
                break;
            case SettingsCommand.ClearTrackerLog:
                CoreCalls.InOrder("tracker log clear", app.Core.TrackerLogClear, Draw);
                break;
            case SettingsCommand.DecideAgentRequest d:
                app.Notifier?.Decide(d.Id, d.Approve, Draw);
                break;
            case SettingsCommand.ShowTrackerTorrents t:
                app.ShowPlace(new LibraryPlace.Tracker(t.TrackerId));
                break;
            default:
                // LinkCli, the Finder's commands, the tracker log viewer and the rules editor are never offered here.
                AppLog.Write($"settings command {command.GetType().Name}: not on Windows");
                break;
        }
    }

    void OpenUrl(string url)
    {
        AppLog.Write("settings: open url");
        if (!Quiet.On && Uri.TryCreate(url, UriKind.Absolute, out var uri)) _ = Windows.System.Launcher.LaunchUriAsync(uri);
    }

    /// <summary>The bundled command-line tool (never against the app's control socket); true when it succeeded, with its error output.</summary>
    static async Task<(bool, string)> Tool(string[] args)
    {
        try
        {
            var psi = new System.Diagnostics.ProcessStartInfo(Bundled) { RedirectStandardError = true, RedirectStandardOutput = true, UseShellExecute = false, CreateNoWindow = true };
            foreach (var a in args) psi.ArgumentList.Add(a);
            using var p = await Task.Run(() => System.Diagnostics.Process.Start(psi)!);
            var err = await p.StandardError.ReadToEndAsync();
            await p.WaitForExitAsync();
            return (p.ExitCode == 0, err.Trim());
        }
        catch (Exception e)
        {
            return (false, e.Message);
        }
    }

    /// <summary>The core's question: one button per choice (each runs its command), and Cancel.</summary>
    async void Ask(SettingsQuestion q)
    {
        AppLog.Write($"settings asks: {q.Title}");
        if (Quiet.On || Root.XamlRoot is not XamlRoot root)
        {
            return;
        }
        var d = new ContentDialog { XamlRoot = root, Title = q.Title, Content = new TextBlock { Text = q.Text, TextWrapping = TextWrapping.Wrap }, CloseButtonText = q.CancelTitle, DefaultButton = ContentDialogButton.Primary };
        if (q.Choices.Length > 0) d.PrimaryButtonText = q.Choices[0].Title;
        if (q.Choices.Length > 1) d.SecondaryButtonText = q.Choices[1].Title;
        ContentDialogResult answer;
        try { answer = await d.ShowAsync(); } catch { return; }
        if (answer == ContentDialogResult.Primary) Run(q.Choices[0].Command);
        else if (answer == ContentDialogResult.Secondary && q.Choices.Length > 1) Run(q.Choices[1].Command);
    }

    // --------------------------------------------------------------------------------------------------- QA

    /// <summary>QA: every row of the tab shown, by id (style, controls), and the tabs.</summary>
    internal string QaDescribe()
    {
        if (model is null) return "no model";
        IEnumerable<SettingsRow> All(SettingsRow r) => r.Style is SettingsRowStyle.Disclosure d ? new[] { r }.Concat(d.Children.SelectMany(All)) : [r];
        var rows = model.Blocks.SelectMany(b => b switch
        {
            SettingsBlock.Single s => s.Group.Rows,
            SettingsBlock.Pair p => p.Left.Rows.Concat(p.Right.Rows),
            _ => [],
        }).SelectMany(All);
        return $"tab {model.Tab}, tabs [{string.Join(",", model.Tabs.Select(t => t.Tab))}], blocks [{string.Join(",", model.Blocks.Select(b => b.GetType().Name))}], rows {rows.Count()}, banner {(model.Banner is null ? "none" : "yes")}";
    }

    /// <summary>QA: what a control of row <paramref name="rowId"/> would send: on|off for a switch, an option id, field text (and unit), a button's title.</summary>
    internal string QaSet(string rowId, string[] value)
    {
        if (model is null) return "no model";
        var (row, rules) = QaAllRows().FirstOrDefault(r => r.Row.Id == rowId);
        if (row is null) return "no such row";
        var v = value.Length > 0 ? value[0] : "";
        foreach (var c in row.Controls)
        {
            switch (c)
            {
                case SettingsControl.Toggle t when v is "on" or "off": Deliver(rules, t.Key, new SettingsValue.Flag(v == "on")); return "set";
                case SettingsControl.Choice ch when ch.Options.Any(o => o.Id == v): Deliver(rules, ch.Key, new SettingsValue.Choice(v)); return "set";
                case SettingsControl.Field f: Deliver(rules, f.Key, f.Units.Length > 0 ? new SettingsValue.Measure(v, value.Length > 1 ? value[1] : f.Unit) : new SettingsValue.Typed(string.Join(' ', value))); return "set";
                case SettingsControl.Folder fo: Deliver(rules, fo.Key, new SettingsValue.Typed(string.Join(' ', value))); return "set";
                case SettingsControl.Button b when b.Action.Title.StartsWith(v): Run(b.Action.Command, rules); return "pressed";
            }
        }
        return "no control for it";
    }

    /// <summary>QA: the ids of the tab's rows, in order.</summary>
    internal string QaRows()
    {
        if (model is null) return "no model";
        return string.Join(" ", QaAllRows().Select(r => r.Row.Id));
    }

    /// <summary>QA: every row of the tab (a disclosure's children too), the rules editor's among them (Rules: its controls are the editor's).</summary>
    IEnumerable<(SettingsRow Row, bool Rules)> QaAllRows()
    {
        if (model is null) yield break;
        IEnumerable<SettingsRow> All(SettingsRow r) => r.Style is SettingsRowStyle.Disclosure d ? new[] { r }.Concat(d.Children.SelectMany(All)) : [r];
        foreach (var b in model.Blocks)
        {
            var (rows, rules) = b switch
            {
                SettingsBlock.Single s => (s.Group.Rows.AsEnumerable(), false),
                SettingsBlock.Pair p => (p.Left.Rows.Concat(p.Right.Rows), false),
                SettingsBlock.RulesEditor => ((RulesModel?.Groups ?? []).SelectMany(g => g.Rows), true),
                _ => (Enumerable.Empty<SettingsRow>(), false),
            };
            foreach (var r in rows.SelectMany(All)) yield return (r, rules);
        }
    }

    /// <summary>QA: the text of a row's controls as drawn (labels, fields).</summary>
    internal string QaRow(string rowId)
    {
        if (model is null) return "no model";
        var row = QaAllRows().FirstOrDefault(r => r.Row.Id == rowId).Row;
        if (row is null) return "no such row";
        return $"{row.Label}: " + string.Join(" | ", row.Controls.Select(c => c switch
        {
            SettingsControl.Toggle t => t.On ? "on" : "off",
            SettingsControl.Choice ch => ch.Selected,
            SettingsControl.Field f => $"{f.Text} {f.Unit}".Trim(),
            SettingsControl.Folder fo => fo.Path,
            SettingsControl.Label l => l.Text,
            SettingsControl.Button b => $"[{b.Action.Title}]",
            _ => c.GetType().Name,
        }));
    }
}
