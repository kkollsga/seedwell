using Microsoft.UI;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Text;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The tray icon's left-click window: the Mac's menu bar menu, drawn from the core's <c>menu_model</c> (content,
/// order and texts are the model's; this only draws, with Fluent controls and the system's flyout look). The Folder
/// Sync line expands in place to <c>sync_menu_model</c>'s folders. Rebuilt when the model's signature changes,
/// otherwise updated in place; asked again every <c>refresh_ms</c> while open, never while closed. It hides as soon as
/// another window is in front (a window Windows did not let come to the foreground is never "deactivated"), and on
/// Esc.
/// </summary>
public sealed partial class FlyoutWindow : Window
{
    const int Width = 360, MaxHeight = 640, Margin = 12;

    readonly App app;
    readonly IntPtr hwnd;
    readonly DispatcherQueueTimer refresh, focus;
    DateTime hiddenAt = DateTime.MinValue;
    string signature = "";
    string syncSignature = "";
    bool syncOpen;
    Snapshot? last;
    SyncSnapshot? lastSync;

    /// <summary>What a row shows, kept to update it in place.</summary>
    sealed record RowView(Grid Mark, TextBlock Subtitle, CheckBox? Check);
    readonly Dictionary<string, RowView> rows = [];
    TextBlock? sharingRate, syncSubtitle;

    public bool IsOpen { get; private set; }

    internal FlyoutWindow(App app)
    {
        this.app = app;
        InitializeComponent();
        // Acrylic, as the system's own flyouts (volume, network) have; without a backdrop the window is plain black.
        SystemBackdrop = new DesktopAcrylicBackdrop();
        var p = OverlappedPresenter.Create();
        p.SetBorderAndTitleBar(true, false);
        p.IsResizable = false;
        p.IsMaximizable = false;
        p.IsMinimizable = false;
        p.IsAlwaysOnTop = true;
        AppWindow.SetPresenter(p);
        AppWindow.IsShownInSwitchers = false;
        AppWindow.SetIcon(System.IO.Path.Combine(AppContext.BaseDirectory, "Seedwell.ico"));
        hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        Activated += (_, e) =>
        {
            if (e.WindowActivationState == WindowActivationState.Deactivated && IsOpen && !menuOpen && !Pinned)
            {
                Hide();
            }
        };
        Root.KeyDown += (_, e) =>
        {
            if (e.Key == Windows.System.VirtualKey.Escape)
            {
                Hide();
            }
        };
        var q = DispatcherQueue.GetForCurrentThread();
        refresh = q.CreateTimer();
        refresh.Tick += (_, _) => Refresh(false);
        focus = q.CreateTimer();
        focus.Interval = TimeSpan.FromMilliseconds(250);
        focus.Tick += (_, _) =>
        {
            // A row's context menu is a window of its own: the flyout stays while it is up.
            if (!menuOpen && GetForegroundWindow() != hwnd)
            {
                Hide();
            }
        };
    }

    bool menuOpen;

    /// <summary>The visual QA run: shown without the focus that hides it, and never hidden by losing it.</summary>
    internal bool Pinned { get; private set; }

    internal FrameworkElement RootElement => Root;

    /// <summary>The visual QA run: opens it as the tray click does, with Folder Sync open or not.</summary>
    internal void OpenPinned(bool withSync)
    {
        Pinned = true;
        signature = "";
        syncOpen = withSync;
        Refresh(true);
        Place();
        Quiet.Show(this);
        IsOpen = true;
    }

    /// <summary>The visual QA run: a transfer the menu shows, for its row commands.</summary>
    internal TransferRow? AnyRow() => last is null ? null : last.Downloading.Concat(last.Recent).Concat(last.Queued).FirstOrDefault();

    /// <summary>The tray icon's left click: show it, or hide it if it is shown.</summary>
    public void Toggle()
    {
        // A click on the tray icon first takes the focus away (which hides it): that click only closes it.
        if (IsOpen || DateTime.UtcNow - hiddenAt < TimeSpan.FromMilliseconds(300))
        {
            Hide();
            return;
        }
        signature = "";
        syncOpen = false;
        app.SyncRemoval.Close();
        Refresh(true);
        Place();
        if (Quiet.On)
        {
            // A QA run (--qa menu): shown, never activated or brought forward.
            Quiet.Show(this);
        }
        else
        {
            AppWindow.Show();
            Activate();
            // The click on the notification icon lets this app take the foreground; take it explicitly.
            SetForegroundWindow(hwnd);
        }
        // The keyboard starts at the first control, without a focus ring until a key is used (as the Mac's menu).
        if (Microsoft.UI.Xaml.Input.FocusManager.FindFirstFocusableElement(Root) is Control first)
        {
            first.Focus(FocusState.Programmatic);
        }
        IsOpen = true;
        focus.Start();
        app.FlyoutChanged();
    }

