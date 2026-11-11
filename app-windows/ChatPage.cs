using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// A conversation on Windows (a channel's General, a topic, a private conversation; the Mac's ChatViews), drawn from the
/// core's <c>chat_page_model</c> (seedwell-ffi chat_page.rs) in the build-49 look: the toolbar (the agents mark, Members,
/// Share, the ⋯ menu), the messages (a channel's runs as one block with a left edge in their author's colour, a private
/// conversation's bubbles; who wrote a run on a quiet line under it; reply quotes that jump to the original; Markdown,
/// mail banners, JSON, images on top with galleries, file cards, tags; the time, Tag and Reply in the row under the
/// pointer, never moving anything), the members flyout and the composer (one rounded field: the reply bar, attachments,
/// attach, the text, Send). Every word, rule and menu is the core's; this reads the page (<c>chat_page_read</c>, which
/// marks it read) on each library refresh and after every send or command, draws it, and runs the
/// <c>ChatPageCommand</c>s.
/// </summary>
sealed class ChatPage
{
    /// <summary>A channel's column, and a bubble, at their widest.</summary>
    const double ColumnMax = 820, BubbleMax = 520;
    /// <summary>A channel message's left edge, and the padding inside its block.</summary>
    const double Edge = 2.5, BlockPadX = 12;
    const string AgentGlyph = "";

    readonly App app;
    readonly LibraryPlace place;
    readonly ChatAttachments attachments;
    readonly Grid root = new();
    readonly StackPanel toolbar = new() { Orientation = Orientation.Horizontal, Spacing = 2, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(20, 0, 20, 6) };
    readonly ScrollViewer scroll = new() { HorizontalScrollMode = ScrollMode.Disabled };
    readonly StackPanel cards = new() { Padding = new Thickness(20, 4, 20, 16) };
    readonly StackPanel composer = new() { Spacing = 6, Padding = new Thickness(20, 4, 20, 14) };
    // The composer's field lives across redraws, so the caret and what is typed stay.
    readonly TextBox draft = new() { AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, FontSize = Theme.Body, MaxHeight = 8 * Theme.BodyLine + 12, VerticalAlignment = VerticalAlignment.Center };
    readonly Border field = new() { CornerRadius = new CornerRadius(Theme.ComposerRadius), BorderThickness = new Thickness(1) };
    readonly Button send = new();
    readonly StackPanel inside = new();
    readonly Grid row = new() { ColumnSpacing = 2, Padding = new Thickness(4, 0, 6, 0) };
    Button? attach;
    readonly Flyout membersFlyout = new();
    readonly List<(ChatImageInput Input, ChatImageDraft Draft)> attached = [];
    // Each message drawn, by id: to jump to it (a reply quote), switch its gallery (an image reference) and show its controls (QA).
    readonly Dictionary<string, Shown> shown = [];
    // The image each gallery shows (view state).
    readonly Dictionary<string, int> galleries = [];
    SyncMember[] members = [];
    ChatPageModel? model;
    SyncChannel? readChannel;
    List<TaskCompletionSource> readsAsked = [];
    bool reading;
    string drawn = "";
    string? lastId;
    string? replyTo;
    bool sending;
    double drawnWidth;

