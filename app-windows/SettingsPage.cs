using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The library window's Settings page. For now its Updates group only, drawn from the core's <c>UpdatesModel</c>
/// (the Mac's Settings › General › Updates): the two switches with their hints (saved as settings), the status line,
/// Check Now, "Update to X", Release Notes and Show Log. Asked again after the model's refresh_ms and when the updater
/// says something changed. NEED: the group's title ("Updates") and the rest of Settings (stage D).
/// </summary>
sealed class SettingsPage
{
    readonly App app;
    readonly StackPanel panel = new() { Padding = new Thickness(28, 12, 28, 28), Spacing = 14, MaxWidth = 720, HorizontalAlignment = HorizontalAlignment.Left };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    string drawn = "";

    public UIElement Root { get; }

    public SettingsPage(App app)
    {
        this.app = app;
        Root = new ScrollViewer { Content = panel };
        timer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.IsRepeating = false;
        timer.Tick += (_, _) => Update();
        app.Updater.Changed += () => { drawn = ""; Update(); };
        Update();
    }

    public void Update()
    {
        timer.Stop();
        UpdatesModel m;
        AppSettings settings;
        try
        {
            m = app.Updater.Updates.Model(App.Now());
            settings = app.Core.SettingsLoad();
        }
        catch (Exception e)
        {
            AppLog.Write($"settings page failed: {e.Message}");
            return;
        }
        var key = $"{m}|{settings.Updates}";
        if (key != drawn)
        {
            drawn = key;
            Draw(m, settings);
        }
        timer.Interval = TimeSpan.FromMilliseconds(Math.Max(1000, m.RefreshMs));
        timer.Start();
    }

    void Draw(UpdatesModel m, AppSettings settings)
    {
        panel.Children.Clear();
        panel.Children.Add(Switch(m.CheckLabel, m.CheckHint, settings.Updates.Check, m.SwitchesEnabled,
            on => Save(s => s with { Updates = s.Updates with { Check = on } })));
        panel.Children.Add(Switch(m.InstallLabel, m.InstallHint, settings.Updates.AutoInstall, m.SwitchesEnabled && settings.Updates.Check,
            on => Save(s => s with { Updates = s.Updates with { AutoInstall = on } })));

        var status = new StackPanel { Spacing = 6 };
        status.Children.Add(new TextBlock { Text = m.StatusLabel });
        status.Children.Add(new TextBlock { Text = m.Status, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
        buttons.Children.Add(Button(m.CheckNow, () => app.Updater.Check(false)));
        if (m.Install is UpdateButton install)
        {
            buttons.Children.Add(Button(install, app.Updater.Install, accent: true));
        }
        if (m.NotesUrl is string notes)
        {
            buttons.Children.Add(new HyperlinkButton { Content = m.NotesTitle, NavigateUri = new Uri(notes) });
        }
        if (m.ShowLog is UpdateButton log)
        {
            buttons.Children.Add(Button(log, app.Updater.ShowLog));
        }
        status.Children.Add(buttons);
        panel.Children.Add(status);
    }

    static UIElement Switch(string label, string hint, bool on, bool enabled, Action<bool> set)
    {
        var s = new StackPanel { Spacing = 2 };
        var toggle = new ToggleSwitch { Header = label, IsOn = on, IsEnabled = enabled };
        toggle.Toggled += (_, _) => set(toggle.IsOn);
        s.Children.Add(toggle);
        s.Children.Add(new TextBlock { Text = hint, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap });
        return s;
    }

    static Button Button(UpdateButton b, Action run, bool accent = false)
    {
        var button = new Button { Content = b.Title, IsEnabled = b.Enabled };
        if (accent)
        {
            button.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
        }
        ToolTipService.SetToolTip(button, b.Help);
        button.Click += (_, _) => run();
        return button;
    }

    void Save(Func<AppSettings, AppSettings> change)
    {
        try
        {
            app.Core.SettingsSave(change(app.Core.SettingsLoad()));
        }
        catch (Exception e)
        {
            AppLog.Write($"settings save failed: {e.Message}");
        }
        app.Updater.Schedule();
        drawn = "";
        Update();
    }
}
