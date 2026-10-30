//! The one place in Seedwell's core that names an operating system (docs/CROSS-PLATFORM.md §2).
//!
//! Every `cfg(unix)` / `cfg(windows)` split of the core lives here, behind functions that mean the
//! same thing on every system. Code outside this crate asks for "the volume of this path" or
//! "read these bytes at this offset", never for `st_dev` or `pread`.

pub mod dirs;
pub mod fs;
pub mod ipc;
pub mod net;
pub mod proc;
pub mod signal;
pub mod update;

/// What this system calls the computer in user-visible text: "Mac", "PC" on Windows, else "computer".
/// A literal, so it can be used inside `concat!` (and so in `format!` strings via `concat!`).
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! computer {
    () => {
        "Mac"
    };
}

/// What this system calls the computer in user-visible text: "Mac", "PC" on Windows, else "computer".
/// A literal, so it can be used inside `concat!` (and so in `format!` strings via `concat!`).
#[cfg(windows)]
#[macro_export]
macro_rules! computer {
    () => {
        "PC"
    };
}

/// What this system calls the computer in user-visible text: "Mac", "PC" on Windows, else "computer".
/// A literal, so it can be used inside `concat!` (and so in `format!` strings via `concat!`).
#[cfg(not(any(target_os = "macos", windows)))]
#[macro_export]
macro_rules! computer {
    () => {
        "computer"
    };
}

/// The same word as [`computer!`], for `format!("... {} ...", COMPUTER)` and the like.
pub const COMPUTER: &str = computer!();

/// What this system calls its file manager in user-visible text ("Show in Finder"): "Finder" on the Mac, "File
/// Explorer" on Windows, else "Files". A literal, like [`computer!`].
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! file_manager {
    () => {
        "Finder"
    };
}

/// What this system calls its file manager in user-visible text ("Show in Finder"): "Finder" on the Mac, "File
/// Explorer" on Windows, else "Files". A literal, like [`computer!`].
#[cfg(windows)]
#[macro_export]
macro_rules! file_manager {
    () => {
        "File Explorer"
    };
}

/// What this system calls its file manager in user-visible text ("Show in Finder"): "Finder" on the Mac, "File
/// Explorer" on Windows, else "Files". A literal, like [`computer!`].
#[cfg(not(any(target_os = "macos", windows)))]
#[macro_export]
macro_rules! file_manager {
    () => {
        "Files"
    };
}

/// What this system calls the place deleted things go in user-visible text ("Move to Trash"): "Trash" on the Mac,
/// "Recycle Bin" on Windows, else "Trash". A literal, like [`computer!`]. [`fs::move_to_trash`] puts things there.
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! trash {
    () => {
        "Trash"
    };
}

/// What this system calls the place deleted things go in user-visible text ("Move to Trash"): "Trash" on the Mac,
/// "Recycle Bin" on Windows, else "Trash". A literal, like [`computer!`]. [`fs::move_to_trash`] puts things there.
#[cfg(windows)]
#[macro_export]
macro_rules! trash {
    () => {
        "Recycle Bin"
    };
}

/// What this system calls the place deleted things go in user-visible text ("Move to Trash"): "Trash" on the Mac,
/// "Recycle Bin" on Windows, else "Trash". A literal, like [`computer!`]. [`fs::move_to_trash`] puts things there.
#[cfg(not(any(target_os = "macos", windows)))]
#[macro_export]
macro_rules! trash {
    () => {
        "Trash"
    };
}

/// The same word as [`trash!`], for `format!` and the like.
pub const TRASH: &str = trash!();

/// What the key that confirms is called in user-visible text ("Remove (Return)"): "Return" on the Mac, "Enter"
/// elsewhere. A literal, like [`computer!`].
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! return_key {
    () => {
        "Return"
    };
}

/// What the key that confirms is called in user-visible text ("Remove (Return)"): "Return" on the Mac, "Enter"
/// elsewhere. A literal, like [`computer!`].
#[cfg(not(target_os = "macos"))]
#[macro_export]
macro_rules! return_key {
    () => {
        "Enter"
    };
}

