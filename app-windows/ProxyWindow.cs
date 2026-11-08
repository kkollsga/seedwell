using System.Runtime.InteropServices;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The "SOCKS5 proxy" window (docs/build-77-proxy.md), the Mac's layout with Windows' button order: the shield, the status
/// and the On switch; what the proxy does; NordVPN or another proxy, with the installed key's line and Replace Key, or the
/// fields; the country and Find a Faster Server; the last result; Remove Key and Close. Every word is the core's. Its
/// controls are made once and followed, never redrawn: the model moves while the user types. The key never comes back.
/// </summary>
sealed class ProxyWindow
{
    readonly App app;
    readonly ProxyUi ui;
    readonly ContentDialog dialog;
    readonly StackPanel content = new() { Spacing = 10, MinWidth = 480 };

    readonly TextBlock title = new() { FontSize = 20, FontWeight = FontWeights.SemiBold };
    readonly TextBlock status = Faint();
    readonly TextBlock problem = new() { FontSize = 12, Foreground = Res("SystemFillColorCriticalBrush"), TextWrapping = TextWrapping.Wrap };
    readonly TextBlock notice = Faint();
    readonly ToggleSwitch toggle;
    readonly ProxyShieldButton shield;

    readonly TextBlock explanation = Faint(13);
    readonly TextBlock killSwitch = new() { TextWrapping = TextWrapping.Wrap };
    readonly TextBlock keyLine = new() { TextWrapping = TextWrapping.Wrap };
    readonly FontIcon keyGlyph = new() { Glyph = "", FontSize = 14, VerticalAlignment = VerticalAlignment.Top };

    readonly Button nordTab = new();
    readonly Button customTab = new();
    readonly StackPanel nordPane = new() { Spacing = 8 };
    readonly StackPanel customPane = new() { Spacing = 8 };

    readonly TextBlock nordIntro = Faint(13);
    readonly HyperlinkButton guideToggle = new() { Padding = new Thickness(0) };
    readonly StackPanel guide = new() { Spacing = 4 };
    readonly TextBlock nordInstalled = new() { VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap };
    readonly Button nordReveal = new();
    readonly Grid nordInstalledRow;
    readonly StackPanel nordFields = new() { Spacing = 8 };
    readonly TextBox username = new();
    readonly PasswordBox password = new();
    readonly TextBlock pasteHint = Faint();
    readonly TextBlock pastedNote = Faint();
    readonly Button nordGo = new() { Style = (Style)Application.Current.Resources["AccentButtonStyle"] };
    readonly Button nordCancel = new();

    readonly TextBlock customIntro = Faint(13);
    readonly TextBlock customInstalled = new() { VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap };
    readonly Button customReveal = new();
    readonly Grid customInstalledRow;
    readonly StackPanel customFields = new() { Spacing = 8 };
    readonly TextBox host = new();
    readonly TextBox port = new() { Width = 96 };
    readonly TextBox customUser = new();
    readonly PasswordBox customPassword = new();
    readonly Button customGo = new() { Style = (Style)Application.Current.Resources["AccentButtonStyle"] };
    readonly Button customCancel = new();

    readonly TextBlock countryLabel = new() { VerticalAlignment = VerticalAlignment.Center };
    // Shows the current country; opens the list ProxyUi draws.
    readonly Button country = new() { MinWidth = 180, HorizontalContentAlignment = HorizontalAlignment.Stretch };
    readonly TextBlock countryName = new();
    readonly FontIcon guideChevron = new() { FontSize = 10 };
    readonly TextBlock guideLabel = new();
    readonly Button findFaster = new() { HorizontalAlignment = HorizontalAlignment.Right };
    readonly TextBlock countryHint = Faint();
    readonly ProgressRing ring = new() { Width = 16, Height = 16, IsActive = false };
    readonly TextBlock progress = new() { VerticalAlignment = VerticalAlignment.Center };
    readonly StackPanel progressRow = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
    readonly FontIcon resultGlyph = new() { FontSize = 14, VerticalAlignment = VerticalAlignment.Top };
    readonly TextBlock result = new() { TextWrapping = TextWrapping.Wrap };
    readonly Grid resultRow;

    readonly DispatcherTimer tick = new() { Interval = TimeSpan.FromMilliseconds(500) };

