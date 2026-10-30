//! Moving a file or folder to the Trash: [`move_to_trash`], one call on every system, and [`trash_check`], the same
//! decision made beforehand without moving anything (for a question that names where the item will go).
//!
//! * A Trash folder the caller names (the engine's `trash_dir` setting; what tests use): a plain move into it under
//!   a name that is free there (`a.torrent`, `a (2).torrent`, …; a folder `Docs`, `Docs 2`, …), on every system.
//! * Otherwise the system's own ([`system_trash`]). On macOS the system's own call,
//!   `-[NSFileManager trashItemAtURL:resultingItemURL:error:]` (what Finder and `FileManager.trashItem` use): an item
//!   goes into the Trash of its own volume (`~/.Trash` on the home volume, `<volume>/.Trashes/<uid>` on another one),
//!   Finder's Put Back works, and nothing is ever deleted (a volume that keeps no Trash makes the call fail and the
//!   item stays). On Windows the Recycle Bin, through the shell (`SHFileOperationW`, `FO_DELETE` with
//!   `FOF_ALLOWUNDO` and no UI). On other systems (Linux) the freedesktop.org home trash, `$XDG_DATA_HOME/Trash`
//!   (else `~/.local/share/Trash`): the item goes into `files/` and a `info/<name>.trashinfo` records where it was and
//!   when, so the desktop's Trash can put it back.
//!
//! A Trash folder lives on one volume, so a move into it from another volume is no rename. The caller says what then
//! happens ([`OtherVolume`]): a small file (a `.torrent`) is copied over and the original removed; anything else is
//! refused, and a folder is always refused (never copied). This holds for a Trash folder the caller names and for the
//! freedesktop.org home trash (its per-volume trash folders, `$topdir/.Trash-$uid`, are not used). The Mac's own
//! call keeps every item on its own volume, so there the volume rule does not apply.
//!
//! Under test (a cargo test binary, or `TRS_FORBID_REAL_HOME`; `TRS_ALLOW_REAL_HOME=1` overrides), the Mac's own call
//! is refused: it uses the user's real Trash whatever `HOME` says. Tests name a Trash folder.
//!
//! Every Windows volume has a Recycle Bin of its own, so the volume rule does not apply there. Instead, an item the
//! Recycle Bin would delete outright instead of keeping (a drive without a Recycle Bin, such as a network or
//! removable drive; one set to delete files at once; an item larger than the drive's Recycle Bin holds) is refused
//! before anything happens ([`TrashError::WouldDelete`]). Should that check miss a case, `FOF_WANTNUKEWARNING` makes
//! the shell ask the user before it deletes anything for good, and a "no" leaves the item where it was.

use std::io;
use std::path::{Path, PathBuf};

/// What a move into a Trash folder does with an item on another volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherVolume {
    /// Copy a file over, then remove the original (a small file such as a `.torrent`). A folder is still refused.
    Copy,
    /// Refuse: the move must be a rename (a folder of any size, which a copy would double on disk for a while).
    Refuse,
}

/// Where the system's Trash is ([`system_trash`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrashPlace {
    /// A folder: the freedesktop.org home trash (Linux), or a Trash folder the caller names.
    Folder(PathBuf),
    /// The Trash of the item's own volume, through the system's own call (macOS): `~/.Trash` on the home volume,
    /// `<volume>/.Trashes/<uid>` on another one. It is always the user's real Trash, whatever `HOME` says.
    VolumeTrash,
    /// The Recycle Bin of the item's own drive (Windows); it has no single folder.
    RecycleBin,
}

/// Where an item went (or would go, from [`trash_check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trashed {
    /// This path in a Trash folder.
    At(PathBuf),
    /// The Recycle Bin, which has no path to show.
    RecycleBin,
}

impl Trashed {
    /// The path in the Trash folder; `None` for the Recycle Bin.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::At(p) => Some(p),
            Self::RecycleBin => None,
        }
    }
}

