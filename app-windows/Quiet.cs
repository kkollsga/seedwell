using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace Seedwell;

/// <summary>
/// QA and visual-QA runs stay out of the user's way (the Mac's QA does the same): a run started with
/// --render-snapshots or --qa-control never activates a window, never takes the foreground or the keyboard, keeps its
/// windows off the screen and out of the taskbar and Alt+Tab, and draws every picture in-process (RenderTargetBitmap),
/// so it works with the screen locked or asleep.
/// </summary>
static class Quiet
{
    /// <summary>This run is a QA or visual-QA run.</summary>
    public static bool On => Program.Render is not null || Program.QaControl;

    /// <summary>Shows a window without activating it; in a quiet run also off the screen and out of the switchers.</summary>
    public static void Show(Window window)
    {
        if (On)
        {
            window.AppWindow.IsShownInSwitchers = false;
            window.AppWindow.Move(new Windows.Graphics.PointInt32(-32000, -32000));
            // Never activatable: WinUI activates a window when it first shows it, Show(false) or not.
            var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(window);
            SetWindowLongPtr(hwnd, GWL_EXSTYLE, GetWindowLongPtr(hwnd, GWL_EXSTYLE) | WS_EX_NOACTIVATE);
        }
        window.AppWindow.Show(false);
    }

    const int GWL_EXSTYLE = -20;
    const nint WS_EX_NOACTIVATE = 0x08000000;

    [System.Runtime.InteropServices.DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
    static extern nint GetWindowLongPtr(nint hwnd, int index);

    [System.Runtime.InteropServices.DllImport("user32.dll", EntryPoint = "SetWindowLongPtrW")]
    static extern nint SetWindowLongPtr(nint hwnd, int index, nint value);

    /// <summary>
    /// Waits until a change is laid out and drawn: a layout pass now, then the next frame XAML renders, at most
    /// <paramref name="capMs"/> (no frame comes while nothing changed; a locked screen still lays out).
    /// </summary>
    public static async Task Settle(UIElement? element, int capMs = 150)
    {
        element?.UpdateLayout();
        var frame = new TaskCompletionSource();
        void Rendered(object? s, object e) => frame.TrySetResult();
        CompositionTarget.Rendering += Rendered;
        try
        {
            await Task.WhenAny(frame.Task, Task.Delay(capMs));
        }
        finally
        {
            CompositionTarget.Rendering -= Rendered;
        }
        element?.UpdateLayout();
    }
}