    string[] shownSteps = [];
    bool showCustom;
    bool guideOpen;
    bool replacingNord;
    bool replacingCustom;
    bool hostFilled;
    bool watching;
    bool done;
    bool progressShown;
    string answer = "";
    bool answerIsProblem;

    // Copied credentials (the core says which field a text fills): what each field got from a copy, and when the username did.
    string? copiedUser;
    string? copiedPassword;
    DateTime? usernameCopied;
    uint clipboardSeen;

    ProxyWindow(App app, XamlRoot root)
    {
        this.app = app;
        ui = app.Proxy;
        toggle = ui.NewSwitch();
        shield = new ProxyShieldButton(ui, 32);
        showCustom = ui.Model.HasKey && ui.Model.Kind == ProxyKind.Custom;

        // (As wide as the content: a dialog's title is only as wide as it asks to be, and the switch goes at its right.)
        var header = new Grid { ColumnSpacing = 12, MinWidth = 588 };
        header.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        header.ColumnDefinitions.Add(new ColumnDefinition());
        header.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        header.Children.Add(shield);
        var head = new StackPanel { Spacing = 1 };
        head.Children.Add(title);
        head.Children.Add(status);
        head.Children.Add(problem);
        head.Children.Add(notice);
        Grid.SetColumn(head, 1);
        header.Children.Add(head);
        Grid.SetColumn(toggle, 2);
        header.Children.Add(toggle);

        content.Children.Add(explanation);
        content.Children.Add(Glyphed(Hand(), killSwitch));
        content.Children.Add(Glyphed(keyGlyph, keyLine));

        var tabs = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        tabs.Children.Add(nordTab);
        tabs.Children.Add(customTab);
        nordTab.Click += (_, _) => SetPane(false);
        customTab.Click += (_, _) => SetPane(true);
        content.Children.Add(tabs);

        nordInstalledRow = Installed(nordInstalled, nordReveal);
        customInstalledRow = Installed(customInstalled, customReveal);
        nordReveal.Click += (_, _) => { replacingNord = true; Follow(); };
        customReveal.Click += (_, _) => { replacingCustom = true; Follow(); };
        nordCancel.Click += (_, _) => { replacingNord = false; EmptyNord(); Follow(); };
        customCancel.Click += (_, _) => { replacingCustom = false; EmptyCustom(); Follow(); };
        var guideFace = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        guideFace.Children.Add(guideChevron);
        guideFace.Children.Add(guideLabel);
        guideToggle.Content = guideFace;
        guideToggle.Click += (_, _) => { guideOpen = !guideOpen; Follow(); };

        nordPane.Children.Add(nordIntro);
        nordPane.Children.Add(guideToggle);
        nordPane.Children.Add(guide);
        nordPane.Children.Add(nordInstalledRow);
        nordFields.Children.Add(username);
        nordFields.Children.Add(password);
        nordFields.Children.Add(pasteHint);
        nordFields.Children.Add(pastedNote);
        nordFields.Children.Add(Buttons(nordGo, nordCancel));
        nordPane.Children.Add(nordFields);
        username.TextChanged += (_, _) =>
        {
            // Typed over: no longer the username a copied password goes with.
            if (username.Text != copiedUser) { copiedUser = null; usernameCopied = null; }
            Follow();
        };
        password.PasswordChanged += (_, _) =>
        {
            if (password.Password != copiedPassword) copiedPassword = null;
            Follow();
        };
        nordGo.Click += (_, _) => _ = EnableNordVpn();

        customPane.Children.Add(customIntro);
        customPane.Children.Add(customInstalledRow);
        var address = new Grid { ColumnSpacing = 8 };
        address.ColumnDefinitions.Add(new ColumnDefinition());
        address.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        address.Children.Add(host);
        Grid.SetColumn(port, 1);
        address.Children.Add(port);
        customFields.Children.Add(address);
        customFields.Children.Add(customUser);
        customFields.Children.Add(customPassword);
        customFields.Children.Add(Buttons(customGo, customCancel));
        customPane.Children.Add(customFields);
        host.TextChanged += (_, _) => Follow();
        customGo.Click += (_, _) => _ = UseCustom();

        content.Children.Add(new Border { Child = PanesHost(), Padding = new Thickness(12), CornerRadius = new CornerRadius(8), Background = Res("CardBackgroundFillColorDefaultBrush"), BorderBrush = Res("CardStrokeColorDefaultBrush"), BorderThickness = new Thickness(1) });

        var countryRow = new Grid { ColumnSpacing = 8 };
        countryRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        countryRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        countryRow.ColumnDefinitions.Add(new ColumnDefinition());
        countryRow.Children.Add(countryLabel);
        Grid.SetColumn(country, 1);
        countryRow.Children.Add(country);
        Grid.SetColumn(findFaster, 2);
        countryRow.Children.Add(findFaster);
        content.Children.Add(countryRow);
        content.Children.Add(countryHint);
        var countryFace = new Grid { ColumnSpacing = 12 };
        countryFace.ColumnDefinitions.Add(new ColumnDefinition());
        countryFace.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        countryFace.Children.Add(countryName);
        var countryChevron = new FontIcon { Glyph = "", FontSize = 10 };
        Grid.SetColumn(countryChevron, 1);
        countryFace.Children.Add(countryChevron);
        country.Content = countryFace;
        country.Click += (_, _) => ui.ShowCountries(country);
        findFaster.Click += (_, _) => _ = FindFaster();

        progressRow.Children.Add(ring);
        progressRow.Children.Add(progress);
        content.Children.Add(progressRow);
        resultRow = Glyphed(resultGlyph, result);
        content.Children.Add(resultRow);

        dialog = new ContentDialog
        {
            XamlRoot = root,
            Title = header,
            Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
        };
        dialog.Resources["ContentDialogMaxWidth"] = 640.0;
        dialog.SecondaryButtonClick += (sender, e) =>
        {
            // Remove Key keeps the window: what it did shows in it.
            e.Cancel = true;
            _ = RemoveKey();
        };
        // Only once it is on screen: ShowAsync throws while another dialog is open, and no Closed comes then, so a window
        // that never opened must never watch the clipboard.
        tick.Tick += (_, _) => Tick();
        dialog.Opened += (_, _) =>
        {
            if (done)
            {
                return;
            }
            watching = true;
            ui.Changed += Follow;
            ui.Watch(true);
            // QA leaves the user's clipboard alone: a check gives its text to Copied.
            if (!Quiet.On)
            {
                clipboardSeen = GetClipboardSequenceNumber();
            }
            tick.Start();
        };
        dialog.Closed += (_, _) => Finish();
        Follow();
    }

