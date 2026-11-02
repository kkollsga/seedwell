//! Reading what Windows' registry says about an app: the executable of a command line (`shell\open\command`), an App
//! Paths value, environment variables in a `REG_EXPAND_SZ`. Pure string work, so it is tested on every system; the
//! registry itself is read in `windows.rs`.

/// The executable a registered command line starts: `"C:\Program Files\VideoLAN\VLC\vlc.exe" --started-from-file "%1"`
/// gives `C:\Program Files\VideoLAN\VLC\vlc.exe`. A command without quotes ends at the first `.exe` followed by a
/// space or the end (a path with spaces is often left unquoted), else at the first space. `None` for an empty one.
pub fn command_executable(command: &str) -> Option<String> {
    let s = command.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let exe = rest[..end].trim();
        return (!exe.is_empty()).then(|| exe.to_string());
    }
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(".exe") {
        let end = from + i + 4;
        if lower[end..].is_empty() || lower[end..].starts_with(' ') {
            return Some(s[..end].to_string());
        }
        from = end;
    }
    let exe = s.split(' ').next().unwrap_or("");
    (!exe.is_empty()).then(|| exe.to_string())
}

/// `%NAME%` in `s` replaced by `lookup(NAME)`; a name `lookup` does not know stays as it is, and so does a lone `%`.
pub fn expand_env(s: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) if end > 0 => {
                let name = &after[..end];
                match lookup(name) {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The file name of a Windows or Unix path, lower case: `C:\Program Files\VLC\VLC.EXE` gives `vlc.exe`.
pub fn file_name_lower(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase()
}

#[cfg_attr(not(windows), allow(dead_code))]
/// An App Paths or registered path value as a plain path: quotes around it removed, variables expanded.
pub(crate) fn plain_path(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let v = value.trim();
    let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
    expand_env(v, lookup)
}

#[cfg_attr(not(windows), allow(dead_code))]
/// The known player whose executable this is (by file name, any case).
pub(crate) fn known_by_executable<'a>(list: &'a [super::KnownPlayer], path: &str) -> Option<&'a super::KnownPlayer> {
    let name = file_name_lower(path);
    list.iter().find(|p| p.files.iter().any(|f| f.to_lowercase() == name))
}

#[cfg(test)]
mod tests {
    use super::super::WINDOWS_PLAYERS;
    use super::*;

    #[test]
    fn a_command_line_names_its_executable() {
        assert_eq!(
            command_executable(r#""C:\Program Files\VideoLAN\VLC\vlc.exe" --started-from-file "%1""#).as_deref(),
            Some(r"C:\Program Files\VideoLAN\VLC\vlc.exe")
        );
        assert_eq!(command_executable(r#""C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe" "%1""#).as_deref(), Some(r"C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe"));
        assert_eq!(command_executable(r"C:\Program Files\MPC-HC\mpc-hc64.exe /open %1").as_deref(), Some(r"C:\Program Files\MPC-HC\mpc-hc64.exe"), "unquoted, with spaces");
        assert_eq!(command_executable(r"C:\Tools\mpv.EXE").as_deref(), Some(r"C:\Tools\mpv.EXE"));
        assert_eq!(command_executable(r"C:\x.exed\mpv.exe %1").as_deref(), Some(r"C:\x.exed\mpv.exe"), "an .exe inside a name is not the end");
        assert_eq!(command_executable("rundll32 shell32.dll").as_deref(), Some("rundll32"));
        assert_eq!(command_executable("   "), None);
        assert_eq!(command_executable(r#""""#), None);
        assert_eq!(command_executable(r#""C:\unterminated"#), None);
    }

    #[test]
    fn variables_are_expanded_and_unknown_ones_kept() {
        let env = |n: &str| match n.to_ascii_uppercase().as_str() {
            "PROGRAMFILES" => Some(r"C:\Program Files".to_string()),
            "LOCALAPPDATA" => Some(r"C:\Users\a\AppData\Local".to_string()),
            _ => None,
        };
        assert_eq!(expand_env(r"%ProgramFiles%\VideoLAN\VLC\vlc.exe", env), r"C:\Program Files\VideoLAN\VLC\vlc.exe");
        assert_eq!(expand_env(r"%LOCALAPPDATA%\mpv\%NOPE%\mpv.exe", env), r"C:\Users\a\AppData\Local\mpv\%NOPE%\mpv.exe");
        assert_eq!(expand_env("100% sure %% here", env), "100% sure %% here");
        assert_eq!(expand_env("no vars", env), "no vars");
        assert_eq!(plain_path(r#" "%ProgramFiles%\MPC-HC\mpc-hc64.exe" "#, env), r"C:\Program Files\MPC-HC\mpc-hc64.exe");
    }

    #[test]
    fn an_executable_names_its_player_whatever_its_case() {
        assert_eq!(file_name_lower(r"C:\Program Files\VideoLAN\VLC\VLC.EXE"), "vlc.exe");
        assert_eq!(file_name_lower("/opt/mpv"), "mpv");
        assert_eq!(known_by_executable(WINDOWS_PLAYERS, r"C:\x\PotPlayerMini64.exe").map(|p| p.id), Some("potplayer"));
        assert_eq!(known_by_executable(WINDOWS_PLAYERS, r"D:\MPC-HC\MPC-HC.EXE").map(|p| p.id), Some("mpc-hc"));
        assert_eq!(known_by_executable(WINDOWS_PLAYERS, r"C:\Windows\explorer.exe"), None);
    }
}