/// Why an item was not moved to the Trash. Nothing was moved or deleted in any case.
#[derive(Debug)]
pub enum TrashError {
    /// There is no Trash: no Trash folder given and the system has none (no home folder).
    NoTrash,
    /// The item is on another volume than the Trash folder `trash`, and may not be copied there.
    OtherVolume { trash: PathBuf },
    /// The Recycle Bin would delete it for good instead of keeping it; `reason` says why, in words.
    WouldDelete { reason: String },
    /// The move failed.
    Io(io::Error),
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTrash => f.write_str(concat!("there is no home folder, so there is no ", crate::trash!())),
            Self::OtherVolume { trash } => write!(f, concat!("it is on another volume than the ", crate::trash!(), " ({})"), trash.display()),
            Self::WouldDelete { reason } => {
                write!(f, concat!("the ", crate::trash!(), " would delete it at once instead of keeping it ({})"), reason)
            }
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TrashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for TrashError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<TrashError> for io::Error {
    fn from(e: TrashError) -> Self {
        match e {
            TrashError::Io(e) => e,
            TrashError::OtherVolume { .. } => io::Error::new(io::ErrorKind::CrossesDevices, e.to_string()),
            e => io::Error::other(e.to_string()),
        }
    }
}

/// The system's Trash: the Trash of each volume on macOS, the Recycle Bin on Windows, the freedesktop.org home trash
/// (`$XDG_DATA_HOME/Trash`, else `~/.local/share/Trash`) elsewhere. `None` without one (no home folder on Linux).
pub fn system_trash() -> Option<TrashPlace> {
    #[cfg(windows)]
    {
        Some(TrashPlace::RecycleBin)
    }
    #[cfg(target_os = "macos")]
    {
        Some(TrashPlace::VolumeTrash)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        freedesktop::home_trash().map(TrashPlace::Folder)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Moves `path` (a file, a folder or a link, which is moved itself) to the Trash: into `trash_dir` when given, else
/// the system's ([`system_trash`]). `other` says what happens to an item on another volume than a Trash folder (the
/// Mac's own Trash keeps every item on its own volume). Never overwrites, never deletes: an item that cannot be kept
/// is left where it is, with the reason.
pub fn move_to_trash(path: &Path, trash_dir: Option<&Path>, other: OtherVolume) -> Result<Trashed, TrashError> {
    if let Some(dir) = trash_dir {
        return into_folder(path, dir, other, &mut |_| Ok(true), &mut |_| {}).map(Trashed::At);
    }
    match system_trash() {
        None => Err(TrashError::NoTrash),
        Some(TrashPlace::RecycleBin) => {
            #[cfg(windows)]
            {
                recycle::recycle(path).map(|()| Trashed::RecycleBin)
            }
            #[cfg(not(windows))]
            {
                Err(TrashError::NoTrash)
            }
        }
        Some(TrashPlace::VolumeTrash) => {
            #[cfg(target_os = "macos")]
            {
                if real_trash_forbidden() {
                    return Err(TrashError::Io(io::Error::other(concat!(
                        "refusing to use the real ",
                        crate::trash!(),
                        " while running under test (name a Trash folder; TRS_ALLOW_REAL_HOME=1 overrides)"
                    ))));
                }
                mac::trash(path).map(Trashed::At)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(TrashError::NoTrash)
            }
        }
        Some(TrashPlace::Folder(dir)) => {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                freedesktop::put(path, &dir, other).map(Trashed::At)
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            {
                into_folder(path, &dir, other, &mut |_| Ok(true), &mut |_| {}).map(Trashed::At)
            }
        }
    }
}

/// Whether this process runs under test, where the Mac's own Trash call (always the user's real Trash) is refused: a
/// cargo test binary (it lives in a `deps` folder), or `TRS_FORBID_REAL_HOME` (a daemon a test started), unless
/// `TRS_ALLOW_REAL_HOME` is set. The same rule as the engine's real-home guard.
#[cfg(target_os = "macos")]
fn real_trash_forbidden() -> bool {
    let test_binary = std::env::current_exe().ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps")).unwrap_or(false);
    (cfg!(test) || test_binary || std::env::var_os("TRS_FORBID_REAL_HOME").is_some()) && std::env::var_os("TRS_ALLOW_REAL_HOME").is_none()
}

/// What [`move_to_trash`] with the same arguments would do now, without moving anything: where the item would go (the
/// name free in the Trash folder at this moment), or why it would be refused. A missing item is an error (a missing
/// one that may not be copied reads as being on another volume: its volume is unknown). For the Mac's own Trash the
/// place is a prediction, for display only (`~/.Trash/<name>` on the home volume, else
/// `<volume>/.Trashes/<uid>/<name>`): the system picks the name, and [`move_to_trash`] returns the real one.
pub fn trash_check(path: &Path, trash_dir: Option<&Path>, other: OtherVolume) -> Result<Trashed, TrashError> {
    let place = match trash_dir {
        Some(d) => TrashPlace::Folder(d.to_path_buf()),
        None => system_trash().ok_or(TrashError::NoTrash)?,
    };
    match place {
        TrashPlace::VolumeTrash => {
            #[cfg(target_os = "macos")]
            {
                mac::check(path).map(Trashed::At)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(TrashError::NoTrash)
            }
        }
        TrashPlace::RecycleBin => {
            #[cfg(windows)]
            {
                let abs = std::path::absolute(path)?;
                std::fs::symlink_metadata(&abs)?;
                recycle::check(&abs).map(|()| Trashed::RecycleBin)
            }
            #[cfg(not(windows))]
            {
                Err(TrashError::NoTrash)
            }
        }
        TrashPlace::Folder(dir) => {
            #[cfg(all(unix, not(target_os = "macos")))]
            let dir = if trash_dir.is_none() { dir.join("files") } else { dir };
            let is_dir = folder_check(path, &dir, other)?;
            let name = item_name(path);
            let free = (1..=MAX_TRIES).map(|n| dir.join(numbered(&name, n, is_dir))).find(|p| !exists(p));
            free.map(Trashed::At).ok_or_else(|| TrashError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "no free name in the Trash")))
        }
    }
}

const MAX_TRIES: u32 = 9999;

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// The volume of `path`, or of its nearest folder that exists (a Trash folder not made yet).
fn nearest_volume(path: &Path) -> Option<u64> {
    path.ancestors().find_map(|d| super::volume_id(if d.as_os_str().is_empty() { Path::new(".") } else { d }).ok())
}

/// The checks before a move into the Trash folder `dir`: the volume rule, then that the item is there. Whether the
/// item is a folder.
fn folder_check(path: &Path, dir: &Path, other: OtherVolume) -> Result<bool, TrashError> {
    let meta = std::fs::symlink_metadata(path);
    let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
    if other == OtherVolume::Refuse || is_dir {
        let here = super::volume_id(path).ok();
        if here.is_none() || here != nearest_volume(dir) {
            return Err(TrashError::OtherVolume { trash: dir.to_path_buf() });
        }
    }
    meta?;
    Ok(is_dir)
}

/// The item's own name (`item` for a path without one, such as a root).
fn item_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "item".into())
}

