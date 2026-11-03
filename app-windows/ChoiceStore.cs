using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The one chooser per torrent that the starting notifier, the chooser window and the list under a held row share
/// (SPEC 7A.6), as the core keeps them (<c>Choices</c>, trs-ffi chooser.rs): its ticks, Start, Cancel and a category
/// change are the core's; the Mac's ChoiceStore. This only tells what draws a chooser that it changed, shows Cancel's
/// quiet note and tells the app that a torrent was resolved.
/// </summary>
sealed class ChoiceStore
{
    readonly App app;

    internal Choices Choices { get; }

    /// <summary>A chooser changed (a tick, a fold, a category, Start or Cancel): whatever draws one reads it again.</summary>
    internal event Action? Changed;

    /// <summary>Start or Cancel resolved a torrent: a chooser window open for it closes.</summary>
    internal event Action<string>? Resolved;

    public ChoiceStore(App app)
    {
        this.app = app;
        Choices = new Choices(app.Core);
    }

    /// <summary>The shared chooser of <paramref name="id"/>, loaded the first time (<paramref name="reopen"/>: a running torrent's Choose…).</summary>
    internal Chooser? Chooser(string id, bool reopen = false)
    {
        var had = Choices.Loaded(id) is not null;
        var c = Choices.Chooser(id, reopen);
        if (!had && c is not null) OnChanged();
        return c;
    }

    /// <summary>The shared chooser, loaded off the UI thread when there is none yet (a window about to open, the held list).</summary>
    internal async Task<Chooser?> Load(string id, bool reopen = false)
    {
        var had = Choices.Loaded(id) is not null;
        Chooser? c = null;
        try { c = await Task.Run(() => Choices.Load(id, reopen)); }
        catch (Exception e) { AppLog.Write($"chooser of {Short(id)} not loaded: {e.Message}"); }
        if (!had && c is not null) OnChanged();
        return c;
    }

    /// <summary>The chooser of <paramref name="id"/> when one is loaded (never loads).</summary>
    internal Chooser? Loaded(string id) => Choices.Loaded(id);

    /// <summary>Cancel would move a .torrent file to the Recycle Bin (so Shift has something to keep).</summary>
    internal bool HasSource(string id) => Choices.HasSource(id);

    /// <summary>The chooser of <paramref name="id"/> as drawn now.</summary>
    internal ChooserPage? Page(string id) => Loaded(id)?.View(HasSource(id));

    /// <summary>A click in a chooser: <paramref name="body"/> changes it in the core; everything that shows it draws again.</summary>
    internal void Edit(string id, Action<Chooser> body)
    {
        if (Loaded(id) is not Chooser c) return;
        body(c);
        OnChanged();
    }

    /// <summary>
    /// Start (or a running torrent's Download): the ticks go to the engine. Returns at once; what shows the chooser closes
    /// once the engine took it, or the core's reason is an alert.
    /// </summary>
    internal void Confirm(string id)
    {
        CoreCalls.InOrder("chooser start", () => Choices.Confirm(id), () =>
        {
            OnChanged();
            Resolved?.Invoke(id);
            app.Refreshed();
        }, e =>
        {
            OnChanged();
            app.Alert(new SyncAlert(e, ""));
        });
    }

    /// <summary>Cancel: the held torrent is removed, its .torrent file to the Recycle Bin unless <paramref name="keepFile"/> (Shift).</summary>
    internal void Cancel(string id, bool keepFile)
    {
        CoreCalls.InOrder("chooser cancel", () => Choices.Cancel(id, keepFile), note =>
        {
            OnChanged();
            Resolved?.Invoke(id);
            // A file that could not go never stops the removal: its note is quiet.
            if (note is CancelNote n) app.Notify(n.Title, n.Body);
            app.Refreshed();
        });
    }

    /// <summary>The pill's menu and the chooser's Change: the core plans the torrent again and rebuilds the shared chooser.</summary>
    internal void ChangeCategory(string id, Category category) =>
        CoreCalls.InOrder("chooser category", () => Choices.ChangeCategory(id, category), () => { OnChanged(); app.Refreshed(); });

    /// <summary>Start or Cancel resolved <paramref name="id"/> elsewhere (the notifier): what showed it closes; a Cancel's note is quiet.</summary>
    internal void ResolvedElsewhere(string id, CancelNote? note = null)
    {
        OnChanged();
        Resolved?.Invoke(id);
        if (note is CancelNote n) app.Notify(n.Title, n.Body);
    }

    internal void OnChanged() => Changed?.Invoke();

    static string Short(string id) => id[..Math.Min(8, id.Length)];
}
