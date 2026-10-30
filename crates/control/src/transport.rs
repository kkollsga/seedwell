//! How the control connection travels (docs/CROSS-PLATFORM.md §2, the transport seam): a Unix socket
//! file on macOS and Linux, a named pipe on Windows. Both reach only the same user, and both are
//! named by one path (the pipe's name is derived from it, `trs_platform::ipc::pipe_name`). There is
//! no TCP listener, ever (SPEC 9A.1).

use std::io;
use std::path::Path;

pub use imp::{Conn, Listener, Owner, connect};

/// A bound listener and facts about it.
pub struct Bound {
    /// The listener.
    pub listener: Listener,
    /// Which socket file is ours, to remove only that one at shutdown.
    pub owner: Owner,
    /// Things worth telling the user (loose permissions on an existing directory).
    pub warnings: Vec<String>,
}

/// Listen at `path`, for this user only. A live listener there makes this fail with `AddrInUse`;
/// a leftover of a crashed run is replaced.
pub async fn bind(path: &Path) -> io::Result<Bound> {
    imp::bind(path).await
}

/// Listen at `path` inside a folder that is already private to this user (mode 0700): no staging
/// next to it, so a path as long as the system allows works. Connections are still checked.
pub async fn bind_in_private_dir(path: &Path) -> io::Result<Bound> {
    imp::bind_in_private_dir(path).await
}

