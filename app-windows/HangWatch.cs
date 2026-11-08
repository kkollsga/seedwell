using System.Diagnostics;
using Microsoft.UI.Dispatching;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// When the UI thread was busy with the renderer's own captures (RenderTargetBitmap, for a render or a QA shot): the work
/// it was asked to do, not the app holding the user up, so the watchdog leaves it out of a stutter (the Mac's
/// RendererCaptures). A stall is a stall all the same, and the app's own work between two captures still counts. A
/// capture here spans awaits: RenderTargetBitmap draws in a frame of its own. Times in <see cref="HangWatch.Now"/> seconds.
/// </summary>
sealed class RendererCaptures
{
    /// <summary>The renderer's and QA's (the app's watchdog reads it).</summary>
    internal static readonly RendererCaptures Shared = new();

    readonly object gate = new();
    readonly List<(double From, double To)> spans = [];
    double open;
    int depth;

    /// <summary>A capture from now until the result is disposed.</summary>
    internal IDisposable Capture()
    {
        lock (gate)
        {
            if (depth++ == 0) open = HangWatch.Now;
        }
        return new End(this);
    }

    sealed class End(RendererCaptures owner) : IDisposable
    {
        public void Dispose()
        {
            var end = HangWatch.Now;
            lock (owner.gate)
            {
                if (--owner.depth > 0) return;
                owner.spans.Add((owner.open, end));
                // A minute is more than any stutter window looks back.
                owner.spans.RemoveAll(s => s.To < end - 60);
            }
        }
    }

    /// <summary>The captures that ran after <paramref name="since"/>, up to <paramref name="now"/> (one still running: until now).</summary>
    internal (double From, double To)[] Spans(double since, double now)
    {
        lock (gate)
        {
            var all = spans.Where(s => s.To > since).ToList();
            if (depth > 0) all.Add((open, now));
            return [.. all];
        }
    }
}

/// <summary>What late beats came to: one stall, or many short ones in a row.</summary>
abstract record HangLate
{
    /// <summary>The UI thread did not answer for this many milliseconds.</summary>
    internal sealed record Stall(ulong Ms) : HangLate;

    /// <summary>The UI thread was held up for this much of the window, in stalls each too short to be one: the app feels frozen while no single stall shows.</summary>
    internal sealed record Stutter(ulong BusyMs, ulong WindowMs) : HangLate;
}

/// <summary>
/// When a beat of the UI thread came late: the watchdog's bookkeeping, apart from threads and clocks (the self-test drives
/// it with its own times, in seconds). The Mac's HangDetector, docs/CROSS-PLATFORM.md §1.
/// </summary>
sealed class HangDetector(double report, double? fail, double stutterWindow = 3, double busyShare = 0.6)
{
    internal enum TurnKind { Ping, Wait, Fail }

    double? askedAt;
    bool failed;
    /// <summary>The short stalls in the last window, each from its ping to its beat: only the time the UI thread surely was busy (a beat waiting for it) is counted, never more.</summary>
    readonly List<(double From, double To)> held = [];

    /// <summary>The watch thread's turn at <paramref name="now"/>: ask for a beat, wait for the one on its way, or (Fail, how many ms) the run fails now.</summary>
    internal (TurnKind Kind, ulong Ms) Turn(double now)
    {
        if (askedAt is not double asked)
        {
            askedAt = now;
            return (TurnKind.Ping, 0);
        }
        if (fail is double limit && !failed && now - asked >= limit)
        {
            failed = true;
            return (TurnKind.Fail, (ulong)((now - asked) * 1000));
        }
        return (TurnKind.Wait, 0);
    }

    /// <summary>
    /// The UI thread gave the beat at <paramref name="now"/>: a stall (this beat was that late), a stutter (the short
    /// stalls of the last window add up to the share of it: said once, the next one needs a span of its own), or null.
    /// <paramref name="excused"/>: the renderer's own captures, left out of a stutter's time; a stall counts whole.
    /// </summary>
    internal HangLate? Beat(double now, (double From, double To)[]? excused = null)
    {
        if (askedAt is not double asked) return null;
        askedAt = null;
        failed = false;
        var late = now - asked;
        if (late >= report)
        {
            // Said as a stall: the time around it is not said again as a stutter.
            held.Clear();
            return new HangLate.Stall((ulong)(late * 1000));
        }
        if (late <= 0) return null;
        var start = now - stutterWindow;
        held.Add((asked, now));
        held.RemoveAll(h => h.To <= start);
        double busy = 0;
        foreach (var h in held)
        {
            var from = Math.Max(h.From, start);
            // The captures in it (they never overlap: each runs on the UI thread).
            var own = (excused ?? []).Sum(c => Math.Max(0, Math.Min(h.To, c.To) - Math.Max(from, c.From)));
            busy += Math.Max(0, h.To - from - own);
        }
        if (busy < busyShare * stutterWindow) return null;
        held.Clear();
        return new HangLate.Stutter((ulong)Math.Round(busy * 1000), (ulong)Math.Round(stutterWindow * 1000));
    }
}

