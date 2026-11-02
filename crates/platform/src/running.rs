//! Other processes of this user: whether one still runs ([`alive`]), which run with their program and arguments
//! ([`list`]), and which of them are other Seedwells ([`other_seedwells`]). Read only where the system lets a process
//! read another of the same user. Nothing here signals, stops or changes another process.

use std::path::{Path, PathBuf};

/// A running process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// Its program: the full path where the system says it, else its file name.
    pub exe: PathBuf,
    /// Its arguments after the program; `None` when the system would not say.
    pub args: Option<Vec<String>>,
}

/// Whether process `pid` still runs (a process of another user counts too).
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 only asks whether the process exists; nothing is sent and no memory is involved.
        unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: the handle is checked, read with GetExitCodeProcess into a local and closed once.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                // Denied: it exists (another user's); gone: it does not.
                return std::io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

/// The processes of this user, their arguments read for those whose program `wanted` takes (reading every process's
/// arguments would be slow). Processes the system does not show are left out.
pub fn list(wanted: impl Fn(&Path) -> bool) -> Vec<Process> {
    imp::list(&wanted)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Process;
    use std::ffi::{CStr, c_int, c_void};
    use std::path::{Path, PathBuf};

    pub(super) fn list(wanted: &dyn Fn(&Path) -> bool) -> Vec<Process> {
        // SAFETY: a null buffer asks for the count; the second call writes at most `pids.len()` ids into it.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as c_int; n as usize + 64];
        let bytes = c_int::try_from(pids.len() * size_of::<c_int>()).unwrap_or(c_int::MAX);
        // SAFETY: as above.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        pids.truncate(n.max(0) as usize);
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let mut out = Vec::new();
        for pid in pids.into_iter().filter(|p| *p > 0) {
            // SAFETY: proc_pidpath writes at most `buf.len()` bytes and returns how many.
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
            if len <= 0 {
                continue;
            }
            let exe = PathBuf::from(String::from_utf8_lossy(&buf[..len as usize]).into_owned());
            let args = if wanted(&exe) { args_of(pid) } else { None };
            out.push(Process { pid: pid as u32, exe, args });
        }
        out
    }

    /// A process's arguments (KERN_PROCARGS2: argc, the program, padding, then argv), without argv[0].
    fn args_of(pid: c_int) -> Option<Vec<String>> {
        let mut max: c_int = 0;
        let mut size = size_of::<c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: writes one c_int into `max`, `size` says how large it is.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 2, (&raw mut max).cast::<c_void>(), &mut size, std::ptr::null_mut(), 0) } != 0 || max <= 0 {
            return None;
        }
        let mut buf = vec![0u8; max as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: writes at most `size` bytes into `buf` and sets `size` to how many.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast::<c_void>(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return None;
        }
        parse_procargs(&buf[..size.min(buf.len())])
    }

    /// See [`args_of`].
    pub(super) fn parse_procargs(b: &[u8]) -> Option<Vec<String>> {
        let argc = i32::from_ne_bytes(b.get(..4)?.try_into().ok()?);
        let mut rest = &b[4..];
        // The program, then NUL padding.
        let end = rest.iter().position(|&c| c == 0)?;
        rest = &rest[end..];
        let start = rest.iter().position(|&c| c != 0)?;
        rest = &rest[start..];
        let mut args = Vec::new();
        for _ in 0..argc.max(0) {
            let s = CStr::from_bytes_until_nul(rest).ok()?;
            args.push(s.to_string_lossy().into_owned());
            rest = &rest[s.to_bytes().len() + 1..];
        }
        (!args.is_empty()).then(|| args.split_off(1))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::Process;
    use std::path::{Path, PathBuf};

    pub(super) fn list(wanted: &dyn Fn(&Path) -> bool) -> Vec<Process> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        let mut out = Vec::new();
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Ok(exe) = std::fs::read_link(e.path().join("exe")) else { continue };
            let args = if wanted(&exe) {
                std::fs::read(e.path().join("cmdline"))
                    .ok()
                    .map(|b| b.split(|&c| c == 0).filter(|a| !a.is_empty()).skip(1).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
            } else {
                None
            };
            out.push(Process { pid, exe: PathBuf::from(exe), args });
        }
        out
    }
}

