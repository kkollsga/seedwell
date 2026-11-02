using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Settings › Devices on Windows (SPEC §7G.1 "Your devices", §7G.5): the PC's own devices, drawn from the core's
/// <c>DevicesView</c> (trs-ffi devices_text.rs, as the Mac's Settings tab): the list with Remove…, a device asking to
/// pair (its id, Refuse and Pair), Pair a Device… (the link, Copy, the countdown), Pair with a Link… and Leave or
/// Start Over, each in a dialog. Read again every second. Only the user's clicks reach these calls; agents never.
/// "On all my devices" is in a folder's Add Folder and Edit dialogs (<see cref="EditFolderDialog"/>), New Channel and
/// the channel menu.
/// </summary>
sealed class DevicesSection
{
    readonly App app;
    readonly StackPanel panel = new() { Spacing = 10 };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    string drawn = "";
    PairLinkView? link;
    string myId = "";
    TextBlock? countdown;

    public UIElement Root => panel;

    public DevicesSection(App app)
    {
        this.app = app;
        timer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.Interval = TimeSpan.FromSeconds(1);
        timer.Tick += (_, _) => Update();
        // Read only while shown (the page is made anew each time it opens).
        panel.Loaded += (_, _) => timer.Start();
        panel.Unloaded += (_, _) => timer.Stop();
        Update();
    }

    static ulong NowS() => (ulong)DateTimeOffset.UtcNow.ToUnixTimeSeconds();

    void Update()
    {
        if (link is PairLinkView l && countdown is TextBlock t)
        {
            t.Text = TrsFfiMethods.DevicesCountdown(l.ExpiresS, NowS());
        }
        DevicesView v;
        try { v = app.Core.DevicesView(); }
        catch (Exception e)
        {
            AppLog.Write($"devices view failed: {e.Message}");
            return;
        }
        var key = Key(v);
        if (key != drawn)
        {
            drawn = key;
            Draw(v);
        }
    }

    /// <summary>What the drawing depends on (a record's own text does not list its rows).</summary>
    static string Key(DevicesView v) =>
        $"{v.Kind}|{string.Join(";", v.Rows.Select(r => $"{r.Id},{r.Title},{r.Detail},{r.Warning}"))}|{string.Join(";", v.Requests.Select(r => $"{r.Id},{r.Question}"))}"
        + $"|{v.JoiningLine}|{v.JoiningProblem}|{v.PairEnabled}|{v.LinkEnabled}|{v.LeaveTitle}";

