//! Settings › Network › SOCKS5 proxy, for torrents only (SPEC §7H): the shield icon, its tooltip and status line,
//! the "SOCKS5 proxy" window with the NordVPN preset and its guide, and the calls that set, toggle and remove the
//! key. Every word is here; the host draws the models and passes the user's input back.
//!
//! **The key** (the proxy's username and password, for NordVPN the *service credentials* from Nord Account) lives in
//! the system's secret store (the Mac's Keychain through the app, DPAPI-sealed files on Windows, a 0600 file in a
//! test's state folder), under [`SECRET_NAME`]: never in `config.toml`, never in a log, never in a model. After it is
//! saved the host only learns that a key is installed. `config.toml` keeps whether the proxy is on, which kind it is
//! and a custom proxy's host and port (`[network.proxy]`).
//!
//! **NordVPN** is used only the way NordVPN documents it: its SOCKS5 servers ([`NORDVPN_SOCKS5_SERVERS`], from its
//! own setup guides) on port 1080, with the service credentials. No other NordVPN interface is ever contacted. The
//! engine picks the server that answers fastest and switches when one fails (`seedwell_engine::proxy`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use seedwell_engine::{ProxyConfig, ProxyHealth, ProxyProblem, ProxyStatus};
use seedwell_socks::Credentials;
use seedwell_state::config::{ProxyKind as StateKind, TorrentProxy};
use seedwell_sync_net::secrets::SecretStore;

use crate::Core;
use crate::types::CoreError;

/// NordVPN's SOCKS5 proxy servers, as NordVPN lists them in its own proxy setup guides (all on port
/// [`NORDVPN_PORT`], with the NordVPN service credentials):
///
/// - "NordVPN proxy setup for qBittorrent", <https://support.nordvpn.com/hc/en-us/articles/20195967385745>
/// - "NordVPN proxy setup on Deluge", <https://support.nordvpn.com/hc/en-us/articles/20341624031505>
///
/// Both list these twelve names (checked 2026-11-05). When NordVPN's guides change, change this list; Seedwell never
/// asks NordVPN for a server list any other way. Each name says its country (`nl`, `se`, `us`) and, for most, its
/// city; the country is read from the name ([`NordServer::country`]), the city only shown.
pub const NORDVPN_SOCKS5_SERVERS: [NordServer; 12] = [
    NordServer { host: "nl.socks.nordhold.net", city: "" },
    NordServer { host: "se.socks.nordhold.net", city: "" },
    NordServer { host: "us.socks.nordhold.net", city: "" },
    NordServer { host: "amsterdam.nl.socks.nordhold.net", city: "Amsterdam" },
    NordServer { host: "atlanta.us.socks.nordhold.net", city: "Atlanta" },
    NordServer { host: "chicago.us.socks.nordhold.net", city: "Chicago" },
    NordServer { host: "dallas.us.socks.nordhold.net", city: "Dallas" },
    NordServer { host: "los-angeles.us.socks.nordhold.net", city: "Los Angeles" },
    NordServer { host: "new-york.us.socks.nordhold.net", city: "New York" },
    NordServer { host: "phoenix.us.socks.nordhold.net", city: "Phoenix" },
    NordServer { host: "san-francisco.us.socks.nordhold.net", city: "San Francisco" },
    NordServer { host: "stockholm.se.socks.nordhold.net", city: "Stockholm" },
];

/// One of NordVPN's SOCKS5 servers.
pub struct NordServer {
    pub host: &'static str,
    /// The city its name gives, for display (empty: the name gives only the country).
    pub city: &'static str,
}

impl NordServer {
    /// The ISO country code its name gives: the label before `socks` (`amsterdam.nl.socks...` → `NL`).
    pub fn country(&self) -> String {
        let labels: Vec<&str> = self.host.split('.').collect();
        labels.iter().position(|l| *l == "socks").and_then(|i| i.checked_sub(1)).map_or(String::new(), |i| labels[i].to_ascii_uppercase())
    }
}

/// The servers to choose from, their countries and their port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    pub servers: Vec<String>,
    /// ISO codes, one per server (empty: unknown).
    pub countries: Vec<String>,
    /// Cities, one per server (empty: not known), for display.
    pub cities: Vec<String>,
    pub port: u16,
}

/// NordVPN's SOCKS5 port (the same guides).
pub const NORDVPN_PORT: u16 = 1080;

/// Where the key is kept in the secret store.
pub const SECRET_NAME: &str = "torrent-proxy-key";

/// How often a host asks for [`ProxyModel`] again: quickly while something is being tested, else every few seconds.
pub const REFRESH_BUSY_MS: u32 = 500;
pub const REFRESH_MS: u32 = 3000;

/// The shield icon's five looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProxyIcon {
    /// No key installed: all grey.
    NoKey,
    /// A key is installed and the proxy is off: green outline, dark grey logo.
    Off,
    /// On and working: all green, white logo.
    On,
    /// On and working, but slow: orange.
    Slow,
    /// On and not working (torrents wait for the proxy): red.
    Failed,
}

/// Which proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProxyKind {
    /// The NordVPN preset.
    NordVpn,
    /// A SOCKS5 proxy the user names.
    Custom,
}

/// The shield icon and the Network settings' proxy area, ready to draw.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ProxyModel {
    pub icon: ProxyIcon,
    /// The icon's tooltip: what it shows and what a click does.
    pub tooltip: String,
    /// One line for Settings › Network ("On · NordVPN · nl.socks.nordhold.net · 84 ms"; "Waiting for proxy · NordVPN", with the
    /// reason in `problem`, shown under it).
    pub status_line: String,
    /// A key is installed (the key itself never leaves Rust).
    pub has_key: bool,
    /// The proxy is switched on.
    pub enabled: bool,
    pub kind: ProxyKind,
    /// The server in use, as shown ("nl.socks.nordhold.net"); empty when none.
    pub server: String,
    /// The server's last handshake, in ms.
    pub latency_ms: Option<u32>,
    /// What is wrong, in words (with [`ProxyIcon::Failed`], or the last failed action).
    pub problem: Option<String>,
    /// A click on the icon opens the window (no key) instead of switching the proxy on or off.
    pub click_opens_window: bool,
    /// The right-click menu: the entry that opens the window.
    pub menu_open_label: String,
    /// The right-click menu: the entry that switches on or off (empty without a key).
    pub menu_toggle_label: String,
    /// Ask again after this many ms.
    pub refresh_ms: u32,
    /// The right-click menu's country entry, "Country: Sweden…" (the chosen one's name). It opens the country list
    /// (not a submenu: a menu cannot be limited in height): `country_visible_rows` rows tall with a scrollbar,
    /// `countries` in order ("No preference" first), a checkmark on `country` ("" none), scrolled so it shows.
    pub country_menu_label: String,
    pub countries: Vec<ProxyCountry>,
    pub country: String,
    /// How many rows of the country list show at once (the rest scroll).
    pub country_visible_rows: u32,
    /// "Find a Faster Server" (empty unless the proxy is on).
    pub find_faster_label: String,
}

/// A country to choose from: its ISO code (the value to pass back) and its name. The first is "No preference"
/// (code "").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ProxyCountry {
    pub code: String,
    pub name: String,
}

/// The "SOCKS5 proxy" window, ready to draw.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ProxyWindowModel {
    /// "SOCKS5 proxy".
    pub title: String,
    /// What goes through it and what does not.
    pub explanation: String,
    /// What happens when it stops working (the kill switch).
    pub kill_switch: String,
    /// "A NordVPN key is installed." / "No key is installed."
    pub key_line: String,
    pub has_key: bool,
    pub kind: ProxyKind,
    /// The NordVPN section.
    pub nordvpn_title: String,
    pub nordvpn_intro: String,
    /// Shows or hides the guide ("How do I get the service credentials?").
    pub guide_show_label: String,
    pub guide_hide_label: String,
    /// The guide's steps, in order.
    pub guide_steps: Vec<String>,
    /// Nord Account's address, for the guide's first step.
    pub nord_account_url: String,
    /// The guide's link to it ("Open Nord Account").
    pub nord_account_label: String,
    pub username_placeholder: String,
    pub password_placeholder: String,
    /// Under the two fields: copying the username and then the password in Nord Account fills them while the window
    /// is open ([`proxy_paste_fill`]).
    pub paste_hint: String,
    /// Under the fields while one of them was filled from a copy ("Filled from what you copied.").
    pub pasted_note: String,
    /// "Enable NordVPN".
    pub enable_nordvpn_label: String,
    /// The custom proxy section.
    pub custom_title: String,
    pub custom_intro: String,
    pub host_label: String,
    pub port_label: String,
    /// The saved custom host and port (not secret), to fill the fields.
    pub custom_host: String,
    pub custom_port: String,
    pub custom_user_label: String,
    pub custom_password_label: String,
    /// "Use This Proxy".
    pub use_custom_label: String,
    /// With a key installed, its pane shows this short line and `replace_reveal_label` instead of the empty setup
    /// form: "NordVPN key installed" (NordVPN's pane) or "Proxy installed: host:port" (the custom pane); empty in the
    /// other pane and without a key, where the form shows.
    pub nordvpn_installed: String,
    pub custom_installed: String,
    /// "Replace Key…": shows that pane's fields again, with `replace_label` in place of Enable NordVPN or Use This
    /// Proxy, and `replace_cancel_label` before it to hide them.
    pub replace_reveal_label: String,
    pub replace_label: String,
    pub replace_cancel_label: String,
    /// "Remove Key" (shown with a key).
    pub remove_key_label: String,
    pub close_label: String,
    /// The country picker: its label, a line saying what it does, the choices (the first "No preference") and the
    /// one chosen ("" none). A button showing `country_name` opens the country list, `country_visible_rows` rows tall
    /// with a scrollbar, as the right-click menu's does.
    pub country_label: String,
    pub country_hint: String,
    pub countries: Vec<ProxyCountry>,
    pub country: String,
    /// The chosen country's name ("No preference" with none).
    pub country_name: String,
    /// How many rows of the country list show at once (the rest scroll).
    pub country_visible_rows: u32,
    /// "Find a Faster Server" (empty unless the proxy is on); its progress and result come in `progress` and
    /// `result`.
    pub find_faster_label: String,
    /// While a key is being tested: "Testing the key with NordVPN's servers…".
    pub progress: String,
    /// The last action's result, a success or what went wrong and what to do.
    pub result: String,
    pub result_is_problem: bool,
}

