//! Sealing secrets with the system's per-user protection, for a vault of key files (`trs_sync_net::secrets`): on
//! Windows DPAPI (`CryptProtectData` for the current user: only this user, on this PC, can open what it sealed).
//! The Mac keeps its keys in the Keychain through the app instead (the FFI's `SecretVault`), so it has no sealer
//! here; nor has Linux yet.
//!
//! Why DPAPI files rather than the Credential Manager: a credential's blob is at most 2,560 bytes
//! (`CRED_MAX_CREDENTIAL_BLOB_SIZE`) and the Credential Manager is one list for the whole user, while sealed files live
//! in the state folder they belong to. So a test run or a QA run with its own `--state-dir` never sees or touches the
//! user's keys, and removing a state folder removes its keys with it (records stay in their container).

use std::io;

/// Seals and opens secrets for this user.
pub trait Sealer: Send + Sync {
    /// `plain` sealed: only [`Sealer::unseal`] for this user (on this computer) opens it again.
    fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>>;
    /// What [`Sealer::seal`] sealed.
    fn unseal(&self, sealed: &[u8]) -> io::Result<Vec<u8>>;
    /// What it is called in messages ("the Windows data protection (DPAPI)").
    fn name(&self) -> &'static str;
}

/// This system's sealer for the state folder's key vault: DPAPI on Windows; `None` where keys stay where they are
/// (the Mac app passes its Keychain; elsewhere the 0600 file).
pub fn system_sealer() -> Option<std::sync::Arc<dyn Sealer>> {
    #[cfg(windows)]
    {
        Some(std::sync::Arc::new(Dpapi))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// DPAPI for the current user, without any prompt; Seedwell's own entropy, so another program of the user that
/// unseals DPAPI blobs blindly does not open these by chance.
#[cfg(windows)]
pub struct Dpapi;

#[cfg(windows)]
const ENTROPY: &[u8] = b"Seedwell key vault v1";

#[cfg(windows)]
impl Dpapi {
    fn call(data: &[u8], seal: bool) -> io::Result<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};
        let len = u32::try_from(data.len()).map_err(|_| io::Error::other("a secret this large cannot be sealed"))?;
        let input = CRYPT_INTEGER_BLOB { cbData: len, pbData: data.as_ptr().cast_mut() };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr().cast_mut() };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: the input blobs point at live slices that DPAPI only reads; the output is allocated by DPAPI, copied
        // out once and freed with LocalFree.
        unsafe {
            let ok = if seal {
                CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData.cast());
            Ok(bytes)
        }
    }
}

#[cfg(windows)]
impl Sealer for Dpapi {
    fn seal(&self, plain: &[u8]) -> io::Result<Vec<u8>> {
        Self::call(plain, true)
    }
    fn unseal(&self, sealed: &[u8]) -> io::Result<Vec<u8>> {
        Self::call(sealed, false)
    }
    fn name(&self) -> &'static str {
        "the Windows data protection (DPAPI)"
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Runs on Windows CI only: the real DPAPI for the user running the tests (nothing is stored).
    #[test]
    fn dpapi_seals_for_this_user_and_refuses_what_it_did_not_seal() {
        let s = system_sealer().expect("Windows has a sealer");
        let secret = [7u8; 32];
        let sealed = s.seal(&secret).unwrap();
        assert!(sealed.len() > secret.len() && !sealed.windows(32).any(|w| w == secret), "sealed, not plain");
        assert_eq!(s.unseal(&sealed).unwrap(), secret);
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(s.unseal(&bad).is_err());
    }
}
