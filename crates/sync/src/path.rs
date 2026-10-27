//! Names on the wire and on disk.
//!
//! * The **wire name** of a path is its NFC form ([`wire_name`]); every device announces NFC.
//! * The **key** of a wire name ([`key`]) is what two names are compared by: NFC, plus case folding when the
//!   folder's filesystem is case-insensitive. Two names with the same key are the same file on that folder.
//! * The **disk name** is whatever the filesystem gave us in a listing, and it is never rewritten. A
//!   file that arrives from the wire is matched to the disk through the key, so an existing file keeps
//!   the spelling it has on this Mac.
//!
//! The macOS pitfall: APFS stores names exactly as written (an app that writes NFD gets NFD back from
//! a listing) but compares them normalization-insensitively, while HFS+ stored every name as NFD. So a
//! listing on a Mac can hold NFD names, and a name typed on Linux arrives as NFC: both have the same
//! key, and the puller writes into the name the disk already has.

use std::io;

use trs_sync_proto::model::{BadPath, STATE_DIR, validate_path};
use unicode_normalization::{UnicodeNormalization, is_nfc_quick};

use crate::fs::Fs;

/// The wire form of a name or path read from disk: NFC.
pub fn wire_name(disk: &str) -> String {
    match is_nfc_quick(disk.chars()) {
        unicode_normalization::IsNormalized::Yes => disk.to_owned(),
        _ => disk.nfc().collect(),
    }
}

/// Is `s` in NFC?
pub fn is_nfc(s: &str) -> bool {
    unicode_normalization::is_nfc(s)
}

/// The comparison key of a wire name or path (`/` is kept, so keys of paths are paths of keys).
///
/// NFC, and when `case_insensitive`, case-folded: each character's lowercase form, plus the handful of
/// characters whose Unicode simple case folding differs from their lowercase form (final sigma, long
/// s, micro sign, the Greek symbol variants; see [`fold_char`]). The gap to *full* case folding is the
/// characters that fold to several (`ß` → `ss`, ligatures such as `ﬁ`): they keep their own key. Where a
/// disk folds further, the puller asks the disk itself (a name `lstat` finds that is not in the listing is
/// a clash), so the second name is refused rather than written over the first.
pub fn key(wire: &str, case_insensitive: bool) -> String {
    let nfc = wire_name(wire);
    if !case_insensitive {
        return nfc;
    }
    let mut folded = String::with_capacity(nfc.len());
    for c in nfc.chars() {
        fold_char(c, &mut folded);
    }
    // Lowercasing can leave a sequence that composes further (a lowercase base before a combining mark).
    wire_name(&folded)
}

/// Appends the case fold of `c` to `out` (the folding [`key`] uses).
pub fn fold_char(c: char, out: &mut String) {
    match special_fold(c) {
        Some(f) => out.push(f),
        None => out.extend(c.to_lowercase()),
    }
}

/// The case fold of `c` when it is one character (else `c` itself): for matching character by
/// character, without allocating.
pub fn fold_one(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    if let Some(f) = special_fold(c) {
        return f;
    }
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(f), None) => f,
        _ => c,
    }
}

/// Characters whose Unicode simple case folding is another lowercase letter, where
/// `char::to_lowercase` keeps them as they are.
fn special_fold(c: char) -> Option<char> {
    Some(match c {
        'ς' => 'σ',
        'ſ' => 's',
        'µ' => 'μ',
        'ϐ' => 'β',
        'ϑ' => 'θ',
        'ϕ' => 'φ',
        'ϖ' => 'π',
        'ϰ' => 'κ',
        'ϱ' => 'ρ',
        'ϵ' => 'ε',
        'ẛ' => 'ṡ',
        '\u{1FBE}' | '\u{0345}' => 'ι',
        _ => return None,
    })
}

/// Why a path from the wire is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadWirePath {
    /// Fails [`validate_path`].
    Path(BadPath),
    /// Not in NFC: a device that normalizes differently, or a forged entry.
    NotNfc,
}

/// Checks a path from the wire: [`validate_path`] and NFC.
pub fn check_wire_path(p: &str) -> Result<(), BadWirePath> {
    validate_path(p).map_err(BadWirePath::Path)?;
    if !is_nfc(p) {
        return Err(BadWirePath::NotNfc);
    }
    Ok(())
}

