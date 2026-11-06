//! Where keys live: the device key and every folder's and channel's key.
//!
//! The Mac app passes a Keychain-backed [`SecretStore`] (implemented in Swift through the FFI). Without one,
//! [`open`] picks the store: on a system with a sealer (`trs_platform::secrets`: DPAPI on Windows) a [`SealedFiles`]
//! vault in the state folder, into which the plain file is moved once ([`migrate`]); elsewhere, and for the daemon
//! and the tests, [`FileSecrets`], a 0600 file in the state directory. Secrets never go into `sync.toml`, the synced
//! folder, or a log.
//!
//! **A vault that fails never loses a key.** The plain file is removed only once every secret in it reads back from
//! the vault, and only when the caller says this version has started fine before: until then an older version that
//! the installer goes back to still finds its keys there. A key the vault already holds keeps the vault's value (a
//! plain one that differs was made by a version that could not see the vault); the plain value is kept aside in the
//! vault. When the sealer or the move fails, [`open`] gives a [`Fallback`]: the plain file first (where new keys then
//! go), the vault under it for keys moved before, and a problem to show. A vault that holds keys but cannot open them
//! answers with an error, never "no such key", so no key is ever made anew in place of one it holds.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use trs_platform::secrets::Sealer;

/// A store of named secrets.
pub trait SecretStore: Send + Sync {
    /// The secret called `name`, if stored.
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Stores (or replaces) a secret.
    fn put(&self, name: &str, value: &[u8]) -> io::Result<()>;
    /// Forgets a secret (no error when it was not there).
    fn delete(&self, name: &str) -> io::Result<()>;
}

/// Secrets in one file, readable by the owner only. One `name = hex` line each.
pub struct FileSecrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileSecrets {
    /// The file at `path` (created on the first `put`).
    pub fn new(path: PathBuf) -> Self {
        FileSecrets { path, lock: Mutex::new(()) }
    }

    fn read(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e),
        };
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once(" = ") else { continue };
            let bytes = (0..v.len())
                .step_by(2)
                .map(|i| v.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{}: damaged entry {k}", self.path.display())))?;
            out.insert(k.to_string(), bytes);
        }
        Ok(out)
    }

    fn write(&self, all: &BTreeMap<String, Vec<u8>>) -> io::Result<()> {
        // The first key can come before folder sync has made its folder (the torrent proxy's, at the first start).
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.path.with_extension("tmp");
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        let mut f = trs_platform::fs::set_create_mode(&mut o, 0o600).open(&tmp)?;
        for (k, v) in all {
            let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
            writeln!(f, "{k} = {hex}")?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Every secret in the file.
    pub fn all(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let _g = crate::shared::lock(&self.lock);
        self.read()
    }

    /// Removes the file, its bytes overwritten first (no error when it is not there).
    pub fn erase(&self) -> io::Result<()> {
        let _g = crate::shared::lock(&self.lock);
        let len = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            let _ = f.write_all(&vec![0u8; len as usize]).and_then(|()| f.sync_all());
        }
        std::fs::remove_file(&self.path)
    }
}

/// Secrets in a folder, one file each, sealed with a [`Sealer`] (DPAPI on Windows): the file's name is the secret's
/// name in hex, its bytes what the sealer made of the secret. Written aside and renamed; readable by the owner only
/// where the system has modes.
pub struct SealedFiles {
    dir: PathBuf,
    sealer: Arc<dyn Sealer>,
    lock: Mutex<()>,
}

impl SealedFiles {
    /// The vault in `dir` (made on the first `put`).
    pub fn new(dir: PathBuf, sealer: Arc<dyn Sealer>) -> Self {
        SealedFiles { dir, sealer, lock: Mutex::new(()) }
    }

    fn file(&self, name: &str) -> PathBuf {
        let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.sealed"))
    }

    /// The sealer's name, for messages.
    pub fn sealer_name(&self) -> &'static str {
        self.sealer.name()
    }
}