#[cfg(windows)]
mod imp {
    use super::Process;
    use std::ffi::c_void;
    use std::path::{Path, PathBuf};
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    fn wide(s: &[u16]) -> String {
        String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())])
    }

    pub(super) fn list(wanted: &dyn Fn(&Path) -> bool) -> Vec<Process> {
        let mut out = Vec::new();
        // SAFETY: the snapshot handle is checked and closed once; each entry is a local with its size set.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut e) != 0;
            while more {
                let pid = e.th32ProcessID;
                if pid != 0 {
                    let name = PathBuf::from(wide(&e.szExeFile));
                    out.push(details(pid, name, wanted));
                }
                more = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    /// Its full path and, when `wanted` takes it, its arguments (a process of another user, or a protected one, keeps
    /// only its name).
    unsafe fn details(pid: u32, name: PathBuf, wanted: &dyn Fn(&Path) -> bool) -> Process {
        // SAFETY: the handle is checked and closed once; buffers are locals with their sizes passed.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Process { pid, exe: name, args: None };
            }
            let mut buf = vec![0u16; 32_768];
            let mut len = buf.len() as u32;
            let exe = if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0 {
                PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]))
            } else {
                name
            };
            let args = if wanted(&exe) { command_line(h) } else { None };
            CloseHandle(h);
            Process { pid, exe, args }
        }
    }

    /// The command line (ProcessCommandLineInformation, Windows 8.1 and later), split as the C runtime does, without
    /// the program.
    unsafe fn command_line(h: HANDLE) -> Option<Vec<String>> {
        // SAFETY: the buffer outlives the UNICODE_STRING read from its start, whose Buffer points into it; the argv
        // block CommandLineToArgvW returns is read for `n` entries and freed once.
        unsafe {
            let mut need = 0u32;
            let _ = NtQueryInformationProcess(h, ProcessCommandLineInformation, std::ptr::null_mut(), 0, &mut need);
            if need == 0 || need > 1 << 20 {
                return None;
            }
            // u64 words: the UNICODE_STRING at its start is aligned.
            let mut buf = vec![0u64; (need as usize).div_ceil(8)];
            if NtQueryInformationProcess(h, ProcessCommandLineInformation, buf.as_mut_ptr().cast::<c_void>(), need, &mut need) < 0 {
                return None;
            }
            let us = &*buf.as_ptr().cast::<UNICODE_STRING>();
            if us.Buffer.is_null() {
                return None;
            }
            let mut line: Vec<u16> = std::slice::from_raw_parts(us.Buffer, us.Length as usize / 2).to_vec();
            line.push(0);
            let mut n = 0i32;
            let argv = CommandLineToArgvW(line.as_ptr(), &mut n);
            if argv.is_null() {
                return None;
            }
            let mut args = Vec::new();
            for i in 1..n.max(0) as usize {
                let p = *argv.add(i);
                let mut len = 0;
                while *p.add(len) != 0 {
                    len += 1;
                }
                args.push(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
            }
            LocalFree(argv.cast());
            Some(args)
        }
    }
}

// MARK: Other Seedwells

/// What another Seedwell process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedwellKind {
    /// The app.
    App,
    /// `seedwell daemon`.
    Daemon,
}

/// Another Seedwell app or daemon on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OtherSeedwell {
    pub pid: u32,
    pub kind: SeedwellKind,
    /// Its state folder; `None` when it cannot be known (its arguments could not be read).
    pub state_dir: Option<PathBuf>,
    pub exe: PathBuf,
}

/// The app's program: `Seedwell` in a Mac app bundle, `Seedwell.exe` on Windows (the CLI is `cli\seedwell.exe`,
/// which Windows' case-blind names would also match: it is told apart by its folder).
fn is_app(exe: &Path) -> bool {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if cfg!(windows) {
        let in_cli = exe.parent().and_then(Path::file_name).and_then(|n| n.to_str()).is_some_and(|d| d.eq_ignore_ascii_case("cli"));
        name.eq_ignore_ascii_case("Seedwell.exe") && !in_cli
    } else {
        name == "Seedwell"
    }
}

/// The command-line tool's program (`seedwell`, `seedwell.exe`), which a daemon runs as.
fn is_cli(exe: &Path) -> bool {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if cfg!(windows) { name.eq_ignore_ascii_case("seedwell.exe") && !is_app(exe) } else { name == "seedwell" }
}

/// A program [`other_seedwells`] reads the arguments of.
pub fn is_seedwell(exe: &Path) -> bool {
    is_app(exe) || is_cli(exe)
}

/// The value of `--state-dir` (`--state-dir X` or `--state-dir=X`).
fn state_arg(args: &[String]) -> Option<PathBuf> {
    args.iter().enumerate().find_map(|(i, a)| match a.strip_prefix("--state-dir") {
        Some("") => args.get(i + 1).map(PathBuf::from),
        Some(v) => v.strip_prefix('=').map(PathBuf::from),
        None => None,
    })
}

/// The same folder, as far as can be told (both made absolute and resolved where they exist; Windows compares
/// without case).
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (norm(a), norm(b));
    if cfg!(windows) { a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() } else { a == b }
}

/// The Seedwell apps and daemons among `procs` other than this process (`me`) whose state folder is not `mine`, or
/// cannot be known. An app without `--state-dir` uses `default_state` (the user's own); a command-line tool that is
/// not a daemon (an agent's `seedwell chat watch`, this `seedwell status`) is no other Seedwell.
pub fn other_seedwells(procs: &[Process], me: u32, mine: &Path, default_state: Option<&Path>) -> Vec<OtherSeedwell> {
    let mut out: Vec<OtherSeedwell> = procs
        .iter()
        .filter(|p| p.pid != me)
        .filter_map(|p| {
            let kind = if is_app(&p.exe) {
                SeedwellKind::App
            } else if is_cli(&p.exe) && p.args.as_ref().is_some_and(|a| a.iter().any(|x| x == "daemon")) {
                SeedwellKind::Daemon
            } else {
                return None;
            };
            let state_dir = match (&p.args, kind) {
                (None, _) => None,
                (Some(a), SeedwellKind::App) => state_arg(a).or_else(|| default_state.map(Path::to_path_buf)),
                (Some(a), SeedwellKind::Daemon) => state_arg(a),
            };
            if state_dir.as_deref().is_some_and(|d| same_dir(d, mine)) {
                return None;
            }
            Some(OtherSeedwell { pid: p.pid, kind, state_dir, exe: p.exe.clone() })
        })
        .collect();
    out.sort_by_key(|o| o.pid);
    out
}

