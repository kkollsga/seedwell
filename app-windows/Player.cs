using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// Play (Part F; the Mac's PlayerLauncher): the core decides which video and which player (<c>Core.play</c>, off the UI
/// thread: it can wait for a starting torrent's stream) and answers with what to do (<c>PlayStart</c>): a calm note at
/// the bottom of the window, or a player to start. Windows starts only <c>PlayLaunch.Run</c> (the app and its
/// arguments, each one argument); OpenFile and OpenUrl become the same. A QA or render run starts no player: it logs
/// the target instead (the Mac's blocked switch).
/// </summary>
static class Player
{
    /// <summary>Plays <paramref name="row"/>'s video (<paramref name="fileIndex"/>: one file; null: the row's own) with
    /// <paramref name="playerId"/> (null: the setting's).</summary>
    internal static void Play(App app, TransferRow row, uint? fileIndex = null, string? playerId = null)
    {
        AppLog.Write($"play {row.Id[..Math.Min(8, row.Id.Length)]} file {fileIndex?.ToString() ?? "-"} with {playerId ?? "the setting's"}");
        _ = Task.Run(() => app.Core.Play(row, fileIndex, playerId)).ContinueWith(t => app.Dispatch(() =>
        {
            if (!t.IsCompletedSuccessfully)
            {
                var e = t.Exception?.InnerException;
                Note(app, e is CoreException ce ? SeedwellFfiMethods.CoreErrorMessage(ce) : e?.Message ?? "");
                return;
            }
            switch (t.Result)
            {
                case PlayStart.Note n: Note(app, n.Text); break;
                case PlayStart.Launch l: Launch(app, l.LaunchValue, l.Player); break;
            }
        }));
    }

    static void Note(App app, string text)
    {
        AppLog.Write($"play note: {text}");
        app.ShowMainWindow();
        app.LibraryWindow.Pane.ShowNote(text);
    }

    static void Launch(App app, PlayLaunch launch, string player)
    {
        var (exe, args) = launch switch
        {
            PlayLaunch.Run r => (r.App, r.Arguments),
            PlayLaunch.OpenFile f => (f.App, new[] { f.Path }),
            PlayLaunch.OpenUrl u => (u.App, new[] { u.Url }),
            _ => ("", Array.Empty<string>()),
        };
        AppLog.Write($"play-url: {exe} {string.Join(' ', args)}");
        if (Quiet.On) return;
        var start = new System.Diagnostics.ProcessStartInfo(exe) { UseShellExecute = false };
        foreach (var a in args) start.ArgumentList.Add(a);
        Shell.Start(start, why => app.Dispatch(() => Note(app, SeedwellFfiMethods.PlayLaunchFailed(player, why))));
    }
}