/// Finds out whether the folder's filesystem is case-insensitive: creates `.seedwell/case-probe.tmp`
/// and looks for `.seedwell/CASE-PROBE.TMP`. The folder marker `.seedwell/` must exist (the probe
/// never creates it: a missing marker means *Folder missing*).
pub fn probe_case_insensitive(fs: &dyn Fs) -> io::Result<bool> {
    let lower = format!("{STATE_DIR}/case-probe.tmp");
    let upper = format!("{STATE_DIR}/CASE-PROBE.TMP");
    // A leftover upper-case probe on a case-sensitive disk would answer "insensitive" wrongly.
    if fs.lstat(&upper)?.is_some() {
        fs.remove_file(&upper)?;
    }
    drop(fs.open_rw(&lower)?);
    let found = fs.lstat(&upper).map(|m| m.is_some());
    let removed = fs.remove_file(&lower);
    let found = found?;
    removed?;
    Ok(found)
}

/// Finds out whether the folder's disk keeps the executable bit: sets and clears it on `.seedwell/exec-probe.tmp`
/// and reads it back each time. exFAT and FAT on a Mac report every file executable, NTFS none; a disk that refuses
/// the change (some network shares) does not keep it either. The folder marker must exist, as for
/// [`probe_case_insensitive`].
pub fn probe_exec_bits(fs: &dyn Fs) -> io::Result<bool> {
    let p = format!("{STATE_DIR}/exec-probe.tmp");
    drop(fs.open_rw(&p)?);
    let mut keeps = true;
    for want in [true, false] {
        match fs.set_exec(&p, want) {
            Ok(()) => keeps &= fs.lstat(&p)?.is_some_and(|m| m.exec == want),
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported) => keeps = false,
            Err(e) => {
                let _ = fs.remove_file(&p);
                return Err(e);
            }
        }
    }
    fs.remove_file(&p)?;
    Ok(keeps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{MemFs, MemFsOptions};

    #[test]
    fn wire_names_are_nfc() {
        let nfd = "A\u{030A}ngstro\u{0308}m";
        assert_eq!(wire_name(nfd), "Ångström");
        assert_eq!(wire_name("plain"), "plain");
        assert!(is_nfc(&wire_name(nfd)) && !is_nfc(nfd));
    }

    #[test]
    fn keys_fold_case_only_when_asked() {
        assert_eq!(key("Ångström/A.TXT", false), "Ångström/A.TXT");
        assert_eq!(key("A\u{030A}ngstro\u{0308}m/A.TXT", true), "ångström/a.txt");
        assert_eq!(key("ΣΟΦΟΣ", true), key("σοφος", true), "final sigma folds like sigma");
        assert_eq!(key("ſ", true), "s");
        assert_eq!(key("Straße", true), "straße", "full folding (ß → ss) is the documented gap");
        // A precomposed capital that lowercases to a base + combining sequence stays NFC.
        assert!(is_nfc(&key("İ", true)));
    }

    #[test]
    fn wire_paths_must_be_nfc_and_valid() {
        assert_eq!(check_wire_path("Æøå/ü"), Ok(()));
        assert_eq!(check_wire_path("u\u{0308}"), Err(BadWirePath::NotNfc));
        assert_eq!(check_wire_path("../x"), Err(BadWirePath::Path(BadPath::Dots)));
    }

    #[test]
    fn the_exec_probe_tells_disks_without_permissions_apart() {
        for (fixed, keeps) in [(None, true), (Some(true), false), (Some(false), false)] {
            let fs = MemFs::with_options(MemFsOptions { fixed_exec: fixed, ..MemFsOptions::default() });
            fs.create_dir(STATE_DIR).unwrap();
            assert_eq!(probe_exec_bits(&fs).unwrap(), keeps, "{fixed:?}");
            assert!(fs.read_dir(STATE_DIR).unwrap().entries.is_empty(), "the probe file is gone");
        }
    }

    #[test]
    fn probe_tells_the_two_kinds_apart() {
        for ci in [false, true] {
            let fs = MemFs::with_options(MemFsOptions { case_insensitive: ci, ..MemFsOptions::default() });
            assert!(probe_case_insensitive(&fs).is_err(), "no marker, no probe");
            fs.create_dir(STATE_DIR).unwrap();
            assert_eq!(probe_case_insensitive(&fs).unwrap(), ci);
            assert!(fs.read_dir(STATE_DIR).unwrap().entries.is_empty(), "the probe cleans up");
        }
    }
}
