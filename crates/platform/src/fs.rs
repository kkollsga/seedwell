//! File operations whose system calls differ.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

mod trash;

pub use trash::{OtherVolume, TrashError, TrashPlace, Trashed, move_to_trash, system_trash, trash_check};

/// Read exactly `buf.len()` bytes at `offset`, without using or moving a shared cursor on Unix.
///
/// On Windows the positioned read moves the file's cursor, so a file read this way must only ever
/// be read and written by offset (every caller in Seedwell does).
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Read up to `buf.len()` bytes at `offset`; 0 means the end of the file.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
            // Reading at or past the end is an error on Windows, 0 bytes on Unix.
            Err(e) if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_HANDLE_EOF as i32) => Ok(0),
            r => r,
        }
    }
}

/// Write all of `buf` at `offset` (see [`read_exact_at`] about the cursor on Windows).
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_write(file, buf, offset) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer")),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// An id of the volume (file system) `path` is on, equal for two paths exactly when a rename
/// between them can work. `path` must exist; [`Metadata`] is not enough on Windows.
pub fn volume_id(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(std::fs::metadata(path)?.dev())
    }
    #[cfg(windows)]
    {
        Ok(u64::from(file_info(path)?.dwVolumeSerialNumber))
    }
}

/// The volume id from metadata already read, where the system keeps it there (Unix); `None` on
/// Windows, where [`volume_id`] has to open the path.
pub fn volume_id_of(meta: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.dev())
    }
    #[cfg(windows)]
    {
        let _ = meta;
        None
    }
}

/// `GetFileInformationByHandle` of a file or folder (folders need backup semantics to open).
#[cfg(windows)]
fn file_info(path: &Path) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle};
    let f = std::fs::OpenOptions::new().access_mode(0).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of this plain C struct.
    let mut info = unsafe { std::mem::zeroed() };
    // SAFETY: `f` is an open handle for the duration of the call and `info` is writable.
    if unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
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

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing `to`.
///
/// macOS: `renamex_np(RENAME_EXCL)`, atomic. Filesystems without support (some exFAT/FAT/SMB
/// volumes answer `ENOTSUP`/`EINVAL`) fall back to "check, then rename", which is racy but is the
/// best those volumes offer. A cross-volume attempt fails with `EXDEV` so callers can copy instead.
/// Linux: `link` then `unlink`. Windows: `MoveFileExW` without the replace flag.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let cf = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let ct = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
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
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // link(2) fails with EEXIST instead of replacing; then drop the old name.
        match std::fs::hard_link(from, to) {
            Ok(()) => std::fs::remove_file(from),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => rename_checked(from, to),
            Err(e) => Err(e),
        }
    }
    #[cfg(windows)]
    {
        // MoveFileExW without MOVEFILE_REPLACE_EXISTING fails with ERROR_ALREADY_EXISTS instead of
        // replacing; without MOVEFILE_COPY_ALLOWED a move to another volume fails (ERROR_NOT_SAME_DEVICE)
        // so callers can copy instead.
        use std::os::windows::ffi::OsStrExt;
        let wide = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let (f, t) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
        if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn rename_checked(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(to) {
        Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(e) if is_not_found(&e) => std::fs::rename(from, to),
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
/// `system.posix_acl_*` ones). Windows: nothing yet (alternate data streams and ACLs stay behind).
pub fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
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
        use std::os::fd::AsRawFd;
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

/// Whether entries can be created in and removed from folder `dir`. On Unix `access(W_OK)`, which
/// also reports a locked folder (`uchg`) and a read-only volume; on Windows the read-only attribute
/// of the folder and of its volume are not reliable signs, so only a missing folder says no.
pub fn dir_writable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(windows)]
    {
        dir.is_dir()
    }
}

/// Bytes available to an unprivileged writer on the volume holding `path`, which must exist.
pub fn available_space(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `st` is plain-old-data that statvfs fully initialises on success; `c` is a valid
        // NUL-terminated string that outlives the call.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: as above; the pointer is to a live, writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: `w` is NUL-terminated; `free` is writable; the other two outputs may be null.
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(free)
    }
}

/// The Unix permission bits of a file, `None` where the system has none (Windows).
pub fn unix_mode(meta: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.mode())
    }
    #[cfg(windows)]
    {
        let _ = meta;
        None
    }
}

/// Create the file with these Unix permission bits (ignored where there are none).
pub fn set_create_mode(opts: &mut std::fs::OpenOptions, mode: u32) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode)
    }
    #[cfg(windows)]
    {
        let _ = mode;
        opts
    }
}