/// The last action, while it runs and after.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Job {
    pub progress: Option<String>,
    /// `(problem, sentence)`.
    pub result: Option<(bool, String)>,
}

/// What the core keeps of the proxy between calls: whether a key is installed (read from the store once, then kept
/// up to date by the calls that change it, so the per-second model never touches the Keychain) and the last action.
#[derive(Default)]
pub struct ProxyCell {
    has_key: Mutex<Option<bool>>,
    job: Mutex<Job>,
    /// The store of a backend without one (the mock): in memory, gone at quit.
    mem: Arc<MemSecrets>,
    /// Tests: loopback servers in place of NordVPN's (tests never reach a real NordVPN server).
    #[cfg(test)]
    preset: Mutex<Option<Preset>>,
}

#[derive(Default)]
struct MemSecrets(Mutex<std::collections::HashMap<String, Vec<u8>>>);

impl SecretStore for MemSecrets {
    fn get(&self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        Ok(lock(&self.0).get(name).cloned())
    }
    fn put(&self, name: &str, value: &[u8]) -> std::io::Result<()> {
        lock(&self.0).insert(name.into(), value.to_vec());
        Ok(())
    }
    fn delete(&self, name: &str) -> std::io::Result<()> {
        lock(&self.0).remove(name);
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------------------------------ the key

/// The key as stored: `v1`, then the username and the password, each after a NUL (neither may hold one). Empty
/// username and password: a custom proxy without login.
fn encode(c: Option<&Credentials>) -> Vec<u8> {
    let mut v = b"v1".to_vec();
    v.push(0);
    if let Some(c) = c {
        v.extend_from_slice(c.user().as_bytes());
        v.push(0);
        v.extend_from_slice(c.pass().as_bytes());
    } else {
        v.push(0);
    }
    v
}

/// The stored key: `Some(None)` is a key without login (a custom proxy that needs none).
fn decode(mut b: Vec<u8>) -> Option<Option<Credentials>> {
    let out = (|| {
        let rest = b.strip_prefix(b"v1\0")?;
        let i = rest.iter().position(|&c| c == 0)?;
        let (u, p) = (std::str::from_utf8(&rest[..i]).ok()?, std::str::from_utf8(&rest[i + 1..]).ok()?);
        if u.is_empty() && p.is_empty() { Some(None) } else { Credentials::new(u, p).ok().map(Some) }
    })();
    b.fill(0);
    out
}

/// Reads the key (`None`: none installed or unreadable).
pub fn read_key(store: &dyn SecretStore) -> Option<Option<Credentials>> {
    store.get(SECRET_NAME).ok().flatten().and_then(decode)
}

/// The engine's proxy for the settings and the key: `None` when off. On without a key is a proxy with no server,
/// which holds every torrent (never a direct connection).
pub fn engine_proxy(cfg: &TorrentProxy, key: Option<Option<Credentials>>, preset: &Preset) -> Option<ProxyConfig> {
    if !cfg.enabled {
        return None;
    }
    let Some(auth) = key else {
        return Some(ProxyConfig::default());
    };
    Some(match cfg.kind {
        StateKind::Nordvpn => {
            let codes: Vec<&str> = preset.countries.iter().map(String::as_str).collect();
            ProxyConfig { servers: preset.servers.clone(), port: preset.port, auth, preference: crate::proxy_countries::preference(&cfg.country, &codes) }
        }
        StateKind::Custom => ProxyConfig {
            servers: if cfg.host.trim().is_empty() { Vec::new() } else { vec![cfg.host.trim().to_owned()] },
            port: if cfg.port == 0 { 1080 } else { cfg.port },
            auth,
            preference: Vec::new(),
        },
    })
}

/// How many rows of the country list show at once, in the menu's and the window's (the user's request: about 60
/// countries were too long a list).
pub const COUNTRY_VISIBLE_ROWS: u32 = 6;

/// The chosen country's name among `country_choices` ("No preference" for "" or a code no longer listed).
fn country_name(code: &str) -> String {
    let choices = country_choices();
    choices.iter().find(|c| c.code == code).unwrap_or(&choices[0]).name.clone()
}

/// The countries to choose from: "No preference" first, then by name.
pub fn country_choices() -> Vec<ProxyCountry> {
    let mut countries: Vec<ProxyCountry> =
        crate::proxy_countries::COUNTRIES.iter().map(|c| ProxyCountry { code: c.code.into(), name: c.name.into() }).collect();
    countries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut v = vec![ProxyCountry { code: String::new(), name: "No preference".into() }];
    v.extend(countries);
    v
}

/// "nl.socks.nordhold.net (Netherlands, 38 ms)": a server with its country (from the preset) and its handshake.
fn server_with_place(server: &str, preset: &Preset, kind: StateKind, ms: Option<u32>) -> String {
    let host = server.rsplit_once(':').map_or(server, |(h, _)| h);
    let i = preset.servers.iter().position(|s| s == host.trim_matches(['[', ']']));
    let city = i.and_then(|i| preset.cities.get(i)).filter(|c| !c.is_empty()).cloned();
    let country = i.and_then(|i| preset.countries.get(i)).and_then(|c| crate::proxy_countries::country(c)).map(|c| c.name.to_owned());
    let inside: Vec<String> = city.into_iter().chain(country).chain(ms.map(fmt_latency)).collect();
    let shown = shown_server(server, kind);
    if inside.is_empty() { shown } else { format!("{shown} ({})", inside.join(", ")) }
}

// ------------------------------------------------------------------------------------------ words

fn kind_of(k: StateKind) -> ProxyKind {
    match k {
        StateKind::Nordvpn => ProxyKind::NordVpn,
        StateKind::Custom => ProxyKind::Custom,
    }
}

fn name_of(k: StateKind) -> &'static str {
    match k {
        StateKind::Nordvpn => "NordVPN",
        StateKind::Custom => "the proxy",
    }
}

/// What is wrong with a proxy that does not work, in words.
pub fn problem_text(p: &ProxyProblem, kind: StateKind) -> String {
    match p {
        ProxyProblem::NoServer => "No key is installed.".into(),
        ProxyProblem::AuthRejected if kind == StateKind::Nordvpn => {
            "NordVPN turned the key down. Use the service credentials from Nord Account, not your Nord Account login.".into()
        }
        ProxyProblem::AuthRejected => "The proxy turned the username and password down.".into(),
        ProxyProblem::AuthMissing => "The proxy asks for a username and password.".into(),
        ProxyProblem::Unreachable(why) => format!("No server answered. {}", sentence(why)),
    }
}

/// A phrase from the proxy's errors ("the proxy refused the connection") as a sentence of its own: "The proxy refused
/// the connection." The problem lines are short sentences one after another, never a phrase in nested parentheses.
fn sentence(phrase: &str) -> String {
    let p = phrase.trim().trim_end_matches('.');
    let mut chars = p.chars();
    match chars.next() {
        Some(c) => format!("{}{}.", c.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// Why the system's secure storage refused, as a sentence (empty when the system says nothing a person can act on):
/// never its own text with "os error N".
fn storage_why(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => " Seedwell isn\u{2019}t allowed to use it.",
        std::io::ErrorKind::StorageFull => " The disk is full.",
        std::io::ErrorKind::ReadOnlyFilesystem => " The disk is read-only.",
        _ => "",
    }
}

/// The host shown for a server label (`host:port`): the host alone for NordVPN, everything for a custom proxy.
fn shown_server(label: &str, kind: StateKind) -> String {
    match kind {
        StateKind::Nordvpn => label.rsplit_once(':').map_or(label, |(h, _)| h).to_owned(),
        StateKind::Custom => label.to_owned(),
    }
}

fn fmt_latency(ms: u32) -> String {
    if ms >= 1000 { format!("{:.1} s", f64::from(ms) / 1000.0) } else { format!("{ms} ms") }
}

/// The model from the settings, whether a key is installed, the engine's status and the last action.
pub fn model(cfg: &TorrentProxy, has_key: bool, st: &ProxyStatus, job: &Job) -> ProxyModel {
    let name = name_of(cfg.kind);
    let title = match cfg.kind {
        StateKind::Nordvpn => "NordVPN",
        StateKind::Custom => "SOCKS5 proxy",
    };
    let on = has_key && cfg.enabled;
    let server = if on { st.server.as_deref().map(|s| shown_server(s, cfg.kind)).unwrap_or_default() } else { String::new() };
    let latency_ms = if on { st.handshake_ms } else { None };
    let mut problem = None;
    let (icon, tooltip, status_line) = if !has_key {
        (ProxyIcon::NoKey, "Torrents connect directly. No proxy key is installed: click to set one up.".to_string(), "No key installed".to_string())
    } else if !cfg.enabled {
        (ProxyIcon::Off, format!("Torrents connect directly. Click to send them through {name}."), format!("Off \u{b7} {title}"))
    } else {
        match st.health {
            ProxyHealth::Up | ProxyHealth::Checking | ProxyHealth::Off if st.problem.is_none() => {
                let at = if server.is_empty() { String::new() } else { format!(" ({server}{})", latency_ms.map(|m| format!(", {}", fmt_latency(m))).unwrap_or_default()) };
                let line = if server.is_empty() { format!("On \u{b7} {title} \u{b7} testing the servers\u{2026}") } else { format!("On \u{b7} {title} \u{b7} {server}{}", latency_ms.map(|m| format!(" \u{b7} {}", fmt_latency(m))).unwrap_or_default()) };
                (ProxyIcon::On, format!("Torrents go through {name}{at}. Click to turn it off."), line)
            }
            ProxyHealth::Slow => {
                let took = latency_ms.map(fmt_latency).unwrap_or_default();
                (
                    ProxyIcon::Slow,
                    format!("Torrents go through {name} ({server}), but it is slow: {took} to answer. Click to turn it off."),
                    format!("Slow \u{b7} {title} \u{b7} {server} \u{b7} {took}"),
                )
            }
            _ => {
                let why = st.problem.as_ref().map_or_else(|| "It doesn\u{2019}t answer.".to_string(), |p| problem_text(p, cfg.kind));
                problem = Some(why.clone());
                (
                    ProxyIcon::Failed,
                    format!("Torrents are waiting for the proxy: {why} They never connect without it while it is on. Click to turn it off."),
                    // The reason is `problem`, under the line where the line is shown: not said twice.
                    format!("Waiting for proxy \u{b7} {title}"),
                )
            }
        }
    };
    if problem.is_none()
        && let Some((true, r)) = &job.result
    {
        problem = Some(r.clone());
    }
    let mut tooltip = tooltip;
    if on && let Some((host, ms)) = &st.passed_over {
        tooltip.push_str(&format!(" ({} is nearer but answered slowly, in {}.)", shown_server(host, cfg.kind), fmt_latency(*ms)));
    }
    let status_line = match &job.progress {
        Some(p) => p.clone(),
        None => status_line,
    };
    let busy = job.progress.is_some() || (on && st.health == ProxyHealth::Checking);
    ProxyModel {
        icon,
        tooltip,
        status_line,
        has_key,
        enabled: cfg.enabled,
        kind: kind_of(cfg.kind),
        server,
        latency_ms,
        problem,
        click_opens_window: !has_key,
        menu_open_label: "SOCKS5 Proxy\u{2026}".into(),
        menu_toggle_label: if !has_key {
            String::new()
        } else if cfg.enabled {
            format!("Turn Off {title}")
        } else {
            format!("Turn On {title}")
        },
        refresh_ms: if busy { REFRESH_BUSY_MS } else { REFRESH_MS },
        country_menu_label: format!("Country: {}\u{2026}", country_name(&cfg.country)),
        countries: country_choices(),
        country: cfg.country.clone(),
        country_visible_rows: COUNTRY_VISIBLE_ROWS,
        find_faster_label: if on { FIND_FASTER.into() } else { String::new() },
    }
}

/// The answer to a switch or Remove Key while a test runs.
const WAIT_FOR_TEST: &str = "Wait until the test that is running has finished.";

/// The settings file could not be written (the system's own words stay out of what the user reads).
const SETTINGS_NOT_SAVED: &str = "Seedwell could not save its settings, so the proxy stays as it was.";

/// The button and menu entry that ranks the servers again now.
const FIND_FASTER: &str = "Find a Faster Server";

/// Nord Account, where the service credentials are.
pub const NORD_ACCOUNT_URL: &str = "https://my.nordaccount.com/";

/// The guide to the service credentials: the clicks in Nord Account.
pub fn guide_steps() -> Vec<String> {
    vec![
        "Open Nord Account (my.nordaccount.com) and log in.".into(),
        "Choose NordVPN, then Advanced settings.".into(),
        "Under \u{201c}Set up NordVPN manually\u{201d}, press Set up, and enter the code NordVPN sends to your email.".into(),
        "Under Service credentials, copy the username and the password. They are not your email and Nord Account password.".into(),
        "Paste them here and press Enable NordVPN. Seedwell tests them with NordVPN\u{2019}s proxy servers before it keeps them.".into(),
    ]
}

/// What a text copied while the "SOCKS5 proxy" window is open fills in: the NordVPN username, or the password.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProxyPaste {
    Username { text: String },
    Password { text: String },
}

/// How long after the username came from a copy the next copy may fill the password: the two copies in Nord Account,
/// one after the other. Later, an unrelated secret copied while the window happens to be open would go to NordVPN.
pub const PASTE_PAIR_MS: u64 = 120_000;

/// The window's paste-friendly fields: the host reads the pasteboard only while the window is open and only when
/// its change count changes, and hands each newly copied text here with what the two NordVPN fields hold and how long
/// ago the username was filled from a copy (`username_copied_ms_ago`; `None`: typed, or not filled). Copying the
/// username and then the password in Nord Account fills them in that order: the username when it is empty; the
/// password only when it is empty and the username came from a copy within [`PASTE_PAIR_MS`]. A field that holds
/// something is never changed. Text that cannot be a service credential (empty, spaces or line breaks in it, a link,
/// longer than SOCKS5 allows; an email address as the username) fills nothing, nor does the username copied again.
/// Whenever a field was filled from a copy, the host shows `ProxyWindowModel::pasted_note` under the fields.
#[uniffi::export]
pub fn proxy_paste_fill(username: String, password: String, copied: String, username_copied_ms_ago: Option<u64>) -> Option<ProxyPaste> {
    let text = copied.trim();
    if text.is_empty() || text.len() > 255 || text.chars().any(char::is_whitespace) || text.contains("://") {
        return None;
    }
    if username.trim().is_empty() {
        return (!text.contains('@')).then(|| ProxyPaste::Username { text: text.to_owned() });
    }
    let paired = username_copied_ms_ago.is_some_and(|ms| ms <= PASTE_PAIR_MS);
    (paired && password.is_empty() && text != username.trim()).then(|| ProxyPaste::Password { text: text.to_owned() })
}

/// The window's model.
pub fn window(cfg: &TorrentProxy, has_key: bool, job: &Job) -> ProxyWindowModel {
    let (result_is_problem, result) = job.result.clone().unwrap_or_default();
    ProxyWindowModel {
        title: "SOCKS5 proxy".into(),
        explanation: concat!(
            "Only torrents go through the proxy: their peers and their trackers. Folder sync, chats and your devices ",
            "always connect directly. While the proxy is on, uTP, UDP trackers, the DHT for torrents, opening the torrent ",
            "port on the router and incoming connections are off, because the proxy carries none of them."
        )
        .into(),
        kill_switch: "If the proxy stops working, torrents wait until it works again. They never connect without it while it is on.".into(),
        key_line: match (has_key, cfg.kind) {
            (false, _) => "No key is installed.".into(),
            (true, StateKind::Nordvpn) => "A NordVPN key is installed. It is kept in the system\u{2019}s secure storage and never shown.".into(),
            (true, StateKind::Custom) => format!(
                "A proxy is set up ({}). Its password is kept in the system\u{2019}s secure storage and never shown.",
                if cfg.port == 0 { cfg.host.clone() } else { format!("{}:{}", cfg.host, cfg.port) }
            ),
        },
        has_key,
        kind: kind_of(cfg.kind),
        nordvpn_title: "NordVPN".into(),
        nordvpn_intro: "Use NordVPN\u{2019}s SOCKS5 proxy servers with your NordVPN service credentials. Seedwell picks the server that answers fastest and switches when one stops working.".into(),
        guide_show_label: "How do I get the service credentials?".into(),
        guide_hide_label: "Hide the steps".into(),
        guide_steps: guide_steps(),
        nord_account_url: NORD_ACCOUNT_URL.into(),
        nord_account_label: "Open Nord Account".into(),
        username_placeholder: "Service username".into(),
        password_placeholder: "Service password".into(),
        paste_hint: "While this window is open, copy the username and then the password in Nord Account: they fill in here.".into(),
        pasted_note: "Filled from what you copied. Check it before you go on.".into(),
        enable_nordvpn_label: "Enable NordVPN".into(),
        custom_title: "Another SOCKS5 proxy".into(),
        custom_intro: "Any SOCKS5 proxy that connects to addresses and names for you. Username and password are optional.".into(),
        host_label: "Server".into(),
        port_label: "Port".into(),
        custom_host: if cfg.kind == StateKind::Custom { cfg.host.clone() } else { String::new() },
        custom_port: if cfg.kind == StateKind::Custom && cfg.port != 0 { cfg.port.to_string() } else { "1080".into() },
        custom_user_label: "Username (optional)".into(),
        custom_password_label: "Password (optional)".into(),
        use_custom_label: "Use This Proxy".into(),
        nordvpn_installed: if has_key && cfg.kind == StateKind::Nordvpn { "NordVPN key installed".into() } else { String::new() },
        custom_installed: if has_key && cfg.kind == StateKind::Custom {
            format!("Proxy installed: {}", if cfg.port == 0 { cfg.host.clone() } else { format!("{}:{}", cfg.host, cfg.port) })
        } else {
            String::new()
        },
        replace_reveal_label: "Replace Key\u{2026}".into(),
        replace_label: "Replace Key".into(),
        replace_cancel_label: "Cancel".into(),
        remove_key_label: "Remove Key".into(),
        close_label: "Close".into(),
        country_label: "Your country".into(),
        country_hint: "Seedwell prefers servers in this country, then the nearest ones, as long as they answer quickly. It never detects where you are.".into(),
        countries: country_choices(),
        country: cfg.country.clone(),
        country_name: country_name(&cfg.country),
        country_visible_rows: COUNTRY_VISIBLE_ROWS,
        find_faster_label: if has_key && cfg.enabled { FIND_FASTER.into() } else { String::new() },
        progress: job.progress.clone().unwrap_or_default(),
        result,
        result_is_problem,
    }
}

// ------------------------------------------------------------------------------------------ testing a key

/// Tests `servers` on `port` with `auth` (all at once, each within [`seedwell_engine::proxy::PROBE_TIMEOUT`]): the fastest
/// that answered, or why none did. On a runtime of its own: the caller is a thread of its own, never a UI thread.
fn test_servers(servers: &[String], port: u16, auth: Option<&Credentials>) -> Result<(String, Duration), ProxyProblem> {
    if servers.is_empty() {
        return Err(ProxyProblem::NoServer);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ProxyProblem::Unreachable("Seedwell couldn\u{2019}t start the test".into()))?;
    let results = rt.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        for (i, host) in servers.iter().enumerate() {
            let p = seedwell_socks::Proxy { host: host.clone(), port, auth: auth.cloned() };
            set.spawn(async move { (i, seedwell_socks::handshake(&p, seedwell_engine::proxy::PROBE_TIMEOUT).await) });
        }
        let mut out: Vec<Result<Duration, seedwell_socks::Error>> = servers.iter().map(|_| Err(seedwell_socks::Error::Timeout)).collect();
        while let Some(Ok((i, r))) = set.join_next().await {
            out[i] = r;
        }
        out
    });
    let best = results.iter().enumerate().filter_map(|(i, r)| r.as_ref().ok().map(|d| (i, *d))).min_by_key(|(i, d)| (*d, *i));
    if let Some((i, d)) = best {
        return Ok((servers[i].clone(), d));
    }
    let errs: Vec<&seedwell_socks::Error> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    if errs.iter().any(|e| **e == seedwell_socks::Error::AuthRejected) {
        return Err(ProxyProblem::AuthRejected);
    }
    if errs.iter().any(|e| **e == seedwell_socks::Error::NoMethod) {
        return Err(ProxyProblem::AuthMissing);
    }
    Err(errs.last().map_or(ProxyProblem::NoServer, |e| ProxyProblem::Unreachable(e.to_string())))
}

