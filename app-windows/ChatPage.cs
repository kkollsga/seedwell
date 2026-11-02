using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// A conversation on Windows (a channel's General, a topic, a private conversation; the Mac's ChatViews), drawn from the
/// core's <c>chat_page_model</c> (trs-ffi chat_page.rs): the toolbar (the agents tag, Members, Share…, the ⋯ menu), the
/// message cards (author runs, reply quotes, Markdown, mail banners, JSON, images, file cards, tags, Reply and Tag under
/// the pointer), the members flyout and the composer (attach, paste and drop, pending sends, the reply line). Every
/// word, rule and menu is the core's; this reads the page (<c>chat_page_read</c>, which marks it read) on each library
/// refresh and after every send or command, draws it, and runs the <c>ChatPageCommand</c>s.
/// </summary>
sealed class ChatPage
{
    readonly App app;
    readonly LibraryPlace place;
    readonly ChatAttachments attachments;
    readonly Grid root = new();
    readonly StackPanel toolbar = new() { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(20, 0, 20, 6) };
    readonly ScrollViewer scroll = new() { HorizontalScrollMode = ScrollMode.Disabled };
    readonly StackPanel cards = new() { Padding = new Thickness(20, 0, 20, 12) };
    readonly StackPanel composer = new() { Spacing = 6, Padding = new Thickness(20, 8, 20, 14) };
    // The composer's field lives across redraws, so the caret and what is typed stay.
    readonly TextBox draft = new() { AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MaxHeight = 160 };
    readonly Button send = new();
    readonly Flyout membersFlyout = new();
    readonly List<(ChatImageInput Input, ChatImageDraft Draft)> attached = [];
    SyncMember[] members = [];
    ChatPageModel? model;
    string drawn = "";
    string? lastId;
    string? replyTo;
    bool sending;

    public UIElement Root => root;

    /// <summary>The conversation it shows.</summary>
    internal LibraryPlace Place => place;

