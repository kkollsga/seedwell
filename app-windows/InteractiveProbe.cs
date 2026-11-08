using System.Runtime.InteropServices;
using Microsoft.UI.Dispatching;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// QA: how interactive the app stays, measured finely enough to show what the watchdog (a report at 1 s) never says.
/// A beat on the dispatcher, one at a time, 10 ms after the last answered: a beat's delay is how long a click would have
/// waited (at Normal priority, behind the work already queued, as the app's own turns are). A cheap core call the tray and
/// the sync rows read, every 50 ms from a thread of its own: a core lock held for seconds shows even when the UI never waits
/// on it. A cycle over every surface, each command timed. The check <c>interactive</c> in qa\run.ps1 judges the result.
/// </summary>
sealed class InteractiveProbe
{
    const int BeatMs = 10;
    const int CoreMs = 50;

    internal static InteractiveProbe? Current { get; private set; }

    /// <summary>A beat: when, how late, what was done last, whether the renderer's own captures took part of it, and how much of it the garbage collector held every thread.</summary>
    readonly record struct Beat(double At, double Ms, string Action, bool Own, double GcMs);
    readonly record struct CoreCall(double At, double Ms, string Action);
    readonly record struct Timed(double At, double Ms, string Name, string Result, int Full, double GcMs);

    readonly DispatcherQueue ui;
    readonly Core core;
    readonly List<Beat> beats = [];
    readonly List<CoreCall> coreCalls = [];
    readonly List<Timed> commands = [];
    readonly double started = HangWatch.Now;
    readonly TimeSpan pausedAtStart = GC.GetTotalPauseDuration();
    readonly int[] collectionsAtStart = [GC.CollectionCount(0), GC.CollectionCount(1), GC.CollectionCount(2)];
    int coreFailures;
    int coreDelayMs;
    volatile bool stopping;
    volatile string[] folders = [];
    Thread? beatThread;
    Thread? coreThread;

    /// <summary>The cycle, while it runs.</summary>
    internal Task? Cycling { get; private set; }

    InteractiveProbe(DispatcherQueue ui, Core core)
    {
        this.ui = ui;
        this.core = core;
    }

    double Clock => HangWatch.Now - started;

    internal static async Task<InteractiveProbe> Start(DispatcherQueue ui, Core core)
    {
        // A probe never reported is stopped off the UI thread: Stop waits up to 2 s for each thread, and the beat thread waits
        // for a beat queued on the UI thread, so a stop on it held the UI thread and the hang watch tripped.
        if (Current is InteractiveProbe before) await Task.Run(before.Stop);
        var probe = new InteractiveProbe(ui, core);
        probe.beatThread = new Thread(probe.Beats) { IsBackground = true, Name = "interactive beats" };
        probe.coreThread = new Thread(probe.CoreCalls) { IsBackground = true, Name = "interactive core" };
        probe.beatThread.Start();
        probe.coreThread.Start();
        return Current = probe;
    }

    void Stop()
    {
        stopping = true;
        beatThread?.Join(2000);
        coreThread?.Join(2000);
    }

    [DllImport("winmm.dll")]
    static extern uint timeBeginPeriod(uint ms);

    [DllImport("winmm.dll")]
    static extern uint timeEndPeriod(uint ms);

    /// <summary>One beat at a time: a held thread counts as one late beat with its whole length, not as many behind it.</summary>
    void Beats()
    {
        // (A sleep of 10 ms is one of 16 otherwise.)
        timeBeginPeriod(1);
        try
        {
            while (!stopping)
            {
                using var answered = new ManualResetEventSlim();
                var posted = HangWatch.Now;
                var pausedBefore = GC.GetTotalPauseDuration();
                var queued = ui.TryEnqueue(DispatcherQueuePriority.Normal, () =>
                {
                    var now = HangWatch.Now;
                    var own = RendererCaptures.Shared.Spans(posted, now).Length > 0;
                    lock (beats) beats.Add(new Beat(posted - started, (now - posted) * 1000, HangWatch.LastAction, own, (GC.GetTotalPauseDuration() - pausedBefore).TotalMilliseconds));
                    answered.Set();
                });
                while (queued && !stopping && !answered.Wait(100)) { }
                // (Stopping with a beat still queued: it must not find the event disposed.)
                if (queued && !answered.IsSet) answered.Wait(5000);
                Thread.Sleep(BeatMs);
            }
        }
        finally
        {
            timeEndPeriod(1);
        }
    }