/// Whether a file carries a lock flag that stops it from being renamed or removed (Finder's
/// "Locked" is `uchg` on macOS). No such flags elsewhere: `false`.
pub fn is_locked(meta: &Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        const LOCKED: u32 = libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;
        meta.st_flags() & LOCKED != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = meta;
        false
    }
}

/// The words after a locked file's name in the message about it: macOS's lock flag is set in
/// Finder; on Windows a file is "locked" when another program has it open.
pub const LOCKED_WORDS: &str =
    if cfg!(windows) { "is open in another program" } else { "is locked (Finder \u{203a} Get Info \u{203a} Locked)" };

/// Whether `e` means another program holds the file so that it cannot be renamed, removed or opened
/// now: on Windows a sharing or lock violation (ERROR_SHARING_VIOLATION 32, ERROR_LOCK_VIOLATION 33);
/// never elsewhere (a file held open can be renamed there).
pub fn is_in_use(e: &io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33))
}

/// Whether another program holds the file at `path` so that it cannot be renamed or removed now
/// (Windows: open without letting others delete it). Opens the file once to ask, with delete access
/// and no data access; nothing is read or changed. Always `false` elsewhere.
pub fn held_by_another_program(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // DELETE access, and every share mode: the only question is whether another opener refuses deletes.
        const DELETE: u32 = 0x0001_0000;
        std::fs::OpenOptions::new().access_mode(DELETE).share_mode(7).open(path).is_err_and(|e| is_in_use(&e))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// A path's bytes as stored in Seedwell's own files: the bytes themselves on Unix, UTF-8 (WTF-8
/// for the rare unpaired surrogate) on Windows. [`path_from_bytes`] reads them back.
pub fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_encoded_bytes()
}

/// A path from bytes written by [`path_bytes`]. `None` for bytes that cannot be a path here (on
/// Windows: not UTF-8), so a damaged file is refused instead of read wrongly.
pub fn path_from_bytes(b: &[u8]) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(std::ffi::OsStr::from_bytes(b).into())
    }
    #[cfg(windows)]
    {
        std::str::from_utf8(b).ok().map(std::path::PathBuf::from)
    }
}

/// A time as Unix seconds and nanoseconds, the way `stat` gives a modification time (seconds may
/// be negative before 1970; nanoseconds are always 0..1e9).
pub fn unix_time(t: std::time::SystemTime) -> (i64, u32) {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            let (s, n) = (d.as_secs() as i64, d.subsec_nanos());
            if n == 0 { (-s, 0) } else { (-s - 1, 1_000_000_000 - n) }
        }
    }
}

/// What Folder Sync compares to tell an edited or moved file from an untouched one, beyond
/// [`Metadata`]: times in nanoseconds since 1970, the file's id on its volume, the owner's
/// executable bit and whether the data lives elsewhere (a cloud placeholder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// Last change of the content.
    pub mtime_ns: i64,
    /// Last change of the content or the metadata (`st_ctime`; NTFS's ChangeTime).
    pub ctime_ns: i64,
    /// Inode number; NTFS file index. Stays with the file through renames.
    pub file_id: u64,
    /// The owner's executable bit; always `false` where files have none (Windows).
    pub exec: bool,
    /// The data is not on this disk (iCloud / File Provider placeholder, OneDrive file on demand).
    pub dataless: bool,
}

/// The [`FileStamp`] of the file at `path`, whose `symlink_metadata` is `meta` (links are not
/// followed). On Unix everything comes from `meta`; Windows has to open the file for its id and
/// change time.
pub fn file_stamp(path: &Path, meta: &Metadata) -> io::Result<FileStamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
        #[cfg(target_os = "macos")]
        let dataless = {
            use std::os::darwin::fs::MetadataExt as _;
            /// `SF_DATALESS` in `st_flags`.
            const SF_DATALESS: u32 = 0x4000_0000;
            meta.st_flags() & SF_DATALESS != 0
        };
        #[cfg(not(target_os = "macos"))]
        let dataless = false;
        Ok(FileStamp {
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
            file_id: meta.ino(),
            exec: meta.mode() & 0o100 != 0,
            dataless,
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_BASIC_INFO,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FileBasicInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx,
        };
        let f = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let h = f.as_raw_handle();
        // SAFETY: plain C structs, all-zero is valid; `h` is open for both calls and the sizes match.
        let (id, basic) = unsafe {
            let mut id = std::mem::zeroed();
            if GetFileInformationByHandle(h, &mut id) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut basic: FILE_BASIC_INFO = std::mem::zeroed();
            if GetFileInformationByHandleEx(h, FileBasicInfo, (&raw mut basic).cast(), size_of::<FILE_BASIC_INFO>() as u32) == 0 {
                return Err(io::Error::last_os_error());
            }
            (id, basic)
        };
        let id: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION = id;
        // 100 ns ticks since 1601 to ns since 1970.
        let ns = |t: i64| (t - 116_444_736_000_000_000).saturating_mul(100);
        let attrs = meta.file_attributes();
        Ok(FileStamp {
            mtime_ns: ns(basic.LastWriteTime),
            ctime_ns: ns(basic.ChangeTime),
            file_id: (u64::from(id.nFileIndexHigh) << 32) | u64::from(id.nFileIndexLow),
            exec: false,
            dataless: attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0,
        })
    }
}

