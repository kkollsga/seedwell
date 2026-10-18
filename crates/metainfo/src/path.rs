//! Path-component sanitization. **Security-critical**: a hostile torrent must
//! never be able to name a file outside its download directory.
//!
//! Policy: hostile input is *rejected* (not repaired) because silently rewriting
//! `..` to something else would make two clients disagree about where data goes.
//! Only benign differences are repaired: invalid UTF-8 becomes U+FFFD and `:`
//! becomes `_` (Finder shows `:` as `/`, and classic HFS paths use it as a
//! separator).

use core::fmt;
use std::borrow::Cow;

/// Longest allowed component in bytes (APFS/HFS+ limit is 255 UTF-8 bytes).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Deepest allowed relative path.
pub const MAX_PATH_DEPTH: usize = 64;
/// Longest allowed relative path (all components plus separators) in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// Why a path component or path was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// Empty component (this is also how an absolute path `/a` appears).
    Empty,
    /// Component is `.`.
    CurrentDir,
    /// Component is `..`.
    ParentDir,
    /// Component contains a NUL byte.
    Nul,
    /// Component contains `/`; a leading one would make the path absolute.
    Separator,
    /// Component longer than 255 bytes (after any lossy UTF-8 repair).
    TooLong,
    /// A file path with zero components.
    NoComponents,
    /// More than [`MAX_PATH_DEPTH`] components.
    TooDeep,
    /// Whole path longer than [`MAX_PATH_BYTES`].
    PathTooLong,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PathError::Empty => "empty path component (or absolute path)",
            PathError::CurrentDir => "`.` path component",
            PathError::ParentDir => "`..` path component",
            PathError::Nul => "NUL in path component",
            PathError::Separator => "`/` inside path component",
            PathError::TooLong => "path component longer than 255 bytes",
            PathError::NoComponents => "file path has no components",
            PathError::TooDeep => "path too deep",
            PathError::PathTooLong => "path too long",
        })
    }
}

impl std::error::Error for PathError {}

/// Repairs applied to a component that was accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Repairs {
    /// Invalid UTF-8 was replaced with U+FFFD.
    pub lossy: bool,
    /// `:` was replaced with `_`.
    pub colon: bool,
}

/// Validates and normalizes one raw path component. Borrows when nothing needs
/// changing, which is the overwhelmingly common case.
pub fn sanitize_component(raw: &[u8]) -> Result<(Cow<'_, str>, Repairs), PathError> {
    if raw.is_empty() {
        return Err(PathError::Empty);
    }
    if raw.contains(&0) {
        return Err(PathError::Nul);
    }
    if raw.contains(&b'/') {
        return Err(PathError::Separator);
    }
    if raw == b"." {
        return Err(PathError::CurrentDir);
    }
    if raw == b".." {
        return Err(PathError::ParentDir);
    }
    // Cheap reject before any allocation. Lossy repair can only grow a component
    // (1 invalid byte -> 3 bytes), so this never rejects something valid.
    if raw.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }
    let mut repairs = Repairs::default();
    let mut text = String::from_utf8_lossy(raw);
    if matches!(text, Cow::Owned(_)) {
        repairs.lossy = true;
    }
    if text.contains(':') {
        repairs.colon = true;
        text = Cow::Owned(text.replace(':', "_"));
    }
    if text.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }
    Ok((text, repairs))
}
