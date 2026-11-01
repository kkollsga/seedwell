using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// New Channel / Join with a Link on Windows (the Mac's New Channel window), a dialog over the library window drawn from
/// the core's <c>ChatNewWindowModel</c> (trs-ffi chat_dialogs.rs): its two sides; on New the name with its note, Agents
/// Yes/No with what the choice means and "On all my devices" while it is shown; on Join the link with the line the core
/// reads from it. The form is the core's value: this keeps it, edits the mode, name, agents and devices choice itself
/// and the link through <c>chat_new_set_link</c>, and draws the model again after each change. Start runs
/// <c>chat_new_start</c> off the UI thread; a failure stays in the dialog under its failure title; the channel opens.
/// </summary>
sealed class NewChannelDialog
{
    readonly App app;
    readonly ContentDialog dialog;
    readonly StackPanel content = new() { Spacing = 10, MinWidth = 420 };
    // The fields live across redraws, so the caret and what is typed stay.
    readonly TextBox name = new();
    readonly TextBox link = new();
    readonly TextBlock failure = new() { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed };
    ChatNewForm form;
    ChatNewWindowModel model;
    bool starting;

    NewChannelDialog(App app, XamlRoot root, ChatNewForm form)
    {
        this.app = app;
        this.form = form;
        model = TrsFfiMethods.ChatNewWindow(form);
        name.Text = form.Name;
        link.Text = form.Link;
        name.TextChanged += (_, _) => { if (this.form.Name != name.Text) Change(this.form with { Name = name.Text }); };
        link.TextChanged += (_, _) => { if (this.form.Link != link.Text) Change(app.Core.ChatNewSetLink(this.form, link.Text)); };
        dialog = new ContentDialog
        {
            XamlRoot = root,
            Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
            DefaultButton = ContentDialogButton.Primary,
        };
        dialog.PrimaryButtonClick += async (_, e) =>
        {
            // Start is slow (the network): the dialog stays open, disabled, until it is done.
            var deferral = e.GetDeferral();
            e.Cancel = !await Start();
            deferral.Complete();
        };
        Draw();
    }

    /// <summary>The dialog open now (QA).</summary>
    internal static NewChannelDialog? Open { get; private set; }

    /// <summary>
    /// Opens the dialog: on Join with <paramref name="prefill"/> (a link that opened the app), else with the clipboard's
    /// text when the core says it looks like a channel link, else on New Channel. One at a time.
    /// </summary>
    internal static async void Show(App app, string? prefill)
    {
        if (Open is not null || app.LibraryWindow.Content.XamlRoot is not XamlRoot root)
        {
            return;
        }
        if (prefill is null && !Quiet.On)
        {
            try
            {
                var clip = Windows.ApplicationModel.DataTransfer.Clipboard.GetContent();
                if (clip.Contains(Windows.ApplicationModel.DataTransfer.StandardDataFormats.Text) && await clip.GetTextAsync() is string text
                    && TrsFfiMethods.ChatLooksLikeLink(text))
                {
                    prefill = text;
                }
            }
            catch (Exception e)
            {
                AppLog.Write($"new channel: clipboard not read ({e.Message})");
            }
        }
        var d = new NewChannelDialog(app, root, app.Core.ChatNewForm(prefill));
        Open = d;
        AppLog.Write($"new channel: open on {d.form.Mode}");
        try { await d.dialog.ShowAsync(); } catch (Exception e) { AppLog.Write($"new channel failed: {e.Message}"); }
        if (Open == d)
        {
            Open = null;
        }
    }

    void Change(ChatNewForm next)
    {
        form = next;
        failure.Visibility = Visibility.Collapsed;
        Draw();
    }

    void Draw()
    {
        model = TrsFfiMethods.ChatNewWindow(form);
        var title = new StackPanel();
        title.Children.Add(new TextBlock { Text = model.Caption, FontSize = 12, Opacity = 0.7 });
        title.Children.Add(new TextBlock { Text = model.Title });
        dialog.Title = title;
        dialog.PrimaryButtonText = model.ConfirmTitle;
        dialog.CloseButtonText = model.CancelTitle;
        dialog.IsPrimaryButtonEnabled = model.CanStart && !starting;

        foreach (var kept in new FrameworkElement[] { name, link, failure })
        {
            (kept.Parent as Panel)?.Children.Remove(kept);
        }
        content.Children.Clear();
        // The two sides, as the Mac's segmented switch: the chosen one in the accent style.
        var modes = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4 };
        foreach (var m in model.Modes)
        {
            var b = new Button { Content = m.Title, IsEnabled = !starting };
            if (m.Mode == form.Mode)
            {
                b.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
            }
            var mode = m.Mode;
            b.Click += (_, _) => Change(form with { Mode = mode });
            modes.Children.Add(b);
        }
        content.Children.Add(modes);