/// When the file at `path` (with metadata `meta`) last changed in content or metadata, a rename
/// into its folder included: `st_ctime`; NTFS's ChangeTime. `None` when the system cannot say.
pub fn change_time(path: &Path, meta: &Metadata) -> Option<std::time::SystemTime> {
    let ns = file_stamp(path, meta).ok()?.ctime_ns;
    let d = std::time::Duration::from_nanos(ns.unsigned_abs());
    if ns >= 0 { std::time::UNIX_EPOCH.checked_add(d) } else { std::time::UNIX_EPOCH.checked_sub(d) }
}

/// Set or clear the executable bits (owner, group, other alike). Where files have none (Windows)
/// nothing changes and it is not an error: the folder then reports that it does not keep them.
pub fn set_exec(path: &Path, exec: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = std::fs::symlink_metadata(path)?.mode() & 0o7777;
        let mode = if exec { mode | 0o111 } else { mode & !0o111 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = (path, exec);
        Ok(())
    }
}

/// Make a symbolic link at `link` pointing to `target`, written with `/` as on the wire (§3.3).
/// Windows needs to know whether the target is a folder (resolved from the link's folder) and the
/// right to make links (Developer Mode or an administrator); without it this fails with
/// [`io::ErrorKind::Unsupported`] and [`SYMLINK_UNSUPPORTED`], and the caller reports the path.
pub fn symlink(target: &str, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        // ERROR_PRIVILEGE_NOT_HELD
        const NO_PRIVILEGE: i32 = 1314;
        let native = target.replace('/', "\\");
        let resolved = link.parent().map_or_else(|| Path::new(&native).to_path_buf(), |d| d.join(&native));
        let made = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&native, link)
        } else {
            std::os::windows::fs::symlink_file(&native, link)
        };
        match made {
            Err(e) if e.raw_os_error() == Some(NO_PRIVILEGE) => Err(io::Error::new(io::ErrorKind::Unsupported, SYMLINK_UNSUPPORTED)),
            r => r,
        }
    }
}

/// Why [`symlink`] could not make a link: the system does not let this user make them.
pub const SYMLINK_UNSUPPORTED: &str =
    "symbolic links need Developer Mode on Windows (Settings > System > For developers); this link is not synced";

/// The target of the symbolic link at `link`, with `/` as on the wire; an error for a target that
/// is not UTF-8.
pub fn read_link(link: &Path) -> io::Result<String> {
    let t = std::fs::read_link(link)?.into_os_string().into_string().map_err(|raw| {
        io::Error::new(io::ErrorKind::InvalidData, format!("link target {raw:?} is not UTF-8"))
    })?;
    #[cfg(windows)]
    let t = t.replace('\\', "/");
    Ok(t)
}

/// Make the entries of folder `dir` durable (a new or renamed name survives a power cut). Unix
/// syncs the folder itself; NTFS journals its metadata, and Windows cannot flush a folder: nothing.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // `sync_all` is F_FULLFSYNC on macOS: a full flush.
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        // Nothing to flush, but a folder that is not there is the same error as on Unix.
        std::fs::metadata(dir).map(|_| ())
    }
}

/// Set the modification time of the file or folder at `path` (links are followed). Windows needs a
/// handle with write-attribute access for it, and backup semantics to open a folder.
pub fn set_modified(path: &Path, t: std::time::SystemTime) -> io::Result<()> {
    #[cfg(unix)]
    let f = File::open(path)?;
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES};
        std::fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?
    };
    f.set_modified(t)
}

/// Flush the file at `path` to the drive (`File::sync_all`). Windows flushes only a handle with
/// write access, so there the file is opened for writing (never created or truncated).
pub fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let f = File::open(path)?;
    #[cfg(windows)]
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.sync_all()
}

/// `name` as a name this system can give a file or folder of its own choosing (a joined folder named
/// after its link): without control characters, `/` and `:` (Finder shows `:` as `/`), and on Windows
/// also without `\ * ? " < > |`, trailing dots and spaces, and with `_` after a reserved device name
/// (`CON`, `NUL`, `COM1`…). May be empty.
pub fn clean_name(name: &str) -> String {
    let bad = |c: char| c.is_control() || c == '/' || c == ':' || windows_bad_char(c);
    let n: String = name.chars().filter(|&c| !bad(c)).collect();
    #[cfg(windows)]
    {
        let n = n.trim_end_matches(['.', ' ']);
        if reserved_device_name(n) { format!("{n}_") } else { n.to_owned() }
    }
    #[cfg(not(windows))]
    n
}

