using System.Net.NetworkInformation;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// Tells the core when the network changes (Core.network_path_changed), as the Mac's NetworkWatcher hands over its path
/// monitor: Windows' address-change and availability notifications, settled for 3 s (moving to another Wi-Fi network
/// sends several). What a change means is the core's: the engine refreshes its port mapping, folder sync closes the
/// sessions on a gone address and finds its devices again at once (SPEC 7G.3).
/// </summary>
static class NetWatch
{
    static readonly object Gate = new();
    static System.Threading.Timer? settle;

    public static void Start(Core core)
    {
        NetworkChange.NetworkAddressChanged += (_, _) => Settle(core);
        NetworkChange.NetworkAvailabilityChanged += (_, _) => Settle(core);
    }

    static void Settle(Core core)
    {
        lock (Gate)
        {
            settle?.Dispose();
            settle = new System.Threading.Timer(_ => Tell(core), null, 3000, System.Threading.Timeout.Infinite);
        }
    }

    static void Tell(Core core)
    {
        bool up = NetworkInterface.GetIsNetworkAvailable();
        AppLog.Write($"network: changed ({(up ? "a network is available" : "no network")})");
        try { core.NetworkPathChanged(up); } catch (Exception e) { AppLog.Write($"network_path_changed failed: {e.Message}"); }
    }
}
