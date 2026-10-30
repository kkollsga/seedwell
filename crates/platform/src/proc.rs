//! The running process: limits, cores, priorities, memory.

use std::io;

/// macOS refuses `setrlimit(NOFILE)` above `OPEN_MAX` regardless of the hard limit.
pub const NOFILE_SAFE_CAP: u64 = 10_240;

/// Raises the soft limit of open files towards `target` (capped by the hard limit, the kernel's
/// per-process maximum and [`NOFILE_SAFE_CAP`]). Returns the resulting limit.
///
/// Call once at startup: the macOS default soft limit of 256 is below the fd-cache default.
/// Windows has no such small limit on file handles: `target` is returned as it is.
pub fn raise_nofile_limit(target: u64) -> io::Result<u64> {
    #[cfg(unix)]
    {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `lim` is a valid, writable rlimit struct for the duration of the call.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut want = target.min(NOFILE_SAFE_CAP);
        if lim.rlim_max != libc::RLIM_INFINITY {
            want = want.min(lim.rlim_max);
        }
        if let Some(k) = sysctl_u64(c"kern.maxfilesperproc") {
            want = want.min(k);
        }
        if lim.rlim_cur >= want {
            return Ok(lim.rlim_cur);
        }
        lim.rlim_cur = want;
        // SAFETY: `lim` is a valid, initialised rlimit struct; the kernel only reads it.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(want)
    }
    #[cfg(windows)]
    {
        Ok(target)
    }
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut buf = [0u8; 8];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `name` is NUL-terminated; `buf`/`len` describe a valid writable buffer; no new value
    // is passed (null, 0).
    let r = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if r != 0 {
        return None;
    }
    match len {
        4 => Some(u64::from(u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]))),
        8 => Some(u64::from_ne_bytes(buf)),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn sysctl_u64(_name: &std::ffi::CStr) -> Option<u64> {
    None
}

/// Number of performance cores (Apple Silicon `hw.perflevel0`), else logical CPUs.
///
/// Hashing is pinned to this many threads so it never competes with efficiency cores' power budget.
pub fn perf_core_count() -> usize {
    sysctl_u64(c"hw.perflevel0.logicalcpu")
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(2)
}

/// Lowers the calling thread's disk I/O priority (scrub/recheck must not hurt foreground I/O).
pub fn set_background_io() {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: setpriority with PRIO_DARWIN_THREAD and who = 0 targets only the calling thread
        // and takes no pointers.
        unsafe {
            libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
        // SAFETY: the pseudo handle of the calling thread is always valid; background mode lowers
        // only this thread's I/O and memory priority.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
    }
}

/// Ask the allocator to give freed memory back to the OS and return roughly how many bytes it
/// released (0 when unknown or when the platform has no such call).
///
/// A burst of work (verifying hundreds of torrents at start-up) leaves the allocator's freed
/// blocks resident: on macOS they stay in the per-thread magazines and count toward the process
/// footprint until the zone is told to relieve pressure. An idle seeder that once verified 500
/// torrents otherwise keeps several times the live heap for as long as it runs.
pub fn release_free_memory() -> usize {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            /// `<malloc/malloc.h>`: `size_t malloc_zone_pressure_relief(malloc_zone_t *zone, size_t goal)`.
            /// A null zone means every zone; a goal of 0 means "as much as possible".
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: the function is part of libSystem on every macOS this crate supports, takes no
        // pointers it dereferences when `zone` is null, and is documented as safe to call from any
        // thread at any time.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` has no preconditions; it only walks the allocator's own free lists.
        unsafe { libc::malloc_trim(0) };
        0
    }
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    {
        0
    }
}

