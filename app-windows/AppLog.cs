namespace Seedwell;

/// <summary>
/// The app's own lines (the Mac's os_log notices: launch, activation, quit and its shutdown result), appended to
/// <c>&lt;state&gt;\logs\app.log</c>. Never fails the caller.
/// </summary>
static class AppLog
{
    static string? path;
    static readonly object gate = new();

    public static void Open(string stateDir)
    {
        var dir = Path.Combine(stateDir, "logs");
        Directory.CreateDirectory(dir);
        path = Path.Combine(dir, "app.log");
    }

    public static void Write(string line)
    {
        if (path is null)
        {
            return;
        }
        lock (gate)
        {
            try { File.AppendAllText(path, $"{DateTime.Now:yyyy-MM-dd HH:mm:ss.fff} [{Environment.ProcessId}] {line}{Environment.NewLine}"); } catch { }
        }
    }
}
