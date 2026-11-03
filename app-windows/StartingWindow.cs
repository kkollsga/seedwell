using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The starting notifier (SPEC 8.6; the Mac's StartingNotifier and StartingView): a small window at the tray, above the
/// taskbar, for as long as a new torrent waits to start. It never takes the focus from the app in front (shown, never
/// activated). One block per torrent as the core lays it out (<c>Starting</c>, trs-ffi starting.rs): a countdown with
/// its ring and line (<c>starting_tick</c>, animated between looks), Cancel and Start; a choice, compact or with its
/// checklist (the shared chooser); a magnet link fetching its files. When it shows, when it closes and what each
/// button does are the core's; this shows the window, ticks while it is up and runs the actions.
/// </summary>
sealed class StartingWindow : Window
{
    const double PanelWidth = 340, MaxPanelHeight = 560, Edge = 12;

    readonly App app;
    readonly ChoiceStore store;
    readonly Grid root = new() { Width = PanelWidth, HorizontalAlignment = HorizontalAlignment.Left, VerticalAlignment = VerticalAlignment.Top };
    readonly StackPanel blocks = new() { Margin = new Thickness(18) };
    readonly ScrollViewer scroll = new() { VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer tick, clock;
    readonly List<(Grid Ring, TextBlock Line, StartingCountdown Countdown)> clocks = [];
    readonly IntPtr hwnd;
    StartingPage page = new([], new StartingShow.Close(), false, null, 1000);
    bool pinned;

    internal Starting Starting { get; }

    /// <summary>It is on screen.</summary>
    internal bool IsShown { get; private set; }

    internal StartingPage Page => page;

    public StartingWindow(App app, ChoiceStore store)
    {
        this.app = app;
        this.store = store;
        Starting = new Starting(app.Core, store.Choices);
        SystemBackdrop = new Microsoft.UI.Xaml.Media.DesktopAcrylicBackdrop();
        var p = OverlappedPresenter.Create();
        p.SetBorderAndTitleBar(true, false);
        p.IsResizable = false;
        p.IsMaximizable = false;
        p.IsMinimizable = false;
        p.IsAlwaysOnTop = true;
        AppWindow.SetPresenter(p);
        AppWindow.IsShownInSwitchers = false;
        AppWindow.Title = "Seedwell";
        hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        scroll.Content = blocks;
        root.Children.Add(scroll);
        Content = root;
        var q = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        tick = q.CreateTimer();
        tick.Tick += (_, _) => Refresh();
        // The rings and lines move between looks (the Mac's TimelineView at 15 frames a second).
        clock = q.CreateTimer();
        clock.Interval = TimeSpan.FromMilliseconds(100);
        clock.Tick += (_, _) => Clocks();
        store.Changed += () => { if (IsShown) Draw(); };
        Closed += (_, e) => { e.Handled = true; Hide(); };
    }

    /// <summary>Asks the core and shows, updates or hides the window as it says (every pending event, every tick while up).</summary>
    internal void Refresh(bool launch = false)
    {
        string[] before = store.Choices.Ids();
        try
        {
            page = Starting.Refresh(app.TakeSnapshot().Downloading, (ulong)App.Now(), launch);
        }
        catch (Exception e)
        {
            AppLog.Write($"starting notifier: {e.Message}");
            return;
        }
        // The checklists the core loaded or forgot are the chooser window's and the window's list's too.
        if (!before.SequenceEqual(store.Choices.Ids())) store.OnChanged();
        switch (page.Show)
        {
            case StartingShow.Show:
                Draw();
                ShowNow();
                break;
            case StartingShow.Quiet:
                tick.Stop();
                break;
            default:
                if (!pinned) Hide();
                break;
        }
    }

    /// <summary>A notice about a torrent counting down was clicked: the notifier shows it, even when put away.</summary>
    internal void Reveal(string id)
    {
        Starting.Reveal(id);
        Refresh();
    }

    /// <summary>The tray's flyout opens on the same spot: the notifier steps aside and stays put away for these torrents.</summary>
    internal void StepAside()
    {
        if (!IsShown) return;
        Starting.DismissAll();
        Hide();
    }

    internal void Hide()
    {
        tick.Stop();
        clock.Stop();
        if (IsShown) AppLog.Write("starting notifier: hidden");
        IsShown = false;
        AppWindow.Hide();
    }

    /// <summary>A magnet link whose files were asked for got them: its block opens here.</summary>
    internal bool ConsumeRequest(string id) => Starting.ConsumeRequest(id);

    void ShowNow()
    {
        tick.Interval = TimeSpan.FromMilliseconds(page.TickMs);
        tick.Start();
        clock.Start();
        Place();
        if (IsShown) return;
        IsShown = true;
        AppLog.Write($"starting notifier: shown ({page.Blocks.Length} waiting)");
        if (Quiet.On)
        {
            Quiet.Show(this);
            return;
        }
        // Shown, never activated: the app in front keeps the keyboard (the first click answers like a popover's).
        SetWindowLongPtr(hwnd, GWL_EXSTYLE, GetWindowLongPtr(hwnd, GWL_EXSTYLE) | WS_EX_NOACTIVATE);
        AppWindow.Show(false);
    }

    /// <summary>Bottom right of the work area, above the tray, as tall as its blocks (up to 560).</summary>
    void Place()
    {
        var work = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        var scale = root.XamlRoot?.RasterizationScale ?? GetDpiForWindow(hwnd) / 96.0;
        root.Measure(new Windows.Foundation.Size(PanelWidth, double.PositiveInfinity));
        double h = Math.Min(pinned ? double.MaxValue : MaxPanelHeight, root.DesiredSize.Height + 2);
        int w = (int)(PanelWidth * scale), hp = Math.Min((int)(h * scale), work.Height), m = (int)(Edge * scale);
        // The panel's own size (the client area; the thin border is outside it); a quiet run keeps it off the screen.
        AppWindow.ResizeClient(new Windows.Graphics.SizeInt32(w, hp));
        if (Quiet.On && IsShown) return;
        var outer = AppWindow.Size;
        AppWindow.Move(new Windows.Graphics.PointInt32(Math.Max(work.X, work.X + work.Width - outer.Width - m), Math.Max(work.Y, work.Y + work.Height - outer.Height - m)));
    }

    // ------------------------------------------------------------------------------------------------ drawing

    static Brush Res(string key) => Theme.Res(key);

    void Draw()
    {
        blocks.Children.Clear();
        clocks.Clear();
        for (int n = 0; n < page.Blocks.Length; n++)
        {
            var b = page.Blocks[n];
            if (n > 0) blocks.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush"), Margin = new Thickness(0, page.Blocks.Length > 1 ? 12 : 14, 0, page.Blocks.Length > 1 ? 12 : 14) });
            blocks.Children.Add(b.State is StartingBlockState.Countdown c ? Countdown(b, c.CountdownValue) : Choice(b));
        }
        // The small × (while someone has to choose): put away, the torrents keep waiting.
        if (root.Children.Count > 1) root.Children.RemoveAt(1);
        if (page.CloseHelp is string help)
        {
            var x = new Button { Content = new FontIcon { Glyph = "", FontSize = 9 }, Padding = new Thickness(5), MinWidth = 0, MinHeight = 0, Margin = new Thickness(0, 6, 6, 0), HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Top, Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
            ToolTipService.SetToolTip(x, help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(x, help);
            x.Click += (_, _) => StepAside();
            root.Children.Add(x);
        }
        Clocks();
        if (IsShown) Place();
    }

    /// <summary>A torrent counting down: its header, the ring and line, Cancel and Start (now).</summary>
    UIElement Countdown(StartingBlock b, StartingCountdown countdown)
    {
        var s = new StackPanel { Spacing = page.Compact ? 9 : 12 };
        // The chevron is the one way to the file list: it ends the countdown and opens the checklist here.
        s.Children.Add(Header(b, () => ChooseFiles(b.Id)));
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var ring = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        var text = new TextBlock { FontSize = Theme.Secondary, VerticalAlignment = VerticalAlignment.Center };
        line.Children.Add(ring);
        line.Children.Add(text);
        clocks.Add((ring, text, countdown));
        s.Children.Add(line);
        s.Children.Add(Choosing.HeldButtons(b.Actions, keep => Cancel(b.Id, keep), () => StartNow(b.Id)));
        return s;
    }

    /// <summary>A torrent waiting for its choice: compact (Cancel, Start), open (its checklist), or fetching its files.</summary>
    UIElement Choice(StartingBlock b)
    {
        var s = new StackPanel { Spacing = 10 };
        s.Children.Add(Header(b, () => ToggleExpanded(b.Id)));
        if (b.State is StartingBlockState.Choice choice && store.Page(b.Id) is ChooserPage chooser)
        {
            s.Children.Add(choice.Expanded
                ? Choosing.Checklist(b.Id, chooser, store, 220, b.Actions, keep => Cancel(b.Id, keep), () => Confirm(b.Id))
                : Choosing.HeldButtons(b.Actions, keep => Cancel(b.Id, keep), () => Confirm(b.Id)));
        }
        else
        {
            var g = new Grid { ColumnSpacing = 8 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.Children.Add(new ProgressRing { IsActive = true, Width = 16, Height = 16 });
            var line = new TextBlock { Text = b.Line, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(line, 1);
            g.Children.Add(line);
            var cancel = Choosing.HeldButtons(b.Actions with { Start = null }, keep => Cancel(b.Id, keep), () => { });
            Grid.SetColumn(cancel, 2);
            g.Children.Add(cancel);
            s.Children.Add(g);
        }
        return s;
    }

    /// <summary>A block's first line: the fold chevron, the title and its line (one link: open in the window), the category pill.</summary>
    UIElement Header(StartingBlock b, Action chevron)
    {
        var g = new Grid { ColumnSpacing = 4 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        if (b.Chevron is StartingChevron c)
        {
            var fold = new Button { Content = new FontIcon { Glyph = c.Open ? "" : "", FontSize = 9 }, Padding = new Thickness(4, 6, 4, 6), Margin = new Thickness(-6, 0, 0, 0), MinWidth = 0, MinHeight = 0, VerticalAlignment = VerticalAlignment.Top, Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
            ToolTipService.SetToolTip(fold, c.Help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(fold, c.Label);
            fold.Click += (_, _) => chevron();
            g.Children.Add(fold);
        }
        var text = new StackPanel { Spacing = 3 };
        text.Children.Add(new TextBlock { Text = b.Title, FontSize = Theme.Heading, FontWeight = FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap, MaxLines = 2, TextTrimming = TextTrimming.CharacterEllipsis });
        text.Children.Add(new TextBlock { Text = b.Detail, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis });
        var open = new Button { Content = text, Padding = new Thickness(0), HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Left, Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
        ToolTipService.SetToolTip(open, b.OpenHelp);
        open.Click += (_, _) => Open(b.Id);
        Grid.SetColumn(open, 1);
        g.Children.Add(open);
        if (b.Category is StartingCategory pill)
        {
            var p = Choosing.CategoryMenu(pill.Title, pill.Help, pill.Choices, cat => ChangeCategory(b.Id, cat), pill: true);
            // The room of the × in the corner.
            p.Margin = new Thickness(6, 0, page.CloseHelp is null ? 0 : 14, 0);
            Grid.SetColumn(p, 2);
            g.Children.Add(p);
        }
        return g;
    }

    /// <summary>Every countdown's ring and line at this moment (the core's starting_tick).</summary>
    void Clocks()
    {
        foreach (var (ring, line, countdown) in clocks)
        {
            var t = TrsFfiMethods.StartingTick(countdown, (ulong)App.Now());
            line.Text = t.Line;
            line.Foreground = Res(t.Paused ? "TextFillColorSecondaryBrush" : "TextFillColorPrimaryBrush");
            ring.Children.Clear();
            ring.Children.Add(FlyoutWindow.Ring(t.Fraction, t.Paused ? Theme.Color("TextFillColorSecondaryBrush") : Theme.Accent));
        }
    }

    // ----------------------------------------------------------------------------------------------- actions

    // Each reaches the engine through the core's async calls, in order: it returns at once, and the notifier is drawn
    // again when the engine has answered.

    void ChooseFiles(string id) => CoreCalls.InOrder("starting choose files", () => Starting.ChooseFiles(id), _ => Refresh());

    void StartNow(string id) => CoreCalls.InOrder("starting start now", () => Starting.StartNow(id), () => { Refresh(); app.Refreshed(); });

    void Confirm(string id) => CoreCalls.InOrder("starting start", () => Starting.Confirm(id), () =>
    {
        store.ResolvedElsewhere(id);
        Refresh();
        app.Refreshed();
    }, e =>
    {
        app.Alert(new SyncAlert(e, ""));
        Refresh();
    });

    // Escape and Shift+Escape can both reach here: the core cancels a torrent once.
    void Cancel(string id, bool keep) => CoreCalls.InOrder("starting cancel", () => Starting.Cancel(id, keep), out_ =>
    {
        if (out_.Cancelled) store.ResolvedElsewhere(id, out_.Note);
        Refresh();
        app.Refreshed();
    });

    /// <summary>The title: the window on this torrent, its file list open (an auto-start torrent stops counting first).</summary>
    void Open(string id) => CoreCalls.InOrder("starting open", () => Starting.Open(id), () =>
    {
        app.ShowTorrent(id);
        Refresh();
    });

    void ChangeCategory(string id, Category c)
    {
        store.ChangeCategory(id, c);
        Refresh();
    }

    void ToggleExpanded(string id)
    {
        Starting.ToggleExpanded(id);
        Refresh();
    }

    // ------------------------------------------------------------------------------------------------------ QA

    /// <summary>QA: shown although nothing waits, until <see cref="QaClose"/>; drawn in full.</summary>
    internal void QaShow()
    {
        pinned = true;
        Refresh();
        Draw();
        ShowNow();
    }

    internal void QaClose()
    {
        pinned = false;
        Hide();
    }

    internal FrameworkElement RootElement => root;

    /// <summary>QA: the chevron in front of a block's title.</summary>
    internal void QaChevron(string id)
    {
        if (page.Blocks.FirstOrDefault(b => b.Id == id) is not StartingBlock b) return;
        if (b.State is StartingBlockState.Choice) ToggleExpanded(id); else ChooseFiles(id);
    }

    /// <summary>QA stand-ins for clicks: ok, cancel [keep], title, category &lt;name&gt;, all, none, open, toggle &lt;row&gt;, start-now.</summary>
    internal void QaAction(string verb, string id, string arg)
    {
        switch (verb)
        {
            case "ok": Confirm(id); break;
            case "start-now": StartNow(id); break;
            case "cancel": Cancel(id, arg == "keep"); break;
            case "title": Open(id); break;
            case "category":
                if (TrsFfiMethods.CategoryChoices(Category.Other).FirstOrDefault(c => string.Equals(c.Title, arg, StringComparison.OrdinalIgnoreCase)) is CategoryChoice cc) ChangeCategory(id, cc.Category);
                break;
            case "all": store.Edit(id, c => c.SelectAll(true)); break;
            case "none": store.Edit(id, c => c.SelectAll(false)); break;
            case "open": store.Edit(id, c => c.OpenAll()); break;
            case "toggle":
                if (store.Loaded(id)?.FindRow(arg) is string row) store.Edit(id, c => c.Toggle(row));
                break;
        }
        Refresh();
    }

    /// <summary>QA: what is shown (the Mac's qaSummary).</summary>
    internal string QaSummary => $"starting: shown {IsShown}, items {page.Blocks.Length} [" + string.Join(" ;; ", page.Blocks.Select(b =>
    {
        var t = $"{b.Title} | {b.Detail} | {b.Line} | {b.State.GetType().Name}";
        if (b.Actions.KeepFile is HeldKeepFile keep) t += $" | cancel caption: {(Choosing.ShiftDown ? keep.Caption : "-")}";
        if (b.State is StartingBlockState.Choice && store.Page(b.Id) is ChooserPage p) t += $" | picked {p.SelectedLine}";
        return t;
    })) + "]";

    // ------------------------------------------------------------------------------------------------- native

    const int GWL_EXSTYLE = -20;
    const nint WS_EX_NOACTIVATE = 0x08000000;

    [System.Runtime.InteropServices.DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    static extern nint GetWindowLongPtr(nint hwnd, int index);

    [System.Runtime.InteropServices.DllImport("user32.dll", EntryPoint = "SetWindowLongPtrW")]
    static extern nint SetWindowLongPtr(nint hwnd, int index, nint value);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern uint GetDpiForWindow(IntPtr hwnd);
}