    /// <summary>An update check ended or an install started: the update row is rebuilt when the menu is open.</summary>
    public void UpdateChanged()
    {
        if (IsOpen)
        {
            signature = "";
            Refresh(false);
        }
    }

    /// <summary>Opens it unless it is open (a torrent was just added).</summary>
    public void ShowOpen()
    {
        if (!IsOpen)
        {
            hiddenAt = DateTime.MinValue;
            Toggle();
        }
    }

    public void Hide()
    {
        refresh.Stop();
        focus.Stop();
        AppWindow.Hide();
        IsOpen = false;
        hiddenAt = DateTime.UtcNow;
        app.FlyoutChanged();
    }

    /// <summary>Bottom right of the work area, as tall as its content (up to <see cref="MaxHeight"/>).</summary>
    void Place()
    {
        var work = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        var scale = Root.XamlRoot?.RasterizationScale ?? (GetDpiForWindow(hwnd) / 96.0);
        Root.Measure(new Windows.Foundation.Size(Width, double.PositiveInfinity));
        // The visual QA run draws all of it (up to the work area), not the scrolled part.
        double h = Math.Min(Pinned ? double.MaxValue : MaxHeight, Root.DesiredSize.Height + 2);
        int w = (int)(Width * scale), hp = (int)(h * scale), m = (int)(Margin * scale);
        hp = Math.Min(hp, work.Height - 2 * m);
        // Never outside the work area, however small it is.
        int x = Math.Max(work.X, work.X + work.Width - w - m), y = Math.Max(work.Y, work.Y + work.Height - hp - m);
        AppWindow.MoveAndResize(new Windows.Graphics.RectInt32(x, y, w, hp));
    }

    void Refresh(bool opening)
    {
        MenuModel model;
        try
        {
            last = app.TakeSnapshot();
            lastSync = app.Core.SyncSnapshot();
            model = TrsFfiMethods.MenuModel(last, lastSync, app.Arrivals, App.Now(), app.Updater.Updates.AvailableVersion());
        }
        catch (Exception e)
        {
            Body.Children.Clear();
            Body.Children.Add(Text(e.Message, wrap: true));
            FooterDivider();
            // Ask again (a busy core answers later); the next answer rebuilds the whole menu.
            signature = "";
            refresh.Interval = TimeSpan.FromSeconds(1);
            refresh.Start();
            return;
        }
        SyncMenuModel? sync = syncOpen ? TrsFfiMethods.SyncMenuModel(lastSync, app.SyncRemoval) : null;
        if (opening || model.Signature != signature || (sync?.Signature ?? "") != syncSignature)
        {
            Build(model, sync);
            if (IsOpen && !opening)
            {
                Place();
            }
        }
        else
        {
            Update(model);
        }
        refresh.Interval = TimeSpan.FromMilliseconds(Math.Max(250, model.RefreshMs));
        refresh.Start();
    }

    // ------------------------------------------------------------------------------------------- building

