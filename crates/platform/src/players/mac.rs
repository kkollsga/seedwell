//! The Mac's players: found by bundle identifier through LaunchServices, else by name in the Applications folders;
//! whether each runs on this CPU from its executable's Mach-O header.

use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};

use objc2::rc::{autoreleasepool, Retained};
use objc2_foundation::{NSArray, NSBundle, NSString, NSURL};

use super::{architectures, can_run, spec_in, Arch, KnownPlayer, Player, MAC_PLAYERS};

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    /// `CFArrayRef LSCopyApplicationURLsForBundleIdentifier(CFStringRef, CFErrorRef *)`: the apps with this bundle
    /// identifier, the preferred one first; `NULL` when there is none. The caller owns the array.
    fn LSCopyApplicationURLsForBundleIdentifier(bundle_id: *const c_void, error: *mut *mut c_void) -> *mut c_void;
}

const ROSETTA_RUNTIME: &str = "/Library/Apple/usr/libexec/oah/libRosettaRuntime";

/// Apple silicon (`hw.optional.arm64` is 1 there, also for a process that runs translated).
fn apple_silicon() -> bool {
    let mut v: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    // SAFETY: the name is NUL-terminated; `v` holds the `size` bytes the call may write.
    let r = unsafe { libc::sysctlbyname(c"hw.optional.arm64".as_ptr(), (&raw mut v).cast(), &mut size, std::ptr::null_mut(), 0) };
    r == 0 && v == 1
}

fn rosetta() -> bool {
    Path::new(ROSETTA_RUNTIME).exists()
}

/// What the machine can run, in a line for QA ("Apple silicon, Rosetta installed").
pub fn host_line() -> String {
    format!(
        "{}, Rosetta {}",
        if apple_silicon() { "Apple silicon" } else { "Intel" },
        if rosetta() { "installed" } else { "not installed" }
    )
}

/// The first app LaunchServices has for `bundle_id`.
fn launch_services(bundle_id: &str) -> Option<PathBuf> {
    let id = NSString::from_str(bundle_id);
    // SAFETY: an NSString is a CFString (toll-free bridged) and lives through the call; a non-NULL result is a +1
    // CFArray of CFURLs, which are NSArray and NSURL, and is released by the `Retained` that takes it.
    let array: Option<Retained<NSArray<NSURL>>> = unsafe {
        let raw = LSCopyApplicationURLsForBundleIdentifier(Retained::as_ptr(&id).cast(), std::ptr::null_mut());
        Retained::from_raw(raw.cast::<NSArray<NSURL>>())
    };
    let url = array?.firstObject()?;
    url.path().map(|p| PathBuf::from(p.to_string()))
}

/// The app of `spec`: LaunchServices by each of its bundle identifiers, then its names in /Applications and
/// ~/Applications.
fn locate(spec: &KnownPlayer) -> Option<PathBuf> {
    if let Some(p) = spec.ids.iter().find_map(|id| launch_services(id)) {
        return Some(p);
    }
    let mut folders = vec![PathBuf::from("/Applications")];
    if let Some(home) = crate::dirs::home() {
        folders.push(home.join("Applications"));
    }
    folders.iter().flat_map(|f| spec.files.iter().map(move |n| f.join(n))).find(|p| p.exists())
}

/// The bundle's main executable and its identifier.
fn bundle(app: &Path) -> (Option<PathBuf>, Option<String>) {
    let Some(b) = NSBundle::bundleWithPath(&NSString::from_str(&app.to_string_lossy())) else { return (None, None) };
    (b.executablePath().map(|p| PathBuf::from(p.to_string())), b.bundleIdentifier().map(|s| s.to_string()))
}

/// The slices of the app's executable; `None` when it cannot be read or is not Mach-O.
fn archs(executable: Option<&Path>) -> Option<Vec<Arch>> {
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(executable?).ok()?.take(4096).read_to_end(&mut head).ok()?;
    architectures(&head)
}

fn found(spec: &KnownPlayer, app: PathBuf, executable: Option<PathBuf>, host: (bool, bool)) -> Player {
    let a = archs(executable.as_deref());
    let runs = can_run(a.as_deref(), host.0, host.1);
    Player::found(spec, app, a, runs)
}

/// The known players installed here, in the order of "Automatic". Reads a few file headers: call it off the main
/// thread now and then, not for every menu.
pub fn installed() -> Vec<Player> {
    autoreleasepool(|_| {
        let host = (apple_silicon(), rosetta());
        MAC_PLAYERS
            .iter()
            .filter_map(|spec| {
                let app = locate(spec)?;
                let (exe, _) = bundle(&app);
                Some(found(spec, app, exe, host))
            })
            .collect()
    })
}

/// The known player at `app` (an app bundle), whether or not LaunchServices lists it: what QA's `play-app` starts.
/// `None` when it is not one Seedwell knows.
pub fn player_at(app: &Path) -> Option<Player> {
    autoreleasepool(|_| {
        let (exe, id) = bundle(app);
        let spec = spec_in(MAC_PLAYERS, &id?)?;
        Some(found(spec, app.to_path_buf(), exe, (apple_silicon(), rosetta())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quicktime_is_found_and_runs() {
        // Every Mac has QuickTime Player; its bundle is the system's own.
        let players = installed();
        let qt = players.iter().find(|p| p.id == "com.apple.QuickTimePlayerX").expect("QuickTime Player");
        assert!(qt.path.ends_with("QuickTime Player.app") && qt.runs && qt.mp4_only);
        assert_eq!(player_at(&qt.path).map(|p| p.id), Some(qt.id.clone()));
        assert!(player_at(Path::new("/System/Applications/Calculator.app")).is_none(), "not a player");
        assert!(host_line().contains("Rosetta"));
    }
}
