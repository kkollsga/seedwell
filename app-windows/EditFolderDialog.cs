using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// A synced folder's Edit dialog on Windows (the Mac's folder editor), drawn from the core's <c>SyncEditWindowModel</c>
/// (trs-ffi sync_dialogs.rs): the name, where it is with Move…, Files on demand, "On all my devices", Share… with its
/// note, and the Options (Pause, Local network only, Rescan Now). The form is the core's value: this keeps it, edits
/// the fields itself and the new place through <c>sync_edit_choose_parent</c>, and draws the model again after each
/// change. Save closes without a call when nothing changed, else runs <c>sync_edit_save</c> off the UI thread (every
/// change in the order that works); a failure stays in the dialog and the form is read again.
/// </summary>
sealed class EditFolderDialog
{
    readonly App app;
    readonly ContentDialog dialog;
    readonly StackPanel content = new() { Spacing = 10, MinWidth = 440 };
    // The name field lives across redraws, so the caret and what is typed stay.
    readonly TextBox name = new();
    readonly TextBlock failure = new() { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Foreground = Res("SystemFillColorCriticalBrush") };
    readonly string id;
    SyncEditForm form;
    SyncEditWindowModel model;
    bool saving;
    string rescanned = "";
    // What runs once the dialog has closed (only one ContentDialog may be open): its Share dialog.
    Action? next;

    EditFolderDialog(App app, XamlRoot root, SyncEditForm form)
    {
        this.app = app;
        this.form = form;
        id = form.FolderId;
        model = Window(form);
        name.Text = form.Name;
        name.TextChanged += (_, _) => { if (this.form.Name != name.Text) Change(this.form with { Name = name.Text }); };
        dialog = new ContentDialog
        {
            XamlRoot = root,
            Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
            DefaultButton = ContentDialogButton.Primary,
        };
        dialog.PrimaryButtonClick += async (_, e) =>
        {
            var deferral = e.GetDeferral();
            e.Cancel = !await Save();
            deferral.Complete();
        };
        dialog.Opened += (_, _) =>
        {
            foreach (var b in RowCommands.Descendants(dialog.XamlRoot).OfType<Button>())
            {
                if (Equals(b.Content, model.ConfirmTitle)) AddFolderDialog.Help(b, model.ConfirmHelp);
                else if (Equals(b.Content, model.CancelTitle)) AddFolderDialog.Help(b, model.CancelHelp);
            }
        };
        Draw();
    }

    /// <summary>The dialog open now (QA).</summary>
    internal static EditFolderDialog? Open { get; private set; }

    /// <summary>Opens folder <paramref name="id"/>'s Edit dialog. One at a time.</summary>
    internal static async void Show(App app, string id)
    {
        if (Open is not null || await AddFolderDialog.Root(app) is not XamlRoot root || Open is not null)
        {
            return;
        }
        var d = new EditFolderDialog(app, root, app.Core.SyncEditForm(id));
        Open = d;
        AppLog.Write($"edit folder: open on {id}");
        try { await d.dialog.ShowAsync(); } catch (Exception e) { AppLog.Write($"edit folder failed: {e.Message}"); }
        if (Open == d)
        {
            Open = null;
        }
        d.next?.Invoke();
    }

    static Brush Res(string key) => (Brush)Application.Current.Resources[key];

    static TextBlock Note(string text) => new() { Text = text, FontSize = 12, Foreground = Res("TextFillColorSecondaryBrush"), TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Left, MaxWidth = 440 };

    static SyncEditWindowModel Window(SyncEditForm form) => TrsFfiMethods.SyncEditWindow(form, App.HostWindows, App.HostFeatures);

    void Change(SyncEditForm next)
    {
        form = next;
        failure.Visibility = Visibility.Collapsed;
        Draw();
    }