    /// <summary>It closed, or never opened: nothing watched any more, nothing kept, and a copy still on its way fills nothing.</summary>
    void Finish()
    {
        done = true;
        tick.Stop();
        if (watching)
        {
            watching = false;
            ui.Changed -= Follow;
            ui.Watch(false);
        }
        EmptyNord();
        EmptyCustom();
    }

    /// <summary>The other pane: what either held goes, and with it the username a copied password would pair with.</summary>
    void SetPane(bool custom)
    {
        showCustom = custom;
        replacingNord = false;
        replacingCustom = false;
        EmptyNord();
        EmptyCustom();
        Follow();
    }

    [DllImport("user32.dll")]
    static extern uint GetClipboardSequenceNumber();

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Faint(double size = 12) => new() { FontSize = size, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };

    /// <summary>
    /// The kill switch's mark, "stop and wait" and not an error: Hand Left, filled, of Microsoft's Fluent UI System Icons
    /// (github.com/microsoft/fluentui-system-icons, MIT, Copyright (c) 2020 Microsoft Corporation), in the proxy's orange.
    /// </summary>
    static Viewbox Hand()
    {
        var hand = new Microsoft.UI.Xaml.Shapes.Path
        {
            Width = 20,
            Height = 20,
            Data = (Geometry)Microsoft.UI.Xaml.Markup.XamlBindingHelper.ConvertValue(typeof(Geometry), "M11 8.5V2.75a.75.75 0 0 0-1.5 0V8.5a.5.5 0 0 1-1 0V3.75a.75.75 0 0 0-1.5 0v6.5a4 4 0 0 0-2-.75c-.97 0-1.79.32-1.98 1.12l-.02.14q0 .24.2.38l1.3.99c1.07.8 2.07 1.86 2.76 2.99q.56.9 1.06 1.86C8.68 17.66 8.9 18 10 18h3.05c.95 0 1.31-.53 1.95-2s1-2.94 1-3.5V5.25a.75.75 0 0 0-1.5 0V9a.5.5 0 0 1-1 0V3.75a.75.75 0 0 0-1.5 0V8.5a.5.5 0 0 1-1 0"),
        };
        void Paint() => hand.Fill = new SolidColorBrush(ProxyShield.Colours(ProxyIcon.Slow, hand.ActualTheme == ElementTheme.Dark).Shield);
        hand.ActualThemeChanged += (_, _) => Paint();
        hand.Loaded += (_, _) => Paint();
        return new Viewbox { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Top, Child = new Canvas { Width = 20, Height = 20, Children = { hand } } };
    }

