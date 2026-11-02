using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The Share dialog on Windows (the Mac's Share window) for a synced folder, a folder not synced yet, or a channel,
/// drawn from the core's <c>SyncShareWindowModel</c> (trs-ffi sync_share.rs): the role a link gives, Single use and Link
/// expires with their menus, Include a chat, the summary, the sharing period (its countdown and Extend, or Stop sharing
/// after), and the active links with Revoke. The form is the core's value: this keeps it and edits its fields, reads
/// the links off the UI thread when it opens and after every change, and draws the model again. Copy Link runs
/// <c>sync_share_copy_link</c> off the UI thread (it syncs a new folder and makes a new chat first) and puts the link on
/// the clipboard: a secret, never shown or logged.
/// </summary>
sealed class ShareDialog
{
    readonly App app;
    readonly ContentDialog dialog;
    readonly StackPanel content = new() { Spacing = 10, MinWidth = 460 };
    readonly TextBlock failure = new() { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Foreground = Res("SystemFillColorCriticalBrush") };
    readonly DispatcherTimer countdown = new();
    SyncShareForm form;
    SyncInvite[] invites = [];
    SyncShareWindowModel model;
    bool copying;
    bool copied;

    ShareDialog(App app, XamlRoot root, SyncShareForm form)
    {
        this.app = app;
        this.form = form;
        model = Window();
        dialog = new ContentDialog
        {
            XamlRoot = root,
            Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
            DefaultButton = ContentDialogButton.Primary,
        };
        dialog.PrimaryButtonClick += async (_, e) =>
        {
            // Copy Link keeps the dialog open: the new link shows in the list.
            var deferral = e.GetDeferral();
            e.Cancel = true;
            await Copy();
            deferral.Complete();
        };
        dialog.Opened += (_, _) => Tips();
        dialog.Closed += (_, _) => countdown.Stop();
        countdown.Tick += (_, _) => Draw();
        Draw();
        LoadInvites();
    }

    /// <summary>The dialog open now (QA).</summary>
    internal static ShareDialog? Open { get; private set; }

    /// <summary>Opens the Share dialog for folder or channel <paramref name="id"/>. One at a time.</summary>
    internal static void Show(App app, string id) => Show(app, f => f.SyncShareForm(id), id);

    /// <summary>Opens it for <paramref name="path"/>, a folder not synced yet (a seedwell:// share link outside every folder).</summary>
    internal static void ShowNew(App app, string path) => Show(app, f => f.SyncShareFormNew(path), path);

    static async void Show(App app, Func<Core, SyncShareForm> make, string what)
    {
        if (Open is not null || await AddFolderDialog.Root(app) is not XamlRoot root || Open is not null)
        {
            return;
        }
        var d = new ShareDialog(app, root, make(app.Core));
        Open = d;
        AppLog.Write($"share: open on {(d.form.NewPath is null ? what : "a new folder")}");
        try { await d.dialog.ShowAsync(); } catch (Exception e) { AppLog.Write($"share failed: {e.Message}"); }
        if (Open == d)
        {
            Open = null;
        }
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Note(string text) => new() { Text = text, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Left, MaxWidth = 460 };

    static ulong Now() => (ulong)DateTimeOffset.UtcNow.ToUnixTimeSeconds();

    SyncShareWindowModel Window() => app.Core.SyncShareWindow(form, invites, Now(), new Clock());

    void LoadInvites()
    {
        if (form.Id.Length == 0)
        {
            return;
        }
        var id = form.Id;
        _ = Task.Run(() => app.Core.SyncInvites(id)).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
        {
            if (t.IsCompletedSuccessfully && form.Id == id)
            {
                invites = t.Result;
                Draw();
            }
        }));
    }

    void Change(SyncShareForm next)
    {
        form = next;
        failure.Visibility = Visibility.Collapsed;
        Draw();
    }

    void Draw()
    {
        model = Window();
        dialog.Title = AddFolderDialog.Title(model.Caption, model.Title);
        dialog.CloseButtonText = model.CloseTitle;
        dialog.PrimaryButtonText = model.CopyTitle is string copy ? (copied ? model.CopiedTitle : copy) : "";
        dialog.IsPrimaryButtonEnabled = model.CopyTitle is not null && !copying;
        // A sharing period counting down is drawn again as it runs.
        if (model.RefreshMs is uint ms)
        {
            countdown.Interval = TimeSpan.FromMilliseconds(ms);
            countdown.Start();
        }
        else
        {
            countdown.Stop();
        }

        (failure.Parent as Panel)?.Children.Remove(failure);
        content.Children.Clear();
        // Gone, or not this PC's to share: the line replaces the settings.
        if ((model.Gone ?? model.Refusal) is string replaced)
        {
            content.Children.Add(new TextBlock { Text = replaced, TextWrapping = TextWrapping.Wrap, MaxWidth = 460 });
            return;
        }
        if (model.NewNote is string newNote)
        {
            content.Children.Add(Note(newNote));
        }

        var roles = new ComboBox { IsEnabled = !copying, MinWidth = 200 };
        foreach (var r in model.Roles)
        {
            roles.Items.Add(r.Title);
        }
        roles.SelectedIndex = Array.FindIndex(model.Roles, r => r.Role == model.Role);
        roles.SelectionChanged += (_, _) =>
        {
            if (roles.SelectedIndex >= 0 && model.Roles[roles.SelectedIndex].Role != form.Role) Change(form with { Role = model.Roles[roles.SelectedIndex].Role });
        };
        AddFolderDialog.Help(roles, model.RoleHelp);
        content.Children.Add(roles);
        content.Children.Add(Note(model.RoleHelp));

        content.Children.Add(Setting(model.SingleUse, on => Change(form with { SingleUse = on }), v => Change(form with { Uses = v })));
        content.Children.Add(Setting(model.LinkExpires, on => Change(form with { LinkExpires = on }), v => Change(form with { LinkDays = v })));

        if (model.Chat is SyncShareChat chat)
        {
            var row = new Grid { ColumnSpacing = 8 };
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var box = new CheckBox { Content = chat.Label, IsChecked = chat.On, IsEnabled = !copying };
            AddFolderDialog.Help(box, chat.Help);
            box.Click += (_, _) => Change(form with { WithChat = box.IsChecked == true });
            row.Children.Add(box);
            if (chat.On)
            {
                var chosen = chat.Choices.FirstOrDefault(c => c.Id == chat.Chosen) ?? chat.Choices.FirstOrDefault();
                var pick = new DropDownButton { Content = chosen?.Title ?? "", IsEnabled = !copying, VerticalAlignment = VerticalAlignment.Center };
                var menu = new MenuFlyout();
                foreach (var c in chat.Choices)
                {
                    var item = new ToggleMenuFlyoutItem { Text = c.Title, IsChecked = c.Id == chat.Chosen };
                    var id = c.Id;
                    item.Click += (_, _) => Change(form with { Chat = id });
                    menu.Items.Add(item);
                    // The new channel first, a line under it.
                    if (c.Id.Length == 0 && chat.Choices.Length > 1)
                    {
                        menu.Items.Add(new MenuFlyoutSeparator());
                    }
                }
                pick.Flyout = menu;
                Grid.SetColumn(pick, 1);
                row.Children.Add(pick);
            }
            content.Children.Add(row);
        }
        if (model.Summary.Length > 0)
        {
            content.Children.Add(new TextBlock { Text = model.Summary, TextWrapping = TextWrapping.Wrap, MaxWidth = 460 });
        }

        if (model.Sharing is SyncShareSharing sharing)
        {
            content.Children.Add(Rule());
            if (sharing.Countdown is string left)
            {
                var row = new Grid { ColumnSpacing = 8 };
                row.ColumnDefinitions.Add(new ColumnDefinition());
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.Children.Add(new TextBlock { Text = left, VerticalAlignment = VerticalAlignment.Center });
                if (sharing.ExtendTitle is string extend)
                {
                    var b = new DropDownButton { Content = extend, IsEnabled = !copying };
                    AddFolderDialog.Help(b, sharing.ExtendHelp);
                    var menu = new MenuFlyout();
                    foreach (var c in sharing.ExtendChoices)
                    {
                        var item = new MenuFlyoutItem { Text = c.Label };
                        var days = c.Value;
                        item.Click += (_, _) => ShareUntil(TrsFfiMethods.SyncSharingExtended(CurrentUntil(), days, Now()));
                        menu.Items.Add(item);
                    }
                    menu.Items.Add(new MenuFlyoutSeparator());
                    var noEnd = new MenuFlyoutItem { Text = sharing.NoEndTitle };
                    noEnd.Click += (_, _) => ShareUntil(0);
                    menu.Items.Add(noEnd);
                    b.Flyout = menu;
                    Grid.SetColumn(b, 1);
                    row.Children.Add(b);
                }
                content.Children.Add(row);
            }
            else if (sharing.Start is SyncShareSetting start)
            {
                content.Children.Add(Setting(start, on => { if (on) ShareUntil(TrsFfiMethods.SyncSharingEnd(form.ShareDays, Now())); }, v => Change(form with { ShareDays = v })));
            }
            content.Children.Add(Note(sharing.Line));
        }

        if (model.Links.Length > 0 || model.LinksMore is not null)
        {
            content.Children.Add(Rule());
            content.Children.Add(new TextBlock { Text = model.LinksCaption, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Foreground = Res("TextFillColorSecondaryBrush") });
            foreach (var l in model.Links)
            {
                var row = new Grid { ColumnSpacing = 8 };
                row.ColumnDefinitions.Add(new ColumnDefinition());
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                var text = new StackPanel { Spacing = 1 };
                text.Children.Add(new TextBlock { Text = l.Line, TextWrapping = TextWrapping.Wrap });
                text.Children.Add(Note(l.Detail));
                row.Children.Add(text);
                var revoke = new Button { Content = model.RevokeTitle, IsEnabled = !copying, VerticalAlignment = VerticalAlignment.Center };
                AddFolderDialog.Help(revoke, model.RevokeHelp);
                var linkId = l.Id;
                revoke.Click += (_, _) => Revoke(linkId);
                Grid.SetColumn(revoke, 1);
                row.Children.Add(revoke);
                content.Children.Add(row);
            }
            if (model.LinksMore is string more)
            {
                content.Children.Add(Note(more));
            }
        }
        if (copying)
        {
            content.Children.Add(new ProgressBar { IsIndeterminate = true });
        }
        content.Children.Add(failure);
    }

    static UIElement Rule() => new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Margin = new Thickness(0, 4, 0, 4), Fill = Res("DividerStrokeColorDefaultBrush") };

    /// <summary>A setting: its switch with the name, the value menu beside it while it has choices (the core's).</summary>
    UIElement Setting(SyncShareSetting s, Action<bool> setOn, Action<uint> setValue)
    {
        var row = new Grid { ColumnSpacing = 8 };
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var toggle = new ToggleSwitch { Header = s.Label, IsOn = s.On, IsEnabled = !copying };
        AddFolderDialog.Help(toggle, s.Help);
        toggle.Toggled += (_, _) => { if (toggle.IsOn != s.On) setOn(toggle.IsOn); };
        row.Children.Add(toggle);
        if (s.Choices.Length > 0)
        {
            var values = new ComboBox { IsEnabled = !copying, VerticalAlignment = VerticalAlignment.Bottom };
            foreach (var c in s.Choices)
            {
                values.Items.Add(c.Label);
            }
            values.SelectedIndex = Array.FindIndex(s.Choices, c => c.Value == s.Value);
            values.SelectionChanged += (_, _) =>
            {
                if (values.SelectedIndex >= 0 && s.Choices[values.SelectedIndex].Value != s.Value) setValue(s.Choices[values.SelectedIndex].Value);
            };
            Grid.SetColumn(values, 1);
            row.Children.Add(values);
        }
        return row;
    }

    void Tips()
    {
        foreach (var b in RowCommands.Descendants(dialog.XamlRoot).OfType<Button>())
        {
            if (model.CopyTitle is string copy && Equals(b.Content, copy)) AddFolderDialog.Help(b, model.CopyHelp);
            else if (Equals(b.Content, model.CloseTitle)) AddFolderDialog.Help(b, model.CloseHelp);
        }
    }

    /// <summary>When the folder's or channel's sharing period ends now (Unix seconds; none: no end).</summary>
    ulong? CurrentUntil()
    {
        var folder = app.Core.SyncSnapshot().Folders.FirstOrDefault(f => f.Id == form.Id);
        return folder is not null ? folder.ShareUntilS : app.Core.ChatChannels().FirstOrDefault(c => c.Id == form.Id)?.ShareUntilS;
    }

    void ShareUntil(ulong until) => Background(() => app.Core.SyncSetShareUntil(form.Id, until), $"share until {until}");

    void Revoke(string linkId) => Background(() => app.Core.SyncRevokeInvite(form.Id, linkId), "revoke");

    /// <summary>A core call off the UI thread; a failure is the core's sentence in the dialog; the links are read again.</summary>
    void Background(Func<Task> work, string what)
    {
        _ = Task.Run(work).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
        {
            AppLog.Write($"share: {what} {(t.IsCompletedSuccessfully ? "done" : "failed")}");
            Draw();
            if (!t.IsCompletedSuccessfully) AddFolderDialog.Failed(failure, Message(t.Exception));
            LoadInvites();
        }));
    }

    static string Message(AggregateException? e) => e?.InnerException is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : e?.InnerException?.Message ?? "";

    /// <summary>Copy Link: makes the link (syncing a new folder, making a new chat first), copies it, keeps the new form.</summary>
    internal async Task<bool> Copy()
    {
        if (model.CopyTitle is null || copying)
        {
            return false;
        }
        copying = true;
        Draw();
        var f = form;
        SyncShareCopied? done = null;
        string? error = null;
        try
        {
            done = await Task.Run(() => app.Core.SyncShareCopyLink(f));
        }
        catch (CoreException e)
        {
            error = TrsFfiMethods.CoreErrorMessage(e);
        }
        copying = false;
        if (done is null)
        {
            AppLog.Write($"share: copy failed: {error}");
            Draw();
            AddFolderDialog.Failed(failure, error ?? "");
            return false;
        }
        // The link is a secret: on the clipboard only, never in the log (a QA run leaves the clipboard alone).
        if (!Quiet.On)
        {
            var package = new Windows.ApplicationModel.DataTransfer.DataPackage();
            package.SetText(done.Invite.Link);
            Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
        }
        AppLog.Write($"share: link copied ({done.Invite.Role})");
        form = done.Form;
        copied = true;
        Draw();
        LoadInvites();
        var shown = model.CopiedMs;
        _ = Task.Delay(TimeSpan.FromMilliseconds(shown)).ContinueWith(_ => content.DispatcherQueue.TryEnqueue(() => { copied = false; Draw(); }));
        app.LibraryWindowIfCreated?.Pane.Refresh();
        return true;
    }

    // --------------------------------------------------------------------------------------------------- QA

    internal void QaRole(SyncRole role) => Change(form with { Role = role });
    internal void QaSingleUse(bool on) => Change(form with { SingleUse = on });
    internal void QaExpires(bool on, uint days) => Change(form with { LinkExpires = on, LinkDays = days == 0 ? form.LinkDays : days });
    internal void QaChat(bool on, string chat) => Change(form with { WithChat = on, Chat = chat });
    internal void QaRevokeFirst()
    {
        if (model.Links.FirstOrDefault() is SyncShareLink l) Revoke(l.Id);
    }
    internal void QaShareFor(uint days) => ShareUntil(TrsFfiMethods.SyncSharingEnd(days, Now()));
    internal void QaNoEnd() => ShareUntil(0);
    internal void QaCancel()
    {
        dialog.Hide();
        if (Open == this)
        {
            Open = null;
        }
    }
    internal SyncShareWindowModel Model => model;
    internal SyncShareForm Form => form;
    internal string Failure => failure.Visibility == Visibility.Visible ? failure.Text : "";
}
