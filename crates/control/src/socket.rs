//! The control socket: where it lives, how it is created (mode 0600 inside a 0700 directory,
//! atomically), and who may connect (same uid only). There is no TCP listener, ever (SPEC 9A.1).

use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;

/// Longest socket path we accept. `sockaddr_un.sun_path` is 104 bytes on macOS (108 on Linux),
/// including the NUL.
pub const MAX_SOCKET_PATH: usize = 100;

/// Same user, nobody else. Root is *not* let in: the socket is for the user's own agents.
pub fn peer_allowed(peer_uid: u32, server_uid: u32) -> bool {
    peer_uid == server_uid
}

/// `~/Library/Application Support/Seedwell/control.sock` (macOS). Other unixes, which only the
/// headless daemon and CI use, get `$XDG_STATE_HOME/seedwell/control.sock` or
/// `~/.local/state/seedwell/control.sock`. `SEEDWELL_SOCKET` overrides both.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SEEDWELL_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Seedwell/control.sock")
    } else {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        base.join("seedwell/control.sock")
    }
}

/// A bound socket and facts about it.
pub struct BoundSocket {
    /// The listener.
    pub listener: UnixListener,
    /// The uid that owns the socket file: ours.
    pub uid: u32,
    /// Inode of the socket file, to remove only our own file at shutdown.
    pub ino: u64,
    /// Things worth telling the user (loose permissions on an existing directory).
    pub warnings: Vec<String>,
}

/// Create the socket at `path`.
///
/// * The parent directory is created with mode 0700 if missing.
/// * The socket is bound inside a fresh private 0700 directory next to the final path, chmodded
///   to 0600 and only then renamed into place, so there is no moment when another user could
///   connect to it.
/// * A leftover socket from a crashed run is replaced; a live one makes this fail with
///   `AddrInUse`.
pub async fn bind_secure(path: &Path) -> io::Result<BoundSocket> {
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path is longer than {MAX_SOCKET_PATH} bytes, the operating system limit for Unix sockets; use a shorter directory (seedwell daemon: --socket PATH)"),
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path needs a directory"))?;
    let mut warnings = Vec::new();
    let existed = parent.exists();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    let pmeta = std::fs::metadata(parent)?;
    if existed && pmeta.mode() & 0o077 != 0 {
        // Not ours to tighten (it could be the home directory); the socket itself is 0600 and
        // every connection is checked, so access stays same-user.
        warnings.push(format!(
            "directory {} is accessible to other users (mode {:o}); the socket itself stays private",
            parent.display(),
            pmeta.mode() & 0o777
        ));
    }

    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another instance is already serving {}", path.display()),
                ));
            }
            // Stale: nobody is listening.
            std::fs::remove_file(path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let mut rnd = [0u8; 6];
    getrandom::fill(&mut rnd).map_err(|e| io::Error::other(e.to_string()))?;
    let tmp_dir = parent.join(format!(
        ".ctl-{}",
        rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&tmp_dir)?;
    let result = (|| -> io::Result<UnixListener> {
        let tmp_sock = tmp_dir.join("s");
        let listener = UnixListener::bind(&tmp_sock)?;
        std::fs::set_permissions(&tmp_sock, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp_sock, path)?;
        Ok(listener)
    })();
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let listener = result?;
    let meta = std::fs::symlink_metadata(path)?;
    Ok(BoundSocket { listener, uid: meta.uid(), ino: meta.ino(), warnings })
}

/// Remove the socket file if it is still the one we created.
pub fn remove_if_ours(path: &Path, ino: u64) {
    if let Ok(m) = std::fs::symlink_metadata(path)
        && m.file_type().is_socket()
        && m.ino() == ino
    {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_same_uid_is_allowed() {
        assert!(peer_allowed(501, 501));
        assert!(!peer_allowed(0, 501), "root is not special");
        assert!(!peer_allowed(502, 501));
        assert!(!peer_allowed(u32::MAX, 0));
    }

    #[tokio::test]
    async fn socket_is_0600_in_a_0700_dir() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("Seedwell/control.sock");
        let b = bind_secure(&path).await.unwrap();
        let sm = std::fs::metadata(&path).unwrap();
        assert!(sm.file_type().is_socket());
        assert_eq!(sm.mode() & 0o777, 0o600);
        let dm = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(dm.mode() & 0o777, 0o700);
        assert_eq!(b.uid, sm.uid());
        assert!(b.warnings.is_empty());
        // No temp directories are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".ctl-"))
            .collect();
        assert!(leftovers.is_empty());
        // And it works.
        let c = tokio::net::UnixStream::connect(&path).await;
        assert!(c.is_ok());
    }

    #[tokio::test]
    async fn live_socket_is_not_stolen_but_stale_one_is_replaced() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("c.sock");
        let first = bind_secure(&path).await.unwrap();
        let err = bind_secure(&path).await.err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(first.listener);
        // The file is still there but nobody listens: a restart replaces it.
        assert!(path.exists());
        let again = bind_secure(&path).await.unwrap();
        assert_ne!(again.ino, 0);
    }

    #[tokio::test]
    async fn refuses_non_socket_and_long_paths() {
        let base = tempfile::tempdir().unwrap();
        let f = base.path().join("file");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(bind_secure(&f).await.err().unwrap().kind(), io::ErrorKind::AlreadyExists);
        let long = base.path().join("x".repeat(120));
        assert_eq!(bind_secure(&long).await.err().unwrap().kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn loose_existing_directory_is_reported_not_changed() {
        let base = tempfile::tempdir().unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let b = bind_secure(&base.path().join("c.sock")).await.unwrap();
        assert_eq!(b.warnings.len(), 1);
        assert_eq!(std::fs::metadata(base.path()).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn removes_only_its_own_file() {
        let base = tempfile::tempdir().unwrap();
        let p = base.path().join("x");
        std::fs::write(&p, b"regular").unwrap();
        remove_if_ours(&p, 1);
        assert!(p.exists());
    }

    #[test]
    fn default_path_shape() {
        // Not asserting the env-dependent prefix; the file name is fixed.
        assert!(default_socket_path().ends_with("control.sock"));
    }
}
