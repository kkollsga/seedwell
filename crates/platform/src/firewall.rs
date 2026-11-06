//! The system's firewall and Seedwell's own program (SPEC §7G.3, *Can other devices reach this one*).
//!
//! **Windows** blocks every inbound connection a rule does not allow, and asks the user for a rule only on a network
//! whose profile says so (`NotifyOnListen`; off for Public networks by default). Seedwell installs per user, without
//! administrator rights, so it cannot add its rule itself: [`state`] reads whether one is there, and
//! [`allow_command`] is what the app runs elevated (one UAC prompt) when the user asks it to.
//!
//! **The Mac's** application firewall asks the user by itself the first time an app listens, and has no rules to
//! read here: [`RULES`] is false, and the user is pointed to its settings ([`SETTINGS_HINT`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// This system's firewall has rules per program that Seedwell reads and can add ([`state`], [`allow_command`]).
pub const RULES: bool = cfg!(windows);

/// The firewall's name in user-visible text.
#[cfg(windows)]
pub const NAME: &str = "Windows Firewall";
/// The firewall's name in user-visible text.
#[cfg(not(windows))]
pub const NAME: &str = "the firewall";

/// Where the user lets an app in, as a sentence for a notice (empty: [`allow_command`] does it).
#[cfg(target_os = "macos")]
pub const SETTINGS_HINT: &str =
    "If the Mac's firewall is on, let Seedwell accept incoming connections in System Settings › Network › Firewall › Options.";
/// Where the user lets an app in, as a sentence for a notice (empty: [`allow_command`] does it).
#[cfg(windows)]
pub const SETTINGS_HINT: &str = "";
/// Where the user lets an app in, as a sentence for a notice (empty: [`allow_command`] does it).
#[cfg(not(any(target_os = "macos", windows)))]
pub const SETTINGS_HINT: &str = "If a firewall runs on this computer, let Seedwell accept incoming connections.";

/// The name of the rule [`allow_command`] adds.
pub const RULE_NAME: &str = "Seedwell";

/// Whether the firewall lets other devices connect to a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// An enabled inbound allow rule for the program covers every current network's profile, and no enabled block
    /// rule for it applies (or the firewall is off for those networks).
    Allowed,
    /// No enabled inbound rule for the program allows it on a current network: Windows blocks it.
    Missing,
    /// An enabled inbound block rule for the program applies (a block wins over an allow).
    Blocked,
    /// It could not be read (no answer in time, the check failed).
    Unknown,
}

/// What [`state`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// What the rules say.
    pub rule: Rule,
    /// The profiles of the current networks ("Public", "Private", "Domain"), as Windows names them.
    pub profiles: Vec<String>,
    /// Windows asks the user for a rule when a program first listens on every one of those networks (`None`: not
    /// known). False on a Public network by default: that is why no rule was made.
    pub asks: Option<bool>,
}

impl State {
    /// Nothing could be read.
    pub fn unknown() -> State {
        State { rule: Rule::Unknown, profiles: Vec::new(), asks: None }
    }
}

/// The program other devices connect to: this process's executable (the app, which hosts the core).
pub fn own_program() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// What the app runs with administrator rights to let `exe` through the firewall: a program and its arguments as one
/// string, as Windows' `ShellExecuteW` takes them (verb `runas`). With no rule ([`Rule::Missing`], also when it could
/// not be read) an inbound allow rule for the program on every profile, TCP and UDP (the sync port's uTP twin) alike;
/// with a block rule ([`Rule::Blocked`]) every inbound rule for the program is made an enabled allow rule on every
/// profile (a block would win over a new allow). `None` with nothing to do ([`Rule::Allowed`]) or a path netsh cannot
/// take (one with a `"`, which Windows paths never hold).
pub fn allow_command(exe: &str, rule: Rule) -> Option<(String, String)> {
    if exe.is_empty() || exe.contains('"') {
        return None;
    }
    let args = match rule {
        Rule::Allowed => return None,
        Rule::Missing | Rule::Unknown => {
            format!("advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow program=\"{exe}\" enable=yes profile=any")
        }
        Rule::Blocked => {
            format!("advfirewall firewall set rule name=all dir=in program=\"{exe}\" new action=allow enable=yes profile=any")
        }
    };
    Some(("netsh.exe".into(), args))
}

/// Reads whether the firewall lets other devices connect to `exe`, within `limit` (then [`Rule::Unknown`]). Blocks
/// (it starts PowerShell, about a second): never on a UI thread. `None` where the firewall has no rules to read
/// ([`RULES`] false).
///
/// Windows: PowerShell's NetSecurity module (`Get-NetFirewallApplicationFilter`, `Get-NetFirewallRule`,
/// `Get-NetFirewallProfile`, `Get-NetConnectionProfile`) on the active policy store, which answers in the same words
/// in every system language (netsh's output is translated), in a hidden window. The script is [`SCRIPT`], the path in
/// [`EXE_VAR`]; its one line of output is read by [`parse`].
pub fn state(exe: &Path, limit: Duration) -> Option<State> {
    #[cfg(windows)]
    {
        Some(windows::state(exe, limit))
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, limit);
        None
    }
}