/// Why a NordVPN key was not saved, in words.
fn nordvpn_refusal(p: &ProxyProblem, user: &str) -> String {
    match p {
        ProxyProblem::AuthRejected | ProxyProblem::AuthMissing => {
            let email = if user.contains('@') { " (an email address is never the service username)" } else { "" };
            format!(
                "NordVPN turned these down: these look like your Nord Account login, not the service credentials{email}. \
                 Copy the service credentials from Nord Account \u{203a} NordVPN \u{203a} Advanced settings \u{203a} Set up NordVPN manually. The key was not saved."
            )
        }
        ProxyProblem::Unreachable(why) => {
            format!("None of NordVPN\u{2019}s proxy servers answered. {} Check the Internet connection and try again. The key was not saved.", sentence(why))
        }
        ProxyProblem::NoServer => "There is no NordVPN server to test. The key was not saved.".into(),
    }
}

/// Why a custom proxy was not saved, in words.
fn custom_refusal(p: &ProxyProblem, label: &str) -> String {
    match p {
        ProxyProblem::AuthRejected => format!("{label} turned the username and password down. Nothing was saved."),
        ProxyProblem::AuthMissing => format!("{label} asks for a username and password. Nothing was saved."),
        ProxyProblem::Unreachable(why) => format!("{label} didn\u{2019}t answer as a SOCKS5 proxy. {} Nothing was saved.", sentence(why)),
        ProxyProblem::NoServer => "Enter the proxy\u{2019}s server. Nothing was saved.".into(),
    }
}

