using System.ComponentModel;
using System.Diagnostics;
using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The library window's Folder Sync page (SPEC 7G.5, the Mac's SyncPage), drawn from the core's <c>sync_page_model</c>
/// (seedwell-ffi sync_page.rs): the folders on the left with their context menus and Add Folder…, and the selected folder on
/// the right: its header and buttons, where it is, its status, the devices gone and Share Again, the Stop Syncing
/// question, and its Members, Conflicts and Issues. What is there, every word and what each item does are the core's;
/// this draws it, reads the members (every 2 s) and conflicts (every 10 s) off the UI thread, and runs the
/// <c>SyncPageCommand</c>s, asking an item's question first.
/// </summary>
sealed class FolderSyncPage
{
    readonly App app;
    // The page's own Stop Syncing question, separate from the tray's.
    readonly SyncRemoval removal = new();
    readonly Grid root = new();
    readonly DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(1) };
    // Above the page in all its states: "Other devices can't reach this PC" (the core's SyncReach).
    readonly Grid frame = new();
    readonly StackPanel reachBanner = new() { Margin = new Thickness(16, 12, 16, 4), Visibility = Visibility.Collapsed };
    readonly DispatcherTimer reachTimer = new();
    SyncReachModel? reachModel;
    bool reachRunning;
    // Runs the core's command elevated (a QA run gives another: it never elevates).
    Action<ProcessStartInfo> reachStart = psi => { using var p = Process.Start(psi); p?.WaitForExit(30_000); };
    string? selected;
    SyncMember[] members = [];
    SyncConflict[] conflicts = [];
    SyncPageModel? model;
    string drawn = "";
    // The page's structure as drawn (the model's Structure): while it stays, a new model only changes values, which the
    // setters below put into the elements drawn, instead of the page being built again.
    string drawnStructure = "";
    readonly List<Action<SyncPageModel>> values = [];
    // The status line as drawn, and how often the page was built and updated in place (QA).
    TextBlock? statusText;
    internal int Draws { get; private set; }
    internal int InPlace { get; private set; }
    // When the members and conflicts were last read (the page model's MembersRefreshMs / ConflictsRefreshMs pace them).
    DateTime membersRead = DateTime.MinValue;
    DateTime conflictsRead = DateTime.MinValue;

    public UIElement Root => frame;

    public FolderSyncPage(App app)
    {
        this.app = app;
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition());
        frame.Children.Add(reachBanner);
        Grid.SetRow(root, 1);
        frame.Children.Add(root);
        reachTimer.Tick += (_, _) => PollReach();
        frame.Loaded += (_, _) => PollReach();
        frame.Unloaded += (_, _) => reachTimer.Stop();
        timer.Tick += (_, _) =>
        {
            // The core's pace, with the Mac's leeway of one refresh (200 ms).
            if (model is not SyncPageModel m) return;
            var now = DateTime.UtcNow;
            if ((now - membersRead).TotalMilliseconds + 200 >= m.MembersRefreshMs) LoadMembers();
            if ((now - conflictsRead).TotalMilliseconds + 200 >= m.ConflictsRefreshMs) LoadConflicts();
        };
        root.Loaded += (_, _) => timer.Start();
        root.Unloaded += (_, _) => timer.Stop();
        Update();
    }

    /// <summary>The folder shown on the right (QA).</summary>
    internal string? Chosen => model?.Selected;

    /// <summary>The model last drawn (QA).</summary>
    internal SyncPageModel? Model => model;

    /// <summary>Shows folder <paramref name="id"/> on the right (a click on its row, a seedwell://folder/ link).</summary>
    internal void Choose(string id)
    {
        if (selected == id)
        {
            return;
        }
        selected = id;
        members = [];
        conflicts = [];
        Update();
        LoadMembers();
        LoadConflicts();
    }

    /// <summary>Asks the core for the page and redraws when its signature changed (on every model refresh).</summary>
    public void Update()
    {
        SyncPageModel m;
        try
        {
            var input = new SyncPageInput(app.Core.SyncSnapshot(), selected, members, conflicts, app.Core.ChatChannels(), false, (ulong)DateTimeOffset.UtcNow.ToUnixTimeSeconds());
            m = SeedwellFfiMethods.SyncPageModel(input, removal, new Clock(), App.HostWindows, App.HostFeatures);
        }
        catch (Exception e)
        {
            AppLog.Write($"sync page failed: {e.Message}");
            return;
        }
        model = m;
        if (m.Selected != selected)
        {
            // The core's choice (none asked, or gone): the first folder. Its members and conflicts are read for it.
            selected = m.Selected;
            members = [];
            conflicts = [];
            LoadMembers();
            LoadConflicts();
        }
        if (m.Signature == drawn)
        {
            return;
        }
        drawn = m.Signature;
        // Only values changed (about twice a second while a folder is added): into the elements drawn. Building the page
        // anew each time made so many WinRT objects that every few draws a blocking full collection of 50-70 ms followed
        // (the check interactive, win-100).
        if (m.Structure == drawnStructure)
        {
            foreach (var v in values)
            {
                v(m);
            }
            InPlace++;
            return;
        }
        drawnStructure = m.Structure;
        Draw(m);
    }

    void Redraw()
    {
        drawn = "";
        drawnStructure = "";
        Update();
    }

    /// <summary>A setter of a text drawn: sets it only when it changed.</summary>
    static Action<string> Texter(TextBlock t, string now)
    {
        var last = now;
        return text =>
        {
            if (text != last)
            {
                last = text;
                t.Text = text;
            }
        };
    }

    void LoadMembers()
    {
        if (selected is not string id)
        {
            return;
        }
        membersRead = DateTime.UtcNow;
        _ = Task.Run(() => app.Core.SyncMembers(id)).ContinueWith(t =>
        {
            if (t.IsCompletedSuccessfully) root.DispatcherQueue.TryEnqueue(() => { if (selected == id) { members = t.Result; Update(); } });
        });
    }

    void LoadConflicts()
    {
        if (selected is not string id)
        {
            return;
        }
        conflictsRead = DateTime.UtcNow;
        _ = Task.Run(() => app.Core.SyncConflicts(id)).ContinueWith(t =>
        {
            if (t.IsCompletedSuccessfully) root.DispatcherQueue.TryEnqueue(() => { if (selected == id) { conflicts = t.Result; Update(); } });
        });
    }

    // ------------------------------------------------------------------------------------------- can others reach it

    /// <summary>Asks the core at its pace (instant: a lock) and draws the banner when it changed.</summary>
    internal void PollReach()
    {
        var m = app.Reach.Model();
        reachTimer.Interval = TimeSpan.FromMilliseconds(m.RefreshMs);
        reachTimer.Start();
        if (m != reachModel)
        {
            reachModel = m;
            DrawReach();
        }
    }

    void DrawReach()
    {
        reachBanner.Children.Clear();
        if (reachModel?.Notice is not SyncReachNotice n)
        {
            reachBanner.Visibility = Visibility.Collapsed;
            return;
        }
        var line = new Grid { ColumnSpacing = 12 };
        line.ColumnDefinitions.Add(new ColumnDefinition());
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var text = new StackPanel { Spacing = 2 };
        text.Children.Add(new TextBlock { Text = n.Title, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap });
        text.Children.Add(new TextBlock { Text = n.Body, FontSize = 12, Foreground = Res("SystemFillColorCautionBrush"), TextWrapping = TextWrapping.Wrap });
        line.Children.Add(text);
        if (n.Action is SyncReachAction action)
        {
            var b = new Button { Content = action.Title, IsEnabled = !reachRunning, VerticalAlignment = VerticalAlignment.Center };
            b.Click += (_, _) => AllowThrough(action);
            Grid.SetColumn(b, 1);
            line.Children.Add(b);
        }
        reachBanner.Children.Add(line);
        reachBanner.Visibility = Visibility.Visible;
    }

    /// <summary>The notice's action: the core's command, elevated, off the UI thread. The user saying No to UAC changes nothing: no
    /// recheck, which would read the same rule and take it for a policy. Any other ending has the core read the rule again.</summary>
    async void AllowThrough(SyncReachAction action)
    {
        reachRunning = true;
        DrawReach();
        var psi = new ProcessStartInfo(action.Program, action.Arguments) { UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden };
        var cancelled = false;
        try
        {
            await Task.Run(() => reachStart(psi));
        }
        catch (Win32Exception e) when (e.NativeErrorCode == 1223)
        {
            cancelled = true;
        }
        catch (Exception e)
        {
            AppLog.Write($"reach: the command did not run: {e.Message}");
        }
        AppLog.Write($"reach: allow through the firewall {(cancelled ? "cancelled" : "ran")}");
        if (!cancelled)
        {
            await Task.Run(() => app.Reach.Recheck());
        }
        reachRunning = false;
        reachModel = null;
        PollReach();
        DrawReach();
    }

    /// <summary>QA: the action as the button runs it, with UAC's answer given: never the real command.</summary>
    internal void QaAllow(bool userSaysNo)
    {
        if (reachModel?.Notice?.Action is SyncReachAction action)
        {
            reachStart = _ => { if (userSaysNo) throw new Win32Exception(1223); };
            AllowThrough(action);
        }
    }

    /// <summary>The banner as drawn (QA).</summary>
    internal string QaReachDrawn() => $"banner {reachBanner.Visibility}, buttons [{string.Join(", ", RowCommands.Tree(reachBanner).OfType<Button>().Select(b => $"{b.Content} {(b.IsEnabled ? "on" : "off")}"))}]";

    // ------------------------------------------------------------------------------------------------ drawing

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Faint(string text, double size = 12) => new() { Text = text, FontSize = size, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };

    void Draw(SyncPageModel m)
    {
        Draws++;
        values.Clear();
        statusText = null;
        root.Children.Clear();
        root.ColumnDefinitions.Clear();
        if (m.Empty is SyncPageEmpty e)
        {
            var s = new StackPanel { Spacing = 10, HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(0, 60, 0, 0) };
            s.Children.Add(new TextBlock { Text = e.Text, FontSize = 13, Foreground = Res("TextFillColorSecondaryBrush"), HorizontalAlignment = HorizontalAlignment.Center });
            if (e.Add is SyncMenuItem add)
            {
                var b = new Button { Content = add.Title, HorizontalAlignment = HorizontalAlignment.Center };
                Help(b, add.Help);
                b.Click += (_, _) => Run(new SyncPageCommand.AddFolder(), null);
                s.Children.Add(b);
            }
            root.Children.Add(s);
            return;
        }
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(260) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        root.ColumnDefinitions.Add(new ColumnDefinition());
        root.Children.Add(FolderList(m));
        var divider = new Microsoft.UI.Xaml.Shapes.Rectangle { Width = 1, Fill = Res("DividerStrokeColorDefaultBrush") };
        Grid.SetColumn(divider, 1);
        root.Children.Add(divider);
        if (m.Page is SyncFolderPage p)
        {
            var right = new ScrollViewer { Content = Page(p) };
            Grid.SetColumn(right, 2);
            root.Children.Add(right);
        }
    }

    UIElement FolderList(SyncPageModel m)
    {
        var g = new Grid();
        g.RowDefinitions.Add(new RowDefinition());
        g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var list = new StackPanel { Padding = new Thickness(8, 10, 8, 10), Spacing = 2 };
        for (int i = 0; i < m.Folders.Length; i++)
        {
            list.Children.Add(FolderRow(m.Folders[i], i));
        }
        g.Children.Add(new ScrollViewer { Content = list });
        if (m.Add is SyncMenuItem add)
        {
            var line = new StackPanel { BorderBrush = Res("DividerStrokeColorDefaultBrush"), BorderThickness = new Thickness(0, 1, 0, 0) };
            var b = new HyperlinkButton { Margin = new Thickness(6, 4, 6, 4), Foreground = Res("TextFillColorSecondaryBrush") };
            var label = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            label.Children.Add(new FontIcon { Glyph = "", FontSize = 12 });
            label.Children.Add(new TextBlock { Text = add.Title, FontSize = 12.5 });
            b.Content = label;
            Help(b, add.Help);
            b.Click += (_, _) => Run(new SyncPageCommand.AddFolder(), null);
            line.Children.Add(b);
            Grid.SetRow(line, 1);
            g.Children.Add(line);
        }
        return g;
    }

    UIElement FolderRow(SyncPageFolderRow f, int index)
    {
        var g = new Grid { ColumnSpacing = 9, Padding = new Thickness(8, 6, 8, 6), CornerRadius = new CornerRadius(6) };
        g.Background = f.Selected ? Res("SubtleFillColorSecondaryBrush") : new SolidColorBrush(Colors.Transparent);
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        var badge = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        FlyoutWindow.SetBadge(badge, f.Badge);
        g.Children.Add(badge);
        var text = new StackPanel { Spacing = 1 };
        var title = new TextBlock { Text = f.Title, FontSize = 13, TextTrimming = TextTrimming.CharacterEllipsis };
        var subtitle = new TextBlock { Text = f.Subtitle, FontSize = 11.5, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis };
        text.Children.Add(title);
        text.Children.Add(subtitle);
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, f.AccessibilityLabel);
        var setTitle = Texter(title, f.Title);
        var setSubtitle = Texter(subtitle, f.Subtitle);
        var label = f.AccessibilityLabel;
        var shown = f.Badge;
        values.Add(n =>
        {
            var r = n.Folders[index];
            setTitle(r.Title);
            setSubtitle(r.Subtitle);
            if (r.AccessibilityLabel != label) { label = r.AccessibilityLabel; Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, label); }
            if (!Equals(r.Badge, shown)) { shown = r.Badge; FlyoutWindow.SetBadge(badge, shown); }
        });
        var id = f.Id;
        g.Tapped += (_, _) => Choose(id);
        if (f.Menu.Length > 0)
        {
            g.ContextFlyout = Menu(f.Menu, id);
        }
        return g;
    }

    UIElement Page(SyncFolderPage p)
    {
        var s = new StackPanel { Padding = new Thickness(26, 22, 26, 22) };
        s.Children.Add(Header(p));
        if (p.Gone.Length > 0 || p.Reshare is not null)
        {
            s.Children.Add(Away(p));
        }
        if (p.Removal is SyncMenuConfirm r)
        {
            s.Children.Add(RemoveStrip(r));
        }
        var members = Section(s, p.Members.Title);
        if (p.Members.Empty is string none)
        {
            members.Children.Add(Faint(none, 13));
        }
        Rows(members, p.Members.Rows.Select((m, i) => MemberRow(m, p.FolderId, i)));
        if (p.Conflicts is SyncPageConflicts c)
        {
            var conflicts = Section(s, c.Title);
            if (c.Empty is string noConflicts)
            {
                conflicts.Children.Add(Faint(noConflicts, 13));
            }
            Rows(conflicts, c.Rows.Select((row, i) => ConflictRow(row, p.FolderId, i)));
        }
        if (p.Issues is SyncPageIssues issues)
        {
            var section = Section(s, issues.Title);
            Rows(section, issues.Rows.Select((i, index) =>
            {
                var line = new StackPanel { Spacing = 2, Padding = new Thickness(0, 6, 0, 6) };
                // No path: the issue is the whole folder's (its reason alone).
                if (i.Path is string path)
                {
                    line.Children.Add(new TextBlock { Text = path, FontSize = 13, TextTrimming = TextTrimming.CharacterEllipsis });
                }
                var reason = Faint(i.Reason, 11.5);
                line.Children.Add(reason);
                var setReason = Texter(reason, i.Reason);
                values.Add(n => setReason(n.Page!.Issues!.Rows[index].Reason));
                return (UIElement)line;
            }));
        }
        return s;
    }

    UIElement Header(SyncFolderPage p)
    {
        var s = new StackPanel { Spacing = 4 };
        var top = new Grid { ColumnSpacing = 8 };
        top.ColumnDefinitions.Add(new ColumnDefinition());
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var title = new TextBlock { Text = p.Title, FontSize = 20, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
        top.Children.Add(title);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var b in p.Buttons)
        {
            buttons.Children.Add(PageButton(b, p.FolderId));
        }
        Grid.SetColumn(buttons, 1);
        top.Children.Add(buttons);
        s.Children.Add(top);

        // Where it is in the secondary colour, the magnifier beside it.
        var place = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        var where = new TextBlock { Text = p.Place, FontSize = 12.5, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center, IsTextSelectionEnabled = true };
        place.Children.Add(where);
        var reveal = new Button { Content = new FontIcon { Glyph = "", FontSize = 12 }, Padding = new Thickness(4), Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
        Help(reveal, p.Reveal.Help.Length > 0 ? p.Reveal.Help : p.Reveal.Title);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(reveal, p.Reveal.AccessibilityLabel ?? p.Reveal.Title);
        reveal.Click += (_, _) => Press(p.Reveal, p.FolderId);
        place.Children.Add(reveal);
        s.Children.Add(place);

        var status = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(0, 2, 0, 0) };
        var badge = new Grid { Width = 14, Height = 14, VerticalAlignment = VerticalAlignment.Center };
        FlyoutWindow.SetBadge(badge, p.Badge);
        status.Children.Add(badge);
        statusText = new TextBlock { Text = p.Status, FontSize = 12.5, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        status.Children.Add(statusText);
        s.Children.Add(status);
        // (Whether there is an error or a note is the page's structure; their texts are values.)
        Action<string>? setError = null, setNote = null;
        if (p.Error is string error)
        {
            var e = new TextBlock { Text = error, FontSize = 12, Foreground = Res("SystemFillColorCriticalBrush"), TextWrapping = TextWrapping.Wrap };
            s.Children.Add(e);
            setError = Texter(e, error);
        }
        if (p.Note is string note)
        {
            var n = Faint(note);
            s.Children.Add(n);
            setNote = Texter(n, note);
        }
        var setTitle = Texter(title, p.Title);
        var setPlace = Texter(where, p.Place);
        var setStatus = Texter(statusText, p.Status);
        var shown = p.Badge;
        values.Add(n =>
        {
            var q = n.Page!;
            setTitle(q.Title);
            setPlace(q.Place);
            setStatus(q.Status);
            if (q.Error is string e) setError?.Invoke(e);
            if (q.Note is string t) setNote?.Invoke(t);
            if (!Equals(q.Badge, shown)) { shown = q.Badge; FlyoutWindow.SetBadge(badge, shown); }
        });
        if (p.OnDemandOff is SyncPageNotice off)
        {
            var line = new Grid { ColumnSpacing = 8, Margin = new Thickness(0, 2, 0, 0) };
            line.ColumnDefinitions.Add(new ColumnDefinition());
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            line.Children.Add(new TextBlock { Text = off.Text, FontSize = 12, Foreground = Res("SystemFillColorCautionBrush"), TextWrapping = TextWrapping.Wrap });
            if (off.Button is SyncPageButton ob)
            {
                var b = PageButton(ob, p.FolderId);
                Grid.SetColumn(b, 1);
                line.Children.Add(b);
            }
            s.Children.Add(line);
        }
        return s;
    }

    /// <summary>Devices gone from the folder, each with its Remove…, then Share Again: one quiet box.</summary>
    UIElement Away(SyncFolderPage p)
    {
        var s = new StackPanel { Spacing = 8 };
        for (int i = 0; i < p.Gone.Length; i++)
        {
            var g = p.Gone[i];
            var index = i;
            var line = BoxLine("", g.Line, PageButton(g.Button, p.FolderId));
            var setLine = Texter((TextBlock)line.Children[1], g.Line);
            values.Add(n => setLine(n.Page!.Gone[index].Line));
            s.Children.Add(line);
        }
        if (p.Reshare is SyncPageReshare r)
        {
            s.Children.Add(BoxLine("", r.Note, PageButton(r.Button, p.FolderId)));
        }
        return Box(s);
    }

    /// <summary>The Stop Syncing question under the header: its question, the Trash option, Cancel and Remove.</summary>
    UIElement RemoveStrip(SyncMenuConfirm r)
    {
        var text = new StackPanel { Spacing = 6 };
        text.Children.Add(Faint(r.Question));
        var trash = new CheckBox { Content = r.TrashLabel, IsChecked = r.Trash, FontSize = 12 };
        trash.Click += (_, _) => { removal.SetTrash(trash.IsChecked == true); Redraw(); };
        text.Children.Add(trash);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        // Windows order: the action first, Cancel after it.
        var remove = new Button { Content = r.RemoveTitle, Foreground = Res("SystemFillColorCriticalBrush") };
        Help(remove, r.RemoveHelp);
        remove.Click += (_, _) => Run(new SyncPageCommand.Remove(), r.FolderId);
        var cancel = new Button { Content = r.CancelTitle };
        Help(cancel, r.CancelHelp);
        cancel.Click += (_, _) => Run(new SyncPageCommand.CancelRemove(), r.FolderId);
        buttons.Children.Add(remove);
        buttons.Children.Add(cancel);
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = r.Trash ? "" : "", FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(0, 2, 0, 0) });
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        Grid.SetColumn(buttons, 2);
        g.Children.Add(buttons);
        return Box(g);
    }

    static Grid BoxLine(string glyph, string text, UIElement button)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = glyph, FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
        var t = Faint(text);
        t.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(t, 1);
        g.Children.Add(t);
        Grid.SetColumn((FrameworkElement)button, 2);
        g.Children.Add(button);
        return g;
    }

    static Border Box(UIElement content) => new()
    {
        Child = content,
        Padding = new Thickness(12, 8, 12, 8),
        Margin = new Thickness(0, 12, 0, 0),
        CornerRadius = new CornerRadius(8),
        Background = Res("SubtleFillColorSecondaryBrush"),
    };

    static StackPanel Section(StackPanel page, string title)
    {
        var s = new StackPanel { Spacing = 4, Margin = new Thickness(0, 26, 0, 0) };
        s.Children.Add(new TextBlock { Text = title, FontSize = 12, FontWeight = FontWeights.SemiBold, Foreground = Res("TextFillColorSecondaryBrush"), Margin = new Thickness(0, 0, 0, 2) });
        page.Children.Add(s);
        return s;
    }

    /// <summary>Rows with a faint line between them.</summary>
    static void Rows(StackPanel section, IEnumerable<UIElement> rows)
    {
        bool first = true;
        foreach (var row in rows)
        {
            if (!first)
            {
                section.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Opacity = 0.6, Fill = Res("DividerStrokeColorDefaultBrush") });
            }
            first = false;
            section.Children.Add(row);
        }
    }

    static string ReachGlyph(SyncMemberReach r) => r switch
    {
        SyncMemberReach.ThisDevice => "",
        SyncMemberReach.LocalNetwork => "",
        SyncMemberReach.Relay => "",
        SyncMemberReach.Internet => "",
        _ => "",
    };

    UIElement MemberRow(SyncPageMember m, string folderId, int index)
    {
        var g = new Grid { ColumnSpacing = 10, Padding = new Thickness(0, 6, 0, 6) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(18) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = ReachGlyph(m.Reach), FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
        var who = new StackPanel { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
        var name = new TextBlock { Text = m.Name, FontSize = 13, TextTrimming = TextTrimming.CharacterEllipsis };
        who.Children.Add(name);
        // The line is a value: always there, folded while it is empty (an empty one may fill in place).
        var line = Faint(m.Line, 11.5);
        line.Visibility = m.Line.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        who.Children.Add(line);
        Grid.SetColumn(who, 1);
        g.Children.Add(who);
        Action<string>? setTransfer = null;
        TextBlock? transferText = null;
        if (m.Transfer is string transfer)
        {
            transferText = new TextBlock { Text = transfer, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
            Help(transferText, m.TransferHelp);
            Grid.SetColumn(transferText, 2);
            g.Children.Add(transferText);
            setTransfer = Texter(transferText, transfer);
        }
        FrameworkElement role;
        Action<SyncPageMember> setRole;
        if (m.RoleMenu is SyncPageMenu menu)
        {
            var button = MenuButton(menu, folderId);
            role = button;
            var title = menu.Title;
            setRole = r => { if (r.RoleMenu is SyncPageMenu now && now.Title != title) { title = now.Title; button.Content = title; } };
        }
        else
        {
            var text = new TextBlock { Text = m.Role, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
            role = text;
            var setText = Texter(text, m.Role);
            setRole = r => setText(r.Role);
        }
        Grid.SetColumn(role, 3);
        g.Children.Add(role);
        var setName = Texter(name, m.Name);
        var setLine = Texter(line, m.Line);
        var help = m.TransferHelp;
        values.Add(n =>
        {
            var r = n.Page!.Members.Rows[index];
            setName(r.Name);
            setLine(r.Line);
            line.Visibility = r.Line.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
            if (r.Transfer is string t) setTransfer?.Invoke(t);
            if (transferText is not null && r.TransferHelp != help) { help = r.TransferHelp; ToolTipService.SetToolTip(transferText, help.Length > 0 ? help : null); }
            setRole(r);
        });
        return g;
    }

    UIElement ConflictRow(SyncPageConflict c, string folderId, int index)
    {
        var g = new Grid { ColumnSpacing = 10, Padding = new Thickness(0, 6, 0, 6) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(18) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = "", FontSize = 12, Foreground = Res("SystemFillColorCautionBrush"), VerticalAlignment = VerticalAlignment.Center });
        var what = new StackPanel { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
        var title = new TextBlock { Text = c.Title, FontSize = 13, TextTrimming = TextTrimming.CharacterEllipsis };
        var line = Faint(c.Line, 11.5);
        what.Children.Add(title);
        what.Children.Add(line);
        var setTitle = Texter(title, c.Title);
        var setLine = Texter(line, c.Line);
        values.Add(n =>
        {
            var r = n.Page!.Conflicts!.Rows[index];
            setTitle(r.Title);
            setLine(r.Line);
        });
        Grid.SetColumn(what, 1);
        g.Children.Add(what);
        var reveal = PageButton(c.Reveal, folderId);
        Grid.SetColumn(reveal, 2);
        g.Children.Add(reveal);
        var keep = MenuButton(c.Keep, folderId);
        Grid.SetColumn(keep, 3);
        g.Children.Add(keep);
        return g;
    }

    Button PageButton(SyncPageButton b, string folderId)
    {
        var button = new Button { Content = b.Title, VerticalAlignment = VerticalAlignment.Center };
        Help(button, b.Help);
        if (b.AccessibilityLabel is string label)
        {
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, label);
        }
        button.Click += (_, _) => Press(b, folderId);
        return button;
    }

    DropDownButton MenuButton(SyncPageMenu menu, string folderId)
    {
        var b = new DropDownButton { Content = menu.Title, VerticalAlignment = VerticalAlignment.Center, Flyout = Menu(menu.Entries, folderId) };
        Help(b, menu.Help);
        return b;
    }

    /// <summary>A menu of the page: items (checked, disabled, red, asking first) and separators.</summary>
    MenuFlyout Menu(SyncPageMenuEntry[] entries, string folderId)
    {
        var flyout = new MenuFlyout();
        foreach (var e in entries)
        {
            switch (e)
            {
                case SyncPageMenuEntry.Separator:
                    flyout.Items.Add(new MenuFlyoutSeparator());
                    break;
                case SyncPageMenuEntry.Item it:
                    MenuFlyoutItem item = it.Checked ? new ToggleMenuFlyoutItem { IsChecked = true } : new MenuFlyoutItem();
                    item.Text = it.Title;
                    item.IsEnabled = it.Enabled;
                    if (it.Destructive)
                    {
                        item.Foreground = Res("SystemFillColorCriticalBrush");
                    }
                    item.Click += (_, _) => { if (it.Question is SyncQuestion q) Ask(q, it.Command, folderId); else Run(it.Command, folderId); };
                    flyout.Items.Add(item);
                    break;
            }
        }
        return flyout;
    }

    static void Help(FrameworkElement e, string help)
    {
        if (help.Length > 0)
        {
            ToolTipService.SetToolTip(e, help);
        }
    }

    // ----------------------------------------------------------------------------------------------- commands

    void Press(SyncPageButton b, string folderId)
    {
        if (b.Question is SyncQuestion q) Ask(q, b.Command, folderId); else Run(b.Command, folderId);
    }

    /// <summary>An item's question (the Mac's confirmation dialog); its command runs on the confirming button.</summary>
    async void Ask(SyncQuestion q, SyncPageCommand command, string folderId)
    {
        AppLog.Write($"sync page asks: {q.Title}");
        if (Quiet.On || root.XamlRoot is not XamlRoot xr)
        {
            return;
        }
        var d = new ContentDialog
        {
            XamlRoot = xr,
            Title = q.Title,
            Content = new TextBlock { Text = q.Text, TextWrapping = TextWrapping.Wrap },
            PrimaryButtonText = q.ConfirmTitle,
            CloseButtonText = q.CancelTitle,
            DefaultButton = ContentDialogButton.Primary,
        };
        if (q.Destructive)
        {
            RowCommands.Destructive(d);
        }
        try
        {
            if (await d.ShowAsync() == ContentDialogResult.Primary) Run(command, folderId);
        }
        catch (Exception e) { AppLog.Write($"sync question failed: {e.Message}"); }
    }

    SyncFolder? Folder(string? id) => id is null ? null : app.Core.SyncSnapshot().Folders.FirstOrDefault(f => f.Id == id);

    /// <summary>
    /// Runs a command of the page on folder <paramref name="id"/> (the page's own items: the folder shown; a list row's
    /// menu: that row's folder). Failures are alerts with the core's <c>sync_*_failed</c> texts.
    /// </summary>
    internal void Run(SyncPageCommand command, string? id)
    {
        AppLog.Write($"sync page: {command.GetType().Name}");
        var f = Folder(id);
        switch (command)
        {
            case SyncPageCommand.Reveal:
                if (f is null) break;
                // The core decides: the folder selected in Explorer, or (missing) its page.
                switch (SeedwellFfiMethods.SyncReveal(f, null))
                {
                    case SyncReveal.Show show: Explorer($"/select,\"{show.Path}\""); break;
                    default: Choose(f.Id); break;
                }
                break;
            case SyncPageCommand.Share:
                if (id is not null) ShareDialog.Show(app, id);
                break;
            case SyncPageCommand.OpenChat chat:
                app.ShowPlace(new LibraryPlace.Chat(chat.ChannelId, null));
                break;
            case SyncPageCommand.Edit:
                if (id is not null) EditFolderDialog.Show(app, id);
                break;
            case SyncPageCommand.AskRemove:
                if (id is null) break;
                Choose(id);
                // The question opens at once; how many files only this PC has follows when the core has counted them.
                removal.Open(id, 0);
                Redraw();
                var asked = id;
                _ = Task.Run(() => app.Core.SyncOnlyHere(asked)).ContinueWith(t => root.DispatcherQueue.TryEnqueue(() =>
                {
                    if (t.IsCompletedSuccessfully) { removal.SetOnlyHere(asked, t.Result); Redraw(); }
                }));
                break;
            case SyncPageCommand.CancelRemove:
                removal.Close();
                Redraw();
                break;
            case SyncPageCommand.Remove:
                if (f is null) break;
                var trash = removal.Trash();
                removal.Close();
                Redraw();
                Background(() => app.Core.SyncRemove(f.Id, trash), alert => alert, e => SeedwellFfiMethods.SyncRemoveFailed(f.Name, e));
                break;
            case SyncPageCommand.AddFolder:
                AddFolderDialog.Show(app, null);
                break;
            case SyncPageCommand.AllowOnDemand:
                // The Mac's File Provider settings; Windows has no on-demand folders yet (Cloud Files).
                break;
            case SyncPageCommand.SetRole sr:
                if (f is null) break;
                var who = members.FirstOrDefault(m => m.DeviceId == sr.DeviceId)?.Name ?? "";
                Background(async () => { await app.Core.SyncSetRole(f.Id, sr.DeviceId, sr.Role); return null; }, _ => null, e => SeedwellFfiMethods.SyncSetRoleFailed(who, e), LoadMembers);
                break;
            case SyncPageCommand.RemoveDevice rd:
                if (f is null) break;
                var name = members.FirstOrDefault(m => m.DeviceId == rd.DeviceId)?.Name ?? f.Gone.FirstOrDefault(g => g.DeviceId == rd.DeviceId)?.Name ?? "";
                Background(async () => { await app.Core.SyncRemoveDevice(f.Id, rd.DeviceId); return null; }, _ => null, e => SeedwellFfiMethods.SyncRemoveDeviceFailed(name, f.Name, e), LoadMembers);
                break;
            case SyncPageCommand.Reshare:
                if (f is null) break;
                _ = Task.Run(() => app.Core.SyncReshare(f.Id)).ContinueWith(t => root.DispatcherQueue.TryEnqueue(() =>
                {
                    if (t.IsCompletedSuccessfully)
                    {
                        // The new folder's page, then its Share dialog.
                        Choose(t.Result.Id);
                        ShareDialog.Show(app, t.Result.Id);
                    }
                    else
                    {
                        app.Alert(SeedwellFfiMethods.SyncReshareFailed(f.Name, Message(t.Exception)));
                    }
                }));
                break;
            case SyncPageCommand.RevealConflict rc:
                Explorer($"/select,\"{rc.Path}\"");
                break;
            case SyncPageCommand.Resolve rs:
                if (f is null) break;
                var original = conflicts.FirstOrDefault(c => c.Copy == rs.Copy)?.Original ?? rs.Copy;
                Background(async () => { await app.Core.SyncResolveConflict(f.Id, rs.Copy, rs.Keep); return null; }, _ => null, e => SeedwellFfiMethods.SyncResolveFailed(original, e), LoadConflicts);
                break;
        }
    }

    /// <summary>
    /// Runs a core call off the UI thread; back on it, an alert for what it returned (<paramref name="ok"/>) or for its
    /// error (<paramref name="failed"/>, given the core's sentence), then <paramref name="done"/> either way.
    /// </summary>
    void Background(Func<Task<SyncAlert?>> work, Func<SyncAlert?, SyncAlert?> ok, Func<string, SyncAlert> failed, Action? done = null)
    {
        _ = Task.Run(work).ContinueWith(t => root.DispatcherQueue.TryEnqueue(() =>
        {
            var alert = t.IsCompletedSuccessfully ? ok(t.Result) : failed(Message(t.Exception));
            if (alert is not null) app.Alert(alert);
            done?.Invoke();
            Redraw();
        }));
    }

    static string Message(AggregateException? e) => e?.InnerException is CoreException ce ? SeedwellFfiMethods.CoreErrorMessage(ce) : e?.InnerException?.Message ?? "";

    static void Explorer(string args)
    {
        if (Quiet.On)
        {
            AppLog.Write($"explorer {args}");
            return;
        }
        Shell.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", args) { UseShellExecute = true });
    }

    // --------------------------------------------------------------------------------------------------- QA

    /// <summary>QA: what a click on the page's button, or a list row's menu item, titled <paramref name="title"/> would run (the question confirmed).</summary>
    internal string QaPress(string title, string? rowId = null)
    {
        if (model is null) return "no model";
        if (rowId is not null)
        {
            var row = model.Folders.FirstOrDefault(r => r.Id == rowId || r.Title == rowId);
            if (row?.Menu.OfType<SyncPageMenuEntry.Item>().FirstOrDefault(i => i.Title.StartsWith(title)) is not SyncPageMenuEntry.Item it) return "no such item";
            Run(it.Command, row.Id);
            return $"ran {it.Command.GetType().Name}{(it.Question is null ? "" : " (asked)")}";
        }
        if (model.Page is not SyncFolderPage p) return "no page";
        if (model.Add is SyncMenuItem add && add.Title.StartsWith(title))
        {
            Run(new SyncPageCommand.AddFolder(), null);
            return "ran AddFolder";
        }
        var b = p.Buttons.Concat([p.Reveal]).Concat(p.Gone.Select(g => g.Button)).Concat(p.Reshare is null ? [] : [p.Reshare.Button]).FirstOrDefault(x => x.Title.StartsWith(title));
        if (b is null)
        {
            if (p.Removal is SyncMenuConfirm r && (r.RemoveTitle.StartsWith(title) || r.CancelTitle.StartsWith(title)))
            {
                Run(r.RemoveTitle.StartsWith(title) ? new SyncPageCommand.Remove() : new SyncPageCommand.CancelRemove(), p.FolderId);
                return $"ran {(r.RemoveTitle.StartsWith(title) ? "Remove" : "CancelRemove")}";
            }
            return "no such button";
        }
        Run(b.Command, p.FolderId);
        return $"ran {b.Command.GetType().Name}{(b.Question is null ? "" : " (asked)")}";
    }

    /// <summary>QA: a member's menu item by its device id and title, as its click after the question (a disabled one does not run).</summary>
    internal string QaMemberPress(string deviceId, string title)
    {
        var row = model?.Page?.Members.Rows.FirstOrDefault(r => r.DeviceId == deviceId);
        if (row?.RoleMenu?.Entries.OfType<SyncPageMenuEntry.Item>().FirstOrDefault(i => i.Title.StartsWith(title)) is not SyncPageMenuEntry.Item it) return "no such item";
        if (!it.Enabled) return "disabled";
        Run(it.Command, model!.Page!.FolderId);
        return $"ran {it.Command.GetType().Name}";
    }

    /// <summary>QA: the member rows as drawn: who, the line under it, the role (menu or plain text) and the menu's items.</summary>
    internal string QaMembers() => string.Join(" || ", (model?.Page?.Members.Rows ?? []).Select(r =>
        $"{r.DeviceId} \"{r.Name}\" line \"{r.Line}\" role {r.Role}, " + (r.RoleMenu is SyncPageMenu m
            ? $"menu [{string.Join(", ", m.Entries.OfType<SyncPageMenuEntry.Item>().Select(i => $"{i.Title} {(i.Enabled ? "enabled" : "disabled")}"))}] question \"{m.Entries.OfType<SyncPageMenuEntry.Item>().LastOrDefault()?.Question?.Text}\""
            : "plain")));

    /// <summary>QA: the Stop Syncing question's Trash box.</summary>
    internal void QaTrash(bool on)
    {
        removal.SetTrash(on);
        Redraw();
    }

    /// <summary>QA: a line per part of the page as drawn.</summary>
    internal string QaDescribe()
    {
        if (model is null) return "no model";
        return QaDescribeModel() + $", drawn {Draws}, in place {InPlace}, status drawn \"{statusText?.Text}\"";
    }

    string QaDescribeModel()
    {
        if (model is null) return "no model";
        if (model.Empty is SyncPageEmpty e) return $"empty \"{e.Text}\", add {(e.Add is null ? "none" : $"\"{e.Add.Title}\"")}";
        var p = model.Page;
        return $"folders [{string.Join(", ", model.Folders.Select(f => f.Title))}], add {(model.Add is null ? "none" : $"\"{model.Add.Title}\"")}, selected {model.Selected}"
            + (p is null ? "" : $", title \"{p.Title}\", buttons [{string.Join(", ", p.Buttons.Select(b => b.Title))}], status \"{p.Status}\", members {p.Members.Rows.Length} \"{p.Members.Title}\", removal {(p.Removal is null ? "none" : $"\"{p.Removal.Question}\" trash {p.Removal.Trash}")}");
    }
}