    void Build(MenuModel m, SyncMenuModel? sync)
    {
        signature = m.Signature;
        syncSignature = sync?.Signature ?? "";
        rows.Clear();
        sharingRate = syncSubtitle = null;
        Body.Children.Clear();
        if (m.Offline is string offline)
        {
            Body.Children.Add(Text(offline, secondary: true, margin: new Thickness(10, 2, 10, 6)));
        }
        foreach (var s in m.Sections)
        {
            if (s.SeparatorBefore)
            {
                Body.Children.Add(Separator());
            }
            Body.Children.Add(Text(s.Title, bold: true, secondary: true, size: 12, margin: new Thickness(10, 6, 10, 2)));
            foreach (var r in s.Rows)
            {
                Body.Children.Add(Row(r));
            }
            if (s.More is string more)
            {
                Body.Children.Add(Line(Text(more, secondary: true), app.ShowMainWindow));
            }
        }
        if (m.Sharing is MenuSharingLine sh)
        {
            if (sh.SeparatorBefore)
            {
                Body.Children.Add(Separator());
            }
            sharingRate = Text(sh.Rate, secondary: !sh.RateActive);
            sharingRate.HorizontalAlignment = HorizontalAlignment.Right;
            var g = new Grid();
            g.Children.Add(Text(sh.Title));
            g.Children.Add(sharingRate);
            Body.Children.Add(Line(g, app.ShowMainWindow));
        }
        if (m.FolderSync is MenuSyncLine fs)
        {
            if (fs.SeparatorBefore)
            {
                Body.Children.Add(Separator());
            }
            var stack = new StackPanel();
            stack.Children.Add(Text(fs.Title));
            syncSubtitle = Text(fs.Subtitle, secondary: true, size: 12);
            stack.Children.Add(syncSubtitle);
            var g = new Grid();
            g.Children.Add(stack);
            g.Children.Add(new FontIcon { Glyph = syncOpen ? "" : "", FontSize = 10, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Center, Opacity = 0.7 });
            Body.Children.Add(Line(g, () =>
            {
                syncOpen = !syncOpen;
                app.SyncRemoval.Close();
                Refresh(true);
                Place();
            }));
            if (sync is not null)
            {
                Body.Children.Add(SyncPanel(sync));
            }
        }
        if (m.Update is string update)
        {
            Body.Children.Add(Separator());
            // The update on offer: a click installs it (the core's installer quits and reopens the app).
            Body.Children.Add(Line(Text(update), () => { Hide(); app.Updater.Install(); }));
        }
        OpenButton.Content = m.OpenTitle;
        QuitButton.Content = m.QuitTitle;
        FooterDivider();
    }

    /// <summary>The footer's divider only below something: idle, the menu is the footer alone.</summary>
    void FooterDivider()
    {
        bool any = Body.Children.Count > 0;
        Footer.BorderThickness = new Thickness(0, any ? 1 : 0, 0, 0);
        Footer.Margin = new Thickness(0, any ? 6 : 0, 0, 0);
        Footer.Padding = new Thickness(0, any ? 6 : 0, 0, 0);
    }

    /// <summary>Updates rows, the Sharing rate and the Folder Sync subtitle in place (same signature).</summary>
    void Update(MenuModel m)
    {
        foreach (var r in m.Sections.SelectMany(s => s.Rows))
        {
            if (!rows.TryGetValue(r.Id, out var v))
            {
                continue;
            }
            v.Subtitle.Text = r.Subtitle;
            v.Subtitle.Visibility = r.Subtitle.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
            SetMark(v.Mark, r.Mark);
            if (v.Check is not null && r.Running is bool running && v.Check.IsChecked != running)
            {
                v.Check.IsChecked = running;
            }
        }
        if (m.Sharing is MenuSharingLine sh && sharingRate is not null)
        {
            sharingRate.Text = sh.Rate;
            sharingRate.Opacity = sh.RateActive ? 1 : 0.6;
        }
        if (m.FolderSync is MenuSyncLine fs && syncSubtitle is not null)
        {
            syncSubtitle.Text = fs.Subtitle;
        }
    }

