using System.Diagnostics;

namespace Seedwell;

/// <summary>
/// Starting another program, off the UI thread (docs/CROSS-PLATFORM.md §1: the app never hangs): the shell can take its
/// time (Explorer busy, a file type's handler asking, a folder on a drive asleep). What the UI thread needs of the result
/// it is told; a start that fails is logged, and no longer an exception on the UI thread.
/// </summary>
static class Shell
{
    /// <summary>QA only (an isolated run): a start takes this long (SEEDWELL_QA_SLOW_SHELL_MS).</summary>
    static readonly int SlowMs = Program.StateDir is not null && int.TryParse(Environment.GetEnvironmentVariable("SEEDWELL_QA_SLOW_SHELL_MS"), out var ms) ? ms : 0;

    /// <summary>Starts <paramref name="start"/>; <paramref name="failed"/>, from another thread, has the reason it could not.</summary>
    internal static void Start(ProcessStartInfo start, Action<string>? failed = null)
    {
        _ = Task.Run(() =>
        {
            try
            {
                if (SlowMs > 0) Thread.Sleep(SlowMs);
                using var p = Process.Start(start);
                if (SlowMs > 0) AppLog.Write($"shell: started {start.FileName}");
            }
            catch (Exception e)
            {
                AppLog.Write($"shell: {start.FileName} could not start: {e.Message}");
                failed?.Invoke(e.Message);
            }
        });
    }
}