    void Draw()
    {
        model = Window(form);
        dialog.Title = AddFolderDialog.Title(model.Caption, model.Title);
        dialog.PrimaryButtonText = model.ConfirmTitle;
        dialog.CloseButtonText = model.CancelTitle;
        dialog.IsPrimaryButtonEnabled = model.CanSave && !saving;

        foreach (var kept in new FrameworkElement[] { name, failure })
        {
            (kept.Parent as Panel)?.Children.Remove(kept);
        }
        content.Children.Clear();
        if (model.Subtitle is string subtitle)
        {
            content.Children.Add(Note(subtitle));
        }
        if (model.Gone is string gone)
        {
            // The folder is gone: its line replaces the form.
            content.Children.Add(new TextBlock { Text = gone, TextWrapping = TextWrapping.Wrap, MaxWidth = 440 });
            return;
        }

        name.Header = model.NameLabel;
        name.IsEnabled = !saving;
        AddFolderDialog.Help(name, model.NameHelp);
        content.Children.Add(name);

        content.Children.Add(new TextBlock { Text = model.LocationLabel });
        var place = new Grid { ColumnSpacing = 8 };
        place.ColumnDefinitions.Add(new ColumnDefinition());
        place.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        var where = new TextBlock
        {
            Text = model.Location,
            TextTrimming = TextTrimming.CharacterEllipsis,
            VerticalAlignment = VerticalAlignment.Center,
            Foreground = model.LocationMoved ? Res("TextFillColorPrimaryBrush") : Res("TextFillColorSecondaryBrush"),
        };
        AddFolderDialog.Help(where, model.LocationHelp);
        place.Children.Add(where);
        var move = new Button { Content = model.MoveTitle, IsEnabled = model.MoveEnabled && !saving };
        AddFolderDialog.Help(move, model.MoveHelp);
        move.Click += async (_, _) => { if (await app.PickFolder(model.MovePicker) is string parent) Change(TrsFfiMethods.SyncEditChooseParent(form, parent)); };
        Grid.SetColumn(move, 1);
        place.Children.Add(move);
        content.Children.Add(place);

        // Files on demand only where the host has them (the core leaves the label out otherwise).
        if (model.OnDemandLabel is string onDemandLabel)
        {
            var onDemand = new CheckBox { Content = onDemandLabel, IsChecked = form.OnDemand, IsEnabled = !saving };
            AddFolderDialog.Help(onDemand, model.OnDemandHelp ?? "");
            onDemand.Click += (_, _) => Change(form with { OnDemand = onDemand.IsChecked == true });
            content.Children.Add(onDemand);
        }
        if (model.OnDemandNote is string onDemandNote)
        {
            content.Children.Add(Note(onDemandNote));
        }
        if (model.Follow is FollowLine follow)
        {
            content.Children.Add(AddFolderDialog.Follow(follow, form.AllDevices, !saving, on => Change(form with { AllDevices = on }), ro => Change(form with { ReadOnly = ro })));
        }

        content.Children.Add(new TextBlock { Text = model.ShareCaption, Margin = new Thickness(0, 6, 0, 0) });
        var share = new Grid { ColumnSpacing = 8 };
        share.ColumnDefinitions.Add(new ColumnDefinition());
        share.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        share.Children.Add(Note(model.ShareNote));
        if (model.ShareTitle is string shareTitle)
        {
            var b = new Button { Content = shareTitle, IsEnabled = !saving, VerticalAlignment = VerticalAlignment.Center };
            AddFolderDialog.Help(b, model.ShareHelp);
            b.Click += (_, _) => { next = () => ShareDialog.Show(app, id); dialog.Hide(); };
            Grid.SetColumn(b, 1);
            share.Children.Add(b);
        }
        content.Children.Add(share);

        var options = new StackPanel { Spacing = 6 };
        var pause = new ToggleSwitch { Header = model.PauseLabel, IsOn = form.Paused, IsEnabled = !saving };
        AddFolderDialog.Help(pause, model.PauseHelp);
        pause.Toggled += (_, _) => { if (pause.IsOn != form.Paused) Change(form with { Paused = pause.IsOn }); };
        options.Children.Add(pause);
        var lan = new ToggleSwitch { Header = model.LanLabel, IsOn = form.LanOnly, IsEnabled = !saving };
        AddFolderDialog.Help(lan, model.LanHelp);
        lan.Toggled += (_, _) => { if (lan.IsOn != form.LanOnly) Change(form with { LanOnly = lan.IsOn }); };
        options.Children.Add(lan);
        var rescan = new Button { Content = model.RescanTitle, IsEnabled = !saving };
        AddFolderDialog.Help(rescan, model.RescanHelp);
        rescan.Click += (_, _) => Rescan();
        options.Children.Add(rescan);
        options.Children.Add(Note(model.RescanNote));
        if (rescanned.Length > 0)
        {
            options.Children.Add(Note(rescanned));
        }
        var expander = new Expander
        {
            Header = model.OptionsTitle,
            Content = options,
            IsExpanded = form.OptionsOpen,
            HorizontalAlignment = HorizontalAlignment.Stretch,
            HorizontalContentAlignment = HorizontalAlignment.Left,
        };
        expander.Expanding += (_, _) => { if (!form.OptionsOpen) form = form with { OptionsOpen = true }; };
        expander.Collapsed += (_, _) => { if (form.OptionsOpen) form = form with { OptionsOpen = false }; };
        content.Children.Add(expander);

        if (saving)
        {
            content.Children.Add(new ProgressBar { IsIndeterminate = true });
        }
        content.Children.Add(failure);
    }

