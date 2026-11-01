using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

namespace Seedwell;

/// <summary>
/// The entry point (the generated one is disabled): one instance only. A second launch hands its
/// activation to the first and ends: with .torrent files, magnet links or seedwell:// links on its command line (an
/// opened file, a clicked link) the first opens what each asks for (the core's command_line_routes), without any it
/// shows its window. <c>--render-snapshots &lt;dir&gt; [--theme
/// light]</c> is the visual QA run instead (<see cref="SnapshotRenderer"/>): no single instance, no tray icon.
/// <c>--register</c> / <c>--unregister</c> write or remove the file and link registration (<see cref="Registration"/>).
/// <c>--quit</c> asks the running app to quit, as its Quit does (scripts, an installer, QA); with none running it just ends.
/// </summary>
public static class Program
{
    const string InstanceKey = "Seedwell.App";

    /// <summary>Set for the visual QA run: where the pictures go and in which theme.</summary>
    internal static (string Dir, bool Light)? Render { get; private set; }

    /// <summary>
    /// <c>--state-dir &lt;dir&gt;</c> (the Mac's): the app on another data folder, for QA. It is its own single instance
    /// (one per folder) and never touches the real one's (%LOCALAPPDATA%\Seedwell).
    /// </summary>
    internal static string? StateDir { get; private set; }

    /// <summary>
    /// This instance was started with <c>--qa-control</c> and <c>--state-dir</c> (the Mac's --qa-control, which needs an
    /// isolated run too): only then are second launches' --qa
    /// commands run. Otherwise any local program could drive the app around the core's agent permission
    /// (Settings › Agents), the only way other programs may act on Seedwell.
    /// </summary>
    internal static bool QaControl { get; private set; }

    /// <summary>What this launch's command line opens: things to add and seedwell:// links, in order.</summary>
    internal static uniffi.trs_ffi.LinkRoute[] Routes { get; private set; } = [];

    [STAThread]
    static int Main(string[] args)
    {
        WinRT.ComWrappersSupport.InitializeComWrappers();
        if (args.Contains("--register") || args.Contains("--unregister"))
        {
            if (args.Contains("--register")) Registration.Register(); else Registration.Unregister();
            return 0;
        }
        Routes = RoutesIn(args);
        int dir = Array.IndexOf(args, "--state-dir");
        if (dir >= 0 && dir + 1 < args.Length)
        {
            // One folder, one key: "state", "state\" and ".\state" are the same instance (a drive's root keeps its \).
            StateDir = Path.TrimEndingDirectorySeparator(Path.GetFullPath(args[dir + 1]));
        }
        // QA commands read the app's windows: only a copy on its own data folder takes them, never the user's Seedwell.
        QaControl = args.Contains("--qa-control") && StateDir is not null;
        int at = Array.IndexOf(args, "--render-snapshots");
        if (at >= 0 && at + 1 < args.Length)
        {
            int theme = Array.IndexOf(args, "--theme");
            Render = (Path.GetFullPath(args[at + 1]), theme >= 0 && theme + 1 < args.Length && args[theme + 1] == "light");
            Start();
            return 0;
        }
        // The hand-off is per state folder: a launch with --state-dir reaches only the instance on that folder (a test
        // never reaches the user's app, which has none), as the Mac's control socket lives in the state folder.
        var main = AppInstance.FindOrRegisterForKey(StateDir is null ? InstanceKey : $"{InstanceKey}:{StateDir.ToLowerInvariant()}");
        if (!main.IsCurrent)
        {
            // RedirectActivationToAsync must not block the STA thread it runs on: wait on another one. This launch may
            // come to the foreground (the user started it); it passes that on, so the first can show its window in front.
            AllowSetForegroundWindow(-1 /* ASFW_ANY */);
            var activation = AppInstance.GetCurrent().GetActivatedEventArgs();
            var done = new ManualResetEventSlim();
            Task.Run(async () =>
            {
                await main.RedirectActivationToAsync(activation);
                done.Set();
            });
            done.Wait(TimeSpan.FromSeconds(10));
            return 0;
        }
        if (args.Contains("--quit"))
        {
            // Nothing runs on this state folder to quit: end, never start one in its place (an installer waits on this).
            return 0;
        }
        main.Activated += (_, e) =>
        {
            // A second launch: its command line comes with the activation.
            var line = (e.Data as Windows.ApplicationModel.Activation.ILaunchActivatedEventArgs)?.Arguments ?? "";
            var words = Split(line);
            var routes = RoutesIn(words);
            AppLog.Write($"second launch: {(words.Contains("--qa") ? "qa" : words.Contains("--quit") ? "quit" : routes.Length > 0 ? $"{routes.Length} to open" : "show the window")}");
            int qa = Array.IndexOf(words, "--qa");
            if (qa >= 0 && QaControl) App.Current?.Qa(words[(qa + 1)..]);
            else if (qa >= 0) AppLog.Write("qa refused: this instance was not started with --qa-control");
            else if (words.Contains("--quit")) App.Current?.QuitFromAnyThread();
            else if (routes.Length > 0) App.Current?.Open(routes);
            else App.Current?.ShowMainWindow();
        };
        Start();
        return 0;
    }

    /// <summary>
    /// What a command line opens: the core's rule (command_line_routes: seedwell:// links and what command_line_sources
    /// adds). The QA words after --qa are the QA command's, never links to open.
    /// </summary>
    internal static uniffi.trs_ffi.LinkRoute[] RoutesIn(string[] args)
    {
        int qa = Array.IndexOf(args, "--qa");
        return uniffi.trs_ffi.TrsFfiMethods.CommandLineRoutes(qa >= 0 ? args[..qa] : args);
    }

    /// <summary>A command line split as Windows splits it (quotes, backslashes).</summary>
    static string[] Split(string line)
    {
        if (line.Trim().Length == 0)
        {
            return [];
        }
        var argv = CommandLineToArgvW(line, out int n);
        if (argv == IntPtr.Zero)
        {
            return [];
        }
        try
        {
            return Enumerable.Range(0, n).Select(i => System.Runtime.InteropServices.Marshal.PtrToStringUni(System.Runtime.InteropServices.Marshal.ReadIntPtr(argv, i * IntPtr.Size)) ?? "").ToArray();
        }
        finally
        {
            LocalFree(argv);
        }
    }

    [System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    static extern IntPtr CommandLineToArgvW(string line, out int count);

    [System.Runtime.InteropServices.DllImport("kernel32.dll")]
    static extern IntPtr LocalFree(IntPtr p);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern bool AllowSetForegroundWindow(int processId);

    static void Start() => Application.Start(p =>
    {
        SynchronizationContext.SetSynchronizationContext(new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread()));
        _ = new App();
    });
}
