//! The one OS call `std` lacks: a write barrier (macOS `fcntl(F_BARRIERFSYNC)`). The only `unsafe` in this crate.
//!
//! On macOS `File::sync_all` is `F_FULLFSYNC`: the drive empties its whole write cache to the medium before it
//! answers, some 5 to 20 ms, whatever was written. `F_BARRIERFSYNC` pushes the file's data to the drive and puts a
//! barrier behind it: everything written later reaches the medium after it, but nothing waits for the medium.
//! The puller uses the barrier for each file and one full flush per commit batch (`pull.rs`). Elsewhere `fsync`
//! is the full flush and the barrier both.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;

/// `F_BARRIERFSYNC` (`<sys/fcntl.h>`).
#[cfg(target_os = "macos")]
const F_BARRIERFSYNC: libc::c_int = 85;

/// The file's data reaches the drive before anything written after this call.
#[cfg(target_os = "macos")]
pub(crate) fn barrier(f: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: `fcntl` with F_BARRIERFSYNC takes no third argument and only reads the descriptor, which `f`
    // keeps open for the call.
    let r = unsafe { libc::fcntl(f.as_raw_fd(), F_BARRIERFSYNC) };
    if r == -1 {
        // A filesystem without barriers (some network or FAT volumes): the full flush does it.
        return f.sync_all();
    }
    Ok(())
}

/// Elsewhere `fsync` gives the ordering (and more).
#[cfg(not(target_os = "macos"))]
pub(crate) fn barrier(f: &File) -> io::Result<()> {
    f.sync_all()
}
