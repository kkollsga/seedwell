using Microsoft.UI.Dispatching;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The hang watchdog (docs/CROSS-PLATFORM.md §1, the Mac's HangWatchdog): a thread of its own asks the UI thread for a
/// beat every <c>ping_ms</c>; a beat later than <c>report_ms</c> is a stall, noted with the core's line
/// (<c>Core::ui_stall</c>) once it ends. In a QA or render run a stall of <c>fail_ms</c> fails the run: its line goes to
/// the log and stderr and the run's exit code says so. Policy and words are the core's (<c>ui_watchdog</c>).
/// </summary>
sealed class HangWatch
{
    readonly DispatcherQueue ui;
    readonly Func<Core?> core;
    readonly UiWatchdog policy = TrsFfiMethods.UiWatchdog();
    readonly bool testRun;
    long beat = Environment.TickCount64;
    long worst;

    /// <summary>What the user (or a QA command) did last, for the stall's line.</summary>
    internal static string LastAction { get; set; } = "";

    /// <summary>A QA or render run had a stall of fail_ms or more (the run fails).</summary>
    internal static bool Failed { get; private set; }

    /// <summary>That stall's line: how long, and what was done last.</summary>
    internal static string? FailedLine { get; private set; }

    public HangWatch(DispatcherQueue ui, Func<Core?> core, bool testRun)
    {
        this.ui = ui;
        this.core = core;
        this.testRun = testRun;
        new Thread(Watch) { IsBackground = true, Name = "hang watch" }.Start();
    }

    void Watch()
    {
        while (true)
        {
            Thread.Sleep((int)policy.PingMs);
            ui.TryEnqueue(DispatcherQueuePriority.High, () => Interlocked.Exchange(ref beat, Environment.TickCount64));
            var late = Environment.TickCount64 - Interlocked.Read(ref beat);
            // A beat is asked for every ping: what is late beyond one ping is the UI thread not answering.
            late -= policy.PingMs;
            if (late >= policy.ReportMs)
            {
                worst = Math.Max(worst, late);
                if (testRun && late >= policy.FailMs && !Failed)
                {
                    Failed = true;
                    var line = TrsFfiMethods.UiStallLine((ulong)late, LastAction, true);
                    FailedLine = line;
                    Console.Error.WriteLine(line);
                    AppLog.Write($"ui stall: {line}");
                }
                continue;
            }
            if (worst > 0)
            {
                // The stall ended: noted once, with how long the UI thread did not answer.
                var stalled = (ulong)worst;
                worst = 0;
                try
                {
                    var line = core()?.UiStall(stalled, LastAction) ?? TrsFfiMethods.UiStallLine(stalled, LastAction, false);
                    AppLog.Write($"ui stall: {line}");
                }
                catch (Exception e)
                {
                    AppLog.Write($"ui stall not noted: {e.Message}");
                }
            }
        }
    }
}
