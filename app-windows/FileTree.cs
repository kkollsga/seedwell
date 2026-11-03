using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The file list under a torrent's row (the Mac's FileTreeBlock, FilesHeader and FileRowView), drawn from the core's
/// <c>FileListView</c>: its status line, or the header (the summary, the filter of a long list, Show All, Select All and
/// Select None) and the rows (a folder's chevron, the check, the icon, the name, the rate, the size capsule, the progress
/// or the complete mark, the priority menu), scrolling inside after the core's <c>max_rows</c>. Every word and rule is
/// the core's; a click goes to <see cref="FilesStore"/>, which asks the core.
/// </summary>
sealed class FileTree
{
    // The Mac's FileLayout, in epx: the row's triangle column and icon, indent per level, row height and column widths.
    const double IconX = 20, Indent = 16, RowHeight = 24, RateWidth = 78, SizeWidth = 66, ProgressWidth = 70, PriorityWidth = 66, PlayWidth = 26;

    readonly App app;
    readonly FilesStore store;
    readonly Library library;
    readonly Action clearRows;
    readonly Dictionary<string, TextBox> filters = [];

    public FileTree(App app, FilesStore store, Library library, Action clearRows)
    {
        this.app = app;
        this.store = store;
        this.library = library;
        this.clearRows = clearRows;
    }

    static Brush Res(string key) => Theme.Res(key);

    /// <summary>A list closed: its filter field goes with it (a list opened again makes a new one).</summary>
    internal void Forget(string id) => filters.Remove(id);

    /// <summary>The whole block of one torrent's open list.</summary>
    internal FrameworkElement Draw(FileListView v)
    {
        var block = new StackPanel { Margin = new Thickness(0, 0, 0, 6) };
        if (v.Status is string status)
        {
            block.Children.Add(Line(status, new Thickness(IconX + 4, 8, 0, 8)));
            return block;
        }
        if (v.Header is FileListHeader h) block.Children.Add(Header(v.TorrentId, h));
        if (v.Empty is string empty)
        {
            block.Children.Add(Line(empty, new Thickness(IconX + 4, 6, 0, 6)));
            return block;
        }
        var rows = new StackPanel();
        foreach (var r in v.Rows) rows.Children.Add(Row(v, r));
        // Past max_rows the list scrolls inside its own height; Show All lets it grow.
        block.Children.Add(v.MaxRows is uint max && v.Rows.Length > max
            ? new ScrollViewer { Content = rows, MaxHeight = max * RowHeight, VerticalScrollBarVisibility = ScrollBarVisibility.Auto }
            : rows);
        return block;
    }

    static TextBlock Line(string text, Thickness margin) => new() { Text = text, FontSize = Theme.Secondary, Foreground = Res("TextFillColorSecondaryBrush"), Margin = margin };

