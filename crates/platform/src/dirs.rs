//! Where things are on this system.

use std::path::{Path, PathBuf};

/// The user's home folder: `$HOME`, else on Windows `%USERPROFILE%`. `None` when unset. Windows
/// takes `HOME` first, as Git and most developer tools do, so pointing `HOME` elsewhere (as tests
/// do) moves `~` on every system.
pub fn home() -> Option<PathBuf> {
    let v = std::env::var_os("HOME").filter(|v| !v.is_empty());
    #[cfg(windows)]
    let v = v.or_else(|| std::env::var_os("USERPROFILE"));
    v.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `path` as user-visible text writes it: inside the home folder as `~/…` (`~` for the home folder itself) on Unix
/// systems, as the Mac's Finder texts and shells do; the full path on Windows, where `~` means nothing.
pub fn shown(path: &Path) -> String {
    #[cfg(unix)]
    if let Some(home) = home().filter(|h| h.is_absolute() && h.parent().is_some())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
    }
    path.display().to_string()
}

/// Folders where a downloaded file could act as code or configuration: the system's own, and the
/// user's settings folder (`~/Library`; `AppData` on Windows, whose Startup folder runs programs).
pub fn sensitive_dirs() -> Vec<PathBuf> {
    #[cfg(unix)]
    let (mut v, settings): (Vec<PathBuf>, _) = (
        ["/System", "/Library", "/usr", "/bin", "/sbin", "/etc", "/private/etc", "/Applications"].map(PathBuf::from).into(),
        "Library",
    );
    #[cfg(windows)]
    let (mut v, settings): (Vec<PathBuf>, _) = (
        ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData"]
            .iter()
            .filter_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
            .collect(),
        "AppData",
    );
    v.extend(home().map(|h| h.join(settings)));
    v
}

/// Seedwell's own state folder for this user: `~/Library/Application Support/Seedwell` on macOS,
/// `$XDG_STATE_HOME/seedwell` (else `~/.local/state/seedwell`) on Linux and other Unix systems,
/// `%LOCALAPPDATA%\Seedwell` on Windows. `None` when the system says no home.
pub fn state_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("Seedwell"));
    }
    if cfg!(target_os = "macos") {
        return home().map(|h| h.join("Library/Application Support/Seedwell"));
    }
    let base = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()).map(PathBuf::from);
    base.or_else(|| home().map(|h| h.join(".local/state"))).map(|b| b.join("seedwell"))
}

/// Folders below which other volumes appear when they are mounted, so a path below a root whose
/// volume is missing can be told apart from a folder not yet made: `/Volumes` on macOS, `/media`,
/// `/run/media` and `/mnt` on Linux. Windows names volumes by drive letter instead: none.
pub fn mount_roots() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/Volumes"]
    } else if cfg!(target_os = "linux") {
        &["/run/media", "/media", "/mnt"]
    } else {
        &[]
    }
}

/// Whether `path` is inside a temporary folder of this system: the process's (`std::env::temp_dir`), `/tmp` on Unix,
/// and the per-user ones under `/var/folders` on macOS. Both sides are also compared as real paths (`/private/tmp`;
/// on Windows the long form, so a TEMP written with 8.3 short names, `C:\Users\RUNNER~1\...`, still matches the same
/// folder written out). For QA switches that must never point at a user's own folders. `path` must be absolute and
/// have no `..` in it.
pub fn in_temp_folder(path: &Path) -> bool {
    let mut roots = vec![std::env::temp_dir()];
    if cfg!(unix) {
        roots.push(PathBuf::from("/tmp"));
    }
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/var/folders"));
    }
    inside_any(path, &roots)
}

fn inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return false;
    }
    let mut roots = roots.to_vec();
    let real: Vec<PathBuf> = roots.iter().filter_map(|r| r.canonicalize().ok()).collect();
    roots.extend(real);
    let inside = |p: &Path| roots.iter().any(|r| r.parent().is_some() && p.starts_with(r));
    inside(path) || real_path(path).is_some_and(|p| inside(&p))
}

/// `path` as a real path: its deepest existing folder resolved (links, `/private`, Windows' long names and `\\?\`
/// form, as `canonicalize` gives the roots), the rest joined as it is.
fn real_path(path: &Path) -> Option<PathBuf> {
    let base = path.ancestors().find(|a| a.exists())?;
    let rest = path.strip_prefix(base).ok()?;
    Some(base.canonicalize().ok()?.join(rest))
}

