using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// A list page of the library window, drawn from the model's <c>ListModel</c> (the Mac's ListViews and RowViews): the
/// category chip, the groups in order with their headers, folds, actions and "Show more", and the rows with their icon,
/// pill, coloured status, detail, progress, rate and controls. Built once per list signature; a row whose revision
/// changed is redrawn in place.
/// </summary>
sealed class LibraryList
{
    readonly App app;
    readonly Library library;
    readonly Action refresh;
    readonly StackPanel panel = new() { Padding = new Thickness(16, 0, 16, 16) };
    readonly Dictionary<string, (Border Element, ulong Revision)> rows = [];

    public UIElement Root { get; }

    public LibraryList(App app, Library library, Action refresh, ListModel list, PendingRemoval? removal)
    {
        this.app = app;
        this.library = library;
        this.refresh = refresh;
        Root = new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Build(list, removal);
    }

    /// <summary>A row's drawn element (to bring a new download into view), if it is in the list.</summary>
    public FrameworkElement? RowElement(string id) => rows.TryGetValue(id, out var drawn) ? drawn.Element : null;

    /// <summary>The same list (same signature): only the rows whose revision changed are redrawn.</summary>
    public void Update(ListModel list)
    {
        foreach (var g in list.Groups)
        {
            foreach (var r in g.Rows)
            {
                if (rows.TryGetValue(r.Id, out var drawn) && drawn.Revision != r.Revision)
                {
                    drawn.Element.Child = RowContent(g, r);
                    drawn.Element.Background = RowBackground(r);
                    rows[r.Id] = (drawn.Element, r.Revision);
                }
            }
        }
    }

