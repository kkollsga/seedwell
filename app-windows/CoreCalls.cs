using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The user's commands to the core, in the order given (docs/CROSS-PLATFORM.md §1, the Mac's CoreCalls.inOrder): the
/// core runs them on one lane in the order they reach it, so they must also be started in order. Each waits for the one
/// before; <c>then</c> runs back on the UI thread once the call returned (the await keeps the DispatcherQueue's context),
/// so what shows its result is refreshed after it. The UI thread never waits for a call.
/// </summary>
static class CoreCalls
{
    static Task tail = Task.CompletedTask;

    /// <summary>A command; a failure is logged and, with <paramref name="failed"/>, told (given the core's sentence).</summary>
    public static void InOrder(string what, Func<Task> work, Action? then = null, Action<string>? failed = null)
    {
        var previous = tail;
        tail = Run();
        async Task Run()
        {
            try { await previous; } catch { }
            try
            {
                await work();
            }
            catch (Exception e)
            {
                var message = Message(e);
                AppLog.Write($"{what} failed: {message}");
                failed?.Invoke(message);
            }
            then?.Invoke();
        }
    }

    /// <summary>A command with an answer; <paramref name="then"/> gets it, or <paramref name="failed"/> the core's sentence.</summary>
    public static void InOrder<T>(string what, Func<Task<T>> work, Action<T> then, Action<string>? failed = null)
    {
        var previous = tail;
        tail = Run();
        async Task Run()
        {
            try { await previous; } catch { }
            T result;
            try
            {
                result = await work();
            }
            catch (Exception e)
            {
                var message = Message(e);
                AppLog.Write($"{what} failed: {message}");
                failed?.Invoke(message);
                return;
            }
            then(result);
        }
    }

    /// <summary>The core's sentence for an error (any other exception's message).</summary>
    public static string Message(Exception e) => e switch
    {
        CoreException ce => TrsFfiMethods.CoreErrorMessage(ce),
        AggregateException { InnerException: Exception inner } => Message(inner),
        _ => e.Message,
    };

    /// <summary>QA: waits until every command given so far has returned.</summary>
    public static async Task Idle()
    {
        try { await tail; } catch { }
    }
}
