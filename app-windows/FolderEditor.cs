using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// A favorite tracker's own folders (SPEC 7A.4, 7E.6; the Mac's TrackerLocationEditor), shared by its Info page and
/// Settings › Rules › Trackers: the core's rows, words and warnings (<c>tracker_folders_view</c>), each change made by
/// the core (<c>tracker_folders_set</c>) and handed to <paramref name="changed"/> as the edited profile.
/// </summary>
static class FolderEditor
{
    static Brush Res(string key) => Theme.Res(key);

    /// <param name="typesOpen">Whether "By content type" is open: null the first time (the core's <c>types_open</c>), then the caller's.</param>
    internal static UIElement Draw(App app, ProfileRecord record, VolumeWarning[] warnings, Action<ProfileRecord> changed, bool? typesOpen, Action<bool> typesToggled)
    {
        var s = new StackPanel { Spacing = 7 };
        var v = SeedwellFfiMethods.TrackerFoldersView(record, warnings);
        foreach (var r in v.Rows) s.Children.Add(Row(app, record, r, changed));
        if (v.Warning is string w) s.Children.Add(new TextBlock { Text = w, FontSize = Theme.Caption, Foreground = Res("SystemFillColorCautionBrush"), TextWrapping = TextWrapping.Wrap });
        s.Children.Add(new TextBlock { Text = v.Note, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush"), TextWrapping = TextWrapping.Wrap });
        var types = new StackPanel { Spacing = 6, Margin = new Thickness(0, 4, 0, 0) };
        foreach (var t in v.Types)
        {
            var head = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
            head.Children.Add(new TextBlock { Text = t.Title, FontSize = Theme.Secondary, FontWeight = FontWeights.SemiBold });
            if (t.Custom is string c) head.Children.Add(new TextBlock { Text = c, FontSize = Theme.Caption, Foreground = Res("AccentTextFillColorPrimaryBrush"), VerticalAlignment = VerticalAlignment.Center });
            if (t.Warning is string tw)
            {
                var warn = new FontIcon { Glyph = "", FontSize = 12, Foreground = Res("SystemFillColorCautionBrush") };
                ToolTipService.SetToolTip(warn, tw);
                head.Children.Add(warn);
            }
            types.Children.Add(head);
            foreach (var r in t.Rows) types.Children.Add(Row(app, record, r, changed));
        }
        var expander = new Expander
        {
            Header = new TextBlock { Text = v.TypesTitle, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush") },
            Content = types,
            IsExpanded = typesOpen ?? v.TypesOpen,
            HorizontalAlignment = HorizontalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Left,
        };
        expander.Expanding += (_, _) => typesToggled(true);
        expander.Collapsed += (_, _) => typesToggled(false);
        s.Children.Add(expander);
        return s;
    }

    /// <summary>A folder: its label, the path (empty: the placeholder, the default), Choose… and its clear button.</summary>
    static FrameworkElement Row(App app, ProfileRecord record, TrackerFolderRow r, Action<ProfileRecord> changed)
    {
        void Set(string? path) => changed(SeedwellFfiMethods.TrackerFoldersSet(record, r.Category, r.Field, path));
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        var box = new TextBox { Text = r.Path ?? "", PlaceholderText = r.Placeholder, Width = 240, MinHeight = 0 };
        ToolTipService.SetToolTip(box, r.Help);
        void Commit() { var p = box.Text.Trim(); if (p != (r.Path ?? "")) Set(p.Length == 0 ? null : p); }
        box.KeyDown += (_, e) => { if (e.Key == Windows.System.VirtualKey.Enter) { e.Handled = true; Commit(); } };
        box.LostFocus += (_, _) => Commit();
        line.Children.Add(box);
        var choose = new Button { Content = r.ChooseTitle };
        ToolTipService.SetToolTip(choose, r.ChooseHelp);
        choose.Click += async (_, _) => { if (await app.PickFolder() is string picked) Set(picked); };
        line.Children.Add(choose);
        if (r.Path is not null)
        {
            var clear = new Button { Content = new FontIcon { Glyph = "", FontSize = 10 }, Padding = new Thickness(6), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
            ToolTipService.SetToolTip(clear, r.ClearHelp);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(clear, r.ClearHelp);
            clear.Click += (_, _) => Set(null);
            line.Children.Add(clear);
        }
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(130) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new TextBlock { Text = r.Label, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap });
        Grid.SetColumn(line, 1);
        g.Children.Add(line);
        return g;
    }
}
