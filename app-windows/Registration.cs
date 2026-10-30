using Microsoft.Win32;

namespace Seedwell;

/// <summary>
/// Opening .torrent files and magnet: links with Seedwell, registered for this user only (HKCU\Software\Classes, no
/// administrator rights): the Windows side of the Mac's CFBundleDocumentTypes and CFBundleURLTypes (app/build.sh), with
/// the same names. <c>Seedwell.exe --register</c> writes it, <c>--unregister</c> removes exactly what it wrote. Windows
/// then offers Seedwell for both; it becomes the default where nothing else is, otherwise the user picks it once
/// (Windows asks, or Settings > Apps > Default apps). install.ps1 runs both, so this is the one place for the rule; its
/// test seam SEEDWELL_INSTALL_TEST=1 puts every key under HKCU\Software\SeedwellInstallTest instead.
/// </summary>
static class Registration
{
    static readonly string Classes = Environment.GetEnvironmentVariable("SEEDWELL_INSTALL_TEST") == "1"
        ? @"Software\SeedwellInstallTest\Classes" : @"Software\Classes";
    const string ProgId = "Seedwell.torrent";
    static string AppKey => $@"{Classes}\Applications\Seedwell.exe";

    static string Exe => Environment.ProcessPath ?? Path.Combine(AppContext.BaseDirectory, "Seedwell.exe");
    static string Open => $"\"{Exe}\" \"%1\"";

    public static void Register()
    {
        // .torrent: a ProgID of our own, listed in the extension's OpenWithProgids (the extension's default is never
        // taken over: Windows lets the user choose).
        using (var p = Registry.CurrentUser.CreateSubKey($@"{Classes}\{ProgId}"))
        {
            p.SetValue("", "BitTorrent file");
            using (var icon = p.CreateSubKey("DefaultIcon")) icon.SetValue("", $"\"{Exe}\",0");
            using (var cmd = p.CreateSubKey(@"shell\open\command")) cmd.SetValue("", Open);
        }
        using (var ext = Registry.CurrentUser.CreateSubKey($@"{Classes}\.torrent\OpenWithProgids"))
        {
            ext.SetValue(ProgId, Array.Empty<byte>(), RegistryValueKind.None);
        }
        // magnet: only when no other application has it: an existing handler stays (the user can still choose Seedwell
        // in Default apps, from the Applications entry below).
        using (var magnet = Registry.CurrentUser.OpenSubKey($@"{Classes}\magnet\shell\open\command"))
        {
            if (magnet is null)
            {
                using var m = Registry.CurrentUser.CreateSubKey($@"{Classes}\magnet");
                m.SetValue("", "URL:Magnet link");
                m.SetValue("URL Protocol", "");
                m.SetValue("SeedwellRegistered", 1);
                using (var icon = m.CreateSubKey("DefaultIcon")) icon.SetValue("", $"\"{Exe}\",0");
                using (var cmd = m.CreateSubKey(@"shell\open\command")) cmd.SetValue("", Open);
            }
        }
        // The application itself, so "Open with" and Default apps list it for both.
        using (var app = Registry.CurrentUser.CreateSubKey(AppKey))
        {
            app.SetValue("FriendlyAppName", "Seedwell");
            using (var cmd = app.CreateSubKey(@"shell\open\command")) cmd.SetValue("", Open);
            using (var types = app.CreateSubKey("SupportedTypes")) types.SetValue(".torrent", "");
        }
        Changed();
    }

    public static void Unregister()
    {
        Registry.CurrentUser.DeleteSubKeyTree($@"{Classes}\{ProgId}", false);
        using (var ext = Registry.CurrentUser.OpenSubKey($@"{Classes}\.torrent\OpenWithProgids", true))
        {
            ext?.DeleteValue(ProgId, false);
        }
        // magnet: only the one this registration made (marked), never another application's.
        using (var m = Registry.CurrentUser.OpenSubKey($@"{Classes}\magnet"))
        {
            if (m?.GetValue("SeedwellRegistered") is not null)
            {
                m.Dispose();
                Registry.CurrentUser.DeleteSubKeyTree($@"{Classes}\magnet", false);
            }
        }
        Registry.CurrentUser.DeleteSubKeyTree(AppKey, false);
        Changed();
    }

    /// <summary>Tells Explorer the associations changed.</summary>
    static void Changed() => SHChangeNotify(0x08000000 /* SHCNE_ASSOCCHANGED */, 0, IntPtr.Zero, IntPtr.Zero);

    [System.Runtime.InteropServices.DllImport("shell32.dll")]
    static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);
}
