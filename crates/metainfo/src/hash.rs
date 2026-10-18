use core::fmt;
use sha1::{Digest as _, Sha1};
use sha2::Sha256;

const HEX: &[u8; 16] = b"0123456789abcdef";
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Lower-case hex encoding.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 15)]));
    }
    s
}

/// Decodes exactly `2 * N` hex digits (either case). `None` on any deviation.
pub fn hex_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != N * 2 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (o, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
        *o = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

/// RFC 4648 base32 (upper-case, no padding), the encoding magnet links use.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = acc << 8 | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(B32[((acc >> bits) & 31) as usize]));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(B32[((acc << (5 - bits)) & 31) as usize]));
    }
    out
}

/// Decodes base32 (either case, no padding) into exactly `N` bytes. Rejects
/// wrong lengths, foreign characters and non-zero trailing bits.
pub fn base32_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != (N * 8).div_ceil(5) {
        return None;
    }
    let mut out = [0u8; N];
    let (mut acc, mut bits, mut idx) = (0u32, 0u32, 0usize);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = acc << 5 | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(idx)? = (acc >> bits) as u8;
            idx += 1;
            acc &= (1 << bits) - 1;
        }
    }
    (idx == N && acc == 0).then_some(out)
}

/// The identity of a torrent: a SHA-1 (v1) hash, a SHA-256 (v2) hash, or both
/// for hybrid torrents.
///
/// size: 54 bytes (align 1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct InfoHash {
    /// SHA-1 of the bencoded `info` dictionary (BEP 3).
    pub v1: Option<[u8; 20]>,
    /// SHA-256 of the bencoded `info` dictionary (BEP 52).
    pub v2: Option<[u8; 32]>,
}

impl InfoHash {
    /// A v1-only hash.
    pub fn from_v1(h: [u8; 20]) -> Self {
        InfoHash {
            v1: Some(h),
            v2: None,
        }
    }

    /// A v2-only hash.
    pub fn from_v2(h: [u8; 32]) -> Self {
        InfoHash {
            v1: None,
            v2: Some(h),
        }
    }

    /// Hashes the raw `info` bytes. Pass the original bytes, never a re-encoding.
    pub fn compute(info: &[u8], v1: bool, v2: bool) -> Self {
        InfoHash {
            v1: v1.then(|| Sha1::digest(info).into()),
            v2: v2.then(|| Sha256::digest(info).into()),
        }
    }

    /// True if `info` hashes to every component present in `self` (and at least
    /// one is present). Use it to validate metadata fetched via BEP 9.
    pub fn verifies(&self, info: &[u8]) -> bool {
        (self.v1.is_some() || self.v2.is_some())
            && self
                .v1
                .is_none_or(|h| <[u8; 20]>::from(Sha1::digest(info)) == h)
            && self
                .v2
                .is_none_or(|h| <[u8; 32]>::from(Sha256::digest(info)) == h)
    }

    /// The 20-byte identifier used on the wire for trackers, DHT and handshakes:
    /// the v1 hash, or the first 20 bytes of the v2 hash for v2-only torrents.
    pub fn swarm_id(&self) -> Option<[u8; 20]> {
        self.v1.or_else(|| {
            let v2 = self.v2?;
            let mut id = [0u8; 20];
            id.copy_from_slice(&v2[..20]);
            Some(id)
        })
    }

    /// True if both identify the same torrent through a shared component.
    pub fn overlaps(&self, other: &InfoHash) -> bool {
        (self.v1.is_some() && self.v1 == other.v1) || (self.v2.is_some() && self.v2 == other.v2)
    }

    /// 40-char lower-case hex of the v1 hash.
    pub fn v1_hex(&self) -> Option<String> {
        self.v1.map(|h| hex_encode(&h))
    }

    /// 64-char lower-case hex of the v2 hash.
    pub fn v2_hex(&self) -> Option<String> {
        self.v2.map(|h| hex_encode(&h))
    }

    /// 32-char base32 form of the v1 hash.
    pub fn v1_base32(&self) -> Option<String> {
        self.v1.map(|h| base32_encode(&h))
    }