    void CoreCalls()
    {
        var watch = new System.Diagnostics.Stopwatch();
        while (!stopping)
        {
            var at = Clock;
            watch.Restart();
            try
            {
                var snapshot = core.SyncSnapshot();
                folders = [.. snapshot.Folders.Select(f => f.Name)];
            }
            catch
            {
                coreFailures++;
            }
            // (Only the check's proof that a slow answer is reported.)
            if (Interlocked.Exchange(ref coreDelayMs, 0) is var delay and > 0) Thread.Sleep(delay);
            var ms = watch.Elapsed.TotalMilliseconds;
            lock (coreCalls) coreCalls.Add(new CoreCall(at, ms, HangWatch.LastAction));
            Thread.Sleep(CoreMs);
        }
    }

    /// <summary>The next core call is reported this much late: the check proves its verdict with it.</summary>
    internal void DelayNextCoreCall(int ms) => Interlocked.Exchange(ref coreDelayMs, ms);

    /// <summary>
    /// Every surface again and again for <paramref name="seconds"/>, each command through <paramref name="run"/> (what the QA
    /// control runs), timed: the tray's flyout open and closed, the library's sections, every Settings page, a chat channel,
    /// the Add window; the folder's row, Edit and Share once the core has <paramref name="folder"/>. With
    /// <paramref name="first"/> that command goes first (the Add window's Start, as the user presses it).
    /// </summary>
    internal void Cycle(double seconds, string? first, string? channel, string? folder, Func<string[], Task<string>> run)
    {
        Cycling = Go();

        async Task Time(string[] words)
        {
            var at = Clock;
            var watch = System.Diagnostics.Stopwatch.StartNew();
            var (full, paused) = (GC.CollectionCount(2), GC.GetTotalPauseDuration());
            var result = await run(words);
            var ms = watch.Elapsed.TotalMilliseconds;
            lock (commands) commands.Add(new Timed(at, ms, string.Join(' ', words), result, GC.CollectionCount(2) - full, (GC.GetTotalPauseDuration() - paused).TotalMilliseconds));
        }

        async Task Go()
        {
            var end = Clock + seconds;
            string[][] Always()
            {
                List<string[]> steps =
                [
                    ["sync-flyout", "open"], ["sync-flyout", "close"],
                    ["place", "downloads"], ["place", "sharing"], ["place", "foldersync"], ["syncpage"],
                    ["place", "settings"],
                ];
                steps.AddRange(Enum.GetNames<SettingsTab>().Select(t => new[] { "settings", "tab", t }));
                if (channel is not null) steps.Add(["place", "chat", channel]);
                steps.Add(["add-folder", "open"]);
                steps.Add(["add-folder", "cancel"]);
                return [.. steps];
            }
            string[][] ForFolder(string name) =>
            [
                ["syncpage", "choose", name],
                ["sync-flyout", "open"], ["sync-flyout", "close"],
                ["edit-folder", "open", name], ["edit-folder", "cancel"],
                ["share", "open", name], ["share", "cancel"],
            ];
            if (first is not null) await Time(first.Split(' '));
            while (Clock < end)
            {
                var steps = Always();
                if (folder is not null && folders.Contains(folder)) steps = [.. steps, .. ForFolder(folder)];
                foreach (var step in steps)
                {
                    if (Clock >= end) break;
                    await Time(step);
                    // A beat of its own between two commands, as between two clicks however fast.
                    await Task.Delay(20);
                }
            }
        }
    }

    /// <summary>Why each collection ran, as the runtime says (a full collection nobody asked for in code has a reason here).</summary>
    sealed class Collections : System.Diagnostics.Tracing.EventListener
    {
        readonly Dictionary<string, int> reasons = [];

        protected override void OnEventSourceCreated(System.Diagnostics.Tracing.EventSource source)
        {
            if (source.Name == "Microsoft-Windows-DotNETRuntime") EnableEvents(source, System.Diagnostics.Tracing.EventLevel.Verbose, (System.Diagnostics.Tracing.EventKeywords)1);
        }

