using Microsoft.Windows.AppNotifications;
using Microsoft.Windows.AppNotifications.Builder;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// Notifications on Windows (the Mac's Notifier), from the core's <c>Notices</c> (seedwell-ffi notify.rs): the core's events
/// and, every <c>chat_poll_ms</c>, the new chat messages, each turned by Rust into a notice to post or a place to open.
/// Every rule (the Notifications settings, hidden trackers, the finished burst, muted channels, the conversation on
/// screen) and every word is the core's; this posts Windows notifications (a key the core gives replaces the earlier
/// notice: the toast's tag) and, on a click, opens the notice's route (<c>notice_route</c>). A QA or render run posts
/// nothing: it logs each notice and keeps it for the QA command <c>notices</c>.
/// </summary>
sealed class Notifier
{
    readonly App app;
    readonly Notices notices;
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer chat;
    bool posting;

    /// <summary>The notices posted in this run, newest last (QA).</summary>
    internal List<Notice> Posted { get; } = [];

    public Notifier(App app, Core core, Microsoft.UI.Dispatching.DispatcherQueue ui)
    {
        this.app = app;
        notices = new Notices(core);
        posting = !Quiet.On && AppNotificationManager.IsSupported();
        if (posting)
        {
            try
            {
                // A click while the app runs comes here (on a thread of the system's): its route, on the UI thread.
                AppNotificationManager.Default.NotificationInvoked += (_, e) =>
                {
                    var click = e.Arguments.TryGetValue("click", out var c) ? c : "";
                    var action = e.Arguments.TryGetValue("action", out var a) ? a : null;
                    ui.TryEnqueue(() => Clicked(click, action));
                };
                AppNotificationManager.Default.Register();
            }
            catch (Exception e)
            {
                AppLog.Write($"notifications: not registered ({e.Message}); the tray's balloon instead");
                posting = false;
            }
        }
        chat = ui.CreateTimer();
        chat.Interval = TimeSpan.FromMilliseconds(notices.Model().ChatPollMs);
        chat.Tick += (_, _) => PollChat();
        chat.Start();
        // Request toasts an earlier run left whose request no longer waits.
        WithdrawStale();
    }

    /// <summary>A core event (hopped onto the UI thread by the listener): the core says post, open or nothing.</summary>
    internal void Event(NoticeEvent e)
    {
        NoticeOutcome outcome;
        try { outcome = notices.Event(e); }
        catch (Exception ex) { AppLog.Write($"notice for {e.GetType().Name} failed: {ex.Message}"); return; }
        switch (outcome)
        {
            case NoticeOutcome.Post post:
                Post(post.Notice);
                break;
            case NoticeOutcome.Open open:
                Route(open.Route);
                break;
        }
    }

    /// <summary>New chat messages since the last look (the window's place and whether it is in front decide what is on screen).</summary>
    void PollChat()
    {
        var main = app.LibraryWindowIfCreated;
        var shown = main is not null && main.AppWindow.IsVisible;
        var view = new NoticeView(shown && main!.IsInFront, shown ? main!.Pane.Model?.Place : null);
        _ = Task.Run(() => notices.Chat(view)).ContinueWith(t => app.Dispatch(() =>
        {
            if (!t.IsCompletedSuccessfully)
            {
                AppLog.Write($"chat notices failed: {t.Exception?.InnerException?.Message}");
                return;
            }
            foreach (var n in t.Result) Post(n);
        }));
    }

    /// <summary>
    /// A notice's click on one line: the core's click can hold a line break (a transfer's id on the line after its kind),
    /// which neither a log line nor a toast's arguments carry. <see cref="Clicked"/> takes it back.
    /// </summary>
    internal static string ClickArg(string click) => click.Replace("%", "%25").Replace("\n", "%0A");

