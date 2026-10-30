using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Get Info for one row, drawn from the core's <c>GetInfoCard</c> (the Mac's info popover): the icon, title and
/// subtitle, the sections of label and value lines (with their buttons, menus and play controls), each tracker with
/// its live state and actions, and the card's buttons. It asks for a new model after <c>refresh_ms</c> and has the
/// core re-read the torrent (reload) off the UI thread after <c>reload_due_ms</c>. Shown as a card over the library page; when
/// it closes the Library hears info_closed.
/// </summary>
sealed class InfoCard
{
    readonly App app;
    readonly GetInfoCard card;
    readonly Func<TransferRow?> row;
    readonly Action close;
    readonly StackPanel panel = new() { Padding = new Thickness(20, 16, 20, 16), Spacing = 6, Width = 460 };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer refresh, reload;
    string signature = "";

    public UIElement Root { get; }

    /// <param name="row">The row as the list has it now (null once it is gone).</param>
    /// <param name="close">Closes the card.</param>
    public InfoCard(App app, TransferRow first, Func<TransferRow?> row, Action close)
    {
        this.app = app;
        this.row = row;
        this.close = close;
        // NEED: the file chooser on Windows (chooser_available false until it exists) and the play offers (Play With).
        card = new GetInfoCard(app.Core, first, new Clock(), false, null);
        Root = new ScrollViewer { Content = panel, MaxHeight = 720, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        var q = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        refresh = q.CreateTimer();
        refresh.IsRepeating = false;
        refresh.Tick += (_, _) => Refresh();
        reload = q.CreateTimer();
        reload.IsRepeating = false;
        reload.Tick += (_, _) => _ = Task.Run(() => { try { card.Reload(); } catch { } });
        Refresh();
    }

    public void Stop()
    {
        refresh.Stop();
        reload.Stop();
    }

    void Refresh()
    {
        if (row() is not TransferRow r)
        {
            close();
            return;
        }
        InfoCardModel m;
        try { m = card.Model(r, App.Now()); }
        catch (Exception e) { AppLog.Write($"info card failed: {e.Message}"); return; }
        if (m.Signature != signature)
        {
            signature = m.Signature;
            Draw(m, r);
        }
        refresh.Interval = TimeSpan.FromMilliseconds(Math.Max(250, m.RefreshMs));
        refresh.Start();
        if (!reload.IsRunning)
        {
            reload.Interval = TimeSpan.FromMilliseconds(Math.Max(1000, m.ReloadDueMs));
            reload.Start();
        }
    }

    static Brush Tone(InfoTone t) => (Brush)Application.Current.Resources[t switch
    {
        InfoTone.Problem => "SystemFillColorCriticalBrush",
        InfoTone.Secondary => "TextFillColorSecondaryBrush",
        _ => "TextFillColorPrimaryBrush",
    }];

    void Draw(InfoCardModel m, TransferRow r)
    {
        panel.Children.Clear();
        var head = new Grid { ColumnSpacing = 12, Margin = new Thickness(0, 0, 0, 8) };
        head.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        head.ColumnDefinitions.Add(new ColumnDefinition());
        var scale = Root.XamlRoot?.RasterizationScale ?? 1;
        if (FileIcons.For(m.IconExt, scale) is ImageSource icon)
        {
            head.Children.Add(new Image { Source = icon, Width = 40, Height = 40, VerticalAlignment = VerticalAlignment.Top });
        }
        var titles = new StackPanel();
        titles.Children.Add(new TextBlock { Text = m.Title, FontSize = 16, FontWeight = FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
        titles.Children.Add(new TextBlock { Text = m.Subtitle, FontSize = 12, Foreground = Tone(InfoTone.Secondary), TextWrapping = TextWrapping.Wrap });
        Grid.SetColumn(titles, 1);
        head.Children.Add(titles);
        panel.Children.Add(head);

        foreach (var s in m.Sections)
        {
            panel.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Margin = new Thickness(0, 6, 0, 6), Fill = (Brush)Application.Current.Resources["DividerStrokeColorDefaultBrush"] });
            panel.Children.Add(new TextBlock { Text = s.Title, FontSize = 12, FontWeight = FontWeights.SemiBold, Foreground = Tone(InfoTone.Secondary) });
            foreach (var line in s.Rows)
            {
                panel.Children.Add(Line(line, r));
            }
            if (s.AskAgain is InfoButton ask)
            {
                var b = Button(ask, r, null);
                b.Margin = new Thickness(130 + 12, 4, 0, 0);
                b.HorizontalAlignment = HorizontalAlignment.Left;
                panel.Children.Add(b);
            }
            if (s.AskAgainNote is string note)
            {
                panel.Children.Add(new TextBlock { Text = note, FontSize = 12, Foreground = Tone(InfoTone.Secondary), Margin = new Thickness(142, 0, 0, 0), TextWrapping = TextWrapping.Wrap });
            }
            foreach (var t in s.Trackers)
            {
                panel.Children.Add(Tracker(t, r));
            }
        }

        if (m.Buttons.Length > 0)
        {
            var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(0, 12, 0, 0) };
            foreach (var b in m.Buttons)
            {
                buttons.Children.Add(Button(b, r, null));
            }
            panel.Children.Add(buttons);
        }
    }