    UIElement Row(MenuRow r)
    {
        // As the Mac's: the running checkbox first (when the row has one), then the mark, then the text.
        var g = new Grid { ColumnSpacing = 8, Opacity = r.Dimmed ? Look.Dimmed : 1 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(20) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        var mark = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(mark, 1);
        SetMark(mark, r.Mark);
        g.Children.Add(mark);
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(text, 2);
        var title = Text(r.Title);
        title.TextTrimming = TextTrimming.CharacterEllipsis;
        text.Children.Add(title);
        var sub = Text(r.Subtitle, secondary: true, size: 12);
        sub.Visibility = r.Subtitle.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
        text.Children.Add(sub);
        g.Children.Add(text);
        CheckBox? check = null;
        if (r.Running is bool running)
        {
            check = new CheckBox { IsChecked = running, MinWidth = 0, Padding = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
            var id = r.Id;
            var countdown = r.Countdown;
            check.Click += (_, _) =>
            {
                var on = check.IsChecked == true;
                try
                {
                    if (countdown)
                    {
                        if (on) app.Core.PendingResume(id); else app.Core.PendingPause(id);
                    }
                    else
                    {
                        if (on) app.Core.Resume([id]); else app.Core.Pause([id]);
                    }
                }
                catch (Exception e)
                {
                    app.ShowPlaceholder(e.Message);
                }
                Refresh(false);
            };
            g.Children.Add(check);
        }
        rows[r.Id] = new RowView(mark, sub, check);
        Action? click = r.Click switch
        {
            MenuClick.Reveal => () => { var t = Find(r.Id); if (t is not null) { Hide(); RowCommands.Reveal([t]); } },
            // NEED: the file chooser window (the Mac's ChoiceStore and chooser are Swift).
            MenuClick.OpenChooser => () => app.ShowPlaceholder("OpenChooser"),
            _ => null,
        };
        var line = Line(g, click);
        line.RightTapped += (_, e) =>
        {
            var t = Find(r.Id);
            if (t is null)
            {
                return;
            }
            var menu = RowCommands.Menu(app.Core, [t], true, Root.XamlRoot);
            menuOpen = true;
            menu.Closed += (_, _) =>
            {
                menuOpen = false;
                if (!Quiet.On)
                {
                    Activate();
                    SetForegroundWindow(hwnd);
                }
            };
            menu.ShowAt(line, e.GetPosition(line));
            e.Handled = true;
        };
        return line;
    }

    TransferRow? Find(string id) =>
        last is null ? null : last.Downloading.Concat(last.Queued).Concat(last.Recent).Concat(last.SharingBusiest).FirstOrDefault(t => t.Id == id);

    /// <summary>
    /// Progress as the Mac's ring (Drawing.ring), drawn rather than a ProgressRing so it is the same at every size: on a
    /// 16 px square, inset 2, a 2 px track in the text colour at 30 % and a 2 px arc in the text colour with round caps
    /// from 12 o'clock clockwise, at least 4 %. <paramref name="color"/>: another ink (the library's pause ring: the
    /// accent).
    /// </summary>
    internal static Grid Ring(double fraction, Windows.UI.Color? color = null)
    {
        var text = color ?? (Windows.UI.Color)Application.Current.Resources["TextFillColorPrimary"];
        var g = new Grid { Width = 16, Height = 16 };
        g.Children.Add(new Ellipse { Width = 12, Height = 12, StrokeThickness = 2, Stroke = new SolidColorBrush(Windows.UI.Color.FromArgb(77, text.R, text.G, text.B)) });
        var ink = new SolidColorBrush(text);
        double f = Math.Clamp(fraction, 0.04, 1);
        if (f >= 1)
        {
            g.Children.Add(new Ellipse { Width = 12, Height = 12, StrokeThickness = 2, Stroke = ink });
            return g;
        }
        double a = f * 2 * Math.PI;
        var arc = new PathFigure { StartPoint = new(8, 2), IsClosed = false };
        arc.Segments.Add(new ArcSegment
        {
            Point = new(8 + 6 * Math.Sin(a), 8 - 6 * Math.Cos(a)),
            Size = new(6, 6),
            IsLargeArc = f > 0.5,
            SweepDirection = SweepDirection.Clockwise,
        });
        var geometry = new PathGeometry();
        geometry.Figures.Add(arc);
        g.Children.Add(new Microsoft.UI.Xaml.Shapes.Path { Data = geometry, Stroke = ink, StrokeThickness = 2, StrokeStartLineCap = PenLineCap.Round, StrokeEndLineCap = PenLineCap.Round });
        return g;
    }

    static void SetMark(Grid mark, MenuMark? m)
    {
        mark.Children.Clear();
        switch (m)
        {
            case MenuMark.Ring ring:
                mark.Children.Add(Ring(ring.Fraction));
                break;
            case MenuMark.Attention:
                mark.Children.Add(new Ellipse { Width = 8, Height = 8, Fill = new SolidColorBrush(Windows.UI.Color.FromArgb(255, 255, 149, 0)) });
                break;
            case MenuMark.Done:
                mark.Children.Add(new Ellipse { Width = 14, Height = 14, Fill = new SolidColorBrush(Windows.UI.Color.FromArgb(255, 52, 199, 89)) });
                mark.Children.Add(new FontIcon { Glyph = "", FontSize = 9, Foreground = new SolidColorBrush(Colors.White) });
                break;
        }
    }

    // ------------------------------------------------------------------------------------------- Folder Sync

    /// <summary>
    /// A synced folder's badge, the nearest Fluent drawing of what the Mac draws (MenuSync.swift): up to date = the
    /// green circle with a white tick (the Done mark); progress = the ring; paused = a filled circle in the label
    /// colour with a pause sign; attention = an orange warning triangle; missing = a folder with a question mark.
    /// </summary>
    internal static void SetBadge(Grid badge, SyncBadge b)
    {
        badge.Children.Clear();
        var label = (Brush)Application.Current.Resources["TextFillColorPrimaryBrush"];
        var secondary = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];
        switch (b)
        {
            case SyncBadge.Progress p:
                badge.Children.Add(Ring(p.Fraction));
                break;
            case SyncBadge.UpToDate:
                SetMark(badge, new MenuMark.Done());
                break;
            case SyncBadge.Paused:
                badge.Children.Add(new Ellipse { Width = 14, Height = 14, Fill = label });
                badge.Children.Add(new FontIcon { Glyph = "", FontSize = 8, Foreground = (Brush)Application.Current.Resources["SolidBackgroundFillColorBaseBrush"] });
                break;
            case SyncBadge.Waiting:
                // The Mac's clock.fill in the secondary label colour: a filled circle with the clock cut out of it.
                badge.Children.Add(new Ellipse { Width = 14, Height = 14, Fill = secondary });
                badge.Children.Add(new FontIcon { Glyph = "", FontSize = 10, Foreground = (Brush)Application.Current.Resources["SolidBackgroundFillColorBaseBrush"] });
                break;
            case SyncBadge.Missing:
                // In the secondary label colour, as the Mac's questionmark.folder.fill.
                badge.Children.Add(new FontIcon { Glyph = "", FontSize = 14, Foreground = secondary });
                badge.Children.Add(new TextBlock { Text = "?", FontSize = 8, FontWeight = FontWeights.Bold, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 2, 0, 0), Foreground = secondary });
                break;
            default:
                // Attention, as the Mac's: a filled orange triangle with a dark "!".
                badge.Children.Add(new Polygon
                {
                    Points = { new(8, 1), new(15.5, 14.5), new(0.5, 14.5) },
                    Fill = new SolidColorBrush(Windows.UI.Color.FromArgb(255, 255, 149, 0)),
                    StrokeLineJoin = PenLineJoin.Round,
                    StrokeThickness = 1,
                    Stroke = new SolidColorBrush(Windows.UI.Color.FromArgb(255, 255, 149, 0)),
                });
                badge.Children.Add(new TextBlock { Text = "!", FontSize = 10, FontWeight = FontWeights.Bold, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(0, 3, 0, 0), Foreground = new SolidColorBrush(Windows.UI.Color.FromArgb(230, 0, 0, 0)) });
                break;
        }
    }

