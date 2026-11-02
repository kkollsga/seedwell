using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Settings › Folder Sync › "Find your devices over the Internet (Cloudflare)" on Windows (SPEC §7G.5), drawn from the
/// core's <c>RendezvousView</c> (trs-ffi sync_rendezvous.rs, as the Mac's RendezvousSection): while off the guide, the
/// token field with Set Up and another device's address with Use; while working the progress line; while on the address
/// with Copy, its status, Update, Turn Off and Test; under them the last result. Read again every second while shown.
/// The API token goes straight to the core and the field is cleared; it is the user's action, never an agent's.
/// </summary>
sealed class RendezvousSection
{
    readonly App app;
    readonly StackPanel panel = new() { Spacing = 10 };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    // Kept across redraws, so a refresh does not take what is being typed.
    readonly PasswordBox token = new() { MinWidth = 220 };
    readonly TextBox address = new() { MinWidth = 300 };
    readonly Button setUp = new() { IsEnabled = false };
    readonly Button use = new() { IsEnabled = false };
    string drawn = "";
    bool updating;
    string? refused;

    public UIElement Root => panel;

    public RendezvousSection(App app)
    {
        this.app = app;
        token.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Enter) SetUp(); };
        address.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Enter) _ = UseAddress(); };
        setUp.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
        token.PasswordChanged += (_, _) => setUp.IsEnabled = token.Password.Trim().Length > 0;
        address.TextChanged += (_, _) => use.IsEnabled = address.Text.Trim().Length > 0;
        setUp.Click += (_, _) => SetUp();
        use.Click += (_, _) => _ = UseAddress();
        timer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.Interval = TimeSpan.FromSeconds(1);
        timer.Tick += (_, _) => Update();
        panel.Loaded += (_, _) => timer.Start();
        panel.Unloaded += (_, _) => timer.Stop();
        Update();
    }

    /// <summary>The view last drawn (QA reads it).</summary>
    internal RendezvousView? View { get; private set; }

    void Update()
    {
        RendezvousView v;
        try { v = app.Core.SyncRendezvous(); }
        catch (Exception e)
        {
            AppLog.Write($"rendezvous view failed: {e.Message}");
            return;
        }
        if (v.State != RendezvousState.On)
        {
            updating = false;
        }
        var key = $"{v}|{updating}|{refused}";
        if (key != drawn)
        {
            drawn = key;
            View = v;
            Draw(v);
        }
    }

    void Draw(RendezvousView v)
    {
        // The fields and their buttons move to the new drawing (a child has one parent).
        foreach (var kept in new FrameworkElement[] { token, address, setUp, use })
        {
            (kept.Parent as Panel)?.Children.Remove(kept);
        }
        panel.Children.Clear();
        panel.Children.Add(new TextBlock { Text = v.Title, FontSize = 18, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Margin = new Thickness(0, 12, 0, 0), TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(new TextBlock { Text = v.Explanation, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        switch (v.State)
        {
            case RendezvousState.Off:
                panel.Children.Add(Guide(v));
                panel.Children.Add(Existing(v));
                break;
            case RendezvousState.Working:
                var working = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
                working.Children.Add(new ProgressRing { IsActive = true, Width = 16, Height = 16 });
                working.Children.Add(new TextBlock { Text = v.Progress, VerticalAlignment = VerticalAlignment.Center });
                panel.Children.Add(working);
                break;
            case RendezvousState.On:
                panel.Children.Add(InUse(v));
                if (updating)
                {
                    panel.Children.Add(Guide(v));
                }
                break;
        }
        if ((refused ?? (v.Result.Length > 0 ? v.Result : null)) is string line)
        {
            bool problem = refused is not null || v.ResultIsProblem;
            var result = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            result.Children.Add(new FontIcon
            {
                Glyph = problem ? "" : "",
                FontSize = 12,
                VerticalAlignment = VerticalAlignment.Top,
                Margin = new Thickness(0, 2, 0, 0),
                Foreground = problem ? (Brush)Application.Current.Resources["SystemFillColorCautionBrush"] : (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
            });
            result.Children.Add(new TextBlock { Text = line, FontSize = 12, TextWrapping = TextWrapping.Wrap, MaxWidth = 600, Opacity = problem ? 1 : 0.7 });
            panel.Children.Add(result);
        }
    }

    /// <summary>The numbered steps to the token, Open, the token field and Set Up.</summary>
    UIElement Guide(RendezvousView v)
    {
        var s = new StackPanel { Spacing = 6 };
        for (int i = 0; i < v.Steps.Length; i++)
        {
            var step = new Grid { ColumnSpacing = 6 };
            step.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(18) });
            step.ColumnDefinitions.Add(new ColumnDefinition());
            step.Children.Add(new TextBlock { Text = $"{i + 1}.", Opacity = 0.7, HorizontalAlignment = HorizontalAlignment.Right });
            var text = new TextBlock { Text = v.Steps[i], TextWrapping = TextWrapping.Wrap };
            Grid.SetColumn(text, 1);
            step.Children.Add(text);
            s.Children.Add(step);
        }
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
        var open = new Button { Content = v.OpenLabel };
        var url = v.TokenPageUrl;
        open.Click += (_, _) => _ = Windows.System.Launcher.LaunchUriAsync(new Uri(url));
        line.Children.Add(open);
        token.PlaceholderText = v.TokenPlaceholder;
        line.Children.Add(token);
        setUp.Content = v.SetUpLabel;
        line.Children.Add(setUp);
        s.Children.Add(line);
        return s;
    }

    /// <summary>Another device's address (or one deployed with wrangler) and Use.</summary>
    UIElement Existing(RendezvousView v)
    {
        var s = new StackPanel { Spacing = 6 };
        s.Children.Add(new TextBlock { Text = v.ExistingHint, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        address.PlaceholderText = v.UrlPlaceholder;
        line.Children.Add(address);
        use.Content = v.UseLabel;
        line.Children.Add(use);
        s.Children.Add(line);
        return s;
    }

    /// <summary>The address with Copy, its status, and Update, Turn Off and Test.</summary>
    UIElement InUse(RendezvousView v)
    {
        var s = new StackPanel { Spacing = 8 };
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        line.Children.Add(new Border
        {
            CornerRadius = new CornerRadius(4),
            Padding = new Thickness(8, 3, 8, 4),
            VerticalAlignment = VerticalAlignment.Center,
            Background = (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"],
            Child = new TextBlock { Text = v.Url, FontFamily = new FontFamily("Consolas"), IsTextSelectionEnabled = true },
        });
        var copy = new Button { Content = v.CopyLabel };
        var url = v.Url;
        copy.Click += (_, _) =>
        {
            var data = new Windows.ApplicationModel.DataTransfer.DataPackage();
            data.SetText(url);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(data);
        };
        line.Children.Add(copy);
        s.Children.Add(line);
        if (v.Status.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = v.Status, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        }
        var buttons = new Grid();
        buttons.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        buttons.ColumnDefinitions.Add(new ColumnDefinition());
        buttons.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var update = new Button { Content = v.UpdateLabel };
        update.Click += (_, _) => { updating = !updating; drawn = ""; Update(); };
        buttons.Children.Add(update);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var off = new Button { Content = v.TurnOffLabel };
        off.Click += async (_, _) => await Run(() => app.Core.SyncRendezvousTurnOff());
        right.Children.Add(off);
        var test = new Button { Content = v.TestLabel };
        test.Click += async (_, _) => await Run(() => app.Core.SyncRendezvousTest());
        right.Children.Add(test);
        Grid.SetColumn(right, 2);
        buttons.Children.Add(right);
        s.Children.Add(buttons);
        return s;
    }

    internal async void SetUp()
    {
        var t = token.Password.Trim();
        if (t.Length == 0)
        {
            return;
        }
        token.Password = "";
        await Run(() => app.Core.SyncRendezvousSetUp(t));
    }

    internal async Task UseAddress()
    {
        var a = address.Text.Trim();
        if (a.Length == 0)
        {
            return;
        }
        if (await Run(() => app.Core.SyncRendezvousUse(a)))
        {
            address.Text = "";
        }
    }

    /// <summary>An action, awaited off the UI thread (it runs on in the background); false when the core refused it, which is shown.</summary>
    internal async Task<bool> Run(Func<Task> act)
    {
        try
        {
            await act();
            refused = null;
        }
        catch (Exception e)
        {
            refused = CoreCalls.Message(e);
        }
        drawn = "";
        Update();
        return refused is null;
    }

    /// <summary>Why the last action was refused at once (QA reads it).</summary>
    internal string? Refused => refused;

    /// <summary>QA: types into the address field (as a paste would), so Use can be pressed.</summary>
    internal void QaAddress(string text) => address.Text = text;
}
