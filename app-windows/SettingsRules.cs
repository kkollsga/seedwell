using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// Settings › Rules (the Mac's RulesTab), drawn from the core's rules editor (<c>rules_editor_model</c>, seedwell-ffi
/// settings_rules.rs): Rules or Trackers, the list on the left (numbered rules that drag into another order, the Default
/// rule apart, trackers, + and −), the editor on the right (the same rows and controls as Settings, compact; their
/// changes go to <c>rules_editor_apply</c>, not to settings_apply), a tracker's folder editor (shared with its Info
/// page) and the footer (what saving would do, Revert and Save). The saves the core asks for run here; a refusal is
/// shown in its problems.
/// </summary>
sealed partial class SettingsPage
{
    RulesEditorState rulesState = SeedwellFfiMethods.RulesEditorState();
    readonly List<string> rulesProblems = [];
    ProfileRecord[]? profiles;
    RuleCount[]? ruleCounts;
    bool? trackerTypesOpen;
    // While the editor's groups are drawn: their controls send to the rules editor.
    bool drawingRules;

    /// <summary>What the editor is built from: the saved rules and trackers, the preview of the rule shown, the counts.</summary>
    RulesEditorInput RulesInput()
    {
        profiles ??= app.Core.ProfilesLoad();
        var rules = state.Rules;
        var q = SeedwellFfiMethods.RulesEditorPreviewQuery(rulesState, rules);
        RulePreview preview;
        try { preview = app.Core.RulePreview(q.Rules, q.RuleId); }
        catch (Exception e) { AppLog.Write($"rule preview failed: {e.Message}"); preview = new RulePreview(0, 0, ""); }
        // Every rule's count, and the Default rule's ("*"), again after a save.
        ruleCounts ??= rules.Rules.Select(r => new RuleCount(r.Id, Count(rules, r.Id))).Append(new RuleCount("*", Count(rules, "*"))).ToArray();
        return new RulesEditorInput(rulesState, rules, profiles, preview, ruleCounts, rulesProblems.ToArray());
    }

    uint Count(RulesDoc rules, string id)
    {
        try { return app.Core.RulePreview(rules, id).Affected; } catch { return 0; }
    }

    /// <summary>A change in the tab: the core's state, then the save it asks for (rules or trackers) and the problems.</summary>
    internal void RulesApply(RulesChange change, bool redraw = true)
    {
        RulesApplied r;
        try { r = SeedwellFfiMethods.RulesEditorApply(RulesInput(), change); }
        catch (Exception e) { AppLog.Write($"rules editor failed: {e.Message}"); return; }
        rulesState = r.State;
        AppLog.Write($"rules editor: {change.GetType().Name}{(r.Effect is null ? "" : $" -> {r.Effect.GetType().Name}")}");
        if (r.ClearProblems) rulesProblems.Clear();
        if (change is RulesChange.SelectTracker or RulesChange.NewTracker or RulesChange.EditTracker) trackerTypesOpen = null;
        switch (r.Effect)
        {
            case RulesEffect.SaveRules sr:
                CoreCalls.InOrder("rules save", () => app.Core.RulesSave(sr.Rules), saved =>
                {
                    state = state with { Rules = saved };
                    ruleCounts = null;
                    rulesProblems.Clear();
                    RulesApply(new RulesChange.RulesSaved(saved));
                }, e => { rulesProblems.Add(e); Draw(); });
                break;
            case RulesEffect.SaveProfiles sp:
                CoreCalls.InOrder("trackers save", () => app.Core.ProfilesSave(sp.Profiles), saved =>
                {
                    profiles = saved;
                    rulesProblems.Clear();
                    RulesApply(new RulesChange.ProfilesSaved(saved));
                    app.Refreshed();
                }, e => { rulesProblems.Add(e); Draw(); });
                break;
        }
        if (redraw) Draw();
    }

    /// <summary>"Edit Tracker…" from elsewhere in the app: Settings › Rules on that tracker.</summary>
    internal void EditTracker(string id)
    {
        Show(SettingsTab.Rules);
        RulesApply(new RulesChange.EditTracker(id));
    }

    /// <summary>The model drawn last (QA).</summary>
    internal RulesEditorModel? RulesModel { get; private set; }

    UIElement RulesEditor()
    {
        RulesEditorModel m;
        try { m = SeedwellFfiMethods.RulesEditorModel(RulesInput()); }
        catch (Exception e) { AppLog.Write($"rules editor model failed: {e.Message}"); return new TextBlock(); }
        RulesModel = m;
        var s = new StackPanel { Spacing = 10 };
        // Rules | Trackers.
        var sections = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, HorizontalAlignment = HorizontalAlignment.Center };
        foreach (var o in m.Sections)
        {
            var b = new Button { Content = o.Title, IsEnabled = o.Enabled, Padding = new Thickness(14, 4, 14, 5) };
            if (o.Id == m.Selected) b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
            var id = o.Id;
            b.Click += (_, _) => RulesApply(new RulesChange.Section(id == "trackers" ? RulesSection.Trackers : RulesSection.Rules));
            sections.Children.Add(b);
        }
        s.Children.Add(sections);
        var g = new Grid { ColumnSpacing = 18 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(280) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(RulesList(m));
        var editor = RulesEditorPane(m);
        Grid.SetColumn(editor, 1);
        g.Children.Add(editor);
        s.Children.Add(g);
        return s;
    }

