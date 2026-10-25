//! Names for conflict copies and archived versions.
//!
//! A conflict copy sits next to the file it lost to, readable in Finder:
//! `Report (conflict, MacBook Air, 2026-10-25 14.32).docx`. The time is the losing version's mtime in
//! the device's local time (passed in already formatted, see [`crate::clock::Clock`]). Two devices that keep
//! the same losing version usually pick the same name, and then the two copies are one file: same
//! path, same content, no new conflict.

/// The last component's stem and extension: `a/b/Report.docx` → (`a/b/Report`, `.docx`). A leading dot
/// is part of the name (`.profile` has no extension), and so is a trailing one.
fn split_ext(path: &str) -> (&str, &str) {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let name = &path[name_start..];
    match name.rfind('.') {
        // `.28)` in `a (conflict, X, 2026-10-25 14.28)` is no extension: a conflict copy of a conflict
        // copy would get its suffix inside the first one's parentheses.
        Some(i) if i > 0 && i + 1 < name.len() && !name[i..].contains([' ', ')']) => path.split_at(name_start + i),
        _ => (path, ""),
    }
}

/// Bytes a name component may have on APFS and ext4.
const MAX_NAME: usize = 255;

/// Cuts `stem`'s last component so that `stem + suffix + ext` fits [`MAX_NAME`], on a char boundary.
fn fit(stem: &str, suffix: &str, ext: &str) -> String {
    let name_start = stem.rfind('/').map_or(0, |i| i + 1);
    let budget = MAX_NAME.saturating_sub(suffix.len() + ext.len());
    let mut end = stem.len().min(name_start + budget);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..end])
}

/// Device names go into file names: no `/`, no control characters, no leading dot, at most 40 chars.
fn clean_device(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == ':' || c.is_control() { '-' } else { c })
        .take(40)
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    if trimmed.is_empty() { "unknown device".to_string() } else { trimmed.to_string() }
}

/// The conflict copy's path for `path`, losing version made by `device` at `stamp`
/// (e.g. `2026-10-25 14.32`). `n` > 1 numbers further copies when that path is taken.
pub fn conflict_path(path: &str, device: &str, stamp: &str, n: u32) -> String {
    let (stem, ext) = split_ext(path);
    let num = if n > 1 { format!(" {n}") } else { String::new() };
    let suffix = format!(" (conflict, {}, {stamp}{num})", clean_device(device));
    fit(stem, &suffix, ext)
}

/// Is this the name of a conflict copy? (Used only for display; the index records `conflict_of`.)
pub fn looks_like_conflict(path: &str) -> bool {
    parse_conflict(path).is_some()
}

/// A conflict copy's path read back: what [`conflict_path`] was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictName {
    /// The path it is a copy of.
    pub original: String,
    /// The losing version's device, as cleaned for the name.
    pub device: String,
    /// `2026-10-25 14.32`.
    pub stamp: String,
}

/// Reads a [`conflict_path`] back (`None` for any other name). The device name may hold `, ` itself;
/// the stamp is the 16 characters before the closing parenthesis (and the copy number, if any).
pub fn parse_conflict(path: &str) -> Option<ConflictName> {
    let (stem, ext) = split_ext(path);
    let inner = stem.strip_suffix(')')?;
    let start = inner.rfind(" (conflict, ")?;
    let (device, rest) = inner[start + " (conflict, ".len()..].rsplit_once(", ")?;
    let stamp = rest.get(..16)?;
    let number = &rest[16..];
    let numbered = number.strip_prefix(' ').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !(number.is_empty() || numbered) || !stamp.as_bytes()[0].is_ascii_digit() || device.is_empty() {
        return None;
    }
    Some(ConflictName { original: format!("{}{ext}", &inner[..start]), device: device.to_string(), stamp: stamp.to_string() })
}

/// Where an archived version goes: `.seedwell/Archive/<path>` with the time before the extension,
/// `Report~20261025-143200.docx`, so Finder still knows the kind.
pub fn archive_path(path: &str, compact_stamp: &str) -> String {
    let (stem, ext) = split_ext(path);
    let suffix = format!("~{compact_stamp}");
    format!("{}/Archive/{}", trs_sync_proto::model::STATE_DIR, fit(stem, &suffix, ext))
}

