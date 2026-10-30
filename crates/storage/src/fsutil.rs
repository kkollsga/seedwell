//! Small file-system helpers: volume guards, free space, renames that never replace. The system
//! calls behind them live in `trs-platform`.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// macOS refuses `setrlimit(NOFILE)` above `OPEN_MAX` regardless of the hard limit.
pub const NOFILE_SAFE_CAP: u64 = trs_platform::proc::NOFILE_SAFE_CAP;

/// Raises the limit of open files towards `target`; returns the resulting limit. Call once at
/// startup: the macOS default soft limit of 256 is below the fd-cache default.
pub fn raise_nofile_limit(target: u64) -> io::Result<u64> {
    trs_platform::proc::raise_nofile_limit(target)
}

/// Number of performance cores, else logical CPUs: hashing is pinned to this many threads so it
/// never competes with efficiency cores' power budget.
pub fn perf_core_count() -> usize {
    trs_platform::proc::perf_core_count()
}

/// Reserves `len` bytes for a freshly created (empty) file, then sets its length
/// ([`trs_platform::fs::preallocate`]).
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    trs_platform::fs::preallocate(file, len)
}

/// Lowers the calling thread's disk I/O priority (scrub/recheck must not hurt foreground I/O).
pub fn set_background_io() {
    trs_platform::proc::set_background_io()
}

/// For `/Volumes/<name>/...` returns `/Volumes/<name>`: the mount point that must exist before
/// we create any directory below it. Without this guard `create_dir_all` could silently build a
/// fake `/Volumes/Ext/...` tree on the boot volume when the external drive is unplugged.
pub fn volume_mount_point(path: &Path) -> Option<PathBuf> {
    let mut it = path.components();
    if it.next() != Some(Component::RootDir) {
        return None;
    }
    match (it.next(), it.next()) {
        (Some(Component::Normal(a)), Some(Component::Normal(b))) if a == "Volumes" => {
            Some(Path::new("/Volumes").join(b))
        }
        _ => None,
    }
}

/// Creates `dir` and missing parents, but never the volume mount point itself.
///
/// Returns [`io::ErrorKind::NotFound`] if the volume is not mounted.
pub fn create_dirs_guarded(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(mp) = volume_mount_point(dir)
        && !mp.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "volume not mounted",
            ));
        }
    std::fs::create_dir_all(dir)
}

/// Volume id of `path`, or of its nearest existing ancestor when it does not exist yet.
///
/// This is the "same volume?" key of SPEC §7A.4: a folder that has not been created yet lives on
/// the volume of the closest folder that exists. Symlinks are followed.
pub fn dev_of_nearest_existing(path: &Path) -> io::Result<u64> {
    nearest_existing(path, trs_platform::fs::volume_id)
}

/// Whether two paths live on the same volume (device id of the nearest existing ancestor).
pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(dev_of_nearest_existing(a)? == dev_of_nearest_existing(b)?)
}

/// Bytes available to an unprivileged writer on the volume holding `path` (nearest existing ancestor).
pub fn available_space(path: &Path) -> io::Result<u64> {
    nearest_existing(path, trs_platform::fs::available_space)
}

