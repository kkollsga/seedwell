//! Windows' players, found in the registry: App Paths (`HKCU`, then `HKLM` in its 64- and 32-bit views), the
//! registered applications (`HKCR\Applications\<exe>`), the handlers registered for video files (`HKCR\.mkv`'s
//! `OpenWithProgids` and default ProgID), then the folders of `PATH` (mpv is often unpacked there). Every path found is
//! checked to exist. The string work is `registry.rs`, tested on every system.

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RRF_SUBKEY_WOW6432KEY, RegCloseKey, RegEnumValueW, RegGetValueW, RegOpenKeyExW,
};

use super::registry::{command_executable, expand_env, known_by_executable, plain_path};
use super::{KnownPlayer, Player, WINDOWS_PLAYERS};

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

/// The known players installed here, in the order of "Automatic". Reads registry keys: call it off the UI thread
/// now and then, not for every menu.
pub fn installed() -> Vec<Player> {
    let handlers = video_handlers();
    // Windows runs x64 apps on Arm too: what is installed runs.
    WINDOWS_PLAYERS.iter().filter_map(|spec| locate(spec, &handlers).map(|p| Player::found(spec, p, None, true))).collect()
}

/// The known player at `app` (an executable), whether or not the registry lists it: what QA's `play-app` starts.
pub fn player_at(app: &Path) -> Option<Player> {
    let spec = known_by_executable(WINDOWS_PLAYERS, &app.to_string_lossy())?;
    app.is_file().then(|| Player::found(spec, app.to_path_buf(), None, true))
}