/// Moves `path` into `dir` under the first free name. `claim(name)` is asked first for each free name and may take it
/// (`Ok(true)`) or pass (`Ok(false)`: taken elsewhere); `release(name)` gives a claimed name back when the move did
/// not happen. The freedesktop.org trash claims a name by creating its `.trashinfo`.
fn into_folder(
    path: &Path,
    dir: &Path,
    other: OtherVolume,
    claim: &mut dyn FnMut(&str) -> io::Result<bool>,
    release: &mut dyn FnMut(&str),
) -> Result<PathBuf, TrashError> {
    let is_dir = folder_check(path, dir, other)?;
    std::fs::create_dir_all(dir)?;
    let name = item_name(path);
    for n in 1..=MAX_TRIES {
        let candidate = numbered(&name, n, is_dir);
        let target = dir.join(&candidate);
        if exists(&target) || !claim(&candidate)? {
            continue;
        }
        match move_item(path, &target, other, is_dir) {
            Ok(()) => return Ok(target),
            Err(e) => {
                release(&candidate);
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(if e.kind() == io::ErrorKind::CrossesDevices {
                        TrashError::OtherVolume { trash: dir.to_path_buf() }
                    } else {
                        TrashError::Io(e)
                    });
                }
            }
        }
    }
    Err(TrashError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "no free name in the Trash")))
}

/// A rename that never replaces; across volumes, for a file the caller lets be copied, a copy and then the removal of
/// the original.
fn move_item(path: &Path, target: &Path, other: OtherVolume, is_dir: bool) -> io::Result<()> {
    match super::rename_no_replace(path, target) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices && other == OtherVolume::Copy && !is_dir => copy_then_remove(path, target),
        r => r,
    }
}

fn copy_then_remove(from: &Path, to: &Path) -> io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    let copied = io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if let Err(e) = copied {
        drop(dst);
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    let _ = super::copy_metadata(&src, &dst);
    // A metadata copy resets the times on some systems: the file keeps its own.
    if let Ok(m) = src.metadata().and_then(|m| m.modified()) {
        let _ = dst.set_modified(m);
    }
    drop((src, dst));
    std::fs::remove_file(from).inspect_err(|_| {
        let _ = std::fs::remove_file(to);
    })
}

