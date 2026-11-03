//! Windows' players, found in the registry: App Paths (`HKCU`, then `HKLM` in its 64- and 32-bit views), the
//! registered applications (`HKCR\Applications\<exe>`), the handlers registered for video files (`HKCR\.mkv`'s
//! `OpenWithProgids` and default ProgID), then the folders of `PATH` (mpv is often unpacked there). Every path found is
//! checked to exist. The string work is `registry.rs`, tested on every system. Whether each runs here: the CPU in its
//! executable's PE header (`pe.rs`) against the PC's own CPU ([`super::can_run_on_windows`]).

use std::ffi::{c_void, OsStr};
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RRF_SUBKEY_WOW6432KEY, RegCloseKey, RegEnumValueW, RegGetValueW, RegOpenKeyExW,
};
use windows_sys::Win32::System::SystemInformation::{IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2, MACHINE_ATTRIBUTES, UserEnabled};

use super::registry::{command_executable, expand_env, known_by_executable, plain_path};
use super::{can_run_on_windows, pe_architecture, Arch, KnownPlayer, Player, WINDOWS_PLAYERS};

const APP_PATHS: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths";
/// The video types whose handlers are looked at.
const VIDEO_TYPES: [&str; 4] = [".mkv", ".mp4", ".avi", ".webm"];

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A string value (`value` `None`: the key's default), as stored: a `REG_EXPAND_SZ` is expanded here, not by the call.
/// `flags` may add `RRF_SUBKEY_WOW6432KEY`.
fn reg_string(root: HKEY, key: &str, value: Option<&str>, flags: u32) -> Option<String> {
    let k = wide(key);
    let v = value.map(wide);
    let v_ptr = v.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
    let flags = flags | RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut size: u32 = 0;
    // SAFETY: the strings are NUL-terminated and outlive the call; a NULL buffer asks for the size only.
    let r = unsafe { RegGetValueW(root, k.as_ptr(), v_ptr, flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
    if r != ERROR_SUCCESS || size == 0 || size > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `bytes` bytes; the call writes at most that many and says how many it wrote.
    let r = unsafe { RegGetValueW(root, k.as_ptr(), v_ptr, flags, std::ptr::null_mut(), buf.as_mut_ptr().cast::<c_void>(), &mut bytes) };
    if r != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]);
    (!s.trim().is_empty()).then_some(s)
}

/// The names of a key's values (`OpenWithProgids` lists ProgIDs as value names).
fn value_names(root: HKEY, key: &str) -> Vec<String> {
    let k = wide(key);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: `k` is NUL-terminated; `h` receives the opened key, closed below.
    if unsafe { RegOpenKeyExW(root, k.as_ptr(), 0, KEY_READ, &mut h) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..256u32 {
        let mut name = [0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` characters; the data is not asked for.
        let r = unsafe {
            RegEnumValueW(h, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        if r != ERROR_SUCCESS {
            break;
        }
        let n = String::from_utf16_lossy(&name[..(len as usize).min(name.len())]);
        if !n.is_empty() {
            out.push(n);
        }
    }
    // SAFETY: `h` was opened above and is closed once.
    unsafe { RegCloseKey(h) };
    out
}

/// An existing file at the path a registry value names.
fn existing(value: &str) -> Option<PathBuf> {
    let p = PathBuf::from(plain_path(value, env));
    p.is_file().then_some(p)
}

/// App Paths of an executable name: the current user's, then the machine's (64-bit view, then 32-bit).
fn app_paths(exe: &str) -> Option<PathBuf> {
    let key = format!(r"{APP_PATHS}\{exe}");
    [(HKEY_CURRENT_USER, 0), (HKEY_LOCAL_MACHINE, 0), (HKEY_LOCAL_MACHINE, RRF_SUBKEY_WOW6432KEY)]
        .into_iter()
        .find_map(|(root, flags)| reg_string(root, &key, None, flags).and_then(|v| existing(&v)))
}

/// The open command of a registered class or application key (`HKCR\<key>\shell\open\command`).
fn open_command(key: &str) -> Option<PathBuf> {
    let cmd = reg_string(HKEY_CLASSES_ROOT, &format!(r"{key}\shell\open\command"), None, 0)?;
    // Only the variables: the quotes say where the executable ends.
    existing(&command_executable(&expand_env(&cmd, env))?)
}

/// The executables registered to open video files, each once.
fn video_handlers() -> Vec<PathBuf> {
    let mut progids: Vec<String> = Vec::new();
    for ext in VIDEO_TYPES {
        if let Some(default) = reg_string(HKEY_CLASSES_ROOT, ext, None, 0) {
            progids.push(default);
        }
        progids.extend(value_names(HKEY_CLASSES_ROOT, &format!(r"{ext}\OpenWithProgids")));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in progids {
        if let Some(exe) = open_command(&p).filter(|e| !out.contains(e)) {
            out.push(exe);
        }
    }
    out
}

/// An executable of `spec` in a folder of `PATH`.
fn on_path(spec: &KnownPlayer) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).flat_map(|dir| spec.files.iter().map(move |f| dir.join(f))).find(|p| p.is_file())
}

fn locate(spec: &KnownPlayer, handlers: &[PathBuf]) -> Option<PathBuf> {
    spec.files
        .iter()
        .find_map(|exe| app_paths(exe))
        .or_else(|| spec.files.iter().find_map(|exe| open_command(&format!(r"Applications\{exe}"))))
        .or_else(|| handlers.iter().find(|h| known_by_executable(WINDOWS_PLAYERS, &h.to_string_lossy()).is_some_and(|k| k.id == spec.id)).cloned())
        .or_else(|| on_path(spec))
}

/// The PC's own CPU and whether it runs x64 apps by emulation.
#[derive(Clone, Copy)]
struct Host {
    native: Arch,
    x64_emulation: bool,
}

fn arch_of(machine: IMAGE_FILE_MACHINE) -> Arch {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => Arch::X86_64,
        IMAGE_FILE_MACHINE_ARM64 => Arch::Arm64,
        IMAGE_FILE_MACHINE_I386 => Arch::X86,
        _ => Arch::Other,
    }
}

/// The machine's CPU, not this process's: an x64 Seedwell on an Arm PC runs emulated, and `consts::ARCH` would say
/// x86_64. `IsWow64Process2` (Windows 10 1709 and later) names the native machine also from an emulated process.
fn native() -> Arch {
    let (mut process, mut native): (IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE) = (0, 0);
    // SAFETY: the pseudo-handle of this process needs no closing; both out-pointers are valid for the call.
    let ok = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) } != 0;
    let a = if ok { arch_of(native) } else { Arch::Other };
    if a == Arch::Other {
        match std::env::consts::ARCH {
            "aarch64" => Arch::Arm64,
            "x86_64" => Arch::X86_64,
            "x86" => Arch::X86,
            _ => Arch::Other,
        }
    } else {
        a
    }
}

/// Whether user-mode x64 code runs here, natively or emulated: `GetMachineTypeAttributes`, looked up at run time since
/// only Windows 11 has it (a static import would keep Seedwell from starting on Windows 10). Without it, only an x64
/// PC runs x64 apps: Windows 10 on Arm emulates x86 alone.
fn x64_emulation(native: Arch) -> bool {
    type GetMachineTypeAttributes = unsafe extern "system" fn(u16, *mut MACHINE_ATTRIBUTES) -> i32;
    let kernel32 = wide("kernel32.dll");
    // SAFETY: kernel32 is loaded in every process; the name is NUL-terminated. The handle is not reference-counted.
    let module = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
    if module.is_null() {
        return native == Arch::X86_64;
    }
    // SAFETY: `module` is kernel32's; the name is a NUL-terminated ANSI string.
    let Some(f) = (unsafe { GetProcAddress(module, c"GetMachineTypeAttributes".as_ptr().cast()) }) else {
        return native == Arch::X86_64;
    };
    // SAFETY: the export has this signature (HRESULT GetMachineTypeAttributes(USHORT, MACHINE_ATTRIBUTES *)).
    let f: GetMachineTypeAttributes = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetMachineTypeAttributes>(f) };
    let mut attributes: MACHINE_ATTRIBUTES = 0;
    // SAFETY: `attributes` is valid for the write.
    let hr = unsafe { f(IMAGE_FILE_MACHINE_AMD64, &mut attributes) };
    if hr < 0 {
        return native == Arch::X86_64;
    }
    attributes & UserEnabled != 0
}

fn host() -> Host {
    let native = native();
    Host { native, x64_emulation: x64_emulation(native) }
}

/// What the PC runs, in a line for QA: "x86_64 Windows", "arm64 Windows, x64 emulation" or "arm64 Windows, no x64
/// emulation".
pub fn host_line() -> String {
    let h = host();
    match h.native {
        Arch::Arm64 => format!("arm64 Windows, {}", if h.x64_emulation { "x64 emulation" } else { "no x64 emulation" }),
        a => format!("{} Windows", a.name()),
    }
}

/// The CPU of an executable; `None` when it cannot be read or is not PE.
fn archs(executable: &Path) -> Option<Vec<Arch>> {
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(executable).ok()?.take(4096).read_to_end(&mut head).ok()?;
    pe_architecture(&head).map(|a| vec![a])
}

fn found(spec: &KnownPlayer, exe: PathBuf, host: Host) -> Player {
    let a = archs(&exe);
    let runs = can_run_on_windows(a.as_deref(), host.native, host.x64_emulation);
    Player::found(spec, exe, a, runs)
}

/// The known players installed here, in the order of "Automatic". Reads registry keys and a few file headers: call it
/// off the UI thread now and then, not for every menu.
pub fn installed() -> Vec<Player> {
    let handlers = video_handlers();
    let host = host();
    WINDOWS_PLAYERS.iter().filter_map(|spec| locate(spec, &handlers).map(|p| found(spec, p, host))).collect()
}

/// The known player at `app` (an executable), whether or not the registry lists it: what QA's `play-app` starts.
pub fn player_at(app: &Path) -> Option<Player> {
    let spec = known_by_executable(WINDOWS_PLAYERS, &app.to_string_lossy())?;
    app.is_file().then(|| found(spec, app.to_path_buf(), host()))
}