/// Remove the socket file at `path` if it is still the one `owner` made (a named pipe goes by itself).
pub fn remove_if_ours(path: &Path, owner: &Owner) {
    imp::remove_if_ours(path, owner)
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::Path;

    use tokio::net::{UnixListener, UnixStream};

    use super::Bound;
    use crate::socket::{MAX_SOCKET_PATH, peer_allowed};

    /// One connection.
    pub type Conn = UnixStream;

    /// The socket file's inode: ours to remove.
    pub struct Owner(u64);

    /// Accepts connections of this user only.
    pub struct Listener {
        inner: UnixListener,
        uid: u32,
    }

    impl Listener {
        /// The next connection of this user. Others are closed without a word. Cancel safe.
        pub async fn accept(&mut self) -> io::Result<Conn> {
            loop {
                let (s, _) = self.inner.accept().await?;
                match s.peer_cred() {
                    Ok(c) if peer_allowed(c.uid(), self.uid) => return Ok(s),
                    _ => continue,
                }
            }
        }
    }

    /// Connect to the listener at `path`.
    pub async fn connect(path: &Path) -> io::Result<Conn> {
        UnixStream::connect(path).await
    }

    /// The socket is made with mode 0600 inside a 0700 directory, atomically:
    ///
    /// * The parent directory is created with mode 0700 if missing.
    /// * The socket is bound inside a fresh private 0700 directory next to the final path, chmodded
    ///   to 0600 and only then renamed into place, so there is no moment when another user could
    ///   connect to it.
    /// * A leftover socket from a crashed run is replaced; a live one makes this fail with
    ///   `AddrInUse`.
    pub async fn bind(path: &Path) -> io::Result<Bound> {
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
                if UnixStream::connect(path).await.is_ok() {
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
        let tmp_dir = parent.join(format!(".ctl-{}", rnd.iter().map(|b| format!("{b:02x}")).collect::<String>()));
        std::fs::DirBuilder::new().mode(0o700).create(&tmp_dir)?;
        let result = (|| -> io::Result<UnixListener> {
            let tmp_sock = tmp_dir.join("s");
            let listener = UnixListener::bind(&tmp_sock)?;
            std::fs::set_permissions(&tmp_sock, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp_sock, path)?;
            Ok(listener)
        })();
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let inner = result?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Bound { listener: Listener { inner, uid: meta.uid() }, owner: Owner(meta.ino()), warnings })
    }

    pub async fn bind_in_private_dir(path: &Path) -> io::Result<Bound> {
        let inner = UnixListener::bind(path)?;
        let meta = std::fs::symlink_metadata(path)?;
        Ok(Bound { listener: Listener { inner, uid: meta.uid() }, owner: Owner(meta.ino()), warnings: Vec::new() })
    }

    pub fn remove_if_ours(path: &Path, owner: &Owner) {
        if let Ok(m) = std::fs::symlink_metadata(path)
            && m.file_type().is_socket()
            && m.ino() == owner.0
        {
            let _ = std::fs::remove_file(path);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn socket_is_0600_in_a_0700_dir() {
            let base = tempfile::tempdir().unwrap();
            let path = base.path().join("Seedwell/control.sock");
            let b = bind(&path).await.unwrap();
            let sm = std::fs::metadata(&path).unwrap();
            assert!(sm.file_type().is_socket());
            assert_eq!(sm.mode() & 0o777, 0o600);
            let dm = std::fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(dm.mode() & 0o777, 0o700);
            assert_eq!(b.listener.uid, sm.uid());
            assert!(b.warnings.is_empty());
            // No temp directories are left behind.
            let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with(".ctl-"))
                .collect();
            assert!(leftovers.is_empty());
        }

        #[tokio::test]
        async fn a_stale_socket_is_replaced() {
            let base = tempfile::tempdir().unwrap();
            let path = base.path().join("c.sock");
            let first = bind(&path).await.unwrap();
            drop(first.listener);
            // The file is still there but nobody listens: a restart replaces it.
            assert!(path.exists());
            let again = bind(&path).await.unwrap();
            assert_ne!(again.owner.0, 0);
        }

        #[tokio::test]
        async fn refuses_non_socket_and_long_paths() {
            let base = tempfile::tempdir().unwrap();
            let f = base.path().join("file");
            std::fs::write(&f, b"x").unwrap();
            assert_eq!(bind(&f).await.err().unwrap().kind(), io::ErrorKind::AlreadyExists);
            let long = base.path().join("x".repeat(120));
            assert_eq!(bind(&long).await.err().unwrap().kind(), io::ErrorKind::InvalidInput);
        }

        #[tokio::test]
        async fn loose_existing_directory_is_reported_not_changed() {
            let base = tempfile::tempdir().unwrap();
            std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            let b = bind(&base.path().join("c.sock")).await.unwrap();
            assert_eq!(b.warnings.len(), 1);
            assert_eq!(std::fs::metadata(base.path()).unwrap().mode() & 0o777, 0o755);
        }

        #[tokio::test]
        async fn a_private_dir_takes_a_path_as_long_as_the_system_allows() {
            let base = tempfile::tempdir().unwrap();
            // 103 bytes in all: the most macOS takes (104 with the NUL), more than `bind` takes.
            let dir = base.path().join("d".repeat(103 - base.path().as_os_str().len() - "/".len() - "/x/s".len()));
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.join("x")).unwrap();
            let path = dir.join("x/s");
            assert!(path.as_os_str().len() > MAX_SOCKET_PATH);
            let mut b = bind_in_private_dir(&path).await.unwrap();
            let accept = tokio::spawn(async move { b.listener.accept().await.map(drop) });
            // Kept open until accepted: the peer check needs a live peer, and a client gone by then is
            // skipped, so the accept would wait forever.
            let _client = connect(&path).await.unwrap();
            accept.await.unwrap().unwrap();
        }

        #[test]
        fn removes_only_its_own_file() {
            let base = tempfile::tempdir().unwrap();
            let p = base.path().join("x");
            std::fs::write(&p, b"regular").unwrap();
            remove_if_ours(&p, &Owner(1));
            assert!(p.exists());
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
    use trs_platform::ipc::{OwnerOnly, create_pipe as create, pipe_name};

    use super::Bound;

    /// `ERROR_PIPE_BUSY`: every instance is taken for the moment.
    const PIPE_BUSY: i32 = 231;

    /// One connection: the server's or the client's end of a pipe.
    pub enum Conn {
        /// Accepted by a [`Listener`].
        Server(NamedPipeServer),
        /// Made by [`connect`].
        Client(NamedPipeClient),
    }

    /// Nothing to remove: a named pipe ends with its last handle.
    pub struct Owner;

    /// Accepts connections; the pipe's security lets only this user open it.
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
        security: OwnerOnly,
    }

    impl Listener {
        /// The next connection. Cancel safe: the waiting pipe instance stays for the next call.
        pub async fn accept(&mut self) -> io::Result<Conn> {
            self.next.connect().await?;
            let fresh = create(&self.name, &mut self.security, false)?;
            Ok(Conn::Server(std::mem::replace(&mut self.next, fresh)))
        }
    }

    /// Connect to the listener for `path`, waiting a little while every instance is busy.
    pub async fn connect(path: &Path) -> io::Result<Conn> {
        let name = pipe_name(path);
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(Conn::Client(c)),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) && Instant::now() < until => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn bind(path: &Path) -> io::Result<Bound> {
        let name = pipe_name(path);
        let mut security = OwnerOnly::new()?;
        let next = create(&name, &mut security, true).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                io::Error::new(io::ErrorKind::AddrInUse, format!("another instance is already serving {}", path.display()))
            } else {
                e
            }
        })?;
        Ok(Bound { listener: Listener { name, next, security }, owner: Owner, warnings: Vec::new() })
    }

    pub async fn bind_in_private_dir(path: &Path) -> io::Result<Bound> {
        bind(path).await
    }

    pub fn remove_if_ours(_path: &Path, _owner: &Owner) {}

    impl AsyncRead for Conn {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Conn::Server(p) => Pin::new(p).poll_read(cx, buf),
                Conn::Client(p) => Pin::new(p).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Conn {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Conn::Server(p) => Pin::new(p).poll_write(cx, buf),
                Conn::Client(p) => Pin::new(p).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Conn::Server(p) => Pin::new(p).poll_flush(cx),
                Conn::Client(p) => Pin::new(p).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Conn::Server(p) => Pin::new(p).poll_shutdown(cx),
                Conn::Client(p) => Pin::new(p).poll_shutdown(cx),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn a_connection_carries_bytes_both_ways() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("c.sock");
        let mut b = bind(&path).await.unwrap();
        let server = tokio::spawn(async move {
            let mut c = b.listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            c.read_exact(&mut buf).await.unwrap();
            c.write_all(&buf).await.unwrap();
            // A second client gets in too.
            let mut c2 = b.listener.accept().await.unwrap();
            c2.write_all(b"two").await.unwrap();
        });
        let mut c = connect(&path).await.unwrap();
        c.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        c.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
        let mut c2 = connect(&path).await.unwrap();
        let mut two = [0u8; 3];
        c2.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"two");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_live_listener_is_not_stolen() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("c.sock");
        let _first = bind(&path).await.unwrap();
        assert_eq!(bind(&path).await.err().unwrap().kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn nothing_listening_is_not_found() {
        let base = tempfile::tempdir().unwrap();
        let e = connect(&base.path().join("none.sock")).await.err().unwrap();
        assert!(matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused), "{e:?}");
    }
}