/// The `n`th name tried for `name`: itself, then a folder `Docs 2`, `Docs 3` and a file `a (2).torrent`,
/// `x (2).tar.gz`. Kept within 255 bytes, the longest name file systems hold.
fn numbered(name: &str, n: u32, is_dir: bool) -> String {
    if n <= 1 {
        return name.to_owned();
    }
    let (stem, ext, suffix) = if is_dir { (name, "", format!(" {n}")) } else {
        let (s, e) = split_ext(name);
        (s, e, format!(" ({n})"))
    };
    let mut end = 255usize.saturating_sub(ext.len() + suffix.len()).min(stem.len());
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..end])
}

/// `name.ext` into `name` and `.ext` (an extension of up to 10 letters and digits; `.tar.gz` stays together; a leading
/// dot is no extension).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(d) if d > 0 && name.len() - d <= 11 && name[d + 1..].bytes().all(|c| c.is_ascii_alphanumeric()) => {
            let stem = &name[..d];
            match stem.rfind('.') {
                Some(d2) if d2 > 0 && stem[d2..].eq_ignore_ascii_case(".tar") => (&name[..d2], &name[d2..]),
                _ => (stem, &name[d..]),
            }
        }
        _ => (name, ""),
    }
}

/// The freedesktop.org home trash (the Trash specification 1.0): `files/` holds the items, `info/<name>.trashinfo`
/// where each was and when it was trashed.
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod freedesktop {
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};

    use super::{OtherVolume, TrashError};

    /// `$XDG_DATA_HOME/Trash` (an absolute one), else `~/.local/share/Trash`.
    pub(super) fn home_trash() -> Option<PathBuf> {
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
        data.or_else(|| crate::dirs::home().filter(|h| h.is_absolute()).map(|h| h.join(".local/share"))).map(|d| d.join("Trash"))
    }

    /// Moves `path` into the trash at `root`; returns its path in `files/`.
    pub(super) fn put(path: &Path, root: &Path, other: OtherVolume) -> Result<PathBuf, TrashError> {
        let (files, info) = (root.join("files"), root.join("info"));
        let made = !root.exists();
        std::fs::create_dir_all(&info)?;
        if made {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
        }
        let original = std::path::absolute(path)?;
        let record = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode(&original), local_now());
        let info_of = |name: &str| info.join(format!("{name}.trashinfo"));
        // A name is ours once its `.trashinfo` is created (never over another one).
        let mut claim = |name: &str| -> io::Result<bool> {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(info_of(name)) {
                Ok(mut f) => f.write_all(record.as_bytes()).and_then(|()| f.sync_all()).map(|()| true).inspect_err(|_| {
                    let _ = std::fs::remove_file(info_of(name));
                }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e),
            }
        };
        let mut release = |name: &str| {
            let _ = std::fs::remove_file(info_of(name));
        };
        super::into_folder(path, &files, other, &mut claim, &mut release)
    }

    /// The path as the specification writes it: percent-encoded bytes, `/` and the unreserved characters as they are.
    pub(super) fn encode(p: &Path) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut s = String::new();
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
                s.push(char::from(b));
            } else {
                s.push_str(&format!("%{b:02X}"));
            }
        }
        s
    }

    /// Local time as `YYYY-MM-DDThh:mm:ss`.
    fn local_now() -> String {
        // SAFETY: `time` with a null pointer only returns; `localtime_r` writes into the `tm` given, which lives
        // through the call.
        let tm = unsafe {
            let t = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            tm
        };
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// The Mac's own Trash: `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSError, NSFileManager, NSURL};

    use super::{MAX_TRIES, TrashError, exists, item_name, numbered};

    /// `NSFileNoSuchFileError`, `NSFileReadNoSuchFileError`, `NSFileReadNoPermissionError`, `NSFileWriteNoPermissionError`,
    /// `NSFileWriteVolumeReadOnlyError` (NSCocoaErrorDomain).
    const NO_SUCH_FILE: [isize; 2] = [4, 260];
    const NO_PERMISSION: [isize; 2] = [257, 513];
    const READ_ONLY_VOLUME: isize = 642;

    /// Moves `path` into the Trash of its volume; where it went. Blocks for as long as the system takes (a folder on its
    /// own volume is a rename). The autorelease pool frees what the call leaves behind on any thread.
    pub(super) fn trash(path: &Path) -> Result<PathBuf, TrashError> {
        let abs = std::path::absolute(path)?;
        let (is_dir, mount) = volume_of(&abs)?;
        autoreleasepool(|_| {
            let url = NSURL::from_file_path(&abs).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a path the system can name"))?;
            let mut out = None;
            NSFileManager::defaultManager().trashItemAtURL_resultingItemURL_error(&url, Some(&mut out)).map_err(|e| TrashError::Io(io_error(&e)))?;
            // Moved; the system says where (it always does; else the place it would have picked). A folder's URL ends
            // in `/`, which the path does not keep.
            let went = out.and_then(|u| u.to_file_path()).map(|p| p.components().collect::<PathBuf>());
            Ok(went.unwrap_or_else(|| predicted(&abs, is_dir, &mount).unwrap_or_else(|_| abs.clone())))
        })
    }

    /// The checks [`trash`] makes, and where the item would go (a prediction: the system picks the name).
    pub(super) fn check(path: &Path) -> Result<PathBuf, TrashError> {
        let abs = std::path::absolute(path)?;
        let (is_dir, mount) = volume_of(&abs)?;
        predicted(&abs, is_dir, &mount)
    }

    /// Whether the item is a folder, and its volume's mount point; refused when the item is missing or its volume is
    /// read-only (no Trash can be made there). A link is looked at itself (its folder's volume), never its target.
    fn volume_of(abs: &Path) -> Result<(bool, PathBuf), TrashError> {
        let meta = std::fs::symlink_metadata(abs)?;
        let on = if meta.file_type().is_symlink() { abs.parent().unwrap_or(abs) } else { abs };
        let (mount, read_only) = statfs(on)?;
        if read_only {
            return Err(TrashError::Io(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, format!("{} is on a read-only volume", abs.display()))));
        }
        Ok((meta.is_dir(), mount))
    }

    /// `~/.Trash/<free name>` for an item on the home folder's volume, else `<volume>/.Trashes/<uid>/<free name>`.
    fn predicted(abs: &Path, is_dir: bool, mount: &Path) -> Result<PathBuf, TrashError> {
        let home = crate::dirs::home().filter(|h| h.is_absolute());
        let same = |a: &Path, b: &Path| matches!((super::super::volume_id(a), super::super::volume_id(b)), (Ok(x), Ok(y)) if x == y);
        let dir = match home {
            Some(h) if same(mount, &h) => h.join(".Trash"),
            // SAFETY: getuid has no preconditions and cannot fail.
            _ => mount.join(".Trashes").join(unsafe { libc::getuid() }.to_string()),
        };
        let name = item_name(abs);
        let free = (1..=MAX_TRIES).map(|n| dir.join(numbered(&name, n, is_dir))).find(|p| !exists(p));
        free.ok_or_else(|| TrashError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "no free name in the Trash")))
    }

    /// The mount point of the volume holding `path`, and whether it is read-only.
    fn statfs(path: &Path) -> io::Result<(PathBuf, bool)> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `st` is plain-old-data that statfs fully initialises on success; `c` is NUL-terminated and outlives
        // the call; the pointer is to a live, writable statfs.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel NUL-terminates f_mntonname within its fixed size.
        let mount = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
        let read_only = st.f_flags & (libc::MNT_RDONLY as u32) != 0;
        Ok((PathBuf::from(std::ffi::OsStr::from_bytes(mount.to_bytes())), read_only))
    }

    /// The system's error as an `io::Error` of the fitting kind, with its own words.
    fn io_error(e: &NSError) -> io::Error {
        let code = e.code();
        let kind = if NO_SUCH_FILE.contains(&code) {
            io::ErrorKind::NotFound
        } else if NO_PERMISSION.contains(&code) {
            io::ErrorKind::PermissionDenied
        } else if code == READ_ONLY_VOLUME {
            io::ErrorKind::ReadOnlyFilesystem
        } else {
            io::ErrorKind::Other
        };
        io::Error::new(kind, e.localizedDescription().to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_place_is_predicted_on_the_items_own_volume_and_nothing_moves() {
            let t = tempfile::tempdir().unwrap();
            let f = t.path().join("a.torrent");
            std::fs::write(&f, b"x").unwrap();
            let p = check(&f).unwrap();
            assert_eq!(p.file_name().unwrap(), "a.torrent");
            let home_trash = crate::dirs::home().map(|h| h.join(".Trash"));
            assert!(home_trash.as_deref() == p.parent() || p.parent().unwrap().parent().unwrap().ends_with(".Trashes"), "{}", p.display());
            assert!(f.is_file(), "nothing moved");
            assert!(matches!(check(&t.path().join("gone")), Err(TrashError::Io(e)) if e.kind() == io::ErrorKind::NotFound));
            // A volume that is not the home folder's: its own `.Trashes/<uid>`. `/dev` is a volume of its own.
            let dev = check(Path::new("/dev/null")).unwrap();
            assert_eq!(dev, Path::new("/dev/.Trashes").join(unsafe { libc::getuid() }.to_string()).join("null"));
        }

        #[test]
        fn under_test_the_real_trash_is_never_used() {
            let t = tempfile::tempdir().unwrap();
            let f = t.path().join("a.torrent");
            std::fs::write(&f, b"x").unwrap();
            let e = super::super::move_to_trash(&f, None, super::super::OtherVolume::Copy).unwrap_err();
            assert!(e.to_string().contains("while running under test"), "{e}");
            assert!(f.is_file());
        }

        /// The one test that uses the real Trash: a file made in a temporary folder goes to the user's Trash, where it
        /// stays (Finder's Put Back shows where it came from). Run by hand only: `cargo test -p trs-platform -- --ignored
        /// the_real_trash`.
        #[test]
        #[ignore = "moves a file into the user's real Trash"]
        fn the_real_trash_takes_a_file_from_a_temporary_folder() {
            let t = tempfile::tempdir().unwrap();
            let f = t.path().join(format!("seedwell-trash-test-{}.txt", std::process::id()));
            std::fs::write(&f, b"a test file from trs-platform; safe to delete").unwrap();
            let went = trash(&f).unwrap();
            assert!(!f.exists() && went.is_file(), "{}", went.display());
            assert_eq!(std::fs::read(&went).unwrap(), b"a test file from trs-platform; safe to delete");
        }
    }
}

