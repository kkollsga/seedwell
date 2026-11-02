using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Add Folder on Windows (the Mac's Add Folder window), a dialog over the library window drawn from the core's
/// <c>SyncAddWindowModel</c> (trs-ffi sync_dialogs.rs): its two sides; From this PC the folder chosen with Choose… and
/// its note; Shared with me the link with the line the core reads from it, and where it goes (the Location menu, its
/// Choose…); then Files on demand and "On all my devices". The form is the core's value: this keeps it, edits the mode,
/// folder, on-demand and devices choice itself, the link through <c>sync_add_set_link</c> and the place through
/// <c>sync_add_set_location</c> / <c>sync_add_choose_parent</c>, and draws the model again after each change. Start runs
/// <c>sync_add_start</c> off the UI thread; a failure stays in the dialog; the new folder's page opens, then its Share
/// dialog when the core says so.
/// </summary>
sealed class AddFolderDialog
{
    readonly App app;
    readonly ContentDialog dialog;
    readonly StackPanel content = new() { Spacing = 10, MinWidth = 440 };
    // The link field lives across redraws, so the caret and what is typed stay.
    readonly TextBox link = new();
    readonly TextBlock failure = new() { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Foreground = Res("SystemFillColorCriticalBrush") };
    SyncAddForm form;
    SyncAddWindowModel model;
    bool starting;
    // What runs once the dialog has closed (only one ContentDialog may be open): the new folder's Share dialog.
    Action? next;

    AddFolderDialog(App app, XamlRoot root, SyncAddForm form)
    {
        this.app = app;
        this.form = form;
        model = app.Core.SyncAddWindow(form, App.HostFeatures);
        link.Text = form.Key;
        link.TextChanged += (_, _) => { if (this.form.Key != link.Text) Change(app.Core.SyncAddSetLink(this.form, link.Text)); };
        dialog = new ContentDialog
        {
            XamlRoot = root,
            Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
            DefaultButton = ContentDialogButton.Primary,
        };
        dialog.PrimaryButtonClick += async (_, e) =>
        {
            // Start may be slow (a join reaches the network): the dialog stays open, disabled, until it is done.
            var deferral = e.GetDeferral();
            e.Cancel = !await Start();
            deferral.Complete();
        };
        dialog.Opened += (_, _) => Tips();
        Draw();
    }

    /// <summary>The dialog open now (QA).</summary>
    internal static AddFolderDialog? Open { get; private set; }

    /// <summary>
    /// Opens the dialog: on Shared with me with <paramref name="prefill"/> (a seedwell://sync/ link), else with the
    /// clipboard's text when the core says it looks like a folder link, else on From this PC. One at a time.
    /// </summary>
    internal static async void Show(App app, string? prefill)
    {
        if (Open is not null || await Root(app) is not XamlRoot root || Open is not null)
        {
            return;
        }
        prefill ??= await Clipboard(TrsFfiMethods.SyncLooksLikeLink);
        var d = new AddFolderDialog(app, root, app.Core.SyncAddForm(prefill));
        Open = d;
        AppLog.Write($"add folder: open on {d.form.Mode}");
        try { await d.dialog.ShowAsync(); } catch (Exception e) { AppLog.Write($"add folder failed: {e.Message}"); }
        if (Open == d)
        {
            Open = null;
        }
        d.next?.Invoke();
    }

    /// <summary>
    /// The library window shown (the tray and links open these dialogs too) and its XamlRoot once its content has
    /// loaded; null when it does not within 3 s.
    /// </summary>
    internal static async Task<XamlRoot?> Root(App app)
    {
        // The visual QA run draws into its own window, already there (ShowMainWindow is the real window's).
        if (Program.Render is null)
        {
            app.ShowMainWindow();
        }
        if (app.LibraryWindow.Content is not FrameworkElement content)
        {
            return null;
        }
        if (content.XamlRoot is null)
        {
            var loaded = new TaskCompletionSource();
            content.Loaded += (_, _) => loaded.TrySetResult();
            await Task.WhenAny(loaded.Task, Task.Delay(3000));
        }
        return content.XamlRoot;
    }

    /// <summary>The clipboard's text when <paramref name="looksRight"/> says so (never in a QA run).</summary>
    internal static async Task<string?> Clipboard(Func<string, bool> looksRight)
    {
        if (Quiet.On)
        {
            return null;
        }
        try
        {
            var clip = Windows.ApplicationModel.DataTransfer.Clipboard.GetContent();
            if (clip.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Text) && await clip.GetTextAsync() is string text && looksRight(text))
            {
                return text;
            }
        }
        catch (Exception e)
        {
            AppLog.Write($"clipboard not read ({e.Message})");
        }
        return null;
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Note(string text) => new() { Text = text, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Left, MaxWidth = 440 };

    void Change(SyncAddForm next)
    {
        form = next;
        failure.Visibility = Visibility.Collapsed;
        Draw();
    }

    void Draw()
    {
        model = app.Core.SyncAddWindow(form, App.HostFeatures);
        dialog.Title = Title(model.Caption, model.Title);
        dialog.PrimaryButtonText = model.ConfirmTitle;
        dialog.CloseButtonText = model.CancelTitle;
        dialog.IsPrimaryButtonEnabled = model.CanStart && !starting;

        foreach (var kept in new FrameworkElement[] { link, failure })
        {
            (kept.Parent as Panel)?.Children.Remove(kept);
        }
        content.Children.Clear();
        content.Children.Add(Modes(model.Modes.Select(m => (m.Title, m.Mode == form.Mode, (Action)(() => Change(form with { Mode = m.Mode })))), !starting));

        if (form.Mode == SyncAddMode.Local)
        {
            content.Children.Add(new TextBlock { Text = model.FolderLabel });
            var row = new Grid { ColumnSpacing = 8 };
            row.ColumnDefinitions.Add(new ColumnDefinition());
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.Children.Add(new TextBlock
            {
                Text = model.FolderText,
                TextTrimming = TextTrimming.CharacterEllipsis,
                VerticalAlignment = VerticalAlignment.Center,
                Foreground = model.FolderChosen ? Res("TextFillColorPrimaryBrush") : Res("TextFillColorSecondaryBrush"),
            });
            var choose = new Button { Content = model.ChooseTitle, IsEnabled = !starting };
            Help(choose, model.ChooseHelp);
            choose.Click += async (_, _) => { if (await app.PickFolder(model.FolderPicker) is string path) Change(form with { Folder = path }); };
            Grid.SetColumn(choose, 1);
            row.Children.Add(choose);
            content.Children.Add(row);
            if (model.LocalNote.Length > 0)
            {
                content.Children.Add(Note(model.LocalNote));
            }
        }
        else
        {
            link.Header = model.LinkLabel;
            link.PlaceholderText = model.LinkPlaceholder;
            link.IsEnabled = !starting;
            content.Children.Add(link);
            if (model.LinkLine.Length > 0)
            {
                content.Children.Add(CheckLine(model.LinkOk, model.LinkLine));
            }
            content.Children.Add(new TextBlock { Text = model.LocationLabel });
            var place = new DropDownButton
            {
                Content = model.Location.Length > 0 ? model.Location : model.LocationPlaceholder,
                IsEnabled = model.LocationEnabled && !starting,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Left,
            };
            Help(place, model.LocationHelp);
            var menu = new MenuFlyout();
            foreach (var l in model.Locations)
            {
                var item = new ToggleMenuFlyoutItem { Text = l.Title, IsChecked = l.Path == form.Location };
                var path = l.Path;
                item.Click += (_, _) => Change(TrsFfiMethods.SyncAddSetLocation(form, path));
                menu.Items.Add(item);
            }
            menu.Items.Add(new MenuFlyoutSeparator());
            var other = new MenuFlyoutItem { Text = model.LocationChoose };
            other.Click += async (_, _) => { if (await app.PickFolder(model.LocationPicker) is string parent) Change(TrsFfiMethods.SyncAddChooseParent(form, parent)); };
            menu.Items.Add(other);
            place.Flyout = menu;
            content.Children.Add(place);
            if (model.LocationNote is string locationNote)
            {
                content.Children.Add(Note(locationNote));
            }
        }

        // Files on demand only where the host has them (the core leaves the label out otherwise).
        if (model.OnDemandLabel is string onDemandLabel)
        {
            var onDemand = new CheckBox { Content = onDemandLabel, IsChecked = form.OnDemand, IsEnabled = !starting };
            Help(onDemand, model.OnDemandHelp ?? "");
            onDemand.Click += (_, _) => Change(form with { OnDemand = onDemand.IsChecked == true });
            content.Children.Add(onDemand);
        }
        if (model.OnDemandNote is string onDemandNote)
        {
            content.Children.Add(Note(onDemandNote));
        }
        if (model.Follow is FollowLine follow)
        {
            content.Children.Add(Follow(follow, form.AllDevices, !starting, on => Change(form with { AllDevices = on })));
        }
        if (starting)
        {
            content.Children.Add(new ProgressBar { IsIndeterminate = true });
        }
        content.Children.Add(failure);
    }

    void Tips()
    {
        foreach (var b in RowCommands.Descendants(dialog.XamlRoot).OfType<Button>())
        {
            if (Equals(b.Content, model.ConfirmTitle)) Help(b, model.ConfirmHelp);
            else if (Equals(b.Content, model.CancelTitle)) Help(b, model.CancelHelp);
        }
    }

    // ------------------------------------------------------------------------- pieces the folder dialogs share

    internal static StackPanel Title(string caption, string title)
    {
        var s = new StackPanel();
        if (caption.Length > 0)
        {
            s.Children.Add(new TextBlock { Text = caption, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush") });
        }
        s.Children.Add(new TextBlock { Text = title, TextTrimming = TextTrimming.CharacterEllipsis });
        return s;
    }

    /// <summary>The two sides, as the Mac's segmented switch: the chosen one in the accent style.</summary>
    internal static StackPanel Modes(IEnumerable<(string Title, bool Chosen, Action Choose)> modes, bool enabled)
    {
        var s = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        foreach (var (title, chosen, choose) in modes)
        {
            var b = new Button { Content = title, IsEnabled = enabled };
            if (chosen)
            {
                b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
            }
            b.Click += (_, _) => choose();
            s.Children.Add(b);
        }
        return s;
    }

    /// <summary>A line with a check mark when it is fine, else a warning sign.</summary>
    internal static StackPanel CheckLine(bool ok, string text)
    {
        var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        line.Children.Add(new FontIcon
        {
            Glyph = ok ? "" : "",
            FontSize = 12,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(0, 2, 0, 0),
            Foreground = ok ? Res("TextFillColorSecondaryBrush") : Res("SystemFillColorCautionBrush"),
        });
        line.Children.Add(new TextBlock { Text = text, FontSize = 12, TextWrapping = TextWrapping.Wrap, MaxWidth = 410 });
        return line;
    }

    /// <summary>"On all my devices": the core's title, its line, its help as the tooltip, bound to the form's choice.</summary>
    internal static StackPanel Follow(FollowLine follow, bool on, bool enabled, Action<bool> set)
    {
        var s = new StackPanel { Spacing = 2 };
        var toggle = new ToggleSwitch { Header = follow.Title, IsOn = on, IsEnabled = follow.Enabled && enabled };
        Help(toggle, follow.Help);
        toggle.Toggled += (_, _) => { if (toggle.IsOn != on) set(toggle.IsOn); };
        s.Children.Add(toggle);
        if (follow.Line.Length > 0)
        {
            s.Children.Add(Note(follow.Line));
        }
        return s;
    }

    internal static void Help(FrameworkElement e, string help)
    {
        if (help.Length > 0)
        {
            ToolTipService.SetToolTip(e, help);
        }
    }

    internal static void Failed(TextBlock failure, string text)
    {
        failure.Text = text;
        failure.Visibility = Visibility.Visible;
    }

    /// <summary>Start: adds or joins the folder off the UI thread; true when it did (the dialog closes, its page opens).</summary>
    internal async Task<bool> Start()
    {
        if (!model.CanStart || starting)
        {
            return false;
        }
        starting = true;
        Draw();
        var f = form;
        var shareAfter = model.ShareAfterStart;
        SyncFolder? folder = null;
        string? error = null;
        try
        {
            folder = await Task.Run(() => app.Core.SyncAddStart(f));
        }
        catch (CoreException e)
        {
            error = TrsFfiMethods.CoreErrorMessage(e);
        }
        starting = false;
        if (folder is null)
        {
            AppLog.Write($"add folder: failed: {error}");
            Draw();
            Failed(failure, error ?? "");
            return false;
        }
        AppLog.Write($"add folder: added {folder.Name} ({folder.Id}), share after {shareAfter}");
        app.ShowPlace(new LibraryPlace.FolderSync());
        app.LibraryWindow.Pane.SyncPage?.Choose(folder.Id);
        var id = folder.Id;
        next = shareAfter ? () => ShareDialog.Show(app, id) : null;
        return true;
    }

    // --------------------------------------------------------------------------------------------------- QA

    /// <summary>QA: what the clicks and typing would do (typing's TextChanged comes later: the form changes at once).</summary>
    internal void QaMode(SyncAddMode mode) => Change(form with { Mode = mode });
    internal void QaFolder(string path) => Change(form with { Folder = path });
    internal void QaLink(string text)
    {
        Change(app.Core.SyncAddSetLink(form, text));
        link.Text = text;
    }
    internal void QaLocation(string path) => Change(TrsFfiMethods.SyncAddSetLocation(form, path));
    internal void QaParent(string path) => Change(TrsFfiMethods.SyncAddChooseParent(form, path));
    internal void QaAllDevices(bool on) => Change(form with { AllDevices = on });
    internal async Task<bool> QaStart()
    {
        if (!await Start())
        {
            return false;
        }
        QaCancel();
        return true;
    }
    internal void QaCancel()
    {
        dialog.Hide();
        if (Open == this)
        {
            Open = null;
        }
    }
    internal SyncAddWindowModel Model => model;
    internal SyncAddForm Form => form;
    internal string Failure => failure.Visibility == Visibility.Visible ? failure.Text : "";
}
