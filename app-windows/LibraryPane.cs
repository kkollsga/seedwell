using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The library window's content, drawn from the core's <c>LibraryViewModel</c> (the Mac's MainWindowView): the sidebar
/// (places, chats, devices, trackers and the footer), the title bar (title, summary, a tracker's rates) and the
/// page. Content, order and texts are the model's; this only draws, the Windows way (a navigation pane, Fluent controls).
/// It asks for a new model after every call it makes and after the model's <c>refresh_ms</c>; it rebuilds when the
/// model's signature changes and otherwise keeps what it drew.
/// </summary>
sealed class LibraryPane
{
    const double SidebarWidth = 240;

    readonly App app;
    readonly Library library;
    readonly Grid root = new();
    readonly StackPanel sidebar = new() { Padding = new Thickness(8, 8, 8, 8) };
    readonly StackPanel footer = new() { Padding = new Thickness(12, 8, 12, 12), Spacing = 2 };
    readonly TextBlock title = new() { FontSize = 20, FontWeight = FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis };
    readonly TextBlock summary = new() { FontSize = 12, Opacity = 0.7, TextTrimming = TextTrimming.CharacterEllipsis };
    readonly TextBlock rates = new() { FontSize = 12, VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(12, 0, 0, 0) };
    readonly ContentControl page = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch, VerticalContentAlignment = VerticalAlignment.Stretch };
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    string signature = "";
    string sidebarSignature = "";
    LibraryList? list;
    FolderSyncPage? syncPage;
    // Get Info: the card shown, the row it is for, and the border it lies in.
    InfoCard? info;
    string? infoRow;
    readonly Border infoHost = new()
    {
        HorizontalAlignment = HorizontalAlignment.Right,
        VerticalAlignment = VerticalAlignment.Top,
        Margin = new Thickness(0, 8, 20, 20),
        CornerRadius = new CornerRadius(8),
        BorderThickness = new Thickness(1),
        Visibility = Visibility.Collapsed,
        Translation = new System.Numerics.Vector3(0, 0, 32),
    };
    string listSignature = "";
    string removalKey = "";
    readonly ContentControl titleQuestion = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch, Margin = new Thickness(0, 0, 16, 0) };
    ContentDialog? addSheet;

    public UIElement Root => root;

    /// <summary>The model last drawn.</summary>
    public LibraryViewModel? Model { get; private set; }

    public LibraryPane(App app, Library library)
    {
        this.app = app;
        this.library = library;
        timer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.IsRepeating = false;
        timer.Tick += (_, _) => Refresh();

        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(SidebarWidth) });
        root.ColumnDefinitions.Add(new ColumnDefinition());
        var side = new Grid();
        side.RowDefinitions.Add(new RowDefinition());
        side.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        side.Children.Add(new ScrollViewer { Content = sidebar, VerticalScrollBarVisibility = ScrollBarVisibility.Auto });
        Grid.SetRow(footer, 1);
        side.Children.Add(footer);
        root.Children.Add(side);

        var main = new Grid { Background = (Brush)Application.Current.Resources["LayerFillColorDefaultBrush"], CornerRadius = new CornerRadius(8, 0, 0, 0) };
        Grid.SetColumn(main, 1);
        main.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        main.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        main.RowDefinitions.Add(new RowDefinition());
        var header = new Grid { Padding = new Thickness(20, 14, 16, 10), ColumnSpacing = 8 };
        header.ColumnDefinitions.Add(new ColumnDefinition());
        header.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var titles = new StackPanel();
        titles.Children.Add(title);
        titles.Children.Add(summary);
        header.Children.Add(titles);
        Grid.SetColumn(rates, 1);
        header.Children.Add(rates);
        // SPEC 8.3 "Toolbar": the title and one grey line, nothing else: no search field, no add or pause buttons
        // (set_search stays for the CLI and QA).
        main.Children.Add(header);
        Grid.SetRow(titleQuestion, 1);
        main.Children.Add(titleQuestion);
        Grid.SetRow(page, 2);
        main.Children.Add(page);
        // Get Info lies over the page, at its top right, with the theme's card colours and a shadow.
        infoHost.Background = (Brush)Application.Current.Resources["SolidBackgroundFillColorTertiaryBrush"];
        infoHost.BorderBrush = (Brush)Application.Current.Resources["CardStrokeColorDefaultBrush"];
        infoHost.Shadow = new ThemeShadow();
        Grid.SetRow(infoHost, 2);
        main.Children.Add(infoHost);
        root.Children.Add(main);
        // Keys go to the Library, which decides what each does (nothing is decided here).
        root.KeyDown += (_, e) =>
        {
            if (Key(e.Key) is LibraryKey key)
            {
                e.Handled = PressKey(key, Modifiers());
            }
        };
    }

    internal Library Library => library;

    /// <summary>A key for the Library (the window's key handler and the QA command): whether it took it.</summary>
    internal bool PressKey(LibraryKey key, KeyModifiers modifiers)
    {
        var outcome = library.Key(key, modifiers, ListHasFocus() ? KeyScope.List : KeyScope.Window, Context());
        if (outcome.Action is LibraryKeyAction action)
        {
            // NEED: the file list under a row (the Mac's step 2): its actions are placeholders for now.
            app.ShowPlaceholder(action.GetType().Name);
        }
        AppLog.Write($"key {key} shift={modifiers.Shift} command={modifiers.Command}: handled {outcome.Handled}");
        Refresh();
        return outcome.Handled;
    }

    /// <summary>What the Library needs to know about the window for keys and new downloads.</summary>
    internal WindowContext Context() => new(Editing(), [], null, 0);

    /// <summary>A text field has the keyboard (the add sheet's): keys are typing, not commands.</summary>
    bool Editing() => root.XamlRoot is not null && Microsoft.UI.Xaml.Input.FocusManager.GetFocusedElement(root.XamlRoot) is TextBox;

    bool ListHasFocus()
    {
        if (root.XamlRoot is null || list is null)
        {
            return false;
        }
        var focused = Microsoft.UI.Xaml.Input.FocusManager.GetFocusedElement(root.XamlRoot) as DependencyObject;
        for (var d = focused; d is not null; d = VisualTreeHelper.GetParent(d))
        {
            if (d == list.Root)
            {
                return true;
            }
        }
        return false;
    }

    static LibraryKey? Key(Windows.System.VirtualKey k) => k switch
    {
        Windows.System.VirtualKey.Delete or Windows.System.VirtualKey.Back => LibraryKey.Delete,
        Windows.System.VirtualKey.Enter => LibraryKey.Return,
        Windows.System.VirtualKey.Escape => LibraryKey.Escape,
        Windows.System.VirtualKey.Space => LibraryKey.Space,
        Windows.System.VirtualKey.Left => LibraryKey.Left,
        Windows.System.VirtualKey.Right => LibraryKey.Right,
        Windows.System.VirtualKey.I => LibraryKey.I,
        _ => null,
    };

    /// <summary>The modifiers held now; Ctrl is the Mac's command key, Alt its option key.</summary>
    static KeyModifiers Modifiers()
    {
        static bool Down(Windows.System.VirtualKey key) =>
            Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(key).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        return new KeyModifiers(Down(Windows.System.VirtualKey.Shift), Down(Windows.System.VirtualKey.Control), Down(Windows.System.VirtualKey.Menu), false);
    }

    /// <summary>The window exists (shown, perhaps minimised) or was closed.</summary>
    public void SetOpen(bool open)
    {
        library.SetOpen(open);
        SetVisible(open);
    }

    /// <summary>Shown or hidden (minimised, closed): the model refreshes only while it can be seen.</summary>
    public void SetVisible(bool visible)
    {
        library.SetVisible(visible);
        if (visible) Refresh(); else timer.Stop();
    }

    /// <summary>Asks for the model and draws it; again after its <c>refresh_ms</c>.</summary>
    public void Refresh()
    {
        timer.Stop();
        LibraryViewModel m;
        try
        {
            m = library.Model(App.Now());
        }
        catch (Exception e)
        {
            AppLog.Write($"library model failed: {e.Message}");
            timer.Interval = TimeSpan.FromSeconds(1);
            timer.Start();
            return;
        }
        Draw(m);
        if (m.RefreshMs is uint ms)
        {
            timer.Interval = TimeSpan.FromMilliseconds(ms);
            timer.Start();
        }
    }

    void Draw(LibraryViewModel m)
    {
        var before = Model;
        Model = m;
        title.Text = m.TitleStar ? $"★ {m.Title}" : m.Title;
        summary.Text = m.Summary;
        summary.Visibility = m.Summary.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        // The rows' live rates, as the Mac's tracker header (Fmt.liveRate with its arrow).
        rates.Text = m.TrackerRates is TrackerRates r ? $"{TrsFfiMethods.FmtLiveRate(r.Down, "↓")}   {TrsFfiMethods.FmtLiveRate(r.Up, "↑")}" : "";
        if (m.Sidebar.Signature != sidebarSignature || !Equals(before?.Place, m.Place))
        {
            DrawSidebar(m);
        }
        DrawFooter(m.Sidebar.Footer);
        DrawAddSheet(m.AddSheet);
        DrawInfo(m);
        ScrollTo(m.ScrollTo);
        // The Remove question is drawn into the list (under its row) or the title bar: a change rebuilds them.
        var removal = m.Removal is PendingRemoval pr ? $"{pr.GroupKey}/{pr.RowId}/{pr.Question.Message}" : "";
        bool removalChanged = removal != removalKey;
        removalKey = removal;
        titleQuestion.Content = m.Removal is { RowId: null } rq ? LibraryList.Question(rq.Question, library, Refresh) : null;
        if (m.Signature != signature || (removalChanged && m.Page == LibraryPage.List))
        {
            signature = m.Signature;
            list = null;
            syncPage = null;
            page.Content = Page(m);
        }
        else if (list is not null && m.List is ListModel same)
        {
            // The same list: rows update in place, or the list is rebuilt when its own signature changed.
            if (same.Signature == listSignature) list.Update(same); else page.Content = Page(m);
        }
        else
        {
            // The Folder Sync page follows the core's sync state on every refresh.
            syncPage?.Update();
        }
    }

    // ------------------------------------------------------------------------------------------- sidebar

    void DrawSidebar(LibraryViewModel m)
    {
        sidebarSignature = m.Sidebar.Signature;
        sidebar.Children.Clear();
        foreach (var item in m.Sidebar.Places)
        {
            sidebar.Children.Add(Item(item, m.Place, 0));
        }
        var chats = m.Sidebar.Chats;
        sidebar.Children.Add(Section(chats.Title));
        foreach (var c in chats.Channels)
        {
            sidebar.Children.Add(Item(c.Item, m.Place, 0, c.Open ? "" : "", () => { library.SetChannelOpen(ChannelId(c.Item.Place), !c.Open); Refresh(); }));
            if (c.Open)
            {
                foreach (var t in c.Topics)
                {
                    sidebar.Children.Add(Item(t, m.Place, 1));
                }
                if (c.NewTopic is SidebarAdd add)
                {
                    sidebar.Children.Add(AddLine(add, 1));
                }
            }
        }
        sidebar.Children.Add(AddLine(chats.Add, 0));
        foreach (var s in new[] { m.Sidebar.Directs, m.Sidebar.Trackers })
        {
            if (s is null)
            {
                continue;
            }
            sidebar.Children.Add(Section(s.Title));
            foreach (var item in s.Items)
            {
                sidebar.Children.Add(Item(item, m.Place, 0));
            }
        }
    }

    static string ChannelId(LibraryPlace p) => p is LibraryPlace.Chat c ? c.ChannelId : "";

    static UIElement Section(string text) => new TextBlock
    {
        Text = text,
        FontSize = 12,
        FontWeight = FontWeights.SemiBold,
        Opacity = 0.7,
        Margin = new Thickness(10, 14, 10, 4),
    };

    static string Glyph(SidebarIcon icon) => icon switch
    {
        SidebarIcon.Downloads => "",
        SidebarIcon.Sharing => "",
        SidebarIcon.FolderSync => "",
        SidebarIcon.Channel => "",
        SidebarIcon.MutedChannel => "",
        SidebarIcon.Topic => "",
        SidebarIcon.Person => "",
        SidebarIcon.FavoriteTracker => "",
        SidebarIcon.PublicTracker => "",
        _ => "",
    };

    /// <summary>A sidebar line: icon, title, its counts at the right; selected when it is the window's place.</summary>
    UIElement Item(SidebarItem item, LibraryPlace selected, int indent, string? disclosure = null, Action? disclose = null)
    {
        bool on = Equals(item.Place, selected);
        var g = new Grid { ColumnSpacing = 10, Padding = new Thickness(indent * 18 + (disclosure is null ? 10 : 0), 6, 10, 6), CornerRadius = new CornerRadius(4) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var head = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2 };
        if (disclosure is not null && disclose is not null)
        {
            // As the Mac's: a channel's disclosure before its icon.
            var b = new Button { Content = new FontIcon { Glyph = disclosure, FontSize = 9 }, Width = 18, Height = 20, Padding = new Thickness(0), Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
            b.Click += (_, _) => disclose();
            head.Children.Add(b);
        }
        g.Background = on ? (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"] : new SolidColorBrush(Colors.Transparent);
        var icon = new FontIcon { Glyph = Glyph(item.Icon), FontSize = 16 };
        head.Children.Add(icon);
        g.Children.Add(head);
        var name = new TextBlock { Text = item.Title, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
        if (on || item.Unread > 0)
        {
            // Selected, or a chat with unread messages (as the Mac's ChatSidebarRow).
            name.FontWeight = FontWeights.SemiBold;
        }
        var nameLine = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        nameLine.Children.Add(name);
        if (item.Online is bool online)
        {
            // A device: a 6 px dot after its name, green online, quiet offline (the Mac's). A row of a horizontal panel
            // is not cut, so the name gets a width that leaves room for the dot and the badges.
            name.MaxWidth = SidebarWidth - 120;
            var dot = new Microsoft.UI.Xaml.Shapes.Ellipse
            {
                Width = 6,
                Height = 6,
                VerticalAlignment = VerticalAlignment.Center,
                Fill = online ? new SolidColorBrush(Windows.UI.Color.FromArgb(255, 52, 199, 89)) : (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
                Opacity = online ? 1 : 0.4,
            };
            // NEED: "Online" / "Offline" for screen readers from Rust (the Mac's are Swift).
            nameLine.Children.Add(dot);
        }
        Grid.SetColumn(nameLine, 1);
        g.Children.Add(nameLine);
        var right = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(right, 2);
        if (item.Flag is SidebarFlag flag)
        {
            var dot = new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 7, Height = 7, Fill = (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"] };
            ToolTipService.SetToolTip(dot, flag.Help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(dot, flag.Accessibility);
            right.Children.Add(dot);
        }
        if (item.Unread > 0)
        {
            right.Children.Add(new InfoBadge { Value = (int)item.Unread });
        }
        else if (item.Badge > 0)
        {
            right.Children.Add(new InfoBadge { Value = (int)item.Badge, Background = (Brush)Application.Current.Resources["ControlStrongFillColorDefaultBrush"] });
        }
        if (item.Count is uint count)
        {
            right.Children.Add(new TextBlock { Text = count.ToString(System.Globalization.CultureInfo.CurrentCulture), FontSize = 12, Opacity = 0.6 });
        }
        g.Children.Add(right);
        ToolTipService.SetToolTip(g, item.Help.Length > 0 ? item.Help : null);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, item.Title);
        var place = item.Place;
        g.Tapped += (_, _) => { library.SelectPlace(place); Refresh(); };
        g.RightTapped += (_, e) =>
        {
            var entries = library.SidebarMenu(place);
            if (entries.Length == 0)
            {
                return;
            }
            SidebarMenu(entries, place).ShowAt(g, e.GetPosition(g));
            e.Handled = true;
        };
        return g;
    }

    UIElement AddLine(SidebarAdd add, int indent)
    {
        // NEED: the New Channel and New Topic windows (stage D); drawn, a placeholder for now.
        // A + where the icons are, so it lines up with them (the Mac's).
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10, Opacity = 0.7, Padding = new Thickness(10 + indent * 18 + (indent == 0 ? 20 : 0), 6, 10, 6) };
        line.Children.Add(new FontIcon { Glyph = "", FontSize = 14 });
        line.Children.Add(new TextBlock { Text = add.Title });
        ToolTipService.SetToolTip(line, add.Help);
        line.Tapped += (_, _) => app.ShowPlaceholder(add.Title);
        return line;
    }

    MenuFlyout SidebarMenu(SidebarMenuEntry[] entries, LibraryPlace place)
    {
        var menu = new MenuFlyout();
        foreach (var e in entries)
        {
            switch (e)
            {
                case SidebarMenuEntry.Separator:
                    menu.Items.Add(new MenuFlyoutSeparator());
                    break;
                case SidebarMenuEntry.Item it:
                    var mi = new MenuFlyoutItem { Text = it.Title };
                    if (it.Destructive)
                    {
                        mi.Foreground = (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"];
                    }
                    var command = it.Command;
                    var title = it.Title;
                    mi.Click += (_, _) =>
                    {
                        if (command == SidebarCommand.TrackerInfo)
                        {
                            library.SelectPlace(place);
                            library.SetTrackerInfo(true);
                            Refresh();
                        }
                        else
                        {
                            // NEED: the other sidebar commands' windows and sheets (rename, favorites, channel sharing).
                            app.ShowPlaceholder(title);
                        }
                    };
                    menu.Items.Add(mi);
                    break;
            }
        }
        return menu;
    }

    void DrawFooter(SidebarFooter f)
    {
        footer.Children.Clear();
        // As the Mac's: the rates on one line, the Settings gear at its end.
        var line = new Grid();
        line.ColumnDefinitions.Add(new ColumnDefinition());
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var rateTexts = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, VerticalAlignment = VerticalAlignment.Center };
        foreach (var rate in new[] { f.Down, f.Up })
        {
            if (rate is FooterRate r)
            {
                var t = new TextBlock { Text = r.Text, FontSize = 12 };
                ToolTipService.SetToolTip(t, r.Help);
                rateTexts.Children.Add(t);
            }
        }
        line.Children.Add(rateTexts);
        if (f.LimitSpeed is string limit)
        {
            footer.Children.Add(new TextBlock { Text = limit, FontSize = 12, Opacity = 0.7 });
        }
        if (f.Offline is FooterNote off)
        {
            var t = new TextBlock { Text = off.Text, FontSize = 12, Foreground = (Brush)Application.Current.Resources["SystemFillColorCautionBrush"] };
            ToolTipService.SetToolTip(t, off.Help);
            footer.Children.Add(t);
        }
        // NEED: the Settings page (stage D); the gear selects the place, whose page is a placeholder for now.
        var settings = new Button
        {
            Content = new FontIcon { Glyph = "", FontSize = 14 },
            Padding = new Thickness(6),
            Background = new SolidColorBrush(Colors.Transparent),
            BorderThickness = new Thickness(0),
        };
        if (f.SettingsActive)
        {
            settings.Foreground = (Brush)Application.Current.Resources["AccentTextFillColorPrimaryBrush"];
        }
        ToolTipService.SetToolTip(settings, f.SettingsHelp);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(settings, f.SettingsLabel);
        settings.Click += (_, _) => { library.SelectPlace(new LibraryPlace.Settings()); Refresh(); };
        Grid.SetColumn(settings, 1);
        line.Children.Add(settings);
        footer.Children.Add(line);
    }

    /// <summary>The row the Library says has Get Info open, if it is in the list.</summary>
    static ListRow? InfoRow(LibraryViewModel m) => m.List?.Groups.SelectMany(g => g.Rows).FirstOrDefault(r => r.InfoOpen);

    /// <summary>
    /// Get Info as a card over the top right of the page (where the Mac's popover sits), for the row the Library marks;
    /// it stays until it is closed (its close button, Esc through key(), or another row's Get Info) and then the Library
    /// hears info_closed. A card rather than a flyout: a flyout closes whenever the window loses focus.
    /// </summary>
    void DrawInfo(LibraryViewModel m)
    {
        var wanted = InfoRow(m);
        if (wanted?.Id == infoRow)
        {
            return;
        }
        info?.Stop();
        info = null;
        infoRow = null;
        infoHost.Child = null;
        infoHost.Visibility = Visibility.Collapsed;
        if (wanted is null)
        {
            return;
        }
        var id = wanted.Id;
        infoRow = id;
        void Close()
        {
            if (infoRow != id)
            {
                return;
            }
            library.InfoClosed(id);
            Refresh();
        }
        info = new InfoCard(app, wanted.Transfer,
            () => Model?.List?.Groups.SelectMany(g => g.Rows).FirstOrDefault(r => r.Id == id)?.Transfer,
            Close);
        var card = new Grid();
        card.Children.Add(info.Root);
        // NEED: the close button's help and name from Rust (the Mac's popover closes by a click outside).
        var x = new Button { Content = new FontIcon { Glyph = "\uE711", FontSize = 10 }, Padding = new Thickness(6), Margin = new Thickness(0, 8, 8, 0), HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Top, Background = new SolidColorBrush(Colors.Transparent), BorderThickness = new Thickness(0) };
        x.Click += (_, _) => Close();
        card.Children.Add(x);
        infoHost.Child = card;
        infoHost.Visibility = Visibility.Visible;
        AppLog.Write($"info: shown for {id}");
    }

    /// <summary>The Get Info card while it is shown (the visual QA run draws it).</summary>
    internal UIElement? InfoCardElement => infoHost.Visibility == Visibility.Visible ? infoHost : null;

    /// <summary>A new download to bring into view: once it is drawn, then the Library is told (scroll_done).</summary>
    void ScrollTo(ScrollTarget? target)
    {
        if (target is null || list?.RowElement(target.RowId) is not FrameworkElement row)
        {
            return;
        }
        row.StartBringIntoView(new BringIntoViewOptions { VerticalAlignmentRatio = 0.5, AnimationDesired = true });
        library.ScrollDone(target.Request);
        AppLog.Write($"scrolled to {target.RowId} (request {target.Request})");
    }

    // ------------------------------------------------------------------------------------------- add sheet

    /// <summary>
    /// NEED: where "Add a Download" opens from on Windows (the Mac's menu bar File menu). The QA command add-sheet opens it.
    /// "Add a Download" while the model has it open: a link or path (add_link_source enables Add with what it finds),
    /// Choose File, Cancel and Add; closing it tells the Library.
    /// </summary>
    void DrawAddSheet(AddLinkSheet? sheet)
    {
        if (sheet is null || addSheet is not null || root.XamlRoot is null)
        {
            return;
        }
        var box = new TextBox { PlaceholderText = sheet.Placeholder, MinWidth = 380 };
        var d = new ContentDialog
        {
            XamlRoot = root.XamlRoot,
            Title = sheet.Title,
            Content = box,
            PrimaryButtonText = sheet.ConfirmTitle,
            SecondaryButtonText = sheet.ChooseFileTitle,
            CloseButtonText = sheet.CancelTitle,
            DefaultButton = ContentDialogButton.Primary,
            IsPrimaryButtonEnabled = false,
        };
        box.TextChanged += (_, _) => d.IsPrimaryButtonEnabled = TrsFfiMethods.AddLinkSource(box.Text) is not null;
        addSheet = d;
        _ = Show();

        async Task Show()
        {
            ContentDialogResult result;
            try { result = await d.ShowAsync(); } catch { result = ContentDialogResult.None; }
            addSheet = null;
            library.CloseAddSheet();
            if (result == ContentDialogResult.Primary && TrsFfiMethods.AddLinkSource(box.Text) is string source)
            {
                app.Add([source]);
            }
            else if (result == ContentDialogResult.Secondary)
            {
                // NEED: the Open dialog for a .torrent file (FileOpenPicker); a placeholder for now.
                app.ShowPlaceholder(sheet.ChooseFileTitle);
            }
            Refresh();
        }
    }

    /// <summary>The add sheet's dialog while it is open (the visual QA run draws it).</summary>
    internal ContentDialog? AddSheetDialog => addSheet;

    // ------------------------------------------------------------------------------------------- page

    UIElement Page(LibraryViewModel m)
    {
        if (m.Page == LibraryPage.List && m.List is ListModel l)
        {
            listSignature = l.Signature;
            list = new LibraryList(app, library, Refresh, l, m.Removal);
            return list.Root;
        }
        if (m.Page == LibraryPage.Settings)
        {
            return new SettingsPage(app).Root;
        }
        if (m.Page == LibraryPage.FolderSync)
        {
            syncPage = new FolderSyncPage(app);
            return syncPage.Root;
        }
        // NEED: Tracker Info and conversations pages (later stages): the page's title for now.
        return Quiet(m.Page == LibraryPage.Missing ? m.Missing ?? "" : m.Title);
    }

    static UIElement Quiet(string text) => new TextBlock
    {
        Text = text,
        Opacity = 0.6,
        HorizontalAlignment = HorizontalAlignment.Center,
        VerticalAlignment = VerticalAlignment.Center,
        TextWrapping = TextWrapping.Wrap,
    };
}