/// The Windows Recycle Bin, through the shell.
#[cfg(windows)]
mod recycle {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW,
        SHQUERYRBINFO, SHQueryRecycleBinW,
    };

    use super::TrashError;

    /// `GetDriveTypeW`: a hard disk (`DRIVE_FIXED`). Network, removable and optical drives have no Recycle Bin.
    const DRIVE_FIXED: u32 = 3;
    const POLICIES: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    const BIT_BUCKET: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume";

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// `path` without the `\\?\` prefix, which the shell does not take.
    fn plain(path: &Path) -> PathBuf {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{rest}"))
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            path.to_path_buf()
        }
    }

    fn would_delete(reason: impl Into<String>) -> TrashError {
        TrashError::WouldDelete { reason: reason.into() }
    }

    fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
        let (k, v) = (wide(key.as_ref()), wide(value.as_ref()));
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: the strings are NUL-terminated and outlive the call; `data` holds the `size` bytes asked for.
        let r = unsafe {
            RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&raw mut data).cast::<c_void>(), &mut size)
        };
        (r == ERROR_SUCCESS).then_some(data)
    }

    /// The bytes of `path`, a folder's counted through its tree (links are not followed); stops counting past `cap`.
    fn size(path: &Path, cap: u64) -> u64 {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
        if !meta.is_dir() {
            return meta.len();
        }
        let mut total = 0u64;
        let mut stack = vec![path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
                if m.is_dir() {
                    stack.push(e.path());
                } else {
                    total = total.saturating_add(m.len());
                }
                if total > cap {
                    return total;
                }
            }
        }
        total
    }

    /// Refuses (before anything happens) what the Recycle Bin of `path`'s drive would delete outright.
    pub(super) fn check(path: &Path) -> Result<(), TrashError> {
        let p = wide(plain(path).as_os_str());
        let mut root = vec![0u16; 1024];
        // SAFETY: `p` is NUL-terminated; `root` has room for the length given.
        if unsafe { GetVolumePathNameW(p.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root_s = from_wide(&root);
        let root_w = wide(root_s.as_ref());
        // SAFETY: `root_w` is a NUL-terminated root path.
        if unsafe { GetDriveTypeW(root_w.as_ptr()) } != DRIVE_FIXED {
            return Err(would_delete(format!("{root_s} is a network or removable drive, which has no Recycle Bin")));
        }
        let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, i64Size: 0, i64NumItems: 0 };
        // SAFETY: `root_w` is NUL-terminated and `info` is sized as `cbSize` says.
        if unsafe { SHQueryRecycleBinW(root_w.as_ptr(), &mut info) } < 0 {
            return Err(would_delete(format!("the drive {root_s} has no Recycle Bin")));
        }
        let policy = |v: &str| reg_dword(HKEY_CURRENT_USER, POLICIES, v).or_else(|| reg_dword(HKEY_LOCAL_MACHINE, POLICIES, v));
        if policy("NoRecycleFiles") == Some(1) {
            return Err(would_delete("the Recycle Bin is set to delete files at once"));
        }
        // The drive's own settings are kept under its volume GUID.
        let mut vol = vec![0u16; 64];
        // SAFETY: `root_w` is a NUL-terminated mount point ending in a backslash; `vol` has room for the length given.
        let guid = (unsafe { GetVolumeNameForVolumeMountPointW(root_w.as_ptr(), vol.as_mut_ptr(), vol.len() as u32) } != 0)
            .then(|| from_wide(&vol))
            .and_then(|v| v.find('{').zip(v.find('}')).map(|(a, b)| v[a..=b].to_owned()));
        let setting = |v: &str| guid.as_ref().and_then(|g| reg_dword(HKEY_CURRENT_USER, &format!(r"{BIT_BUCKET}\{g}"), v));
        if setting("NukeOnDelete") == Some(1) {
            return Err(would_delete(format!("the Recycle Bin of {root_s} is set to delete files at once")));
        }
        let mut total = 0u64;
        // SAFETY: `root_w` is NUL-terminated; the out pointers are valid or null.
        let ok = unsafe { GetDiskFreeSpaceExW(root_w.as_ptr(), std::ptr::null_mut(), &mut total, std::ptr::null_mut()) } != 0;
        // How much the drive's Recycle Bin holds: the policy's percentage, the drive's setting (MB), or (never set)
        // 5% of the drive, which is at most what Windows gives it by default.
        let cap = match (policy("RecycleBinSize"), setting("MaxCapacity")) {
            (Some(pct), _) if ok => total / 100 * u64::from(pct.min(100)),
            (_, Some(mb)) => u64::from(mb) * 1024 * 1024,
            _ if ok => total / 20,
            _ => 0,
        };
        if size(path, cap) > cap {
            return Err(would_delete(format!("larger than the Recycle Bin of {root_s} holds")));
        }
        Ok(())
    }

    /// Moves `path` to its drive's Recycle Bin, after [`check`].
    pub(super) fn recycle(path: &Path) -> Result<(), TrashError> {
        let abs = std::path::absolute(path)?;
        std::fs::symlink_metadata(&abs)?;
        check(&abs)?;
        let shown = plain(&abs);
        // A list of paths: each NUL-terminated, the list ended by a second NUL.
        let mut from: Vec<u16> = shown.as_os_str().encode_wide().collect();
        from.extend([0, 0]);
        // Its own thread: the shell wants a single-threaded COM apartment, which must not stay behind on a pooled
        // thread of the caller.
        let (code, aborted) = std::thread::spawn(move || {
            // SAFETY: a null reserved pointer; uninitialised again below when this succeeded.
            let com = unsafe { CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
            let mut op = SHFILEOPSTRUCTW {
                hwnd: std::ptr::null_mut(),
                wFunc: FO_DELETE,
                pFrom: from.as_ptr(),
                pTo: std::ptr::null(),
                // Undo (the Recycle Bin), no questions, no progress, no error dialogs; but a warning before anything
                // would be deleted for good, should `check` have missed it ("no" keeps the item).
                fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING) as u16,
                fAnyOperationsAborted: 0,
                hNameMappings: std::ptr::null_mut(),
                lpszProgressTitle: std::ptr::null(),
            };
            // SAFETY: `op` is fully set up and `from` (double NUL-terminated) outlives the call.
            let code = unsafe { SHFileOperationW(&mut op) };
            if com >= 0 {
                // SAFETY: paired with the successful CoInitializeEx above, on the same thread.
                unsafe { CoUninitialize() };
            }
            (code, op.fAnyOperationsAborted != 0)
        })
        .join()
        .map_err(|_| std::io::Error::other("the Recycle Bin call failed"))?;
        if aborted {
            return Err(would_delete("it was kept where it is"));
        }
        if code != 0 {
            return Err(std::io::Error::other(format!("the Recycle Bin refused it (shell error {code:#x})")).into());
        }
        if std::fs::symlink_metadata(&abs).is_ok() {
            return Err(std::io::Error::other("the Recycle Bin left it where it is").into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_get_a_number_before_the_extension_and_folders_after_the_name() {
        assert_eq!(numbered("a.torrent", 1, false), "a.torrent");
        assert_eq!(numbered("a.torrent", 2, false), "a (2).torrent");
        assert_eq!(numbered("x.tar.gz", 3, false), "x (3).tar.gz");
        assert_eq!(numbered(".hidden", 2, false), ".hidden (2)");
        assert_eq!(numbered("noext", 2, false), "noext (2)");
        assert_eq!(numbered("Docs", 2, true), "Docs 2");
        assert_eq!(numbered("My.Folder", 2, true), "My.Folder 2", "a folder has no extension");
        let long = format!("{}.torrent", "é".repeat(200));
        let n = numbered(&long, 12, false);
        assert!(n.len() <= 255 && n.ends_with(" (12).torrent"), "{n}");
    }

    #[test]
    fn a_trash_folder_takes_files_and_folders_under_free_names() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        let put = |name: &str| {
            let f = t.path().join(name);
            std::fs::write(&f, name).unwrap();
            let r = move_to_trash(&f, Some(&bin), OtherVolume::Copy).unwrap();
            assert!(!f.exists(), "moved, not copied");
            r
        };
        assert_eq!(put("a.torrent"), Trashed::At(bin.join("a.torrent")), "the Trash folder is made as needed");
        assert_eq!(put("a.torrent"), Trashed::At(bin.join("a (2).torrent")));
        assert_eq!(put("a.torrent").path(), Some(bin.join("a (3).torrent").as_path()));
        assert_eq!(std::fs::read(bin.join("a.torrent")).unwrap(), b"a.torrent", "nothing is overwritten");

        let docs = t.path().join("Docs");
        std::fs::create_dir_all(docs.join("sub")).unwrap();
        std::fs::write(docs.join("sub/f"), b"x").unwrap();
        std::fs::create_dir_all(bin.join("Docs")).unwrap();
        let checked = trash_check(&docs, Some(&bin), OtherVolume::Refuse).unwrap();
        assert_eq!(checked, Trashed::At(bin.join("Docs 2")), "the check names the place");
        assert!(docs.exists(), "and moves nothing");
        assert_eq!(move_to_trash(&docs, Some(&bin), OtherVolume::Refuse).unwrap(), checked);
        assert_eq!(std::fs::read(bin.join("Docs 2/sub/f")).unwrap(), b"x");
    }

    #[test]
    fn a_missing_item_is_an_error_and_one_on_an_unknown_volume_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        let gone = t.path().join("gone.torrent");
        assert!(matches!(move_to_trash(&gone, Some(&bin), OtherVolume::Copy), Err(TrashError::Io(e)) if e.kind() == io::ErrorKind::NotFound));
        let e = trash_check(&gone, Some(&bin), OtherVolume::Refuse).unwrap_err();
        assert!(matches!(&e, TrashError::OtherVolume { trash } if trash == &bin), "{e}");
        assert!(e.to_string().contains("another volume"), "{e}");
        assert!(!bin.exists(), "nothing was made");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_trashed_itself_never_what_it_points_at() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("bin");
        let real = t.path().join("real.dat");
        std::fs::write(&real, b"keep").unwrap();
        let link = t.path().join("link.torrent");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        move_to_trash(&link, Some(&bin), OtherVolume::Copy).unwrap();
        assert!(std::fs::symlink_metadata(bin.join("link.torrent")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn the_freedesktop_trash_records_where_each_item_was() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Trash");
        let dir = t.path().join("my files");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a b%.torrent");
        for _ in 0..2 {
            std::fs::write(&f, b"x").unwrap();
            freedesktop::put(&f, &root, OtherVolume::Copy).unwrap();
        }
        assert!(root.join("files/a b%.torrent").is_file() && root.join("files/a b% (2).torrent").is_file());
        let info = std::fs::read_to_string(root.join("info/a b% (2).torrent.trashinfo")).unwrap();
        let abs = std::path::absolute(&f).unwrap();
        let mut lines = info.lines();
        assert_eq!(lines.next(), Some("[Trash Info]"));
        assert_eq!(lines.next(), Some(format!("Path={}", freedesktop::encode(&abs)).as_str()));
        let date = lines.next().unwrap().strip_prefix("DeletionDate=").unwrap();
        assert_eq!((date.len(), &date[4..5], &date[10..11]), (19, "-", "T"), "{date}");
        assert!(freedesktop::encode(&abs).ends_with("/my%20files/a%20b%25.torrent"));
        // A failed move leaves no record behind.
        assert!(freedesktop::put(&dir.join("gone"), &root, OtherVolume::Copy).is_err());
        assert!(!root.join("info/gone.trashinfo").exists());
    }
}