        protected override void OnEventWritten(System.Diagnostics.Tracing.EventWrittenEventArgs e)
        {
            // What brings a collection nobody asked for: the wrappers' memory pressure (each element a walk looks at gets one), by the action it came under.
            if (e.EventName is not null && e.EventName.StartsWith("IncreaseMemoryPressure"))
            {
                var under = $"pressure under {HangWatch.LastAction}";
                lock (reasons)
                {
                    reasons["pressure calls"] = reasons.GetValueOrDefault("pressure calls") + 1;
                    reasons[under] = reasons.GetValueOrDefault(under) + 1;
                }
                return;
            }
            if (e.EventName is null || !e.EventName.StartsWith("GCStart") || e.PayloadNames is null || e.Payload is null) return;
            var at = e.PayloadNames.IndexOf("Reason");
            var depth = e.PayloadNames.IndexOf("Depth");
            if (at < 0) return;
            var key = $"gen{(depth >= 0 ? e.Payload[depth] : "?")} reason {e.Payload[at]}";
            lock (reasons) reasons[key] = reasons.GetValueOrDefault(key) + 1;
        }

        internal string Said()
        {
            lock (reasons) return string.Join(", ", reasons.OrderByDescending(r => r.Value).Select(r => $"{r.Key}: {r.Value}"));
        }
    }

    readonly Collections collections = new();

    static double Percentile(List<double> sorted, double share) => sorted.Count == 0 ? 0 : sorted[Math.Min(sorted.Count - 1, (int)Math.Ceiling(share * sorted.Count) - 1)];

    static double Round(double ms) => Math.Round(ms, 1);

    /// <summary>Ends the probe and writes what it saw to <paramref name="file"/> (JSON, for the check).</summary>
    internal async Task Report(string file)
    {
        if (Cycling is Task cycling) await cycling;
        var duration = Clock;
        await Task.Run(Stop);
        if (Current == this) Current = null;
        List<Beat> all;
        lock (beats) all = [.. beats];
        // Inside the renderer's own captures: said apart, and no part of the verdict.
        var counted = all.Where(b => !b.Own).ToList();
        var own = all.Where(b => b.Own).ToList();
        var delays = counted.Select(b => b.Ms).Order().ToList();
        var worst = counted.OrderByDescending(b => b.Ms).Take(5).Select(b => new { at = Round(b.At), ms = Round(b.Ms), action = b.Action, gc = Round(b.GcMs) }).ToList();
        List<CoreCall> calls;
        lock (coreCalls) calls = [.. coreCalls];
        var trips = calls.Select(c => c.Ms).Order().ToList();
        List<Timed> timed;
        lock (commands) timed = [.. commands];
        var report = new
        {
            seconds = Round(duration),
            ui = new
            {
                beats = counted.Count,
                max = Round(delays.LastOrDefault()),
                maxAction = worst.FirstOrDefault()?.action ?? "",
                p99 = Round(Percentile(delays, 0.99)),
                over50 = delays.Count(d => d > 50),
                over100 = delays.Count(d => d > 100),
                worst,
                own = new { beats = own.Count, max = Round(own.Select(b => b.Ms).DefaultIfEmpty().Max()), over100 = own.Count(b => b.Ms > 100) },
            },
            core = new
            {
                calls = calls.Count,
                failures = coreFailures,
                max = Round(trips.LastOrDefault()),
                p99 = Round(Percentile(trips, 0.99)),
                over100 = trips.Count(d => d > 100),
                over500 = trips.Count(d => d > 500),
                worst = calls.OrderByDescending(c => c.Ms).Take(5).Select(c => new { at = Round(c.At), ms = Round(c.Ms), action = c.Action }).ToList(),
            },
            gc = new { reasons = collections.Said(), heapMb = GC.GetGCMemoryInfo().HeapSizeBytes / 1048576, pauseMs = Round((GC.GetTotalPauseDuration() - pausedAtStart).TotalMilliseconds), gen0 = GC.CollectionCount(0) - collectionsAtStart[0], gen1 = GC.CollectionCount(1) - collectionsAtStart[1], gen2 = GC.CollectionCount(2) - collectionsAtStart[2], concurrent = System.Runtime.GCSettings.IsServerGC ? "server" : "workstation", latency = System.Runtime.GCSettings.LatencyMode.ToString() },
            commands = timed.Select(c => new { at = Round(c.At), ms = Round(c.Ms), name = c.Name, full = c.Full, gc = Round(c.GcMs), result = c.Result.Length > 200 ? c.Result[..200] : c.Result }).ToList(),
        };
        var text = System.Text.Json.JsonSerializer.Serialize(report);
        collections.Dispose();
        Directory.CreateDirectory(Path.GetDirectoryName(file) ?? ".");
        await Task.Run(() => File.WriteAllText(file, text));
    }
}
