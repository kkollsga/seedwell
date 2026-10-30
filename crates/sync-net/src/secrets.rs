//! Where keys live: the device key and every folder's key.
//!
//! The app passes a Keychain-backed [`SecretStore`] (implemented in Swift through the FFI); the daemon and
//! the tests use [`FileSecrets`], a 0600 file in the state directory. Secrets never go into `sync.toml`,
//! the synced folder, or a log.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

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
}