    void Draw(DevicesView v)
    {
        panel.Children.Clear();
        panel.Children.Add(new TextBlock { Text = "Devices", FontSize = 18, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Margin = new Thickness(0, 12, 0, 0) });
        panel.Children.Add(new TextBlock { Text = v.Intro, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        foreach (var r in v.Rows)
        {
            var row = new Grid { ColumnSpacing = 12 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var text = new StackPanel { Spacing = 2 };
            text.Children.Add(new TextBlock { Text = r.Title });
            text.Children.Add(new TextBlock { Text = r.Detail, FontSize = 12, Opacity = 0.7 });
            if (r.Warning is string w)
            {
                text.Children.Add(new TextBlock { Text = w, FontSize = 12, TextWrapping = TextWrapping.Wrap, Foreground = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Orange) });
            }
            row.Children.Add(text);
            if (r.Removable)
            {
                var remove = new Button { Content = "Remove…", VerticalAlignment = VerticalAlignment.Center };
                ToolTipService.SetToolTip(remove, "Take this device out of your devices for good");
                var id = r.Id;
                remove.Click += (_, _) => AskRemove(id);
                Grid.SetColumn(remove, 1);
                row.Children.Add(remove);
            }
            panel.Children.Add(row);
        }
        foreach (var r in v.Requests)
        {
            panel.Children.Add(Request(r));
        }
        if (v.JoiningLine is string line)
        {
            panel.Children.Add(new TextBlock { Text = line, TextWrapping = TextWrapping.Wrap });
            if (v.JoiningProblem is string p)
            {
                panel.Children.Add(new TextBlock { Text = p, TextWrapping = TextWrapping.Wrap, Foreground = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.OrangeRed) });
            }
            panel.Children.Add(new TextBlock { Text = v.MyIdLine, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"), IsTextSelectionEnabled = true, Opacity = 0.7 });
        }
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var pair = new Button { Content = v.PairTitle, IsEnabled = v.PairEnabled };
        pair.Click += (_, _) => ShowPairLink();
        buttons.Children.Add(pair);
        var paste = new Button { Content = v.LinkTitle, IsEnabled = v.LinkEnabled };
        myId = v.MyIdLine;
        paste.Click += (_, _) => ShowPaste(v.MyIdLine);
        buttons.Children.Add(paste);
        if (v.LeaveTitle is string leave)
        {
            var b = new Button { Content = leave };
            b.Click += (_, _) => AskLeave(v);
            buttons.Children.Add(b);
        }
        panel.Children.Add(buttons);
        panel.Children.Add(new TextBlock { Text = v.Note, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
    }

    UIElement Request(PairRequestRow r)
    {
        var s = new StackPanel { Spacing = 4 };
        s.Children.Add(new TextBlock { Text = r.Question, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap });
        s.Children.Add(new TextBlock { Text = r.IdLine, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"), FontSize = 15, IsTextSelectionEnabled = true });
        s.Children.Add(new TextBlock { Text = r.Hint, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        var b = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        var refuse = new Button { Content = r.RefuseTitle };
        refuse.Click += (_, _) => Run(() => app.Core.DevicesRefuse(r.Id));
        var ok = new Button { Content = r.PairTitle, Style = (Style)Application.Current.Resources["AccentButtonStyle"] };
        ok.Click += (_, _) => Run(() => app.Core.DevicesConfirm(r.Id));
        b.Children.Add(refuse);
        b.Children.Add(ok);
        s.Children.Add(b);
        return s;
    }

    /// <summary>Runs a call; a failure is shown in an alert. The view is read again either way.</summary>
    void Run(Func<Task> work)
    {
        CoreCalls.InOrder("devices", work, () => { drawn = ""; Update(); }, Say);
    }

    void Say(string text)
    {
        if (panel.XamlRoot is not XamlRoot root)
        {
            return;
        }
        _ = new ContentDialog { XamlRoot = root, Content = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap }, CloseButtonText = TrsFfiMethods.AlertOkTitle() }.ShowAsync();
    }

    async void ShowPairLink()
    {
        if (panel.XamlRoot is not XamlRoot root)
        {
            return;
        }
        PairLinkView l;
        try { l = app.Core.DevicesCurrentLink() ?? await app.Core.DevicesPairLink(); }
        catch (Exception e) { Say(CoreCalls.Message(e)); return; }
        link = l;
        var content = new StackPanel { Spacing = 10, MaxWidth = 460 };
        content.Children.Add(new TextBlock { Text = l.Text, TextWrapping = TextWrapping.Wrap, Opacity = 0.8 });
        content.Children.Add(new TextBox { Text = l.Link, IsReadOnly = true, TextWrapping = TextWrapping.Wrap, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas") });
        countdown = new TextBlock { Text = TrsFfiMethods.DevicesCountdown(l.ExpiresS, NowS()), Opacity = 0.7 };
        content.Children.Add(countdown);
        var d = new ContentDialog { XamlRoot = root, Title = l.Title, Content = content, PrimaryButtonText = l.CopyTitle, CloseButtonText = "Done", DefaultButton = ContentDialogButton.Primary };
        d.PrimaryButtonClick += (_, args) =>
        {
            args.Cancel = true;
            var data = new Windows.ApplicationModel.DataTransfer.DataPackage();
            data.SetText(l.Link);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(data);
        };
        await d.ShowAsync();
        link = null;
        countdown = null;
    }

    /// <summary>
    /// Pair with a Link… with <paramref name="link"/> filled in (a seedwell://devices/ link that opened the app); once the
    /// section is on screen when it was just made.
    /// </summary>
    internal void ShowPaste(string link)
    {
        if (panel.XamlRoot is null)
        {
            void Once(object s, RoutedEventArgs e) { panel.Loaded -= Once; ShowPaste(myId, link); }
            panel.Loaded += Once;
            return;
        }
        ShowPaste(myId, link);
    }

    async void ShowPaste(string myId, string link = "")
    {
        if (panel.XamlRoot is not XamlRoot root)
        {
            return;
        }
        var content = new StackPanel { Spacing = 8, MinWidth = 420 };
        var box = new TextBox { PlaceholderText = "seedwell://devices/…" };
        var problem = new TextBlock { TextWrapping = TextWrapping.Wrap, Opacity = 0.8 };
        content.Children.Add(box);
        content.Children.Add(problem);
        content.Children.Add(new TextBlock { Text = myId, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"), Opacity = 0.7, IsTextSelectionEnabled = true });
        var d = new ContentDialog { XamlRoot = root, Title = "Pair with a Link", Content = content, PrimaryButtonText = "Pair", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Primary, IsPrimaryButtonEnabled = false };
        box.TextChanged += (_, _) =>
        {
            var p = app.Core.DevicesLinkProblem(box.Text);
            problem.Text = p ?? "";
            d.IsPrimaryButtonEnabled = p == null && box.Text.Trim().Length > 0;
        };
        box.Text = link;
        AppLog.Write($"devices: pair with a link{(link.Length > 0 ? " (filled in)" : "")}");
        if (await d.ShowAsync() == ContentDialogResult.Primary)
        {
            var text = box.Text.Trim();
            Run(() => app.Core.DevicesJoin(text));
        }
    }

    async void AskRemove(string id)
    {
        if (panel.XamlRoot is not XamlRoot root)
        {
            return;
        }
        RemoveDeviceView v;
        try { v = await app.Core.DevicesRemoveView(id); }
        catch (Exception e) { Say(CoreCalls.Message(e)); return; }
        var content = new StackPanel { Spacing = 8, MaxWidth = 460 };
        content.Children.Add(new TextBlock { Text = v.Text, TextWrapping = TextWrapping.Wrap, Opacity = 0.8 });
        var ticks = new List<(string Id, CheckBox Box)>();
        if (v.Places.Length > 0)
        {
            content.Children.Add(new TextBlock { Text = v.PlacesTitle });
            foreach (var p in v.Places)
            {
                var c = new CheckBox { Content = p.Title, IsChecked = p.Ticked };
                ticks.Add((p.Id, c));
                content.Children.Add(c);
            }
        }
        if (v.AskOwner is string a)
        {
            content.Children.Add(new TextBlock { Text = a, TextWrapping = TextWrapping.Wrap, Opacity = 0.7 });
        }
        if (v.Paired is string pd)
        {
            content.Children.Add(new TextBlock { Text = pd, TextWrapping = TextWrapping.Wrap, Opacity = 0.7 });
        }
        var d = new ContentDialog { XamlRoot = root, Title = v.Title, Content = content, PrimaryButtonText = v.RemoveTitle, CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Close };
        if (await d.ShowAsync() == ContentDialogResult.Primary)
        {
            var places = ticks.Where(t => t.Box.IsChecked == true).Select(t => t.Id).ToArray();
            Run(() => app.Core.DevicesRemove(v.DeviceId, places));
        }
    }

    async void AskLeave(DevicesView v)
    {
        if (panel.XamlRoot is not XamlRoot root)
        {
            return;
        }
        var d = new ContentDialog
        {
            XamlRoot = root,
            Content = new TextBlock { Text = v.LeaveQuestion, TextWrapping = TextWrapping.Wrap },
            PrimaryButtonText = v.LeaveAction,
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Close,
        };
        if (await d.ShowAsync() == ContentDialogResult.Primary)
        {
            var startOver = v.Kind == DevicesKind.Removed;
            Run(() => startOver ? app.Core.DevicesStartOver() : app.Core.DevicesLeave());
        }
    }
}
