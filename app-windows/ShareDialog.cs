using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The Share dialog on Windows (the Mac's Share window) for a synced folder, a folder not synced yet, or a channel,
/// drawn from the core's <c>SyncShareWindowModel</c> (seedwell-ffi sync_share.rs): the role a link gives, Single use and Link
/// expires with their menus, Include a chat, the summary, the sharing period (its countdown and Extend, or Stop sharing
/// after), "On all my devices", "Read only", and the active links with Revoke. The form is the core's value: this keeps it and edits its fields, reads
/// the links off the UI thread when it opens and after every change, and draws the model again. Copy Link runs
/// <c>sync_share_copy_link</c> off the UI thread (it syncs a new folder and makes a new chat first) and puts the link on
/// the clipboard: a secret, never shown or logged. It opens at once with nothing read (<c>sync_share_form_opening</c>):
/// a ring, no Copy Link, while the folder's form is read off the UI thread. While open it asks again as often as the
/// model's <c>refresh_ms</c> says (the countdown, changes made elsewhere), off the UI thread, drawn only when the result
/// differs and never under an open drop-down (a drawing makes the controls anew, which would close it).
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
    bool settingDevices;
    bool opening;
    bool refreshing;
    readonly TaskCompletionSource ready = new();
    // This drawing's drop-downs and menus: a refresh waits while one is open.
    readonly List<ComboBox> combos = [];
    readonly List<MenuFlyout> menus = [];
    // The rows waiting to be shown, as the dialog opens (Unfold).
    readonly List<UIElement> folded = [];
    readonly TaskCompletionSource unfolded = new();

    /// <summary>QA only (an isolated run): reading the folder's form takes this long (SEEDWELL_QA_SLOW_SHARE_FORM_MS).</summary>
    static readonly int SlowFormMs = Program.StateDir is not null && int.TryParse(Environment.GetEnvironmentVariable("SEEDWELL_QA_SLOW_SHARE_FORM_MS"), out var ms) ? ms : 0;

    ShareDialog(App app, XamlRoot root, SyncShareForm form, bool opening)
    {
        this.app = app;
        this.form = form;
        this.opening = opening;
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
        countdown.Tick += (_, _) => _ = Refresh();
        Draw();
        if (!opening) _ = LoadInvites();
    }

    /// <summary>The dialog open now (QA).</summary>
    internal static ShareDialog? Open { get; private set; }

    /// <summary>The first real drawing is there: the folder's form and its links are read (or could not be).</summary>
    internal Task Ready => ready.Task;

    /// <summary>All its rows are shown (QA).</summary>
    internal Task Unfolded => unfolded.Task;

    /// <summary>Opens the Share dialog for folder or channel <paramref name="id"/>, its form read off the UI thread. One at a time.</summary>
    internal static void Show(App app, string id) => Show(app, id, id, () => app.Core.SyncShareForm(id));

    /// <summary>Opens it for <paramref name="path"/>, a folder not synced yet (a seedwell:// share link outside every folder), the same way.</summary>
    internal static void ShowNew(App app, string path) => Show(app, "", "a new folder", () => app.Core.SyncShareFormNew(path));

    static async void Show(App app, string id, string what, Func<SyncShareForm> read)
    {
        if (Open is not null || await AddFolderDialog.Root(app) is not XamlRoot root || Open is not null)
        {
            return;
        }
        var d = new ShareDialog(app, root, SeedwellFfiMethods.SyncShareFormOpening(id), true);
        Open = d;
        AppLog.Write($"share: open on {what}");
        d.ReadForm(read);
        // A moment for the real content, so a quick read never flashes the opening state.
        await Task.WhenAny(d.Ready, Task.Delay(150));
        // Unfolded completes on every path: QA's share open waits for it (else it waits for the run's limit).
        if (Open != d) { d.unfolded.TrySetResult(); return; }
        d.Fold();
        try { var showing = d.dialog.ShowAsync(); d.Unfold(); await showing; } catch (Exception e) { AppLog.Write($"share failed: {e.Message}"); }
        finally { d.unfolded.TrySetResult(); }
        if (Open == d)
        {
            Open = null;
        }
    }

    /// <summary>
    /// The dialog opens with its first rows and the others follow, a few to a turn, while it fades in. The system lays out and
    /// makes the templates of all that is shown in one turn, 35-65 ms for the whole dialog, and that turn with a collection in it
    /// held the UI thread over 100 ms (the check interactive). The first control stays: the dialog gives it the keyboard.
    /// </summary>
    void Fold()
    {
        if (!Ready.IsCompleted) return;
        foreach (var row in content.Children.Skip(FirstRows).Where(r => r.Visibility == Visibility.Visible))
        {
            row.Visibility = Visibility.Collapsed;
            folded.Add(row);
        }
    }

    const int FirstRows = 2, RowsATurn = 3;

    async void Unfold()
    {
        // (A drawing in between makes the rows anew, all shown: nothing is left to do.)
        while (folded.Count > 0)
        {
            await Quiet.AfterLayout();
            foreach (var row in folded.Take(RowsATurn)) row.Visibility = Visibility.Visible;
            folded.RemoveRange(0, Math.Min(RowsATurn, folded.Count));
        }
        unfolded.TrySetResult();
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Note(string text) => new() { Text = text, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Left, MaxWidth = 460 };

    static ulong Now() => (ulong)DateTimeOffset.UtcNow.ToUnixTimeSeconds();

    SyncShareWindowModel Window() => app.Core.SyncShareWindow(form, invites, Now(), new Clock());

    /// <summary>The folder's form, off the UI thread (it reads the core); the dialog is opening until it is there.</summary>
    void ReadForm(Func<SyncShareForm> read)
    {
        _ = Task.Run(() =>
        {
            if (SlowFormMs > 0) Thread.Sleep(SlowFormMs);
            return read();
        }).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
        {
            opening = false;
            if (t.IsCompletedSuccessfully)
            {
                // Drawn once, with its links: the form alone first would be one whole drawing more in the same moment.
                form = t.Result;
                failure.Visibility = Visibility.Collapsed;
                _ = LoadInvites(true);
                return;
            }
            AppLog.Write($"share: the form could not be read: {Message(t.Exception)}");
            Draw();
            AddFolderDialog.Failed(failure, Message(t.Exception));
            ready.TrySetResult();
        }));
    }

    /// <param name="first">The form just arrived and is not drawn yet: drawn whatever the links' read comes to.</param>
    /// <remarks>Done once the links read are drawn.</remarks>
    async Task LoadInvites(bool first = false)
    {
        if (form.Id.Length == 0)
        {
            if (first) { Draw(); Tips(); }
            ready.TrySetResult();
            return;
        }
        var id = form.Id;
        var read = false;
        try
        {
            var got = await Task.Run(() => app.Core.SyncInvites(id));
            if (form.Id == id) { invites = got; read = true; }
        }
        catch (Exception e) { AppLog.Write($"share: links failed: {CoreCalls.Message(e)}"); }
        if (read || first)
        {
            Draw();
        }
        if (first) Tips();
        ready.TrySetResult();
    }

    /// <summary>A drop-down or menu of this drawing is open.</summary>
    internal bool MenuOpen => combos.Any(c => c.IsDropDownOpen) || menus.Any(m => m.IsOpen);

    // Records with arrays in them do not compare by value: what they hold is compared.
    static string Signature(SyncShareWindowModel m) => System.Text.Json.JsonSerializer.Serialize(m);

    /// <summary>
    /// The model's refresh_ms ran out: the links and the model read off the UI thread, drawn only when they differ (a
    /// refresh mostly finds nothing changed). A change since has its own drawing; a drop-down open keeps its drawing, and
    /// the refresh is tried again a moment later.
    /// </summary>
    internal async Task Refresh()
    {
        if (refreshing || opening || copying || settingDevices) return;
        refreshing = true;
        var (f, listed, shown) = (form, invites, model);
        try
        {
            var (list, next, same) = await Task.Run(() =>
            {
                var l = f.Id.Length > 0 ? app.Core.SyncInvites(f.Id) : listed;
                var w = app.Core.SyncShareWindow(f, l, Now(), new Clock());
                return (l, w, Signature(w) == Signature(shown));
            });
            if (!ReferenceEquals(form, f)) return;
            invites = list;
            if (!same && MenuOpen)
            {
                countdown.Interval = TimeSpan.FromSeconds(1);
                countdown.Start();
                return;
            }
            if (same)
            {
                if (next.RefreshMs is uint ms) countdown.Interval = TimeSpan.FromMilliseconds(ms);
                return;
            }
            Draw();
        }
        catch (Exception e)
        {
            AppLog.Write($"share: refresh failed: {e.Message}");
        }
        finally
        {
            refreshing = false;
        }
    }

    void Change(SyncShareForm next)
    {
        form = next;
        failure.Visibility = Visibility.Collapsed;
        Draw();
    }

    void Draw() => FocusKeeper.Redraw(content, dialog.XamlRoot, DrawAnew);

    void DrawAnew()
    {
        Draws++;
        folded.Clear();
        model = Window();
        combos.Clear();
        menus.Clear();
        dialog.Title = AddFolderDialog.Title(model.Caption, model.Title);
        dialog.CloseButtonText = model.CloseTitle;
        dialog.PrimaryButtonText = model.CopyTitle is string copy && !opening ? (copied ? model.CopiedTitle : copy) : "";
        dialog.IsPrimaryButtonEnabled = model.CopyTitle is not null && !copying && !opening;
        if (opening)
        {
            (failure.Parent as Panel)?.Children.Remove(failure);
            content.Children.Clear();
            content.Children.Add(new ProgressRing { IsActive = true, Width = 32, Height = 32, Margin = new Thickness(0, 48, 0, 48), HorizontalAlignment = HorizontalAlignment.Center });
            return;
        }
        // Asked again as often as the core says, while it is open: the countdown, and changes made elsewhere (Refresh).
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
        combos.Add(roles);
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
                menus.Add(menu);
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
                        item.Click += (_, _) => Extend(days);
                        menu.Items.Add(item);
                    }
                    menu.Items.Add(new MenuFlyoutSeparator());
                    var noEnd = new MenuFlyoutItem { Text = sharing.NoEndTitle };
                    noEnd.Click += (_, _) => ShareUntil(0);
                    menu.Items.Add(noEnd);
                    b.Flyout = menu;
                    menus.Add(menu);
                    Grid.SetColumn(b, 1);
                    row.Children.Add(b);
                }
                content.Children.Add(row);
            }
            else if (sharing.Start is SyncShareSetting start)
            {
                content.Children.Add(Setting(start, on => { if (on) ShareUntil(SeedwellFfiMethods.SyncSharingEnd(form.ShareDays, Now())); }, v => Change(form with { ShareDays = v })));
            }
            content.Children.Add(Note(sharing.Line));
        }

        if (model.Devices is FollowLine devices)
        {
            content.Children.Add(Rule());
            content.Children.Add(AddFolderDialog.Follow(devices, devices.On, !copying && !settingDevices, SetDevices));
        }
        if (model.ReadOnly is ReadOnlyLine readOnly)
        {
            content.Children.Add(Rule());
            content.Children.Add(AddFolderDialog.ReadOnly(readOnly, !copying && !settingDevices, SetReadOnly));
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

    internal static UIElement Rule() => new Microsoft.UI.Xaml.Shapes.Rectangle { Height = 1, Margin = new Thickness(0, 4, 0, 4), Fill = Res("DividerStrokeColorDefaultBrush") };

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
            combos.Add(values);
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
        foreach (var b in RowCommands.DialogButtons(dialog))
        {
            if (model.CopyTitle is string copy && Equals(b.Content, copy)) AddFolderDialog.Help(b, model.CopyHelp);
            else if (Equals(b.Content, model.CloseTitle)) AddFolderDialog.Help(b, model.CloseHelp);
        }
    }

    /// <summary>Extend: the sharing period ends <paramref name="days"/> later (the core reads when it ends now).</summary>
    Task Extend(uint days)
    {
        var id = form.Id;
        return Background(() => app.Core.SyncShareExtend(id, days, Now()), $"extend {days}");
    }

    Task ShareUntil(ulong until) => Background(() => app.Core.SyncSetShareUntil(form.Id, until), $"share until {until}");

    Task Revoke(string linkId) => Background(() => app.Core.SyncRevokeInvite(form.Id, linkId), "revoke");

    void SetDevices(bool on) => SetSwitch(f => app.Core.SyncShareSetDevices(f, on), $"devices {on}");

    void SetReadOnly(bool on) => SetSwitch(f => app.Core.SyncShareSetReadOnly(f, on), $"read-only {on}");

    /// <summary>"On all my devices" or "Read only" switched: applied at once, off the UI thread; of the answer only the role is kept (the user may have changed the rest meanwhile).</summary>
    void SetSwitch(Func<SyncShareForm, Task<SyncShareForm>> set, string what)
    {
        if (settingDevices)
        {
            return;
        }
        settingDevices = true;
        failure.Visibility = Visibility.Collapsed;
        var f = form;
        Draw();
        _ = Task.Run(() => set(f)).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
        {
            settingDevices = false;
            AppLog.Write($"share: {what} {(t.IsCompletedSuccessfully ? "done" : "failed")}");
            if (t.IsCompletedSuccessfully)
            {
                form = form with { Role = t.Result.Role };
            }
            Draw();
            if (!t.IsCompletedSuccessfully) AddFolderDialog.Failed(failure, Message(t.Exception));
        }));
    }

    /// <summary>A core call off the UI thread; a failure is the core's sentence in the dialog; the links are read again.</summary>
    /// <remarks>Done once the links are read again and drawn.</remarks>
    async Task Background(Func<Task> work, string what)
    {
        string? failedWith = null;
        try { await Task.Run(work); }
        catch (Exception e) { failedWith = CoreCalls.Message(e); }
        AppLog.Write($"share: {what} {(failedWith is null ? "done" : "failed")}");
        Draw();
        if (failedWith is not null) AddFolderDialog.Failed(failure, failedWith);
        await LoadInvites();
    }

    static string Message(AggregateException? e) => e?.InnerException is CoreException ce ? SeedwellFfiMethods.CoreErrorMessage(ce) : e?.InnerException?.Message ?? "";

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
            error = SeedwellFfiMethods.CoreErrorMessage(e);
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
        _ = LoadInvites();
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
    internal Task QaRevokeFirst() => model.Links.FirstOrDefault() is SyncShareLink l ? Revoke(l.Id) : Task.CompletedTask;
    internal Task QaShareFor(uint days) => ShareUntil(SeedwellFfiMethods.SyncSharingEnd(days, Now()));
    internal Task QaNoEnd() => ShareUntil(0);
    internal Task QaExtend(uint days) => Extend(days);
    internal void QaDropDown(bool open) { if (combos.FirstOrDefault() is ComboBox c) c.IsDropDownOpen = open; }
    internal bool Opening => opening;
    /// <summary>QA: a tooltip opens over the dialog, as the pointer or the keyboard's focus may open one at any moment.</summary>
    /// <remarks>Done once the tooltip is drawn open (or gone): a tooltip's popup opens a moment after IsOpen.</remarks>
    internal async Task QaTooltip(bool open)
    {
        if (combos.FirstOrDefault() is not ComboBox c) return;
        if (ToolTipService.GetToolTip(c) is not ToolTip tip)
        {
            tip = new ToolTip { Content = "A tooltip over the dialog, about as wide as the real ones are" };
            ToolTipService.SetToolTip(c, tip);
        }
        tip.IsOpen = open;
        bool Shown() => dialog.XamlRoot is XamlRoot xr && Microsoft.UI.Xaml.Media.VisualTreeHelper.GetOpenPopupsForXamlRoot(xr).Any(p => p.IsOpen && p.Child is FrameworkElement e && (e == tip || RowCommands.Tree(e).Contains(tip)) && tip.ActualWidth > 0);
        for (var waited = 0; Shown() != open && waited < 100; waited++)
        {
            await Quiet.AfterLayout();
            await Task.Yield();
        }
    }
    internal string QaFocused => FocusKeeper.Describe(content, dialog.XamlRoot);
    internal Task<string> QaWalk() => FocusKeeper.QaWalk(content, dialog.XamlRoot, Draw);
    internal bool QaFocus(string label) => FocusKeeper.QaFocus(content, label);
    internal int Draws { get; private set; }
    internal void QaDevices(bool on) => SetDevices(on);
    internal void QaReadOnly(bool on) => SetReadOnly(on);
    internal bool SettingDevices => settingDevices;
    internal string Switches => AddFolderDialog.QaSwitches(content);
    internal void QaScrollEnd() => (dialog.Content as ScrollViewer)?.ChangeView(null, double.MaxValue, null, true);
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
