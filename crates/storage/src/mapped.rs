//! A global allocator that maps big blocks straight from the kernel.
//!
//! Why: on macOS the system allocator keeps what it has freed. Start-up verification and parsing
//! allocate and free hundreds of buffers of 128 KiB to a few MiB (the 256 KiB hashing scratch of
//! every recheck, piece buffers, hash tables that grew by doubling), and every freed block stays
//! resident inside the allocator's regions next to the small long-lived records: a seeder with a
//! 2 MB live heap kept 8 to 11 MB of footprint for as long as it ran, and `malloc_zone_pressure_relief`
//! (see [`crate::release_free_memory`]) returned 0 bytes of it. A block of at least [`MAP_MIN`] bytes is
//! an anonymous `mmap` here and a `munmap` when freed, so it leaves the footprint the moment it is
//! dropped and never pins a page that holds a small record. Measured (engine README, Memory):
//! about 3 MB less footprint at every torrent count, and a reproducible one; the per-torrent slope after a restart from
//! 11 KiB (noise) to 3.6 KiB.
//!
//! The route depends only on the `Layout`, so `dealloc` and `realloc` always find the allocator a block
//! came from. Mapped blocks are page aligned (16 KiB on Apple silicon, 4 KiB on Intel) and zero filled, which makes
//! `vec![0; n]` of a big buffer free of the memset. On other platforms the type is the system allocator
//! unchanged (glibc already serves such blocks with `mmap` and `malloc_trim` returns the rest).
//!
//! Install it in a binary with `#[global_allocator] static A: trs_storage::MappedAlloc = trs_storage::MappedAlloc;`.

use std::alloc::{GlobalAlloc, Layout, System};

/// Blocks of at least this many bytes (alignment up to 4 KiB) are mapped directly.
pub const MAP_MIN: usize = 128 * 1024;

/// The allocator to install with `#[global_allocator]`.
pub struct MappedAlloc;

/// The page size the mapped path rounds lengths to: the largest on supported Macs (Apple silicon), a
/// multiple of the 4 KiB pages of Intel Macs.
#[cfg(target_os = "macos")]
const PAGE: usize = 16 * 1024;

/// Alignment a mapping always has (the smallest page size of any supported Mac).
#[cfg(target_os = "macos")]
const MAP_ALIGN: usize = 4 * 1024;

#[cfg(target_os = "macos")]
#[inline]
fn mapped(l: &Layout) -> bool {
    l.size() >= MAP_MIN && l.align() <= MAP_ALIGN
}

#[cfg(target_os = "macos")]
#[inline]
fn map_len(size: usize) -> usize {
    size.next_multiple_of(PAGE)
}

/// An anonymous private mapping of `size` bytes rounded up to a page (null on failure).
#[cfg(target_os = "macos")]
fn map(size: usize) -> *mut u8 {
    // SAFETY: an anonymous mapping with no address hint touches no existing memory.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len(size),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED { std::ptr::null_mut() } else { p.cast() }
}

// SAFETY: every method either forwards to `System` or maps/unmaps whole pages. The route is a pure
// function of the `Layout` (size, align), which `GlobalAlloc`'s contract makes the caller pass
// unchanged to `dealloc`/`realloc`, so a block is always freed by the allocator that made it.
unsafe impl GlobalAlloc for MappedAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        #[cfg(target_os = "macos")]
        if mapped(&l) {
            return map(l.size());
        }
        // SAFETY: same contract as ours.
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        #[cfg(target_os = "macos")]
        if mapped(&l) {
            return map(l.size());
        }
        // SAFETY: same contract as ours.
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        #[cfg(target_os = "macos")]
        if mapped(&l) {
            // SAFETY: `p` came from `map(l.size())`, which mapped `map_len(l.size())` bytes.
            unsafe { libc::munmap(p.cast(), map_len(l.size())) };
            return;
        }
        // SAFETY: same contract as ours.
        unsafe { System.dealloc(p, l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: the caller guarantees `new` is a valid size for `l.align()`.
            let nl = unsafe { Layout::from_size_align_unchecked(new, l.align()) };
            match (mapped(&l), mapped(&nl)) {
                (false, false) => {}
                // Same number of pages: nothing to do.
                (true, true) if map_len(l.size()) == map_len(new) => return p,
                _ => {
                    // SAFETY: `nl` is valid, `p` holds `l.size()` bytes and is freed once, below.
                    let q = unsafe { self.alloc(nl) };
                    if !q.is_null() {
                        unsafe {
                            std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                            self.dealloc(p, l);
                        }
                    }
                    return q;
                }
            }
        }
        // SAFETY: same contract as ours.
        unsafe { System.realloc(p, l, new) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(p: *mut u8, n: usize, v: u8) {
        for i in (0..n).step_by(4093) {
            // SAFETY: tests only write inside blocks they own.
            unsafe { *p.add(i) = v };
        }
    }

    #[test]
    fn blocks_of_every_route_round_trip_through_realloc() {
        let a = MappedAlloc;
        for &(from, to) in &[
            (100usize, 200usize),
            (100, MAP_MIN),
            (MAP_MIN, 100),
            (MAP_MIN, MAP_MIN + 1),
            (MAP_MIN + 5, MAP_MIN + 6),
            (MAP_MIN, 4 * MAP_MIN),
            (3 * MAP_MIN, MAP_MIN + 17),
            (MAP_MIN - 1, MAP_MIN + 1),
        ] {
            let l = Layout::from_size_align(from, 8).unwrap();
            // SAFETY: layouts are valid; every pointer is used with the layout it was made with.
            unsafe {
                let p = a.alloc(l);
                assert!(!p.is_null());
                fill(p, from, 0x5a);
                *p = 1;
                *p.add(from - 1) = 2;
                let q = a.realloc(p, l, to);
                assert!(!q.is_null());
                assert_eq!(*q, 1);
                if from <= to {
                    assert_eq!(*q.add(from - 1), 2);
                }
                a.dealloc(q, Layout::from_size_align(to, 8).unwrap());
            }
        }
    }

    #[test]
    fn big_zeroed_blocks_are_zero_and_page_aligned_on_macos() {
        let a = MappedAlloc;
        let l = Layout::from_size_align(MAP_MIN + 123, 16).unwrap();
        // SAFETY: valid layout; the block is freed with the same layout.
        unsafe {
            let p = a.alloc_zeroed(l);
            assert!(!p.is_null());
            assert!((0..l.size()).step_by(997).all(|i| *p.add(i) == 0));
            #[cfg(target_os = "macos")]
            assert_eq!(p as usize % 4096, 0);
            a.dealloc(p, l);
        }
    }

    #[test]
    fn a_vec_of_a_few_mib_grows_and_frees() {
        // Not the global allocator in a test binary; exercise the type through a local wrapper.
        let a = MappedAlloc;
        let mut l = Layout::from_size_align(MAP_MIN, 1).unwrap();
        // SAFETY: each step reallocs with the layout of the previous block.
        unsafe {
            let mut p = a.alloc(l);
            for _ in 0..6 {
                let n = l.size() * 2;
                *p.add(l.size() - 1) = 7;
                p = a.realloc(p, l, n);
                assert_eq!(*p.add(l.size() - 1), 7);
                l = Layout::from_size_align(n, 1).unwrap();
            }
            a.dealloc(p, l);
        }
    }
}