    sealed record Shown(FrameworkElement Item, Action<bool> Hover, Action Light, Action<int> Gallery);

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
        // In a grid, which centres the column at its widest (the scroll viewer itself places it by its desired width).
        scroll.Content = new Grid { Children = { cards } };
        Grid.SetRow(scroll, 1);
        root.Children.Add(scroll);
        Grid.SetRow(composer, 2);
        root.Children.Add(composer);
        Plain(draft);
        RoundSend();
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        Grid.SetColumn(draft, 1);
        row.Children.Add(draft);
        Grid.SetColumn(send, 2);
        row.Children.Add(send);
        inside.Children.Add(row);
        field.Child = inside;
        draft.TextChanged += (_, _) => send.IsEnabled = CanSend();
        draft.PreviewKeyDown += DraftKey;
        draft.Paste += Paste;
        draft.GotFocus += (_, _) => field.BorderBrush = Theme.Brush(Theme.Accent);
        draft.LostFocus += (_, _) => field.BorderBrush = Theme.Faint(0.10);
        send.Click += (_, _) => _ = SendClicked();
        // A wider or narrower window fits the images again.
        scroll.SizeChanged += (_, _) =>
        {
            if (Math.Abs(scroll.ActualWidth - drawnWidth) > 24 && model?.Messages.Any(m => m.Images.Length > 0) == true) Redraw();
        };
        root.ActualThemeChanged += (_, _) => Redraw();
        // The first draw comes before the page has a size: the newest comes into view once it has.
        root.Loaded += (_, _) => { scroll.UpdateLayout(); scroll.ChangeView(null, scroll.ScrollableHeight, null, true); };
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
            // Dropped onto the field's text: the images' references go where they land (the core places them).
            var point = e.GetPosition(draft);
            uint? at = draft.Text.Length > 0 && point.X >= 0 && point.Y >= 0 && point.X <= draft.ActualWidth && point.Y <= draft.ActualHeight
                ? IndexAt(draft, point) : null;
            var inputs = (await e.DataView.GetStorageItemsAsync()).OfType<Windows.Storage.StorageFile>().Select(f => new ChatImageInput(f.Name, f.Path, null)).ToList();
            await AttachDropped(inputs, at);
        };
        Update();
    }

    /// <summary>The text field without a box of its own: the composer's rounded field is its box.</summary>
    static void Plain(TextBox box)
    {
        var none = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        foreach (var state in new[] { "", "PointerOver", "Focused", "Disabled" })
        {
            box.Resources["TextControlBackground" + state] = none;
            box.Resources["TextControlBorderBrush" + state] = none;
        }
        box.Resources["TextControlBorderThemeThickness"] = new Thickness(0);
        box.Resources["TextControlBorderThemeThicknessFocused"] = new Thickness(0);
    }

    /// <summary>Send: a filled arrow-up circle, the accent when it can send and grey when not; its title is its name.</summary>
    void RoundSend()
    {
        send.Width = send.Height = 28;
        send.Padding = new Thickness(0);
        send.CornerRadius = new CornerRadius(14);
        send.VerticalAlignment = VerticalAlignment.Bottom;
        send.Margin = new Thickness(0, 0, 0, 4);
        send.Content = new FontIcon { Glyph = "", FontSize = 13 };
        // The accent button: the accent while it can send, the system's grey while it cannot.
        send.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
    }

    static Brush Res(string key) => Theme.Res(key);

    static TextBlock Faint(string text, double size = Theme.Secondary) => new() { Text = text, FontSize = size, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap };

    /// <summary>The channel as last read with the page.</summary>
    SyncChannel? Channel => readChannel;

    string? DeviceId => (place as LibraryPlace.Direct)?.DeviceId;

    /// <summary>The model last drawn (QA).</summary>
    internal ChatPageModel? Model => model;

    /// <summary>Reads the page and redraws it when its signature changed (on every library refresh), off the UI thread.</summary>
    public void Update() => _ = Read();

    /// <summary>
    /// What <see cref="Update"/> does, to wait for: done once a read that began after this call is drawn. One read at a
    /// time; all those asked for meanwhile are served by the one that follows it, however often the library refreshes.
    /// </summary>
    internal Task Read()
    {
        var done = new TaskCompletionSource();
        readsAsked.Add(done);
        if (!reading) _ = ReadAndDraw();
        return done.Task;
    }

    async Task ReadAndDraw()
    {
        reading = true;
        while (readsAsked.Count > 0)
        {
            var served = readsAsked;
            readsAsked = [];
            try
            {
                var title = app.LibraryWindowIfCreated?.Pane.Model?.Title ?? "";
                var known = members;
                var (m, channel) = await Task.Run(() => ReadPage(title, known));
                readChannel = channel;
                model = m;
                // No model: not a conversation, or its channel is gone; the library says why.
                if (m is not null && m.Signature != drawn)
                {
                    drawn = m.Signature;
                    HangWatch.Doing = "drawing the conversation";
                    Draw(m);
                    HangWatch.Doing = "";
                }
            }
            catch (Exception e)
            {
                HangWatch.Doing = "";
                AppLog.Write($"chat page failed: {e.Message}");
            }
            foreach (var done in served) done.SetResult();
        }
        reading = false;
    }

    /// <summary>
    /// The page's reads and its model, on a thread of the pool: they can wait for the disk (what the page shows counts as
    /// read, and that is saved) or for a lock a save holds, and the UI thread never waits for the core.
    /// </summary>
    (ChatPageModel? Model, SyncChannel? Channel) ReadPage(string title, SyncMember[] known)
    {
        if (QaSlowReadMs > 0) Thread.Sleep(QaSlowReadMs);
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
        // To the minute: the cards' times change only with the day (the Mac's).
        var now = (ulong)(DateTimeOffset.UtcNow.ToUnixTimeSeconds() / 60 * 60);
        return (SeedwellFfiMethods.ChatPageModel(new ChatPageInput(place, title, channel, topic, direct, read, known, now), new Clock(), App.HostWindows), channel);
    }

    void Redraw() => _ = Redrawn();

    /// <summary>Redraws whatever the signature says (the composer's own state changed); done once it is drawn.</summary>
    Task Redrawn()
    {
        drawn = "";
        return Read();
    }

    // ------------------------------------------------------------------------------------------------ drawing

    bool Dark => root.ActualTheme == ElementTheme.Dark;

    double Scale => root.XamlRoot?.RasterizationScale ?? 1;

    /// <summary>The width of the column the messages stand in: a channel's at most 820, a private conversation's the page's.</summary>
    double ColumnWidth(bool bubbles)
    {
        var page = Math.Max(280, (scroll.ActualWidth > 0 ? scroll.ActualWidth : 900) - 40);
        return bubbles ? page : Math.Min(ColumnMax, page);
    }

    void Draw(ChatPageModel m)
    {
        DrawToolbar(m.Toolbar);
        cards.Children.Clear();
        shown.Clear();
        drawnWidth = scroll.ActualWidth;
        // A channel's column is centred; the composer stands under it.
        cards.MaxWidth = composer.MaxWidth = m.Bubbles ? double.PositiveInfinity : ColumnMax + 40;
        if (m.Empty is string empty)
        {
            var e = Faint(empty, Theme.Body);
            e.HorizontalAlignment = HorizontalAlignment.Center;
            e.Margin = new Thickness(0, 40, 0, 0);
            cards.Children.Add(e);
        }
        bool first = true;
        foreach (var card in m.Messages)
        {
            var c = m.Bubbles ? Bubble(card) : Block(card);
            // Apart above a run's first message; a run's messages touch (a little air between bubbles).
            c.Margin = new Thickness(0, first ? 4 : card.StartsRun ? Theme.RunGap : m.Bubbles ? 2 : Theme.InRunGap, 0, 0);
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
            // The agents mark: the agent symbol, the tag's help as its tooltip.
            var mark = new FontIcon { Glyph = AgentGlyph, FontSize = 15, Margin = new Thickness(6, 0, 6, 0), VerticalAlignment = VerticalAlignment.Center, Foreground = Res("TextFillColorSecondaryBrush") };
            Help(mark, tag.Help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(mark, tag.Text);
            toolbar.Children.Add(mark);
        }
        if (t.Members is ChatPageButton members)
        {
            var b = ToolbarButton("", members);
            FlyoutBase.SetAttachedFlyout(b, membersFlyout);
            toolbar.Children.Add(b);
        }
        if (t.Share is ChatPageButton share)
        {
            toolbar.Children.Add(ToolbarButton("", share));
        }
        var more = Glyph("", 15, new Thickness(8, 6, 8, 6));
        more.Flyout = Menu(t.Menu.Entries);
        Help(more, t.Menu.Help);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(more, t.Menu.Help);
        toolbar.Children.Add(more);
    }

    /// <summary>A toolbar symbol: its help is its tooltip, its title its accessibility name.</summary>
    Button ToolbarButton(string glyph, ChatPageButton b)
    {
        var button = Glyph(glyph, 15, new Thickness(8, 6, 8, 6));
        Help(button, b.Help.Length > 0 ? b.Help : b.Title);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, b.AccessibilityLabel ?? b.Title);
        button.Click += (_, _) => Press(b);
        return button;
    }

    /// <summary>A symbol on no background (the toolbar's, the hover row's, the composer's).</summary>
    static Button Glyph(string glyph, double size, Thickness padding, Brush? ink = null) => new()
    {
        Content = new FontIcon { Glyph = glyph, FontSize = size, Foreground = ink ?? Res("TextFillColorSecondaryBrush") },
        Padding = padding,
        MinWidth = 0,
        MinHeight = 0,
        VerticalAlignment = VerticalAlignment.Center,
        Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
        BorderThickness = new Thickness(0),
    };

    /// <summary>The colour of a message's edge, and of a quote's: the accent, the muted accent, or the person's.</summary>
    Windows.UI.Color EdgeColor(ChatAuthorKind who, uint? palette) => who switch
    {
        ChatAuthorKind.Mine => Theme.Accent,
        ChatAuthorKind.OurAgent => Theme.MutedAccent,
        _ => Theme.Person(palette ?? 0, who == ChatAuthorKind.TheirAgent, Dark),
    };

    /// <summary>
    /// A channel's message: a block with its author's left edge (the user's tinted with the accent), the run's ends
    /// rounded; under a run's last message its meta line with the controls at the right end (kept while hidden); inside a
    /// run the controls float over the boundary to the next message.
    /// </summary>
    FrameworkElement Block(ChatMessageCard c)
    {
        var last = c.Meta is not null;
        var mine = c.Who == ChatAuthorKind.Mine;
        var ink = new MarkdownInk(OpenLink, i => ShowImage(c.Id, (int)i));
        var content = new StackPanel { Spacing = 6 };
        if (c.ReplyQuote is ChatQuote q) content.Children.Add(Quote(q, false));
        var gallery = AddImages(content, c, ColumnWidth(false) - Edge - 2 * BlockPadX, HorizontalAlignment.Center);
        if (Body(c.Body, ink) is UIElement body) content.Children.Add(body);
        if (c.Files.Length > 0) content.Children.Add(attachments.Files(c.Place, c.Files, Redraw));
        if (Tags(c) is UIElement tags) content.Children.Add(tags);
        var top = c.StartsRun ? 6 : 0;
        var bottom = last ? 6 : 0;
        Brush rest = mine ? Theme.Brush(Theme.Accent, 0.07) : new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        Brush hover = mine ? Theme.Brush(Theme.Accent, 0.11) : Theme.Faint(0.045);
        var block = new Border
        {
            Child = content,
            Padding = new Thickness(BlockPadX, 7, BlockPadX, 7),
            BorderThickness = new Thickness(Edge, 0, 0, 0),
            BorderBrush = Theme.Brush(EdgeColor(c.Who, c.Palette)),
            CornerRadius = new CornerRadius(top, top, bottom, bottom),
            Background = rest,
        };
        var pill = Controls(c);
        var item = new Grid();
        item.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        item.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var cell = new Grid();
        cell.Children.Add(block);
        item.Children.Add(cell);
        pill.HorizontalAlignment = HorizontalAlignment.Right;
        if (c.Meta is ChatMessageMeta meta)
        {
            var row = new Grid { Margin = new Thickness(Edge + BlockPadX, 4, 0, 0), MinHeight = 18 };
            row.Children.Add(Meta(meta, c.Who));
            pill.VerticalAlignment = VerticalAlignment.Center;
            row.Children.Add(pill);
            Grid.SetRow(row, 1);
            item.Children.Add(row);
        }
        else
        {
            // Over the boundary to the next message, centred on it; it never pushes the next one down.
            pill.VerticalAlignment = VerticalAlignment.Bottom;
            pill.Margin = new Thickness(0, 0, 8, 0);
            pill.RenderTransform = new TranslateTransform { Y = 10 };
            cell.Children.Add(pill);
        }
        void Hover(bool on)
        {
            pill.Opacity = on ? 1 : 0;
            block.Background = on ? hover : rest;
            Canvas.SetZIndex(item, on ? 1 : 0);
        }
        item.PointerEntered += (_, _) => Hover(true);
        item.PointerExited += (_, _) => Hover(false);
        shown[c.Id] = new Shown(item, Hover, () => Light(block, rest), gallery);
        return item;
    }

    /// <summary>
    /// A private conversation's message: a bubble (the user's on the right in the accent, their agents' in the muted
    /// accent, the other person's on the left in grey); images above it and file cards under it, outside it, on its
    /// side (an image- or file-only message has no bubble); the meta line under the run's last bubble with the controls
    /// beside it on the side away from the edge; inside a run the controls hang under the bubble's far end.
    /// </summary>
    FrameworkElement Bubble(ChatMessageCard c)
    {
        var right = c.Who is ChatAuthorKind.Mine or ChatAuthorKind.OurAgent;
        var side = right ? HorizontalAlignment.Right : HorizontalAlignment.Left;
        var ink = new MarkdownInk(OpenLink, i => ShowImage(c.Id, (int)i), OnAccent: right);
        var stack = new StackPanel { Spacing = 4, HorizontalAlignment = side };
        var gallery = AddImages(stack, c, Math.Min(BubbleMax, ColumnWidth(true)), side);
        var text = new StackPanel { Spacing = 6 };
        if (c.ReplyQuote is ChatQuote q) text.Children.Add(Quote(q, right));
        if (Body(c.Body, ink) is UIElement body) text.Children.Add(body);
        Border? bubble = null;
        Brush rest = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        if (text.Children.Count > 0)
        {
            rest = c.Who switch
            {
                ChatAuthorKind.Mine => Theme.Brush(Theme.Accent),
                ChatAuthorKind.OurAgent => Theme.Brush(Theme.MutedAccent),
                _ => Theme.Faint(0.08),
            };
            bubble = new Border { Child = text, Padding = new Thickness(12, 7, 12, 7), CornerRadius = new CornerRadius(Theme.BubbleRadius), Background = rest, MaxWidth = BubbleMax, HorizontalAlignment = side };
            stack.Children.Add(bubble);
        }
        if (c.Files.Length > 0)
        {
            var files = attachments.Files(c.Place, c.Files, Redraw);
            files.HorizontalAlignment = side;
            stack.Children.Add(files);
        }
        if (Tags(c) is FrameworkElement tags)
        {
            tags.HorizontalAlignment = side;
            stack.Children.Add(tags);
        }
        var pill = Controls(c);
        var item = new Grid();
        item.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        item.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var cell = new Grid { HorizontalAlignment = side };
        cell.Children.Add(stack);
        item.Children.Add(cell);
        if (c.Meta is ChatMessageMeta meta)
        {
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = side, Margin = new Thickness(6, 4, 6, 0), MinHeight = 18 };
            var line = Meta(meta, c.Who);
            if (right) { row.Children.Add(pill); row.Children.Add(line); }
            else { row.Children.Add(line); row.Children.Add(pill); }
            Grid.SetRow(row, 1);
            item.Children.Add(row);
        }
        else
        {
            pill.HorizontalAlignment = right ? HorizontalAlignment.Left : HorizontalAlignment.Right;
            pill.VerticalAlignment = VerticalAlignment.Bottom;
            pill.RenderTransform = new TranslateTransform { X = right ? -8 : 8, Y = 12 };
            cell.Children.Add(pill);
        }
        void Hover(bool on)
        {
            pill.Opacity = on ? 1 : 0;
            Canvas.SetZIndex(item, on ? 1 : 0);
        }
        item.PointerEntered += (_, _) => Hover(true);
        item.PointerExited += (_, _) => Hover(false);
        shown[c.Id] = new Shown(item, Hover, () => { if (bubble is not null) Light(bubble, rest); }, gallery);
        return item;
    }

    /// <summary>A message's images at its top (one, or a gallery); returns what switches its gallery.</summary>
    Action<int> AddImages(Panel into, ChatMessageCard c, double maxWidth, HorizontalAlignment side)
    {
        if (c.Images.Length == 0) return _ => { };
        var id = c.Id;
        var (view, show) = attachments.Images(c.Place, c.Images, c.Gallery, Math.Max(120, maxWidth), Scale, side,
            galleries.GetValueOrDefault(id), i => galleries[id] = i, Redraw);
        into.Children.Add(view);
        return show;
    }

    /// <summary>Lights a message for a moment (a reply quote jumped to it): the accent at 16 % behind it, about 1.4 s.</summary>
    void Light(Border target, Brush rest)
    {
        target.Background = Theme.Brush(Theme.Accent, 0.16);
        var timer = root.DispatcherQueue.CreateTimer();
        timer.Interval = TimeSpan.FromSeconds(1.4);
        timer.IsRepeating = false;
        timer.Tick += (_, _) => target.Background = rest;
        timer.Start();
    }

    /// <summary>The message's body: Markdown, a mail (its banner over its text), JSON, or what cannot be shown.</summary>
    UIElement? Body(ChatMessageBody b, MarkdownInk ink)
    {
        switch (b)
        {
            case ChatMessageBody.Markdown md when md.Blocks.Length > 0:
                return ChatMarkdown.Blocks(md.Blocks, ink);
            case ChatMessageBody.Mail mail:
                var s = new StackPanel { Spacing = 6 };
                s.Children.Add(Banner(mail.Banner));
                if (mail.Blocks.Length > 0) s.Children.Add(ChatMarkdown.Blocks(mail.Blocks, ink));
                return s;
            case ChatMessageBody.Json json:
                var j = new TextBlock { Text = json.Text, FontFamily = Theme.MonoFont, FontSize = Theme.Mono, IsTextSelectionEnabled = true, TextWrapping = TextWrapping.Wrap };
                if (ink.OnAccent) j.Foreground = new SolidColorBrush(Microsoft.UI.Colors.White);
                return new Border { Child = j, Padding = new Thickness(10, 6, 10, 6), CornerRadius = new CornerRadius(Theme.CodeRadius), Background = Res("SubtleFillColorSecondaryBrush"), HorizontalAlignment = HorizontalAlignment.Left };
            case ChatMessageBody.Unsupported u:
                return Faint(u.Text);
            default:
                return null;
        }
    }

    /// <summary>Its tags by owner, one quiet line.</summary>
    static FrameworkElement? Tags(ChatMessageCard c)
    {
        if (c.Tags.Length == 0) return null;
        var tags = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 10 };
        foreach (var t in c.Tags)
        {
            var one = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
            one.Children.Add(new FontIcon { Glyph = t.Seen ? "" : "", FontSize = 10, Foreground = Res("TextFillColorTertiaryBrush") });
            one.Children.Add(new TextBlock { Text = t.Text, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush") });
            tags.Children.Add(one);
        }
        return tags;
    }

    /// <summary>
    /// A reply quote: an inset block with the quoted author's edge (white for the user's own on an accent bubble), the
    /// name and one line of the text; a click scrolls the original to the middle and lights it.
    /// </summary>
    FrameworkElement Quote(ChatQuote q, bool onAccent)
    {
        var white = Microsoft.UI.Colors.White;
        var edge = onAccent && q.Who == ChatAuthorKind.Mine ? white : EdgeColor(q.Who, q.Palette);
        var g = new Grid
        {
            ColumnSpacing = 8,
            Padding = new Thickness(8, 5, 10, 5),
            CornerRadius = new CornerRadius(Theme.ChipRadius),
            Background = onAccent ? Theme.Brush(white, 0.16) : Theme.Faint(0.05),
            HorizontalAlignment = HorizontalAlignment.Left,
        };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Width = Edge, RadiusX = 1.25, RadiusY = 1.25, Fill = Theme.Brush(edge) });
        var s = new StackPanel();
        s.Children.Add(new TextBlock { Text = q.Title, FontSize = Theme.Caption, Foreground = onAccent ? Theme.Brush(white, 0.85) : Res("TextFillColorSecondaryBrush") });
        s.Children.Add(new TextBlock { Text = q.Text, FontSize = Theme.Caption, MaxLines = 1, TextTrimming = TextTrimming.CharacterEllipsis, Foreground = onAccent ? Theme.Brush(white) : Res("TextFillColorPrimaryBrush") });
        Grid.SetColumn(s, 1);
        g.Children.Add(s);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(g, $"{q.Title}: {q.Text}");
        g.Tapped += (_, e) => { e.Handled = true; JumpTo(q.MessageId); };
        return g;
    }

    /// <summary>Scrolls a message to the middle and lights it (a reply quote's click).</summary>
    internal bool JumpTo(string id)
    {
        if (!shown.TryGetValue(id, out var s)) return false;
        var y = s.Item.TransformToVisual(cards).TransformPoint(new Windows.Foundation.Point(0, 0)).Y;
        scroll.ChangeView(null, Math.Max(0, y + s.Item.ActualHeight / 2 - scroll.ViewportHeight / 2), null, Quiet.On);
        s.Light();
        AppLog.Write("chat quote: jumped to the original");
        return true;
    }

    /// <summary>An image reference's chip: the message's gallery shows that image, and the message's top comes into view.</summary>
    void ShowImage(string id, int index)
    {
        galleries[id] = index;
        if (!shown.TryGetValue(id, out var s)) return;
        s.Gallery(index);
        var y = s.Item.TransformToVisual(cards).TransformPoint(new Windows.Foundation.Point(0, 0)).Y;
        if (y < scroll.VerticalOffset || y > scroll.VerticalOffset + scroll.ViewportHeight - 40)
        {
            scroll.ChangeView(null, Math.Max(0, y - 8), null, Quiet.On);
        }
    }

    /// <summary>
    /// Who wrote a run, one quiet line under it: a round initial (another person), the same circle with the agent symbol
    /// (another person's agent) or the bare agent symbol (your agent), then the name and the detail; its tooltip the core's.
    /// </summary>
    static FrameworkElement Meta(ChatMessageMeta m, ChatAuthorKind who)
    {
        var tertiary = Res("TextFillColorTertiaryBrush");
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 5, VerticalAlignment = VerticalAlignment.Center, HorizontalAlignment = HorizontalAlignment.Left };
        Border Circle(UIElement inner) => new() { Width = 13, Height = 13, CornerRadius = new CornerRadius(6.5), BorderThickness = new Thickness(1), BorderBrush = tertiary, Child = inner, VerticalAlignment = VerticalAlignment.Center };
        switch (who)
        {
            case ChatAuthorKind.Theirs:
                s.Children.Add(Circle(new TextBlock { Text = m.Initial, FontSize = 7.5, FontWeight = FontWeights.SemiBold, Foreground = tertiary, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center }));
                break;
            case ChatAuthorKind.TheirAgent:
                s.Children.Add(Circle(new FontIcon { Glyph = AgentGlyph, FontSize = 7, Foreground = tertiary }));
                break;
            case ChatAuthorKind.OurAgent:
                s.Children.Add(new FontIcon { Glyph = AgentGlyph, FontSize = 11, Foreground = tertiary, VerticalAlignment = VerticalAlignment.Center });
                break;
        }
        var line = new TextBlock { FontSize = Theme.Caption, Foreground = tertiary, VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
        line.Inlines.Add(new Run { Text = m.Name });
        if (m.Detail.Length > 0) line.Inlines.Add(new Run { Text = " · " + m.Detail });
        s.Children.Add(line);
        Help(s, m.Help);
        return s;
    }

    /// <summary>The time, Tag (its menu) and Reply, small and quiet on a faint capsule, hidden until the pointer is on the message.</summary>
    Border Controls(ChatMessageCard c)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 0, VerticalAlignment = VerticalAlignment.Center };
        s.Children.Add(new TextBlock { Text = c.Time, FontSize = Theme.Caption, Foreground = Res("TextFillColorTertiaryBrush"), VerticalAlignment = VerticalAlignment.Center, Margin = new Thickness(6, 0, 4, 1) });
        if (c.TagMenu is ChatPageMenu tagMenu)
        {
            var tag = Glyph("", 11, new Thickness(5, 3, 5, 3));
            tag.Flyout = Menu(tagMenu.Entries);
            Help(tag, tagMenu.Help);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(tag, tagMenu.Help);
            s.Children.Add(tag);
        }
        if (c.Reply is ChatPageButton reply)
        {
            var b = Glyph("", 11, new Thickness(5, 3, 5, 3));
            Help(b, reply.Help.Length > 0 ? reply.Help : reply.Title);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, reply.AccessibilityLabel ?? reply.Title);
            b.Click += (_, _) => Press(reply);
            s.Children.Add(b);
        }
        return new Border
        {
            Child = s,
            Height = 22,
            Padding = new Thickness(2, 0, 2, 0),
            CornerRadius = new CornerRadius(11),
            Background = Res("SolidBackgroundFillColorSecondaryBrush"),
            BorderBrush = Res("CardStrokeColorDefaultBrush"),
            BorderThickness = new Thickness(1),
            Opacity = 0,
        };
    }

    /// <summary>A text button of the page (a gone device's Remove from Channel…).</summary>
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

    /// <summary>
    /// The composer: the pending sends over one rounded field (a faint fill, the accent border while it has focus) that
    /// holds the reply bar, the attachments, then attach, the text and Send; tooltips only, no captions.
    /// </summary>
    void DrawComposer(ChatComposer c)
    {
        if (!c.CanPost)
        {
            composer.Children.Clear();
            var cannot = Faint(c.CannotPost ?? "");
            cannot.HorizontalAlignment = HorizontalAlignment.Center;
            cannot.Margin = new Thickness(0, 6, 0, 4);
            composer.Children.Add(cannot);
            return;
        }
        // The field itself stays in the page, only what is around it is made anew: out of the page, the text field would
        // lose the focus (and what an input method is composing) with every redraw.
        AllBut(composer, field);
        if (composer.Children.Count == 0) composer.Children.Add(field);
        var place = model?.Place ?? "";
        if (c.Pending.Length > 0) composer.Children.Insert(0, attachments.Pending(place, c.Pending, Redraw));
        AllBut(inside, row);
        if (replyTo is string id && model?.Messages.FirstOrDefault(m => m.Id == id) is ChatMessageCard replying)
        {
            inside.Children.Insert(inside.Children.Count - 1, ReplyBar(replying.Replying, c.ReplyCancelHelp));
        }
        if (attached.Count > 0)
        {
            var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            for (int n = 0; n < attached.Count; n++)
            {
                var at = n;
                chips.Children.Add(attachments.Draft(attached[n].Draft, () => RemoveAttached(at)));
            }
            inside.Children.Insert(inside.Children.Count - 1, new ScrollViewer { Content = chips, Padding = new Thickness(10, 8, 10, 0), HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, HorizontalScrollMode = ScrollMode.Auto });
        }
        if (c.Attach && attach is null)
        {
            attach = Glyph("", 15, new Thickness(8, 7, 6, 7));
            attach.VerticalAlignment = VerticalAlignment.Bottom;
            attach.Margin = new Thickness(0, 0, 0, 2);
            Help(attach, attachments.Texts.AttachHelp);
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(attach, attachments.Texts.Attach);
            attach.Click += (_, _) => PickAttachments();
            row.Children.Add(attach);
        }
        if (attach is not null) attach.Visibility = c.Attach ? Visibility.Visible : Visibility.Collapsed;
        draft.PlaceholderText = c.Placeholder;
        send.IsEnabled = CanSend();
        Help(send, c.SendHelp);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(send, c.SendTitle);
        field.Background = Theme.Faint(0.045);
        field.BorderBrush = draft.FocusState != FocusState.Unfocused ? Theme.Brush(Theme.Accent) : Theme.Faint(0.10);
    }

    /// <summary>Takes everything out of <paramref name="panel"/> but <paramref name="kept"/>.</summary>
    static void AllBut(Panel panel, UIElement kept)
    {
        for (int i = panel.Children.Count - 1; i >= 0; i--)
        {
            if (panel.Children[i] != kept) panel.Children.RemoveAt(i);
        }
    }

    /// <summary>The reply being written, docked on top of the field: the accent bar, "Replying to Kari", one line of it, and ×.</summary>
    UIElement ReplyBar(ChatQuote q, string cancelHelp)
    {
        var g = new Grid { ColumnSpacing = 8, Padding = new Thickness(12, 8, 6, 2) };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.ColumnDefinitions.Add(new ColumnDefinition());
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(new Microsoft.UI.Xaml.Shapes.Rectangle { Width = 3, RadiusX = 1.5, RadiusY = 1.5, Fill = Theme.Brush(Theme.Accent) });
        var s = new StackPanel();
        s.Children.Add(new TextBlock { Text = q.Title, FontSize = Theme.Caption, FontWeight = FontWeights.SemiBold, Foreground = Res("AccentTextFillColorPrimaryBrush") });
        s.Children.Add(new TextBlock { Text = q.Text, FontSize = Theme.Caption, MaxLines = 1, TextTrimming = TextTrimming.CharacterEllipsis, Foreground = Res("TextFillColorSecondaryBrush") });
        Grid.SetColumn(s, 1);
        g.Children.Add(s);
        var close = Glyph("", 10, new Thickness(6));
        Help(close, cancelHelp);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(close, cancelHelp);
        close.Click += (_, _) => CancelReply();
        Grid.SetColumn(close, 2);
        g.Children.Add(close);
        return g;
    }

    void CancelReply()
    {
        replyTo = null;
        Redraw();
    }

    /// <summary>A chip removed from the composer: an image's references go from the text with it, the later ones renumbered (the core's).</summary>
    void RemoveAttached(int at)
    {
        if (at >= attached.Count) return;
        if (!attached[at].Draft.File)
        {
            var index = attached.Take(at).Count(a => !a.Draft.File);
            draft.Text = SeedwellFfiMethods.ChatComposerWithoutImage(draft.Text, (uint)index);
        }
        attached.RemoveAt(at);
        Redraw();
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
            var who = new StackPanel { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
            who.Children.Add(name);
            if (r.Removal is string removal)
            {
                who.Children.Add(Faint(removal, Theme.Caption));
            }
            Grid.SetColumn(who, 1);
            g.Children.Add(who);
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
        if (b.Question is SyncQuestion q) Ask(q, b.Command); else _ = Run(b.Command);
    }

    /// <summary>An item: its dialog first (the sidebar's chat dialogs, which run what its action does), else its command.</summary>
    void RunItem(ChatPageMenuEntry.Item it)
    {
        if (it.Dialog is ChatDialog d)
        {
            app.ChatDialogs.Show(d, Channel, DeviceId);
            return;
        }
        _ = Run(it.Command);
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
        try { if (await d.ShowAsync() == ContentDialogResult.Primary) await Run(command); }
        catch (Exception e) { AppLog.Write($"chat question failed: {e.Message}"); }
    }

    /// <summary>What the host does for each command (the core's table); failures are alerts with the core's sentence.</summary>
    /// <remarks>It comes to the work it starts (QA awaits that, never a fixed time).</remarks>
    internal Task Run(ChatPageCommand command)
    {
        AppLog.Write($"chat page: {command.GetType().Name}");
        var channel = Channel;
        var call = Task.CompletedTask;
        switch (command)
        {
            case ChatPageCommand.ShowMembers:
                call = LoadMembers(open: true);
                break;
            case ChatPageCommand.Share:
                if (channel is not null) ShareDialog.Show(app, channel.Id);
                break;
            case ChatPageCommand.SetMuted mute when channel is not null:
                return Call(() => app.Core.ChatSetMuted(channel.Id, mute.Muted), e => new SyncAlert(e, ""));
            case ChatPageCommand.SetAgents agents when channel is not null:
                call = Call(() => app.Core.ChatSetAgents(channel.Id, agents.Open), e => new SyncAlert(SeedwellFfiMethods.ChatAgentsMenu(channel).FailureTitle, e));
                break;
            case ChatPageCommand.SetFollow follow when channel is not null:
                call = Call(() => app.Core.SyncSetFollow(channel.Id, follow.On), e => new SyncAlert(e, ""));
                break;
            case ChatPageCommand.SetDirectAgents access:
                call = Call(() => Task.Run(() => app.Core.ChatSetDirectAgents(access.Access)), e => new SyncAlert(e, ""));
                break;
            case ChatPageCommand.SetRole sr when channel is not null:
                var who = members.FirstOrDefault(m => m.DeviceId == sr.DeviceId)?.Name ?? "";
                call = Call(() => app.Core.SyncSetRole(channel.Id, sr.DeviceId, sr.Role), e => SeedwellFfiMethods.SyncSetRoleFailed(who, e), () => LoadMembers(false));
                break;
            case ChatPageCommand.RemoveDevice rd when channel is not null:
                var name = members.FirstOrDefault(m => m.DeviceId == rd.DeviceId)?.Name ?? "";
                call = Call(() => app.Core.SyncRemoveDevice(channel.Id, rd.DeviceId), e => SeedwellFfiMethods.SyncRemoveDeviceFailed(name, channel.Name, e), () => LoadMembers(false));
                break;
            case ChatPageCommand.MessagePrivately mp:
                membersFlyout.Hide();
                app.Library.NoteDirectName(mp.DeviceId, mp.Name);
                app.ShowPlace(new LibraryPlace.Direct(mp.DeviceId));
                return Task.CompletedTask;
            case ChatPageCommand.Reply r:
                replyTo = r.MessageId;
                FocusAfter(Redrawn());
                return Task.CompletedTask;
            case ChatPageCommand.SetTags tags:
                return SendText(SeedwellFfiMethods.ChatTagNote(tags.MessageId, tags.Tags), null);
            case ChatPageCommand.MoveToGroup to when channel is not null:
                // Into a sidebar group (null: out of its group); the sidebar is read again after it.
                call = Call(() => app.Core.ChatSetGroup(channel.Id, to.GroupId), e => new SyncAlert(SeedwellFfiMethods.ChatGroupFailureTitle(true), e), () => { app.LibraryWindowIfCreated?.Pane.Refresh(); return Task.CompletedTask; });
                break;
            default:
                // WakeCommand, NewTopic, Rename, Leave, DeleteConversation and NewGroup come with their dialog (RunItem).
                AppLog.Write($"chat page: {command.GetType().Name} has no dialog here");
                break;
        }
        Redraw();
        return call;
    }

    /// <summary>The field gets the focus once the page is drawn (a redraw takes the field out and puts it back).</summary>
    async void FocusAfter(Task drawing)
    {
        await drawing;
        draft.Focus(FocusState.Programmatic);
    }

    /// <summary>A core call off the UI thread; back on it, a failure's alert, then <paramref name="done"/> and the page drawn again: done when all that is.</summary>
    async Task Call(Func<Task> work, Func<string, SyncAlert> failed, Func<Task>? done = null)
    {
        try
        {
            await Task.Run(work);
        }
        catch (Exception e)
        {
            app.Alert(failed(CoreCalls.Message(e)));
        }
        if (done is not null) await done();
        await Redrawn();
    }

    /// <summary>Reads the channel's members off the UI thread, then draws the panel (opening it when asked): done when it is drawn.</summary>
    async Task LoadMembers(bool open)
    {
        if (Channel is not SyncChannel channel) return;
        try { members = await Task.Run(() => app.Core.SyncMembers(channel.Id)); }
        catch (Exception e) { AppLog.Write($"chat members failed: {CoreCalls.Message(e)}"); }
        await Redrawn();
        if (model?.Members is ChatMembersPanel panel)
        {
            membersFlyout.Content = Members(panel);
            if (open && toolbar.Children.OfType<Button>().FirstOrDefault(b => FlyoutBase.GetAttachedFlyout(b) == membersFlyout) is Button anchor)
            {
                membersFlyout.ShowAt(anchor);
            }
        }
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

    bool CanSend() => !sending && SeedwellFfiMethods.ChatComposerCanSend(draft.Text, (uint)attached.Count);

    /// <summary>Return sends (the core's rule for the modifiers); Escape cancels the reply being written, else goes on.</summary>
    void DraftKey(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == Windows.System.VirtualKey.Escape)
        {
            if (Escape()) e.Handled = true;
            return;
        }
        if (e.Key != Windows.System.VirtualKey.Enter) return;
        bool Down(Windows.System.VirtualKey k) => Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        if (SeedwellFfiMethods.ChatComposerKey(Down(Windows.System.VirtualKey.Shift), Down(Windows.System.VirtualKey.Menu)) == ChatComposerKey.Send)
        {
            e.Handled = true;
            _ = Send();
        }
    }

    /// <summary>Escape in the field: cancels the reply being written (true), else it goes on.</summary>
    internal bool Escape()
    {
        if (replyTo is null) return false;
        CancelReply();
        return true;
    }

    /// <summary>
    /// Paste: an image or files on the clipboard become attachments; text goes into the field as usual. An image pasted
    /// into a field that has text leaves its reference at the caret (<c>chat_image_marker</c>).
    /// </summary>
    async void Paste(object sender, TextControlPasteEventArgs e)
    {
        if (model?.Composer is not { Attach: true }) return;
        var clip = Windows.ApplicationModel.DataTransfer.Clipboard.GetContent();
        int? caret = draft.Text.Length > 0 ? draft.SelectionStart : null;
        if (clip.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.StorageItems))
        {
            e.Handled = true;
            var items = await clip.GetStorageItemsAsync();
            foreach (var item in items)
            {
                // A reference only for one image: several would land in an order the checks finish in.
                if (item is Windows.Storage.StorageFile f) _ = Attach(new ChatImageInput(f.Name, f.Path, null), items.Count == 1 ? caret : null);
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
            _ = Attach(new ChatImageInput(attachments.Texts.PastedName, null, bytes), caret);
        }
    }

    async void PickAttachments()
    {
        foreach (var path in await app.PickFiles(attachments.Texts.Extensions))
        {
            _ = Attach(new ChatImageInput(Path.GetFileName(path), path, null));
        }
    }

    /// <summary>
    /// An image or file for the next message: the core checks it and makes its chip; a refusal is an alert. With
    /// <paramref name="marker"/> (a paste into text), an image's reference goes into the text there.
    /// </summary>
    /// <remarks>Done once the chip is drawn (or the refusal told).</remarks>
    internal async Task Attach(ChatImageInput input, int? marker = null)
    {
        if (attached.Count >= attachments.Texts.MaxImages)
        {
            app.Alert(new SyncAlert(attachments.Texts.TooMany, ""));
            return;
        }
        try
        {
            var d = await Task.Run(() => app.Core.ChatImageCheck(input));
            attached.Add((input, d));
            AppLog.Write($"chat attach: {d.Caption}");
            if (marker is int at && !d.File)
            {
                var reference = SeedwellFfiMethods.ChatImageMarker((uint)(attached.Count(a => !a.Draft.File) - 1));
                var where = Math.Min(at, draft.Text.Length);
                draft.Text = draft.Text.Insert(where, reference);
                draft.SelectionStart = where + reference.Length;
            }
        }
        catch (Exception e)
        {
            app.Alert(new SyncAlert(CoreCalls.Message(e), ""));
        }
        await Redrawn();
    }

    /// <summary>
    /// The character a point over the field falls on (UTF-16 index): the nearest character edge on the line under it,
    /// from the field's own layout (GetRectFromCharacterIndex); the end of the text past the last line.
    /// </summary>
    static uint IndexAt(TextBox box, Windows.Foundation.Point p)
    {
        int best = box.Text.Length;
        double bestScore = double.MaxValue;
        for (int i = 0; i <= box.Text.Length; i++)
        {
            var r = i < box.Text.Length ? box.GetRectFromCharacterIndex(i, false) : box.GetRectFromCharacterIndex(i - 1, true);
            double dy = p.Y < r.Top ? r.Top - p.Y : p.Y > r.Bottom ? p.Y - r.Bottom : 0;
            double score = dy * 10_000 + Math.Abs(p.X - r.X);
            if (score < bestScore) { bestScore = score; best = i; }
        }
        return (uint)best;
    }

    /// <summary>
    /// Files dropped on the conversation: each checked by the core and added; with <paramref name="at"/> (they landed on
    /// the field's text) the images' references go in there (<c>chat_composer_with_images</c>: spacing and caret the core's).
    /// </summary>
    internal async Task AttachDropped(IReadOnlyList<ChatImageInput> inputs, uint? at)
    {
        var first = (uint)attached.Count(a => !a.Draft.File);
        uint images = 0;
        foreach (var input in inputs)
        {
            if (attached.Count >= attachments.Texts.MaxImages)
            {
                app.Alert(new SyncAlert(attachments.Texts.TooMany, ""));
                break;
            }
            try
            {
                var d = await Task.Run(() => app.Core.ChatImageCheck(input));
                attached.Add((input, d));
                AppLog.Write($"chat attach: {d.Caption}");
                if (!d.File) images++;
            }
            catch (Exception e)
            {
                app.Alert(new SyncAlert(e is CoreException ce ? SeedwellFfiMethods.CoreErrorMessage(ce) : e.Message, ""));
            }
        }
        if (at is uint point && images > 0 && SeedwellFfiMethods.ChatComposerWithImages(draft.Text, first, images, new ChatTextRange(point, 0)) is ChatComposerEdit edit)
        {
            draft.Text = edit.Text;
            draft.SelectionStart = (int)Math.Min(edit.Caret, (uint)edit.Text.Length);
        }
        Redraw();
    }

    /// <summary>A click on Send: one goes on typing in the field (the Mac's buttons never take the keyboard focus).</summary>
    Task<bool> SendClicked()
    {
        draft.Focus(FocusState.Programmatic);
        return Send();
    }

    /// <summary>Send: the field and the attachments, to the conversation (as a reply when one is being written).</summary>
    internal async Task<bool> Send()
    {
        if (!CanSend() || model is not ChatPageModel m) return false;
        var body = SeedwellFfiMethods.ChatComposerBody(draft.Text);
        var inputs = attached.Select(a => a.Input).ToList();
        bool ok;
        // Nothing is typed into what is on its way (read only keeps the focus; disabled would lose it).
        draft.IsReadOnly = true;
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
            draft.IsReadOnly = false;
            return false;
        }
        draft.IsReadOnly = false;
        if (ok)
        {
            draft.Text = "";
            attached.Clear();
            replyTo = null;
        }
        // Done once the page is drawn again (QA reads it right after).
        await Redrawn();
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
            var message = e is CoreException ce ? SeedwellFfiMethods.CoreErrorMessage(ce) : e.Message;
            AppLog.Write($"chat: send failed: {message}");
            app.Alert(new SyncAlert(message, ""));
            return false;
        }
    }

    // --------------------------------------------------------------------------------------------------- QA

    /// <summary>QA: every read of the page takes this much longer, as on a slow disk: the UI thread must not notice.</summary>
    internal static int QaSlowReadMs { get; set; }

    internal void QaType(string text) => draft.Text = text;

    /// <summary>QA: the field takes the focus with its caret at <paramref name="caret"/>, as someone typing there has it.</summary>
    internal void QaFocus(int caret)
    {
        draft.Focus(FocusState.Programmatic);
        draft.SelectionStart = caret;
    }

    /// <summary>QA: a click on Send: the button takes the focus as a click gives it, then its click's call.</summary>
    internal Task<bool> QaClickSend()
    {
        send.Focus(FocusState.Pointer);
        return SendClicked();
    }

    /// <summary>QA: a redraw of the whole page, as a change of its signature makes.</summary>
    internal Task QaRedraw() => Redrawn();

    /// <summary>QA: the field as someone typing there has it.</summary>
    internal string QaField => $"field \"{draft.Text}\" focus {draft.FocusState != FocusState.Unfocused} caret {draft.SelectionStart}";

    /// <summary>QA: a file dropped on the field's text at <paramref name="at"/> (what a drop there does).</summary>
    internal Task QaDropAt(string path, uint at) => AttachDropped([new ChatImageInput(Path.GetFileName(path), path, null)], at);

    /// <summary>
    /// QA: a drop at a point over the field: the point just inside the leading edge of character <paramref name="ch"/>
    /// (from the field's layout), hit-tested as a real drop is (IndexAt), then dropped there. Answers the index it found.
    /// </summary>
    internal async Task<uint> QaDropOver(int ch, string path)
    {
        var r = draft.GetRectFromCharacterIndex(ch, false);
        var at = IndexAt(draft, new Windows.Foundation.Point(r.X + 0.5, r.Y + r.Height / 2));
        await AttachDropped([new ChatImageInput(Path.GetFileName(path), path, null)], at);
        return at;
    }
    internal bool SendEnabled => send.IsEnabled;

    /// <summary>The visual QA run: the ⋯ menu as a click opens it.</summary>
    internal MenuFlyout RenderMenu() => Menu(model?.Toolbar.Menu.Entries ?? []);
    internal void QaReply(int n)
    {
        if (model?.Messages.ElementAtOrDefault(n)?.Reply is ChatPageButton b) _ = Run(b.Command);
    }
    internal Task QaMembers() => Run(new ChatPageCommand.ShowMembers());

    /// <summary>QA: the member rows of the panel: who, the removal line, the role and its menu.</summary>
    internal string QaMemberRows() => string.Join(" || ", (model?.Members?.Rows ?? []).Select(r =>
        $"{r.DeviceId} \"{r.Name}\" removal \"{r.Removal}\" role {r.Role}, " + (r.RoleMenu is ChatPageMenu m
            ? $"menu [{string.Join(", ", m.Entries.OfType<ChatPageMenuEntry.Item>().Select(i => $"{i.Title} {(i.Enabled ? "enabled" : "disabled")}"))}]"
            : "plain")));

    /// <summary>QA: a member's menu item by its device id and title, as its click (a disabled one does not run).</summary>
    internal async Task<string> QaMemberPress(string deviceId, string title)
    {
        var row = model?.Members?.Rows.FirstOrDefault(r => r.DeviceId == deviceId);
        if (row?.RoleMenu?.Entries.OfType<ChatPageMenuEntry.Item>().FirstOrDefault(i => i.Title.StartsWith(title)) is not ChatPageMenuEntry.Item it) return "no such item";
        if (!it.Enabled) return "disabled";
        await Run(it.Command);
        return $"ran {it.Command.GetType().Name}";
    }

    /// <summary>QA: the panel's content on the theme's page colour, saved as a picture: a flyout closes when a QA launch takes the focus.</summary>
    internal async Task<string> QaMembersShot(string path)
    {
        if (model?.Members is not ChatMembersPanel panel) return "no panel";
        var sheet = new Border { Child = Members(panel), Padding = new Thickness(16), Background = Res("SolidBackgroundFillColorBaseBrush") };
        var host = new Microsoft.UI.Xaml.Controls.Primitives.Popup { XamlRoot = root.XamlRoot, Child = sheet, IsOpen = true };
        await Task.Delay(300);
        await SnapshotRenderer.SaveImage(sheet, path, true);
        host.IsOpen = false;
        return File.Exists(path) ? "panel saved" : "panel not saved";
    }

    /// <summary>QA: message <paramref name="n"/>'s controls shown (or hidden) as the pointer on it does.</summary>
    internal bool QaHover(int n, bool on)
    {
        if (model?.Messages.ElementAtOrDefault(n) is not ChatMessageCard c || !shown.TryGetValue(c.Id, out var s)) return false;
        s.Hover(on);
        return true;
    }

    /// <summary>QA: a click on message <paramref name="n"/>'s reply quote.</summary>
    internal string QaQuote(int n) => model?.Messages.ElementAtOrDefault(n)?.ReplyQuote is ChatQuote q
        ? JumpTo(q.MessageId) ? $"jumped to {model.Messages.ToList().FindIndex(m => m.Id == q.MessageId)}" : "not drawn"
        : "no quote";

    /// <summary>QA: an image reference's click on message <paramref name="n"/> (its gallery shows image <paramref name="index"/>).</summary>
    internal void QaShowImage(int n, int index)
    {
        if (model?.Messages.ElementAtOrDefault(n) is ChatMessageCard c) ShowImage(c.Id, index);
    }

    /// <summary>QA: the image message <paramref name="n"/>'s gallery shows (view state), -1 when none was chosen.</summary>
    internal int GalleryShown(int n) => model?.Messages.ElementAtOrDefault(n) is ChatMessageCard c && galleries.TryGetValue(c.Id, out var i) ? i : -1;
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