impl SecretStore for SealedFiles {
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let _g = crate::shared::lock(&self.lock);
        match std::fs::read(self.file(name)) {
            Ok(sealed) => self.sealer.unseal(&sealed).map(Some).map_err(|e| io::Error::new(e.kind(), format!("{name}: {} cannot open it: {e}", self.sealer.name()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn put(&self, name: &str, value: &[u8]) -> io::Result<()> {
        let _g = crate::shared::lock(&self.lock);
        let sealed = self.sealer.seal(value).map_err(|e| io::Error::new(e.kind(), format!("{} cannot seal a key: {e}", self.sealer.name())))?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file(name);
        let tmp = path.with_extension("tmp");
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        let mut f = trs_platform::fs::set_create_mode(&mut o, 0o600).open(&tmp)?;
        f.write_all(&sealed)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        let _g = crate::shared::lock(&self.lock);
        match std::fs::remove_file(self.file(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// The store while the vault fails: the plain file first (new keys go there), the vault under it for the keys moved
/// into it before. A key is forgotten in both.
pub struct Fallback {
    plain: FileSecrets,
    vault: Arc<dyn SecretStore>,
}

impl SecretStore for Fallback {
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match self.plain.get(name)? {
            Some(v) => Ok(Some(v)),
            None => self.vault.get(name),
        }
    }

    fn put(&self, name: &str, value: &[u8]) -> io::Result<()> {
        self.plain.put(name, value)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        self.plain.delete(name)?;
        self.vault.delete(name)
    }
}

/// Copies every secret of the plain file into `vault`: each one is put, then read back and compared. A name the vault
/// already holds with another value keeps the vault's; the plain value is kept aside in the vault as `<name>~<n>`
/// (nothing reads it, nothing is lost). With `erase`, once all of them are in the vault the plain file is erased.
/// Returns how many were copied. On any failure the plain file stays as it was (what was put in the vault meanwhile is
/// a copy). Run before the service starts; a crash halfway through only means the next start copies them again.
pub fn migrate(plain: &FileSecrets, vault: &dyn SecretStore, erase: bool) -> io::Result<usize> {
    let all = plain.all()?;
    let mut copied = 0;
    for (name, value) in &all {
        let (target, keep) = match vault.get(name)? {
            Some(v) if v == *value => continue,
            None => (name.clone(), value.clone()),
            Some(_) => {
                let aside = (1..).map(|n| format!("{name}~{n}")).find(|n| !matches!(vault.get(n), Ok(Some(_)))).unwrap_or_default();
                (aside, value.clone())
            }
        };
        vault.put(&target, &keep)?;
        if vault.get(&target)?.as_deref() != Some(keep.as_slice()) {
            return Err(io::Error::other(format!("{target} read back differently from the vault")));
        }
        copied += 1;
    }
    if erase {
        plain.erase()?;
    }
    Ok(copied)
}

/// The store for the state folder's sync directory `dir` when the app passes none: see [`open_with`], with this
/// system's sealer.
pub fn open(dir: &Path, erase_plain: bool) -> (Arc<dyn SecretStore>, Option<String>) {
    open_with(dir, trs_platform::secrets::system_sealer(), erase_plain)
}

/// The store in `dir`: without a sealer the plain file `secrets`; with one, the vault `vault/` once the plain file is
/// copied into it (a first check that the sealer opens what it sealed, then [`migrate`]; `erase_plain`: this version
/// has started fine before, so the plain file goes). When that fails, a [`Fallback`] and why, to show as a problem
/// (the next start tries again). Never loses a key: see the module.
pub fn open_with(dir: &Path, sealer: Option<Arc<dyn Sealer>>, erase_plain: bool) -> (Arc<dyn SecretStore>, Option<String>) {
    let plain = FileSecrets::new(dir.join("secrets"));
    let Some(sealer) = sealer else { return (Arc::new(plain), None) };
    let name = sealer.name();
    let vault: Arc<dyn SecretStore> = Arc::new(SealedFiles::new(dir.join("vault"), sealer.clone()));
    let probe = b"seedwell vault check";
    let works = sealer.seal(probe).and_then(|s| sealer.unseal(&s)).and_then(|p| if p == probe { Ok(()) } else { Err(io::Error::other("it opened something else than it sealed")) });
    match works.and_then(|()| migrate(&plain, vault.as_ref(), erase_plain)) {
        Ok(_) => (vault, None),
        Err(e) => {
            let why = format!(
                "Folder and channel keys are kept unsealed in Seedwell's state folder, because {name} failed: {e}. Nothing is lost; the next start tries again."
            );
            (Arc::new(Fallback { plain, vault }), Some(why))
        }
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let _g = crate::shared::lock(&self.lock);
        Ok(self.read()?.remove(name))
    }

    fn put(&self, name: &str, value: &[u8]) -> io::Result<()> {
        let _g = crate::shared::lock(&self.lock);
        let mut all = self.read()?;
        all.insert(name.to_string(), value.to_vec());
        self.write(&all)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        let _g = crate::shared::lock(&self.lock);
        let mut all = self.read()?;
        if all.remove(name).is_some() {
            self.write(&all)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failure: a key stored before folder sync has made its folder is lost ("No such file or directory").
    #[test]
    fn the_first_key_makes_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileSecrets::new(dir.path().join("sync").join("secrets"));
        s.put("a", b"1").unwrap();
        assert_eq!(s.get("a").unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn stores_and_forgets_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileSecrets::new(dir.path().join("secrets"));
        assert_eq!(s.get("a").unwrap(), None);
        s.put("a", &[1, 2, 255]).unwrap();
        s.put("b", b"xyz").unwrap();
        assert_eq!(s.get("a").unwrap(), Some(vec![1, 2, 255]));
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
        assert_eq!(s.get("b").unwrap(), Some(b"xyz".to_vec()));
        if let Some(mode) = trs_platform::fs::unix_mode(&std::fs::metadata(dir.path().join("secrets")).unwrap()) {
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// A stand-in for DPAPI (the real one is tested on Windows CI, in `trs_platform::secrets`): sealing reverses the
    /// bytes and adds a mark; it can be made to fail sealing, opening, or to open wrongly.
    #[derive(Default)]
    struct Fake {
        no_seal: bool,
        no_unseal: bool,
        wrong: bool,
    }

    impl Sealer for Fake {
        fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>> {
            if self.no_seal {
                return Err(io::Error::other("sealing is broken"));
            }
            Ok([b"S1".as_slice(), &plain.iter().rev().copied().collect::<Vec<u8>>()].concat())
        }
        fn unseal(&self, sealed: &[u8]) -> io::Result<Vec<u8>> {
            if self.no_unseal {
                return Err(io::Error::other("opening is broken"));
            }
            let body = sealed.strip_prefix(b"S1").ok_or_else(|| io::Error::other("not sealed here"))?;
            let mut out: Vec<u8> = body.iter().rev().copied().collect();
            if self.wrong {
                out.push(0);
            }
            Ok(out)
        }
        fn name(&self) -> &'static str {
            "the test sealer"
        }
    }

    fn sealer(f: Fake) -> Option<Arc<dyn Sealer>> {
        Some(Arc::new(f))
    }

    #[test]
    fn the_plain_file_moves_into_the_vault_and_goes_once_this_version_started_fine() {
        let dir = tempfile::tempdir().unwrap();
        let plain = FileSecrets::new(dir.path().join("secrets"));
        plain.put("device", &[1; 32]).unwrap();
        plain.put("folder-ABC", &[2; 32]).unwrap();
        // The first start of this version: copied into the vault, the plain file kept for an older version the
        // installer may go back to.
        let (s, problem) = open_with(dir.path(), sealer(Fake::default()), false);
        assert_eq!(problem, None);
        assert!(dir.path().join("secrets").is_file(), "kept until this version started fine");
        assert_eq!(s.get("device").unwrap(), Some(vec![1; 32]));
        assert_eq!(s.get("folder-ABC").unwrap(), Some(vec![2; 32]));
        // Sealed on disk: the key's bytes are not there as they are.
        let files: Vec<_> = std::fs::read_dir(dir.path().join("vault")).unwrap().flatten().map(|e| std::fs::read(e.path()).unwrap()).collect();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.starts_with(b"S1")));
        // New keys go to the vault only.
        s.put("channel-X", b"k").unwrap();
        s.delete("folder-ABC").unwrap();
        assert_eq!(plain.get("channel-X").unwrap(), None);
        // A later start, this version having started fine: the plain copy goes; the vault has everything.
        let (again, problem) = open_with(dir.path(), sealer(Fake::default()), true);
        assert_eq!(problem, None);
        assert!(!dir.path().join("secrets").exists(), "the plain copy is gone");
        assert_eq!(again.get("channel-X").unwrap(), Some(b"k".to_vec()));
        assert_eq!(again.get("device").unwrap(), Some(vec![1; 32]));
        // The deleted key was still in the plain file: it came back from there (a key too many is never a loss).
        assert_eq!(again.get("folder-ABC").unwrap(), Some(vec![2; 32]));
        // Without a sealer (the Mac, the daemon): the plain file, as before.
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        let (plain_only, problem) = open_with(&dir.path().join("other"), None, true);
        plain_only.put("a", b"1").unwrap();
        assert!(problem.is_none() && dir.path().join("other/secrets").is_file());
    }

    #[test]
    fn a_failing_vault_keeps_the_plain_file_says_so_and_loses_nothing() {
        for broken in [Fake { no_seal: true, ..Fake::default() }, Fake { no_unseal: true, ..Fake::default() }, Fake { wrong: true, ..Fake::default() }] {
            let dir = tempfile::tempdir().unwrap();
            FileSecrets::new(dir.path().join("secrets")).put("device", &[1; 32]).unwrap();
            let (s, problem) = open_with(dir.path(), sealer(broken), true);
            let problem = problem.expect("said");
            assert!(problem.contains("the test sealer failed") && problem.contains("Nothing is lost"), "{problem}");
            assert!(dir.path().join("secrets").is_file(), "the plain copy stays");
            assert_eq!(s.get("device").unwrap(), Some(vec![1; 32]));
            s.put("folder-new", b"n").unwrap();
            assert_eq!(FileSecrets::new(dir.path().join("secrets")).get("folder-new").unwrap(), Some(b"n".to_vec()), "new keys go to the plain file");
            // Once the vault works, all of them move.
            let (s, problem) = open_with(dir.path(), sealer(Fake::default()), true);
            assert_eq!(problem, None);
            assert_eq!((s.get("device").unwrap(), s.get("folder-new").unwrap()), (Some(vec![1; 32]), Some(b"n".to_vec())));
            assert!(!dir.path().join("secrets").exists());
        }
    }

    #[test]
    fn keys_in_a_vault_that_cannot_be_opened_are_an_error_never_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = open_with(dir.path(), sealer(Fake::default()), true);
        s.put("device", &[9; 32]).unwrap();
        // A later start where the sealer fails (a broken profile): the key is not "not there", so none is made anew.
        let (s, problem) = open_with(dir.path(), sealer(Fake { no_unseal: true, ..Fake::default() }), true);
        assert!(problem.is_some());
        let e = s.get("device").unwrap_err();
        assert!(e.to_string().contains("cannot open it"), "{e}");
        assert_eq!(s.get("never-stored").unwrap(), None);
    }

    /// An older version started after the move (the user went back) finds no plain file and makes a new key: when this
    /// version comes back, the vault's key stays the one in force and the other is kept aside, so neither is lost.
    #[test]
    fn a_key_the_vault_holds_keeps_its_value_and_a_different_plain_one_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (s, _) = open_with(dir.path(), sealer(Fake::default()), true);
        s.put("device", &[9; 32]).unwrap();
        let plain = FileSecrets::new(dir.path().join("secrets"));
        plain.put("device", &[8; 32]).unwrap();
        plain.put("folder-made-meanwhile", b"m").unwrap();
        let vault = SealedFiles::new(dir.path().join("vault"), Arc::new(Fake::default()));
        assert_eq!(migrate(&plain, &vault, true).unwrap(), 2);
        assert_eq!(vault.get("device").unwrap(), Some(vec![9; 32]));
        assert_eq!(vault.get("device~1").unwrap(), Some(vec![8; 32]));
        assert_eq!(vault.get("folder-made-meanwhile").unwrap(), Some(b"m".to_vec()));
        // Again: another aside, never one written over.
        plain.put("device", &[7; 32]).unwrap();
        migrate(&plain, &vault, true).unwrap();
        assert_eq!((vault.get("device~1").unwrap(), vault.get("device~2").unwrap()), (Some(vec![8; 32]), Some(vec![7; 32])));
    }

    #[test]
    fn names_of_any_kind_are_kept_apart() {
        let dir = tempfile::tempdir().unwrap();
        let v = SealedFiles::new(dir.path().join("vault"), Arc::new(Fake::default()));
        for (i, n) in ["a/b", "a\\b", "..", "Folder Key ü", "a.b"].iter().enumerate() {
            v.put(n, &[i as u8]).unwrap();
        }
        for (i, n) in ["a/b", "a\\b", "..", "Folder Key ü", "a.b"].iter().enumerate() {
            assert_eq!(v.get(n).unwrap(), Some(vec![i as u8]), "{n}");
        }
        assert_eq!(std::fs::read_dir(dir.path().join("vault")).unwrap().count(), 5);
        assert_eq!(v.sealer_name(), "the test sealer");
    }
}