    /// <summary>The list: its note, the rows (rules drag into another order), the Default rule apart, + and −.</summary>
    UIElement RulesList(RulesEditorModel m)
    {
        var s = new StackPanel { Spacing = 6 };
        if (m.Note.Length > 0) s.Children.Add(new TextBlock { Text = m.Note, FontSize = Theme.Caption, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap });
        var list = new ListView { SelectionMode = ListViewSelectionMode.None, CanReorderItems = m.Reorderable, AllowDrop = m.Reorderable, CanDragItems = m.Reorderable, IsItemClickEnabled = false };
        var ids = m.Rows.Select(r => r.Id).ToList();
        foreach (var r in m.Rows) list.Items.Add(new ListViewItem { Content = RulesRow(r), Tag = r.Id, Padding = new Thickness(0), MinHeight = 0, HorizontalContentAlignment = HorizontalAlignment.Stretch });
        // A rule dragged before another: the core moves it (to = the index it goes before).
        list.DragItemsCompleted += (_, e) =>
        {
            if (e.Items.FirstOrDefault() is not ListViewItem moved || moved.Tag is not string id) return;
            var from = ids.IndexOf(id);
            var now = list.Items.IndexOf(moved);
            if (from < 0 || now < 0 || from == now) return;
            RulesApply(new RulesChange.MoveRule((uint)from, (uint)(now > from ? now + 1 : now)));
        };
        if (m.Rows.Length > 0) s.Children.Add(new Border { Child = list, CornerRadius = new CornerRadius(8), Background = Res("CardBackgroundFillColorDefaultBrush"), BorderBrush = Res("CardStrokeColorDefaultBrush"), BorderThickness = new Thickness(1) });
        if (m.Empty is string empty) s.Children.Add(new TextBlock { Text = empty, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, Margin = new Thickness(4, 8, 4, 8) });
        if (m.DefaultRow is RulesListRow d)
        {
            s.Children.Add(new Border { Child = RulesRow(d), Margin = new Thickness(0, 6, 0, 0), CornerRadius = new CornerRadius(8), Background = Res("CardBackgroundFillColorDefaultBrush"), BorderBrush = Res("CardStrokeColorDefaultBrush"), BorderThickness = new Thickness(1) });
        }
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        buttons.Children.Add(SymbolButton("", m.ListButtons.Add));
        buttons.Children.Add(SymbolButton("", m.ListButtons.Remove));
        s.Children.Add(buttons);
        return s;
    }

