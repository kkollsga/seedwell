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
    string sidebarKey = "";
    LibraryList? list;
    FolderSyncPage? syncPage;
    ChatPage? chatPage;
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
    readonly ContentControl banner = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch };
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
        // Under the title: the settings banner (outside Settings), then a Remove question for the whole page.
        var under = new StackPanel();
        under.Children.Add(banner);
        under.Children.Add(titleQuestion);
        under.Children.Add(favoriteQuestion);
        Grid.SetRow(under, 1);
        main.Children.Add(under);
        Grid.SetRow(page, 2);
        main.Children.Add(page);
        // Play's calm note (the core's sentence) at the bottom of the page, for 6 s.
        Grid.SetRow(note, 2);
        main.Children.Add(note);
        // Get Info lies over the page, at its top right, with the theme's card colours and a shadow.
        infoHost.Background = (Brush)Application.Current.Resources["SolidBackgroundFillColorTertiaryBrush"];
        infoHost.BorderBrush = (Brush)Application.Current.Resources["CardStrokeColorDefaultBrush"];
        infoHost.Shadow = new ThemeShadow();
        Grid.SetRow(infoHost, 2);
        main.Children.Add(infoHost);
        root.Children.Add(main);
        // Keys go to the Library, which decides what each does (nothing is decided here); Ctrl with a letter may be an
        // app command's shortcut (the core's app_commands: Add Torrent File…, Limit Speed, ...). Handled keys too: the
        // list takes the arrows itself, and the Library's selection must win.
        root.AddHandler(UIElement.KeyDownEvent, new Microsoft.UI.Xaml.Input.KeyEventHandler((_, e) =>
        {
            var mods = Modifiers();
            if (mods.Command && !Editing() && AppCommandFor(e.Key, mods) is AppCommand command)
            {
                app.RunAppCommand(command);
                e.Handled = true;
            }
            else if (Key(e.Key) is LibraryKey key)
            {
                e.Handled = PressKey(key, mods);
            }
        }), true);
        // Drag and drop onto the window: .torrent files and links go to the core like opened ones (it says why it
        // refuses one), as the Mac's dropDestination.
        root.AllowDrop = true;
        root.DragOver += (_, e) =>
        {
            if (e.DataView.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems)
                || e.DataView.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.WebLink)
                || e.DataView.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Text))
            {
                e.AcceptedOperation = Windows.ApplicationModel.DataTransfer.DataPackageOperation.Copy;
            }
        };
        root.Drop += async (_, e) =>
        {
            var deferral = e.GetDeferral();
            var sources = new List<string>();
            try
            {
                var data = e.DataView;
                if (data.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems))
                {
                    sources.AddRange((await data.GetStorageItemsAsync()).Select(i => i.Path).Where(p => p.Length > 0));
                }
                else if (data.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.WebLink))
                {
                    sources.Add((await data.GetWebLinkAsync()).OriginalString);
                }
                else if (data.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Text)
                    && TrsFfiMethods.AddLinkSource(await data.GetTextAsync()) is string link)
                {
                    sources.Add(link);
                }
            }
            catch (Exception ex)
            {
                AppLog.Write($"drop failed: {ex.Message}");
            }
            finally
            {
                deferral.Complete();
            }
            AppLog.Write($"drop: {sources.Count} to add");
            if (sources.Count > 0)
            {
                app.Add(sources.ToArray());
            }
        };
    }

    internal Library Library => library;

    /// <summary>
    /// A key for the Library (the window's key handler and the QA command): whether it took it. The window is asked
    /// first, the list only when the window did not take it and the list has the focus (the core's KeyScope rule);
    /// <paramref name="inList"/> says the list has it (QA, whose window never takes the focus).
    /// </summary>
    internal bool PressKey(LibraryKey key, KeyModifiers modifiers, bool inList = false)
    {
        var outcome = library.Key(key, modifiers, KeyScope.Window, Context());
        if (!outcome.Handled && (inList || ListHasFocus()))
        {
            outcome = library.Key(key, modifiers, KeyScope.List, Context());
        }
        RunKeyAction(outcome.Action);
        AppLog.Write($"key {key} shift={modifiers.Shift} command={modifiers.Command}: handled {outcome.Handled}");
        Refresh();
        return outcome.Handled;
    }

    /// <summary>What the Library needs to know about the window for keys and new downloads.</summary>
    internal WindowContext Context() => new(Editing() || app.Favorites.View.Busy, app.Files.Expanded, app.Files.Selection?.TorrentId, (uint)(app.Files.Selection?.Nodes.Length ?? 0));

    /// <summary>The file lists' part of a key (the core's LibraryKeyAction); the rest the Library did already.</summary>
    void RunKeyAction(LibraryKeyAction? action)
    {
        var files = app.Files;
        switch (action)
        {
            case LibraryKeyAction.ToggleFileRows: files.ToggleSelected(); break;
            case LibraryKeyAction.OpenFileFolder: files.OpenSelected(); break;
            case LibraryKeyAction.CloseFileFolder:
                if (files.CloseSelected().SelectTorrent is string torrent) library.SetSelection([torrent]);
                break;
            case LibraryKeyAction.OpenFiles open: files.Expand(open.TorrentId); break;
            case LibraryKeyAction.CloseFiles close: files.Collapse(close.TorrentId); break;
            case LibraryKeyAction.ClearFileSelection: files.ClearSelection(); break;
        }
    }

    readonly Border note = new()
    {
        Visibility = Visibility.Collapsed,
        HorizontalAlignment = HorizontalAlignment.Center,
        VerticalAlignment = VerticalAlignment.Bottom,
        Margin = new Thickness(20, 0, 20, 18),
        Padding = new Thickness(14, 7, 14, 8),
        CornerRadius = new CornerRadius(16),
        Background = (Brush)Application.Current.Resources["SolidBackgroundFillColorTertiaryBrush"],
        BorderBrush = (Brush)Application.Current.Resources["CardStrokeColorDefaultBrush"],
        BorderThickness = new Thickness(1),
    };
    Microsoft.UI.Dispatching.DispatcherQueueTimer? noteTimer;

    /// <summary>A calm note at the bottom of the window (Play could not start: the core's sentence), gone after 6 s.</summary>
    internal void ShowNote(string text)
    {
        note.Child = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, MaxWidth = 520, FontSize = Theme.Secondary };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(note, text);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetLiveSetting(note, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        note.Visibility = Visibility.Visible;
        if (noteTimer is null)
        {
            noteTimer = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread().CreateTimer();
            noteTimer.Interval = TimeSpan.FromSeconds(6);
            noteTimer.IsRepeating = false;
            noteTimer.Tick += (_, _) => note.Visibility = Visibility.Collapsed;
        }
        noteTimer.Stop();
        noteTimer.Start();
    }

    /// <summary>The note shown now (QA), or none.</summary>
    internal string? NoteShown => note.Visibility == Visibility.Visible ? (note.Child as TextBlock)?.Text : null;

    // The Remove from Favorites question over the tracker's list (its Info page draws its own).
    readonly ContentControl favoriteQuestion = new() { HorizontalContentAlignment = HorizontalAlignment.Stretch, Margin = new Thickness(16, 0, 16, 0) };
    TrackerInfoPage? trackerPage;

    /// <summary>The Tracker Info page while shown (QA).</summary>
    internal TrackerInfoPage? TrackerPage => trackerPage;

    /// <summary>The favorite editor changed: the sidebar (a name being edited), the question over the list.</summary>
    internal void FavoritesChanged()
    {
        sidebarKey = "";
        Refresh();
    }

    void DrawFavoriteQuestion(LibraryViewModel m)
    {
        var f = app.Favorites;
        favoriteQuestion.Content = m.Page == LibraryPage.List && m.Place is LibraryPlace.Tracker t && f.View.Removal is FavoriteRemoval q && q.Id == t.TrackerId
            ? f.RemovalStrip(q, f.ConfirmRemoval, f.CancelRemoval)
            : null;
    }

    /// <summary>The list page while shown (QA clicks its file rows).</summary>
    internal LibraryList? ListShown => list;

    /// <summary>A chooser changed (a tick here, in the notifier or the chooser window): the held lists draw it again.</summary>
    internal void ChoicesChanged() => list?.ChoicesChanged();

    /// <summary>A file list changed (read, an edit, the engine's answer): the list draws it under its row.</summary>
    internal void FilesChanged(string? id) => list?.FilesChanged(id);

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
        Windows.System.VirtualKey.Up => LibraryKey.Up,
        Windows.System.VirtualKey.Down => LibraryKey.Down,
        Windows.System.VirtualKey.I => LibraryKey.I,
        Windows.System.VirtualKey.A => LibraryKey.A,
        _ => null,
    };

    /// <summary>The enabled app command whose shortcut is this key with these modifiers (Ctrl held), if any.</summary>
    AppCommand? AppCommandFor(Windows.System.VirtualKey k, KeyModifiers mods)
    {
        // A letter key's VirtualKey is its capital letter's code.
        if (k is < Windows.System.VirtualKey.A or > Windows.System.VirtualKey.Z)
        {
            return null;
        }
        var letter = ((char)(int)k).ToString().ToLowerInvariant();
        try
        {
            return app.Core.AppCommands().SelectMany(m => m.Entries).OfType<AppMenuEntry.Item>()
                .FirstOrDefault(i => i.Enabled && i.Shortcut is KeyShortcut s && s.Key == letter && s.Shift == mods.Shift && s.Alt == mods.Option)?.Command;
        }
        catch (Exception e)
        {
            AppLog.Write($"app commands failed: {e.Message}");
            return null;
        }
    }

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
        // Open file lists are read only while the window can be seen.
        if (visible) app.Files.Resume(); else app.Files.Suspend();
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
        // The open file lists the core says are due are read again (the Mac's tick on the window's refresh).
        if (m.Page == LibraryPage.List) app.Files.Tick();
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
        summary.Visibility = m.Summary.Length > 0 && m.Page != LibraryPage.TrackerInfo ? Visibility.Visible : Visibility.Collapsed;
        // The Info page has its own title row (the star, the name, "Info" under Back): the header gives way (one title).
        title.Visibility = m.Page == LibraryPage.TrackerInfo ? Visibility.Collapsed : Visibility.Visible;
        // The rows' live rates, as the Mac's tracker header (Fmt.liveRate with its arrow).
        // On the Info page the header has them (the core's TrackerHeaderModel).
        rates.Text = m.TrackerRates is TrackerRates r && m.Page != LibraryPage.TrackerInfo ? $"{TrsFfiMethods.FmtLiveRate(r.Down, "↓")}   {TrsFfiMethods.FmtLiveRate(r.Up, "↑")}" : "";
        // Another place: every torrent opens closed again.
        if (before is not null && !Equals(before.Place, m.Place)) app.Files.CollapseAll();
        if (SidebarKey(m.Sidebar) != sidebarKey || !Equals(before?.Place, m.Place))
        {
            DrawSidebar(m);
        }
        DrawFooter(m.Sidebar.Footer);
        DrawFavoriteQuestion(m);
        DrawAddSheet(m.AddSheet);
        DrawInfo(m);
        ScrollTo(m.ScrollTo);
        // The Remove question is drawn into the list (under its row) or the title bar: a change rebuilds them.
        var removal = m.Removal is PendingRemoval pr ? $"{pr.GroupKey}/{pr.RowId}/{pr.Question.Message}" : "";
        bool removalChanged = removal != removalKey;
        removalKey = removal;
        titleQuestion.Content = m.Removal is { RowId: null } rq ? LibraryList.Question(rq.Question, library, Refresh) : null;
        // Settings draws the banner itself (without Open Settings).
        banner.Content = m.Page == LibraryPage.Settings ? null : app.Banner.Draw(false, () => app.ShowPlace(new LibraryPlace.Settings()));
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
            // The Folder Sync page follows the core's sync state on every refresh, a conversation its messages.
            syncPage?.Update();
            chatPage?.Update();
        }
    }

    // ------------------------------------------------------------------------------------------- sidebar

    void DrawSidebar(LibraryViewModel m)
    {
        sidebarKey = SidebarKey(m.Sidebar);
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
                    var place = c.Item.Place;
                    sidebar.Children.Add(AddLine(add, 1, () => { if (Channel(place) is SyncChannel ch) app.ChatDialogs.Show(TrsFfiMethods.ChatNewTopicDialog(ch), ch); }));
                }
            }
        }
        if (chats.Add is SidebarAdd newChannel)
        {
            sidebar.Children.Add(AddLine(newChannel, 0, () => NewChannelDialog.Show(app, null)));
        }
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

    /// <summary>
    /// What the sidebar's drawing depends on: the core's signature (which items, in what order, open or not) and what
    /// each item shows (its icon, counts, dot, tag and texts), which the core changes in place under the same signature
    /// (a channel muted, a new unread message). The sidebar is small: any change redraws it.
    /// </summary>
    /// <summary>The sidebar as last drawn (QA checks it follows the core).</summary>
    internal string SidebarDrawn => sidebarKey;

    static string SidebarKey(SidebarModel s)
    {
        static string Item(SidebarItem i) => $"{i.Title}/{i.Icon}/{i.Unread}/{i.Badge}/{i.Count}/{i.Online}/{i.Flag?.Help}/{i.Tag?.Text}/{i.Help}";
        var items = s.Places
            .Concat(s.Chats.Channels.SelectMany(c => c.Topics.Prepend(c.Item)))
            .Concat(s.Directs?.Items ?? [])
            .Concat(s.Trackers?.Items ?? []);
        return $"{s.Signature}#{string.Join(";", items.Select(Item))}#{s.Chats.Add?.Title}";
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
        // A favorite being renamed: its label is the field (the core's one edit).
        if (item.Place is LibraryPlace.Tracker renamed && app.Favorites.View.Target is FavoriteTarget.SidebarName sn && sn.TrackerId == renamed.TrackerId)
        {
            var field = new StackPanel();
            field.Children.Add(app.Favorites.Field(sn));
            if (app.Favorites.Hint() is FrameworkElement hint) field.Children.Add(hint);
            Grid.SetColumn(field, 1);
            g.Children.Add(field);
            return g;
        }
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
        if (item.Tag is SidebarTag tag)
        {
            // An agent channel's quiet "agents" capsule after its name (the Mac's ChatTagCapsule).
            name.MaxWidth = SidebarWidth - 140;
            var capsule = new Border
            {
                CornerRadius = new CornerRadius(8),
                Padding = new Thickness(5, 0, 5, 1),
                VerticalAlignment = VerticalAlignment.Center,
                Background = (Brush)Application.Current.Resources["SubtleFillColorSecondaryBrush"],
                Child = new TextBlock { Text = tag.Text, FontSize = 11, Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"] },
            };
            ToolTipService.SetToolTip(capsule, tag.Help);
            nameLine.Children.Add(capsule);
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

    UIElement AddLine(SidebarAdd add, int indent, Action run)
    {
        // A + where the icons are, so it lines up with them (the Mac's).
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10, Opacity = 0.7, Padding = new Thickness(10 + indent * 18 + (indent == 0 ? 20 : 0), 6, 10, 6) };
        line.Children.Add(new FontIcon { Glyph = "", FontSize = 14 });
        line.Children.Add(new TextBlock { Text = add.Title });
        ToolTipService.SetToolTip(line, add.Help);
        line.Tapped += (_, _) => run();
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
                    // A checked entry (On All My Devices while it is on) draws its check mark.
                    MenuFlyoutItem mi = it.Checked ? new ToggleMenuFlyoutItem { Text = it.Title, IsChecked = true } : new MenuFlyoutItem { Text = it.Title };
                    mi.IsEnabled = it.Enabled;
                    if (it.Help.Length > 0)
                    {
                        ToolTipService.SetToolTip(mi, it.Help);
                    }
                    if (it.Destructive)
                    {
                        mi.Foreground = (Brush)Application.Current.Resources["SystemFillColorCriticalBrush"];
                    }
                    var command = it.Command;
                    mi.Click += (_, _) => RunSidebarCommand(command, place);
                    menu.Items.Add(mi);
                    break;
            }
        }
        return menu;
    }

    /// <summary>
    /// What a sidebar menu entry does (the menu and the QA command sidebar). The core lists only commands whose window
    /// this host has (<c>set_host_windows</c>); the chat ones act on the channel as it is now (the menu may have been
    /// open a while), as the Mac's SidebarMenus.run.
    /// </summary>
    internal void RunSidebarCommand(SidebarCommand command, LibraryPlace place)
    {
        var c = Channel(place);
        switch (command)
        {
            case SidebarCommand.ShareChannel when c is not null:
                ShareDialog.Show(app, c.Id);
                break;
            case SidebarCommand.TrackerInfo:
                library.SelectPlace(place);
                library.SetTrackerInfo(true);
                break;
            // Favorites (the core's FavoriteEditor): the name becomes a field, the question opens, an automatic tracker
            // becomes a favorite.
            case SidebarCommand.RenameTracker when place is LibraryPlace.Tracker rt:
                app.Favorites.BeginRename(rt.TrackerId);
                break;
            case SidebarCommand.RemoveFavorite when place is LibraryPlace.Tracker ft:
                app.Favorites.AskRemove(ft.TrackerId);
                break;
            case SidebarCommand.SaveAsFavorite when place is LibraryPlace.Tracker st:
                app.Favorites.SaveAsFavorite(st.TrackerId);
                break;
            case SidebarCommand.Mute or SidebarCommand.Unmute when c is not null:
                var muted = command is SidebarCommand.Mute;
                CoreCalls.InOrder("chat mute", () => app.Core.ChatSetMuted(c.Id, muted), Refresh);
                return;
            case SidebarCommand.SetFollow follow when c is not null:
                CoreCalls.InOrder("set follow", () => app.Core.SyncSetFollow(c.Id, follow.On), Refresh, e => app.Alert(new SyncAlert(e, "")));
                break;
            case SidebarCommand.NewTopic when c is not null:
                app.ChatDialogs.Show(TrsFfiMethods.ChatNewTopicDialog(c), c);
                break;
            case SidebarCommand.RenameChannel when c is not null:
                app.ChatDialogs.Show(TrsFfiMethods.ChatRenameDialog(c), c);
                break;
            case SidebarCommand.LeaveChannel when c is not null:
                app.ChatDialogs.Show(TrsFfiMethods.ChatLeaveDialog(c), c);
                break;
            case SidebarCommand.ChannelAgents when c is not null:
                app.ChatDialogs.Show(TrsFfiMethods.ChatAgentsDialog(c), c);
                break;
            default:
                // Not offered on Windows (its window is not in the host's set) or the channel is gone.
                AppLog.Write($"sidebar command {command} on {place}: nothing to run");
                break;
        }
        Refresh();
    }

    /// <summary>The channel of a chat place, as the core has it now.</summary>
    SyncChannel? Channel(LibraryPlace place)
    {
        if (place is not LibraryPlace.Chat chat)
        {
            return null;
        }
        try { return app.Core.ChatChannels().FirstOrDefault(c => c.Id == chat.ChannelId); }
        catch (Exception e)
        {
            AppLog.Write($"chat channels failed: {e.Message}");
            return null;
        }
    }

    /// <summary>The visual QA run: a chat place's sidebar menu.</summary>
    internal MenuFlyout RenderMenu(LibraryPlace place) => SidebarMenu(library.SidebarMenu(place), place);

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
        // The gear selects Settings (its page: SettingsPage).
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
    /// "Add a Download" opens with Ctrl+Shift+O (the Mac's ⌘⇧O in its File menu) and the QA command add-sheet.
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
                app.ChooseTorrentFile();
            }
            Refresh();
        }
    }

    /// <summary>The Folder Sync page while shown (QA).</summary>
    internal FolderSyncPage? SyncPage => syncPage;
    internal ChatPage? ChatPageShown => chatPage;

    /// <summary>The Settings page last made (QA reaches its sections).</summary>
    internal SettingsPage? SettingsPageShown { get; private set; }

    /// <summary>The add sheet's dialog while it is open (the visual QA run draws it).</summary>
    internal ContentDialog? AddSheetDialog => addSheet;

    // ------------------------------------------------------------------------------------------- page

    UIElement Page(LibraryViewModel m)
    {
        // A conversation's page reads (and marks read) only while it is shown.
        if (m.Page != LibraryPage.Conversation)
        {
            chatPage = null;
        }
        // The Info page reads its tracker only while it is shown.
        trackerPage?.Stop();
        trackerPage = null;
        if (m.Page == LibraryPage.List && m.List is ListModel l)
        {
            listSignature = l.Signature;
            list = new LibraryList(app, library, Refresh, l, m.Removal);
            return list.Root;
        }
        if (m.Page == LibraryPage.Settings)
        {
            SettingsPageShown = new SettingsPage(app);
            return SettingsPageShown.Root;
        }
        if (m.Page == LibraryPage.FolderSync)
        {
            syncPage = new FolderSyncPage(app);
            return syncPage.Root;
        }
        if (m.Page == LibraryPage.Conversation)
        {
            // The same conversation keeps its page (the draft, the reply, the attachments) when the sidebar changes.
            if (chatPage is null || !Equals(chatPage.Place, m.Place))
            {
                chatPage = new ChatPage(app, m.Place);
            }
            return chatPage.Root;
        }
        if (m.Page == LibraryPage.TrackerInfo && m.Place is LibraryPlace.Tracker tracker)
        {
            trackerPage = new TrackerInfoPage(app, app.Favorites, tracker.TrackerId);
            return trackerPage.Root;
        }
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