        if (form.Mode == ChatNewMode.New)
        {
            name.Header = model.NameLabel;
            name.PlaceholderText = model.NamePlaceholder;
            name.IsEnabled = !starting;
            content.Children.Add(name);
            content.Children.Add(Note(model.NameNote));
            var agents = new RadioButtons { Header = model.AgentsLabel, MaxColumns = model.AgentsChoices.Length, IsEnabled = !starting };
            foreach (var c in model.AgentsChoices)
            {
                agents.Items.Add(c.Title);
            }
            agents.SelectedIndex = Array.FindIndex(model.AgentsChoices, c => c.Checked);
            agents.SelectionChanged += (_, _) =>
            {
                if (agents.SelectedIndex >= 0 && model.AgentsChoices[agents.SelectedIndex].Open != form.Agents)
                {
                    Change(form with { Agents = model.AgentsChoices[agents.SelectedIndex].Open });
                }
            };
            content.Children.Add(agents);
            content.Children.Add(Note(model.AgentsNote));
            if (model.Follow is FollowLine follow)
            {
                var toggle = new ToggleSwitch { Header = follow.Title, IsOn = follow.On, IsEnabled = follow.Enabled && !starting };
                if (follow.Help.Length > 0)
                {
                    ToolTipService.SetToolTip(toggle, follow.Help);
                }
                toggle.Toggled += (_, _) => { if (toggle.IsOn != form.AllDevices) Change(form with { AllDevices = toggle.IsOn }); };
                content.Children.Add(toggle);
                if (follow.Line.Length > 0)
                {
                    content.Children.Add(Note(follow.Line));
                }
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
                var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
                line.Children.Add(new FontIcon
                {
                    Glyph = model.LinkOk ? "" : "",
                    FontSize = 12,
                    VerticalAlignment = VerticalAlignment.Top,
                    Margin = new Thickness(0, 2, 0, 0),
                    Foreground = model.LinkOk ? (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"] : (Brush)Application.Current.Resources["SystemFillColorCautionBrush"],
                });
                line.Children.Add(new TextBlock { Text = model.LinkLine, FontSize = 12, TextWrapping = TextWrapping.Wrap, MaxWidth = 380 });
                content.Children.Add(line);
            }
        }
        if (starting)
        {
            content.Children.Add(new ProgressBar { IsIndeterminate = true });
        }
        content.Children.Add(failure);
        dialog.Opened -= TipsOnOpen;
        dialog.Opened += TipsOnOpen;
    }

    /// <summary>The Start button's help as its tooltip, once the dialog's buttons exist.</summary>
    void TipsOnOpen(ContentDialog sender, ContentDialogOpenedEventArgs args)
    {
        foreach (var b in RowCommands.Descendants(dialog.XamlRoot).OfType<Button>())
        {
            if (Equals(b.Content, model.ConfirmTitle)) ToolTipService.SetToolTip(b, model.ConfirmHelp);
        }
    }

    static TextBlock Note(string text) => new() { Text = text, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap, MaxWidth = 420 };

    /// <summary>Start: makes or joins the channel off the UI thread; true when it did (the dialog closes, the channel opens).</summary>
    internal async Task<bool> Start()
    {
        if (!model.CanStart || starting)
        {
            return false;
        }
        starting = true;
        Draw();
        var f = form;
        SyncChannel? channel = null;
        string? error = null;
        try
        {
            channel = await Task.Run(() => app.Core.ChatNewStart(f));
        }
        catch (CoreException e)
        {
            error = TrsFfiMethods.CoreErrorMessage(e);
        }
        starting = false;
        if (channel is null)
        {
            AppLog.Write($"new channel: failed: {error}");
            Draw();
            failure.Inlines.Clear();
            failure.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = model.FailureTitle, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
            failure.Inlines.Add(new Microsoft.UI.Xaml.Documents.LineBreak());
            failure.Inlines.Add(new Microsoft.UI.Xaml.Documents.Run { Text = error ?? "" });
            failure.Visibility = Visibility.Visible;
            return false;
        }
        AppLog.Write($"new channel: started {channel.Name} ({channel.Id})");
        // The channel's page. A new channel's next step is its Share dialog, which Windows does not have yet.
        app.Library.SelectPlace(new LibraryPlace.Chat(channel.Id, null));
        app.LibraryWindowIfCreated?.Pane.Refresh();
        return true;
    }

    /// <summary>QA: what the clicks and typing would do (typing's TextChanged comes later: the form changes at once).</summary>
    internal void QaMode(ChatNewMode mode) => Change(form with { Mode = mode });
    internal void QaName(string text)
    {
        Change(form with { Name = text });
        name.Text = text;
    }
    internal void QaLink(string text)
    {
        Change(app.Core.ChatNewSetLink(form, text));
        link.Text = text;
    }
    internal void QaAgents(bool open) => Change(form with { Agents = open });
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
    internal ChatNewWindowModel Model => model;
    internal ChatNewForm Form => form;
}