/// What the Shift key is called in user-visible text ("Hold ⇧ to keep the file"): "⇧" on the Mac, "Shift" elsewhere.
/// A literal, like [`computer!`].
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! shift_key {
    () => {
        "⇧"
    };
}

/// What the Shift key is called in user-visible text ("Hold ⇧ to keep the file"): "⇧" on the Mac, "Shift" elsewhere.
/// A literal, like [`computer!`].
#[cfg(not(target_os = "macos"))]
#[macro_export]
macro_rules! shift_key {
    () => {
        "Shift"
    };
}

/// A keyboard shortcut in user-visible text ("Settings (⌘,)"): the key with the system's command key, "⌘," on the
/// Mac and "Ctrl+," elsewhere; `shortcut!(alt "L")` adds the Option / Alt key: "⌥⌘L", "Ctrl+Alt+L". A literal, like
/// [`computer!`].
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! shortcut {
    (alt $key:literal) => {
        concat!("⌥⌘", $key)
    };
    ($key:literal) => {
        concat!("⌘", $key)
    };
}

/// A keyboard shortcut in user-visible text ("Settings (⌘,)"): the key with the system's command key, "⌘," on the
/// Mac and "Ctrl+," elsewhere; `shortcut!(alt "L")` adds the Option / Alt key: "⌥⌘L", "Ctrl+Alt+L". A literal, like
/// [`computer!`].
#[cfg(not(target_os = "macos"))]
#[macro_export]
macro_rules! shortcut {
    (alt $key:literal) => {
        concat!("Ctrl+Alt+", $key)
    };
    ($key:literal) => {
        concat!("Ctrl+", $key)
    };
}

/// What [`fs::move_to_trash`] does with a folder that cannot simply be renamed into the Trash (`OtherVolume::Refuse`),
/// in user-visible text after "permission full; ": with a Trash folder (Linux's home trash, the `trash_dir` setting)
/// one on another volume is refused; the Mac's own Trash keeps it in the Trash of its own volume; on Windows every
/// drive has a Recycle Bin of its own, and what is refused is a folder that it would delete at once instead of
/// keeping. A literal, like [`computer!`].
#[cfg(target_os = "macos")]
#[macro_export]
macro_rules! trash_folder_rule {
    () => {
        concat!("a folder on another disk goes to that disk's own ", $crate::trash!(), "; with trash_dir set, one on another volume than that folder is refused")
    };
}

/// What [`fs::move_to_trash`] does with a folder that cannot simply be renamed into the Trash (`OtherVolume::Refuse`),
/// in user-visible text after "permission full; ": with a Trash folder (Linux's home trash, the `trash_dir` setting)
/// one on another volume is refused; the Mac's own Trash keeps it in the Trash of its own volume; on Windows every
/// drive has a Recycle Bin of its own, and what is refused is a folder that it would delete at once instead of
/// keeping. A literal, like [`computer!`].
#[cfg(windows)]
#[macro_export]
macro_rules! trash_folder_rule {
    () => {
        concat!("refused for a folder the ", $crate::trash!(), " would delete at once instead of keeping, such as one on a network drive or one too large for it")
    };
}

/// What [`fs::move_to_trash`] does with a folder that cannot simply be renamed into the Trash (`OtherVolume::Refuse`),
/// in user-visible text after "permission full; ": with a Trash folder (Linux's home trash, the `trash_dir` setting)
/// one on another volume is refused; the Mac's own Trash keeps it in the Trash of its own volume; on Windows every
/// drive has a Recycle Bin of its own, and what is refused is a folder that it would delete at once instead of
/// keeping. A literal, like [`computer!`].
#[cfg(not(any(target_os = "macos", windows)))]
#[macro_export]
macro_rules! trash_folder_rule {
    () => {
        concat!("refused for a folder on another volume than the ", $crate::trash!())
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortcuts_name_the_systems_own_keys() {
        #[cfg(target_os = "macos")]
        assert_eq!((shortcut!(","), shortcut!(alt "L")), ("⌘,", "⌥⌘L"));
        #[cfg(not(target_os = "macos"))]
        assert_eq!((shortcut!(","), shortcut!(alt "L")), ("Ctrl+,", "Ctrl+Alt+L"));
    }
}