    UIElement SyncPanel(SyncMenuModel sm)
    {
        var panel = new StackPanel { Margin = new Thickness(16, 0, 0, 4) };
        foreach (var r in sm.Rows)
        {
            var g = new Grid { ColumnSpacing = 8 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(20) });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            var badge = new Grid { Width = 16, Height = 16, VerticalAlignment = VerticalAlignment.Center };
            SetBadge(badge, r.Badge);
            AutomationLabel(badge, r.AccessibilityLabel);
            g.Children.Add(badge);
            var text = new StackPanel();
            Grid.SetColumn(text, 1);
            // What the model's middle cut leaves must still fit: it ends in "…", never mid-letter (the subtitle too).
            var title = Text(r.Title);
            title.TextTrimming = TextTrimming.CharacterEllipsis;
            text.Children.Add(title);
            if (r.Subtitle.Length > 0)
            {
                var subtitle = Text(r.Subtitle, secondary: true, size: 12);
                subtitle.TextTrimming = TextTrimming.CharacterEllipsis;
                text.Children.Add(subtitle);
            }
            g.Children.Add(text);
            // As the Mac's: a pencil and a bin at the row's end, shown while the pointer or the keyboard is on the row or
            // while its confirmation is open (the bin then lit); the labels are their tooltips and names.
            var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
            // Over the end of the text column: the name has the full width while they are hidden (as the Mac's).
            Grid.SetColumn(buttons, 1);
            buttons.HorizontalAlignment = HorizontalAlignment.Right;
            var id = r.Id;
            // NEED: the folder editor window (Edit); drawn, opens a placeholder for now.
            void Edit() => app.ShowPlaceholder(r.EditLabel);
            void Remove() { app.SyncRemoval.Toggle(id); Refresh(false); }
            var pencil = IconButton("", r.EditLabel, Edit);
            buttons.Children.Add(pencil);
            var bin = IconButton("", r.RemoveLabel, Remove);
            if (r.Confirming)
            {
                // While its question is open: the bin lit, the pencil secondary.
                pencil.Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];
                bin.Foreground = (Brush)Application.Current.Resources["AccentTextFillColorPrimaryBrush"];
            }
            buttons.Children.Add(bin);
            g.Children.Add(buttons);
            var path = lastSync?.Folders.FirstOrDefault(f => f.Id == id)?.Path;
            var line = Line(g, path is null ? null : () => { Hide(); System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"\"{path}\"") { UseShellExecute = true }); });
            bool pointer = false, keyboard = false;
            void Show()
            {
                bool shown = pointer || keyboard || r.Confirming;
                buttons.Opacity = shown ? 1 : 0;
                // The text stops short of them only while they show (2 x 28 px and a gap).
                text.Margin = new Thickness(0, 0, shown ? 60 : 0, 0);
            }
            Show();
            line.PointerEntered += (_, _) => { pointer = true; Show(); };
            line.PointerExited += (_, _) => { pointer = false; Show(); };
            line.GotFocus += (_, _) => { keyboard = true; Show(); };
            line.LostFocus += (_, _) => { keyboard = false; Show(); };
            // The Mac's Cmd+E and Cmd+Backspace: Ctrl+E edits, Delete asks to remove the row the keyboard is on.
            line.KeyDown += (_, e) =>
            {
                var ctrl = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(Windows.System.VirtualKey.Control)
                    .HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
                if (e.Key == Windows.System.VirtualKey.E && ctrl)
                {
                    Edit();
                    e.Handled = true;
                }
                else if (e.Key == Windows.System.VirtualKey.Delete)
                {
                    Remove();
                    e.Handled = true;
                }
            };
            panel.Children.Add(line);
            if (r.Confirming && sm.Confirm is SyncMenuConfirm c && c.FolderId == id)
            {
                panel.Children.Add(Confirm(c));
            }
        }
        panel.Children.Add(Separator());
        // NEED: Add Folder… (a folder picker and sync_add); drawn, opens a placeholder for now.
        var add = Text(sm.Add.Title);
        ToolTipService.SetToolTip(add, sm.Add.Help);
        panel.Children.Add(Line(add, () => app.ShowPlaceholder(sm.Add.Title)));
        return panel;
    }

    UIElement Confirm(SyncMenuConfirm c)
    {
        var p = new StackPanel { Spacing = 6, Margin = new Thickness(28, 2, 8, 8) };
        p.Children.Add(Text(c.Question, wrap: true));
        var trash = new CheckBox { Content = c.TrashLabel, IsChecked = c.Trash };
        trash.Click += (_, _) => app.SyncRemoval.SetTrash(trash.IsChecked == true);
        p.Children.Add(trash);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right };
        // As the Mac's: two small plain buttons of one width (the destructive colour is the Remove dialog's only).
        var cancel = new Button { Content = c.CancelTitle, MinWidth = 80, FontSize = 12, Padding = new Thickness(8, 3, 8, 4) };
        ToolTipService.SetToolTip(cancel, c.CancelHelp);
        cancel.Click += (_, _) => { app.SyncRemoval.Close(); Refresh(false); };
        var remove = new Button { Content = c.RemoveTitle, MinWidth = 80, FontSize = 12, Padding = new Thickness(8, 3, 8, 4) };
        ToolTipService.SetToolTip(remove, c.RemoveHelp);
        remove.Click += async (_, _) =>
        {
            // The core stops syncing and, when asked, moves the folder to the Recycle Bin; it refuses one the bin would
            // delete at once (the folder stays synced). A move that fails after the folder stopped comes back as an alert.
            // Both answer this click: the same dialog, as the Mac's alert.
            SyncAlert? alert;
            try
            {
                alert = app.Core.SyncRemove(c.FolderId, trash.IsChecked == true);
            }
            catch (CoreException e)
            {
                alert = TrsFfiMethods.SyncRemoveFailed(FolderName(c.FolderId), TrsFfiMethods.CoreErrorMessage(e));
            }
            catch (Exception e)
            {
                // Anything else (a panic in the core) is said the same way; an async void handler must not let it out.
                alert = TrsFfiMethods.SyncRemoveFailed(FolderName(c.FolderId), e.Message);
            }
            app.SyncRemoval.Close();
            Refresh(false);
            if (alert is not null)
            {
                try
                {
                    await Alert(alert);
                }
                catch (Exception e)
                {
                    // The dialog itself could not open (another one is showing): say it in a window of its own.
                    app.ShowPlaceholder($"{alert.Title}\n\n{alert.Text}\n\n{e.Message}");
                }
            }
        };
        buttons.Children.Add(cancel);
        buttons.Children.Add(remove);
        p.Children.Add(buttons);
        return p;
    }

    string FolderName(string id) => lastSync?.Folders.FirstOrDefault(f => f.Id == id)?.Name ?? "";

    /// <summary>An alert in the flyout: its title and text, and the core's closing button.</summary>
    async Task Alert(SyncAlert alert)
    {
        var d = new ContentDialog
        {
            XamlRoot = Content.XamlRoot,
            Title = alert.Title,
            Content = new TextBlock { Text = alert.Text, TextWrapping = TextWrapping.Wrap },
            CloseButtonText = TrsFfiMethods.AlertOkTitle(),
            DefaultButton = ContentDialogButton.Close,
        };
        await d.ShowAsync();
    }

    // ------------------------------------------------------------------------------------------- small parts

    static void AutomationLabel(UIElement e, string label) => Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(e, label);

    static TextBlock Text(string text, bool bold = false, bool secondary = false, double size = 14, bool wrap = false, Thickness? margin = null) => new()
    {
        Text = text,
        FontSize = size,
        FontWeight = bold ? FontWeights.SemiBold : FontWeights.Normal,
        Opacity = secondary ? 0.65 : 1,
        TextWrapping = wrap ? TextWrapping.Wrap : TextWrapping.NoWrap,
        Margin = margin ?? new Thickness(0),
    };

    static UIElement Separator() => new Rectangle { Height = 1, Margin = new Thickness(8, 5, 8, 5), Fill = (Brush)Application.Current.Resources["DividerStrokeColorDefaultBrush"] };

    /// <summary>A small borderless icon button; <paramref name="label"/> is its tooltip and its name.</summary>
    static Button IconButton(string glyph, string label, Action run)
    {
        var b = new Button
        {
            Content = new FontIcon { Glyph = glyph, FontSize = 14 },
            Width = 28,
            Height = 28,
            Padding = new Thickness(0),
            Background = new SolidColorBrush(Colors.Transparent),
            BorderThickness = new Thickness(0),
        };
        ToolTipService.SetToolTip(b, label);
        AutomationLabel(b, label);
        b.Click += (_, _) => run();
        return b;
    }

    /// <summary>A line of the menu: highlighted under the pointer; <paramref name="click"/> runs on a click.</summary>
    Grid Line(UIElement content, Action? click)
    {
        var hover = (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"];
        var g = new Grid { Padding = new Thickness(10, 5, 10, 5), CornerRadius = new CornerRadius(4), Background = new SolidColorBrush(Colors.Transparent) };
        g.Children.Add(content);
        g.PointerEntered += (_, _) => g.Background = hover;
        g.PointerExited += (_, _) => g.Background = new SolidColorBrush(Colors.Transparent);
        if (click is not null)
        {
            g.Tapped += (_, e) =>
            {
                // A click on a control inside the line (the checkbox, Edit, Remove) is that control's.
                if (e.OriginalSource is FrameworkElement fe && IsInsideControl(fe, g))
                {
                    return;
                }
                click();
            };
        }
        return g;
    }

    static bool IsInsideControl(DependencyObject? d, Grid line)
    {
        while (d is not null && d != line)
        {
            if (d is Control)
            {
                return true;
            }
            d = VisualTreeHelper.GetParent(d);
        }
        return false;
    }

    void OnOpen(object sender, RoutedEventArgs e)
    {
        Hide();
        app.ShowMainWindow();
    }

    void OnQuit(object sender, RoutedEventArgs e)
    {
        Hide();
        app.Quit();
    }

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern IntPtr GetForegroundWindow();

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern bool SetForegroundWindow(IntPtr hWnd);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern uint GetDpiForWindow(IntPtr hWnd);
}
