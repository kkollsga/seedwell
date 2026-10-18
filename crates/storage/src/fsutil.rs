//! Small OS helpers: rlimit, core counts, preallocation, volume guards. All `unsafe` lives here
//! and in `disk.rs`'s thread-priority call.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// macOS refuses `setrlimit(NOFILE)` above `OPEN_MAX` regardless of the hard limit.
pub const NOFILE_SAFE_CAP: u64 = 10_240;

/// Raises the soft `RLIMIT_NOFILE` towards `target` (capped by the hard limit, the kernel's
/// per-process maximum and [`NOFILE_SAFE_CAP`]). Returns the resulting soft limit.
///
/// Call once at startup: the macOS default soft limit of 256 is below the fd-cache default.
pub fn raise_nofile_limit(target: u64) -> io::Result<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
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

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
    let mut buf = [0u8; 8];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `name` is NUL-terminated; `buf`/`len` describe a valid writable buffer; no new value
    // is passed (null, 0).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    match len {
        4 => Some(u64::from(u32::from_ne_bytes([
            buf[0], buf[1], buf[2], buf[3],
        ]))),
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

/// Reserves `len` bytes for a freshly created (empty) file, then sets its length.
///
/// Uses `fcntl(F_PREALLOCATE)` (contiguous first, then any) on macOS and `posix_fallocate` on Linux
/// (a file system that cannot allocate ahead, such as some network or overlay mounts, gets a sparse
/// file instead); elsewhere only `set_len`.
pub fn preallocate(file: &File, len: u64) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if len > 0 && len <= i64::MAX as u64 {
            let mut st = libc::fstore_t {
                fst_flags: libc::F_ALLOCATECONTIG,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: len as libc::off_t,
                fst_bytesalloc: 0,
            };
            let fd = file.as_raw_fd();
            // SAFETY: `fd` is a valid open descriptor owned by `file`; `st` is a fully initialised
            // fstore_t that outlives the call.
            let mut r = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut st) };
            if r == -1 {
                st.fst_flags = libc::F_ALLOCATEALL;
                // SAFETY: as above.
                r = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut st) };
            }
            if r == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        if len > 0 && len <= i64::MAX as u64 {
            // SAFETY: `file` owns a valid open descriptor; the call takes no pointers.
            let r = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
            // The call returns the error number itself. "Not supported" is not an error here: the
            // setting asked for a reservation where the file system can give one.
            match r {
                0 => {}
                libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS => {}
                e => return Err(io::Error::from_raw_os_error(e)),
            }
        }
    }
    file.set_len(len)
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
    if let Some(mp) = volume_mount_point(dir) {
        if !mp.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "volume not mounted",
            ));
        }
    }
    std::fs::create_dir_all(dir)
}

/// Device id (`st_dev`) of `path`, or of its nearest existing ancestor when it does not exist yet.
///
/// This is the "same volume?" key of SPEC §7A.4: a folder that has not been created yet lives on
/// the volume of the closest folder that exists. Symlinks are followed.
pub fn dev_of_nearest_existing(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for a in path.ancestors() {
        let target = if a.as_os_str().is_empty() {
            Path::new(".")
        } else {
            a
        };
        match std::fs::metadata(target) {
            Ok(m) => return Ok(m.dev()),
            Err(e) if crate::error::is_not_found(&e) => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Whether two paths live on the same volume (device id of the nearest existing ancestor).
pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(dev_of_nearest_existing(a)? == dev_of_nearest_existing(b)?)
}

/// Bytes available to an unprivileged writer on the volume holding `path` (nearest existing ancestor).
pub fn available_space(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for a in path.ancestors() {
        let target = if a.as_os_str().is_empty() {
            Path::new(".")
        } else {
            a
        };
        let c = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `st` is plain-old-data that statvfs fully initialises on success; `c` is a valid
        // NUL-terminated string that outlives the call.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: as above; the pointer is to a live, writable statvfs.
        let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
        if r == 0 {
            #[allow(clippy::unnecessary_cast)]
            return Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64));
        }
        let e = io::Error::last_os_error();
        if crate::error::is_not_found(&e) {
            last = e;
        } else {
            return Err(e);
        }
    }
    Err(last)
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing `to`.
///
/// macOS: `renamex_np(RENAME_EXCL)`, atomic. Filesystems without support (some exFAT/FAT/SMB
/// volumes answer `ENOTSUP`/`EINVAL`) fall back to "check, then rename", which is racy but is the
/// best those volumes offer. A cross-volume attempt fails with `EXDEV` so callers can copy instead.
pub fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let cf = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let ct = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    #[cfg(target_os = "macos")]
    {
        // SAFETY: both pointers are valid NUL-terminated strings that outlive the call;
        // RENAME_EXCL is a documented flag of renamex_np.
        let r = unsafe { libc::renamex_np(cf.as_ptr(), ct.as_ptr(), libc::RENAME_EXCL) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EINVAL) => rename_checked(from, to),
            _ => Err(e),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (&cf, &ct);
        // link(2) fails with EEXIST instead of replacing; then drop the old name.
        match std::fs::hard_link(from, to) {
            Ok(()) => std::fs::remove_file(from),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => rename_checked(from, to),
            Err(e) => Err(e),
        }
    }
}