    /// <summary>A row: its badge (a rule's number, a tracker image, a plus for the new tracker), title, subtitle and count.</summary>
    UIElement RulesRow(RulesListRow r)
    {
        var g = new Grid { ColumnSpacing = 10, Padding = new Thickness(10, 7, 10, 7), Opacity = r.Enabled ? 1 : 0.5, Background = r.Selected ? Theme.Brush(Theme.Accent, 0.18) : new SolidColorBrush(Microsoft.UI.Colors.Transparent), CornerRadius = new CornerRadius(6) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        UIElement badge = r.Badge is string number
            ? new Border { Width = 22, Height = 22, CornerRadius = new CornerRadius(11), Background = Theme.Faint(0.10), Child = new TextBlock { Text = number, FontSize = Theme.Caption, FontWeight = FontWeights.SemiBold, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center } }
            : new FontIcon { Glyph = r.New ? "" : "", FontSize = 14, Width = 22, Foreground = Res("TextFillColorSecondaryBrush") };
        g.Children.Add(badge);
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
        text.Children.Add(new TextBlock { Text = r.Title, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
        if (r.Subtitle.Length > 0) text.Children.Add(new TextBlock { Text = r.Subtitle, FontSize = Theme.Caption, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis });
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        if (r.Count is uint count)
        {
            var c = new TextBlock { Text = count.ToString(), FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(c, 2);
            g.Children.Add(c);
        }
        var select = r.Select;
        g.Tapped += (_, _) => Run(select, rules: true);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, r.Title);
        return g;
    }

    /// <summary>The editor: its groups (compact, their controls the editor's), the folder editor after its group, the footer.</summary>
    FrameworkElement RulesEditorPane(RulesEditorModel m)
    {
        var s = new StackPanel { Spacing = 14 };
        if (m.Placeholder is string placeholder)
        {
            s.Children.Add(new TextBlock { Text = placeholder, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(0, 40, 0, 0) });
            return s;
        }
        drawingRules = true;
        try
        {
            foreach (var group in m.Groups)
            {
                s.Children.Add(Group(group));
                if (group.Id == m.FoldersAfter && m.FoldersProfile is ProfileRecord profile)
                {
                    var f = new StackPanel { Spacing = 6 };
                    f.Children.Add(new TextBlock { Text = m.FoldersTitle.ToUpperInvariant(), FontSize = 11, FontWeight = FontWeights.SemiBold, CharacterSpacing = 40, Foreground = Res("TextFillColorSecondaryBrush"), Margin = new Thickness(4, 0, 0, 0) });
                    f.Children.Add(new Border
                    {
                        Padding = new Thickness(12),
                        CornerRadius = new CornerRadius(8),
                        Background = Res("CardBackgroundFillColorDefaultBrush"),
                        BorderBrush = Res("CardStrokeColorDefaultBrush"),
                        BorderThickness = new Thickness(1),
                        // Only its folders are taken from the profile sent back.
                        Child = FolderEditor.Draw(app, profile, [], edited => RulesApply(new RulesChange.TrackerFolders(edited)), trackerTypesOpen, open => trackerTypesOpen = open),
                    });
                    s.Children.Add(f);
                }
            }
            if (m.Footer is RulesFooter footer) s.Children.Add(RulesFooter(footer));
        }
        finally
        {
            drawingRules = false;
        }
        return s;
    }

    /// <summary>What saving would do (bold parts; red when it is a refusal), then Save (primary, first) and Revert.</summary>
    UIElement RulesFooter(RulesFooter f)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new FontIcon { Glyph = "", FontSize = 14, Foreground = Res(f.Problem ? "SystemFillColorCriticalBrush" : "TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(0, 2, 0, 0) });
        var text = new TextBlock { TextWrapping = TextWrapping.Wrap, FontSize = Theme.Secondary, Foreground = Res(f.Problem ? "SystemFillColorCriticalBrush" : "TextFillColorSecondaryBrush") };
        foreach (var r in f.Runs)
        {
            var run = new Microsoft.UI.Xaml.Documents.Run { Text = r.Text };
            if (r.Bold) run.FontWeight = FontWeights.SemiBold;
            text.Inlines.Add(run);
        }
        Grid.SetColumn(text, 1);
        g.Children.Add(text);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Top };
        var save = ButtonControl(f.Save);
        if (save is Button sb) sb.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
        buttons.Children.Add(save);
        buttons.Children.Add(ButtonControl(f.Revert));
        Grid.SetColumn(buttons, 2);
        g.Children.Add(buttons);
        return g;
    }

    /// <summary>QA: what a click in the tab does (a section, a row, + and −, Save and Revert, a drag, a new tracker for a host).</summary>
    internal string QaRulesCommand(string[] a)
    {
        if (RulesModel is not RulesEditorModel m) return "no rules editor";
        string Arg(int i) => a.Length > i ? a[i] : "";
        switch (Arg(0))
        {
            case "section": RulesApply(new RulesChange.Section(Arg(1) == "trackers" ? RulesSection.Trackers : RulesSection.Rules)); return "done";
            case "select":
                var title = string.Join(' ', a.Skip(1));
                var row = m.Rows.Append(m.DefaultRow).OfType<RulesListRow>().FirstOrDefault(r => r.Title.Contains(title, StringComparison.OrdinalIgnoreCase));
                if (row is null) return "no such row";
                Run(row.Select, rules: true);
                return "selected";
            case "add": Run(m.ListButtons.Add.Command, rules: true); return "added";
            case "remove": Run(m.ListButtons.Remove.Command, rules: true); return "removed";
            case "save" when m.Footer is RulesFooter f: Run(f.Save.Command, rules: true); return "saved";
            case "revert" when m.Footer is RulesFooter f: Run(f.Revert.Command, rules: true); return "reverted";
            case "move" when uint.TryParse(Arg(1), out var from) && uint.TryParse(Arg(2), out var to): RulesApply(new RulesChange.MoveRule(from, to)); return "moved";
            case "new-tracker": RulesApply(new RulesChange.NewTracker(Arg(1).Length > 0 ? Arg(1) : null)); return "new tracker";
            case "status" or "": return "status";
            default: return "unknown";
        }
    }

    /// <summary>QA: the editor as drawn (section, list, the selected row's title, footer and problems).</summary>
    internal string QaRules()
    {
        if (RulesModel is not RulesEditorModel m) return "no rules editor";
        var rows = string.Join(", ", m.Rows.Select(r => $"{r.Badge ?? (r.New ? "+" : "t")}:{r.Title}{(r.Selected ? "*" : "")}{(r.Count is uint c ? $"({c})" : "")}"));
        var footer = m.Footer is RulesFooter f ? string.Concat(f.Runs.Select(x => x.Text)) + (f.Problem ? " (problem)" : "") : "";
        return $"section {m.Selected}, rows [{rows}], default {(m.DefaultRow is RulesListRow d ? $"{d.Title}{(d.Selected ? "*" : "")}" : "none")}, groups [{string.Join(", ", m.Groups.Select(g => g.Id))}], folders {(m.FoldersProfile is null ? "none" : "shown")}, footer \"{footer}\", problems [{string.Join("; ", rulesProblems)}]";
    }
}