/// <summary>
/// The hang watchdog (docs/CROSS-PLATFORM.md §1, the Mac's HangWatchdog): a thread of its own asks the UI thread for a
/// beat every <c>ping_ms</c>; a beat later than <c>report_ms</c> is a stall, noted with the core's line
/// (<c>Core::ui_stall</c>) once it ends; shorter stalls that hold the UI thread up for <c>stutter_busy_pct</c> of
/// <c>stutter_window_ms</c> are a stutter, noted the same way (<c>Core::ui_stutter</c>), the renderer's own captures left
/// out. In a QA or render run a stall of <c>fail_ms</c> or a stutter fails the run: its line goes to the log and stderr
/// and the run's exit code says so. Policy and words are the core's (<c>ui_watchdog</c>).
/// </summary>
sealed class HangWatch
{
    readonly DispatcherQueue ui;
    readonly Func<Core?> core;
    readonly UiWatchdog policy = SeedwellFfiMethods.UiWatchdog();
    readonly bool testRun;
    readonly HangDetector detector;
    readonly object gate = new();
    /// <summary>The long step seen while the beat on its way waited: by the beat, the step is over, and the stall's line names it all the same.</summary>
    volatile string waitedIn = "";

    /// <summary>The watchdog's clock, in seconds (fine-grained: a short stall is what it measures).</summary>
    internal static double Now => Stopwatch.GetTimestamp() / (double)Stopwatch.Frequency;

    /// <summary>What the user (or a QA command) did last, for the stall's line.</summary>
    internal static string LastAction { get; set; } = "";

    /// <summary>The long step the UI thread is in, if any: a stall's line names it.</summary>
    internal static string Doing { get; set; } = "";

    static string Action => Doing.Length == 0 ? LastAction : $"{LastAction}, in {Doing}";

    /// <summary>A QA or render run had a stall of fail_ms or more, or a stutter (the run fails).</summary>
    internal static bool Failed { get; private set; }

    /// <summary>That stall's or stutter's line: how long, and what was done last.</summary>
    internal static string? FailedLine { get; private set; }

    public HangWatch(DispatcherQueue ui, Func<Core?> core, bool testRun)
    {
        this.ui = ui;
        this.core = core;
        this.testRun = testRun;
        detector = new HangDetector(policy.ReportMs / 1000.0, testRun ? policy.FailMs / 1000.0 : null, policy.StutterWindowMs / 1000.0, policy.StutterBusyPct / 100.0);
        new Thread(Watch) { IsBackground = true, Name = "hang watch" }.Start();
    }

    void Watch()
    {
        while (true)
        {
            (HangDetector.TurnKind Kind, ulong Ms) turn;
            lock (gate) turn = detector.Turn(Now);
            if (turn.Kind == HangDetector.TurnKind.Ping)
            {
                waitedIn = "";
                ui.TryEnqueue(DispatcherQueuePriority.High, Beat);
            }
            else if (Doing is { Length: > 0 } step)
            {
                waitedIn = step;
            }
            else if (turn.Kind == HangDetector.TurnKind.Fail)
            {
                // From here at once: a hang may never end.
                Fail("ui stall", SeedwellFfiMethods.UiStallLine(turn.Ms, Action, true));
            }
            Thread.Sleep((int)policy.PingMs);
        }
    }

    /// <summary>On the UI thread: the beat.</summary>
    void Beat()
    {
        var now = Now;
        var own = RendererCaptures.Shared.Spans(now - policy.StutterWindowMs / 1000.0, now);
        HangLate? late;
        lock (gate) late = detector.Beat(now, own);
        if (late is null) return;
        var action = waitedIn.Length == 0 ? LastAction : $"{LastAction}, in {waitedIn}";
        // A stutter is known only now (a long stall already failed the run from the watch thread).
        if (testRun && late is HangLate.Stutter s) Fail("ui stutter", SeedwellFfiMethods.UiStutterLine(s.BusyMs, s.WindowMs, action, true));
        // Noted off the UI thread (the log's file, the core's developer log).
        _ = Task.Run(() => Note(late, action));
    }

    void Fail(string what, string line)
    {
        if (Failed) return;
        Failed = true;
        FailedLine = line;
        Console.Error.WriteLine(line);
        AppLog.Write($"{what}: {line}");
    }

    void Note(HangLate late, string action)
    {
        try
        {
            switch (late)
            {
                case HangLate.Stall s:
                    AppLog.Write($"ui stall: {core()?.UiStall(s.Ms, action) ?? SeedwellFfiMethods.UiStallLine(s.Ms, action, false)}");
                    break;
                case HangLate.Stutter t:
                    AppLog.Write($"ui stutter: {core()?.UiStutter(t.BusyMs, t.WindowMs, action) ?? SeedwellFfiMethods.UiStutterLine(t.BusyMs, t.WindowMs, action, false)}");
                    break;
            }
        }
        catch (Exception e)
        {
            AppLog.Write($"ui stall not noted: {e.Message}");
        }
    }
}