    void Build(ListModel list, PendingRemoval? removal)
    {
        panel.Children.Clear();
        rows.Clear();
        if (list.FilterChip is FilterChip chip)
        {
            panel.Children.Add(Chip(chip));
        }
        foreach (var g in list.Groups)
        {
            if (g.Title is not null)
            {
                panel.Children.Add(Header(g));
            }
            if (g.Folded)
            {
                continue;
            }
            bool firstRow = true;
            foreach (var r in g.Rows)
            {
                if (!firstRow)
                {
                    // A thin line between a group's rows, from the text's edge to the right (the Mac's).
                    panel.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle
                    {
                        Height = 1,
                        Margin = new Thickness((g.Parent is null ? 0 : 16) + 76, 0, 8, 0),
                        Fill = (Brush)Application.Current.Resources["DividerStrokeColorDefaultBrush"],
                    });
                }
                firstRow = false;
                var border = new Border { CornerRadius = new CornerRadius(6), Background = RowBackground(r), Child = RowContent(g, r) };
                if (g.Parent is not null)
                {
                    border.Margin = new Thickness(16, 0, 0, 0);
                }
                Wire(border, g, r);
                rows[r.Id] = (border, r.Revision);
                panel.Children.Add(border);
                if (removal is { RowId: string rid, GroupKey: string gk } && rid == r.Id && gk == g.Key)
                {
                    panel.Children.Add(Question(removal.Question, library, refresh));
                }
            }
            if (g.More is GroupMore more)
            {
                var t = new HyperlinkButton { Content = more.Title, Margin = new Thickness(52, 2, 0, 6) };
                ToolTipService.SetToolTip(t, more.Help);
                t.Click += (_, _) => { library.ShowMore(more.Group); refresh(); };
                panel.Children.Add(t);
            }
        }
        if (list.Empty is string empty)
        {
            panel.Children.Add(new TextBlock { Text = empty, Opacity = 0.6, HorizontalAlignment = HorizontalAlignment.Center, Margin = new Thickness(0, 80, 0, 0), TextWrapping = TextWrapping.Wrap });
        }
    }

    UIElement Chip(FilterChip chip)
    {
        var b = new Button { Margin = new Thickness(36, 0, 0, 8), CornerRadius = new CornerRadius(12), Padding = new Thickness(10, 2, 8, 3) };
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        s.Children.Add(new TextBlock { Text = chip.Label, FontSize = 12 });
        s.Children.Add(new FontIcon { Glyph = "", FontSize = 9 });
        b.Content = s;
        ToolTipService.SetToolTip(b, chip.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, chip.Accessibility);
        b.Click += (_, _) => { library.ClearCategory(); refresh(); };
        return b;
    }

    UIElement Header(ListGroup g)
    {
        var grid = new Grid { Margin = new Thickness(g.Parent is null ? 20 : 36, g.Block ? 18 : 12, 4, 4) };
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var head = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        var title = new TextBlock
        {
            Text = g.Title,
            FontSize = g.Block ? 15 : 13,
            FontWeight = FontWeights.SemiBold,
            Foreground = Tone(g.Tone),
            Opacity = g.Block ? 1 : 0.85,
        };
        head.Children.Add(title);
        if (g.FoldHelp is string foldHelp)
        {
            // A group that folds: its title is the fold's button, with a chevron as the Mac's.
            head.Children.Add(new FontIcon { Glyph = g.Folded ? "" : "", FontSize = 9, Opacity = 0.7 });
            ToolTipService.SetToolTip(head, foldHelp);
            var key = g.Key;
            head.Tapped += (_, _) => { app.ToggleFold(key); refresh(); };
        }
        else if (g.Hint is string hint)
        {
            ToolTipService.SetToolTip(head, hint);
        }
        grid.Children.Add(head);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10 };
        Grid.SetColumn(right, 1);
        if (g.Count is string count)
        {
            right.Children.Add(new TextBlock { Text = count, FontSize = 12, Opacity = 0.6, VerticalAlignment = VerticalAlignment.Center });
        }
        if (g.Action is GroupAction action)
        {
            var b = new HyperlinkButton { Content = action.Title, Padding = new Thickness(4, 0, 4, 0), Foreground = Tone(TextTone.Secondary) };
            ToolTipService.SetToolTip(b, action.Help);
            b.Click += (_, _) =>
            {
                if (action.Command == ListCommand.ClearRecent)
                {
                    app.Core.ClearRecent();
                }
                refresh();
            };
            right.Children.Add(b);
        }
        grid.Children.Add(right);
        return grid;
    }

    static Brush Tone(TextTone tone) => (Brush)Application.Current.Resources[tone switch
    {
        TextTone.Attention => "SystemFillColorCautionBrush",
        TextTone.Problem => "SystemFillColorCriticalBrush",
        _ => "TextFillColorSecondaryBrush",
    }];

    static Brush RowBackground(ListRow r) => r.Selected
        ? (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"]
        : new SolidColorBrush(Colors.Transparent);

    /// <summary>
    /// The row's pointer behaviour: a click selects through the Library (Ctrl as the Mac's ⌘, Shift for a range); hover
    /// reveals its hover-only controls; right-click opens the command menu for the rows the Library names, its Remove
    /// asking under the row.
    /// </summary>
    void Wire(Border border, ListGroup g, ListRow r)
    {
        var id = r.Id;
        var key = g.Key;
        border.PointerEntered += (_, _) => SetHover(border, true);
        border.PointerExited += (_, _) => SetHover(border, false);
        border.Tapped += (_, e) =>
        {
            // A click on a control in the row (a pill, the ring, Play) is that control's.
            for (var d = e.OriginalSource as DependencyObject; d is not null && d != border; d = VisualTreeHelper.GetParent(d))
            {
                if (d is ButtonBase)
                {
                    return;
                }
            }
            library.ClickRow(key, id, Down(Windows.System.VirtualKey.Control), Down(Windows.System.VirtualKey.Shift));
            refresh();
        };
        border.RightTapped += (_, e) =>
        {
            var targets = library.MenuRows(id);
            if (targets.Length == 0)
            {
                return;
            }
            var menu = RowCommands.Menu(app.Core, targets, false, border.XamlRoot, (rows, deleteFiles) => { library.AskRemoval(rows, deleteFiles); refresh(); });
            menu.ShowAt(border, e.GetPosition(border));
            e.Handled = true;
            refresh();
        };
    }

    static bool Down(Windows.System.VirtualKey key) =>
        Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(key).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);

    /// <summary>
    /// The Remove… question as the Mac's strip (under the row, or in the title bar for several): the message beside its
    /// icon, Cancel (cancel_removal) and the red confirm button (confirm_removal: the Library removes).
    /// </summary>
    internal static UIElement Question(RemovalQuestion q, Library library, Action refresh)
    {
        var strip = new Grid
        {
            Padding = new Thickness(16, 10, 12, 10),
            Margin = new Thickness(36, 2, 0, 6),
            ColumnSpacing = 12,
            CornerRadius = new CornerRadius(6),
            Background = (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"],
        };
        strip.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        strip.ColumnDefinitions.Add(new ColumnDefinition());
        strip.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        strip.Children.Add(q.DeleteFiles
            ? new FontIcon { Glyph = "", FontSize = 16, VerticalAlignment = VerticalAlignment.Center }
            : MinusCircle());
        var text = new TextBlock { Text = q.Message, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(text, 1);
        strip.Children.Add(text);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        Grid.SetColumn(buttons, 2);
        // As the Mac's: two small buttons, the confirm one plain with red text (Esc and Enter reach them through key()).
        var cancel = new Button { Content = q.CancelTitle, MinWidth = 72, FontSize = 12, Padding = new Thickness(10, 3, 10, 4) };
        ToolTipService.SetToolTip(cancel, q.CancelHelp);
        cancel.Click += (_, _) => { library.CancelRemoval(); refresh(); };
        var confirm = new Button
        {
            Content = q.ConfirmTitle,
            MinWidth = 72,
            FontSize = 12,
            Padding = new Thickness(10, 3, 10, 4),
            Foreground = (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"],
        };
        ToolTipService.SetToolTip(confirm, q.ConfirmHelp);
        confirm.Click += (_, _) => { library.ConfirmRemoval(); refresh(); };
        buttons.Children.Add(cancel);
        buttons.Children.Add(confirm);
        strip.Children.Add(buttons);
        // For screen readers: the strip is announced when it appears (a live region named by its question), and each
        // button says what it does (its help, the Mac's tooltips and accessibility hints).
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(strip, q.Message);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetLiveSetting(strip, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Assertive);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(cancel, q.CancelHelp);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetHelpText(confirm, q.ConfirmHelp);
        strip.Loaded += (_, _) =>
        {
            if (Microsoft.UI.Xaml.Automation.Peers.FrameworkElementAutomationPeer.FromElement(strip) is { } peer)
            {
                peer.RaiseAutomationEvent(Microsoft.UI.Xaml.Automation.Peers.AutomationEvents.LiveRegionChanged);
            }
        };
        return strip;
    }

    /// <summary>A minus in a circle (the Mac's minus.circle), in the text colour.</summary>
    static UIElement MinusCircle()
    {
        var ink = (Brush)Application.Current.Resources["TextFillColorPrimaryBrush"];
        var g = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        g.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 15, Height = 15, StrokeThickness = 1.4, Stroke = ink });
        g.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Width = 7, Height = 1.6, RadiusX = 0.8, RadiusY = 0.8, Fill = ink });
        return g;
    }

    static void SetHover(Border border, bool on)
    {
        if (border.Child is Grid g && g.Tag is StackPanel controls)
        {
            foreach (var c in controls.Children.OfType<FrameworkElement>())
            {
                if (c.Tag is "hover")
                {
                    c.Opacity = on ? 1 : 0;
                }
            }
        }
    }

    /// <summary>
    /// A row: [disclosure] icon | title, pill, attention dot / status runs · detail / progress | rate | controls.
    /// </summary>
    UIElement RowContent(ListGroup g, ListRow r)
    {
        var grid = new Grid { Padding = new Thickness(4, 8, 8, 8), ColumnSpacing = 12, Opacity = r.Dimmed ? Look.Dimmed : 1 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(16) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(32) });
        grid.ColumnDefinitions.Add(new ColumnDefinition());
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        if (r.Disclosure is not null)
        {
            // NEED: the file list and the chooser under a row (the Mac's steps 2 and 3); drawn, not yet opened.
            grid.Children.Add(new FontIcon { Glyph = "", FontSize = 10, Opacity = 0.6, VerticalAlignment = VerticalAlignment.Center });
        }

        var icon = new Grid { Width = 32, Height = 32 };
        Grid.SetColumn(icon, 1);
        // Explorer's icon for the file type (a folder for a folder torrent); a glyph when Windows has none.
        icon.Children.Add(FileIcons.For(r.IconExt, app.LibraryWindow.Content?.XamlRoot?.RasterizationScale ?? 1) is ImageSource source
            ? new Image { Source = source, Width = 32, Height = 32 }
            : new FontIcon { Glyph = FileGlyph(r.IconExt), FontSize = 26, Opacity = 0.85 });
        if (r.Linked)
        {
            icon.Children.Add(new FontIcon { Glyph = "", FontSize = 11, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Bottom, Foreground = (Brush)Application.Current.Resources["AccentTextFillColorPrimaryBrush"] });
        }
        grid.Children.Add(icon);

        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center, Spacing = 2 };
        Grid.SetColumn(text, 2);
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var title = new TextBlock { Text = r.Title, FontSize = 14, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis };
        if (r.TitleHelp is string th)
        {
            ToolTipService.SetToolTip(title, th);
        }
        line.Children.Add(title);
        line.Children.Add(Pill(r.Pill));
        if (r.AttentionDot)
        {
            line.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 7, Height = 7, VerticalAlignment = VerticalAlignment.Center, Fill = (Brush)Application.Current.Resources["SystemFillColorCautionBrush"] });
        }
        text.Children.Add(line);
        var status = new TextBlock { FontSize = 12, TextTrimming = TextTrimming.CharacterEllipsis };
        foreach (var run in r.Status)
        {
            status.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = run.Text, Foreground = Tone(run.Tone) });
        }
        text.Children.Add(status);
        if (r.Detail is string detail)
        {
            // Its own line under the status (the Mac's).
            text.Children.Add(new TextBlock { Text = detail, FontSize = 12, Foreground = Tone(TextTone.Secondary), TextTrimming = TextTrimming.CharacterEllipsis });
        }
        if (r.Progress is double progress)
        {
            text.Children.Add(new ProgressBar { Value = progress * 100, Maximum = 100, Margin = new Thickness(0, 4, 0, 0) });
        }
        grid.Children.Add(text);

        if (r.Rate is RateSlot rate)
        {
            var t = new TextBlock
            {
                Text = rate.Text,
                FontSize = 13,
                FontWeight = FontWeights.SemiBold,
                VerticalAlignment = VerticalAlignment.Center,
                Foreground = rate.Active ? (Brush)Application.Current.Resources["TextFillColorPrimaryBrush"] : Tone(TextTone.Secondary),
            };
            ToolTipService.SetToolTip(t, rate.Help);
            Grid.SetColumn(t, 3);
            grid.Children.Add(t);
        }

        var controls = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(controls, 4);
        foreach (var c in r.Controls)
        {
            var e = Control(c, r);
            if (c.HoverOnly && !r.Selected)
            {
                // Hidden until the pointer is on the row; kept in its place or not, as the model says.
                e.Opacity = 0;
                e.Tag = "hover";
                if (!c.KeepsSpace)
                {
                    e.Tag = "hover";
                }
            }
            controls.Children.Add(e);
        }
        grid.Tag = controls;
        grid.Children.Add(controls);
        return grid;
    }

    UIElement Pill(RowPill pill)
    {
        // A capsule with the category's glyph before its label, secondary on a faint fill (the Mac's); the accent when
        // it is the list's filter.
        var text = (Windows.UI.Color)Application.Current.Resources["TextFillColorPrimary"];
        var content = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        content.Children.Add(new FontIcon { Glyph = CategoryGlyph(pill.Category), FontSize = 10 });
        content.Children.Add(new TextBlock { Text = pill.Label, FontSize = 11 });
        var b = new Button
        {
            Content = content,
            Padding = new Thickness(7, 1, 8, 2),
            MinHeight = 0,
            CornerRadius = new CornerRadius(9),
            VerticalAlignment = VerticalAlignment.Center,
            BorderThickness = new Thickness(0),
            Background = pill.Active
                ? (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"]
                : new SolidColorBrush(Windows.UI.Color.FromArgb(20, text.R, text.G, text.B)),
            Foreground = pill.Active
                ? (Brush)Application.Current.Resources["TextOnAccentFillColorPrimaryBrush"]
                : Tone(TextTone.Secondary),
        };
        ToolTipService.SetToolTip(b, pill.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, pill.Accessibility);
        var category = pill.Category;
        b.Click += (_, _) => { library.ToggleCategory(category); refresh(); };
        return b;
    }

    /// <summary>One of the row's controls, by kind; a click runs its command (or, for the ring, pauses or resumes).</summary>
    FrameworkElement Control(RowControl c, ListRow r)
    {
        if (c.Kind is RowControlKind.CountdownActions or RowControlKind.ChoiceActions)
        {
            return HeldButtons(c, r);
        }
        var b = new Button
        {
            Padding = new Thickness(4),
            MinWidth = 32,
            MinHeight = 32,
            Background = new SolidColorBrush(Colors.Transparent),
            BorderThickness = new Thickness(0),
        };
        if (c.Kind == RowControlKind.PauseRing)
        {
            var look = c.Ring ?? new RingLook(0, false, false);
            var ring = new Grid();
            ring.Children.Add(new Viewbox { Width = 28, Height = 28, Child = FlyoutWindow.Ring(look.Fraction, (Windows.UI.Color)Application.Current.Resources["SystemAccentColor"]) });
            ring.Children.Add(RingGlyph(look));
            b.Content = ring;
            var id = r.Id;
            b.Click += (_, _) =>
            {
                if (look.Paused) app.Core.Resume([id]); else app.Core.Pause([id]);
                refresh();
            };
        }
        else if (c.Title is string title)
        {
            b.Content = new TextBlock { Text = title, FontSize = 12 };
            b.Background = (Brush)Application.Current.Resources["ControlFillColorDefaultBrush"];
            b.Padding = new Thickness(10, 3, 10, 4);
        }
        else
        {
            b.Content = new FontIcon
            {
                Glyph = ControlGlyph(c.Kind),
                FontSize = 15,
                Foreground = c.Kind switch
                {
                    RowControlKind.Reveal or RowControlKind.GetInfo => (Brush)Application.Current.Resources["AccentTextFillColorPrimaryBrush"],
                    RowControlKind.ErrorBadge or RowControlKind.DeadBadge => (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"],
                    _ => (Brush)Application.Current.Resources["TextFillColorPrimaryBrush"],
                },
            };
        }
        ToolTipService.SetToolTip(b, c.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, c.Accessibility ?? c.Help);
        if (c.Command is RowCommand command)
        {
            var rows = new[] { r.Transfer };
            b.Click += (_, _) => { RowCommands.Run(app.Core, command, rows, b.XamlRoot); refresh(); };
        }
        return b;
    }

    /// <summary>
    /// Inside the pause ring, small and solid as the Mac's bold symbols: two bars (pause), a triangle (play), or the
    /// retry arrow; in the text colour.
    /// </summary>
    static UIElement RingGlyph(RingLook look)
    {
        var ink = (Brush)Application.Current.Resources["TextFillColorPrimaryBrush"];
        if (look.Retry)
        {
            return new FontIcon { Glyph = "", FontSize = 9, FontWeight = FontWeights.Bold, Foreground = ink };
        }
        if (look.Paused)
        {
            return new Microsoft.UI.Xaml.Shapes.Polygon { Points = { new(0, 0), new(7, 4.5), new(0, 9) }, Fill = ink, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(2, 0, 0, 0) };
        }
        var bars = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2.5, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
        for (int i = 0; i < 2; i++)
        {
            bars.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Width = 2.5, Height = 9, RadiusX = 0.8, RadiusY = 0.8, Fill = ink });
        }
        return bars;
    }

    /// <summary>
    /// A held row's Cancel and Start (the Mac's held buttons): small buttons with the core's words, Start in the accent
    /// with the control's help. A click runs the command as the row menu does.
    /// </summary>
    FrameworkElement HeldButtons(RowControl c, ListRow r)
    {
        var panel = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        var rows = new[] { r.Transfer };
        foreach (var item in RowCommands.Held(r.Transfer))
        {
            var start = item.Command is not RowCommand.CancelHeld;
            var b = new Button { Content = new TextBlock { Text = item.Title, FontSize = 12 }, IsEnabled = item.Enabled, Padding = new Thickness(10, 3, 10, 4) };
            if (start)
            {
                b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
                ToolTipService.SetToolTip(b, c.Help);
            }
            var command = item.Command;
            b.Click += (_, _) => { RowCommands.Run(app.Core, command, rows, b.XamlRoot); refresh(); };
            panel.Children.Add(b);
        }
        return panel;
    }

    static string ControlGlyph(RowControlKind kind) => kind switch
    {
        RowControlKind.AskAgain => "",
        RowControlKind.GetInfo => "",
        RowControlKind.PlayWhileDownloading or RowControlKind.Play or RowControlKind.ResumeSharing => "",
        RowControlKind.Reveal => "",
        RowControlKind.LowPower => "",
        RowControlKind.ErrorBadge => "",
        RowControlKind.DeadBadge => "",
        RowControlKind.Remove => "",
        _ => "",
    };

    static string CategoryGlyph(Category c) => c switch
    {
        Category.Movie => "",
        Category.Tv => "",
        Category.Music => "",
        Category.Audiobook => "",
        Category.Book => "",
        Category.Software => "",
        Category.Game => "",
        _ => "",
    };

    /// <summary>A glyph for the file's kind, when Windows has no icon for it.</summary>
    static string FileGlyph(string ext) => ext.ToLowerInvariant() switch
    {
        "" => "",
        "mkv" or "mp4" or "avi" or "mov" or "m4v" or "webm" => "",
        "mp3" or "flac" or "m4a" or "m4b" or "ogg" or "wav" or "opus" => "",
        "epub" or "pdf" or "mobi" or "azw3" or "cbz" or "txt" => "",
        "iso" or "img" or "dmg" => "",
        "zip" or "7z" or "rar" or "tar" or "gz" => "",
        _ => "",
    };
}