/// The environment variable that carries the program's path to [`SCRIPT`]: never inside the script's text, where a
/// quote in a folder name (PowerShell's single-quoted strings end at `'` and at U+2018 to U+201B) would end the string
/// early, break the check or run what follows.
pub const EXE_VAR: &str = "SEEDWELL_EXE";

/// The PowerShell script [`state`] runs (PowerShell 5.1, Windows' own), the program's path in [`EXE_VAR`]. It prints
/// one line: `<allowed|missing|blocked|off>|<profiles, comma-separated>|<NotifyOnListen of each, comma-separated>`.
pub const SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$p = $env:SEEDWELL_EXE
$need = 0; $names = @()
foreach ($c in @(Get-NetConnectionProfile | ForEach-Object { [string]$_.NetworkCategory })) {
  switch ($c) { 'DomainAuthenticated' { $need = $need -bor 1; $names += 'Domain' } 'Private' { $need = $need -bor 2; $names += 'Private' } default { $need = $need -bor 4; $names += 'Public' } }
}
if ($need -eq 0) { $need = 7; $names = @('Domain', 'Private', 'Public') }
$names = @($names | Select-Object -Unique)
$profiles = @($names | ForEach-Object { Get-NetFirewallProfile -PolicyStore ActiveStore -Name $_ })
$notify = @($profiles | ForEach-Object { [string]$_.NotifyOnListen })
if (@($profiles | Where-Object { [string]$_.Enabled -eq 'True' }).Count -eq 0) { $state = 'off' } else {
  $rules = @(Get-NetFirewallApplicationFilter -PolicyStore ActiveStore -Program $p -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object { [string]$_.Direction -eq 'Inbound' -and [string]$_.Enabled -eq 'True' })
  $block = @($rules | Where-Object { [string]$_.Action -eq 'Block' -and (([int]$_.Profile -eq 0) -or (([int]$_.Profile -band $need) -ne 0)) })
  $allow = @($rules | Where-Object { [string]$_.Action -eq 'Allow' -and (([int]$_.Profile -eq 0) -or (([int]$_.Profile -band $need) -eq $need)) })
  $state = if ($block.Count -gt 0) { 'blocked' } elseif ($allow.Count -gt 0) { 'allowed' } else { 'missing' }
}
$state + '|' + ($names -join ',') + '|' + ($notify -join ',')
"#;

/// Reads the line [`SCRIPT`] prints. Anything else (an error, nothing) is [`Rule::Unknown`]. A firewall that is off is
/// [`Rule::Allowed`]: nothing to allow. `NotifyOnListen` is "True", "False" or "NotConfigured" (Windows' default,
/// which asks); `asks` is whether every current network's profile asks.
pub fn parse(out: &str) -> State {
    let Some(line) = out.lines().map(str::trim).rfind(|l| !l.is_empty()) else { return State::unknown() };
    let mut parts = line.split('|');
    let rule = match parts.next() {
        Some("allowed" | "off") => Rule::Allowed,
        Some("missing") => Rule::Missing,
        Some("blocked") => Rule::Blocked,
        _ => return State::unknown(),
    };
    let list = |s: Option<&str>| s.unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let profiles = list(parts.next());
    let notify = list(parts.next());
    let asks = if notify.is_empty() { None } else { Some(notify.iter().all(|n| !n.eq_ignore_ascii_case("False"))) };
    State { rule, profiles, asks }
}

/// `bytes` in Base64 (PowerShell's `-EncodedCommand` takes the script so, as UTF-16LE: no quoting of the command line
/// can break it).
pub fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(windows)]
mod windows {
    use std::io::Read as _;
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{EXE_VAR, SCRIPT, State, base64, parse};

