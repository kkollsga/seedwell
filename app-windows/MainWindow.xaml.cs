using Microsoft.UI.Xaml;

namespace Seedwell;

/// <summary>
/// The library window: <see cref="LibraryPane"/> (the core's library model) on Mica. Closing it hides it: the app keeps
/// running in the tray; the library knows whether it is open and seen.
/// </summary>
public sealed partial class MainWindow : Window
{
    bool quitting;
    readonly LibraryPane pane;

    internal LibraryPane Pane => pane;

    internal MainWindow(LibraryPane pane)
    {
        InitializeComponent();
        this.pane = pane;
        Chrome.Apply(this, pane.Root, "Seedwell");
        SystemBackdrop = new Microsoft.UI.Xaml.Media.MicaBackdrop();
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "Seedwell.ico"));
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1100, 720));
        AppWindow.Closing += (_, e) =>
        {
            if (!quitting)
            {
                e.Cancel = true;
                AppWindow.Hide();
                pane.SetOpen(false);
            }
        };
        // Minimised or covered by being hidden: the model stops refreshing until it can be seen again.
        AppWindow.Changed += (_, e) =>
        {
            if (e.DidPresenterChange || e.DidVisibilityChange)
            {
                bool minimised = AppWindow.Presenter is Microsoft.UI.Windowing.OverlappedPresenter p && p.State == Microsoft.UI.Windowing.OverlappedPresenterState.Minimized;
                pane.SetVisible(AppWindow.IsVisible && !minimised);
            }
        };
    }

    public void ShowAndActivate()
    {
        pane.SetOpen(true);
        if (Quiet.On)
        {
            // A QA run: shown, never activated or brought forward.
            Quiet.Show(this);
            return;
        }
        AppWindow.Show();
        Activate();
        SetForegroundWindow(WinRT.Interop.WindowNative.GetWindowHandle(this));
    }

    /// <summary>It is the foreground window (for the log).</summary>
    public bool IsInFront => GetForegroundWindow() == WinRT.Interop.WindowNative.GetWindowHandle(this);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern bool SetForegroundWindow(IntPtr hWnd);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    static extern IntPtr GetForegroundWindow();

    /// <summary>Closes it for real (Quit).</summary>
    public void CloseForQuit()
    {
        quitting = true;
        Close();
    }
}
