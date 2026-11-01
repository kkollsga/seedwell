//! Signed development-build manifests (SPEC 12A, Development builds).
//!
//! A folder the app watches for builds (the user's devices share one through Folder Sync) holds the builds' zips and
//! one `build.json`: a manifest naming each system's build (version, commit, zip, SHA-256 of the zip), signed with
//! an Ed25519 **build key**. Anyone who can write to that folder can drop a zip there, so a device installs a build
//! from it only when the manifest verifies under a key this copy of Seedwell trusts ([`TRUSTED_KEYS`], compiled in:
//! the trust comes with the app itself, from a release) and the zip's SHA-256 is the one signed.
//!
//! The secret key never enters the repository or a log: it stays in the publishing Mac's Keychain and reaches the
//! signer (`seedwell-build-sign`, run by `tools/build-sign.sh`) on standard input.
//!
//! ```text
//! build.json = {"manifest": "<the manifest's JSON text>", "key": "<public key, hex>", "signature": "<hex>"}
//! signature  = Ed25519(build key, "seedwell-build-manifest/v1\n" ‖ the manifest's JSON text, as UTF-8)
//! manifest   = {"format": 1, "signed_ms": …, "builds": {"macos": Build, "windows": Build}}
//! Build      = {"version": "0.1.12.3", "commit": "abc1234", "zip": "Seedwell-0.1.12.3.zip", "sha256": "…", "minimum_system": "15.0"}
//! ```
//!
//! The manifest is carried as text, so the bytes signed are the bytes verified (no canonical JSON needed). What a
//! build must be to install (this system's zip name for its version, newer than the running one, not failed before)
//! is the updater's rule (`trs-ffi`'s `updates.rs`); this crate says only whether the manifest is the build key's.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

/// The manifest's file name in a watched folder.
pub const FILE: &str = "build.json";

/// The most a `build.json` may be.
pub const MAX_BYTES: usize = 64 * 1024;

/// What a signature covers before the manifest's text: a build key's signature can mean nothing else.
const CONTEXT: &[u8] = b"seedwell-build-manifest/v1\n";

/// The manifest format this crate writes and reads.
pub const FORMAT: u32 = 1;

/// The build keys every copy of Seedwell trusts: hex Ed25519 public keys. The secret half of each is kept by the
/// device that publishes builds (`tools/build-sign.sh key new` made it, in that Mac's Keychain). A key added here
/// is trusted from the release that carries it on; a key removed is no longer trusted from then on.
pub const TRUSTED_KEYS: &[&str] = &[
    // Kristian's Mac mini, made 2026-10-31.
    "200a670ebc3ed34aa032d9e4c823ebebeec1dfdc24617e7a86a1620b204932d4",
];

/// An Ed25519 public key.
pub type PublicKey = [u8; 32];

/// One system's build in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Build {
    /// `X.Y.Z.N` (or `X.Y.Z`).
    pub version: String,
    /// The commit it was built from, for information (`<sha>-dirty` with uncommitted changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The zip's file name, in the same folder.
    pub zip: String,
    /// The zip's SHA-256, lower-case hex.
    pub sha256: String,
    /// The oldest system version it runs on (`15.0`, `10.0.17763`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_system: Option<String>,
}

/// What `build.json` says: each system's build (`macos`, `windows`), signed together.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// [`FORMAT`].
    pub format: u32,
    /// When it was signed (milliseconds since 1970), for information.
    #[serde(default)]
    pub signed_ms: u64,
    /// The builds by system.
    pub builds: BTreeMap<String, Build>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    manifest: String,
    key: String,
    signature: String,
}

/// Why a `build.json` is not installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not a signed manifest at all (an older folder's `latest.json` style, garbage, too large).
    Unreadable,
    /// Signed by a key this copy does not trust (its public key, hex).
    UntrustedKey(String),
    /// The signature does not match the manifest: changed after signing, or not signed by that key.
    BadSignature,
    /// Signed by a trusted key, but not a manifest this copy can read.
    BadManifest(&'static str),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Unreadable => write!(f, "{FILE} is not a signed build manifest"),
            Refused::UntrustedKey(k) => write!(f, "{FILE} is signed by a key this copy of Seedwell does not trust ({}…)", &k[..k.len().min(16)]),
            Refused::BadSignature => write!(f, "{FILE}'s signature does not match it (changed after it was signed)"),
            Refused::BadManifest(why) => write!(f, "{FILE} is signed but unreadable: {why}"),
        }
    }
}