    /// <summary>A label and its value: the label right-aligned in its column, the value in runs of their tones.</summary>
    UIElement Line(InfoCardLine line, TransferRow r)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(130) });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new TextBlock { Text = line.Label, Foreground = Tone(InfoTone.Secondary), HorizontalAlignment = HorizontalAlignment.Right, TextAlignment = TextAlignment.Right, TextWrapping = TextWrapping.Wrap });
        // The value takes the room left and is cut (or wraps); its buttons and menus keep their size after it.
        var value = new Grid { ColumnSpacing = 8 };
        value.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        value.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var after = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Top };
        Grid.SetColumn(after, 1);
        var text = new TextBlock
        {
            TextWrapping = line.Style == InfoValueStyle.Text ? TextWrapping.NoWrap : TextWrapping.Wrap,
            TextTrimming = line.Style == InfoValueStyle.Text ? TextTrimming.CharacterEllipsis : TextTrimming.None,
            IsTextSelectionEnabled = line.Selectable,
        };
        if (line.Style == InfoValueStyle.Code)
        {
            text.FontFamily = new FontFamily("Consolas");
        }
        foreach (var run in line.Value)
        {
            text.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = run.Text, Foreground = Tone(run.Tone) });
        }
        if (line.Help is string help)
        {
            ToolTipService.SetToolTip(text, help);
        }
        value.Children.Add(text);
        value.Children.Add(after);
        if (line.Button is InfoButton b)
        {
            after.Children.Add(Button(b, r, null));
        }
        if (line.Menu is InfoMenu menu)
        {
            var d = new DropDownButton { Content = menu.Title, Padding = new Thickness(8, 1, 8, 2), FontSize = 12 };
            ToolTipService.SetToolTip(d, menu.Help);
            d.Flyout = RowCommands.MenuFrom(app.Core, menu.Entries, [r], d);
            after.Children.Add(d);
        }
        foreach (var play in line.PlayControls)
        {
            if (play.Entry is CommandEntry.Item item)
            {
                var b2 = new Button { Content = item.Title, Padding = new Thickness(10, 1, 10, 2), FontSize = 12, IsEnabled = item.Enabled };
                ToolTipService.SetToolTip(b2, play.Help);
                var command = item.Command;
                b2.Click += (_, _) => RowCommands.Run(app.Core, command, [r], b2.XamlRoot);
                after.Children.Add(b2);
            }
            else if (play.Entry is CommandEntry.Submenu sub)
            {
                var d = new DropDownButton { Content = sub.Title, Padding = new Thickness(8, 1, 8, 2), FontSize = 12 };
                ToolTipService.SetToolTip(d, play.Help);
                d.Flyout = RowCommands.MenuFrom(app.Core, sub.Entries, [r], d);
                after.Children.Add(d);
            }
        }
        Grid.SetColumn(value, 1);
        g.Children.Add(value);
        return g;
    }

    UIElement Tracker(InfoTracker t, TransferRow r)
    {
        var s = new StackPanel { Margin = new Thickness(142, 6, 0, 2), Spacing = 1 };
        var top = new Grid { ColumnSpacing = 6 };
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        top.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        top.Children.Add(new TextBlock { Text = t.Url, FontFamily = new FontFamily("Consolas"), FontSize = 12, IsTextSelectionEnabled = true, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center });
        var copy = Button(t.Copy, r, t.Index);
        Grid.SetColumn(copy, 1);
        top.Children.Add(copy);
        s.Children.Add(top);
        s.Children.Add(new TextBlock { Text = t.Live, FontSize = 12 });
        if (t.Allowed is string allowed)
        {
            s.Children.Add(new TextBlock { Text = allowed, FontSize = 12, Foreground = Tone(InfoTone.Secondary), TextWrapping = TextWrapping.Wrap });
        }
        if (t.Detail.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = t.Detail, FontSize = 12, Foreground = Tone(InfoTone.Secondary), TextWrapping = TextWrapping.Wrap });
        }
        if (t.AnnounceAnyway is InfoButton anyway)
        {
            var b = Button(anyway, r, t.Index);
            b.HorizontalAlignment = HorizontalAlignment.Left;
            s.Children.Add(b);
        }
        return s;
    }

    /// <summary>A card button: its title (or its symbol, the title then its tooltip), red when destructive.</summary>
    Button Button(InfoButton b, TransferRow r, uint? tracker)
    {
        var button = new Button { Padding = new Thickness(10, 2, 10, 3), FontSize = 12 };
        if (b.SymbolOnly)
        {
            button.Content = new FontIcon { Glyph = Glyph(b.Command), FontSize = 12 };
            button.Padding = new Thickness(5, 3, 5, 3);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, b.Title);
        }
        else
        {
            button.Content = b.Title;
        }
        if (b.Destructive)
        {
            button.Foreground = (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"];
        }
        ToolTipService.SetToolTip(button, b.Help);
        button.Click += (_, _) =>
        {
            Run(b.Command, r, tracker, button.XamlRoot);
            if (b.ClosesCard)
            {
                close();
            }
            else
            {
                signature = "";
                Refresh();
            }
        };
        return button;
    }

    static string Glyph(InfoCommand c) => c switch
    {
        InfoCommand.Reveal => "",
        InfoCommand.CopyTrackerUrl => "",
        InfoCommand.AskTrackerAgain or InfoCommand.AnnounceAnyway => "",
        InfoCommand.Remove => "",
        _ => "",
    };

    void Run(InfoCommand c, TransferRow r, uint? tracker, XamlRoot root)
    {
        try
        {
            switch (c)
            {
                case InfoCommand.Reveal: RowCommands.Reveal([r]); break;
                case InfoCommand.Remove: app.Library.AskRemoval(app.Library.MenuRows(r.Id), false); break;
                case InfoCommand.AskTrackerAgain: app.Core.AskTrackerAgain([r.Id]); break;
                case InfoCommand.AnnounceAnyway: app.Core.AnnounceAnyway([r.Id]); break;
                case InfoCommand.CopyTrackerUrl when tracker is uint index:
                    var data = new Windows.ApplicationModel.DataTransfer.DataPackage();
                    data.SetText(app.Core.TrackerUrl(r.Id, index));
                    Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(data);
                    break;
                default:
                    // NEED: the file chooser window (Choose…).
                    app.ShowPlaceholder(c.ToString());
                    break;
            }
        }
        catch (Exception e)
        {
            AppLog.Write($"info command {c} failed: {e.Message}");
        }
        app.LibraryWindow.Pane.Refresh();
    }
}