/// Offset of the local time zone from UTC, in seconds, valid at Unix time `unix` (daylight saving
/// included). UTC (0) when the system cannot say.
pub fn utc_offset(unix: i64) -> i32 {
    #[cfg(unix)]
    {
        // `time_t` is 64 bits wherever Seedwell runs; the check is for a 32-bit one.
        #[allow(irrefutable_let_patterns)]
        let Ok(t) = libc::time_t::try_from(unix) else { return 0 };
        // SAFETY: a zeroed `tm` is a valid value; `localtime_r` writes only into the `tm` we own.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live values for the duration of the call.
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return 0;
        }
        i32::try_from(tm.tm_gmtoff).unwrap_or(0)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime};
        // FILETIME counts 100 ns from 1601-01-01.
        const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
        let Some(ticks) = unix.checked_add(EPOCH_DIFF_SECS).and_then(|s| s.checked_mul(10_000_000)).filter(|t| *t >= 0) else { return 0 };
        let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
        // SAFETY: plain C structs, all-zero is valid; every pointer is to a live local for its call.
        unsafe {
            let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
            let mut back: FILETIME = std::mem::zeroed();
            if FileTimeToSystemTime(&ft, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0
                || SystemTimeToFileTime(&local, &mut back) == 0
            {
                return 0;
            }
            let local_ticks = (i64::from(back.dwHighDateTime) << 32) | i64::from(back.dwLowDateTime);
            ((local_ticks - ticks) / 10_000_000) as i32
        }
    }
}

/// A command run by the system shell (`/bin/sh -c`; `cmd /C` on Windows) in a group of its own, so
/// [`Shell::kill_all`] stops everything it started too: a process group on Unix, a Job Object on
/// Windows (whose processes also end when Seedwell does).
pub struct Shell {
    child: std::process::Child,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl Shell {
    /// Start `command`; `setup` sets its environment and standard streams.
    pub fn spawn(command: &str, setup: impl FnOnce(&mut std::process::Command)) -> io::Result<Shell> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(command).process_group(0);
            setup(&mut c);
            Ok(Shell { child: c.spawn()? })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
            };
            // SAFETY: no security attributes and no name; the handle is owned below.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh handle nobody else owns.
            let job = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: an all-zero limit struct is valid; the size passed is its own.
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
            // cmd.exe reads its command line by its own rules, not the C runtime's that `arg` quotes
            // for (`"` became `\"`, so a quoted path broke): it gets the command as written.
            use std::os::windows::process::CommandExt as _;
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").raw_arg(command);
            setup(&mut c);
            let mut child = c.spawn()?;
            // SAFETY: both handles are open; a process can be assigned to a job right after it starts.
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
                let e = io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
            Ok(Shell { child, job })
        }
    }

    /// The exit status once the shell has ended.
    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Stop the shell and everything it started, and wait for the shell.
    pub fn kill_all(&mut self) {
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: a negative pid names the process group the shell leads; no pointers.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the job handle is open; terminating it ends every process assigned to it.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A command for `program` that keeps running when the terminal that started it closes: on Unix
/// it ignores SIGHUP (through `/bin/sh`, which then becomes the program), on Windows it gets no
/// console and its own process group. Add arguments and streams as usual.
///
/// On Windows this also changes the calling process: its own stdin, stdout and stderr stop being
/// inheritable. A child gets every inheritable handle of its parent, whatever streams it was given,
/// so a long-lived detached child held the pipes of whoever reads this process's output (a shell's
/// `$(…)`, a test's `Command::output()`), which then waited for the child to end. Later children
/// still get these streams when they ask for them (`Stdio::inherit()` hands over a copy).
pub fn detached_command(program: &std::path::Path) -> std::process::Command {
    #[cfg(unix)]
    {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg("trap '' HUP; exec \"$0\" \"$@\"").arg(program);
        c
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        for h in [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()] {
            if !h.is_null() {
                // SAFETY: `h` is this process's standard handle (null when there is none: skipped);
                // only its inherit flag changes. A failure (no such handle) leaves it as it was.
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
            }
        }
        let mut c = std::process::Command::new(program);
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        c
    }
}

/// A command whose process can later be asked to stop with [`request_stop`]. On Windows it gets its
/// own process group, the only way another process can send it Ctrl+Break; Ctrl+C typed in a shared
/// console then no longer reaches it. Elsewhere a plain command.
pub fn stoppable_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut, reason = "changed on Windows only")]
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        c.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    c
}

/// Ask `child` (started with [`stoppable_command`]) to stop as a user would: SIGTERM on Unix,
/// Ctrl+Break on Windows ([`crate::signal::Shutdown`] hears both). Returns at once; wait for the child
/// to end.
pub fn request_stop(child: &std::process::Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(child.id()).map_err(|_| io::Error::other("pid out of range"))?;
        // SAFETY: kill() only sends a signal to the process with this id; no memory is involved.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        // SAFETY: sends Ctrl+Break to the process group whose id is the child's (its own group, made by
        // stoppable_command); no memory is involved.
        if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }
}

