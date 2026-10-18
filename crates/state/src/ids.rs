//! Identifier syntax shared by rules and tracker profiles.

/// Rule and profile ids: 1-64 characters of `a-z 0-9 - _`, starting with a letter or digit.
pub fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        for ok in ["a", "tl", "tracker-z_2", "9lives"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "-x", "_x", "Upper", "a b", "ä", &"x".repeat(65), "a/b", "a.b"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }
}
