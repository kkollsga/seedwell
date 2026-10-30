//! The filesystem the core works on, injected so the same scanner and puller run on disk ([`StdFs`])
//! and in the simulator ([`MemFs`]).
//!
//! Paths are relative to the folder root, `/` between components, `""` for the root itself, and
//! exactly as the name is stored on disk (normalization is the caller's job: `path.rs`). Every call is
//! blocking; the driver runs them on blocking threads.

use std::io;

use trs_sync_proto::model::FileKind;

mod disk;
mod flush;
mod mem;

pub use disk::StdFs;
pub use mem::{Clock, ManualClock, MemFs, MemFsOptions};

/// What `lstat` says about a path (links are not followed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// File, directory or symlink. Other kinds (sockets, devices, fifos) say `File` with
    /// [`Meta::other`] set, and are never synced.
    pub kind: FileKind,
    /// Bytes (files only; 0 otherwise).
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub mtime_ns: i64,
    /// Status-change time, Unix nanoseconds: changes on every write, rename and permission change, so
    /// (size, mtime, inode, ctime) unchanged means the content is unchanged.
    pub ctime_ns: i64,
    /// Inode number (follows a file across renames on one volume).
    pub inode: u64,
    /// The owner's executable bit.
    pub exec: bool,
    /// A cloud placeholder whose data is not on this Mac (`SF_DATALESS`): reading it would download it.
    pub dataless: bool,
    /// A socket, device or fifo: never synced.
    pub other: bool,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The name as stored on disk. Names that are not valid UTF-8 are left out of listings (and
    /// reported by [`Fs::read_dir`] in `skipped`): they cannot be synced to other systems safely.
    pub name: String,
}

/// A directory listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The entries, in no particular order.
    pub entries: Vec<DirEntry>,
    /// Entries whose names are not UTF-8 (lossy form, for the issue shown to the user).
    pub skipped: Vec<String>,
}

/// An open file.
// `len` is an I/O call on an open file, not a container's size: an `is_empty` would add nothing.
#[allow(clippy::len_without_is_empty)]
pub trait FileHandle: Send {
    /// Reads at `offset`; fewer bytes than asked only at the end of the file.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes all of `buf` at `offset`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Truncates or extends to `len`.
    fn set_len(&self, len: u64) -> io::Result<()>;
    /// The current length.
    fn len(&self) -> io::Result<u64>;
    /// Flushes data and metadata to stable storage (on macOS `F_FULLFSYNC`: the drive empties its cache).
    fn sync_all(&self) -> io::Result<()>;
    /// Orders the file's data before everything written after it, without waiting for the medium (macOS
    /// `F_BARRIERFSYNC`; `fsync` elsewhere). Durable at the next full flush ([`Self::sync_all`] or
    /// [`Fs::sync_dir`] of anything).
    fn sync_barrier(&self) -> io::Result<()> {
        self.sync_all()
    }
}

/// The operations the scanner and the puller need.
pub trait Fs: Send + Sync {
    /// `lstat`; `Ok(None)` when the path does not exist.
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>>;
    /// Lists a directory.
    fn read_dir(&self, rel: &str) -> io::Result<Listing>;
    /// Opens an existing file for reading.
    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>>;
    /// Opens a file for reading and writing, creating it (empty) when missing.
    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>>;
    /// Creates one directory (the parent must exist; an existing directory is not an error).
    fn create_dir(&self, rel: &str) -> io::Result<()>;
    /// Removes an empty directory.
    fn remove_dir(&self, rel: &str) -> io::Result<()>;
    /// Removes a file or symlink.
    fn remove_file(&self, rel: &str) -> io::Result<()>;
    /// Renames, replacing `to` atomically when it is a file (POSIX `rename`).
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// A second name for the same file (for the archive: costs nothing until the file is replaced).
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()>;
    /// Copies a file; a clone (copy on write) where the filesystem supports it (APFS).
    fn copy(&self, from: &str, to: &str) -> io::Result<()>;
    /// Sets the modification time.
    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()>;
    /// Sets or clears the executable bits (owner, group, other alike).
    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()>;
    /// Creates a symlink at `rel` pointing to `target` (stored as given).
    fn symlink(&self, target: &str, rel: &str) -> io::Result<()>;
    /// Reads a symlink's target.
    fn read_link(&self, rel: &str) -> io::Result<String>;
    /// Makes a directory's entries durable (`fsync` on the directory; on macOS `F_FULLFSYNC`, which also
    /// empties the drive's cache, so data behind a [`FileHandle::sync_barrier`] is durable too): the files
    /// created, renamed into or out of it and removed from it since. Without it a crash can undo a rename even
    /// after the file's own data was synced.
    fn sync_dir(&self, rel: &str) -> io::Result<()>;
    /// Why this filesystem cannot hold an item named `name` (one path component), or `None`: the
    /// puller does not try such a path (see [`crate::pull::Unsupported`]).
    fn name_problem(&self, name: &str) -> Option<&'static str> {
        let _ = name;
        None
    }
}

/// Reads a whole file (small files: tests, the ignore file, the simulator).
pub fn read_all(fs: &dyn Fs, rel: &str) -> io::Result<Vec<u8>> {
    let fh = fs.open_read(rel)?;
    let len = usize::try_from(fh.len()?).map_err(|_| io::Error::other(format!("{rel}: too large to read whole")))?;
    let mut buf = vec![0; len];
    let n = fh.read_at(0, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Creates or replaces the content of a file in place (not atomic: the puller builds a temp file and
/// renames it instead).
pub fn write_all(fs: &dyn Fs, rel: &str, data: &[u8]) -> io::Result<()> {
    let fh = fs.open_rw(rel)?;
    fh.set_len(0)?;
    fh.write_at(0, data)
}

/// Splits a relative path into its components, refusing anything that could leave the folder root:
/// an absolute path, `..`, `.`, empty components and NUL. `""` is the root itself (no components).
///
/// Paths are checked by [`trs_sync_proto::model::validate_path`] long before they get here; this is the
/// second line of defense, so a bug elsewhere cannot write outside the folder.
pub fn components(rel: &str) -> io::Result<Vec<&str>> {
    if rel.is_empty() {
        return Ok(Vec::new());
    }
    let refuse = |why: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("refused path {rel:?}: {why}"));
    if rel.starts_with('/') {
        return Err(refuse("absolute"));
    }
    let parts: Vec<&str> = rel.split('/').collect();
    for c in &parts {
        match *c {
            "" => return Err(refuse("empty component")),
            "." | ".." => return Err(refuse("dot component")),
            _ if c.contains('\0') => return Err(refuse("NUL")),
            _ => {}
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_stay_inside_the_root() {
        assert_eq!(components("").unwrap(), Vec::<&str>::new());
        assert_eq!(components("a/b c/.d").unwrap(), vec!["a", "b c", ".d"]);
        for bad in ["/etc", "..", "a/../b", "a/./b", "a//b", "a/", "a\0b"] {
            let e = components(bad).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad}");
        }
    }
}
