using Microsoft.UI.Dispatching;

namespace Seedwell;

/// <summary>
/// A DispatcherQueueTimer holds its Tick handler in native code, out of the collector's sight: one whose handler can reach the
/// timer (through a closure it shares, or an owner with the timer in a field) is never collected, with all its handler reaches
/// and two handles of the system's (a Timer and a WaitCompletionPacket). What is made again and again must not do that: a timer
/// for each draw of Settings' fields and of Cancel's Shift caption brought 0.1.20 to 15.8 GB and 232,000 handles in 22 hours
/// (the check leaks). A timer made once for the app's life may hold what it likes.
/// </summary>
static class Timers
{
    /// <summary>A timer for an owner made more than once: its handler holds the owner weakly, so both can go. <paramref name="tick"/> must not capture the owner (make it static).</summary>
    internal static DispatcherQueueTimer Owned<T>(this DispatcherQueue queue, T owner, Action<T> tick) where T : class
    {
        var weak = new WeakReference<T>(owner);
        var timer = queue.CreateTimer();
        timer.Tick += (sender, _) =>
        {
            if (weak.TryGetTarget(out var alive)) tick(alive);
            else sender.Stop();
        };
        return timer;
    }
}