// ------------------------------------------------------------------------------------------ the calls

/// NordVPN's documented servers, their countries and the port.
pub fn nordvpn_preset() -> Preset {
    Preset {
        servers: NORDVPN_SOCKS5_SERVERS.iter().map(|s| s.host.to_owned()).collect(),
        countries: NORDVPN_SOCKS5_SERVERS.iter().map(NordServer::country).collect(),
        cities: NORDVPN_SOCKS5_SERVERS.iter().map(|s| s.city.to_owned()).collect(),
        port: NORDVPN_PORT,
    }
}

/// Where an isolated run's stand-ins for NordVPN's servers are named, in its state folder.
pub const QA_SERVERS_FILE: &str = "qa/proxy-servers.txt";

/// The servers of an isolated (test or QA) run, which never reaches NordVPN: the loopback fakes
/// `<state>/qa/proxy-servers.txt` names, one `host:port [country] [city]` per line (`127.0.0.1:41080 NL Amsterdam`,
/// `[::1]:41080 SE`; every one on the first one's port), and none without that file (a key test then finds no
/// server, and nothing is kept).
pub fn isolated_preset(dir: &std::path::Path) -> Preset {
    let text = std::fs::read_to_string(dir.join(QA_SERVERS_FILE)).unwrap_or_default();
    let mut p = Preset { servers: Vec::new(), countries: Vec::new(), cities: Vec::new(), port: NORDVPN_PORT };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        let Some((host, port)) = words.next().and_then(|a| a.rsplit_once(':')) else { continue };
        let Ok(port) = port.parse::<u16>() else { continue };
        if p.servers.is_empty() {
            p.port = port;
        }
        p.servers.push(host.trim_matches(['[', ']']).to_owned());
        p.countries.push(words.next().unwrap_or_default().to_ascii_uppercase());
        p.cities.push(words.collect::<Vec<_>>().join(" "));
    }
    p
}

/// The servers to choose from: NordVPN's, or an isolated run's stand-ins ([`isolated_preset`]).
pub fn preset_for(dir: &std::path::Path, isolated: bool) -> Preset {
    if isolated { isolated_preset(dir) } else { nordvpn_preset() }
}

impl Core {
    fn preset(&self) -> Preset {
        #[cfg(test)]
        if let Some(p) = lock(&self.proxy.preset).clone() {
            return p;
        }
        preset_for(&self.dir, self.isolated)
    }

    fn proxy_store(&self) -> Arc<dyn SecretStore> {
        self.backend.secrets().unwrap_or_else(|| self.proxy.mem.clone() as Arc<dyn SecretStore>)
    }

    fn proxy_settings(&self) -> TorrentProxy {
        self.settings.config().network.proxy
    }

    /// Tests: loopback servers in place of NordVPN's.
    #[cfg(test)]
    pub(crate) fn set_proxy_preset_for_tests(&self, servers: Vec<String>, port: u16) {
        let countries = servers.iter().map(|_| String::new()).collect();
        let cities = servers.iter().map(|_| String::new()).collect();
        *lock(&self.proxy.preset) = Some(Preset { servers, countries, cities, port });
    }

    /// Tests: loopback servers with the countries they stand for.
    #[cfg(test)]
    pub(crate) fn set_proxy_preset_countries_for_tests(&self, servers: Vec<(String, String)>, port: u16) {
        let (servers, countries): (Vec<String>, Vec<String>) = servers.into_iter().unzip();
        let cities = servers.iter().map(|_| String::new()).collect();
        *lock(&self.proxy.preset) = Some(Preset { servers, countries, cities, port });
    }

    /// Whether a key is installed, read from the store the first time only (`Core::open` makes that first read, so a
    /// host's poll never waits for the Keychain).
    pub(crate) fn proxy_has_key(&self) -> bool {
        if let Some(k) = *lock(&self.proxy.has_key) {
            return k;
        }
        let k = read_key(&*self.proxy_store()).is_some();
        *lock(&self.proxy.has_key) = Some(k);
        k
    }

    /// Saves the settings and hands the engine its proxy.
    fn proxy_apply(&self, cfg: &TorrentProxy, key: Option<Option<Credentials>>) -> Result<(), CoreError> {
        self.settings.save_proxy(cfg).map_err(|_| CoreError::Failed { message: SETTINGS_NOT_SAVED.into() })?;
        self.backend.set_torrent_proxy(engine_proxy(cfg, key, &self.preset()));
        self.backend.settings_saved();
        Ok(())
    }

    /// Switching or removing the key while a key test or "Find a Faster Server" runs would undo itself (the test
    /// saves the proxy on when it ends) or wipe the progress the window shows: refused, in words.
    fn refuse_while_testing(&self) -> Result<(), CoreError> {
        if lock(&self.proxy.job).progress.is_some() {
            return Err(CoreError::Failed { message: WAIT_FOR_TEST.into() });
        }
        Ok(())
    }

    fn proxy_begin(&self, progress: &str) -> Result<(), CoreError> {
        let mut j = lock(&self.proxy.job);
        if j.progress.is_some() {
            return Err(CoreError::Failed { message: WAIT_FOR_TEST.into() });
        }
        *j = Job { progress: Some(progress.into()), result: None };
        Ok(())
    }

    fn proxy_end(&self, r: Result<String, String>) -> Result<String, CoreError> {
        let mut j = lock(&self.proxy.job);
        match r {
            Ok(s) => {
                *j = Job { progress: None, result: Some((false, s.clone())) };
                Ok(s)
            }
            Err(s) => {
                *j = Job { progress: None, result: Some((true, s.clone())) };
                Err(CoreError::Failed { message: s })
            }
        }
    }

    /// Saves NordVPN service credentials after testing them with NordVPN's proxy servers, and turns the proxy on
    /// (torrents only). Wrong credentials, or no server answering, keep nothing and say why. Blocks for the test (at
    /// most a few seconds): hosts call `proxy_set_nordvpn_key` (async). The sentence to show on success.
    pub fn proxy_set_nordvpn_key(&self, user: String, pass: String) -> Result<String, CoreError> {
        let creds = Credentials::new(&user, &pass).map_err(|m| CoreError::Failed { message: m.into() })?;
        if user.contains('\0') || pass.contains('\0') {
            return Err(CoreError::Failed { message: "The key holds a character it cannot hold.".into() });
        }
        drop((user, pass));
        self.proxy_begin("Testing the key with NordVPN\u{2019}s proxy servers\u{2026}")?;
        let preset = self.preset();
        let r = self.proxy_save_tested(&preset.servers, preset.port, Some(creds), StateKind::Nordvpn, String::new(), 0);
        self.proxy_end(r)
    }

    /// A SOCKS5 proxy of the user's own: tested, then saved and turned on. `user` and `pass` both or neither.
    pub fn proxy_set_custom(&self, host: String, port: u16, user: Option<String>, pass: Option<String>) -> Result<String, CoreError> {
        let host = host.trim().trim_matches(['[', ']']).to_owned();
        if host.is_empty() || host.contains(['/', ' ', '\0']) {
            return Err(CoreError::Failed { message: "Enter the proxy\u{2019}s server: a name or an address, without http:// or a path.".into() });
        }
        if port == 0 {
            return Err(CoreError::Failed { message: "Enter the proxy\u{2019}s port (1080 for most SOCKS5 proxies).".into() });
        }
        let user = user.filter(|u| !u.trim().is_empty());
        let pass = pass.filter(|p| !p.trim().is_empty());
        let creds = match (user, pass) {
            (None, None) => None,
            (Some(u), Some(p)) if !u.contains('\0') && !p.contains('\0') => Some(Credentials::new(&u, &p).map_err(|m| CoreError::Failed { message: m.into() })?),
            (Some(_), Some(_)) => return Err(CoreError::Failed { message: "The key holds a character it cannot hold.".into() }),
            _ => return Err(CoreError::Failed { message: "Enter both the username and the password, or neither.".into() }),
        };
        self.proxy_begin("Testing the proxy\u{2026}")?;
        let r = self.proxy_save_tested(std::slice::from_ref(&host), port, creds, StateKind::Custom, host.clone(), port);
        self.proxy_end(r)
    }

