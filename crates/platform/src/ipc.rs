//! Local connections between Seedwell's processes (the app or daemon, the CLI, agents).
//!
//! Unix uses a socket file whose path is the address. Windows uses a named pipe: its name is derived
//! from that same path, so one setting (`SEEDWELL_SOCKET`, `--socket`) names the address everywhere,
//! and only the user who made the pipe may open it.

use std::path::Path;

/// The named pipe for the control address `path`: `\\.\pipe\seedwell-<hash of the path>`. Two
/// spellings of one path (case, `/` or `\`) name the same pipe.
pub fn pipe_name(path: &Path) -> String {
    let key = path.to_string_lossy().replace('/', "\\").to_lowercase();
    // FNV-1a: stable across versions and builds, which `DefaultHasher` is not.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!(r"\\.\pipe\seedwell-{h:016x}")
}

/// Does a process serve the control address `path` right now? On Unix: a socket file is there. On
/// Windows: the pipe [`pipe_name`] exists, asked with `WaitNamedPipeW`, which (unlike opening the
/// pipe, or `Path::exists` on it) does not connect to it; a pipe whose instances are all busy counts.
pub fn is_served(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::{ERROR_SEM_TIMEOUT, GetLastError};
        use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
        let name: Vec<u16> = std::ffi::OsStr::new(&pipe_name(path)).encode_wide().chain([0]).collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call; GetLastError reads
        // this thread's last error, set by the call just before.
        unsafe { WaitNamedPipeW(name.as_ptr(), 1) != 0 || GetLastError() == ERROR_SEM_TIMEOUT }
    }
}

/// Security attributes that let only the current user (and nobody else, administrators included)
/// open the object they are given to; for [`pipe_name`] pipes.
#[cfg(windows)]
pub struct OwnerOnly {
    attrs: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is owned memory that nothing else mutates; it is only read by the system.
#[cfg(windows)]
unsafe impl Send for OwnerOnly {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for OwnerOnly {}

#[cfg(windows)]
impl OwnerOnly {
    /// A protected DACL with one entry: full access for this process's user.
    pub fn new() -> std::io::Result<OwnerOnly> {
        use std::io;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SAFETY: every pointer passed below is to a live local or to memory the system allocated
        // for us, which is freed exactly once (LocalFree, CloseHandle).
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            let err = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(err);
            }
            let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let mut sid_text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut n = 0;
            while *sid_text.add(n) != 0 {
                n += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, n));
            LocalFree(sid_text.cast());
            // D:P = a protected DACL (nothing inherited); A;;GA;;;<sid> = allow everything to the user.
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})").encode_utf16().chain(Some(0)).collect();
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnerOnly {
                attrs: SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
                descriptor,
            })
        }
    }

    /// The `SECURITY_ATTRIBUTES` to pass to the call that makes the object; valid while `self` lives.
    pub fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        (&raw mut self.attrs).cast()
    }
}

/// Make an instance of the pipe `name` that only this user can open and only on this machine;
/// `first` makes it fail (`PermissionDenied`) when the pipe exists already, which means another
/// process serves it.
#[cfg(windows)]
pub fn create_pipe(name: &str, security: &mut OwnerOnly, first: bool) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let mut o = tokio::net::windows::named_pipe::ServerOptions::new();
    o.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: the attributes and their descriptor stay valid for the call (`security` outlives it).
    unsafe { o.create_with_security_attributes_raw(name, security.as_ptr()) }
}

#[cfg(windows)]
impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from ConvertStringSecurityDescriptorToSecurityDescriptorW and
        // is freed once.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.descriptor) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_path_names_one_pipe() {
        let a = pipe_name(Path::new("C:/Users/Kari/AppData/Local/Seedwell/control.sock"));
        assert_eq!(a, pipe_name(Path::new(r"c:\users\kari\appdata\local\seedwell\control.sock")));
        assert_ne!(a, pipe_name(Path::new("C:/Users/Kari/AppData/Local/Seedwell/other.sock")));
        assert!(a.starts_with(r"\\.\pipe\seedwell-") && a.len() == r"\\.\pipe\seedwell-".len() + 16);
    }

    #[test]
    fn a_served_address_is_seen_and_an_unserved_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sock");
        assert!(!is_served(&path));
        #[cfg(unix)]
        let _server = std::os::unix::net::UnixListener::bind(&path).unwrap();
        #[cfg(windows)]
        let server = {
            use std::os::windows::ffi::OsStrExt as _;
            use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
            use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES};
            let name: Vec<u16> = std::ffi::OsStr::new(&pipe_name(&path)).encode_wide().chain([0]).collect();
            // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes.
            let h = unsafe { CreateNamedPipeW(name.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, 0, 0, 0, std::ptr::null()) };
            assert_ne!(h, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
            h
        };
        assert!(is_served(&path));
        assert!(is_served(&path), "asking twice: the check did not take the pipe's only instance");
        #[cfg(windows)]
        {
            // SAFETY: closes the pipe made above, once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(server) };
            assert!(!is_served(&path), "closed: not served any more");
        }
    }
}
