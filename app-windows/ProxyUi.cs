using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The torrent proxy as every view of it shares it (docs/build-77-proxy.md): the core's model, read at its pace while a
/// view of it is open; a call at a time, with the switch, the shield and the country off meanwhile; the shield's click
/// and its context menu. Every word and state is the core's.
/// </summary>
sealed class ProxyUi
{
    readonly App app;
    readonly DispatcherQueueTimer timer;
    readonly DispatcherQueueTimer noticeTimer;
    int watchers;

    internal ProxyUi(App app)
    {
        this.app = app;
        var queue = DispatcherQueue.GetForCurrentThread();
        timer = queue.CreateTimer();
        timer.Tick += (_, _) => Poll();
        noticeTimer = queue.CreateTimer();
        noticeTimer.IsRepeating = false;
        noticeTimer.Interval = TimeSpan.FromSeconds(5);
        noticeTimer.Tick += (_, _) =>
        {
            Notice = "";
            Changed?.Invoke();
        };
        Model = app.Core.ProxyModel();
    }

    internal ProxyModel Model { get; private set; }

    /// <summary>A call of ours runs (a toggle, a key test, Find a Faster Server, a country).</summary>
    internal bool Busy { get; private set; }

    /// <summary>A sentence for a few seconds: a refusal, or what Find a Faster Server found.</summary>
    internal string Notice { get; private set; } = "";

    internal event Action? Changed;

    /// <summary>Runs before the window opens from here (the flyout goes away).</summary>
    internal Action? BeforeWindow { get; set; }

    /// <summary>The menu or the Country list opened or closed: the menu closes as the list opens, in either order.</summary>
    void PopupOpen(bool open)
    {
        popups = Math.Max(0, popups + (open ? 1 : -1));
        MenuOpen?.Invoke(popups > 0);
    }

    int popups;

    /// <summary>A context menu is open (the flyout stays while it is).</summary>
    internal Action<bool>? MenuOpen { get; set; }

    /// <summary>A view of the proxy opened or went: the model is read only while one is open.</summary>
    internal void Watch(bool on)
    {
        watchers = Math.Max(0, watchers + (on ? 1 : -1));
        if (watchers == 0)
        {
            timer.Stop();
        }
        else if (on)
        {
            Poll();
        }
    }

    /// <summary>A step on the UI thread: a stall's line names it, and in a QA run a slow one is noted (qa\run.ps1 proxy fails on it).</summary>
    internal static T Step<T>(string what, Func<T> step)
    {
        var doing = HangWatch.Doing;
        HangWatch.Doing = what;
        var clock = System.Diagnostics.Stopwatch.StartNew();
        try
        {
            return step();
        }
        finally
        {
            HangWatch.Doing = doing;
            if (Quiet.On && clock.ElapsedMilliseconds >= SlowStepMs)
            {
                AppLog.Write($"proxy: slow on the UI thread: {what} took {clock.ElapsedMilliseconds} ms");
            }
        }
    }

    const long SlowStepMs = 250;

    internal void Poll()
    {
        var m = Step("ProxyModel", app.Core.ProxyModel);
        if (watchers > 0)
        {
            timer.Interval = TimeSpan.FromMilliseconds(m.RefreshMs);
            timer.Start();
        }
        // (The countries are a new array at each read.)
        if (m with { Countries = Model.Countries } != Model)
        {
            Model = m;
            Changed?.Invoke();
        }
    }

    internal void Say(string sentence)
    {
        Notice = sentence;
        noticeTimer.Stop();
        noticeTimer.Start();
        Changed?.Invoke();
    }

    /// <summary>A call: nothing else meanwhile, the model read after it; a refusal or failure is the core's sentence.</summary>
    internal async Task Run(Func<Task> call)
    {
        if (Busy)
        {
            return;
        }
        Busy = true;
        Changed?.Invoke();
        try
        {
            await call();
        }
        catch (Exception e)
        {
            Say(CoreCalls.Message(e));
        }
        Busy = false;
        Poll();
        Changed?.Invoke();
    }

    /// <summary>For the window's calls, whose answer it shows itself.</summary>
    internal void SetBusy(bool on)
    {
        Busy = on;
        Changed?.Invoke();
    }

    internal Task Toggle() => Run(async () => Update(await app.Core.ProxyToggle()));

    internal Task SetCountry(string code) => Run(async () => Update(await app.Core.ProxySetCountry(code)));

    internal Task FindFaster() => Run(async () => Say(await app.Core.ProxyFindFaster()));

    void Update(ProxyModel m)
    {
        Model = m;
    }

    /// <summary>A left click: the window without a key, the switch with one.</summary>
    internal void Click()
    {
        if (Model.ClickOpensWindow)
        {
            OpenWindow();
        }
        else
        {
            _ = Toggle();
        }
    }

