using System.Runtime.InteropServices;

namespace Seedwell;

/// <summary>
/// Windows' TaskbarCreated message (sent to every top-level window when Explorer makes the taskbar: at sign-in, late,
/// or after a restart of Explorer), watched on one of the app's top-level windows (the tray flyout's, made at launch
/// and kept). The app makes its notification-area icon then if it has none.
/// </summary>
static class TaskbarWatch
{
    delegate IntPtr SubclassProc(IntPtr hwnd, uint msg, IntPtr wParam, IntPtr lParam, UIntPtr id, UIntPtr data);

    // Kept for the life of the app: the window calls it.
    static SubclassProc? proc;
    static uint taskbarCreated;

    public static void Start(IntPtr hwnd, Action created)
    {
        taskbarCreated = RegisterWindowMessageW("TaskbarCreated");
        proc = (h, msg, w, l, _, _) =>
        {
            if (msg == taskbarCreated)
            {
                AppLog.Write("tray: the taskbar was created");
                created();
            }
            return DefSubclassProc(h, msg, w, l);
        };
        if (taskbarCreated == 0 || !SetWindowSubclass(hwnd, proc, UIntPtr.Zero, UIntPtr.Zero))
        {
            AppLog.Write($"tray: TaskbarCreated is not watched (error {Marshal.GetLastWin32Error()})");
        }
    }

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint RegisterWindowMessageW(string name);

    [DllImport("comctl32.dll", SetLastError = true)]
    static extern bool SetWindowSubclass(IntPtr hwnd, SubclassProc proc, UIntPtr id, UIntPtr data);

    [DllImport("comctl32.dll")]
    static extern IntPtr DefSubclassProc(IntPtr hwnd, uint msg, IntPtr wParam, IntPtr lParam);
}
