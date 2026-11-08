using System.Runtime.InteropServices;
using Microsoft.Win32;
using uniffi.seedwell_ffi;

namespace Seedwell;

/// <summary>
/// The one "Seedwell" entry in File Explorer's navigation pane, opening the Seedwell folder (SPEC §7G.11), by Microsoft's
/// "Integrate a Cloud Storage Provider": per-user keys only, written as the core's <c>SyncNavPane</c> says and removed when
/// it says so. What it is, where it opens and when it shows are the core's; no state is kept here. Each key lies under
/// the test seam's root in a test run (SEEDWELL_INSTALL_TEST=1, as <see cref="Registration"/>): Explorer never sees those.
/// </summary>
static class NavPaneEntry
{
    static readonly string Base = Environment.GetEnvironmentVariable("SEEDWELL_INSTALL_TEST") == "1" ? @"Software\SeedwellInstallTest\" : @"Software\";
    static readonly string Clsids = Base + @"Classes\CLSID";
    static readonly string Desktop = Base + @"Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";
    static readonly string Hidden = Base + @"Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";

    const int SHCNE_ASSOCCHANGED = 0x08000000;

    [DllImport("shell32.dll")]
    static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);

    /// <summary>The entry as the core says, then Explorer told. Older entries with the marker under another id go too.</summary>
    internal static void Apply(SyncNavPane entry, string exe)
    {
        // The entry first: a key the sweep cannot read must not cost it.
        if (entry.Show)
        {
            Write(entry, exe);
        }
        Sweep(entry.Marker, entry.Show ? entry.Clsid : null);
        SHChangeNotify(SHCNE_ASSOCCHANGED, 0, IntPtr.Zero, IntPtr.Zero);
    }

    /// <summary>Uninstalling: every entry of ours goes, Explorer told. Needs no core: the marker is a free function of it.</summary>
    internal static void RemoveAll(string marker)
    {
        Sweep(marker, null);
        SHChangeNotify(SHCNE_ASSOCCHANGED, 0, IntPtr.Zero, IntPtr.Zero);
    }

    static void Write(SyncNavPane e, string exe)
    {
        using var key = Registry.CurrentUser.CreateSubKey($@"{Clsids}\{e.Clsid}");
        key.SetValue("", e.Name);
        key.SetValue(e.Marker, 1, RegistryValueKind.DWord);
        key.SetValue("System.IsPinnedToNameSpaceTree", 1, RegistryValueKind.DWord);
        key.SetValue("SortOrderIndex", 0x42, RegistryValueKind.DWord);
        using (var icon = key.CreateSubKey("DefaultIcon"))
        {
            icon.SetValue("", $"{exe},0", RegistryValueKind.ExpandString);
        }
        using (var server = key.CreateSubKey("InProcServer32"))
        {
            server.SetValue("", @"%SystemRoot%\system32\shell32.dll", RegistryValueKind.ExpandString);
        }
        using (var instance = key.CreateSubKey("Instance"))
        {
            instance.SetValue("CLSID", "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}");
            using var bag = instance.CreateSubKey("InitPropertyBag");
            bag.SetValue("Attributes", 0x11, RegistryValueKind.DWord);
            // A plain string, not the expanding one of Microsoft's example: measured, a folder named %TEMP%x opens with this
            // and does not open with that (CLASS_E_CLASSNOTAVAILABLE). The core's path is absolute: nothing to expand.
            bag.SetValue("TargetFolderPath", e.Target, RegistryValueKind.String);
        }
        using (var folder = key.CreateSubKey("ShellFolder"))
        {
            folder.SetValue("FolderValueFlags", 0x28, RegistryValueKind.DWord);
            folder.SetValue("Attributes", unchecked((int)0xF080004D), RegistryValueKind.DWord);
        }
        using (var desktop = Registry.CurrentUser.CreateSubKey($@"{Desktop}\{e.Clsid}"))
        {
            desktop.SetValue("", e.Name);
            desktop.SetValue(e.Marker, 1, RegistryValueKind.DWord);
        }
        using var hidden = Registry.CurrentUser.CreateSubKey(Hidden);
        hidden.SetValue(e.Clsid, 1, RegistryValueKind.DWord);
    }

    /// <summary>Every entry carrying the marker but <paramref name="keep"/>: only ours, never another program's.</summary>
    static void Sweep(string marker, string? keep)
    {
        using var all = Registry.CurrentUser.OpenSubKey(Clsids);
        if (all is null)
        {
            return;
        }
        foreach (var id in all.GetSubKeyNames())
        {
            if (string.Equals(id, keep, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }
            try
            {
                using var key = all.OpenSubKey(id);
                if (key?.GetValue(marker) is not null)
                {
                    Remove(id);
                }
            }
            catch (Exception e) when (e is System.Security.SecurityException or UnauthorizedAccessException or IOException)
            {
                // Another program's key this user may not read: not ours to touch, and no reason to stop.
                AppLog.Write($"navigation pane: passed over {id} ({e.GetType().Name})");
            }
        }
        // The lists' keys carry the marker too: one left by a crash between the removals below, whose CLSID is gone.
        using var desktop = Registry.CurrentUser.OpenSubKey(Desktop);
        foreach (var id in desktop?.GetSubKeyNames() ?? [])
        {
            if (string.Equals(id, keep, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }
            using var key = desktop!.OpenSubKey(id);
            if (key?.GetValue(marker) is not null)
            {
                Remove(id);
            }
        }
    }

    /// <summary>The lists first, the CLSID last: a crash between them leaves what a next sweep still finds.</summary>
    static void Remove(string id)
    {
        using (var hidden = Registry.CurrentUser.OpenSubKey(Hidden, true))
        {
            hidden?.DeleteValue(id, false);
        }
        Registry.CurrentUser.DeleteSubKeyTree($@"{Desktop}\{id}", false);
        Registry.CurrentUser.DeleteSubKeyTree($@"{Clsids}\{id}", false);
    }

    /// <summary>What is registered under this id (QA, the test seam's keys).</summary>
    internal static string QaDescribe(string clsid)
    {
        using var key = Registry.CurrentUser.OpenSubKey($@"{Clsids}\{clsid}");
        if (key is null)
        {
            return "entry none";
        }
        using var bag = key.OpenSubKey(@"Instance\InitPropertyBag");
        using var icon = key.OpenSubKey("DefaultIcon");
        using var desktop = Registry.CurrentUser.OpenSubKey($@"{Desktop}\{clsid}");
        using var hidden = Registry.CurrentUser.OpenSubKey(Hidden);
        return $"entry \"{key.GetValue("")}\", target \"{bag?.GetValue("TargetFolderPath", null, RegistryValueOptions.DoNotExpandEnvironmentNames)}\", icon \"{icon?.GetValue("", null, RegistryValueOptions.DoNotExpandEnvironmentNames)}\", on the desktop list {desktop is not null}, hidden from the desktop {hidden?.GetValue(clsid) is not null}";
    }
}