    /// Parses a v1 hash from 40 hex digits or 32 base32 characters.
    pub fn parse_v1(s: &str) -> Option<Self> {
        hex_decode::<20>(s)
            .or_else(|| base32_decode::<20>(s))
            .map(Self::from_v1)
    }
}

/// Prints the v1 hash as hex, or the v2 hash if there is no v1 (hybrid torrents
/// print their v1 hash because that is what trackers know them by).
impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.v1, self.v2) {
            (Some(h), _) => f.write_str(&hex_encode(&h)),
            (None, Some(h)) => f.write_str(&hex_encode(&h)),
            (None, None) => f.write_str("<empty>"),
        }
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InfoHash")
            .field("v1", &self.v1_hex())
            .field("v2", &self.v2_hex())
            .finish()
    }
}

/// Client prefix (Azureus style, 8 bytes): `-SW<major><minor><patch>0-` in a release build (built
/// with `SEEDWELL_RELEASE` set, as `app/release-zip.sh` does), `-SX0001-` in every other build.
pub const PEER_ID_PREFIX: [u8; 8] = match option_env!("SEEDWELL_RELEASE") {
    Some(v) if !v.is_empty() => release_prefix(env!("CARGO_PKG_VERSION")),
    _ => *b"-SX0001-",
};

/// `-SW` + one digit each for major, minor and patch + `0-` ("0.1.0" → `-SW0100-`). A version
/// part above 9 does not fit the four-digit scheme and fails the build.
pub(crate) const fn release_prefix(version: &str) -> [u8; 8] {
    let v = version.as_bytes();
    let mut out = *b"-SW0000-";
    let (mut i, mut part, mut len) = (0, 0, 0);
    while i < v.len() && part < 3 {
        let c = v[i];
        if c == b'.' {
            assert!(len == 1, "each version part must be one digit for the peer id");
            part += 1;
            len = 0;
        } else if c.is_ascii_digit() {
            assert!(len == 0, "each version part must be one digit for the peer id");
            out[3 + part] = c;
            len = 1;
        } else {
            break; // a pre-release suffix such as "-rc1"
        }
        i += 1;
    }
    assert!(part == 2 && len == 1, "the version must be major.minor.patch");
    out
}

const PEER_ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A 20-byte BitTorrent peer id: [`PEER_ID_PREFIX`] plus 12 random URL-safe
/// alphanumerics. Generated once per session, never per torrent.
///
/// size: 20 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; 20]);

impl PeerId {
    /// Generates a fresh id from OS entropy.
    pub fn generate() -> Self {
        Self::generate_with(|buf| {
            if getrandom::fill(buf).is_err() {
                // OS entropy is effectively infallible; if it ever is not, a
                // weak time-seeded stream still yields a usable (if guessable)
                // id rather than a panic. Peer ids are not secrets.
                weak_fill(buf);
            }
        })
    }

    /// Generates an id drawing randomness from `fill` (injectable for tests).
    pub fn generate_with(mut fill: impl FnMut(&mut [u8])) -> Self {
        let mut id = [0u8; 20];
        id[..8].copy_from_slice(&PEER_ID_PREFIX);
        let mut n = 8;
        // Rejection sampling (b < 248 = 4 * 62) removes modulo bias. The round
        // cap keeps a broken `fill` from looping forever.
        for _ in 0..16 {
            let mut buf = [0u8; 32];
            fill(&mut buf);
            for &b in &buf {
                if b < 248 && n < 20 {
                    id[n] = PEER_ALPHABET[usize::from(b % 62)];
                    n += 1;
                }
            }
            if n == 20 {
                return PeerId(id);
            }
        }
        while n < 20 {
            id[n] = PEER_ALPHABET[(n * 7) % 62];
            n += 1;
        }
        PeerId(id)
    }

    /// The raw 20 bytes.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

fn weak_fill(buf: &mut [u8]) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = nanos ^ (u64::from(std::process::id()) << 32) ^ 0x9E37_79B9_7F4A_7C15;
    for b in buf {
        // splitmix64 step
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *b = (z ^ (z >> 31)) as u8;
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({self})")
    }
}