/// Why this system cannot hold a file or folder named `name` (one component, as on the wire), or
/// `None`. Windows refuses `\ : * ? " < > |` and control characters, drops a trailing dot or space
/// without a word (the item would land under another name), and makes reserved device names
/// (`CON`, `NUL`, `COM1`…) that most programs cannot open, move or delete. Same rules as
/// [`clean_name`].
pub fn name_problem(name: &str) -> Option<&'static str> {
    #[cfg(windows)]
    {
        let bad = name.chars().any(|c| c.is_control() || windows_bad_char(c)) || name.ends_with(['.', ' ']) || reserved_device_name(name);
        bad.then_some(NAME_NOT_ALLOWED)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        None
    }
}

/// Why [`name_problem`] refuses a name.
pub const NAME_NOT_ALLOWED: &str = "this name is not allowed on Windows; it is not synced";

/// A character Windows does not allow in a name (besides `/` and control characters); none elsewhere.
fn windows_bad_char(c: char) -> bool {
    cfg!(windows) && matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9` or `LPT1`-`LPT9`, with or without an extension.
#[cfg(windows)]
fn reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit() && stem.as_bytes()[3] != b'0')
}

/// "Not found" also when a path component is a file: both mean the path is not there.
pub fn is_not_found(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positioned_reads_and_writes_meet() {
        let t = tempfile::tempdir().unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(t.path().join("f")).unwrap();
        write_all_at(&f, b"world", 6).unwrap();
        write_all_at(&f, b"hello ", 0).unwrap();
        let mut buf = [0u8; 11];
        read_exact_at(&f, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello world");
        let mut tail = [0u8; 8];
        assert_eq!(read_at(&f, &mut tail, 6).unwrap(), 5);
        assert_eq!(read_at(&f, &mut tail, 11).unwrap(), 0, "at the end: 0 bytes, not an error");
        assert_eq!(read_exact_at(&f, &mut tail, 6).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_folder_and_a_file_in_it_share_a_volume() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("f"), b"x").unwrap();
        assert_eq!(volume_id(t.path()).unwrap(), volume_id(&t.path().join("f")).unwrap());
        assert!(volume_id(&t.path().join("missing")).is_err());
    }

    #[test]
    fn times_before_1970_count_down_with_positive_nanoseconds() {
        use std::time::{Duration, UNIX_EPOCH};
        // (Nanoseconds in steps of 100: Windows keeps SystemTime in 100 ns units.)
        assert_eq!(unix_time(UNIX_EPOCH + Duration::new(5, 700)), (5, 700));
        assert_eq!(unix_time(UNIX_EPOCH - Duration::new(5, 0)), (-5, 0));
        assert_eq!(unix_time(UNIX_EPOCH - Duration::new(4, 999_999_900)), (-5, 100));
    }

    #[test]
    fn a_file_and_its_folder_are_flushed_and_dated_by_path() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        // Windows: a read-only handle cannot be flushed, nor a folder opened as a file.
        sync_file(&f).unwrap();
        sync_dir(t.path()).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        for p in [f.as_path(), t.path()] {
            set_modified(p, old).unwrap();
            assert_eq!(std::fs::metadata(p).unwrap().modified().unwrap(), old, "{}", p.display());
        }
        assert!(is_not_found(&sync_file(&t.path().join("missing")).unwrap_err()));
        assert_eq!(std::fs::read(&f).unwrap(), b"x", "never truncated");
    }

    #[test]
    fn a_cleaned_name_is_one_this_system_can_hold() {
        assert_eq!(clean_name("Notes"), "Notes");
        assert_eq!(clean_name("a/b:c\u{7}"), "abc");
        if cfg!(windows) {
            assert_eq!(clean_name(r#"a\b*?"<>|c. ."#), "abc");
            assert_eq!(clean_name("con"), "con_");
            assert_eq!(clean_name("COM1.txt"), "COM1.txt_");
            assert_eq!(clean_name("COM0"), "COM0");
            assert_eq!(clean_name("Console"), "Console");
        } else {
            assert_eq!(clean_name(r#"a\b*?"<>|c. ."#), r#"a\b*?"<>|c. ."#);
        }
    }

    #[test]
    fn paths_round_trip_through_their_bytes() {
        let p = std::path::Path::new("a/b é/c.txt");
        assert_eq!(path_from_bytes(path_bytes(p)).as_deref(), Some(p));
    }
}
