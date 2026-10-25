//! RFC 4648 base32 (`A–Z2–7`), no padding: the text form of device ids and folder keys.
//!
//! Decoding is lenient because people copy these strings out of mail, chat and paper: case, whitespace and `-`
//! are ignored. Bits left over in the last character are ignored too (the check characters and check bytes on
//! top of base32 are what catch typos).

/// The RFC 4648 alphabet.
pub const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A character outside the alphabet, or a length no byte string encodes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base32Error {
    /// This character is not base32 (and not ignorable).
    Character(char),
    /// The number of characters cannot come from whole bytes (1, 3 or 6 left over after groups of 8).
    Length,
}

impl std::fmt::Display for Base32Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Base32Error::Character(c) => write!(f, "{c:?} is not a base32 character"),
            Base32Error::Length => f.write_str("the base32 text has an impossible length"),
        }
    }
}

impl std::error::Error for Base32Error {}

/// `bytes` in base32, upper case, no padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The value of one base32 character (either case), `None` for anything else.
pub(crate) fn value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a'),
        b'2'..=b'7' => Some(c - b'2' + 26),
        _ => None,
    }
}

/// Characters a person may put between groups: skipped when decoding.
pub(crate) fn ignorable(c: char) -> bool {
    c == '-' || c.is_whitespace()
}

/// Decodes base32 text, ignoring case, whitespace and `-`.
pub fn decode(text: &str) -> Result<Vec<u8>, Base32Error> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut acc, mut bits, mut chars) = (0u32, 0u32, 0usize);
    for c in text.chars() {
        if ignorable(c) {
            continue;
        }
        let v = u8::try_from(c).ok().and_then(value).ok_or(Base32Error::Character(c))?;
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        chars += 1;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    // 8 characters carry 5 bytes; a tail of 1, 3 or 6 characters cannot end on a byte boundary.
    if matches!(chars % 8, 1 | 3 | 6) {
        return Err(Base32Error::Length);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_without_padding() {
        let cases = [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ];
        for (plain, coded) in cases {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn decoding_is_lenient_about_case_spaces_and_dashes() {
        assert_eq!(decode("mzxw-6ytb oi").unwrap(), b"foobar");
        assert_eq!(decode("MZXW\n6YTB\tOI").unwrap(), b"foobar");
    }

    #[test]
    fn decoding_refuses_foreign_characters_and_impossible_lengths() {
        assert_eq!(decode("MZ1"), Err(Base32Error::Character('1')));
        assert_eq!(decode("MZ="), Err(Base32Error::Character('=')));
        assert_eq!(decode("MZXW6YTBO"), Err(Base32Error::Length));
        assert_eq!(decode("M"), Err(Base32Error::Length));
        assert_eq!(decode("Mæ"), Err(Base32Error::Character('æ')));
    }

    #[test]
    fn every_length_roundtrips() {
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..70 {
            assert_eq!(decode(&encode(&data[..n])).unwrap(), &data[..n], "{n} bytes");
        }
    }
}