    /// <summary>Posts a notice the core made (also one a host asked for, such as an on-demand file that is not here yet).</summary>
    internal void Post(Notice n)
    {
        AppLog.Write($"notice: {n.Kind}: {n.Title} | {n.Body} [{n.Key ?? "-"}] {{{ClickArg(n.Click)}}}");
        Posted.Add(n);
        if (Posted.Count > 50) Posted.RemoveAt(0);
        if (Quiet.On)
        {
            return;
        }
        if (!posting)
        {
            app.Notify(n.Title, n.Body);
            return;
        }
        try
        {
            var builder = new AppNotificationBuilder()
                .AddArgument("click", ClickArg(n.Click))
                .AddText(n.Title)
                .AddText(n.Body);
            // An agent's request stays on screen until it is answered or dismissed (a reminder; it has its two buttons).
            // TimeSensitive is a default toast until a notice needs more.
            if (n.Urgency == NoticeUrgency.Persistent) builder.SetScenario(AppNotificationScenario.Reminder);
            // The notice's buttons (an agent's request: Approve, Deny), each with its action for notice_decision.
            foreach (var a in n.Actions)
            {
                var button = new AppNotificationButton(a.Title).AddArgument("click", ClickArg(n.Click)).AddArgument("action", a.Id);
                if (a.Destructive) button.SetButtonStyle(AppNotificationButtonStyle.Critical);
                builder.AddButton(button);
            }
            var toast = builder.BuildNotification();
            if (n.Key is string key)
            {
                // A later notice with the same key replaces this one; a tag holds 64 characters, so a longer key is hashed.
                toast.Tag = key.Length <= 64 ? key : Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(System.Text.Encoding.UTF8.GetBytes(key)))[..64];
                toast.Group = "seedwell";
            }
            AppNotificationManager.Default.Show(toast);
        }
        catch (Exception e)
        {
            AppLog.Write($"notice not shown: {e.Message}");
        }
    }

    /// <summary>
    /// A click on a notice (also one of an older version, which opens the window), or on one of its buttons
    /// (<paramref name="action"/>): an agent's request is approved or denied by the core, and its answer posted.
    /// </summary>
    internal void Clicked(string click, string? action = null)
    {
        click = Uri.UnescapeDataString(click);
        AppLog.Write($"notice clicked{(action is null ? "" : $": {action}")}");
        if (action is not null && SeedwellFfiMethods.NoticeDecision(click, action) is NoticeDecision d)
        {
            Decide(d.RequestId, d.Approve);
            return;
        }
        Route(SeedwellFfiMethods.NoticeRoute(click));
    }

    /// <summary>An agent's request approved or denied (a notice's button, the tray, Settings): the core decides, its answer is posted.</summary>
    internal void Decide(string requestId, bool approve, Action? then = null)
    {
        CoreCalls.InOrder("agent request", () => app.Core.AgentRequestDecide(requestId, approve), answer =>
        {
            AppLog.Write($"agent request {(approve ? "approved" : "denied")}: {answer.Title}");
            if (!Quiet.On) app.Notify(answer.Title, answer.Body);
            WithdrawStale();
            then?.Invoke();
        }, e => app.Alert(new SyncAlert(e, "")));
    }

    /// <summary>
    /// Takes away the toasts whose request no longer waits (answered anywhere, expired, withdrawn): of the tags still in
    /// Action Center, the core names the ones to remove (<c>Notices.withdrawn</c>; never another notice's). At launch,
    /// after each agent activity and after each answer the app gave; off the UI thread.
    /// </summary>
    internal void WithdrawStale()
    {
        if (!posting) return;
        _ = Task.Run(async () =>
        {
            try
            {
                var shown = await AppNotificationManager.Default.GetAllAsync();
                foreach (var tag in notices.Withdrawn(shown.Select(t => t.Tag).Where(t => !string.IsNullOrEmpty(t)).ToArray()))
                {
                    await AppNotificationManager.Default.RemoveByTagAndGroupAsync(tag, "seedwell");
                    AppLog.Write($"notice withdrawn: {tag}");
                }
            }
            catch (Exception e)
            {
                AppLog.Write($"notices not withdrawn: {e.Message}");
            }
        });
    }

    /// <summary>What a notice opens (the core's table).</summary>
    internal void Route(NoticeRoute route)
    {
        AppLog.Write($"notice route: {route.GetType().Name}");
        switch (route)
        {
            case NoticeRoute.Transfer t:
                app.ShowTransfer(t.Id);
                break;
            case NoticeRoute.Chooser c:
                ChooserDialog.Show(app, c.Id);
                break;
            case NoticeRoute.Starting s:
                if (app.StartingNotifier is StartingWindow starting) starting.Reveal(s.Id); else app.ShowTransfer(s.Id);
                break;
            case NoticeRoute.Place p:
                app.ShowPlace(p.PlaceValue);
                break;
            case NoticeRoute.AgentRequests:
                // Settings › Agents, where the requests are listed.
                app.ShowPlace(new LibraryPlace.Settings());
                app.LibraryWindow.Pane.SettingsPageShown?.Show(SettingsTab.Agents);
                break;
            default:
                app.ShowMainWindow();
                break;
        }
    }

    /// <summary>Uninstall (Seedwell.exe --unregister): the notifications' registration goes with the app's others.</summary>
    internal static void Unregister()
    {
        try
        {
            if (AppNotificationManager.IsSupported()) AppNotificationManager.Default.UnregisterAll();
        }
        catch (Exception e)
        {
            AppLog.Write($"notifications: not unregistered ({e.Message})");
        }
    }
}