    /// <summary>The line above the rows: the summary, the filter, Show All, Select All and Select None.</summary>
    UIElement Header(string id, FileListHeader h)
    {
        var g = new Grid { ColumnSpacing = 10, Margin = new Thickness(IconX, 0, 10, 3) };
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var summary = new TextBlock { Text = h.Summary, FontSize = Theme.Caption, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
        ToolTipService.SetToolTip(summary, h.SummaryHelp);
        g.Children.Add(summary);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(right, 1);
        if (h.Filter is FileFilterField f)
        {
            // The field lives across redraws (one per torrent), so typing keeps the caret.
            if (!filters.TryGetValue(id, out var box))
            {
                box = new TextBox { FontSize = Theme.Caption, Width = 150, MinHeight = 26, Padding = new Thickness(8, 3, 8, 3) };
                // Tag: the text the core has, so a redraw's own change is not sent back.
                box.TextChanged += (_, _) => { if (box.Tag as string != box.Text) { box.Tag = box.Text; store.SetFilter(id, box.Text); } };
                filters[id] = box;
            }
            // Out of the header it was in (its visual parent: the logical one can be gone already).
            (box.Parent as Panel ?? VisualTreeHelper.GetParent(box) as Panel)?.Children.Remove(box);
            box.Tag = f.Text;
            if (box.Text != f.Text) box.Text = f.Text;
            box.PlaceholderText = f.Placeholder;
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(box, f.Placeholder);
            right.Children.Add(box);
            if (f.Text.Length > 0)
            {
                var clear = Small("", f.ClearHelp);
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(clear, f.ClearLabel);
                clear.Click += (_, _) => store.SetFilter(id, "");
                right.Children.Add(clear);
            }
        }
        if (h.ShowAll is FileListButton all) right.Children.Add(Link(all, () => store.SetShowAll(!store.ShowAll)));
        right.Children.Add(Link(h.SelectAll, () => store.SelectAll(id, true)));
        right.Children.Add(Link(h.SelectNone, () => store.SelectAll(id, false)));
        g.Children.Add(right);
        return g;
    }

    static HyperlinkButton Link(FileListButton b, Action click)
    {
        var link = new HyperlinkButton { Content = b.Title, IsEnabled = b.Enabled, FontSize = Theme.Caption, Padding = new Thickness(4, 2, 4, 2) };
        ToolTipService.SetToolTip(link, b.Help);
        link.Click += (_, _) => click();
        return link;
    }

    static Button Small(string glyph, string help)
    {
        var b = new Button { Content = new FontIcon { Glyph = glyph, FontSize = 10 }, Padding = new Thickness(4), MinWidth = 0, MinHeight = 0, Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
        ToolTipService.SetToolTip(b, help);
        return b;
    }

    /// <summary>One file or folder row.</summary>
    UIElement Row(FileListView v, FileListRow item)
    {
        var id = v.TorrentId;
        var f = item.File;
        var skipped = f.Check == FileCheck.Off;
        var g = new Grid { Height = RowHeight, ColumnSpacing = 6, Padding = new Thickness(IconX + f.Depth * Indent, 0, 10, 0), CornerRadius = new CornerRadius(6) };
        foreach (var w in new[] { 14.0, 20, 16, double.NaN, RateWidth, SizeWidth, ProgressWidth, PriorityWidth, PlayWidth })
        {
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = double.IsNaN(w) ? new GridLength(1, GridUnitType.Star) : new GridLength(w) });
        }
        if (f.IsFolder)
        {
            var fold = Small(f.IsOpen ? "" : "", item.FoldHelp);
            ((FontIcon)fold.Content).FontSize = 9;
            fold.Padding = new Thickness(2);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(fold, item.FoldLabel);
            fold.Click += (_, _) => store.ToggleFolder(id, f.Id);
            g.Children.Add(fold);
        }
        g.Children.Add(At(Check(id, item), 1));
        g.Children.Add(At(Icon(f, skipped), 2));
        var name = new TextBlock { Text = f.Name, FontSize = Theme.Caption, Foreground = Res(skipped ? "TextFillColorTertiaryBrush" : "TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
        ToolTipService.SetToolTip(name, f.Path);
        g.Children.Add(At(name, 3));
        g.Children.Add(At(new TextBlock { Text = f.Rate, FontSize = Theme.Micro, Foreground = Res("TextFillColorTertiaryBrush"), HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center }, 4));
        g.Children.Add(At(Capsule(f, item.SizeLabel), 5));
        g.Children.Add(At(Progress(f, v.CompleteLabel), 6));
        g.Children.Add(At(Priority(id, item, v.PriorityHelp), 7));
        if (item.Play is FilePlayButton play)
        {
            var b = Small("\uE768", play.Help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, play.Help);
            b.Click += (_, _) => PlayFile(id, f.Id, null);
            g.Children.Add(At(b, 8));
        }
        if (item.Selected) g.Background = Theme.Brush(Theme.Accent, 0.22);
        g.Tapped += (_, e) =>
        {
            // A click on the row's own controls is theirs.
            for (var d = e.OriginalSource as DependencyObject; d is not null && d != g; d = VisualTreeHelper.GetParent(d))
            {
                if (d is ButtonBase) return;
            }
            Click(id, f.Id);
        };
        g.RightTapped += (_, e) =>
        {
            if (Menu(id, f.Id) is MenuFlyout menu)
            {
                menu.ShowAt(g, e.GetPosition(g));
                e.Handled = true;
            }
        };
        return g;
    }

    static FrameworkElement At(FrameworkElement e, int column)
    {
        Grid.SetColumn(e, column);
        return e;
    }

    /// <summary>A click on a file or folder row (also QA's): the torrent rows give up the selection, the file takes it.</summary>
    internal void Click(string torrent, uint node)
    {
        bool Down(Windows.System.VirtualKey k) => Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        clearRows();
        store.Select(torrent, node, Down(Windows.System.VirtualKey.Control), Down(Windows.System.VirtualKey.Shift));
    }

    /// <summary>The check: on, mixed or off, its tooltip and what a screen reader says (the core's).</summary>
    FrameworkElement Check(string id, FileListRow item)
    {
        var box = new CheckBox
        {
            IsThreeState = true,
            IsChecked = item.File.Check switch { FileCheck.On => true, FileCheck.Off => false, _ => null },
            MinWidth = 0,
            MinHeight = 0,
            Padding = new Thickness(0),
            VerticalAlignment = VerticalAlignment.Center,
        };
        ToolTipService.SetToolTip(box, item.CheckHelp);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(box, item.File.Name);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetItemStatus(box, item.CheckValue);
        // The core decides what a click makes of it (and of the selection it is in): the box only asks.
        box.Click += (_, _) => store.ToggleCheck(id, item.File.Id);
        return box;
    }

    FrameworkElement Icon(FileRow f, bool skipped)
    {
        var scale = app.LibraryWindowIfCreated?.Content?.XamlRoot?.RasterizationScale ?? 1;
        FrameworkElement icon = FileIcons.For(f.IsFolder ? "" : f.Ext, scale) is ImageSource source
            ? new Image { Source = source, Width = 16, Height = 16 }
            : new FontIcon { Glyph = f.IsFolder ? "" : "", FontSize = 14 };
        icon.Opacity = skipped ? 0.5 : 1;
        icon.VerticalAlignment = VerticalAlignment.Center;
        return icon;
    }

    /// <summary>The size in a capsule filled as far as the core says (a file's downloaded part, a folder's share).</summary>
    static FrameworkElement Capsule(FileRow f, string label)
    {
        var g = new Grid { Height = 18, CornerRadius = new CornerRadius(9), Background = Theme.Faint(0.06), VerticalAlignment = VerticalAlignment.Center };
        g.Children.Add(new Border { Background = Theme.Faint(0.12), HorizontalAlignment = HorizontalAlignment.Left, Width = SizeWidth * Math.Clamp(f.Fill, 0, 1) });
        g.Children.Add(new TextBlock { Text = f.Size, FontSize = Theme.Micro, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Foreground = Res("TextFillColorSecondaryBrush") });
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, label);
        return g;
    }

    /// <summary>A check mark when everything wanted is here, else a thin bar and the percent; nothing for a skipped file.</summary>
    static FrameworkElement Progress(FileRow f, string completeLabel)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 5, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center };
        if (f.Check == FileCheck.Off) return s;
        if (f.Complete)
        {
            var mark = new FontIcon { Glyph = "", FontSize = 10, Foreground = Res("TextFillColorSecondaryBrush") };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(mark, completeLabel);
            s.Children.Add(mark);
            return s;
        }
        s.Children.Add(new ProgressBar { Value = f.Progress * 100, Maximum = 100, Width = 30, MinHeight = 3, VerticalAlignment = VerticalAlignment.Center });
        s.Children.Add(new TextBlock { Text = $"{f.Percent}%", FontSize = Theme.Micro, Width = 30, TextAlignment = TextAlignment.Right, Foreground = Res("TextFillColorSecondaryBrush") });
        return s;
    }

    /// <summary>The priority: its label as a quiet menu of High, Normal and Low (the row's own checked).</summary>
    FrameworkElement Priority(string id, FileListRow item, string help)
    {
        var menu = new MenuFlyout();
        foreach (var p in item.Priorities)
        {
            var choice = new ToggleMenuFlyoutItem { Text = p.Title, IsChecked = p.Checked };
            var priority = p.Priority;
            choice.Click += (_, _) => store.SetPriority(id, item.File.Id, priority);
            menu.Items.Add(choice);
        }
        var b = new Button
        {
            Content = new TextBlock { Text = item.File.PriorityLabel, FontSize = Theme.Micro, Foreground = Res(item.File.Priority == FilePriority.Skip ? "TextFillColorTertiaryBrush" : "TextFillColorSecondaryBrush") },
            Flyout = menu,
            Padding = new Thickness(4, 1, 4, 1),
            MinWidth = 0,
            MinHeight = 0,
            HorizontalAlignment = HorizontalAlignment.Right,
            VerticalAlignment = VerticalAlignment.Center,
            Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
            BorderThickness = new Thickness(0),
        };
        ToolTipService.SetToolTip(b, help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, $"{help}: {item.File.PriorityLabel}");
        return b;
    }

    /// <summary>A row's menu (the core's: Show in File Explorer, Priority ▸; Play waits for the player, Part F).</summary>
    MenuFlyout? Menu(string torrent, uint node)
    {
        if (library.MenuRows(torrent).FirstOrDefault(r => r.Id == torrent) is not TransferRow row) return null;
        PlayerOption[] players = [];
        try { players = app.Core.PlayPlayers(); } catch { }
        var entries = store.Lists.Menu(torrent, node, row, players, App.HostWindows);
        if (entries.Length == 0) return null;
        var menu = new MenuFlyout();
        foreach (var e in Entries(torrent, node, row, entries)) menu.Items.Add(e);
        return menu;
    }

    IEnumerable<MenuFlyoutItemBase> Entries(string torrent, uint node, TransferRow row, FileMenuEntry[] entries)
    {
        foreach (var e in entries)
        {
            switch (e)
            {
                case FileMenuEntry.Separator:
                    yield return new MenuFlyoutSeparator();
                    break;
                case FileMenuEntry.Note n:
                    yield return new MenuFlyoutItem { Text = n.Title, IsEnabled = false };
                    break;
                case FileMenuEntry.Submenu s:
                    var sub = new MenuFlyoutSubItem { Text = s.Title };
                    foreach (var c in Entries(torrent, node, row, s.Entries)) sub.Items.Add(c);
                    yield return sub;
                    break;
                case FileMenuEntry.Item it:
                    var item = new MenuFlyoutItem { Text = it.Title };
                    var command = it.Command;
                    item.Click += (_, _) => Run(torrent, node, row, command);
                    yield return item;
                    break;
            }
        }
    }

    /// <summary>A file's Play (its button, or Play With in its menu): the core plays that file of the torrent.</summary>
    void PlayFile(string torrent, uint node, string? playerId)
    {
        if (store.Lists.File(torrent, node) is not TorrentFile file) return;
        if (library.MenuRows(torrent).FirstOrDefault(r => r.Id == torrent) is not TransferRow row) return;
        Player.Play(app, row, file.Index, playerId);
    }

    /// <summary>What a file menu's entry does (the core's table).</summary>
    internal void Run(string torrent, uint node, TransferRow row, FileCommand command)
    {
        AppLog.Write($"file command: {command.GetType().Name}");
        switch (command)
        {
            case FileCommand.Reveal r when r.Path is string path:
                if (!Quiet.On) System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"/select,\"{path}\"") { UseShellExecute = true });
                break;
            case FileCommand.Reveal:
                if (app.LibraryWindowIfCreated?.Content?.XamlRoot is XamlRoot root) RowCommands.Run(app.Core, new RowCommand.Reveal(), [row], root);
                break;
            case FileCommand.SetFilePriority p:
                store.SetPriority(torrent, node, p.Priority);
                break;
            case FileCommand.PlayFile pf:
                PlayFile(torrent, pf.Node, pf.PlayerId);
                break;
        }
    }
}