/// `f` of `path`'s nearest ancestor that exists (`path` itself first).
fn nearest_existing<T>(path: &Path, f: impl Fn(&Path) -> io::Result<T>) -> io::Result<T> {
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for a in path.ancestors() {
        let target = if a.as_os_str().is_empty() { Path::new(".") } else { a };
        match f(target) {
            Ok(v) => return Ok(v),
            Err(e) if crate::error::is_not_found(&e) => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing `to`
/// ([`trs_platform::fs::rename_no_replace`]). A cross-volume attempt fails so callers can copy instead.
pub fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    trs_platform::fs::rename_no_replace(from, to)
}

/// Turns the page cache off for `file`: bulk copies must not evict the working set that seeding
/// needs. Best effort.
pub fn set_nocache(file: &File) {
    trs_platform::fs::set_nocache(file)
}
/// Copy a file's metadata from `src` to `dst`, both open ([`trs_platform::fs::copy_metadata`]).
pub fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    trs_platform::fs::copy_metadata(src, dst)
}

/// Ask the allocator to give freed memory back to the OS and return roughly how many bytes it
/// Ask the allocator to give freed memory back to the OS ([`trs_platform::proc::release_free_memory`]).
pub fn release_free_memory() -> usize {
    trs_platform::proc::release_free_memory()
}

/// Finder's own bookkeeping: `.DS_Store` (view settings) and AppleDouble `._name` files. A folder
/// that holds nothing else is empty to the user.
pub(crate) fn is_finder_litter(name: &std::ffi::OsStr) -> bool {
    let b = name.as_encoded_bytes();
    b == b".DS_Store" || (b.len() > 2 && b.starts_with(b"._"))
}

/// Removes `dir` if it is empty or holds only Finder litter (`.DS_Store`, `._*` files), which goes
/// with it. Anything else (a file, a folder, a custom folder icon) keeps the folder and is never
/// touched. `true` when the folder is gone.
pub fn remove_dir_if_empty(dir: &Path) -> bool {
    use std::fs;
    if fs::remove_dir(dir).is_ok() {
        return true;
    }
    let Ok(rd) = fs::read_dir(dir) else { return false };
    let mut litter = Vec::new();
    for e in rd {
        let Ok(e) = e else { return false };
        if !e.file_type().is_ok_and(|t| t.is_file()) || !is_finder_litter(&e.file_name()) {
            return false;
        }
        litter.push(e.path());
    }
    for p in &litter {
        let _ = fs::remove_file(p);
    }
    fs::remove_dir(dir).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_guard() {
        assert_eq!(
            volume_mount_point(Path::new("/Volumes/Ext/a/b")),
            Some(PathBuf::from("/Volumes/Ext"))
        );
        assert_eq!(volume_mount_point(Path::new("/Users/x")), None);
        assert_eq!(volume_mount_point(Path::new("/Volumes")), None);
        let e = create_dirs_guarded(Path::new("/Volumes/__trs_no_such_volume__/a/b")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!Path::new("/Volumes/__trs_no_such_volume__").exists());
    }

    #[test]
    fn rename_excl_never_replaces() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();
        let e = rename_excl(&a, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&a).unwrap(), b"A");
        assert_eq!(std::fs::read(&b).unwrap(), b"B");
        rename_excl(&a, &c).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&c).unwrap(), b"A");
    }

    #[test]
    fn volume_helpers() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("x/y/z");
        assert!(same_volume(d.path(), &deep).unwrap());
        assert_eq!(
            dev_of_nearest_existing(&deep).unwrap(),
            dev_of_nearest_existing(d.path()).unwrap()
        );
        assert!(available_space(&deep).unwrap() > 0);
    }

    #[test]
    fn nofile_and_cores() {
        let n = raise_nofile_limit(4096).unwrap();
        assert!(n >= 256);
        assert!(perf_core_count() >= 1);
    }

    #[test]
    fn preallocate_sets_length() {
        let dir = tempfile::tempdir().unwrap();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("p"))
            .unwrap();
        preallocate(&f, 3 << 20).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 3 << 20);
    }

    #[test]
    fn a_folder_with_only_finder_litter_is_empty() {
        use std::fs;
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("Harbor Watch");
        fs::create_dir(&d).unwrap();
        fs::write(d.join(".DS_Store"), b"x").unwrap();
        fs::write(d.join("._E01.mkv"), b"x").unwrap();
        assert!(remove_dir_if_empty(&d));
        assert!(!d.exists());

        fs::create_dir(&d).unwrap();
        fs::write(d.join(".DS_Store"), b"x").unwrap();
        fs::write(d.join("E05.mkv"), b"x").unwrap();
        assert!(!remove_dir_if_empty(&d));
        assert!(d.join(".DS_Store").exists() && d.join("E05.mkv").exists(), "nothing is touched");

        let e = t.path().join("nested");
        fs::create_dir_all(e.join("sub")).unwrap();
        fs::write(e.join(".DS_Store"), b"x").unwrap();
        assert!(!remove_dir_if_empty(&e), "a folder inside keeps it");
        assert!(e.join(".DS_Store").exists());
    }
}