/// This process measured now; each figure `None` when the system would not say.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SelfStats {
    /// Resident memory in bytes (the working set on Windows).
    pub rss_bytes: Option<u64>,
    /// CPU use in percent of one core (can pass 100). Unix: as `ps` reports it (on Linux the average
    /// over the process's life, on macOS a recent decaying average); Windows: the average over the
    /// process's life.
    pub cpu_percent: Option<f64>,
    /// Threads.
    pub threads: Option<u32>,
    /// Open file descriptors on Unix (sockets included); open handles on Windows.
    pub open_files: Option<u32>,
}

/// Measure this process. On Unix it runs `ps` once or twice (run it on a blocking thread); on
/// Windows it asks the system directly.
pub fn self_stats() -> SelfStats {
    let mut out = SelfStats::default();
    #[cfg(unix)]
    {
        let pid = std::process::id();
        let ps = |args: &[&str]| -> Option<String> {
            // The C locale: in a Norwegian one `ps` writes "0,0".
            let o = std::process::Command::new("ps").env("LC_ALL", "C").args(args).arg("-p").arg(pid.to_string()).output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        if let Some(t) = ps(&["-o", "rss=", "-o", "%cpu="]) {
            let mut it = t.split_whitespace();
            out.rss_bytes = it.next().and_then(|v| v.parse::<u64>().ok()).map(|kb| kb * 1024);
            out.cpu_percent = it.next().and_then(decimal);
        }
        out.threads = if cfg!(target_os = "macos") {
            // One line per thread plus a header.
            ps(&["-M"]).map(|t| t.lines().count().saturating_sub(1) as u32)
        } else {
            ps(&["-o", "nlwp="]).and_then(|t| t.trim().parse().ok())
        };
        let fd_dir = if cfg!(target_os = "macos") { "/dev/fd" } else { "/proc/self/fd" };
        // The directory handle used for the listing is itself one descriptor.
        out.open_files = std::fs::read_dir(fd_dir).ok().map(|d| d.count().saturating_sub(1) as u32);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount, GetProcessTimes};
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        // SAFETY: the pseudo handle of this process needs no closing; every out-pointer is a local of
        // the right type and size; the snapshot handle is closed once.
        unsafe {
            let me = GetCurrentProcess();
            let mut mem: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            mem.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(me, &mut mem, mem.cb) != 0 {
                out.rss_bytes = Some(mem.WorkingSetSize as u64);
            }
            let (mut created, mut exited, mut kernel, mut user) = std::mem::zeroed::<(FILETIME, FILETIME, FILETIME, FILETIME)>();
            if GetProcessTimes(me, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
                // FILETIME counts 100 ns since 1601; now in the same units from the system clock.
                let since_1601 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_nanos() as u64 / 100 + 116_444_736_000_000_000);
                if let Some(now) = since_1601
                    && now > ticks(created)
                {
                    out.cpu_percent = Some((ticks(kernel) + ticks(user)) as f64 * 100.0 / (now - ticks(created)) as f64);
                }
            }
            let mut handles = 0u32;
            if GetProcessHandleCount(me, &mut handles) != 0 {
                out.open_files = Some(handles);
            }
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap != INVALID_HANDLE_VALUE {
                let pid = std::process::id();
                let mut e: THREADENTRY32 = std::mem::zeroed();
                e.dwSize = size_of::<THREADENTRY32>() as u32;
                let mut n = 0u32;
                let mut ok = Thread32First(snap, &mut e) != 0;
                while ok {
                    n += u32::from(e.th32OwnerProcessID == pid);
                    ok = Thread32Next(snap, &mut e) != 0;
                }
                CloseHandle(snap);
                out.threads = Some(n);
            }
        }
    }
    out
}

