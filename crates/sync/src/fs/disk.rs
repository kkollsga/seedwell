//! [`StdFs`]: the real filesystem, through `std` only.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use trs_sync_proto::model::FileKind;

use super::{DirEntry, FileHandle, Fs, Listing, Meta, components};

/// The folder on disk. Every path is joined below `root` through [`components`], so `..` and absolute
/// paths are refused. Operations that write, and [`Fs::open_read`], also refuse a path whose parent
/// directories include a symlink: a directory replaced by a link behind our back must not make the
/// puller write (or a peer read) outside the folder. That check is a few `lstat`s and is not atomic
/// (a race between the check and the call remains); [`Fs::lstat`] and [`Fs::read_dir`], the scanner's
/// hot path, skip it, and the scanner never follows links anyway.
#[derive(Clone, Debug)]
pub struct StdFs {
    root: PathBuf,
}

impl StdFs {
    /// The folder at `root` (it is not created or checked here).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StdFs { root: root.into() }
    }

    /// The folder root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `root/rel`, refusing paths that leave the root.
    fn join(&self, rel: &str) -> io::Result<PathBuf> {
        let mut p = self.root.clone();
        for c in components(rel)? {
            p.push(c);
        }
        Ok(p)
    }

    /// [`Self::join`], and no symlink among the parents of `rel` (the root itself may be a link).
    fn join_checked(&self, rel: &str) -> io::Result<PathBuf> {
        let parts = components(rel)?;
        let mut p = self.root.clone();
        for (i, c) in parts.iter().enumerate() {
            if i > 0 {
                match fs::symlink_metadata(&p) {
                    Ok(m) if m.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("refused path {rel:?}: {} is a symlink", parts[..i].join("/")),
                        ));
                    }
                    // Missing parents fail in the call itself with the usual error.
                    _ => {}
                }
            }
            p.push(c);
        }
        Ok(p)
    }
}

fn meta_of(path: &Path, m: &fs::Metadata) -> io::Result<Meta> {
    let ft = m.file_type();
    let (kind, other) = if ft.is_symlink() {
        (FileKind::Symlink, false)
    } else if ft.is_dir() {
        (FileKind::Dir, false)
    } else {
        (FileKind::File, !ft.is_file())
    };
    let st = trs_platform::fs::file_stamp(path, m)?;
    Ok(Meta {
        kind,
        size: if kind == FileKind::File && !other { m.len() } else { 0 },
        mtime_ns: st.mtime_ns,
        ctime_ns: st.ctime_ns,
        inode: st.file_id,
        exec: st.exec,
        dataless: st.dataless,
        other,
    })
}

/// Splits raw directory entry names into UTF-8 names and the lossy forms of the others.
pub(crate) fn split_names(names: impl IntoIterator<Item = OsString>) -> Listing {
    let mut out = Listing::default();
    for n in names {
        match n.into_string() {
            Ok(name) => out.entries.push(DirEntry { name }),
            Err(raw) => out.skipped.push(raw.to_string_lossy().into_owned()),
        }
    }
    // Not promised by the trait, but cheap, and it keeps scans (and their logs) in a stable order.
    out.entries.sort_by(|a, b| a.name.cmp(&b.name));
    out.skipped.sort();
    out
}

fn system_time(ns: i64) -> SystemTime {
    let d = Duration::from_nanos(ns.unsigned_abs());
    if ns >= 0 { UNIX_EPOCH + d } else { UNIX_EPOCH - d }
}

struct StdHandle(File);

impl FileHandle for StdHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match trs_platform::fs::read_at(&self.0, &mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        trs_platform::fs::write_all_at(&self.0, buf, offset)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn sync_all(&self) -> io::Result<()> {
        self.0.sync_all()
    }

    fn sync_barrier(&self) -> io::Result<()> {
        super::flush::barrier(&self.0)
    }
}

impl Fs for StdFs {
    fn name_problem(&self, name: &str) -> Option<&'static str> {
        trs_platform::fs::name_problem(name)
    }

    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        let p = self.join(rel)?;
        // Gone between the two calls (Windows opens the file for its stamp) is gone too.
        match fs::symlink_metadata(&p).and_then(|m| meta_of(&p, &m)) {
            Ok(m) => Ok(Some(m)),
            Err(e) if trs_platform::fs::is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        let mut names = Vec::new();
        for e in fs::read_dir(self.join(rel)?)? {
            names.push(e?.file_name());
        }
        Ok(split_names(names))
    }

    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        Ok(Box::new(StdHandle(File::open(self.join_checked(rel)?)?)))
    }

    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(self.join_checked(rel)?)?;
        Ok(Box::new(StdHandle(f)))
    }

    fn create_dir(&self, rel: &str) -> io::Result<()> {
        let p = self.join_checked(rel)?;
        match fs::create_dir(&p) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(&p)?.is_dir() => Ok(()),
            r => r,
        }
    }

    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        fs::remove_dir(self.join_checked(rel)?)
    }

    fn remove_file(&self, rel: &str) -> io::Result<()> {
        fs::remove_file(self.join_checked(rel)?)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.join_checked(from)?, self.join_checked(to)?)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        fs::hard_link(self.join_checked(from)?, self.join_checked(to)?)
    }

    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        // `std::fs::copy` clones (`fclonefileat`) on APFS and falls back to `fcopyfile`; on Linux it
        // uses `copy_file_range`, which reflinks on Btrfs and XFS.
        fs::copy(self.join_checked(from)?, self.join_checked(to)?).map(|_| ())
    }

    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        trs_platform::fs::set_modified(&self.join_checked(rel)?, system_time(mtime_ns))
    }

    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        trs_platform::fs::set_exec(&self.join_checked(rel)?, exec)
    }

    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        trs_platform::fs::symlink(target, &self.join_checked(rel)?)
    }

    fn read_link(&self, rel: &str) -> io::Result<String> {
        trs_platform::fs::read_link(&self.join_checked(rel)?).map_err(|e| io::Error::new(e.kind(), format!("{rel}: {e}")))
    }

    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        trs_platform::fs::sync_dir(&self.join_checked(rel)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_are_not_utf8_are_skipped() {
        #[cfg(unix)]
        let bad = std::os::unix::ffi::OsStringExt::from_vec(b"bad\xff".to_vec());
        // An unpaired surrogate: the one kind of Windows name that is not Unicode.
        #[cfg(windows)]
        let bad = std::os::windows::ffi::OsStringExt::from_wide(&[u16::from(b'b'), u16::from(b'a'), u16::from(b'd'), 0xD800]);
        let l = split_names([OsString::from("b"), bad, OsString::from("a")]);
        assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(l.skipped, ["bad\u{FFFD}"]);
    }

    #[test]
    fn times_before_1970_round_trip() {
        assert_eq!(system_time(-1_500_000_000), UNIX_EPOCH - Duration::from_millis(1500));
        assert_eq!(system_time(2_000_000_001), UNIX_EPOCH + Duration::new(2, 1));
    }
}
