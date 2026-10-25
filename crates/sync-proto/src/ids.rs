//! Device and folder identities, and their text form.
//!
//! The text form is Syncthing's device id format, so people who know Syncthing recognise it and the same habits
//! (reading it out in groups, comparing the first group) carry over:
//!
//! 1. base32 of the 32 bytes (52 characters);
//! 2. four groups of 13, each followed by a Luhn mod-32 check character (56 characters);
//! 3. shown as eight groups of 7 joined by `-`, e.g. `MFZWI3D-BONSGYC-YLTMRWG-C43ENR5-QXGZDMM-FZWI3DP-BONSGYY-LTMRWAD`.
//!
//! Parsing ignores case, whitespace and `-`, reads `0`, `1` and `8` as `O`, `I` and `B` (as Syncthing does: they
//! are not in the alphabet, so a person who wrote one meant the letter), and requires every check character.

use std::fmt;
use std::str::FromStr;

use ring::digest::{SHA256, digest};

use crate::base32;

/// A device: SHA-256 of its Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub [u8; 32]);

/// The first 64 bits of a [`DeviceId`] (big-endian): the key of a device's counter in a version vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShortId(pub u64);

/// A folder: SHA-256 of its signing public key. Shared, but not a secret on its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(pub [u8; 32]);

impl DeviceId {
    /// The id of the device with this Ed25519 public key.
    pub fn of_public_key(pk: &[u8; 32]) -> Self {
        DeviceId(sha256(pk))
    }

    /// Its short id.
    pub fn short(&self) -> ShortId {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[..8]);
        ShortId(u64::from_be_bytes(b))
    }
}

impl FolderId {
    /// The id of the folder with this signing public key.
    pub fn of_public_key(pk: &[u8; 32]) -> Self {
        FolderId(sha256(pk))
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, data).as_ref());
    out
}

/// Why an id's text form was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    /// Not 56 base32 characters (after dropping `-` and whitespace).
    Length,
    /// A character that is not base32.
    Character(char),
    /// A check character does not match: a typo.
    Check,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Length => f.write_str("an id has 56 characters (8 groups of 7)"),
            IdError::Character(c) => write!(f, "{c:?} cannot appear in an id"),
            IdError::Check => f.write_str("the id has a typo (a check character does not match)"),
        }
    }
}

impl std::error::Error for IdError {}

/// Syncthing's `luhn.go`, ported as is. It is not the textbook Luhn algorithm: the factor starts at 1 on the
/// *left* and alternates 1, 2, 1, … (textbook Luhn starts doubling at the right), and each product is folded as
/// `p / n + p % n`. It has to be this one so ids match Syncthing's character for character.
/// `None` when `s` has a character outside `alphabet`.
fn luhn(alphabet: &[u8], s: &[u8]) -> Option<u8> {
    let n = alphabet.len();
    let (mut factor, mut sum) = (1, 0);
    for c in s {
        let codepoint = alphabet.iter().position(|a| a == c)?;
        let addend = factor * codepoint;
        factor = if factor == 2 { 1 } else { 2 };
        sum += addend / n + addend % n;
    }
    Some(alphabet[(n - sum % n) % n])
}

/// The 32 bytes in the grouped, checked text form.
fn to_text(bytes: &[u8; 32]) -> String {
    let plain = base32::encode(bytes);
    let mut checked = Vec::with_capacity(56);
    for group in plain.as_bytes().chunks(13) {
        checked.extend_from_slice(group);
        // The groups are base32, so the check character always exists.
        checked.push(luhn(base32::ALPHABET, group).unwrap_or(b'A'));
    }
    let mut out = String::with_capacity(63);
    for (i, group) in checked.chunks(7).enumerate() {
        if i > 0 {
            out.push('-');
        }
        out.extend(group.iter().map(|&b| b as char));
    }
    out
}

/// The first group of the text form: enough for a person (and a log line) to tell devices apart.
fn first_group(bytes: &[u8; 32]) -> String {
    to_text(bytes)[..7].to_owned()
}

/// Parses the text form back into 32 bytes, checking every check character.
fn from_text(text: &str) -> Result<[u8; 32], IdError> {
    let mut chars = Vec::with_capacity(56);
    for c in text.chars() {
        if base32::ignorable(c) {
            continue;
        }
        let c = match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            c => c,
        };
        let b = u8::try_from(c).ok().filter(|&b| base32::value(b).is_some()).ok_or(IdError::Character(c))?;
        if chars.len() == 56 {
            return Err(IdError::Length);
        }
        chars.push(b);
    }
    if chars.len() != 56 {
        return Err(IdError::Length);
    }
    let mut plain = String::with_capacity(52);
    for group in chars.chunks(14) {
        let (data, check) = group.split_at(13);
        if luhn(base32::ALPHABET, data) != Some(check[0]) {
            return Err(IdError::Check);
        }
        plain.extend(data.iter().map(|&b| b as char));
    }
    let bytes = base32::decode(&plain).map_err(|_| IdError::Length)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| IdError::Length)
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_text(&self.0))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", first_group(&self.0))
    }
}

