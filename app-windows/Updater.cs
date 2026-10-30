using System.Runtime.InteropServices;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The app's updates on Windows, around the core's <c>Updates</c> (SPEC 12A): the core decides when to check, what is
/// on offer, when to install and what everything says; this keeps the timer, runs the blocking check off the UI thread
/// and starts the installer command the core gives, detached and without a window, its output appended to the log.
/// A release build (release-zip.ps1 compiles SEEDWELL_RELEASE in) checks by itself; other builds only when asked.
/// SEEDWELL_UPDATE_REPO points it at another server (tests), as on the Mac.
/// </summary>
sealed class Updater
{
#if SEEDWELL_RELEASE
    const bool ReleaseBuild = true;
#else
    const bool ReleaseBuild = false;
#endif
    const string RepoUrl = "https://github.com/kkollsga/seedwell";

    readonly App app;
    readonly Microsoft.UI.Dispatching.DispatcherQueue ui;
    readonly Microsoft.UI.Dispatching.DispatcherQueueTimer timer;
    bool checking;

    public Updates Updates { get; }

    /// <summary>The model changed (a check ended, an install started): the views that show it ask again.</summary>
    public event Action? Changed;

    public Updater(App app, Core core)
    {
        this.app = app;
        ui = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        var repo = Environment.GetEnvironmentVariable("SEEDWELL_UPDATE_REPO") is { Length: > 0 } r ? r : RepoUrl;
        var v = Environment.OSVersion.Version;
        Updates = new Updates(core, ReleaseBuild, repo, $"{v.Major}.{v.Minor}.{v.Build}", App.Now());
        timer = ui.CreateTimer();
        timer.IsRepeating = false;
        timer.Tick += (_, _) => Check(true);
        Schedule();
    }

    /// <summary>The next automatic check, when the core has one (none for a development build or with checks off).</summary>
    public void Schedule()
    {
        timer.Stop();
        if (Updates.NextDueMs(App.Now()) is ulong due)
        {
            timer.Interval = TimeSpan.FromMilliseconds(Math.Min(due, int.MaxValue));
            timer.Start();
        }
    }

    /// <summary>A check (automatic: <paramref name="silent"/>; Check Now: not), blocking, so on a worker thread.</summary>
    public void Check(bool silent)
    {
        if (checking)
        {
            return;
        }
        checking = true;
        Changed?.Invoke();
        _ = Task.Run(() =>
        {
            try { Updates.Check(silent, App.Now()); } catch (Exception e) { AppLog.Write($"update check failed: {e.Message}"); }
            ui.TryEnqueue(() =>
            {
                checking = false;
                AppLog.Write($"update check: {(Updates.AvailableVersion() is string v ? $"{v} on offer" : "nothing new")}");
                Changed?.Invoke();
                Schedule();
                InstallIfDue();
            });
        });
    }

    /// <summary>After a check and whenever downloads stop: installs when the core says so (the auto-install setting).</summary>
    public void InstallIfDue()
    {
        if (Updates.ShouldInstall())
        {
            Install();
        }
    }

    /// <summary>Starts the core's installer command into the folder that holds this app; it quits and reopens the app.</summary>
    public void Install()
    {
        var dir = AppContext.BaseDirectory.TrimEnd('\\', '/');
        if (Updates.Install(dir, App.Now()) is not InstallCommand cmd)
        {
            return;
        }
        AppLog.Write($"update: installing into {dir} ({cmd.Argv[0]}; log {cmd.Log})");
        if (!Start(cmd))
        {
            Updates.InstallFailed();
        }
        Changed?.Invoke();
    }

    /// <summary>The log of the last install, in File Explorer.</summary>
    public void ShowLog() => RowCommandsReveal(Updates.LogPath());

    static void RowCommandsReveal(string path)
    {
        try { System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"/select,\"{path}\"") { UseShellExecute = true }); } catch { }
    }

    // ------------------------------------------------------------------------------------------- the detached start

    /// <summary>
    /// CreateProcess with the command's environment added to the app's, in the temp folder, and its stdout and stderr
    /// appended to the log: the installer outlives the app it quits. CREATE_NO_WINDOW gives it a console of its own
    /// that is never shown; not DETACHED_PROCESS (no console at all): Windows PowerShell 5.1 started that way runs
    /// nothing, writes nothing and exits 0.
    /// </summary>
    static bool Start(InstallCommand cmd)
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(cmd.Log)!);
            var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf<SECURITY_ATTRIBUTES>(), bInheritHandle = true };
            var log = CreateFileW(cmd.Log, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, ref sa, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (log == new IntPtr(-1))
            {
                return false;
            }
            try
            {
                var env = System.Environment.GetEnvironmentVariables().Cast<System.Collections.DictionaryEntry>()
                    .ToDictionary(e => (string)e.Key, e => (string?)e.Value ?? "", StringComparer.OrdinalIgnoreCase);
                foreach (var v in cmd.Env)
                {
                    env[v.Name] = v.Value;
                }
                // A Unicode environment block: NAME=value\0 ... \0, sorted as Windows expects.
                var block = string.Concat(env.OrderBy(e => e.Key, StringComparer.OrdinalIgnoreCase).Select(e => $"{e.Key}={e.Value}\0")) + "\0";
                var si = new STARTUPINFO { cb = Marshal.SizeOf<STARTUPINFO>(), dwFlags = STARTF_USESTDHANDLES, hStdInput = IntPtr.Zero, hStdOutput = log, hStdError = log };
                var line = string.Join(" ", cmd.Argv.Select(Quote));
                var envPtr = Marshal.StringToHGlobalUni(block);
                try
                {
                    // Started in the temp folder, never the install folder: Windows does not rename a folder that is a
                    // process's current directory, and the app's (from its shortcut) is the install folder.
                    if (!CreateProcessW(null, line, IntPtr.Zero, IntPtr.Zero, true, CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT, envPtr, Path.GetTempPath(), ref si, out var pi))
                    {
                        AppLog.Write($"update: the installer did not start (error {Marshal.GetLastWin32Error()})");
                        return false;
                    }
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    return true;
                }
                finally { Marshal.FreeHGlobal(envPtr); }
            }
            finally { CloseHandle(log); }
        }
        catch (Exception e)
        {
            AppLog.Write($"update: the installer did not start ({e.Message})");
            return false;
        }
    }

    /// <summary>One argument as CommandLineToArgvW reads it back (quotes, and backslashes before quotes doubled).</summary>
    static string Quote(string arg)
    {
        if (arg.Length > 0 && arg.IndexOfAny([' ', '\t', '"']) < 0)
        {
            return arg;
        }
        var s = new System.Text.StringBuilder("\"");
        int slashes = 0;
        foreach (var c in arg)
        {
            if (c == '\\') { slashes++; continue; }
            if (c == '"') { s.Append('\\', slashes * 2 + 1).Append('"'); slashes = 0; continue; }
            s.Append('\\', slashes).Append(c);
            slashes = 0;
        }
        return s.Append('\\', slashes * 2).Append('"').ToString();
    }

    const uint FILE_APPEND_DATA = 0x4, FILE_SHARE_READ = 0x1, FILE_SHARE_WRITE = 0x2, OPEN_ALWAYS = 4, FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint CREATE_NO_WINDOW = 0x08000000, CREATE_UNICODE_ENVIRONMENT = 0x400;
    const int STARTF_USESTDHANDLES = 0x100;

    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public bool bInheritHandle; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb; public string? lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2; public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, ref SECURITY_ATTRIBUTES sa, uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessW(string? app, string commandLine, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string? dir, ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);
}