impl std::error::Error for Refused {}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A public key from its hex.
pub fn parse_key(s: &str) -> Option<PublicKey> {
    unhex::<32>(s)
}

/// The keys compiled in ([`TRUSTED_KEYS`]).
pub fn trusted_keys() -> Vec<PublicKey> {
    TRUSTED_KEYS.iter().filter_map(|k| parse_key(k)).collect()
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A plain file name in the folder: no path, nothing hidden.
fn is_plain_zip_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.ends_with(".zip")
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn is_version(s: &str) -> bool {
    !s.is_empty() && s.len() <= 48 && s.bytes().all(|b| b.is_ascii_digit() || b == b'.') && !s.contains("..")
}

impl Manifest {
    /// A manifest with no builds yet.
    pub fn new(signed_ms: u64) -> Self {
        Manifest { format: FORMAT, signed_ms, builds: BTreeMap::new() }
    }

    fn check(&self) -> Result<(), &'static str> {
        if self.format != FORMAT {
            return Err("a newer manifest format");
        }
        for (system, b) in &self.builds {
            if system.is_empty() || !system.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
                return Err("a system name that is not one");
            }
            if !is_version(&b.version) {
                return Err("a version that is not one");
            }
            if !is_plain_zip_name(&b.zip) {
                return Err("a zip name that is not a plain file name");
            }
            if !is_sha256(&b.sha256) {
                return Err("a SHA-256 that is not one");
            }
        }
        Ok(())
    }
}

/// The manifest in `doc` (a `build.json`) and the key that signed it, when it is signed by one of `trusted` and
/// well formed. SHA-256s are lower-cased.
pub fn verify(doc: &[u8], trusted: &[PublicKey]) -> Result<(Manifest, PublicKey), Refused> {
    if doc.len() > MAX_BYTES {
        return Err(Refused::Unreadable);
    }
    let env: Envelope = serde_json::from_slice(doc).map_err(|_| Refused::Unreadable)?;
    let key = parse_key(&env.key).ok_or(Refused::Unreadable)?;
    let sig = unhex::<64>(&env.signature).ok_or(Refused::Unreadable)?;
    if !trusted.contains(&key) {
        return Err(Refused::UntrustedKey(hex(&key)));
    }
    let mut signed = CONTEXT.to_vec();
    signed.extend_from_slice(env.manifest.as_bytes());
    UnparsedPublicKey::new(&ED25519, &key).verify(&signed, &sig).map_err(|_| Refused::BadSignature)?;
    let mut m: Manifest = serde_json::from_str(&env.manifest).map_err(|_| Refused::BadManifest("not a manifest"))?;
    m.check().map_err(Refused::BadManifest)?;
    for b in m.builds.values_mut() {
        b.sha256.make_ascii_lowercase();
    }
    Ok((m, key))
}

/// A build key's secret half (an Ed25519 key pair from its 32-byte seed).
pub struct SigningKey(Ed25519KeyPair);

impl SigningKey {
    /// The key whose seed is `s` (64 hex characters; surrounding white space is ignored).
    pub fn from_seed_hex(s: &str) -> Option<Self> {
        let seed = unhex::<32>(s)?;
        Ed25519KeyPair::from_seed_unchecked(&seed).ok().map(SigningKey)
    }