impl FromStr for DeviceId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        from_text(s).map(DeviceId)
    }
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_text(&self.0))
    }
}

impl fmt::Debug for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FolderId({})", first_group(&self.0))
    }
}

impl FromStr for FolderId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, IdError> {
        from_text(s).map(FolderId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example id in Syncthing's documentation: the bytes `asdl` eight times.
    const DOCS_ID: &str = "MFZWI3D-BONSGYC-YLTMRWG-C43ENR5-QXGZDMM-FZWI3DP-BONSGYY-LTMRWAD";
    /// The id in Syncthing's `deviceid_test.go`.
    const TEST_ID: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";

    #[test]
    fn luhn_matches_syncthings_own_vectors() {
        assert_eq!(luhn(b"abcdef", b"abcdef"), Some(b'e'));
        assert_eq!(luhn(b"0123456789", b"7992739871"), Some(b'3'));
        assert_eq!(luhn(b"0123456789", b"79x"), None);
    }

    #[test]
    fn syncthing_documentation_id() {
        let id: DeviceId = DOCS_ID.parse().unwrap();
        assert_eq!(&id.0, b"asdlasdlasdlasdlasdlasdlasdlasdl");
        assert_eq!(id.to_string(), DOCS_ID);
        assert_eq!(format!("{id:?}"), "DeviceId(MFZWI3D)");
    }

    #[test]
    fn syncthing_test_id_roundtrips() {
        let id: DeviceId = TEST_ID.parse().unwrap();
        assert_eq!(id.to_string(), TEST_ID);
    }

    #[test]
    fn parsing_is_lenient_about_layout_case_and_lookalikes() {
        let want: DeviceId = TEST_ID.parse().unwrap();
        let squashed = TEST_ID.replace('-', "");
        assert_eq!(squashed.parse::<DeviceId>(), Ok(want));
        assert_eq!(TEST_ID.to_lowercase().parse::<DeviceId>(), Ok(want));
        assert_eq!(format!("  {} ", TEST_ID.replace('-', " ")).parse::<DeviceId>(), Ok(want));
        // Other grouping: only the characters count.
        let regrouped: Vec<String> = squashed.as_bytes().chunks(4).map(|g| String::from_utf8_lossy(g).into()).collect();
        assert_eq!(regrouped.join("-").parse::<DeviceId>(), Ok(want));
        // `O` and `I` typed as digits.
        assert_eq!(TEST_ID.replace('O', "0").replace('I', "1").parse::<DeviceId>(), Ok(want));
    }

    #[test]
    fn a_typo_is_caught() {
        let typo = TEST_ID.replacen("MZJNU2Y", "MZJNV2Y", 1);
        assert_eq!(typo.parse::<DeviceId>(), Err(IdError::Check));
        assert_eq!(TEST_ID[..62].parse::<DeviceId>(), Err(IdError::Length));
        assert_eq!(format!("{TEST_ID}A").parse::<DeviceId>(), Err(IdError::Length));
        assert_eq!(TEST_ID.replacen('P', "9", 1).parse::<DeviceId>(), Err(IdError::Character('9')));
        // The 52-character form without check characters (Syncthing's pre-0.9 ids) is not accepted.
        let unchecked: String =
            TEST_ID.replace('-', "").chars().enumerate().filter(|(i, _)| i % 14 != 13).map(|(_, c)| c).collect();
        assert_eq!(unchecked.parse::<DeviceId>(), Err(IdError::Length));
    }

    #[test]
    fn every_single_character_change_is_caught() {
        let text = TEST_ID.replace('-', "");
        for i in 0..text.len() {
            for &c in base32::ALPHABET {
                let mut b = text.clone().into_bytes();
                if b[i] == c {
                    continue;
                }
                b[i] = c;
                let changed = String::from_utf8(b).unwrap();
                assert_eq!(changed.parse::<DeviceId>(), Err(IdError::Check), "position {i}, {}", c as char);
            }
        }
    }

    #[test]
    fn folder_ids_use_the_same_form() {
        let f = FolderId::of_public_key(&[7; 32]);
        assert_eq!(f.to_string().parse::<FolderId>(), Ok(f));
        assert_eq!(format!("{f:?}"), format!("FolderId({})", &f.to_string()[..7]));
    }

    #[test]
    fn short_id_is_the_big_endian_prefix() {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(DeviceId(b).short(), ShortId(0x0102_0304_0506_0708));
    }
}