    static Grid Glyphed(FrameworkElement glyph, TextBlock text)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(glyph);
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        return g;
    }

    static Grid Installed(TextBlock line, Button reveal)
    {
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = "", FontSize = 16, Foreground = Res("SystemFillColorSuccessBrush"), VerticalAlignment = VerticalAlignment.Center });
        Grid.SetColumn(line, 1);
        g.Children.Add(line);
        Grid.SetColumn(reveal, 2);
        g.Children.Add(reveal);
        return g;
    }

    /// <summary>The pane's buttons, Windows' order: the action first.</summary>
    static StackPanel Buttons(Button go, Button cancel)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        s.Children.Add(go);
        s.Children.Add(cancel);
        return s;
    }

    Grid PanesHost()
    {
        var g = new Grid();
        g.Children.Add(nordPane);
        g.Children.Add(customPane);
        return g;
    }

    /// <summary>The window open now.</summary>
    internal static ProxyWindow? Open { get; private set; }

    /// <summary>The window last made, open or not (QA).</summary>
    internal static ProxyWindow? QaLast { get; private set; }

    /// <summary>Opens it. One at a time.</summary>
    internal static async void Show(App app)
    {
        if (Open is not null || await AddFolderDialog.Root(app) is not XamlRoot root || Open is not null)
        {
            return;
        }
        var w = new ProxyWindow(app, root);
        Open = w;
        QaLast = w;
        AppLog.Write("proxy: window open");
        try
        {
            await w.dialog.ShowAsync();
        }
        catch (Exception e)
        {
            AppLog.Write($"proxy window failed: {e.Message}");
            w.Finish();
        }
        if (Open == w)
        {
            Open = null;
        }
    }

    void Tick()
    {
        if (done)
        {
            return;
        }
        // (A call started elsewhere shows as Progress: followed until it has ended, or the controls stay off.)
        if (ui.Busy || progressShown)
        {
            Follow();
        }
        if (Quiet.On)
        {
            return;
        }
        // Only when the clipboard changed, never its text on a timer; never while the fields do not show.
        var seen = GetClipboardSequenceNumber();
        if (seen != clipboardSeen)
        {
            clipboardSeen = seen;
            if (WantsCopies)
            {
                _ = ReadClipboard();
            }
        }
    }

    bool WantsCopies => !done && !showCustom && nordFields.Visibility == Visibility.Visible && (username.Text.Length == 0 || password.Password.Length == 0);

    async Task ReadClipboard()
    {
        try
        {
            var data = ProxyUi.Step("Clipboard.GetContent", Windows.ApplicationModel.DataTransfer.Clipboard.GetContent);
            if (data.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Text))
            {
                Copied(await data.GetTextAsync());
            }
        }
        catch (Exception e)
        {
            // Another program holds the clipboard: nothing filled. Never its text in the log.
            AppLog.Write($"proxy: the clipboard could not be read ({e.GetType().Name})");
        }
    }

    /// <summary>A new text on the clipboard: the core says which field it fills, if any.</summary>
    void Copied(string text)
    {
        if (!WantsCopies)
        {
            return;
        }
        ulong? ago = usernameCopied is DateTime at ? (ulong)(DateTime.UtcNow - at).TotalMilliseconds : null;
        switch (SeedwellFfiMethods.ProxyPasteFill(username.Text, password.Password, text, ago))
        {
            case ProxyPaste.Username u:
                copiedUser = u.Text;
                usernameCopied = DateTime.UtcNow;
                username.Text = u.Text;
                break;
            case ProxyPaste.Password p:
                copiedPassword = p.Text;
                password.Password = p.Text;
                break;
        }
        Follow();
    }

    void EmptyNord()
    {
        copiedUser = null;
        copiedPassword = null;
        usernameCopied = null;
        username.Text = "";
        password.Password = "";
    }

    void EmptyCustom()
    {
        customUser.Text = "";
        customPassword.Password = "";
        hostFilled = false;
    }

    /// <summary>Follows the core's model and this window's own state: no control is made anew.</summary>
    void Follow() => ProxyUi.Step("the SOCKS5 window following the model", () =>
    {
        FollowNow();
        return true;
    });

    void FollowNow()
    {
        var m = ui.Model;
        var w = ProxyUi.Step("ProxyOpenModalModel", app.Core.ProxyOpenModalModel);
        var busy = ui.Busy || w.Progress.Length > 0;
        progressShown = w.Progress.Length > 0;

        title.Text = w.Title;
        status.Text = m.StatusLine;
        Show(problem, m.Problem ?? "");
        Show(notice, ui.Notice);
        ui.Sync(toggle);
        toggle.IsEnabled = !busy;
        country.IsEnabled = !busy;

        explanation.Text = w.Explanation;
        killSwitch.Text = w.KillSwitch;
        keyLine.Text = w.KeyLine;
        keyGlyph.Foreground = Res(w.HasKey ? "SystemFillColorSuccessBrush" : "TextFillColorSecondaryBrush");

        Tab(nordTab, w.NordvpnTitle, !showCustom);
        Tab(customTab, w.CustomTitle, showCustom);
        nordPane.Visibility = showCustom ? Visibility.Collapsed : Visibility.Visible;
        customPane.Visibility = showCustom ? Visibility.Visible : Visibility.Collapsed;

        nordIntro.Text = w.NordvpnIntro;
        guideLabel.Text = guideOpen ? w.GuideHideLabel : w.GuideShowLabel;
        guideChevron.Glyph = guideOpen ? "" : "";
        FollowGuide(w);
        guide.Visibility = guideOpen ? Visibility.Visible : Visibility.Collapsed;
        var nordKey = w.NordvpnInstalled.Length > 0;
        nordInstalled.Text = w.NordvpnInstalled;
        nordReveal.Content = w.ReplaceRevealLabel;
        nordInstalledRow.Visibility = nordKey && !replacingNord ? Visibility.Visible : Visibility.Collapsed;
        nordFields.Visibility = !nordKey || replacingNord ? Visibility.Visible : Visibility.Collapsed;
        username.PlaceholderText = w.UsernamePlaceholder;
        password.PlaceholderText = w.PasswordPlaceholder;
        pasteHint.Text = w.PasteHint;
        Show(pastedNote, copiedUser is not null || copiedPassword is not null ? w.PastedNote : "");
        nordGo.Content = nordKey ? w.ReplaceLabel : w.EnableNordvpnLabel;
        nordGo.IsEnabled = !busy && username.Text.Trim().Length > 0 && password.Password.Length > 0;
        nordCancel.Content = w.ReplaceCancelLabel;
        nordCancel.Visibility = nordKey ? Visibility.Visible : Visibility.Collapsed;
        nordReveal.IsEnabled = !busy;

        customIntro.Text = w.CustomIntro;
        var customKey = w.CustomInstalled.Length > 0;
        customInstalled.Text = w.CustomInstalled;
        customReveal.Content = w.ReplaceRevealLabel;
        customInstalledRow.Visibility = customKey && !replacingCustom ? Visibility.Visible : Visibility.Collapsed;
        customFields.Visibility = !customKey || replacingCustom ? Visibility.Visible : Visibility.Collapsed;
        host.PlaceholderText = w.HostLabel;
        port.PlaceholderText = w.PortLabel;
        customUser.PlaceholderText = w.CustomUserLabel;
        customPassword.PlaceholderText = w.CustomPasswordLabel;
        if (!hostFilled)
        {
            hostFilled = true;
            host.Text = w.CustomHost;
            port.Text = w.CustomPort;
        }
        customGo.Content = customKey ? w.ReplaceLabel : w.UseCustomLabel;
        customGo.IsEnabled = !busy && host.Text.Trim().Length > 0;
        customCancel.Content = w.ReplaceCancelLabel;
        customCancel.Visibility = customKey ? Visibility.Visible : Visibility.Collapsed;
        customReveal.IsEnabled = !busy;

        countryLabel.Text = w.CountryLabel;
        countryHint.Text = w.CountryHint;
        FollowCountries(w);
        findFaster.Content = w.FindFasterLabel;
        findFaster.Visibility = w.FindFasterLabel.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        findFaster.IsEnabled = !busy;

        progress.Text = w.Progress;
        ring.IsActive = w.Progress.Length > 0;
        progressRow.Visibility = w.Progress.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        var said = w.Result.Length > 0 ? w.Result : answer;
        var bad = w.Result.Length > 0 ? w.ResultIsProblem : answerIsProblem;
        result.Text = said;
        result.Foreground = Res(bad ? "SystemFillColorCriticalBrush" : "TextFillColorPrimaryBrush");
        resultGlyph.Glyph = bad ? "" : "";
        resultGlyph.Foreground = Res(bad ? "SystemFillColorCriticalBrush" : "SystemFillColorSuccessBrush");
        resultRow.Visibility = said.Length > 0 && w.Progress.Length == 0 ? Visibility.Visible : Visibility.Collapsed;

        dialog.CloseButtonText = w.CloseLabel;
        dialog.SecondaryButtonText = w.HasKey ? w.RemoveKeyLabel : "";
        dialog.IsSecondaryButtonEnabled = !busy;
    }

    static void Show(TextBlock block, string text)
    {
        block.Text = text;
        block.Visibility = text.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    static void Tab(Button tab, string text, bool chosen)
    {
        tab.Content = text;
        tab.Style = chosen ? (Style)Application.Current.Resources["AccentButtonStyle"] : (Style)Application.Current.Resources["DefaultButtonStyle"];
    }

    void FollowGuide(ProxyWindowModel w)
    {
        if (shownSteps.SequenceEqual(w.GuideSteps))
        {
            return;
        }
        shownSteps = w.GuideSteps;
        FocusKeeper.Redraw(guide, guide.XamlRoot, () => DrawGuide(w));
    }

    void DrawGuide(ProxyWindowModel w)
    {
        guide.Children.Clear();
        int n = 1;
        foreach (var step in w.GuideSteps)
        {
            guide.Children.Add(new TextBlock { Text = $"{n++}. {step}", FontSize = 12, TextWrapping = TextWrapping.Wrap });
        }
        if (Uri.TryCreate(w.NordAccountUrl, UriKind.Absolute, out var url) && w.NordAccountLabel.Length > 0)
        {
            guide.Children.Add(new HyperlinkButton { Content = w.NordAccountLabel, NavigateUri = url, Padding = new Thickness(0) });
        }
    }

    void FollowCountries(ProxyWindowModel w)
    {
        countryName.Text = w.CountryName;
    }

    /// <summary>A call of the window: others off and the core's progress shown meanwhile; its answer, or its failure in the core's words.</summary>
    async Task Call(Func<Task<string>> work, Action? succeeded = null)
    {
        if (ui.Busy)
        {
            return;
        }
        ui.SetBusy(true);
        answer = "";
        answerIsProblem = false;
        try
        {
            answer = await work();
            succeeded?.Invoke();
        }
        catch (Exception e)
        {
            answer = CoreCalls.Message(e);
            answerIsProblem = true;
        }
        ui.SetBusy(false);
        ui.Poll();
        Follow();
    }

    internal Task EnableNordVpn()
    {
        var (user, pass) = (username.Text.Trim(), password.Password);
        // (Whatever it answers: the password never stays in the field.)
        password.Password = "";
        copiedPassword = null;
        return Call(() => app.Core.ProxySetNordvpnKey(user, pass), () =>
        {
            replacingNord = false;
            EmptyNord();
        });
    }

    internal Task UseCustom()
    {
        var (h, p, user, pass) = (host.Text.Trim(), ushort.TryParse(port.Text.Trim(), out var n) ? n : (ushort)0, customUser.Text.Trim(), customPassword.Password);
        customPassword.Password = "";
        return Call(() => app.Core.ProxySetCustom(h, p, user.Length > 0 ? user : null, pass.Length > 0 ? pass : null), () =>
        {
            replacingCustom = false;
            EmptyCustom();
        });
    }

    internal Task FindFaster() => Call(() => app.Core.ProxyFindFaster());

    internal Task RemoveKey() => Call(async () =>
    {
        await app.Core.ProxyRemoveKey();
        return "";
    });

    // --------------------------------------------------------------------------------------------------- QA

    internal void QaFill(string user, string pass) { EmptyNord(); username.Text = user; password.Password = pass; Follow(); }
    internal void QaFillCustom(string h, string p, string user, string pass) { host.Text = h; port.Text = p; customUser.Text = user; customPassword.Password = pass; Follow(); }
    internal void QaPane(bool custom) => SetPane(custom);
    internal void QaReplace(bool on) { if (showCustom) { replacingCustom = on; if (!on) EmptyCustom(); } else { replacingNord = on; if (!on) EmptyNord(); } Follow(); }
    internal void QaGuide(bool open) { guideOpen = open; Follow(); }
    internal void QaCopied(string text) => Copied(text);
    internal void QaClose() { dialog.Hide(); if (Open == this) Open = null; }
    internal void QaHover(bool on) => shield.Hover(on);
    internal void QaOpenCountries() => ui.ShowCountries(country);

    /// <summary>The window as drawn; a password only as whether it holds one.</summary>
    /// <summary>QA: each control walked (<see cref="FocusKeeper.QaWalk"/>), each redraw the core's guide changing.</summary>
    internal Task<string> QaWalk() => FocusKeeper.QaWalk(content, content.XamlRoot, () => { shownSteps = []; Follow(); });

    internal string QaDescribe()
    {
        static string V(FrameworkElement e, string name) => e.Visibility == Visibility.Visible ? name : "";
        var shown = string.Join(",", (showCustom
            ? new[] { V(customPane, "custom"), V(customInstalledRow, "custom-installed"), V(customFields, "custom-fields") }
            : [V(nordPane, "nord"), V(nordInstalledRow, "nord-installed"), V(nordFields, "nord-fields"), V(guide, "guide"), V(pastedNote, "pasted")])
            .Concat([V(findFaster, "faster"), V(toggle, "switch"), V(resultRow, "result"), V(progressRow, "progress")]).Where(s => s.Length > 0));
        return $"window \"{title.Text}\", status \"{status.Text}\", shows [{shown}], user filled {username.Text.Length > 0}, password filled {password.Password.Length > 0}, enable on {nordGo.IsEnabled}, switch on {toggle.IsOn} enabled {toggle.IsEnabled}, country \"{countryName.Text}\" enabled {country.IsEnabled}, result \"{result.Text}\", shield {shield.QaDescribe()}, remove \"{dialog.SecondaryButtonText}\"";
    }

}

