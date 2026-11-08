//! The torrent proxy over the control socket (SPEC §7H, 9A.2): `proxy.status`, `proxy.set` (on, off, the country),
//! `proxy.key` (the NordVPN key or a proxy of the user's own, tested before it is kept), `proxy.remove_key` and
//! `proxy.faster`, as Settings › Network's proxy box and its window do them. The app does the work (its secret store
//! holds the key, it hands the engine the servers); this is where the server finds it, filled in once the app runs.
//! A daemon has none, and says so.
//!
//! **The key never comes back**: no answer, error or activity entry holds it; `proxy.status` says only whether one is
//! installed.

use std::sync::{Arc, Mutex};

use crate::types::ProxyDto;

/// The app's torrent proxy, as the control server sees it. Each `Ok` is a sentence for the person or agent who asked;
/// an `Err` is a refusal in words (no key installed, the key failed its test, a test is running). Calls that test
/// block for the test (a few seconds).
pub trait ProxyControl: Send + Sync {
    /// On or off, which proxy, the country, whether a key is installed, the status line.
    fn status(&self) -> ProxyDto;
    /// Switch the proxy on or off, as the shield's click does (needs a key).
    fn switch(&self, on: bool) -> Result<String, String>;
    /// Where the user is, for the server choice (an ISO code, "" for no preference).
    fn country(&self, code: &str) -> Result<String, String>;
    /// NordVPN's service credentials: tested with NordVPN's proxy servers, then kept and switched on.
    fn nordvpn(&self, user: String, pass: String) -> Result<String, String>;
    /// A SOCKS5 proxy of the user's own (with a username and password, or none): tested, then kept and switched on.
    fn custom(&self, host: String, port: u16, key: Option<(String, String)>) -> Result<String, String>;
    /// Forget the key and switch the proxy off.
    fn remove_key(&self) -> Result<String, String>;
    /// Test every server now and use the best ("Find a Faster Server").
    fn faster(&self) -> Result<String, String>;
}

/// Where the control server finds the proxy (empty until the app registers it).
#[derive(Clone, Default)]
pub struct ProxySlot(Arc<Mutex<Option<Arc<dyn ProxyControl>>>>);

impl ProxySlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// The proxy from now on (replacing any earlier one).
    pub fn set(&self, p: Arc<dyn ProxyControl>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(p);
    }

    /// The proxy, if one registered.
    pub fn get(&self) -> Option<Arc<dyn ProxyControl>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl std::fmt::Debug for ProxySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.get().is_some() { "ProxySlot(set)" } else { "ProxySlot(empty)" })
    }
}