    fn proxy_save_tested(&self, servers: &[String], port: u16, creds: Option<Credentials>, kind: StateKind, host: String, cport: u16) -> Result<String, String> {
        let label = if kind == StateKind::Custom { seedwell_socks::Proxy { host: host.clone(), port: cport, auth: None }.label() } else { String::new() };
        let (server, took) = test_servers(servers, port, creds.as_ref()).map_err(|p| match kind {
            StateKind::Nordvpn => nordvpn_refusal(&p, creds.as_ref().map_or("", Credentials::user)),
            StateKind::Custom => custom_refusal(&p, &label),
        })?;
        let mut sealed = encode(creds.as_ref());
        let saved = self.proxy_store().put(SECRET_NAME, &sealed);
        sealed.fill(0);
        saved.map_err(|e| format!("The key could not be kept in the system\u{2019}s secure storage.{} Nothing changed.", storage_why(&e)))?;
        *lock(&self.proxy.has_key) = Some(true);
        // The country stays (it is the user's place, not the proxy's).
        let cfg = TorrentProxy { enabled: true, kind, host, port: cport, country: self.proxy_settings().country };
        self.proxy_apply(&cfg, Some(creds)).map_err(|_| SETTINGS_NOT_SAVED.to_string())?;
        let shown = shown_server(&format!("{server}:{port}"), kind);
        Ok(match kind {
            StateKind::Nordvpn => format!(
                "NordVPN is on. Torrents go through it now ({shown} answered fastest, in {}). Folder sync and chats still connect directly.",
                fmt_latency(u32::try_from(took.as_millis()).unwrap_or(u32::MAX))
            ),
            StateKind::Custom => format!("The proxy is on. Torrents go through {shown} now. Folder sync and chats still connect directly."),
        })
    }

    /// Tests every server of the proxy now and switches to the best (the country preference first, then speed), then
    /// says what came of it: "Now using … (Netherlands, 38 ms)." or "Already on the best server: …". Blocks for the
    /// test (a few seconds at most): hosts call `proxy_find_faster` (async); meanwhile `proxy_model` says "Testing
    /// servers…" and asks to be polled every 500 ms.
    pub fn proxy_find_faster(&self) -> Result<String, CoreError> {
        let cfg = self.proxy_settings();
        if !(cfg.enabled && self.proxy_has_key()) {
            return Err(CoreError::Failed { message: "Turn the proxy on first.".into() });
        }
        self.proxy_begin("Testing servers\u{2026}")?;
        let before = self.backend.torrent_proxy_status();
        self.backend.torrent_proxy_rank_now();
        // A ranking already running when asked tested the servers as they were before (right after the key was set):
        // the one asked for comes after it, so its answer is the one shown.
        let fresh = |s: &ProxyStatus| s.ranks > before.ranks_started.max(before.ranks);
        let running = before.ranks_started > before.ranks;
        let end = std::time::Instant::now() + seedwell_engine::proxy::PROBE_TIMEOUT * if running { 2 } else { 1 } + Duration::from_secs(4);
        let mut after = before.clone();
        while std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
            after = self.backend.torrent_proxy_status();
            if fresh(&after) {
                break;
            }
        }
        let preset = self.preset();
        let r = match (&after.server, fresh(&after)) {
            (_, false) => Err("The servers didn\u{2019}t all answer in time. The one in use stays.".to_string()),
            (None, true) => Err(after.problem.as_ref().map_or_else(|| "No server answered.".to_string(), |p| problem_text(p, cfg.kind))),
            (Some(s), true) => {
                let shown = server_with_place(s, &preset, cfg.kind, after.handshake_ms);
                let mut said = if before.server.as_deref() == Some(s.as_str()) {
                    format!("Already on the best server: {shown}.")
                } else {
                    format!("Now using {shown}.")
                };
                if let Some((host, ms)) = &after.passed_over {
                    said.push_str(&format!(" {} is nearer but answered slowly ({}).", server_with_place(host, &preset, cfg.kind, None), fmt_latency(*ms)));
                }
                Ok(said)
            }
        };
        self.proxy_end(r)
    }

    /// Where the user is, for the server choice (an ISO code from `countries`; "" for no preference). Chosen by hand
    /// only. Kept in `config.toml`; with the proxy on, every server is ranked again at once (torrents are not held).
    pub fn proxy_set_country(&self, code: String) -> Result<ProxyModel, CoreError> {
        let code = code.trim().to_ascii_uppercase();
        if !code.is_empty() && crate::proxy_countries::country(&code).is_none() {
            return Err(CoreError::Failed { message: format!("Seedwell doesn\u{2019}t know the country {code}.") });
        }
        let mut cfg = self.proxy_settings();
        cfg.country = code;
        let key = if cfg.enabled { read_key(&*self.proxy_store()) } else { None };
        self.proxy_apply(&cfg, key)?;
        Ok(self.proxy_model())
    }

    /// Forgets the key and turns the proxy off: torrents connect directly again.
    pub fn proxy_remove_key(&self) -> Result<(), CoreError> {
        self.refuse_while_testing()?;
        self.proxy_store()
            .delete(SECRET_NAME)
            .map_err(|e| CoreError::Failed { message: format!("The key could not be removed from the system\u{2019}s secure storage.{}", storage_why(&e)) })?;
        *lock(&self.proxy.has_key) = Some(false);
        let mut cfg = self.proxy_settings();
        cfg.enabled = false;
        self.proxy_apply(&cfg, None)?;
        *lock(&self.proxy.job) = Job { progress: None, result: Some((false, "The key was removed. Torrents connect directly.".into())) };
        Ok(())
    }

    /// Switches the proxy on or off (the icon's click with a key installed). Without a key it refuses: the host
    /// opens the window instead ([`ProxyModel::click_opens_window`]). The new model.
    pub fn proxy_toggle(&self) -> Result<ProxyModel, CoreError> {
        let key = read_key(&*self.proxy_store());
        *lock(&self.proxy.has_key) = Some(key.is_some());
        if key.is_none() {
            return Err(CoreError::Failed { message: "No proxy key is installed. Set one up in the SOCKS5 proxy window.".into() });
        }
        self.refuse_while_testing()?;
        let mut cfg = self.proxy_settings();
        cfg.enabled = !cfg.enabled;
        self.proxy_apply(&cfg, key)?;
        // The last result was about the proxy as it was; a test running now would have refused the switch above.
        let mut j = lock(&self.proxy.job);
        if j.progress.is_none() {
            *j = Job::default();
        }
        drop(j);
        Ok(self.proxy_model())
    }

    /// The engine's proxy at start, from the settings, the key in `store` and the servers of `preset`
    /// (`real::RealBackend::start`).
    pub(crate) fn proxy_at_start(cfg: &TorrentProxy, store: &dyn SecretStore, preset: &Preset) -> Option<ProxyConfig> {
        if !cfg.enabled {
            return None;
        }
        engine_proxy(cfg, read_key(store), preset)
    }
}

#[uniffi::export]
impl Core {
    /// The shield icon and the Network settings' proxy line. Cheap (no Keychain read after the first, no network):
    /// call it every [`ProxyModel::refresh_ms`].
    pub fn proxy_model(&self) -> ProxyModel {
        let has_key = self.proxy_has_key();
        model(&self.proxy_settings(), has_key, &self.backend.torrent_proxy_status(), &lock(&self.proxy.job))
    }

    /// The "SOCKS5 proxy" window's words. Cheap.
    pub fn proxy_open_modal_model(&self) -> ProxyWindowModel {
        let has_key = self.proxy_has_key();
        window(&self.proxy_settings(), has_key, &lock(&self.proxy.job))
    }
}

/// The control server's way to the proxy (`proxy.*`, `seedwell proxy`, SPEC §7H): the calls Settings' proxy box and
/// window make, so an agent's change ends where the user's would. Holds the core weakly (the core owns the server
/// that holds this).
pub(crate) struct ControlHook(pub std::sync::Weak<Core>);

fn words(e: CoreError) -> String {
    e.to_string()
}

impl ControlHook {
    fn core(&self) -> Result<Arc<Core>, String> {
        self.0.upgrade().ok_or_else(|| "Seedwell is quitting.".to_string())
    }
}

impl seedwell_control::ProxyControl for ControlHook {
    fn status(&self) -> seedwell_control::types::ProxyDto {
        let Some(core) = self.0.upgrade() else { return Default::default() };
        let cfg = core.proxy_settings();
        let m = core.proxy_model();
        seedwell_control::types::ProxyDto {
            enabled: cfg.enabled,
            kind: match cfg.kind {
                StateKind::Nordvpn => "nordvpn".into(),
                StateKind::Custom => "custom".into(),
            },
            host: cfg.host,
            port: cfg.port,
            country: cfg.country,
            has_key: m.has_key,
            server: m.server,
            status: m.status_line,
            problem: m.problem,
            note: String::new(),
        }
    }

    fn switch(&self, on: bool) -> Result<String, String> {
        let core = self.core()?;
        if core.proxy_settings().enabled != on {
            core.proxy_toggle().map_err(words)?;
        }
        Ok(if on { "The proxy is on: torrents go through it." } else { "The proxy is off: torrents connect directly." }.into())
    }

    fn country(&self, code: &str) -> Result<String, String> {
        let m = self.core()?.proxy_set_country(code.to_owned()).map_err(words)?;
        Ok(format!("Servers are chosen for: {}.", country_name(&m.country)))
    }