    internal void OpenWindow()
    {
        BeforeWindow?.Invoke();
        ProxyWindow.Show(app);
    }

    /// <summary>A right click (the Mac's context menu): the window, the switch, Find a Faster Server, Country.</summary>
    internal void ShowMenu(FrameworkElement at)
    {
        var m = Model;
        var menu = new MenuFlyout();
        foreach (var (title, run, enabled) in MenuEntries(m))
        {
            var item = new MenuFlyoutItem { Text = title, IsEnabled = enabled };
            item.Click += (_, _) => run();
            menu.Items.Add(item);
        }
        menu.Items.Add(new MenuFlyoutSeparator());
        var countries = new MenuFlyoutItem { Text = m.CountryMenuLabel, IsEnabled = !Busy };
        countries.Click += (_, _) => ShowCountries(at);
        menu.Items.Add(countries);
        menu.Opened += (_, _) => PopupOpen(true);
        menu.Closed += (_, _) => PopupOpen(false);
        menu.ShowAt(at);
    }

    /// <summary>
    /// The Country list, the same from the menu and the window's button: the core's rows tall and scrolling,
    /// "No preference" first, a check at the current one, which is in view as it opens. Arrows move, Return or a click
    /// chooses, a letter goes to the first country with it.
    /// </summary>
    internal void ShowCountries(FrameworkElement at)
    {
        const double RowHeight = 36;
        var m = Model;
        var row = new Style(typeof(ListViewItem));
        row.Setters.Add(new Setter(FrameworkElement.MinHeightProperty, RowHeight));
        row.Setters.Add(new Setter(FrameworkElement.HeightProperty, RowHeight));
        var list = new ListView { SelectionMode = ListViewSelectionMode.Single, IsItemClickEnabled = true, ItemContainerStyle = row, Height = m.CountryVisibleRows * RowHeight, MinWidth = 240 };
        foreach (var c in m.Countries)
        {
            var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
            line.Children.Add(new FontIcon { Glyph = "", FontSize = 12, Opacity = c.Code == m.Country ? 1 : 0 });
            line.Children.Add(new TextBlock { Text = c.Name, VerticalAlignment = VerticalAlignment.Center });
            list.Items.Add(line);
        }
        list.SelectedIndex = Array.FindIndex(m.Countries, c => c.Code == m.Country);
        var flyout = new Flyout { Content = list, Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.Bottom };
        void Choose(int i)
        {
            flyout.Hide();
            if (i >= 0 && m.Countries[i].Code != Model.Country)
            {
                _ = SetCountry(m.Countries[i].Code);
            }
        }
        bool Key(Windows.System.VirtualKey key)
        {
            if (key == Windows.System.VirtualKey.Enter)
            {
                Choose(list.SelectedIndex);
                return true;
            }
            if (key is >= Windows.System.VirtualKey.A and <= Windows.System.VirtualKey.Z)
            {
                var letter = key.ToString();
                var i = Array.FindIndex(m.Countries, c => c.Code.Length > 0 && c.Name.StartsWith(letter, StringComparison.OrdinalIgnoreCase));
                if (i >= 0)
                {
                    list.SelectedIndex = i;
                    list.ScrollIntoView(list.Items[i]);
                }
                return true;
            }
            return false;
        }
        list.ItemClick += (_, e) => Choose(list.Items.IndexOf(e.ClickedItem));
        list.PreviewKeyDown += (_, e) => e.Handled = Key(e.Key);
        list.Loaded += (_, _) =>
        {
            if (list.SelectedItem is not null)
            {
                list.ScrollIntoView(list.SelectedItem);
            }
            list.Focus(FocusState.Programmatic);
        };
        flyout.Opened += (_, _) => PopupOpen(true);
        flyout.Closed += (_, _) =>
        {
            PopupOpen(false);
            QaCountries = null;
        };
        QaCountries = (list, k => Key(k), Choose);
        flyout.ShowAt(at);
    }

    /// <summary>The Country list open now, what a key does in it and what a click does (QA).</summary>
    internal (ListView List, Func<Windows.System.VirtualKey, bool> Key, Action<int> Choose)? QaCountries { get; private set; }

    IEnumerable<(string Title, Action Run, bool Enabled)> MenuEntries(ProxyModel m)
    {
        yield return (m.MenuOpenLabel, OpenWindow, true);
        if (m.MenuToggleLabel.Length > 0)
        {
            yield return (m.MenuToggleLabel, () => _ = Toggle(), !Busy);
        }
        if (m.FindFasterLabel.Length > 0)
        {
            yield return (m.FindFasterLabel, () => _ = FindFaster(), !Busy);
        }
    }