/// <summary>
/// The contact sheet as the Mac draws it (docs/proxy-shield.md, Renders): the same pixel layout, a row per state, 16, 24
/// and 32 at 8 times their size on the light and the dark window, so qa\run.ps1 proxy compares it cell by cell.
/// </summary>
static class ProxySheet
{
    internal static async Task<string> Save(XamlRoot root, string path)
    {
        var sheet = new Canvas { Width = 1806, Height = 1730, Background = new SolidColorBrush(Windows.UI.Color.FromArgb(255, 0x70, 0x70, 0x70)) };
        ProxyIcon[] states = [ProxyIcon.NoKey, ProxyIcon.Off, ProxyIcon.On, ProxyIcon.Slow, ProxyIcon.Failed];
        (double X, double Y, double Side, double Size)[] cells = [(150, 134, 192, 16), (352, 102, 256, 24), (618, 70, 320, 32)];
        for (int row = 0; row < states.Length; row++)
        {
            foreach (var dark in new[] { false, true })
            {
                foreach (var c in cells)
                {
                    var shield = new ProxyShield(c.Size * 8) { Icon = states[row], HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
                    var cell = new Grid
                    {
                        Width = c.Side,
                        Height = c.Side,
                        RequestedTheme = dark ? ElementTheme.Dark : ElementTheme.Light,
                        Background = new SolidColorBrush(dark ? Windows.UI.Color.FromArgb(255, 0x1E, 0x1E, 0x1E) : Windows.UI.Color.FromArgb(255, 255, 255, 255)),
                    };
                    cell.Children.Add(shield);
                    Canvas.SetLeft(cell, c.X + (dark ? 828 : 0));
                    Canvas.SetTop(cell, c.Y + 330 * row);
                    sheet.Children.Add(cell);
                }
            }
        }
        var host = new Microsoft.UI.Xaml.Controls.Primitives.Popup { XamlRoot = root, Child = sheet, IsOpen = true };
        await Task.Delay(300);
        await SnapshotRenderer.SaveImage(sheet, path, true);
        host.IsOpen = false;
        return File.Exists(path) ? "sheet saved" : "sheet not saved";
    }
}