    fn nordvpn(&self, user: String, pass: String) -> Result<String, String> {
        self.core()?.proxy_set_nordvpn_key(user, pass).map_err(words)
    }

    fn custom(&self, host: String, port: u16, key: Option<(String, String)>) -> Result<String, String> {
        let (user, pass) = key.unzip();
        self.core()?.proxy_set_custom(host, port, user, pass).map_err(words)
    }

    fn remove_key(&self) -> Result<String, String> {
        self.core()?.proxy_remove_key().map_err(words)?;
        Ok("The key was removed. Torrents connect directly.".into())
    }

    fn faster(&self) -> Result<String, String> {
        self.core()?.proxy_find_faster().map_err(words)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(health: ProxyHealth, server: Option<&str>, ms: Option<u32>, problem: Option<ProxyProblem>) -> ProxyStatus {
        ProxyStatus { health, server: server.map(Into::into), handshake_ms: ms, problem, ..ProxyStatus::default() }
    }

    /// Failure: the icon shows a state the proxy is not in (green while torrents are held, grey with a key), or a
    /// click toggles where it should open the window.
    #[test]
    fn the_icon_follows_the_key_the_switch_and_the_health() {
        let mut cfg = TorrentProxy::default();
        let none = model(&cfg, false, &ProxyStatus::default(), &Job::default());
        assert_eq!((none.icon, none.click_opens_window, none.menu_toggle_label.as_str()), (ProxyIcon::NoKey, true, ""));
        let off = model(&cfg, true, &ProxyStatus::default(), &Job::default());
        assert_eq!((off.icon, off.click_opens_window), (ProxyIcon::Off, false));
        assert_eq!(off.menu_toggle_label, "Turn On NordVPN");
        cfg.enabled = true;
        let on = model(&cfg, true, &st(ProxyHealth::Up, Some("nl.socks.nordhold.net:1080"), Some(84), None), &Job::default());
        assert_eq!((on.icon, on.server.as_str(), on.latency_ms), (ProxyIcon::On, "nl.socks.nordhold.net", Some(84)));
        assert!(on.status_line.contains("84 ms") && on.tooltip.contains("Click to turn it off"), "{on:?}");
        let slow = model(&cfg, true, &st(ProxyHealth::Slow, Some("us.socks.nordhold.net:1080"), Some(2100), None), &Job::default());
        assert_eq!(slow.icon, ProxyIcon::Slow);
        assert!(slow.status_line.contains("2.1 s"), "{}", slow.status_line);
        let failed = model(&cfg, true, &st(ProxyHealth::Down, None, None, Some(ProxyProblem::AuthRejected)), &Job::default());
        assert_eq!(failed.icon, ProxyIcon::Failed);
        assert!(failed.problem.as_deref().unwrap().contains("not your Nord Account login"), "{failed:?}");
        assert!(failed.tooltip.contains(failed.problem.as_deref().unwrap()), "the tooltip says why: {}", failed.tooltip);
        // The hosts show the problem under the status line: the line does not say it again.
        assert!(!failed.status_line.contains(failed.problem.as_deref().unwrap()), "{}", failed.status_line);
        // A key and the switch on, but the key was removed meanwhile: grey, not green.
        assert_eq!(model(&cfg, false, &ProxyStatus::default(), &Job::default()).icon, ProxyIcon::NoKey);
    }

    /// Failure: a stored key does not read back (the proxy would hold every torrent after a restart), or a custom
    /// proxy without login reads as no key.
    #[test]
    fn the_key_round_trips_and_a_proxy_without_login_is_a_key() {
        let c = Credentials::new("svcuser", "svcpass").unwrap();
        assert_eq!(decode(encode(Some(&c))), Some(Some(c)));
        assert_eq!(decode(encode(None)), Some(None));
        assert_eq!(decode(b"garbage".to_vec()), None);
    }

    /// Failure: the proxy on without a key connects directly (the kill switch must hold torrents instead), or the
    /// NordVPN preset uses anything but the documented servers on 1080.
    #[test]
    fn on_without_a_key_holds_and_the_preset_uses_the_documented_servers() {
        let cfg = TorrentProxy { enabled: true, ..TorrentProxy::default() };
        let held = engine_proxy(&cfg, None, &nordvpn_preset()).unwrap();
        assert!(held.servers.is_empty(), "no server: every torrent is held");
        let nord = engine_proxy(&cfg, Some(Some(Credentials::new("u", "p").unwrap())), &nordvpn_preset()).unwrap();
        assert_eq!(nord.port, 1080);
        assert_eq!(nord.servers.len(), NORDVPN_SOCKS5_SERVERS.len());
        assert!(nord.servers.iter().all(|s| s.ends_with(".socks.nordhold.net")));
        assert!(engine_proxy(&TorrentProxy::default(), None, &nordvpn_preset()).is_none(), "off");
    }

    /// Failure: the window lacks the guide, or its words name the key.
    #[test]
    fn the_window_has_the_guide_and_never_the_key() {
        let cfg = TorrentProxy { enabled: true, ..TorrentProxy::default() };
        let w = window(&cfg, true, &Job::default());
        assert_eq!(w.title, "SOCKS5 proxy");
        assert_eq!(w.enable_nordvpn_label, "Enable NordVPN");
        assert!(w.guide_steps.iter().any(|s| s.contains("Service credentials")), "{:?}", w.guide_steps);
        assert!(w.explanation.contains("Folder sync, chats and your devices"));
    }

    /// Failure: with a key installed the window shows the empty first-time form (it reads as if setup were unfinished),
    /// or the "installed" line shows in the pane of the kind that is not installed, or without a key.
    #[test]
    fn an_installed_key_replaces_the_setup_form_in_its_own_pane() {
        let mut cfg = TorrentProxy { enabled: true, ..TorrentProxy::default() };
        let nord = window(&cfg, true, &Job::default());
        assert!(!nord.nordvpn_installed.is_empty() && nord.custom_installed.is_empty(), "{nord:?}");
        cfg.kind = StateKind::Custom;
        cfg.host = "proxy.example".into();
        cfg.port = 1081;
        let custom = window(&cfg, true, &Job::default());
        assert!(custom.nordvpn_installed.is_empty() && custom.custom_installed.contains("proxy.example"), "{custom:?}");
        let none = window(&cfg, false, &Job::default());
        assert!(none.nordvpn_installed.is_empty() && none.custom_installed.is_empty(), "no key: the form");
    }

    /// Failure: the window's paste-friendly fields overwrite what the user typed, fill the password before the
    /// username, take the username copied twice as the password, or fill in a sentence, a link or an email address
    /// copied while the window was open.
    #[test]
    fn a_copied_credential_fills_the_first_empty_field_only() {
        // The username came from a copy a moment ago.
        let fill = |u: &str, p: &str, c: &str| proxy_paste_fill(u.into(), p.into(), c.into(), Some(1_000));
        let user = |t: &str| Some(ProxyPaste::Username { text: t.into() });
        let pass = |t: &str| Some(ProxyPaste::Password { text: t.into() });
        assert_eq!(fill("", "", " qJ8sVsvc2Lw9 \n"), user("qJ8sVsvc2Lw9"), "trimmed, into the username first");
        assert_eq!(fill("qJ8sVsvc2Lw9", "", "Zk3pW7mN"), pass("Zk3pW7mN"), "then the password");
        assert_eq!(fill("qJ8sVsvc2Lw9", "Zk3pW7mN", "other"), None, "both filled: nothing changes");
        assert_eq!(fill("typed", "", "typed"), None, "the username copied again is not the password");
        assert_eq!(fill("", "typed-first", "abc"), user("abc"), "a typed password stays; the username is still empty");
        for junk in ["", "   ", "two words", "line\nbreak", "https://my.nordaccount.com/", &"x".repeat(256)] {
            assert_eq!(fill("", "", junk), None, "{junk:?}");
        }
        assert_eq!(fill("", "", "me@example.com"), None, "an email address is never the service username");
    }

    /// Failure: a secret copied long after the username, or after a typed one, fills the hidden-from-mind password
    /// field and is sent to NordVPN's servers.
    #[test]
    fn the_password_comes_only_from_a_copy_soon_after_the_copied_username() {
        let pass = |ago: Option<u64>| proxy_paste_fill("svc-user".into(), String::new(), "s3cret".into(), ago);
        assert!(pass(Some(5_000)).is_some(), "the second copy in Nord Account");
        assert!(pass(Some(PASTE_PAIR_MS + 1)).is_none(), "too long after the username");
        assert!(pass(None).is_none(), "a typed username: no copy fills the password");
    }

    /// Failure: an isolated (QA or test) run reaches NordVPN's real servers, or its stand-ins are read wrong (the
    /// wrong port, a country lost).
    #[test]
    fn an_isolated_run_uses_only_the_servers_its_state_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(preset_for(dir.path(), false), nordvpn_preset());
        let none = preset_for(dir.path(), true);
        assert!(none.servers.is_empty(), "no file: no server at all, never NordVPN's: {none:?}");
        std::fs::create_dir_all(dir.path().join("qa")).unwrap();
        std::fs::write(dir.path().join(QA_SERVERS_FILE), "# QA\n127.0.0.1:41080 nl Amsterdam\n[::1]:41080 SE\nbad line\n").unwrap();
        let p = preset_for(dir.path(), true);
        assert_eq!((p.servers.as_slice(), p.port), (["127.0.0.1".to_string(), "::1".to_string()].as_slice(), 41080));
        assert_eq!(p.countries, ["NL", "SE"]);
        assert_eq!(p.cities, ["Amsterdam", ""]);
    }

    // ---------------------------------------------------------------------- a real core, as the host calls it

    /// A real core in temporary folders, its NordVPN preset a loopback fake proxy.
    fn real_core(fake: &seedwell_socks::testkit::FakeProxy) -> (Arc<Core>, std::path::PathBuf, [tempfile::TempDir; 2]) {
        real_core_for(fake, crate::settings::AgentPermission::Off)
    }

