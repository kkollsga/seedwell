using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The Info page of a tracker (Part F; the Mac's TrackerInfoPage and TrackerHeader), drawn from the core's page
/// (<c>FavoriteEditor.tracker_info_page</c>): Back, the title (a star for a favorite), the Remove from Favorites strip,
/// the header (six numbers and the upload sparkline), Now, peer exchange, the speed limits (rate fields from
/// <c>settings_rate_control</c>, read with <c>settings_field_read</c>), the favorite's folders (the core's
/// <c>tracker_folders_view</c>, Revert and Save) and its addresses (a pen, an x, a plus; the one inline field). The
/// tracker is read off the UI thread every <c>refresh_ms</c>; every command goes to the editor.
/// </summary>
sealed class TrackerInfoPage
{
    readonly App app;
    readonly FavoriteEditing editing;
    readonly string trackerId;
    readonly ScrollViewer scroll = new() { VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
    readonly StackPanel body = new() { Spacing = 18, Padding = new Thickness(22, 4, 22, 16), MaxWidth = 640, HorizontalAlignment = HorizontalAlignment.Left };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    TrackerInfoRead? read;
    bool? typesOpen;
    bool loading;

    public UIElement Root => scroll;

    internal string TrackerId => trackerId;

    /// <summary>The page last drawn (QA).</summary>
    internal TrackerInfoPageModel? Model { get; private set; }

    public TrackerInfoPage(App app, FavoriteEditing editing, string trackerId)
    {
        this.app = app;
        this.editing = editing;
        this.trackerId = trackerId;
        scroll.Content = body;
        // A folder draft of another tracker is not this one's.
        editing.Editor.RevertFolders();
        timer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.IsRepeating = false;
        timer.Tick += (_, _) => Load();
        editing.Changed += Draw;
        scroll.Unloaded += (_, _) => timer.Stop();
        scroll.Loaded += (_, _) => Load();
        Draw();
        Load();
    }

    /// <summary>The page's own end (another page shown): it stops reading.</summary>
    internal void Stop()
    {
        timer.Stop();
        editing.Changed -= Draw;
    }

    TrackerSummary? Tracker => app.TakeSnapshot().Trackers.FirstOrDefault(t => t.Id == trackerId);

    /// <summary>Reads the tracker off the UI thread, then draws; again after the page's refresh_ms.</summary>
    internal void Load()
    {
        if (loading || Tracker is not TrackerSummary t) return;
        loading = true;
        _ = Task.Run(() => app.Core.TrackerInfoRead(t)).ContinueWith(task => app.Dispatch(() =>
        {
            loading = false;
            if (task.IsCompletedSuccessfully) read = task.Result;
            editing.Editor.Reload();
            Draw();
            timer.Interval = TimeSpan.FromMilliseconds(Model?.RefreshMs ?? 10_000);
            timer.Start();
        }));
    }

    static Brush Res(string key) => Theme.Res(key);

    void Draw()
    {
        if (Tracker is not TrackerSummary t) return;
        var rates = app.LibraryWindowIfCreated?.Pane.Model?.TrackerRates;
        TrackerInfoPageModel p;
        try
        {
            p = editing.Editor.TrackerInfoPage(new TrackerInfoInput(t, read, rates?.Down ?? 0, rates?.Up ?? 0, (ulong)DateTimeOffset.UtcNow.ToUnixTimeSeconds(), App.HostWindows));
        }
        catch (Exception e)
        {
            AppLog.Write($"tracker info page failed: {e.Message}");
            return;
        }
        Model = p;
        body.Children.Clear();
        var back = new HyperlinkButton { Content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, Children = { new FontIcon { Glyph = "", FontSize = 10 }, new TextBlock { Text = p.Back.Title, FontSize = Theme.Secondary } } }, Padding = new Thickness(0, 2, 4, 2) };
        ToolTipService.SetToolTip(back, p.Back.Help);
        back.Click += (_, _) => { app.Library.SetTrackerInfo(false); app.Refreshed(); };
        body.Children.Add(back);
        var title = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        if (p.Favorite) title.Children.Add(new FontIcon { Glyph = "", FontSize = 15, Foreground = Theme.Brush(Theme.Accent, 0.85), VerticalAlignment = VerticalAlignment.Center });
        title.Children.Add(new TextBlock { Text = p.Title, FontSize = Theme.Title, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
        title.Children.Add(new TextBlock { Text = p.Subtitle, FontSize = Theme.Body, Foreground = Res("TextFillColorTertiaryBrush"), VerticalAlignment = VerticalAlignment.Center });
        body.Children.Add(title);
        if (p.Removal is FavoriteRemoval q)
        {
            body.Children.Add(editing.RemovalStrip(q, () => editing.Run(trackerId, new TrackerInfoCommand.ConfirmRemoval()), () => editing.Run(trackerId, new TrackerInfoCommand.CancelRemoval())));
        }
        if (p.Header is TrackerHeaderModel h) body.Children.Add(Header(h));
        body.Children.Add(Facts(p));
        if (p.Pex is TrackerInfoPex x) body.Children.Add(Pex(x));
        body.Children.Add(Limits(p.Limits));
        body.Children.Add(Location(p.Location));
        body.Children.Add(Addresses(p.Addresses));
    }

    static TextBlock Section(string t) => new() { Text = t.ToUpperInvariant(), FontSize = Theme.Caption, FontWeight = FontWeights.SemiBold, CharacterSpacing = 40, Foreground = Res("TextFillColorTertiaryBrush") };

    static TextBlock Quiet(string t) => new() { Text = t, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush"), TextWrapping = TextWrapping.Wrap };

    static StackPanel Block(string title)
    {
        var s = new StackPanel { Spacing = 7 };
        s.Children.Add(Section(title));
        return s;
    }

    /// <summary>The six numbers with their captions and grey lines, and the sparkline of the last 30 days' upload.</summary>
    static UIElement Header(TrackerHeaderModel h)
    {
        var s = new StackPanel { Spacing = 14 };
        var stats = new Grid { ColumnSpacing = 16 };
        for (int n = 0; n < h.Stats.Length; n++)
        {
            stats.ColumnDefinitions.Add(new ColumnDefinition());
            var st = h.Stats[n];
            var one = new StackPanel { Spacing = 2 };
            one.Children.Add(new TextBlock { Text = st.Value, FontSize = Theme.Title, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
            one.Children.Add(new TextBlock { Text = st.Caption, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap });
            one.Children.Add(new TextBlock { Text = st.Sub ?? " ", FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush") });
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(one, $"{st.Caption}: {st.Value}");
            Grid.SetColumn(one, n);
            stats.Children.Add(one);
        }
        s.Children.Add(stats);
        if (h.Chart.Length > 0)
        {
            var chart = new StackPanel { Spacing = 4 };
            chart.Children.Add(Sparkline(h.Chart));
            chart.Children.Add(new TextBlock { Text = h.ChartCaption, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush") });
            s.Children.Add(chart);
        }
        return s;
    }

    /// <summary>An area of the days' upload (oldest left), without axes or dates: the caption says what it is.</summary>
    static UIElement Sparkline(TrackerHeaderPoint[] points)
    {
        const double width = 596, height = 30;
        var days = points.Max(p => p.DaysAgo);
        var top = Math.Max(1, points.Max(p => p.UpBytes));
        var ordered = points.OrderByDescending(p => p.DaysAgo).ToArray();
        double X(uint daysAgo) => days == 0 ? width : (days - daysAgo) * width / days;
        double Y(ulong up) => height - up * (height - 2) / (double)top;
        var line = new Microsoft.UI.Xaml.Shapes.Polyline { Stroke = Theme.Brush(Theme.Accent, 0.8), StrokeThickness = 1.5 };
        var area = new Microsoft.UI.Xaml.Shapes.Polygon { Fill = Theme.Brush(Theme.Accent, 0.18) };
        area.Points.Add(new Windows.Foundation.Point(X(ordered[0].DaysAgo), height));
        foreach (var p in ordered)
        {
            var pt = new Windows.Foundation.Point(X(p.DaysAgo), Y(p.UpBytes));
            line.Points.Add(pt);
            area.Points.Add(pt);
        }
        area.Points.Add(new Windows.Foundation.Point(X(ordered[^1].DaysAgo), height));
        var canvas = new Canvas { Width = width, Height = height, HorizontalAlignment = HorizontalAlignment.Left };
        canvas.Children.Add(area);
        canvas.Children.Add(line);
        return canvas;
    }

    static FrameworkElement LabelRow(string label, UIElement value)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(130) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new TextBlock { Text = label, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap });
        Grid.SetColumn((FrameworkElement)value, 1);
        g.Children.Add(value);
        return g;
    }

    static UIElement Facts(TrackerInfoPageModel p)
    {
        var s = Block(p.NowTitle);
        foreach (var f in p.Now)
        {
            s.Children.Add(LabelRow(f.Label, new TextBlock { Text = f.Value, FontSize = Theme.Body, MaxLines = 2, TextWrapping = TextWrapping.Wrap, Foreground = f.Error ? Res("SystemFillColorCriticalBrush") : Res("TextFillColorPrimaryBrush") }));
        }
        return s;
    }

    UIElement Pex(TrackerInfoPex x)
    {
        var s = Block(x.Title);
        var g = new Grid();
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new TextBlock { Text = x.Label, FontSize = Theme.Body, VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap });
        var toggle = new ToggleSwitch { IsOn = x.On, IsEnabled = x.Enabled, OnContent = "", OffContent = "", MinWidth = 0 };
        ToolTipService.SetToolTip(toggle, x.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(toggle, x.Label);
        toggle.Toggled += (_, _) => { if (toggle.IsOn != x.On) editing.Run(trackerId, new TrackerInfoCommand.SetPex(toggle.IsOn)); };
        Grid.SetColumn(toggle, 1);
        g.Children.Add(toggle);
        s.Children.Add(g);
        var note = Quiet(x.Note);
        if (x.NoteIsReason) note.Foreground = Res("TextFillColorSecondaryBrush");
        s.Children.Add(note);
        return s;
    }

    /// <summary>One cap for the tracker's torrents together, per direction: a rate field and the rate now against it.</summary>
    UIElement Limits(TrackerInfoLimits l)
    {
        var s = Block(l.Title);
        ulong Value(RateDirection d) => l.Rows.FirstOrDefault(r => r.Direction == d)?.Limit ?? 0;
        foreach (var row in l.Rows)
        {
            var direction = row.Direction;
            var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10 };
            line.Children.Add(RateField(row.Limit, bytes =>
            {
                // A change saves both: the other as shown.
                var down = direction == RateDirection.Download ? bytes : Value(RateDirection.Download);
                var up = direction == RateDirection.Upload ? bytes : Value(RateDirection.Upload);
                editing.SetLimits(trackerId, down, up);
            }));
            line.Children.Add(new TextBlock { Text = row.Line, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
            s.Children.Add(LabelRow(row.Label, line));
        }
        if (l.Note.Length > 0) s.Children.Add(Quiet(l.Note));
        return s;
    }

    /// <summary>
    /// A speed limit's field as the core lays it out (<c>settings_rate_control</c>: the number, its unit menu, "Unlimited"
    /// when empty); what is typed is read by the core (<c>settings_field_read</c>) on Return, when it loses the keyboard
    /// and when the unit changes.
    /// </summary>
    static UIElement RateField(ulong bytes, Action<ulong> changed)
    {
        if (TrsFfiMethods.SettingsRateControl(bytes, 76) is not SettingsControl.Field f) return new TextBlock();
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        var box = new TextBox { Text = f.Text, PlaceholderText = f.Placeholder, Width = Math.Max(70, f.Width), TextAlignment = f.Trailing ? TextAlignment.Right : TextAlignment.Left, MinHeight = 0 };
        if (f.Help is string help) ToolTipService.SetToolTip(box, help);
        var unit = new ComboBox { ItemsSource = f.Units, SelectedItem = f.Unit, MinWidth = 0, VerticalAlignment = VerticalAlignment.Center };
        void Read()
        {
            var u = unit.SelectedItem as string ?? f.Unit;
            if (TrsFfiMethods.SettingsFieldRead(f.Kind, box.Text, u, bytes) is ulong v && v != bytes) changed(v);
        }
        box.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Enter) { e.Handled = true; Read(); } };
        box.LostFocus += (_, _) => Read();
        unit.SelectionChanged += (_, _) => { if (unit.SelectedItem as string != f.Unit) Read(); };
        s.Children.Add(box);
        if (f.Units.Length > 0) s.Children.Add(unit);
        return s;
    }

    /// <summary>The favorite's own folders (the core's rows and words), Revert and Save while changed; an automatic tracker's note.</summary>
    UIElement Location(TrackerInfoLocation l)
    {
        var s = Block(l.Title);
        if (l.Record is ProfileRecord record)
        {
            s.Children.Add(FolderEditor.Draw(app, record, l.Warnings, edited => editing.SetFolderDraft(trackerId, edited), typesOpen, open => typesOpen = open));
            if (l.Buttons.Length > 0)
            {
                // The Windows order: the prominent one (Save) first.
                var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
                foreach (var b in l.Buttons.OrderBy(b => b.Prominent ? 0 : 1))
                {
                    var button = new Button { Content = b.Title };
                    if (b.Prominent) button.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
                    ToolTipService.SetToolTip(button, b.Help);
                    var command = b.Command;
                    button.Click += (_, _) => editing.Run(trackerId, command);
                    buttons.Children.Add(button);
                }
                s.Children.Add(buttons);
            }
            if (l.Hint is string hint) s.Children.Add(new TextBlock { Text = hint, FontSize = Theme.Caption, Foreground = Res("SystemFillColorCriticalBrush"), TextWrapping = TextWrapping.Wrap });
        }
        if (l.Note is string n) s.Children.Add(Quiet(n));
        return s;
    }

    /// <summary>The addresses: each as text with its pen and x, or the field while edited; the new line; the plus; the note.</summary>
    UIElement Addresses(TrackerInfoAddresses a)
    {
        var s = Block(a.Title);
        s.Spacing = 5;
        foreach (var row in a.Rows)
        {
            if (row.Field is TrackerInfoField f)
            {
                s.Children.Add(Field(f));
                continue;
            }
            var g = new Grid { MinHeight = 24 };
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.Children.Add(new TextBlock { Text = row.Text, FontFamily = Theme.MonoFont, FontSize = Theme.Mono, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis, IsTextSelectionEnabled = true, VerticalAlignment = VerticalAlignment.Center });
            var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2 };
            foreach (var b in row.Buttons) buttons.Children.Add(SymbolButton(b));
            Grid.SetColumn(buttons, 1);
            g.Children.Add(buttons);
            s.Children.Add(g);
        }
        if (a.Adding is TrackerInfoField adding) s.Children.Add(Field(adding));
        if (a.Add is TrackerInfoButton add) s.Children.Add(SymbolButton(add));
        if (a.Note is string n) s.Children.Add(Quiet(n));
        return s;
    }

    UIElement Field(TrackerInfoField f)
    {
        var s = new StackPanel();
        s.Children.Add(editing.Field(f.Target, f.Prompt, mono: true));
        if (editing.Hint() is FrameworkElement hint) s.Children.Add(hint);
        return s;
    }

    /// <summary>A small symbol button (pen, x, plus): its help is its tooltip and screen reader name.</summary>
    Button SymbolButton(TrackerInfoButton b)
    {
        var glyph = b.Symbol switch
        {
            TrackerInfoSymbol.Edit => "",
            TrackerInfoSymbol.Remove => "",
            TrackerInfoSymbol.Back => "",
            _ => "",
        };
        var button = new Button { Content = new FontIcon { Glyph = glyph, FontSize = 11 }, Padding = new Thickness(5, 3, 5, 3), MinWidth = 0, MinHeight = 0, HorizontalAlignment = HorizontalAlignment.Left, Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
        ToolTipService.SetToolTip(button, b.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, b.Help.Length > 0 ? b.Help : b.Title);
        var command = b.Command;
        button.Click += (_, _) => editing.Run(trackerId, command);
        return button;
    }
}