    public ChatPage(App app, LibraryPlace place)
    {
        this.app = app;
        this.place = place;
        attachments = new ChatAttachments(app);
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition());
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.Children.Add(toolbar);
        scroll.Content = cards;
        Grid.SetRow(scroll, 1);
        root.Children.Add(scroll);
        var bottom = new Border { Child = composer, BorderThickness = new Thickness(0, 1, 0, 0), BorderBrush = Res("DividerStrokeColorDefaultBrush") };
        Grid.SetRow(bottom, 2);
        root.Children.Add(bottom);
        draft.TextChanged += (_, _) => send.IsEnabled = CanSend();
        draft.PreviewKeyDown += DraftKey;
        draft.Paste += Paste;
        send.Click += (_, _) => _ = Send();
        // Drop: images and files from Explorer become attachments.
        root.AllowDrop = true;
        root.DragOver += (_, e) =>
        {
            if (model?.Composer is { CanPost: true, Attach: true } && e.DataView.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems))
            {
                e.AcceptedOperation = Windows.ApplicationModel.DataTransfer.DataPackageOperation.Copy;
            }
        };
        root.Drop += async (_, e) =>
        {
            if (!e.DataView.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems)) return;
            foreach (var item in await e.DataView.GetStorageItemsAsync())
            {
                if (item is Windows.Storage.StorageFile f) Attach(new ChatImageInput(f.Name, f.Path, null));
            }
        };
        Update();
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Faint(string text, double size = 12) => new() { Text = text, FontSize = size, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };

    SyncChannel? Channel => place is LibraryPlace.Chat c ? app.Core.ChatChannels().FirstOrDefault(x => x.Id == c.ChannelId) : null;

    string? DeviceId => (place as LibraryPlace.Direct)?.DeviceId;

    /// <summary>The model last drawn (QA).</summary>
    internal ChatPageModel? Model => model;

    /// <summary>Reads the page and asks for its model; redraws when its signature changed (on every library refresh).</summary>
    public void Update()
    {
        ChatPageModel? m;
        try
        {
            var read = app.Core.ChatPageRead(place);
            SyncChannel? channel = null;
            SyncTopic? topic = null;
            SyncDirect? direct = null;
            if (place is LibraryPlace.Chat c)
            {
                channel = app.Core.ChatChannels().FirstOrDefault(x => x.Id == c.ChannelId);
                topic = c.TopicId is string t ? app.Core.ChatTopics(c.ChannelId).FirstOrDefault(x => x.Id == t) : null;
            }
            else if (place is LibraryPlace.Direct d)
            {
                direct = app.Core.ChatDirects().FirstOrDefault(x => x.DeviceId == d.DeviceId);
            }
            var title = app.LibraryWindowIfCreated?.Pane.Model?.Title ?? "";
            // To the minute: the cards' times change only with the day (the Mac's).
            var now = (ulong)(DateTimeOffset.UtcNow.ToUnixTimeSeconds() / 60 * 60);
            m = TrsFfiMethods.ChatPageModel(new ChatPageInput(place, title, channel, topic, direct, read, members, now), new Clock(), App.HostWindows);
        }
        catch (Exception e)
        {
            AppLog.Write($"chat page failed: {e.Message}");
            return;
        }
        model = m;
        if (m is null)
        {
            // Not a conversation, or its channel is gone: the library says why.
            return;
        }
        if (m.Signature == drawn)
        {
            return;
        }
        drawn = m.Signature;
        Draw(m);
    }

    void Redraw()
    {
        drawn = "";
        Update();
    }

    // ------------------------------------------------------------------------------------------------ drawing

    void Draw(ChatPageModel m)
    {
        DrawToolbar(m.Toolbar);
        cards.Children.Clear();
        if (m.Empty is string empty)
        {
            var e = Faint(empty, 13);
            e.HorizontalAlignment = HorizontalAlignment.Center;
            e.Margin = new Thickness(0, 40, 0, 0);
            cards.Children.Add(e);
        }
        bool first = true;
        foreach (var card in m.Messages)
        {
            var c = Card(card);
            // 10 points above a run's first card, 4 above the others.
            c.Margin = new Thickness(0, first ? 4 : card.Author is null ? 4 : 10, 0, 0);
            first = false;
            cards.Children.Add(c);
        }
        DrawComposer(m.Composer);
        if (membersFlyout.IsOpen && m.Members is ChatMembersPanel panel)
        {
            membersFlyout.Content = Members(panel);
        }
        // Keep the newest in view.
        var last = m.Messages.LastOrDefault()?.Id;
        if (last != lastId)
        {
            lastId = last;
            cards.UpdateLayout();
            scroll.UpdateLayout();
            scroll.ChangeView(null, scroll.ScrollableHeight, null, true);
        }
    }

    void DrawToolbar(ChatPageToolbar t)
    {
        toolbar.Children.Clear();
        if (t.Tag is SidebarTag tag)
        {
            var capsule = new Border { CornerRadius = new CornerRadius(8), Padding = new Thickness(7, 1, 7, 2), Background = Res("SubtleFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center, Child = new TextBlock { Text = tag.Text, FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush") } };
            Help(capsule, tag.Help);
            toolbar.Children.Add(capsule);
        }
        if (t.Members is ChatPageButton members)
        {
            var b = PageButton(members);
            FlyoutBase.SetAttachedFlyout(b, membersFlyout);
            toolbar.Children.Add(b);
        }
        if (t.Share is ChatPageButton share)
        {
            toolbar.Children.Add(PageButton(share));
        }
        var more = new DropDownButton { Content = new FontIcon { Glyph = "", FontSize = 14 }, Flyout = Menu(t.Menu.Entries) };
        Help(more, t.Menu.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(more, t.Menu.Help);
        toolbar.Children.Add(more);
    }

    Button PageButton(ChatPageButton b)
    {
        var button = new Button { Content = b.Title, VerticalAlignment = VerticalAlignment.Center };
        Help(button, b.Help);
        if (b.AccessibilityLabel is string label) Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, label);
        button.Click += (_, _) => Press(b);
        return button;
    }

    /// <summary>A symbol button (Reply, Tag, Message): its title is its tooltip and name.</summary>
    Button Symbol(string glyph, ChatPageButton b)
    {
        var button = new Button { Content = new FontIcon { Glyph = glyph, FontSize = 12 }, Padding = new Thickness(6, 4, 6, 4), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
        Help(button, b.Help.Length > 0 ? b.Help : b.Title);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, b.AccessibilityLabel ?? b.Title);
        button.Click += (_, _) => Press(b);
        return button;
    }

    FrameworkElement Card(ChatMessageCard c)
    {
        var s = new StackPanel { Spacing = 3 };
        if (c.Author is ChatMessageAuthor a)
        {
            var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            line.Children.Add(new TextBlock { Text = a.Name, FontSize = 13, FontWeight = FontWeights.SemiBold, VerticalAlignment = VerticalAlignment.Center });
            if (a.Badge is SidebarTag badge)
            {
                var capsule = new Border { CornerRadius = new CornerRadius(7), Padding = new Thickness(6, 0, 6, 1), Background = Res("SubtleFillColorTertiaryBrush"), VerticalAlignment = VerticalAlignment.Center, Child = new TextBlock { Text = badge.Text, FontSize = 10.5, Foreground = Res("TextFillColorSecondaryBrush") } };
                Help(capsule, badge.Help);
                line.Children.Add(capsule);
            }
            if (a.Computer is string computer)
            {
                line.Children.Add(new FontIcon { Glyph = "", FontSize = 10, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
                line.Children.Add(new TextBlock { Text = computer, FontSize = 11.5, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
            }
            line.Children.Add(new TextBlock { Text = a.Time, FontSize = 11.5, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
            s.Children.Add(line);
        }
        if (c.ReplyQuote is string quote)
        {
            s.Children.Add(new TextBlock { Text = quote, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextTrimming = TextTrimming.CharacterEllipsis });
        }
        switch (c.Body)
        {
            case ChatMessageBody.Markdown md when md.Blocks.Length > 0:
                s.Children.Add(ChatMarkdown.Blocks(md.Blocks, OpenLink));
                break;
            case ChatMessageBody.Mail mail:
                s.Children.Add(Banner(mail.Banner));
                if (mail.Blocks.Length > 0) s.Children.Add(ChatMarkdown.Blocks(mail.Blocks, OpenLink));
                break;
            case ChatMessageBody.Json json:
                s.Children.Add(new TextBlock { Text = json.Text, FontFamily = new FontFamily("Consolas"), FontSize = 12, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.Wrap });
                break;
            case ChatMessageBody.Unsupported u:
                s.Children.Add(Faint(u.Text));
                break;
        }
        if (c.Images.Length > 0) s.Children.Add(attachments.Images(c.Place, c.Images, Redraw));
        if (c.Files.Length > 0) s.Children.Add(attachments.Files(c.Place, c.Files, Redraw));
        if (c.Tags.Length > 0)
        {
            var tags = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10 };
            foreach (var t in c.Tags)
            {
                var one = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
                one.Children.Add(new FontIcon { Glyph = t.Seen ? "" : "", FontSize = 10, Foreground = Res("TextFillColorSecondaryBrush") });
                one.Children.Add(new TextBlock { Text = t.Text, FontSize = 11.5, Foreground = Res("TextFillColorSecondaryBrush") });
                tags.Children.Add(one);
            }
            s.Children.Add(tags);
        }
        // Reply and Tag, shown while the pointer is on the card.
        var hover = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, HorizontalAlignment = HorizontalAlignment.Right, VerticalAlignment = VerticalAlignment.Top, Opacity = 0 };
        if (c.TagMenu is ChatPageMenu tagMenu)
        {
            var tag = new DropDownButton { Content = new FontIcon { Glyph = "", FontSize = 12 }, Padding = new Thickness(6, 4, 6, 4), Flyout = Menu(tagMenu.Entries) };
            Help(tag, tagMenu.Help);
            hover.Children.Add(tag);
        }
        if (c.Reply is ChatPageButton reply)
        {
            hover.Children.Add(Symbol("", reply));
        }
        var g = new Grid();
        g.Children.Add(s);
        g.Children.Add(hover);
        var border = new Border { Child = g, CornerRadius = new CornerRadius(10), Padding = new Thickness(12, 8, 12, 8) };
        // Mine: tinted with the accent colour (a brush of its own; the theme's is shared).
        border.Background = c.Mine
            ? new SolidColorBrush(((SolidColorBrush)Res("AccentFillColorDefaultBrush")).Color) { Opacity = 0.14 }
            : new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        if (c.Id == replyTo) border.BorderBrush = Res("AccentFillColorDefaultBrush");
        if (c.Id == replyTo) border.BorderThickness = new Thickness(1);
        border.PointerEntered += (_, _) => hover.Opacity = 1;
        border.PointerExited += (_, _) => hover.Opacity = 0;
        return border;
    }

    UIElement Banner(ChatMailBanner b)
    {
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        switch (b)
        {
            case ChatMailBanner.Note n:
                line.Children.Add(new FontIcon { Glyph = n.Seen ? "" : "", FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush") });
                line.Children.Add(Faint(n.Line));
                break;
            case ChatMailBanner.Status st:
                line.Children.Add(new FontIcon { Glyph = st.Done ? "" : "", FontSize = 13, Foreground = st.Done ? Res("SystemFillColorSuccessBrush") : Res("SystemFillColorCautionBrush") });
                line.Children.Add(new TextBlock { Text = st.Title, FontWeight = FontWeights.SemiBold });
                break;
            case ChatMailBanner.Mail mail:
                if (mail.Chip is string chip)
                {
                    line.Children.Add(new Border { CornerRadius = new CornerRadius(6), Padding = new Thickness(6, 0, 6, 1), Background = new SolidColorBrush(Tint(mail.Tint)), VerticalAlignment = VerticalAlignment.Center, Child = new TextBlock { Text = chip, FontSize = 11, FontWeight = FontWeights.SemiBold, Foreground = new SolidColorBrush(Microsoft.UI.Colors.White) } });
                }
                if (mail.To is string to)
                {
                    line.Children.Add(new FontIcon { Glyph = "", FontSize = 11, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
                    line.Children.Add(new TextBlock { Text = to, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
                }
                if (mail.Re is string re)
                {
                    line.Children.Add(new TextBlock { Text = re, FontSize = 12, FontFamily = new FontFamily("Consolas"), Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center });
                }
                if (mail.Summary is string summary)
                {
                    line.Children.Add(new TextBlock { Text = summary, FontSize = 13.5, FontWeight = FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center });
                }
                break;
        }
        return line;
    }

    static Windows.UI.Color Tint(ChatMailTint t) => t switch
    {
        ChatMailTint.Red => Windows.UI.Color.FromArgb(255, 0xD1, 0x34, 0x38),
        ChatMailTint.Blue => Windows.UI.Color.FromArgb(255, 0x00, 0x67, 0xC0),
        ChatMailTint.Orange => Windows.UI.Color.FromArgb(255, 0xCA, 0x50, 0x10),
        ChatMailTint.Teal => Windows.UI.Color.FromArgb(255, 0x03, 0x83, 0x87),
        _ => Windows.UI.Color.FromArgb(255, 0x6E, 0x6E, 0x6E),
    };

    void DrawComposer(ChatComposer c)
    {
        (draft.Parent as Panel)?.Children.Remove(draft);
        (send.Parent as Panel)?.Children.Remove(send);
        composer.Children.Clear();
        if (!c.CanPost)
        {
            composer.Children.Add(Faint(c.CannotPost ?? ""));
            return;
        }
        var place = model?.Place ?? "";
        if (c.Pending.Length > 0) composer.Children.Add(attachments.Pending(place, c.Pending, Redraw));
        if (replyTo is string id && model?.Messages.FirstOrDefault(m => m.Id == id) is ChatMessageCard replying)
        {
            var line = new Grid { ColumnSpacing = 6 };
            line.ColumnDefinitions.Add(new ColumnDefinition());
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            line.Children.Add(Faint(replying.Replying));
            var close = new Button { Content = new FontIcon { Glyph = "", FontSize = 10 }, Padding = new Thickness(4), Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0) };
            Help(close, c.ReplyCancelHelp);
            close.Click += (_, _) => { replyTo = null; Redraw(); };
            Grid.SetColumn(close, 1);
            line.Children.Add(close);
            composer.Children.Add(line);
        }
        if (attached.Count > 0)
        {
            var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            foreach (var a in attached.ToArray())
            {
                chips.Children.Add(attachments.Draft(a.Draft, () => { attached.Remove(a); Redraw(); }));
            }
            composer.Children.Add(new ScrollViewer { Content = chips, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, HorizontalScrollMode = ScrollMode.Auto });
        }
        var row = new Grid { ColumnSpacing = 8 };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        if (c.Attach)
        {
            var attach = new Button { Content = new FontIcon { Glyph = "", FontSize = 14 }, VerticalAlignment = VerticalAlignment.Bottom };
            Help(attach, attachments.Texts.AttachHelp);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(attach, attachments.Texts.Attach);
            attach.Click += (_, _) => PickAttachments();
            row.Children.Add(attach);
        }
        draft.PlaceholderText = c.Placeholder;
        draft.IsEnabled = !sending;
        Grid.SetColumn(draft, 1);
        row.Children.Add(draft);
        send.Content = c.SendTitle;
        send.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
        send.VerticalAlignment = VerticalAlignment.Bottom;
        send.IsEnabled = CanSend();
        Help(send, c.SendHelp);
        Grid.SetColumn(send, 2);
        row.Children.Add(send);
        composer.Children.Add(row);
    }

    UIElement Members(ChatMembersPanel p)
    {
        var s = new StackPanel { Spacing = 6, MinWidth = 320 };
        s.Children.Add(new TextBlock { Text = p.Title, FontWeight = FontWeights.SemiBold });
        foreach (var r in p.Rows)
        {
            var g = new Grid { ColumnSpacing = 8 };
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition());
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.Children.Add(new Microsoft.UI.Xaml.Shapes.Ellipse { Width = 8, Height = 8, VerticalAlignment = VerticalAlignment.Center, Fill = r.Online ? Res("SystemFillColorSuccessBrush") : Res("TextFillColorTertiaryBrush") });
            var name = new TextBlock { Text = r.Name, VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
            Grid.SetColumn(name, 1);
            g.Children.Add(name);
            FrameworkElement role = r.RoleMenu is ChatPageMenu menu
                ? new DropDownButton { Content = menu.Title, Flyout = Menu(menu.Entries), VerticalAlignment = VerticalAlignment.Center }
                : new TextBlock { Text = r.Role, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(role, 2);
            g.Children.Add(role);
            if (r.Message is ChatPageButton message)
            {
                var b = Symbol("", message);
                Grid.SetColumn(b, 3);
                g.Children.Add(b);
            }
            s.Children.Add(g);
        }
        if (p.Gone.Length > 0)
        {
            s.Children.Add(new MenuFlyoutSeparator());
            foreach (var gone in p.Gone)
            {
                var g = new Grid { ColumnSpacing = 8 };
                g.ColumnDefinitions.Add(new ColumnDefinition());
                g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                g.Children.Add(Faint(gone.Line));
                var b = PageButton(gone.Button);
                Grid.SetColumn(b, 1);
                g.Children.Add(b);
                s.Children.Add(g);
            }
        }
        return s;
    }

    /// <summary>A menu of the page: items (checked, toggles, disabled, red, with their tooltips, asking first), submenus, notes, separators.</summary>
    MenuFlyout Menu(ChatPageMenuEntry[] entries)
    {
        var flyout = new MenuFlyout();
        foreach (var item in Items(entries))
        {
            flyout.Items.Add(item);
        }
        return flyout;
    }

    IEnumerable<MenuFlyoutItemBase> Items(ChatPageMenuEntry[] entries)
    {
        foreach (var e in entries)
        {
            switch (e)
            {
                case ChatPageMenuEntry.Separator:
                    yield return new MenuFlyoutSeparator();
                    break;
                case ChatPageMenuEntry.Note n:
                    yield return new MenuFlyoutItem { Text = n.Text, IsEnabled = false, FontSize = 12 };
                    break;
                case ChatPageMenuEntry.Submenu sub:
                    var s = new MenuFlyoutSubItem { Text = sub.Title };
                    foreach (var child in Items(sub.Entries)) s.Items.Add(child);
                    yield return s;
                    break;
                case ChatPageMenuEntry.Item it:
                    MenuFlyoutItem item = it.Checked || it.Toggle ? new ToggleMenuFlyoutItem { IsChecked = it.Checked } : new MenuFlyoutItem();
                    item.Text = it.Title;
                    item.IsEnabled = it.Enabled;
                    if (it.Destructive) item.Foreground = Res("SystemFillColorCriticalBrush");
                    if (it.Help.Length > 0) ToolTipService.SetToolTip(item, it.Help);
                    item.Click += (_, _) => RunItem(it);
                    yield return item;
                    break;
            }
        }
    }

    static void Help(FrameworkElement e, string help)
    {
        if (help.Length > 0) ToolTipService.SetToolTip(e, help);
    }

    // ----------------------------------------------------------------------------------------------- commands

    void Press(ChatPageButton b)
    {
        if (b.Question is SyncQuestion q) Ask(q, b.Command); else Run(b.Command);
    }

    /// <summary>An item: its dialog first (the sidebar's chat dialogs, which run what its action does), else its command.</summary>
    void RunItem(ChatPageMenuEntry.Item it)
    {
        if (it.Dialog is ChatDialog d)
        {
            app.ChatDialogs.Show(d, Channel, DeviceId);
            return;
        }
        Run(it.Command);
    }

    async void Ask(SyncQuestion q, ChatPageCommand command)
    {
        AppLog.Write($"chat page asks: {q.Title}");
        if (Quiet.On || root.XamlRoot is not XamlRoot xr)
        {
            return;
        }
        var d = new ContentDialog
        {
            XamlRoot = xr,
            Title = q.Title,
            Content = new TextBlock { Text = q.Text, TextWrapping = TextWrapping.Wrap },
            PrimaryButtonText = q.ConfirmTitle,
            CloseButtonText = q.CancelTitle,
            DefaultButton = ContentDialogButton.Primary,
        };
        if (q.Destructive) RowCommands.Destructive(d);
        try { if (await d.ShowAsync() == ContentDialogResult.Primary) Run(command); }
        catch (Exception e) { AppLog.Write($"chat question failed: {e.Message}"); }
    }

    /// <summary>What the host does for each command (the core's table); failures are alerts with the core's sentence.</summary>
    internal void Run(ChatPageCommand command)
    {
        AppLog.Write($"chat page: {command.GetType().Name}");
        var channel = Channel;
        switch (command)
        {
            case ChatPageCommand.ShowMembers:
                LoadMembers(open: true);
                break;
            case ChatPageCommand.Share:
                if (channel is not null) ShareDialog.Show(app, channel.Id);
                break;
            case ChatPageCommand.SetMuted mute when channel is not null:
                Call(() => app.Core.ChatSetMuted(channel.Id, mute.Muted), e => new SyncAlert(e, ""));
                return;
            case ChatPageCommand.SetAgents agents when channel is not null:
                Call(() => app.Core.ChatSetAgents(channel.Id, agents.Open), e => new SyncAlert(TrsFfiMethods.ChatAgentsMenu(channel).FailureTitle, e));
                break;
            case ChatPageCommand.SetFollow follow when channel is not null:
                Call(() => app.Core.SyncSetFollow(channel.Id, follow.On), e => new SyncAlert(e, ""));
                break;
            case ChatPageCommand.SetDirectAgents access:
                app.Core.ChatSetDirectAgents(access.Access);
                break;
            case ChatPageCommand.SetRole sr when channel is not null:
                var who = members.FirstOrDefault(m => m.DeviceId == sr.DeviceId)?.Name ?? "";
                Call(() => app.Core.SyncSetRole(channel.Id, sr.DeviceId, sr.Role), e => TrsFfiMethods.SyncSetRoleFailed(who, e), () => LoadMembers(false));
                break;
            case ChatPageCommand.RemoveDevice rd when channel is not null:
                var name = members.FirstOrDefault(m => m.DeviceId == rd.DeviceId)?.Name ?? "";
                Call(() => app.Core.SyncRemoveDevice(channel.Id, rd.DeviceId), e => TrsFfiMethods.SyncRemoveDeviceFailed(name, channel.Name, e), () => LoadMembers(false));
                break;
            case ChatPageCommand.MessagePrivately mp:
                membersFlyout.Hide();
                app.Library.NoteDirectName(mp.DeviceId, mp.Name);
                app.ShowPlace(new LibraryPlace.Direct(mp.DeviceId));
                return;
            case ChatPageCommand.Reply r:
                replyTo = r.MessageId;
                Redraw();
                draft.Focus(FocusState.Programmatic);
                return;
            case ChatPageCommand.SetTags tags:
                _ = SendText(TrsFfiMethods.ChatTagNote(tags.MessageId, tags.Tags), null);
                return;
            default:
                // WakeCommand, NewTopic, Rename, Leave and DeleteConversation come with their dialog (RunItem).
                AppLog.Write($"chat page: {command.GetType().Name} has no dialog here");
                break;
        }
        Redraw();
    }

    void Call(Func<Task> work, Func<string, SyncAlert> failed, Action? done = null)
    {
        _ = Task.Run(work).ContinueWith(t => app.Dispatch(() =>
        {
            if (!t.IsCompletedSuccessfully)
            {
                var e = t.Exception?.InnerException is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : t.Exception?.InnerException?.Message ?? "";
                app.Alert(failed(e));
            }
            done?.Invoke();
            Redraw();
        }));
    }

    /// <summary>Reads the channel's members off the UI thread, then draws the panel (opening it when asked).</summary>
    void LoadMembers(bool open)
    {
        if (Channel is not SyncChannel channel) return;
        _ = Task.Run(() => app.Core.SyncMembers(channel.Id)).ContinueWith(t => app.Dispatch(() =>
        {
            if (t.IsCompletedSuccessfully) members = t.Result;
            Redraw();
            if (model?.Members is ChatMembersPanel panel)
            {
                membersFlyout.Content = Members(panel);
                if (open && toolbar.Children.OfType<Button>().FirstOrDefault(b => FlyoutBase.GetAttachedFlyout(b) == membersFlyout) is Button anchor)
                {
                    membersFlyout.ShowAt(anchor);
                }
            }
        }));
    }

    void OpenLink(ChatLink link)
    {
        AppLog.Write($"chat link: {link.Target}");
        if (link.Target == ChatLinkTarget.Route)
        {
            app.Open([app.Core.RouteLink(link.Url)]);
        }
        else if (!Quiet.On && Uri.TryCreate(link.Url, UriKind.Absolute, out var uri))
        {
            _ = Windows.System.Launcher.LaunchUriAsync(uri);
        }
    }

    // ----------------------------------------------------------------------------------------------- composer

    bool CanSend() => !sending && TrsFfiMethods.ChatComposerCanSend(draft.Text, (uint)attached.Count);

    void DraftKey(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key != Windows.System.VirtualKey.Enter) return;
        bool Down(Windows.System.VirtualKey k) => Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        if (TrsFfiMethods.ChatComposerKey(Down(Windows.System.VirtualKey.Shift), Down(Windows.System.VirtualKey.Menu)) == ChatComposerKey.Send)
        {
            e.Handled = true;
            _ = Send();
        }
    }

    /// <summary>Paste: an image or files on the clipboard become attachments; text goes into the field as usual.</summary>
    async void Paste(object sender, TextControlPasteEventArgs e)
    {
        if (model?.Composer is not { Attach: true }) return;
        var clip = Windows.ApplicationModel.DataTransfer.Clipboard.GetContent();
        if (clip.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems))
        {
            e.Handled = true;
            foreach (var item in await clip.GetStorageItemsAsync())
            {
                if (item is Windows.Storage.StorageFile f) Attach(new ChatImageInput(f.Name, f.Path, null));
            }
        }
        else if (clip.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Bitmap))
        {
            e.Handled = true;
            var reference = await clip.GetBitmapAsync();
            using var stream = await reference.OpenReadAsync();
            var bytes = new byte[stream.Size];
            using var reader = new Windows.Storage.Streams.DataReader(stream);
            await reader.LoadAsync((uint)stream.Size);
            reader.ReadBytes(bytes);
            Attach(new ChatImageInput(attachments.Texts.PastedName, null, bytes));
        }
    }

    async void PickAttachments()
    {
        foreach (var path in await app.PickFiles(attachments.Texts.Extensions))
        {
            Attach(new ChatImageInput(Path.GetFileName(path), path, null));
        }
    }

    /// <summary>An image or file for the next message: the core checks it and makes its chip; a refusal is an alert.</summary>
    internal void Attach(ChatImageInput input)
    {
        if (attached.Count >= attachments.Texts.MaxImages)
        {
            app.Alert(new SyncAlert(attachments.Texts.TooMany, ""));
            return;
        }
        _ = Task.Run(() => app.Core.ChatImageCheck(input)).ContinueWith(t => app.Dispatch(() =>
        {
            if (t.IsCompletedSuccessfully)
            {
                attached.Add((input, t.Result));
                AppLog.Write($"chat attach: {t.Result.Caption}");
            }
            else
            {
                var e = t.Exception?.InnerException is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : t.Exception?.InnerException?.Message ?? "";
                app.Alert(new SyncAlert(e, ""));
            }
            Redraw();
        }));
    }

    /// <summary>Send: the field and the attachments, to the conversation (as a reply when one is being written).</summary>
    internal async Task<bool> Send()
    {
        if (!CanSend() || model is not ChatPageModel m) return false;
        var body = TrsFfiMethods.ChatComposerBody(draft.Text);
        var inputs = attached.Select(a => a.Input).ToList();
        bool ok;
        if (inputs.Count > 0)
        {
            sending = true;
            var reply = replyTo;
            ok = await Background(() => app.Core.ChatSendAttachments(m.Place, m.Topic, body ?? "", reply, inputs.ToArray()));
            sending = false;
        }
        else if (body is string text)
        {
            ok = await SendText(text, replyTo);
        }
        else
        {
            return false;
        }
        if (ok)
        {
            draft.Text = "";
            attached.Clear();
            replyTo = null;
        }
        Redraw();
        return ok;
    }

    async Task<bool> SendText(string text, string? reply)
    {
        sending = true;
        var ok = place switch
        {
            LibraryPlace.Chat c => await Background(() => app.Core.ChatSend(c.ChannelId, c.TopicId, text, reply)),
            LibraryPlace.Direct d => await Background(() => app.Core.ChatSendDirect(d.DeviceId, text, reply)),
            _ => false,
        };
        sending = false;
        Redraw();
        return ok;
    }

    async Task<bool> Background(Func<Task> work)
    {
        try
        {
            await Task.Run(work);
            AppLog.Write("chat: sent");
            return true;
        }
        catch (Exception e)
        {
            var message = e is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : e.Message;
            AppLog.Write($"chat: send failed: {message}");
            app.Alert(new SyncAlert(message, ""));
            return false;
        }
    }

    // --------------------------------------------------------------------------------------------------- QA

    internal void QaType(string text) => draft.Text = text;

    /// <summary>The visual QA run: the ⋯ menu as a click opens it.</summary>
    internal MenuFlyout RenderMenu() => Menu(model?.Toolbar.Menu.Entries ?? []);
    internal void QaReply(int n)
    {
        if (model?.Messages.ElementAtOrDefault(n)?.Reply is ChatPageButton b) Run(b.Command);
    }
    internal void QaMembers() => Run(new ChatPageCommand.ShowMembers());
    internal string Draft => draft.Text;
    internal int Attached => attached.Count;
    internal string? ReplyingTo => replyTo;
    internal ChatAttachments Attachments => attachments;
    internal bool MembersOpen => membersFlyout.IsOpen;

    /// <summary>QA: runs the ⋯ menu's item titled <paramref name="title"/> (in a submenu too); its dialog opens as a click would.</summary>
    internal string QaMenu(string title)
    {
        if (model is null) return "no model";
        ChatPageMenuEntry.Item? Find(IEnumerable<ChatPageMenuEntry> es)
        {
            foreach (var e in es)
            {
                if (e is ChatPageMenuEntry.Item it && it.Title.StartsWith(title)) return it;
                if (e is ChatPageMenuEntry.Submenu s && Find(s.Entries) is ChatPageMenuEntry.Item found) return found;
            }
            return null;
        }
        if (Find(model.Toolbar.Menu.Entries) is not ChatPageMenuEntry.Item item) return "no such item";
        if (!item.Enabled) return "disabled";
        RunItem(item);
        return $"ran {item.Command.GetType().Name}{(item.Dialog is null ? "" : " (dialog)")}";
    }

    /// <summary>QA: the menu as drawn, one entry per title ([x] checked, (off) disabled, &gt; submenu).</summary>
    internal string QaMenuText()
    {
        if (model is null) return "";
        string Text(IEnumerable<ChatPageMenuEntry> es) => string.Join(" | ", es.Select(e => e switch
        {
            ChatPageMenuEntry.Item it => $"{it.Title}{(it.Checked ? " [x]" : "")}{(it.Enabled ? "" : " (off)")}",
            ChatPageMenuEntry.Submenu s => $"{s.Title} > [{Text(s.Entries)}]",
            ChatPageMenuEntry.Note n => $"({n.Text})",
            _ => "-",
        }));
        return Text(model.Toolbar.Menu.Entries);
    }
}