    /// A new key, and its seed as hex (to keep: it is the secret).
    pub fn generate() -> Result<(Self, String), String> {
        use ring::rand::SecureRandom as _;
        let mut seed = [0u8; 32];
        ring::rand::SystemRandom::new().fill(&mut seed).map_err(|_| "no random numbers from the system".to_owned())?;
        let k = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|e| e.to_string())?;
        Ok((SigningKey(k), hex(&seed)))
    }

    /// Its public key.
    pub fn public_key(&self) -> PublicKey {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(self.0.public_key().as_ref());
        pk
    }

    /// `m` signed: the bytes of a `build.json`.
    pub fn sign(&self, m: &Manifest) -> Vec<u8> {
        let manifest = serde_json::to_string(m).expect("a manifest is JSON");
        let mut signed = CONTEXT.to_vec();
        signed.extend_from_slice(manifest.as_bytes());
        let signature = hex(self.0.sign(&signed).as_ref());
        let env = Envelope { manifest, key: hex(&self.public_key()), signature };
        let mut out = serde_json::to_vec_pretty(&env).expect("an envelope is JSON");
        out.push(b'\n');
        out
    }
}

/// The SHA-256 of a file, lower-case hex.
pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// The system and version a build's zip is for, from its name: `Seedwell-X.Y.Z.N.zip` is the Mac's (`macos`),
/// `Seedwell-X.Y.Z.N-windows-x64.zip` Windows' (`windows`), as the build scripts name them.
pub fn zip_system(name: &str) -> Option<(&'static str, String)> {
    let v = name.strip_prefix("Seedwell-")?.strip_suffix(".zip")?;
    let (system, v) = match v.strip_suffix("-windows-x64") {
        Some(v) => ("windows", v),
        None => ("macos", v),
    };
    is_version(v).then(|| (system, v.to_owned()))
}

/// A build to put in a folder's manifest ([`add`]).
#[derive(Debug, Clone, Copy)]
pub struct Adding<'a> {
    /// The zip's file name, in the folder.
    pub zip: &'a str,
    /// Its SHA-256 as the one who built it reported it: the file there must have it.
    pub sha256: &'a str,
    /// The commit it was built from.
    pub commit: Option<&'a str>,
    /// The oldest system version it runs on.
    pub minimum: Option<&'a str>,
}

/// Puts the build `a.zip` (a file in `folder`, whose SHA-256 must be `a.sha256`) in the folder's `build.json`,
/// signed with `key`: the entry for its system is replaced; the other systems' entries stay when the manifest there
/// is signed by `key` or one of `trusted` and their zip is still in the folder. Written aside and renamed into
/// place, so a watcher never reads half a file. Returns what was written.
pub fn add(folder: &Path, a: &Adding, key: &SigningKey, trusted: &[PublicKey], now_ms: u64) -> Result<Manifest, String> {
    let Adding { zip, sha256, commit, minimum } = *a;
    let (system, version) = zip_system(zip).ok_or_else(|| format!("{zip} is not a build's zip (Seedwell-X.Y.Z.N.zip or Seedwell-X.Y.Z.N-windows-x64.zip)"))?;
    if !is_plain_zip_name(zip) {
        return Err(format!("{zip} is not a plain file name"));
    }
    let want = sha256.trim().to_ascii_lowercase();
    if !is_sha256(&want) {
        return Err(format!("{sha256} is not a SHA-256"));
    }
    let have = sha256_file(&folder.join(zip)).map_err(|e| format!("{} cannot be read: {e}", folder.join(zip).display()))?;
    if have != want {
        return Err(format!("{zip} in {} has SHA-256 {have}, not {want}: not signed", folder.display()));
    }
    let mut keys = trusted.to_vec();
    keys.push(key.public_key());
    let mut m = Manifest::new(now_ms);
    if let Ok(doc) = std::fs::read(folder.join(FILE))
        && let Ok((old, _)) = verify(&doc, &keys)
    {
        m.builds = old.builds.into_iter().filter(|(s, b)| s != system && folder.join(&b.zip).is_file()).collect();
    }
    m.builds.insert(
        system.to_owned(),
        Build { version, commit: commit.map(str::to_owned), zip: zip.to_owned(), sha256: want, minimum_system: minimum.map(str::to_owned) },
    );
    let tmp = folder.join(format!(".{FILE}.part"));
    std::fs::write(&tmp, key.sign(&m)).map_err(|e| format!("{} cannot be written: {e}", tmp.display()))?;
    std::fs::rename(&tmp, folder.join(FILE)).map_err(|e| format!("{} cannot be put in place: {e}", folder.join(FILE).display()))?;
    Ok(m)
}

#[cfg(test)]
mod tests;