    /// <summary>Rescan Now: the core's, off the UI thread; a failure is the core's sentence under the button.</summary>
    void Rescan()
    {
        _ = Task.Run(() => app.Core.SyncRescan(id)).ContinueWith(t => content.DispatcherQueue.TryEnqueue(() =>
        {
            rescanned = t.IsCompletedSuccessfully ? "" : t.Exception?.InnerException is CoreException ce ? TrsFfiMethods.CoreErrorMessage(ce) : "";
            AppLog.Write($"edit folder: rescan {(t.IsCompletedSuccessfully ? "started" : $"failed: {rescanned}")}");
            Draw();
        }));
    }

    /// <summary>Save: nothing changed closes; otherwise the core saves every change off the UI thread. True when it closes.</summary>
    internal async Task<bool> Save()
    {
        if (!model.CanSave || saving)
        {
            return false;
        }
        if (!model.Changed)
        {
            AppLog.Write("edit folder: nothing changed");
            return true;
        }
        saving = true;
        Draw();
        var f = form;
        string? error = null;
        try
        {
            await Task.Run(() => app.Core.SyncEditSave(f));
        }
        catch (CoreException e)
        {
            error = TrsFfiMethods.CoreErrorMessage(e);
        }
        saving = false;
        if (error is not null)
        {
            // What did change stays changed: the form is read again.
            AppLog.Write($"edit folder: failed: {error}");
            var open = form.OptionsOpen;
            form = app.Core.SyncEditForm(id) with { OptionsOpen = open };
            name.Text = form.Name;
            Draw();
            AddFolderDialog.Failed(failure, error);
            return false;
        }
        AppLog.Write($"edit folder: saved {id}");
        app.LibraryWindowIfCreated?.Pane.Refresh();
        return true;
    }

    // --------------------------------------------------------------------------------------------------- QA

    internal void QaName(string text)
    {
        Change(form with { Name = text });
        name.Text = text;
    }
    internal void QaPaused(bool on) => Change(form with { Paused = on });
    internal void QaLanOnly(bool on) => Change(form with { LanOnly = on });
    internal void QaAllDevices(bool on) => Change(form with { AllDevices = on });
    internal string Switches => AddFolderDialog.QaSwitches(content);
    internal void QaReadOnly(bool on) => Change(form with { ReadOnly = on });
    internal void QaOnDemand(bool on) => Change(form with { OnDemand = on });
    internal void QaOptions(bool open) => Change(form with { OptionsOpen = open });
    internal void QaParent(string path) => Change(TrsFfiMethods.SyncEditChooseParent(form, path));
    internal void QaShare()
    {
        next = () => ShareDialog.Show(app, id);
        QaCancel();
        // A QA run's Hide ends ShowAsync, which runs next.
    }
    internal async Task<bool> QaSave()
    {
        if (!await Save())
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
    internal SyncEditWindowModel Model => model;
    internal SyncEditForm Form => form;
    internal string Failure => failure.Visibility == Visibility.Visible ? failure.Text : "";
}
