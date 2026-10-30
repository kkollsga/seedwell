namespace Seedwell;

/// <summary>
/// Keeps the computer from idle sleep while something downloads ("Prevent sleep while downloading", the core's
/// power_needs) or an agent asks to keep it awake ("Keep awake while agents work"): the Mac's SleepAssertion. Seeding
/// alone never does; the display still sleeps and the screen still locks (ES_SYSTEM_REQUIRED only, never
/// ES_DISPLAY_REQUIRED). The request belongs to the thread that makes it, so it is always made on the UI thread.
/// </summary>
sealed class SleepGuard
{
    const uint EsContinuous = 0x80000000, EsSystemRequired = 0x00000001;

    public bool WhileDownloading { get; set; } = true;
    public bool ForAgents { get; set; } = true;
    public bool Held { get; private set; }

    /// <summary>The core's answer for this moment: downloading, and whether an agent asked to stay awake.</summary>
    public void Update(bool downloading, bool agentsAwake)
    {
        bool want = (WhileDownloading && downloading) || (ForAgents && agentsAwake);
        if (want == Held)
        {
            return;
        }
        SetThreadExecutionState(EsContinuous | (want ? EsSystemRequired : 0));
        Held = want;
        AppLog.Write(want ? $"sleep: held ({(downloading ? "downloading" : "agents")})" : "sleep: released");
    }

    [System.Runtime.InteropServices.DllImport("kernel32.dll")]
    static extern uint SetThreadExecutionState(uint flags);
}