/// True when `path` (or the folder it would be made in) is inside a folder another sync engine
/// manages: iCloud Drive and the File Provider folders on macOS (`~/Library/CloudStorage`: OneDrive,
/// Dropbox, Google Drive), any cloud sync root on Windows (OneDrive, iCloud, Dropbox: the Cloud
/// Files API knows them all). Their files may be placeholders, and two engines on one folder fight.
pub fn in_cloud_storage(path: &Path) -> bool {
    #[cfg(unix)]
    {
        let s = path.to_string_lossy();
        s.contains("/Library/Mobile Documents") || s.contains("/Library/CloudStorage")
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::CloudFilters::{CF_SYNC_ROOT_BASIC_INFO, CF_SYNC_ROOT_INFO_BASIC, CfGetSyncRootInfoByPath};
        let Some(p) = path.ancestors().find(|a| a.exists()) else { return false };
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut info = CF_SYNC_ROOT_BASIC_INFO { SyncRootFileId: 0 };
        let mut len = 0u32;
        // SAFETY: `wide` is NUL-terminated and `info` is a live buffer of the size given; both outlive the call.
        let hr = unsafe {
            CfGetSyncRootInfoByPath(wide.as_ptr(), CF_SYNC_ROOT_INFO_BASIC, (&raw mut info).cast(), size_of::<CF_SYNC_ROOT_BASIC_INFO>() as u32, &mut len)
        };
        // S_OK inside a sync root; ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT (or no Cloud Files on this system) outside.
        hr >= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub's Windows runner has TEMP as `C:\Users\RUNNER~1\AppData\Local\Temp` while the same folder written out
    /// is `C:\Users\runneradmin\...`: an isolated run in its temp folder was refused as inside the user's profile.
    #[cfg(windows)]
    #[test]
    fn a_folder_written_out_is_inside_its_root_written_with_short_names() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("a-long-folder-name");
        std::fs::create_dir(&long).unwrap();
        let wide: Vec<u16> = long.as_os_str().encode_wide().chain([0]).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated and `buf` has room for 1024 units, which is its length passed.
        let n = unsafe { windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), 1024) };
        assert!(n > 0 && (n as usize) < buf.len());
        let short = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]));
        if short == long {
            return; // 8.3 names are off on this volume: nothing to tell apart
        }
        assert!(inside_any(&long.join("state"), &[short.clone()]), "{} in {}", long.display(), short.display());
        assert!(inside_any(&short.join("state"), &[long.clone()]));
        assert!(!inside_any(&dir.path().join("elsewhere"), &[short]));
    }

    #[test]
    fn temporary_folders_are_told_apart_from_the_users_own() {
        assert!(in_temp_folder(&std::env::temp_dir().join("seedwell-qa/bin")));
        // The same folder written as its real path, and through a resolved link, is inside too.
        let real = std::env::temp_dir().canonicalize().unwrap();
        assert!(in_temp_folder(&real.join("seedwell-qa/bin")));
        assert!(!in_temp_folder(&std::env::temp_dir().join("../escape")), "no way out with ..");
        assert!(!in_temp_folder(Path::new("bin")), "relative");
        assert!(!in_temp_folder(Path::new(if cfg!(windows) { r"C:\Users\me\Downloads" } else { "/home/me/Downloads" })));
        #[cfg(unix)]
        assert!(in_temp_folder(Path::new("/tmp/seedwell-qa/bin")));
    }

    #[test]
    fn a_path_in_the_home_folder_is_shown_with_a_tilde_where_the_system_writes_one() {
        let outside = std::env::temp_dir().join("seedwell-shown");
        let Some(home) = home().filter(|h| h.is_absolute() && h.parent().is_some() && !outside.starts_with(h)) else { return };
        let inside = home.join("Seedwell").join("Photos");
        let sibling = home.with_file_name(format!("{}x", home.file_name().unwrap().to_string_lossy()));
        assert_eq!(shown(&outside), outside.display().to_string());
        assert_eq!(shown(&sibling), sibling.display().to_string(), "only inside it");
        if cfg!(unix) {
            assert_eq!(shown(&inside), "~/Seedwell/Photos");
            assert_eq!(shown(&home), "~");
        } else {
            assert_eq!(shown(&inside), inside.display().to_string());
        }
    }

    #[test]
    fn a_temporary_folder_is_not_cloud_storage_but_a_provider_folder_is() {
        let t = std::env::temp_dir().join("seedwell-not-made-yet/x");
        assert!(!in_cloud_storage(&t), "{}", t.display());
        #[cfg(unix)]
        {
            assert!(in_cloud_storage(Path::new("/Users/a/Library/CloudStorage/OneDrive-Personal/Docs")));
            assert!(in_cloud_storage(Path::new("/Users/a/Library/Mobile Documents/com~apple~CloudDocs/x")));
        }
        // On Windows, OneDrive's own folder when this PC has it (%OneDrive%).
        #[cfg(windows)]
        if let Some(od) = std::env::var_os("OneDrive").map(PathBuf::from).filter(|p| p.exists()) {
            assert!(in_cloud_storage(&od.join("seedwell-not-made-yet")), "{}", od.display());
        }
    }
}