    pub(super) fn state(exe: &Path, limit: Duration) -> State {
        let utf16: Vec<u8> = SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
        // Windows' own PowerShell by its full path (not whatever the PATH finds first).
        let ps = std::env::var_os("SystemRoot")
            .map(|r| Path::new(&r).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| "powershell.exe".into());
        let mut c = Command::new(ps);
        c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"])
            .arg(base64(&utf16))
            .env(EXE_VAR, exe)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        let Ok(mut child) = c.spawn() else { return State::unknown() };
        let end = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return State::unknown();
                }
            }
        }
        let mut out = String::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        parse(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure: the user's install path (it has spaces, and may have non-ASCII letters) cut at a space, so netsh
    /// adds a rule for another program (or none) and the notice never goes.
    #[test]
    fn the_command_quotes_a_path_with_spaces_whole() {
        let exe = r"C:\Users\Kari Nordmann\AppData\Local\Programs\Seedwell\Seedwell.exe";
        let (program, args) = allow_command(exe, Rule::Missing).unwrap();
        assert_eq!(program, "netsh.exe");
        assert_eq!(
            args,
            r#"advfirewall firewall add rule name="Seedwell" dir=in action=allow program="C:\Users\Kari Nordmann\AppData\Local\Programs\Seedwell\Seedwell.exe" enable=yes profile=any"#
        );
        assert_eq!(allow_command(exe, Rule::Unknown).unwrap().1, args, "not known: added all the same (harmless)");
        let (_, set) = allow_command(exe, Rule::Blocked).unwrap();
        assert!(set.starts_with("advfirewall firewall set rule name=all dir=in program=\"C:\\Users\\Kari Nordmann\\"), "{set}");
        assert!(set.ends_with("Seedwell.exe\" new action=allow enable=yes profile=any"), "{set}");
        assert_eq!(allow_command(exe, Rule::Allowed), None);
        assert_eq!(allow_command("C:\\a\"b.exe", Rule::Missing), None, "a quote would end the value early");
    }

    /// The failure: the check's answer read wrongly: an allow rule taken for none (a notice that never goes), a block
    /// rule or no rule taken for an allow (a blocked PC told nothing), an error or no answer taken for either.
    #[test]
    fn the_check_maps_yes_no_blocked_and_unknown() {
        let s = parse("missing|Public|False\r\n");
        assert_eq!((s.rule, s.profiles.as_slice(), s.asks), (Rule::Missing, ["Public".to_string()].as_slice(), Some(false)));
        assert_eq!(parse("allowed|Private|NotConfigured").rule, Rule::Allowed);
        assert_eq!(parse("allowed|Private|NotConfigured").asks, Some(true), "not configured: Windows asks");
        assert_eq!(parse("off|Public|False").rule, Rule::Allowed, "the firewall is off");
        assert_eq!(parse("blocked|Private,Public|True,False").rule, Rule::Blocked);
        assert_eq!(parse("blocked|Private,Public|True,False").asks, Some(false), "one network does not ask");
        assert_eq!(parse("").rule, Rule::Unknown);
        assert_eq!(parse("Get-NetFirewallProfile : Access is denied.").rule, Rule::Unknown);
        assert_eq!(parse("WARNING: something\nmissing||").asks, None, "the last line counts");
        assert_eq!(parse("WARNING: something\nmissing||").rule, Rule::Missing);
    }

    /// The failure (review): the path inside the script's text, where a quote in a folder name (O'Brien, or a
    /// typographic one, U+2018 to U+201B, which PowerShell also ends a single-quoted string at) breaks the check or
    /// runs what follows; or the encoded command garbled, so it runs nothing.
    #[test]
    fn the_path_never_enters_the_script_and_it_encodes_whole() {
        assert!(SCRIPT.contains("$p = $env:SEEDWELL_EXE"), "{SCRIPT}");
        assert_eq!(EXE_VAR, "SEEDWELL_EXE");
        assert!(!SCRIPT.contains("{{") && !SCRIPT.contains("}}"), "no format! escapes left");
        // The netsh command quotes with double quotes, which no Windows path holds; typographic quotes stay as they are.
        let odd = "C:\\Users\\Ann O\u{2019}Brien \u{2018}\u{201A}\u{201B}'\\Seedwell.exe";
        assert!(allow_command(odd, Rule::Missing).unwrap().1.contains(&format!("program=\"{odd}\" ")));
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // UTF-16LE as PowerShell's -EncodedCommand takes it: "a" is 61 00.
        assert_eq!(base64(&[0x61, 0x00]), "YQA=");
    }

    /// The failure: the script does not run on Windows' own PowerShell (a syntax error, a cmdlet's parameter it lacks,
    /// a quoting the encoded command breaks), so every reading is "not known" and the notice can never go. This test's
    /// own program has no rule: missing, or allowed where the firewall is off (a CI runner); never not known.
    #[cfg(windows)]
    #[test]
    fn the_rule_of_this_program_is_read_on_windows() {
        let exe = std::env::current_exe().unwrap();
        let s = state(&exe, Duration::from_secs(60)).unwrap();
        assert_ne!(s.rule, Rule::Unknown, "{s:?}");
        assert!(!s.profiles.is_empty(), "{s:?}");
        // A path with every quote PowerShell ends a single-quoted string at: read all the same (no rule names it).
        let odd = std::path::PathBuf::from("C:\\Users\\Ann O\u{2019}Brien \u{2018}\u{201A}\u{201B}'; exit 3 #\\Seedwell.exe");
        let s = state(&odd, Duration::from_secs(60)).unwrap();
        assert!(matches!(s.rule, Rule::Missing | Rule::Allowed), "{s:?}");
    }

    /// The failure: a system without per-program rules (the Mac, whose firewall asks by itself) offered an action that
    /// cannot work there.
    #[test]
    fn only_windows_has_rules_to_read() {
        assert_eq!(state(Path::new("Seedwell"), Duration::from_millis(1)).is_some(), RULES);
        assert_eq!(RULES, cfg!(windows));
    }
}