    /// The same, with agents allowed at `agents` (its control socket is `<state>/control.sock`).
    fn real_core_for(fake: &seedwell_socks::testkit::FakeProxy, agents: crate::settings::AgentPermission) -> (Arc<Core>, std::path::PathBuf, [tempfile::TempDir; 2]) {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = crate::SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.network.port_mapping = false;
        st.agents.permission = agents;
        store.save_settings(&st).unwrap();
        let options = crate::CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
        let core = Core::new(options).unwrap();
        core.set_proxy_preset_for_tests(vec![fake.addr().ip().to_string()], fake.addr().port());
        (core, dir, [home, work])
    }

    /// Failure: an agent's `seedwell proxy` (the control socket's `proxy.*`) does not end where the user's click and
    /// window do (the key kept and tested, the proxy on in `config.toml` and in the engine; off again with `proxy
    /// off`), or the key comes back in an answer, `settings get` or the activity log the user reads in Settings ›
    /// Agents. The calls come as the CLI makes them: over the socket, from another process's runtime.
    #[test]
    fn an_agent_sets_the_key_and_switches_the_proxy_through_the_control_socket() {
        use seedwell_control::proto::{Empty, ProxyKeyParams, ProxySetParams, Request, SettingsGetParams};
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (user, pass) = ("svc-user-Q8R1", "svc-pass-M4N7P");
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start(user, pass));
        let (core, dir, _t) = real_core_for(&fake, crate::settings::AgentPermission::Full);
        let sock = dir.join("control.sock");
        let call = |req: Request| {
            rt.block_on(async {
                let mut c = seedwell_control::Client::connect(&sock, "agent-test").await.unwrap();
                c.call(&req).await
            })
        };
        let mut answers = Vec::new();
        let key = |p: &str| Request::ProxyKey(ProxyKeyParams { kind: "nordvpn".into(), user: Some(user.into()), pass: Some(p.into()), host: None, port: None });
        answers.push(format!("{:?}", call(key("wrong-pass")).unwrap_err()));
        assert!(!core.proxy_model().has_key, "a key that fails its test is not kept");
        let on = call(key(pass)).unwrap();
        assert!(on["enabled"] == true && on["has_key"] == true && on["note"].as_str().is_some_and(|n| n.starts_with("NordVPN is on")), "{on}");
        answers.push(on.to_string());
        wait_icon(&core, ProxyIcon::On);
        assert!(crate::SettingsStore::new(&dir).config().network.proxy.enabled);
        let got = call(Request::SettingsGet(SettingsGetParams::default())).unwrap();
        assert_eq!(got["settings"]["network.proxy.key"], "installed");
        answers.push(got.to_string());
        let off = call(Request::ProxySet(ProxySetParams { on: Some(false), country: None })).unwrap();
        assert_eq!(off["enabled"], false, "{off}");
        wait_icon(&core, ProxyIcon::Off);
        assert!(!crate::SettingsStore::new(&dir).config().network.proxy.enabled);
        answers.push(call(Request::ProxyStatus(Empty {})).unwrap().to_string());
        // `settings set` does not go around it.
        let e = call(Request::SettingsSet(seedwell_control::proto::SettingsSetParams {
            changes: serde_json::json!({"network.proxy.enabled": true}).as_object().unwrap().clone(),
            ..Default::default()
        }))
        .unwrap_err();
        assert!(format!("{e:?}").contains("seedwell proxy"), "{e:?}");
        let log = core.agent_activity(50);
        assert!(log.iter().any(|a| a.method == "proxy.key" && a.summary.contains("key not logged") && a.outcome == "ok"), "{log:?}");
        answers.extend(log.iter().map(|a| format!("{a:?}")));
        for a in &answers {
            assert!(!a.contains(user) && !a.contains(pass) && !a.contains("wrong-pass"), "the key leaked: {a}");
        }
        core.shutdown(5);
    }

    /// The host's way: the call on a thread of its own (the async FFI call runs it apart).
    fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().unwrap()
    }

    fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { files_under(&p, out) } else { out.push(p) }
        }
    }

    fn wait_icon(core: &Core, want: ProxyIcon) -> ProxyModel {
        let end = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let m = core.proxy_model();
            if m.icon == want {
                return m;
            }
            assert!(std::time::Instant::now() < end, "waiting for {want:?}: {m:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Failure: wrong credentials are kept anyway (the proxy would then hold every torrent), or the refusal does not
    /// say that the Nord Account login is not the key.
    #[test]
    fn wrong_credentials_fail_with_the_login_hint_and_nothing_is_kept() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("svc-user", "svc-pass"));
        let (core, dir, _t) = real_core(&fake);
        let c = core.clone();
        let e = on_a_thread(move || c.proxy_set_nordvpn_key("me@example.com".into(), "my-login-password".into())).unwrap_err();
        let CoreError::Failed { message } = e else { panic!("{e:?}") };
        assert!(message.contains("these look like your Nord Account login, not the service credentials"), "{message}");
        assert!(message.contains("not saved"), "{message}");
        let m = core.proxy_model();
        assert_eq!((m.icon, m.has_key, m.enabled), (ProxyIcon::NoKey, false, false));
        assert!(m.problem.is_some(), "the failure is shown");
        assert!(read_key(&*core.proxy_store()).is_none(), "no key kept");
        assert!(!crate::SettingsStore::new(&dir).config().network.proxy.enabled);
        assert_eq!(fake.auth_failures(), 1);
        core.shutdown(5);
    }

    /// Failure: the key leaks: into `config.toml`, a log or any other file of the state folder but the secret store,
    /// or into a model the host gets. And the host's calls (save, toggle) do not reach the engine.
    #[test]
    fn the_key_is_kept_in_the_secret_store_only_and_the_calls_reach_the_engine() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (user, pass) = ("svc-user-K7Q2", "svc-pass-Z9X4W");
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start(user, pass));
        let (core, dir, _t) = real_core(&fake);
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_set_nordvpn_key(format!(" {user} "), pass.into())).unwrap();
        assert!(said.starts_with("NordVPN is on"), "{said}");
        let on = wait_icon(&core, ProxyIcon::On);
        assert!(on.has_key && on.enabled && on.latency_ms.is_some(), "{on:?}");
        let text = format!("{on:?} {:?} {said}", core.proxy_open_modal_model());
        assert!(!text.contains(user) && !text.contains(pass), "{text}");
        assert!(crate::SettingsStore::new(&dir).config().network.proxy.enabled);
        // Off and on again with the icon's click; the key stays.
        let c = core.clone();
        assert_eq!(on_a_thread(move || c.proxy_toggle()).unwrap().icon, ProxyIcon::Off);
        let c = core.clone();
        on_a_thread(move || c.proxy_toggle()).unwrap();
        wait_icon(&core, ProxyIcon::On);
        core.shutdown(5);
        let mut files = Vec::new();
        files_under(&dir, &mut files);
        let store = [dir.join("sync").join("secrets"), dir.join("sync").join("vault")];
        let mut looked = 0;
        for f in files.iter().filter(|f| !store.iter().any(|s| f.starts_with(s))) {
            let Ok(b) = std::fs::read(f) else { continue };
            looked += 1;
            for secret in [user, pass] {
                assert!(!b.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} in {}", f.display());
            }
        }
        assert!(looked > 0 && files.iter().any(|f| f.ends_with("config.toml")), "{files:?}");
        // The key is there for the next start.
        let (secrets, _) = crate::sync::secret_store_checked(None, &dir);
        let k = read_key(&*secrets).unwrap().unwrap();
        assert_eq!((k.user(), k.pass()), (user, pass));
    }

    /// Failure: the QA app (an isolated run) tests a key against NordVPN instead of the loopback fake its state
    /// folder names, or the engine it starts uses NordVPN's servers.
    #[test]
    fn an_isolated_core_tests_keys_and_connects_only_through_its_own_stand_ins() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("svc-user", "svc-pass"));
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = crate::SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.network.port_mapping = false;
        store.save_settings(&st).unwrap();
        let options = crate::CoreOptions {
            config_dir: dir.display().to_string(),
            mock: false,
            allow_loopback_trackers: false,
            forbidden_root: Some(home.path().display().to_string()),
        };
        let core = Core::new(options).unwrap();
        // No stand-ins named: the test finds no server and keeps nothing (it never falls back to NordVPN).
        let c = core.clone();
        let e = on_a_thread(move || c.proxy_set_nordvpn_key("svc-user".into(), "svc-pass".into())).unwrap_err();
        assert!(e.to_string().contains("not saved"), "{e}");
        assert_eq!(core.proxy_model().icon, ProxyIcon::NoKey);
        std::fs::create_dir_all(dir.join("qa")).unwrap();
        std::fs::write(dir.join(QA_SERVERS_FILE), format!("127.0.0.1:{} NL\n", fake.addr().port())).unwrap();
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_set_nordvpn_key("svc-user".into(), "svc-pass".into())).unwrap();
        assert!(said.starts_with("NordVPN is on"), "{said}");
        let on = wait_icon(&core, ProxyIcon::On);
        assert_eq!(on.server, "127.0.0.1", "{on:?}");
        core.shutdown(5);
    }

    /// Failure: "Find a Faster Server" pressed right after the key was set answers with the ranking that was already
    /// running (the servers as they were before the press: here a slow answer that has since gone) instead of one made
    /// after it was asked (found by the app's QA, `capture_proxy.sh`).
    #[test]
    fn find_faster_answers_with_a_ranking_made_after_it_was_asked() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("u", "p"));
        let (core, _dir, _t) = real_core(&fake);
        fake.set_delay(Duration::from_millis(1200));
        let c = core.clone();
        on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap();
        // The engine's first ranking is under way, its handshake waiting for the slow answer.
        let end = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let s = core.backend.torrent_proxy_status();
            if s.ranks_started > s.ranks {
                break;
            }
            assert!(std::time::Instant::now() < end, "no ranking started: {s:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        // Faster now, though not at once: the press's own ranking takes a moment of its own.
        fake.set_delay(Duration::from_millis(300));
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_find_faster()).unwrap();
        assert!(said.ends_with(" ms)."), "the answer is the ranking from before the press (over a second): {said}");
        core.shutdown(5);
    }

    /// Failure: what the user reads when the proxy refuses the connection holds the system's "os error 61" and
    /// parentheses inside parentheses ("No server answered (the proxy can't be reached (Connection refused (os error
    /// 61))).") instead of plain sentences: the shield's problem, and the refusal of a key tested against it.
    #[test]
    fn a_refused_connection_reads_as_plain_sentences() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("u", "p"));
        let (core, _dir, _t) = real_core(&fake);
        let c = core.clone();
        on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap();
        wait_icon(&core, ProxyIcon::On);
        rt.block_on(fake.stop());
        // Off and on again: the servers are tested at once, and refuse.
        for _ in 0..2 {
            let c = core.clone();
            on_a_thread(move || c.proxy_toggle()).unwrap();
        }
        let failed = wait_icon(&core, ProxyIcon::Failed);
        let problem = failed.problem.clone().unwrap();
        let c = core.clone();
        let CoreError::Failed { message: refusal } = on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap_err() else { panic!() };
        for text in [&problem, &failed.tooltip, &failed.status_line, &refusal] {
            assert!(!text.contains("os error") && !text.contains('('), "{text}");
        }
        core.shutdown(5);
    }

    /// Failure: the switch flipped while a key test runs wipes its progress (a second test can start beside it) and is
    /// undone when the test saves the proxy on; Remove Key then is undone the same way.
    #[test]
    fn switching_or_removing_waits_for_a_running_test() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("u", "p"));
        let (core, _dir, _t) = real_core(&fake);
        let c = core.clone();
        on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap();
        fake.set_delay(Duration::from_millis(800));
        let c = core.clone();
        let test = std::thread::spawn(move || c.proxy_set_nordvpn_key("u".into(), "p".into()));
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while core.proxy_open_modal_model().progress.is_empty() {
            assert!(std::time::Instant::now() < end, "the test never showed its progress");
            std::thread::sleep(Duration::from_millis(10));
        }
        let c = core.clone();
        assert!(on_a_thread(move || c.proxy_toggle()).is_err(), "the switch waits");
        let c = core.clone();
        assert!(on_a_thread(move || c.proxy_remove_key()).is_err(), "Remove Key waits");
        assert!(!core.proxy_open_modal_model().progress.is_empty(), "the progress stays");
        test.join().unwrap().unwrap();
        let m = core.proxy_model();
        assert!(m.has_key && m.enabled, "{m:?}");
        let c = core.clone();
        assert!(on_a_thread(move || c.proxy_toggle()).is_ok(), "after the test the switch works");
        core.shutdown(5);
    }

    /// Failure: no server at all to test reads as a server that did not answer (the user would check the network).
    #[test]
    fn no_server_to_test_is_said_as_such() {
        assert_eq!(test_servers(&[], 1080, None), Err(ProxyProblem::NoServer));
    }

    /// Failure: Remove Key leaves the key in the store, or leaves torrents held by a proxy with no key.
    #[test]
    fn remove_key_forgets_it_and_torrents_connect_directly() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let fake = rt.block_on(seedwell_socks::testkit::FakeProxy::start("u", "p"));
        let (core, dir, _t) = real_core(&fake);
        let c = core.clone();
        on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap();
        let c = core.clone();
        on_a_thread(move || c.proxy_remove_key()).unwrap();
        let m = core.proxy_model();
        assert_eq!((m.icon, m.has_key, m.enabled), (ProxyIcon::NoKey, false, false));
        assert!(read_key(&*core.proxy_store()).is_none());
        assert!(!crate::SettingsStore::new(&dir).config().network.proxy.enabled);
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while core.backend.torrent_proxy_status().health != ProxyHealth::Off {
            assert!(std::time::Instant::now() < end, "the engine still uses the proxy");
            std::thread::sleep(Duration::from_millis(50));
        }
        let c = core.clone();
        assert!(on_a_thread(move || c.proxy_toggle()).is_err(), "no key: the host opens the window instead");
        core.shutdown(5);
    }

    /// Failure: the menu and the window list the countries out of order (the table's late additions, Egypt and Kenya,
    /// came last, after Vietnam), so one is not where the user looks for it.
    #[test]
    fn the_countries_are_listed_by_name_after_no_preference() {
        let c = country_choices();
        assert_eq!((c[0].code.as_str(), c[0].name.as_str()), ("", "No preference"));
        let names: Vec<&str> = c[1..].iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(c.len(), crate::proxy_countries::COUNTRIES.len() + 1, "every country once");
    }

    /// Failure: the country list shows all its rows again (about 60, taller than the screen's menu space): the
    /// menu's and the window's models do not carry the visible row count, or carry different ones, so a host draws
    /// the whole list or the two lists differ; or the entry and the button do not name the chosen country.
    #[test]
    fn both_country_lists_show_a_few_rows_and_name_the_chosen_one() {
        let mut cfg = TorrentProxy { country: "SE".into(), ..TorrentProxy::default() };
        let m = model(&cfg, true, &ProxyStatus::default(), &Job::default());
        let w = window(&cfg, true, &Job::default());
        assert_eq!(m.country_visible_rows, w.country_visible_rows, "the menu's list and the window's are as tall");
        assert!(m.country_visible_rows >= 3, "a few rows show");
        assert!((m.country_visible_rows as usize) < m.countries.len() / 4, "only a few of {} rows show", m.countries.len());
        assert!(m.country_menu_label.contains("Sweden"), "{}", m.country_menu_label);
        assert_eq!(w.country_name, "Sweden");
        cfg.country = String::new();
        let m = model(&cfg, true, &ProxyStatus::default(), &Job::default());
        assert!(m.country_menu_label.contains(&m.countries[0].name), "none chosen: {}", m.country_menu_label);
        assert_eq!(window(&cfg, false, &Job::default()).country_name, m.countries[0].name);
    }

    /// Failure: a server's country is read wrong from its NordVPN name (the preference would steer to the wrong
    /// continent).
    #[test]
    fn every_nordvpn_server_has_the_country_its_name_gives() {
        let p = nordvpn_preset();
        assert_eq!(p.countries.len(), p.servers.len());
        assert!(p.countries.iter().all(|c| ["NL", "SE", "US"].contains(&c.as_str())), "{:?}", p.countries);
        assert_eq!(NORDVPN_SOCKS5_SERVERS[3].country(), "NL", "amsterdam.nl");
        assert_eq!(NORDVPN_SOCKS5_SERVERS[11].country(), "SE", "stockholm.se");
        assert!(p.countries.iter().all(|c| crate::proxy_countries::country(c).is_some()), "every server country can be chosen");
    }

    /// Two loopback proxies on one port (127.0.0.1 and ::1) standing for servers in two countries, or `None` without
    /// IPv6 loopback.
    async fn two_servers() -> Option<(seedwell_socks::testkit::FakeProxy, seedwell_socks::testkit::FakeProxy)> {
        for _ in 0..20 {
            let a = seedwell_socks::testkit::FakeProxy::start("u", "p").await;
            let at = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), a.addr().port());
            if let Some(b) = seedwell_socks::testkit::FakeProxy::start_on(at, "u", "p").await {
                return Some((a, b));
            }
        }
        eprintln!("no IPv6 loopback: skipping");
        None
    }

    fn wait_server(core: &Core, want: &str) -> ProxyModel {
        let end = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let m = core.proxy_model();
            if m.server == want && m.icon != ProxyIcon::Failed {
                return m;
            }
            assert!(std::time::Instant::now() < end, "waiting for {want}: {m:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Failure: "Find a Faster Server" does not rank again (stays on a server that became slow) or does not say the
    /// outcome; a chosen country's working server loses to a faster foreign one within the healthy range; a Slow
    /// one in the chosen country wins over a healthy one, or that is not said; with no country the fastest does not
    /// win.
    #[test]
    fn find_faster_and_the_chosen_country_pick_the_server_by_the_rule() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let Some((us, no)) = rt.block_on(two_servers()) else { return };
        let (core, _dir, _t) = real_core(&us);
        core.set_proxy_preset_countries_for_tests(vec![("127.0.0.1".into(), "US".into()), ("::1".into(), "NO".into())], us.addr().port());
        no.set_delay(Duration::from_millis(150));
        // No country: the fastest.
        let c = core.clone();
        on_a_thread(move || c.proxy_set_nordvpn_key("u".into(), "p".into())).unwrap();
        let m = wait_server(&core, "127.0.0.1");
        assert!(m.countries.len() > 50 && m.countries[0].code.is_empty() && m.country.is_empty(), "{:?}", m.countries.first());
        assert_eq!(m.find_faster_label, "Find a Faster Server");
        // The US server slows down (still healthy): Find a Faster Server moves to Norway's and says so.
        us.set_delay(Duration::from_millis(350));
        no.set_delay(Duration::ZERO);
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_find_faster()).unwrap();
        assert!(said.starts_with("Now using [::1] (Norway"), "{said}");
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_find_faster()).unwrap();
        assert!(said.starts_with("Already on the best server: [::1]"), "{said}");
        // The user is in the US: its server wins though slower, while it is healthy.
        let c = core.clone();
        let m = on_a_thread(move || c.proxy_set_country("us".into())).unwrap();
        assert_eq!(m.country, "US");
        wait_server(&core, "127.0.0.1");
        // Too slow at home (over 1.5 s): the healthy one abroad, and why.
        us.set_delay(Duration::from_millis(1000));
        let c = core.clone();
        let said = on_a_thread(move || c.proxy_find_faster()).unwrap();
        assert!(said.starts_with("Now using [::1] (Norway") && said.contains("127.0.0.1 (United States) is nearer but answered slowly"), "{said}");
        assert!(core.proxy_model().tooltip.contains("nearer but answered slowly"));
        us.set_delay(Duration::ZERO);
        core.shutdown(5);
        drop(rt);
    }
}
