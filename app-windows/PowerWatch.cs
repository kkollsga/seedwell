using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Tells the core whether the PC runs on battery (Core.power_changed), at start and on every change of the power
/// source; the "On battery" setting and what to pause or limit are the core's (the Mac's PowerWatcher hands over the
/// same fact). A desktop without a battery is never on battery.
/// </summary>
static class PowerWatch
{
    public static void Start(Core core)
    {
        Tell(core);
        Windows.System.Power.PowerManager.PowerSupplyStatusChanged += (_, _) => Tell(core);
    }

    static void Tell(Core core)
    {
        bool onBattery = GetSystemPowerStatus(out var s) && s.ACLineStatus == 0;
        AppLog.Write($"power: {(onBattery ? "on battery" : "on AC")}");
        try { core.PowerChanged(onBattery); } catch (Exception e) { AppLog.Write($"power_changed failed: {e.Message}"); }
    }

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    struct SYSTEM_POWER_STATUS
    {
        public byte ACLineStatus, BatteryFlag, BatteryLifePercent, SystemStatusFlag;
        public int BatteryLifeTime, BatteryFullLifeTime;
    }

    [System.Runtime.InteropServices.DllImport("kernel32.dll")]
    static extern bool GetSystemPowerStatus(out SYSTEM_POWER_STATUS status);
}
