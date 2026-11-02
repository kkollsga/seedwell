using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The settings banner (the core's <c>settings_banner</c>, trs-ffi notify.rs): what a save or a rejected file said, the
/// values the core replaced (<c>Core::settings_problems</c>) and the settings that apply after a restart, problems
/// first. Outside Settings it has Open Settings (only with problems); in Settings it is drawn without it.
/// </summary>
sealed class SettingsBannerView
{
    readonly List<string> problems = [];
    string[] notes = [];
    string[] restart = [];

    /// <summary>Changed: the views that draw it ask again.</summary>
    internal event Action? Changed;

    /// <summary>A save the core refused, or a file it rejected (on_settings_rejected).</summary>
    internal void Problem(string text)
    {
        if (!problems.Contains(text)) problems.Add(text);
        Changed?.Invoke();
    }

    /// <summary>A save went through: what the last ones said is no longer so.</summary>
    internal void Saved()
    {
        problems.Clear();
        Changed?.Invoke();
    }

    /// <summary>The core's notes on the settings (read at start and after each change).</summary>
    internal void Notes(string[] current)
    {
        notes = current;
        Changed?.Invoke();
    }

    /// <summary>on_settings_changed's restart list: the last non-empty one is kept.</summary>
    internal void RestartRequired(string[] list)
    {
        if (list.Length == 0) return;
        restart = list;
        Changed?.Invoke();
    }

    internal SettingsBanner? Model(bool inSettings) => TrsFfiMethods.SettingsBanner(problems.ToArray(), notes, restart, inSettings);

    /// <summary>What the Settings page's model is built from (SettingsLive's problems, notes and restart list).</summary>
    internal string[] Problems => problems.ToArray();
    internal string[] Notes_ => notes;
    internal string[] Restart => restart;

    /// <summary>The banner, or null when there is nothing to say. <paramref name="open"/>: Open Settings.</summary>
    internal UIElement? Draw(bool inSettings, Action open) => Model(inSettings) is SettingsBanner b ? Draw(b, open) : null;

    /// <summary>A banner the core gave (here, or in the Settings page's model).</summary>
    internal static UIElement? Draw(SettingsBanner b, Action open)
    {
        if (b.Lines.Length == 0)
        {
            return null;
        }
        var res = Application.Current.Resources;
        var g = new Grid { ColumnSpacing = 10 };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var lines = new StackPanel { Spacing = 4 };
        foreach (var l in b.Lines)
        {
            var problem = l.Tone == SettingsBannerTone.Problem;
            // The icon, then the text wrapping in the rest of the width.
            var row = new Grid { ColumnSpacing = 6 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.Children.Add(new FontIcon
            {
                Glyph = problem ? "" : "",
                FontSize = 12,
                VerticalAlignment = VerticalAlignment.Top,
                Margin = new Thickness(0, 3, 0, 0),
                Foreground = (Brush)res[problem ? "SystemFillColorCriticalBrush" : "SystemFillColorCautionBrush"],
            });
            var text = new TextBlock { Text = l.Text, TextWrapping = TextWrapping.Wrap };
            Grid.SetColumn(text, 1);
            row.Children.Add(text);
            lines.Children.Add(row);
        }
        g.Children.Add(lines);
        if (b.Action is SettingsBannerAction a)
        {
            var button = new Button { Content = a.Title, VerticalAlignment = VerticalAlignment.Center };
            if (a.Help.Length > 0) ToolTipService.SetToolTip(button, a.Help);
            button.Click += (_, _) => open();
            Grid.SetColumn(button, 1);
            g.Children.Add(button);
        }
        return new Border
        {
            Child = g,
            Padding = new Thickness(12, 8, 12, 8),
            Margin = new Thickness(20, 0, 16, 8),
            CornerRadius = new CornerRadius(8),
            Background = (Brush)res["SubtleFillColorSecondaryBrush"],
        };
    }

    /// <summary>QA: the banner's lines as the core gives them.</summary>
    internal string QaText(bool inSettings) => Model(inSettings) is SettingsBanner b
        ? $"{string.Join(" / ", b.Lines.Select(l => $"{l.Tone}: {l.Text}"))}{(b.Action is null ? "" : $" [{b.Action.Title}]")}"
        : "none";
}