/// What an archived version (its path below `.seedwell/Archive/`) is a version of, and when it was
/// archived (Unix ns). `None` for a name [`archive_path`] did not make. A long name cut to fit gives a
/// shortened path: only used to group versions of one file.
pub fn archived_of(below_archive: &str) -> Option<(String, i64)> {
    let name_start = below_archive.rfind('/').map_or(0, |i| i + 1);
    let name = &below_archive[name_start..];
    let tilde = name.rfind('~')?;
    let rest = &name[tilde + 1..];
    let at = crate::clock::parse_compact_stamp(rest.get(..15)?)?;
    let mut tail = &rest[15..];
    // `-2`, `-3`: further versions archived in the same second.
    if let Some(n) = tail.strip_prefix('-') {
        let digits = n.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        tail = &n[digits..];
    }
    if !(tail.is_empty() || tail.starts_with('.')) {
        return None;
    }
    Some((format!("{}{}{tail}", &below_archive[..name_start], &name[..tilde]), at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names_read_well() {
        assert_eq!(
            conflict_path("Docs/Report.docx", "MacBook Air", "2026-10-25 14.32", 1),
            "Docs/Report (conflict, MacBook Air, 2026-10-25 14.32).docx"
        );
        assert_eq!(conflict_path("notes", "iMac", "2026-10-25 14.32", 2), "notes (conflict, iMac, 2026-10-25 14.32 2)");
        assert_eq!(conflict_path(".profile", "iMac", "t", 1), ".profile (conflict, iMac, t)");
        assert_eq!(conflict_path("a.tar.gz", "iMac", "t", 1), "a.tar (conflict, iMac, t).gz");
        assert_eq!(conflict_path("dir.d/file", "x", "t", 1), "dir.d/file (conflict, x, t)");
        assert!(looks_like_conflict("Docs/Report (conflict, MacBook Air, 2026-10-25 14.32).docx"));
        assert!(!looks_like_conflict("Docs/Report.docx"));
    }

    #[test]
    fn conflict_names_are_read_back() {
        let stamp = "2026-10-25 14.32";
        for (path, device, n) in [
            ("Docs/Report.docx", "MacBook Air", 1),
            ("notes", "Kim, the Mac", 2),
            (".profile", "iMac", 12),
            ("a.tar.gz", "x", 1),
            ("a (conflict, X, 2026-10-25 14.28)", "Y", 1),
        ] {
            let copy = conflict_path(path, device, stamp, n);
            let got = parse_conflict(&copy).unwrap_or_else(|| panic!("{copy}"));
            assert_eq!(got, ConflictName { original: path.into(), device: device.into(), stamp: stamp.into() }, "{copy}");
        }
        for not in ["Report.txt", "x (conflict, y).txt", "x (conflict, y, 2026-10-25 14.32 z)", "x (conflict, , 2026-10-25 14.32)"] {
            assert_eq!(parse_conflict(not), None, "{not}");
        }
    }

    #[test]
    fn device_names_cannot_break_paths() {
        let p = conflict_path("a.txt", "../evil/\u{7}name", "t", 1);
        assert_eq!(p, "a (conflict, -evil--name, t).txt");
        assert!(!p.contains('/'));
        assert_eq!(conflict_path("a", "   ", "t", 1), "a (conflict, unknown device, t)");
    }

    #[test]
    fn long_names_are_cut_to_fit_on_a_char_boundary() {
        let long = format!("dir/{}.txt", "æ".repeat(200));
        let p = conflict_path(&long, "Device", "2026-10-25 14.32", 1);
        let name = p.rsplit('/').next().unwrap();
        assert!(name.len() <= MAX_NAME, "{}", name.len());
        assert!(name.ends_with("(conflict, Device, 2026-10-25 14.32).txt"));
    }

    #[test]
    fn archive_keeps_the_extension() {
        assert_eq!(archive_path("a/Report.docx", "20261025-143200"), ".seedwell/Archive/a/Report~20261025-143200.docx");
        let at = crate::clock::parse_compact_stamp("20261025-143200").unwrap();
        assert_eq!(archived_of("a/Report~20261025-143200.docx"), Some(("a/Report.docx".into(), at)));
        assert_eq!(archived_of("notes~20261025-143200-3"), Some(("notes".into(), at)));
        assert_eq!(archived_of("a/Report.docx"), None);
        assert_eq!(archived_of("x~20261025-143200junk"), None);
    }
}