/// <summary>QA: the detector's promises, ported from the Mac's HangWatchdogTests. Each names the way it can fail. Answers "detector ok", or what failed.</summary>
static class HangDetectorSelfTest
{
    /// <summary>Beats <paramref name="late"/> seconds late, one per ping of 0.25 s from <paramref name="from"/> for <paramref name="seconds"/>: what each beat was said to be.</summary>
    static List<HangLate> Beats(HangDetector d, double from, double seconds, double late, List<string> failures)
    {
        var said = new List<HangLate>();
        var t = from;
        while (t < from + seconds)
        {
            if (d.Turn(t).Kind != HangDetector.TurnKind.Ping) failures.Add($"no ping at {t}");
            if (d.Beat(t + late) is HangLate l) said.Add(l);
            // The next ping at the watch thread's next turn after the beat.
            t += Math.Floor(late / 0.25) * 0.25 + 0.25;
        }
        return said;
    }

    internal static string Run()
    {
        var failures = new List<string>();
        void Expect(bool ok, string what) { if (!ok) failures.Add(what); }

        // A hang that never ends fails the run from the watch thread, once; a stall that ends is said with its length.
        var d = new HangDetector(1.0, 2.0);
        Expect(d.Turn(10).Kind == HangDetector.TurnKind.Ping, "first turn is a ping");
        Expect(d.Turn(11).Kind == HangDetector.TurnKind.Wait, "a beat on its way is waited for");
        Expect(d.Turn(12.5) is (HangDetector.TurnKind.Fail, 2500), "a hang of 2.5 s fails the run");
        Expect(d.Turn(12.75).Kind == HangDetector.TurnKind.Wait, "a hang fails the run once");
        Expect(d.Beat(13) is HangLate.Stall(3000), "the stall that ended is 3 s long");

        // Short stalls in a row are a stutter, said once for its span, never a stall (the Share window after adding a 914 GB folder).
        d = new HangDetector(1.0, 2.0, 3, 0.6);
        var said = Beats(d, 0, 3, 0.8, failures);
        Expect(said.Count == 1 && said[0] is HangLate.Stutter { BusyMs: >= 1800 and <= 3000, WindowMs: 3000 }, $"short stalls in a row: {string.Join(", ", said)}");
        var next = Beats(d, 3, 0.9, 0.8, failures);
        Expect(next.Count == 0, $"said once, the next needs a span of its own: {string.Join(", ", next)}");

        // Busy for less than the share (40 %), or briefly, is no stutter, however long it goes on; nor are on-time beats.
        d = new HangDetector(1.0, 2.0, 3, 0.6);
        Expect(Beats(d, 0, 20, 0.1, failures).Count == 0, "40 % busy is a stutter");
        Expect(Beats(d, 20, 20, 0, failures).Count == 0, "on-time beats are a stutter");
        Expect(Beats(d, 40, 1.2, 0.8, failures).Count == 0, "1.6 s held up of 3 s is a stutter");

        // A long stall's time is not said again as part of a stutter (1.5 s of stall and 0.6 s of short stalls would be 70 %).
        d = new HangDetector(1.0, null, 3, 0.6);
        said = [.. Beats(d, 1, 0.25, 1.5, failures), .. Beats(d, 2.5, 0.75, 0.2, failures)];
        Expect(said.Count == 1 && said[0] is HangLate.Stall(1500), $"a stall is also a stutter: {string.Join(", ", said)}");

        // A run failing as a stutter while it does what it was asked (captures of 0.4 s with 50 ms between): not a stutter.
        d = new HangDetector(1.0, 2.0, 3, 0.6);
        var own = new List<(double From, double To)>();
        said = [];
        var t = 0.0;
        while (t < 6)
        {
            own.Add((t, t + 0.4));
            Expect(d.Turn(t + 0.05).Kind == HangDetector.TurnKind.Ping, "captures: a ping");
            if (d.Beat(t + 0.4, [.. own]) is HangLate l) said.Add(l);
            t += 0.45;
        }
        Expect(said.Count == 0, $"the renderer's own captures were said: {string.Join(", ", said)}");
        // But the exclusion must not swallow the app's own stutter: between short captures the app holds the thread 0.35 s.
        while (t < 12)
        {
            own.Add((t, t + 0.05));
            Expect(d.Turn(t + 0.01).Kind == HangDetector.TurnKind.Ping, "between captures: a ping");
            if (d.Beat(t + 0.4, [.. own]) is HangLate l) said.Add(l);
            t += 0.45;
        }
        Expect(said.Any(l => l is HangLate.Stutter), $"a stutter between captures went unsaid: {string.Join(", ", said)}");
        // A capture as long as a stall is still a stall.
        Expect(d.Turn(20).Kind == HangDetector.TurnKind.Ping, "a long capture: a ping");
        Expect(d.Beat(21.5, [.. own, (19.9, 21.5)]) is HangLate.Stall(1500), "a capture as long as a stall is not a stall");

        return failures.Count == 0 ? "detector ok" : $"detector failed: {string.Join(" || ", failures)}";
    }
}