/// [`other_seedwells`] on this computer now, for this process whose state folder is `mine`.
pub fn other_seedwells_now(mine: &Path) -> Vec<OtherSeedwell> {
    other_seedwells(&list(is_seedwell), std::process::id(), mine, crate::dirs::state_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, exe: &str, args: Option<&[&str]>) -> Process {
        Process { pid, exe: exe.into(), args: args.map(|a| a.iter().map(|s| s.to_string()).collect()) }
    }

    #[cfg(not(windows))]
    #[test]
    fn other_apps_and_daemons_on_another_state_folder_are_found_and_nothing_else() {
        let d = tempfile::tempdir().unwrap();
        let mine = d.path().join("mine");
        let other = d.path().join("old");
        std::fs::create_dir_all(&mine).unwrap();
        let o = other.display().to_string();
        let procs = [
            // This process, an app on the same folder (the default), agents' watches, a daemon elsewhere, an app on
            // another folder, an app whose arguments could not be read, something else called seedwell-ish.
            p(1, "/Applications/Seedwell.app/Contents/MacOS/Seedwell", Some(&[])),
            p(2, "/usr/local/bin/seedwell", Some(&["chat", "watch", "Dev", "--unread"])),
            p(3, "/opt/old/seedwell", Some(&["daemon", "--state-dir", &o, "--download-dir", "/tmp/x"])),
            p(4, "/tmp/qa/Seedwell.app/Contents/MacOS/Seedwell", Some(&[&format!("--state-dir={o}")])),
            p(5, "/Applications/Seedwell.app/Contents/MacOS/Seedwell", None),
            p(6, "/usr/bin/seedwell-relay", Some(&["daemon"])),
            p(7, "/usr/local/bin/seedwell", Some(&["daemon", "--state-dir", &mine.display().to_string()])),
            p(9, "/usr/local/bin/seedwell", Some(&["status"])),
        ];
        let found = other_seedwells(&procs, 9, &mine, Some(&mine));
        let got: Vec<(u32, SeedwellKind, Option<PathBuf>)> = found.iter().map(|o| (o.pid, o.kind, o.state_dir.clone())).collect();
        assert_eq!(got, [(3, SeedwellKind::Daemon, Some(other.clone())), (4, SeedwellKind::App, Some(other)), (5, SeedwellKind::App, None)]);
        // The default state folder is someone else's from where a daemon on its own folder looks.
        let from_daemon = other_seedwells(&procs, 7, &mine.join("nested"), Some(&mine));
        assert!(from_daemon.iter().any(|o| o.pid == 1), "{from_daemon:?}");
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_app_and_its_cli_are_told_apart_by_folder() {
        let mine = Path::new(r"C:\Users\me\AppData\Local\Seedwell");
        let procs = [
            p(1, r"C:\Users\me\AppData\Local\Programs\Seedwell\Seedwell.exe", Some(&[])),
            p(2, r"C:\Users\me\AppData\Local\Programs\Seedwell\cli\seedwell.exe", Some(&["chat", "watch"])),
            p(3, r"C:\old\seedwell.exe", Some(&["daemon", "--state-dir", r"C:\old\state"])),
        ];
        let got: Vec<u32> = other_seedwells(&procs, 99, mine, Some(mine)).iter().map(|o| o.pid).collect();
        assert_eq!(got, [3]);
    }

    #[test]
    fn this_process_is_alive_and_listed_and_a_finished_one_is_not() {
        assert!(alive(std::process::id()));
        let me = list(|_| true).into_iter().find(|p| p.pid == std::process::id()).expect("this process is listed");
        assert!(me.exe.file_name().is_some());
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" }).args(if cfg!(windows) { &["/C", "exit"][..] } else { &[][..] }).spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!alive(pid), "reaped: gone");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_process_arguments_are_parsed_without_the_program() {
        let mut b = 3i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/usr/local/bin/seedwell\0\0\0\0seedwell\0daemon\0--state-dir\0PATH=/bin\0");
        assert_eq!(imp::parse_procargs(&b), Some(vec!["daemon".to_string(), "--state-dir".to_string()]));
        assert!(imp::parse_procargs(&[1, 0]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn this_process_arguments_are_read() {
        let me = list(|_| true).into_iter().find(|p| p.pid == std::process::id()).unwrap();
        let want: Vec<String> = std::env::args().skip(1).collect();
        assert_eq!(me.args, Some(want));
    }
}
