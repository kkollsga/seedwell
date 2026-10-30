//! Where the control connection lives and who may use it (same user only). How it travels is
//! [`crate::transport`]. There is no TCP listener, ever (SPEC 9A.1).

use std::path::PathBuf;

/// Longest socket path we accept. `sockaddr_un.sun_path` is 104 bytes on macOS (108 on Linux),
/// including the NUL.
pub const MAX_SOCKET_PATH: usize = 100;

/// Same user, nobody else. Root is *not* let in: the socket is for the user's own agents.
pub fn peer_allowed(peer_uid: u32, server_uid: u32) -> bool {
    peer_uid == server_uid
}

/// `control.sock` in Seedwell's state folder (`trs_platform::dirs::state_dir`: Application Support
/// on macOS, the XDG state folder elsewhere, `%LOCALAPPDATA%` on Windows, where it names a pipe).
/// `SEEDWELL_SOCKET` overrides it.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SEEDWELL_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    trs_platform::dirs::state_dir().unwrap_or_else(|| std::env::temp_dir().join("seedwell")).join("control.sock")
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

    #[test]
    fn default_path_shape() {
        // Not asserting the env-dependent prefix; the file name is fixed.
        assert!(default_socket_path().ends_with("control.sock"));
    }
}