fn rename_checked(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(to) {
        Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(e) if crate::error::is_not_found(&e) => std::fs::rename(from, to),
        Err(e) => Err(e),
    }
}

/// Turns the page cache off for `file` (`F_NOCACHE`): bulk copies must not evict the working set
/// that seeding needs. Best effort; a no-op off macOS.
pub fn set_nocache(file: &File) {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` owns a valid descriptor; F_NOCACHE takes an int argument and no pointers.
        unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = file;
}

/// Copy a file's metadata (extended attributes: Finder tags, comments, quarantine and so on; ACLs;
/// owner and mode where permitted) from `src` to `dst`, both open. Best effort in the parts the
/// caller may not own (a different owner cannot be set by an ordinary user), strict about the
/// rest: an error is returned only when nothing could be copied at all.
///
/// macOS: `fcopyfile(COPYFILE_METADATA)`, falling back to extended attributes and ACLs only when
/// the security part is refused. Linux: every extended attribute (POSIX ACLs are the
/// `system.posix_acl_*` ones).
pub fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    {
        let (s, d) = (src.as_raw_fd(), dst.as_raw_fd());
        // SAFETY: both descriptors are open for the call; a null state is allowed.
        let rc = unsafe { libc::fcopyfile(s, d, std::ptr::null_mut(), libc::COPYFILE_METADATA) };
        if rc == 0 {
            return Ok(());
        }
        let first = io::Error::last_os_error();
        // SAFETY: as above.
        let rc = unsafe { libc::fcopyfile(s, d, std::ptr::null_mut(), libc::COPYFILE_XATTR | libc::COPYFILE_ACL) };
        if rc == 0 {
            return Ok(());
        }
        // SAFETY: as above.
        let rc = unsafe { libc::fcopyfile(s, d, std::ptr::null_mut(), libc::COPYFILE_XATTR) };
        if rc == 0 { Ok(()) } else { Err(first) }
    }
    #[cfg(target_os = "linux")]
    {
        let (s, d) = (src.as_raw_fd(), dst.as_raw_fd());
        // SAFETY: valid descriptor; a null buffer with size 0 asks for the needed length.
        let need = unsafe { libc::flistxattr(s, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return if need < 0 { Err(io::Error::last_os_error()) } else { Ok(()) };
        }
        let mut names = vec![0u8; need as usize];
        // SAFETY: `names` has `need` writable bytes.
        let got = unsafe { libc::flistxattr(s, names.as_mut_ptr().cast(), names.len()) };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        names.truncate(got as usize);
        for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
            let mut cname = name.to_vec();
            cname.push(0);
            // SAFETY: `cname` is NUL-terminated.
            let len = unsafe { libc::fgetxattr(s, cname.as_ptr().cast(), std::ptr::null_mut(), 0) };
            if len < 0 {
                continue;
            }
            let mut val = vec![0u8; len as usize];
            // SAFETY: `val` has `len` writable bytes.
            let n = unsafe { libc::fgetxattr(s, cname.as_ptr().cast(), val.as_mut_ptr().cast(), val.len()) };
            if n < 0 {
                continue;
            }
            // SAFETY: `val` has `n` readable bytes.
            unsafe { libc::fsetxattr(d, cname.as_ptr().cast(), val.as_ptr().cast(), n as usize, 0) };
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (src, dst);
        Ok(())
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

/// Finder's own bookkeeping: `.DS_Store` (view settings) and AppleDouble `._name` files. A folder
/// that holds nothing else is empty to the user.
pub(crate) fn is_finder_litter(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let b = name.as_bytes();
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