/// "12.5", or "12,5" from a `ps` that kept a locale's decimal comma.
#[cfg(any(unix, test))]
fn decimal(v: &str) -> Option<f64> {
    v.replace(',', ".").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_measured_whatever_the_locale() {
        assert_eq!((decimal("12.5"), decimal("0,0"), decimal("x")), (Some(12.5), Some(0.0), None));
        let s = self_stats();
        eprintln!("{s:?}");
        assert!(s.cpu_percent.is_some() && s.rss_bytes.is_some_and(|b| b > 0), "{s:?}");
        assert!(s.threads.is_some_and(|n| n >= 1) && s.open_files.is_some_and(|n| n >= 1), "{s:?}");
    }

    #[test]
    fn a_stopped_shell_takes_what_it_started_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        // The shell starts a long child and says which; then it waits for it.
        #[cfg(unix)]
        let cmd = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        #[cfg(windows)]
        let cmd = format!("echo started > \"{}\" & ping -n 30 127.0.0.1 >NUL", pid_file.display());
        let mut sh = Shell::spawn(&cmd, |c| {
            c.stdout(std::process::Stdio::null());
        })
        .unwrap();
        let t = std::time::Instant::now();
        while std::fs::read_to_string(&pid_file).map_or(true, |s| s.trim().is_empty()) {
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "the shell never started its child");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(sh.try_wait().unwrap().is_none());
        sh.kill_all();
        assert!(sh.try_wait().unwrap().is_some());
        #[cfg(unix)]
        {
            let child: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
            let t = std::time::Instant::now();
            // SAFETY: signal 0 only asks whether the process exists.
            while unsafe { libc::kill(child, 0) } == 0 {
                assert!(t.elapsed() < std::time::Duration::from_secs(5), "the shell's child {child} outlived the stop");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn the_local_offset_is_a_whole_quarter_hour_within_a_day() {
        let off = utc_offset(1_790_000_000);
        assert!(off.abs() <= 14 * 3600 && off % 900 == 0, "{off}");
    }

    /// Not a test on its own: `a_detached_child_does_not_hold_the_callers_pipes` runs the test binary
    /// with `TRS_DETACHED_HELPER` set to play the processes it needs.
    #[test]
    fn detached_helper() {
        use std::io::Write as _;
        let Some(mode) = std::env::var_os("TRS_DETACHED_HELPER") else { return };
        let me = std::env::current_exe().unwrap();
        let helper = |m: &str| {
            let mut c = std::process::Command::new(&me);
            c.args(["--exact", "proc::tests::detached_helper", "--nocapture"]).env("TRS_DETACHED_HELPER", m);
            c
        };
        match mode.to_str().unwrap() {
            // Lives long, says who it is.
            "grandchild" => {
                std::fs::write(std::env::var_os("TRS_DETACHED_PID").unwrap(), std::process::id().to_string()).unwrap();
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
            "print" => writeln!(std::io::stdout(), "printed through an inherited stdout").unwrap(),
            // What `seedwell sim add` does: a detached child with its own streams, then it ends.
            _ => {
                let mut g = detached_command(&me);
                g.args(["--exact", "proc::tests::detached_helper"]).env("TRS_DETACHED_HELPER", "grandchild");
                g.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                #[allow(clippy::zombie_processes, reason = "detached on purpose; the test ends it by its pid")]
                g.spawn().unwrap();
                // A later child that asks for this process's stdout still gets it.
                let st = helper("print").stdout(std::process::Stdio::inherit()).status().unwrap();
                assert!(st.success());
            }
        }
    }

    /// A long-lived detached child does not keep the pipes of whoever reads our output open (Windows
    /// handed them down: a test's `output()` of `seedwell sim add` waited for the sim host to end).
    #[test]
    fn a_detached_child_does_not_hold_the_callers_pipes() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let t = std::time::Instant::now();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proc::tests::detached_helper", "--nocapture"])
            .env("TRS_DETACHED_HELPER", "middle")
            .env("TRS_DETACHED_PID", &pid_file)
            .output()
            .unwrap();
        let took = t.elapsed();
        // The grandchild is still there; end it.
        let t0 = std::time::Instant::now();
        while !pid_file.exists() && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        #[cfg(unix)]
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill").args(["/F", "/PID", pid.trim()]).output();
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("printed through an inherited stdout"), "{out:?}");
        assert!(took < std::time::Duration::from_secs(15), "output() waited {took:?} for the detached child");
    }
}
