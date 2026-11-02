using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The questions the core asks (the Mac's alerts; trs-ffi prompts.rs), each a dialog over the library window with the
/// core's words: a watch folder's "Ask first" file (Add / Don't Add), torrents a change of locations leaves elsewhere
/// (Move / Don't Move, one question at a time), and the backlog offer at start (asked once ever). The first button runs
/// the core's call, in order, off the UI thread. A QA run shows no dialog: it logs the question and waits for the QA
/// command <c>prompt</c> to answer it.
/// </summary>
sealed class Prompts(App app)
{
    bool locationOpen;
    // QA: the question waiting for an answer, and what answers it.
    TaskCompletionSource<bool>? qaAnswer;

    /// <summary>The question waiting in a QA run (its title), or null.</summary>
    internal string? Waiting { get; private set; }

    /// <summary>QA: answers the waiting question with its first button (yes) or its second.</summary>
    internal bool QaAnswer(bool yes)
    {
        if (qaAnswer is null) return false;
        qaAnswer.TrySetResult(yes);
        return true;
    }

    /// <summary>The question: true on the first button.</summary>
    async Task<bool> Ask(string title, string text, string yes, string no)
    {
        AppLog.Write($"prompt: {title} | {text} [{yes} / {no}]");
        if (Quiet.On)
        {
            Waiting = title;
            qaAnswer = new TaskCompletionSource<bool>();
            var answer = await qaAnswer.Task;
            qaAnswer = null;
            Waiting = null;
            AppLog.Write($"prompt answered: {(answer ? yes : no)}");
            return answer;
        }
        if (await AddFolderDialog.Root(app) is not XamlRoot root)
        {
            return false;
        }
        var d = new ContentDialog
        {
            XamlRoot = root,
            Title = title,
            Content = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap },
            PrimaryButtonText = yes,
            CloseButtonText = no,
            DefaultButton = ContentDialogButton.Primary,
        };
        try { return await d.ShowAsync() == ContentDialogResult.Primary; }
        catch (Exception e) { AppLog.Write($"prompt failed: {e.Message}"); return false; }
    }

    /// <summary>A watch folder's file that asks first (on_add_request).</summary>
    internal async void AddRequest(AddRequestInfo request)
    {
        var p = TrsFfiMethods.AddRequestPrompt(request);
        if (await Ask(p.Title, p.Text, p.AddTitle, p.DontAddTitle))
        {
            app.Add([p.Path]);
        }
    }

    /// <summary>
    /// The locations changed (on_locations_changed): the torrents now elsewhere than the settings say, moved on Move.
    /// One question at a time: a change while it is open is dropped.
    /// </summary>
    internal async void LocationsChanged()
    {
        if (locationOpen)
        {
            return;
        }
        locationOpen = true;
        try
        {
            MoveProposalInfo proposal;
            try { proposal = await app.Core.LocationMoves(); }
            catch (Exception e) { AppLog.Write($"location moves failed: {CoreCalls.Message(e)}"); return; }
            if (TrsFfiMethods.LocationMovePrompt(proposal) is not LocationMovePrompt p)
            {
                return;
            }
            if (!await Ask(p.Title, p.Text, p.MoveTitle, p.DontMoveTitle))
            {
                return;
            }
            CoreCalls.InOrder("move torrents", () => app.Core.MoveTorrents(p.Ids), r =>
            {
                if (TrsFfiMethods.LocationMoveResult(r) is SyncAlert alert) app.Alert(alert);
                app.Refreshed();
            }, e => app.Alert(new SyncAlert(e, "")));
        }
        finally
        {
            locationOpen = false;
        }
    }

    /// <summary>At start: the files already in the watch folders, offered once ever (the core keeps that it asked).</summary>
    internal async void Backlog()
    {
        BacklogPrompt? p;
        try { p = await app.Core.BacklogPrompt(); }
        catch (Exception e) { AppLog.Write($"backlog prompt failed: {CoreCalls.Message(e)}"); return; }
        if (p is null)
        {
            return;
        }
        if (await Ask(p.Offer.Title, p.Offer.Text, p.Offer.AddTitle, p.Offer.LaterTitle))
        {
            app.Add(p.Files);
        }
    }
}
