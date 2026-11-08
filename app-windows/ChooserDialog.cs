using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// Choose what to download (SPEC 7A.6; the Mac's ChooserView), over the library window, drawn from the core's
/// <c>ChooserPage</c>: the header, the category card with its Change menu, the rule card, the notice, the quick picks,
/// the list (groups with tri-state boxes, folds, "In library" badges, sizes), the totals and Cancel and Start. Nothing
/// downloads before Start. Every tick goes to the shared chooser (<see cref="ChoiceStore"/>), so the notifier and the
/// list under a held row show the same ticks; the dialog closes when the torrent is resolved anywhere.
/// </summary>
sealed class ChooserDialog
{
    /// <summary>The one open (one at a time), for QA.</summary>
    internal static ChooserDialog? Open { get; private set; }

    readonly App app;
    readonly string id;
    readonly ContentDialog dialog;
    readonly ContentControl body = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch };

    internal string TorrentId => id;

    ChooserDialog(App app, string id, XamlRoot root)
    {
        this.app = app;
        this.id = id;
        dialog = new ContentDialog { XamlRoot = root, Content = body };
        // The page is 640 wide (the Mac's); the dialog's own limit is narrower.
        dialog.Resources["ContentDialogMaxWidth"] = 720.0;
        dialog.Resources["ContentDialogMaxHeight"] = 900.0;
        app.Choices.Changed += Redraw;
        app.Choices.Resolved += Closed;
        dialog.Closed += (_, _) =>
        {
            app.Choices.Changed -= Redraw;
            app.Choices.Resolved -= Closed;
            if (Open == this) Open = null;
            AppLog.Write($"chooser closed: {Short(id)}");
        };
        // Escape closes the dialog as Cancel would (the core's Cancel is its button, with the question it may ask).
        dialog.Closing += (_, e) =>
        {
            if (e.Result == ContentDialogResult.None && !closing) e.Cancel = !Dismiss();
        };
    }

    bool closing;

    /// <summary>
    /// Opens the chooser of <paramref name="id"/> (a held torrent's, or a running one's Choose… with
    /// <paramref name="reopen"/>), loaded off the UI thread when no surface has it yet.
    /// </summary>
    internal static async void Show(App app, string id, bool reopen = false)
    {
        if (Open?.id == id) return;
        Open?.Close();
        // A torrent that does not wait for a choice (a running one's Choose…) opens on what it downloads now.
        var awaiting = app.TakeSnapshot().Downloading.Any(r => r.Id == id && r.AwaitingChoice);
        // Whether Cancel would move a .torrent file (looked at off the UI thread), then the chooser itself.
        try { await Task.Run(() => app.Choices.Choices.HasSourceFile(id)); } catch { }
        if (await app.Choices.Load(id, reopen || !awaiting) is null)
        {
            // Nothing to choose (the core cannot say): the window on the torrent instead.
            AppLog.Write($"chooser of {Short(id)}: nothing to choose");
            app.ShowTransfer(id);
            return;
        }
        app.ShowMainWindow();
        if (app.LibraryWindow.Content?.XamlRoot is not XamlRoot root) return;
        var d = new ChooserDialog(app, id, root);
        Open = d;
        d.Redraw();
        AppLog.Write($"chooser shown: {Short(id)}");
        if (Quiet.On)
        {
            // A QA run: shown in the window's popup layer as a click shows it, never waited on.
            _ = d.dialog.ShowAsync();
            return;
        }
        try { await d.dialog.ShowAsync(); }
        catch (Exception e) { AppLog.Write($"chooser not shown: {e.Message}"); }
    }

    /// <summary>The page drawn again (any surface changed the chooser).</summary>
    void Redraw()
    {
        if (app.Choices.Page(id) is not ChooserPage page)
        {
            Close();
            return;
        }
        FocusKeeper.Redraw(body, dialog.XamlRoot, () => body.Content = Page(page));
    }

    void Closed(string resolved)
    {
        if (resolved == id) Close();
    }

    internal string QaFocused => FocusKeeper.Describe(body, dialog.XamlRoot);
    internal Task<string> QaWalk() => FocusKeeper.QaWalk(body, dialog.XamlRoot, Redraw);
    internal bool QaFocusFirstButton() => FocusKeeper.QaFocusFirstButton(body);

    internal void Close()
    {
        closing = true;
        dialog.Hide();
    }

    /// <summary>Escape: a running torrent's chooser closes without a change; a held one's stays (Cancel removes the torrent).</summary>
    bool Dismiss()
    {
        if (app.Choices.Page(id) is { Awaiting: true }) return false;
        closing = true;
        return true;
    }

    static Brush Res(string key) => Theme.Res(key);

    UIElement Page(ChooserPage page)
    {
        var s = new StackPanel { Width = 640, Spacing = 0 };
        // The header.
        s.Children.Add(new TextBlock { Text = page.Origin.ToUpperInvariant(), FontSize = Theme.Caption, FontWeight = FontWeights.SemiBold, CharacterSpacing = 40, Foreground = Res("TextFillColorSecondaryBrush") });
        s.Children.Add(new TextBlock { Text = page.Name, FontSize = Theme.Title, FontWeight = FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap, MaxLines = 2, Margin = new Thickness(0, 2, 0, 0) });
        s.Children.Add(new TextBlock { Text = page.Summary, FontSize = Theme.Body, Foreground = Res("TextFillColorSecondaryBrush"), Margin = new Thickness(0, 2, 0, 0) });
        s.Children.Add(CategoryCard(page));
        if (page.RuleLines.Length > 0) s.Children.Add(RuleCard(page));
        if (page.Notice is string notice)
        {
            var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 12, 0, 0) };
            line.Children.Add(new FontIcon { Glyph = page.Unsure ? "" : "", FontSize = 14, Foreground = Res(page.Unsure ? "SystemFillColorCautionBrush" : "SystemFillColorSuccessBrush"), VerticalAlignment = VerticalAlignment.Top });
            line.Children.Add(new TextBlock { Text = notice, FontSize = Theme.Body, TextWrapping = TextWrapping.Wrap, MaxWidth = 600 });
            s.Children.Add(line);
        }
        if (page.Picks.Length > 0) s.Children.Add(Picks(page));
        s.Children.Add(List(page));
        s.Children.Add(Footer(page));
        return s;
    }

    /// <summary>The category: its label and confidence, tinted (the warning colour when the core is unsure), and Change ▸.</summary>
    UIElement CategoryCard(ChooserPage page)
    {
        var tint = page.Unsure ? Theme.Color("SystemFillColorCautionBrush") : Theme.Accent;
        var g = new Grid { ColumnSpacing = 12, Padding = new Thickness(10), Margin = new Thickness(0, 14, 0, 0), CornerRadius = new CornerRadius(12), Background = Theme.Brush(tint, 0.14), BorderBrush = Theme.Brush(tint, 0.35), BorderThickness = new Thickness(page.Unsure ? 1 : 0.5) };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var text = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        text.Children.Add(new TextBlock { Text = page.CategoryTitle, FontSize = Theme.Heading, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
        text.Children.Add(new TextBlock { Text = page.CategoryDetail, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush") });
        g.Children.Add(text);
        var change = Choosing.CategoryMenu(page.ChangeTitle, page.ChangeHelp, page.Categories, c => app.Choices.ChangeCategory(id, c), pill: false);
        change.VerticalAlignment = VerticalAlignment.Center;
        Grid.SetColumn(change, 1);
        g.Children.Add(change);
        return g;
    }

    /// <summary>Which rule applies, where the files go, what happens after.</summary>
    static UIElement RuleCard(ChooserPage page)
    {
        var s = new StackPanel { Spacing = 3, Padding = new Thickness(14, 10, 14, 10), Margin = new Thickness(0, 8, 0, 0), CornerRadius = new CornerRadius(12), Background = Theme.Faint(0.05), BorderBrush = Theme.Faint(0.08), BorderThickness = new Thickness(0.5) };
        foreach (var line in page.RuleLines)
        {
            var t = new TextBlock { FontSize = Theme.Body, TextTrimming = TextTrimming.CharacterEllipsis, TextWrapping = line.Emphasized ? TextWrapping.Wrap : TextWrapping.NoWrap };
            t.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = line.Label, Foreground = Res("TextFillColorSecondaryBrush") });
            var value = new Microsoft.UI.Xaml.Documents.Run { Text = line.Value };
            if (line.Emphasized) value.FontWeight = FontWeights.SemiBold;
            t.Inlines.Add(value);
            s.Children.Add(t);
        }
        return s;
    }

    /// <summary>The quick picks as a segmented control: the chosen one filled.</summary>
    UIElement Picks(ChooserPage page)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, Padding = new Thickness(2), Margin = new Thickness(0, 12, 0, 0), CornerRadius = new CornerRadius(9), Background = Theme.Faint(0.06), HorizontalAlignment = HorizontalAlignment.Left };
        foreach (var p in page.Picks)
        {
            var b = new Button
            {
                Content = new TextBlock { Text = p.Title, FontSize = Theme.Body, Foreground = Res(p.Selected ? "TextFillColorPrimaryBrush" : "TextFillColorSecondaryBrush") },
                Padding = new Thickness(12, 5, 12, 5),
                CornerRadius = new CornerRadius(7),
                BorderThickness = new Thickness(0),
                Background = p.Selected ? Theme.Faint(0.16) : new SolidColorBrush(Microsoft.UI.Colors.Transparent),
            };
            ToolTipService.SetToolTip(b, p.Help);
            var pick = p.Pick;
            b.Click += (_, _) => app.Choices.Edit(id, c => c.ApplyPick(pick));
            row.Children.Add(b);
        }
        return row;
    }

    /// <summary>The rows, a thin line above each group, scrolling past about 12 rows.</summary>
    UIElement List(ChooserPage page)
    {
        var rows = new StackPanel();
        for (int n = 0; n < page.Rows.Length; n++)
        {
            var r = page.Rows[n];
            if (n > 0 && r.Level == 0) rows.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Fill = Res("DividerStrokeColorDefaultBrush"), Opacity = 0.5 });
            rows.Children.Add(Choosing.Row(id, r, app.Choices, false, () => { }));
        }
        return new Border
        {
            Margin = new Thickness(0, 10, 0, 0),
            CornerRadius = new CornerRadius(Theme.CodeRadius),
            BorderBrush = Theme.Faint(0.10),
            BorderThickness = new Thickness(1),
            Child = new ScrollViewer { Content = rows, MaxHeight = 12 * 30, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
        };
    }

    /// <summary>The totals (the warning colour and "Not enough space" when it does not fit), then Cancel and Start.</summary>
    UIElement Footer(ChooserPage page)
    {
        var g = new Grid { Margin = new Thickness(0, 12, 0, 0), ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var totals = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Top, Margin = new Thickness(0, 6, 0, 0) };
        var ink = page.NotEnoughSpace ? Res("SystemFillColorCautionBrush") : Res("TextFillColorSecondaryBrush");
        totals.Children.Add(new TextBlock { Text = page.TotalsLine, FontSize = Theme.Secondary, Foreground = ink, TextWrapping = TextWrapping.Wrap });
        if (page.NotEnoughSpace) totals.Children.Add(new TextBlock { Text = page.SpaceWarning, FontSize = Theme.Secondary, FontWeight = FontWeights.SemiBold, Foreground = ink });
        g.Children.Add(totals);
        var buttons = Choosing.HeldButtons(page.Buttons, keep =>
        {
            // A running torrent's Cancel closes without a change; a held one's removes the torrent.
            if (page.Awaiting) app.Choices.Cancel(id, keep); else Close();
        }, () => app.Choices.Confirm(id));
        Grid.SetColumn(buttons, 1);
        g.Children.Add(buttons);
        return g;
    }

    static string Short(string id) => id[..Math.Min(8, id.Length)];
}
