using Microsoft.Win32;

namespace Seedwell;

/// <summary>
/// "Launch at login" (the core's general.launch_at_login; the Mac's SMAppService.mainApp): this user's Run entry,
/// written while the setting is on and removed when it is off. Only for the real app (not --state-dir, not the QA
/// render), as the Mac skips it for its development runs.
/// </summary>
static class LoginItem
{
    const string Run = @"Software\Microsoft\Windows\CurrentVersion\Run";
    const string Name = "Seedwell";

    public static void Apply(bool want)
    {
        using var run = Registry.CurrentUser.CreateSubKey(Run);
        var command = $"\"{Environment.ProcessPath}\"";
        if (want)
        {
            if (run.GetValue(Name) as string != command)
            {
                run.SetValue(Name, command);
            }
        }
        else if (run.GetValue(Name) is not null)
        {
            run.DeleteValue(Name, false);
        }
    }
}
