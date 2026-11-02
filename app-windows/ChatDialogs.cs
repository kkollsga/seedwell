using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The small chat dialogs on Windows (the Mac's ChatActions alerts), each drawn from the core's <c>ChatDialog</c>
/// (trs-ffi chat_dialogs.rs): New Topic…, Rename…, Wake Command…, Leave Channel…, Delete Conversation… and Agents:
/// Yes/No…. A title, the question, a text field when it asks for one, Cancel and the action (red when it removes
/// something). What was typed goes through <c>chat_dialog_check</c>: the action is disabled with the core's sentence
/// under the field while it can't run, and closes without a change when nothing changes. A failure is an alert with the
/// dialog's failure title and the error's sentence.
/// </summary>
sealed class ChatDialogs
{
    readonly App app;
    ContentDialog? open;
    TextBox? field;
    ChatDialog? shown;
    // What the open dialog acts on (QA's confirm runs what its button runs).
    SyncChannel? channel;
    string? deviceId;

    public ChatDialogs(App app) => this.app = app;

    /// <summary>The dialog open now (QA).</summary>
    internal ChatDialog? Shown => shown;

    /// <summary>
    /// Shows <paramref name="d"/> over the library window; <paramref name="channel"/> is the channel it acts on, or
    /// <paramref name="deviceId"/> the private conversation's device. One at a time.
    /// </summary>
    internal void Show(ChatDialog d, SyncChannel? channel, string? deviceId = null)
    {
        if (open is not null || app.LibraryWindow.Content.XamlRoot is not XamlRoot root)
        {
            return;
        }
        var content = new StackPanel { Spacing = 10, MinWidth = 360 };
        if (d.Message.Length > 0)
        {
            content.Children.Add(new TextBlock { Text = d.Message, TextWrapping = TextWrapping.Wrap });
        }
        var problem = new TextBlock
        {
            FontSize = 12,
            TextWrapping = TextWrapping.Wrap,
            Visibility = Visibility.Collapsed,
            Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["SystemFillColorCautionBrush"],
        };
        var dialog = new ContentDialog
        {
            XamlRoot = root,
            Title = d.Title,
            Content = content,
            PrimaryButtonText = d.ConfirmTitle,
            CloseButtonText = d.CancelTitle,
            DefaultButton = ContentDialogButton.Primary,
        };
        if (d.Destructive)
        {
            RowCommands.Destructive(dialog);
        }
        field = null;
        if (d.Field is ChatDialogField f)
        {
            var box = new TextBox { Header = f.Label.Length > 0 ? f.Label : null, PlaceholderText = f.Placeholder, Text = f.Text };
            box.TextChanged += (_, _) => Check();
            content.Children.Add(box);
            content.Children.Add(problem);
            field = box;
            dialog.Opened += (_, _) => { box.Focus(FocusState.Programmatic); box.SelectAll(); };
        }
        void Check()
        {
            var input = TrsFfiMethods.ChatDialogCheck(d, field?.Text ?? "");
            dialog.IsPrimaryButtonEnabled = input is not ChatDialogInput.Problem;
            problem.Text = input is ChatDialogInput.Problem p ? p.Text : "";
            // Shown only once something was typed: an empty field that opened empty needs no sentence yet.
            problem.Visibility = input is ChatDialogInput.Problem && (field?.Text.Length ?? 0) > 0 ? Visibility.Visible : Visibility.Collapsed;
        }
        Check();
        dialog.PrimaryButtonClick += (_, _) => Run(d, field?.Text ?? "", channel, deviceId);
        open = dialog;
        shown = d;
        this.channel = channel;
        this.deviceId = deviceId;
        AppLog.Write($"chat dialog: {d.Kind} \"{d.Title}\"");
        _ = ShowAsync(dialog);
    }

    async Task ShowAsync(ContentDialog dialog)
    {
        try { await dialog.ShowAsync(); } catch (Exception e) { AppLog.Write($"chat dialog failed: {e.Message}"); }
        // Closed: forgotten, unless it was already and another has opened since.
        if (open == dialog)
        {
            Forget();
        }
    }

    void Forget()
    {
        open = null;
        field = null;
        shown = null;
    }

    /// <summary>The dialog's action with what was typed: the core call its kind names; a failure is an alert.</summary>
    void Run(ChatDialog d, string text, SyncChannel? channel, string? deviceId)
    {
        var input = TrsFfiMethods.ChatDialogCheck(d, text);
        if (input is not ChatDialogInput.Ready ready)
        {
            AppLog.Write($"chat dialog {d.Kind}: {(input is ChatDialogInput.Unchanged ? "unchanged" : "refused")}");
            return;
        }
        var core = app.Core;
        var value = ready.Value;
        // What the action calls (in order, off the UI thread), and what follows once it returned.
        Action? after = null;
        Func<Task>? work = d.Kind switch
        {
            ChatDialogKind.NewTopic when channel is not null => async () =>
            {
                var topic = await core.ChatNewTopic(channel.Id, value ?? "");
                after = () => app.Library.SelectPlace(new LibraryPlace.Chat(channel.Id, topic.Id));
            },
            ChatDialogKind.RenameChannel when channel is not null => () => { core.ChatRename(channel.Id, value ?? ""); return Task.CompletedTask; },
            ChatDialogKind.WakeCommand when channel is not null => () => { core.ChatSetWakeCommand(channel.Id, value); return Task.CompletedTask; },
            ChatDialogKind.LeaveChannel when channel is not null => () => core.ChatLeave(channel.Id),
            ChatDialogKind.DeleteConversation when deviceId is not null => async () =>
            {
                await core.ChatDeleteDirect(deviceId);
                after = () => app.Library.SelectPlace(new LibraryPlace.FolderSync());
            },
            ChatDialogKind.ChannelAgents when channel is not null => () => { core.ChatSetAgents(channel.Id, !channel.Agents); return Task.CompletedTask; },
            _ => null,
        };
        if (work is null)
        {
            AppLog.Write($"chat dialog {d.Kind}: nothing to act on");
            return;
        }
        bool done = true;
        CoreCalls.InOrder($"chat dialog {d.Kind}", work,
            () =>
            {
                if (done) AppLog.Write($"chat dialog {d.Kind}: done");
                after?.Invoke();
                app.LibraryWindowIfCreated?.Pane.Refresh();
            },
            e => { done = false; app.Alert(new SyncAlert(d.FailureTitle, e)); });
    }

    /// <summary>QA: types into the open dialog's field.</summary>
    internal void QaType(string text)
    {
        if (field is not null)
        {
            field.Text = text;
        }
    }

    /// <summary>QA: the action button's click (it runs the action, then the dialog closes); false while it is disabled.</summary>
    internal bool QaConfirm()
    {
        if (open is not ContentDialog d || shown is not ChatDialog dialog || !d.IsPrimaryButtonEnabled)
        {
            return false;
        }
        Run(dialog, field?.Text ?? "", channel, deviceId);
        d.Hide();
        Forget();
        return true;
    }

    internal void QaCancel()
    {
        open?.Hide();
        Forget();
    }
}
