using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The core's events (the Mac's handle(_ event:)). Called on the core's threads; what the app does with one runs on the
/// UI thread: settings changes apply the settings the host implements itself and feed the settings banner; the
/// transfer, choice, add and agent events go to the core's Notices (Notifier), which says what to post or open; an add
/// request and a change of locations ask their questions (Prompts).
/// </summary>
sealed class Listener(App app) : CoreListener
{
    public void OnSettingsChanged(string file, string[] warnings, string[] restartRequired)
    {
        AppLog.Write($"settings changed: {file}");
        app.Dispatch(() => app.Banner.RestartRequired(restartRequired));
        if (file == "config.toml")
        {
            app.SettingsChanged();
        }
    }

    public void OnSettingsRejected(string file, string error)
    {
        AppLog.Write($"settings rejected: {file}: {error}");
        app.Dispatch(() => app.Banner.Problem(error));
    }

    void Notice(NoticeEvent e) => app.Dispatch(() => app.Notifier?.Event(e));

    public void OnTransferFinished(TransferRow row) => Notice(new NoticeEvent.Finished(row));
    public void OnError(string? id, string message) => Notice(new NoticeEvent.Error(id, message));
    public void OnGoalReached(TransferRow row) => Notice(new NoticeEvent.GoalReached(row));
    public void OnNeedsChoice(TransferRow row) => app.Dispatch(() => app.NeedsChoice(row));
    public void OnAutoAdded(AutoAdded added) => Notice(new NoticeEvent.AutoAdded(added));
    public void OnDuplicate(string id, string name, uint trackersAdded) => Notice(new NoticeEvent.Duplicate(id, name, trackersAdded));
    public void OnAddRequest(AddRequestInfo request) => app.Dispatch(() => app.Prompts.AddRequest(request));
    // A request coming, answered (also by a chat reply) or expiring is an agent activity: the icon's dot and the request
    // toasts follow it at once (the Mac's).
    public void OnAgentActivity(AgentActivity activity) => app.Dispatch(() =>
    {
        app.Notifier?.Event(new NoticeEvent.Agent(activity));
        app.AgentActivity();
    });
    public void OnLocationsChanged(uint torrents, ulong bytes) => app.Dispatch(() => app.Prompts.LocationsChanged());
    public void OnAttentionChanged() { }
    // A torrent began or ended waiting to start: the notifier looks again.
    public void OnPendingChanged() => app.Dispatch(() => app.StartingNotifier?.Refresh());
}
