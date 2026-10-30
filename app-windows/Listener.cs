using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The core's events (the Mac's handle(_ event:)). Called on the core's threads; what the app does with one runs on the
/// UI thread. For now only settings changes are used: a change to config.toml by the app, an agent, the CLI or a hand
/// edit applies the settings the host implements itself. NEED: the notifications the Mac shows for the other events
/// (finished, goal reached, needs a choice, auto-added, duplicate, errors, agent activity).
/// </summary>
sealed class Listener(App app) : CoreListener
{
    public void OnSettingsChanged(string file, string[] warnings, string[] restartRequired)
    {
        AppLog.Write($"settings changed: {file}");
        if (file == "config.toml")
        {
            app.SettingsChanged();
        }
    }

    public void OnSettingsRejected(string file, string error) => AppLog.Write($"settings rejected: {file}: {error}");

    public void OnTransferFinished(TransferRow row) { }
    public void OnError(string? id, string message) { }
    public void OnGoalReached(TransferRow row) { }
    public void OnNeedsChoice(TransferRow row) { }
    public void OnAutoAdded(AutoAdded added) { }
    public void OnDuplicate(string name, uint trackersAdded) { }
    public void OnAddRequest(AddRequestInfo request) { }
    public void OnAgentActivity(AgentActivity activity) { }
    public void OnLocationsChanged(uint torrents, ulong bytes) { }
    public void OnAttentionChanged() { }
    public void OnPendingChanged() { }
}