    /// <summary>The On switch of Settings and of the window: shown with a key, on while enabled, off while a call runs.</summary>
    internal ToggleSwitch NewSwitch()
    {
        var s = new ToggleSwitch { OnContent = "", OffContent = "", MinWidth = 0, VerticalAlignment = VerticalAlignment.Center };
        s.Toggled += (_, _) =>
        {
            if (s.IsOn != Model.Enabled)
            {
                _ = Toggle();
            }
        };
        return s;
    }

    internal void Sync(ToggleSwitch s)
    {
        var m = Model;
        s.Visibility = m.HasKey ? Visibility.Visible : Visibility.Collapsed;
        // (While its own call runs it shows what the user chose: the model catches up when the call ends.)
        if (!Busy)
        {
            s.IsOn = m.Enabled;
        }
        s.IsEnabled = !Busy;
        ToolTipService.SetToolTip(s, m.MenuToggleLabel);
        AutomationProperties.SetName(s, m.MenuToggleLabel);
    }

    /// <summary>The model as this reads it (QA).</summary>
    internal string QaDescribe()
    {
        var m = Model;
        return $"icon {m.Icon}, has key {m.HasKey}, enabled {m.Enabled}, kind {m.Kind}, server \"{m.Server}\", status \"{m.StatusLine}\", problem \"{m.Problem}\", tooltip \"{m.Tooltip}\", opens window {m.ClickOpensWindow}, country \"{m.Country}\", countries {m.Countries.Length}, find faster \"{m.FindFasterLabel}\", busy {Busy}, notice \"{Notice}\", watchers {watchers}";
    }
}

/// <summary>Settings › Network's Torrent proxy box: the shield, the status, the key's line, the switch, "SOCKS5 Proxy…".</summary>
sealed class ProxySection
{
    readonly App app;
    readonly ProxyUi ui;
    readonly TextBlock status = new() { FontWeight = FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap };
    readonly TextBlock keyLine = new() { FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };
    readonly TextBlock problem = new() { FontSize = 12, Foreground = Res("SystemFillColorCriticalBrush"), TextWrapping = TextWrapping.Wrap };
    readonly TextBlock notice = new() { FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };
    readonly ToggleSwitch toggle;
    readonly Button open = new() { VerticalAlignment = VerticalAlignment.Center };
    readonly Grid row = new() { ColumnSpacing = 12, Padding = new Thickness(12, 10, 12, 10) };

    internal ProxySection(App app, string title)
    {
        this.app = app;
        ui = app.Proxy;
        toggle = ui.NewSwitch();
        Shield = new ProxyShieldButton(ui, 24);
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.Children.Add(Shield);
        var text = new StackPanel { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
        text.Children.Add(status);
        text.Children.Add(keyLine);
        text.Children.Add(problem);
        text.Children.Add(notice);
        Grid.SetColumn(text, 1);
        row.Children.Add(text);
        Grid.SetColumn(toggle, 2);
        row.Children.Add(toggle);
        Grid.SetColumn(open, 3);
        row.Children.Add(open);
        open.Click += (_, _) => ui.OpenWindow();

        var s = new StackPanel { Spacing = 6 };
        s.Children.Add(new TextBlock { Text = title.ToUpperInvariant(), FontSize = 11, FontWeight = FontWeights.SemiBold, CharacterSpacing = 40, Foreground = Res("TextFillColorSecondaryBrush"), Margin = new Thickness(4, 0, 0, 0) });
        s.Children.Add(new Border { Child = row, CornerRadius = new CornerRadius(8), Background = Res("CardBackgroundFillColorDefaultBrush"), BorderBrush = Res("CardStrokeColorDefaultBrush"), BorderThickness = new Thickness(1) });
        Root = s;
        s.Loaded += (_, _) =>
        {
            ui.Changed += Follow;
            ui.Watch(true);
            Follow();
        };
        s.Unloaded += (_, _) =>
        {
            ui.Changed -= Follow;
            ui.Watch(false);
        };
        Follow();
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    internal FrameworkElement Root { get; }

    internal ProxyShieldButton Shield { get; }

    void Follow()
    {
        var m = ui.Model;
        status.Text = m.StatusLine;
        keyLine.Text = m.HasKey ? app.Core.ProxyOpenModalModel().KeyLine : "";
        keyLine.Visibility = m.HasKey ? Visibility.Visible : Visibility.Collapsed;
        problem.Text = m.Problem ?? "";
        problem.Visibility = m.Problem is null ? Visibility.Collapsed : Visibility.Visible;
        notice.Text = ui.Notice;
        notice.Visibility = ui.Notice.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
        ui.Sync(toggle);
        open.Content = m.MenuOpenLabel;
        AutomationProperties.SetName(row, m.Problem is null ? m.StatusLine : $"{m.StatusLine}. {m.Problem}");
    }
}
