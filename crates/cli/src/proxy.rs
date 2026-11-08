//! `seedwell proxy`: the torrent proxy (SPEC §7H), as Settings › Network's proxy box and its window set it: on or off,
//! the country its servers are chosen by, the key (NordVPN's service credentials, or a SOCKS5 proxy of the user's own),
//! "Find a Faster Server" and removing the key. Permission full; every change is in the activity log, the key never.
//!
//! **The key is read from standard input only**, two lines: the username, then the password. It is never an argument
//! (it would stay in the shell's history and in the process list), never printed and never logged.
#![allow(missing_docs)] // variant fields are named after the options they hold

use std::io::{IsTerminal, Read};

use seedwell_control::proto::{Empty, ProxyKeyParams, ProxySetParams, Request};
use seedwell_control::types::ProxyDto;

use crate::args::{Opts, Usage};
use crate::commands::{CliError, Global, R, ask, connect, show};

/// A `proxy` command.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// How the proxy is.
    Status,
    /// On or off.
    Switch(bool),
    /// The country its servers are chosen by ("" none).
    Country(String),
    /// NordVPN's key, from standard input.
    NordVpn,
    /// A proxy of the user's own; `key`: its username and password come from standard input.
    Custom { host: String, port: u16, key: bool },
    /// Test the servers now and use the best.
    Faster,
    /// Forget the key (the proxy goes off).
    RemoveKey,
}

const USAGE: &str = "usage: seedwell proxy [status] | on | off | country <code>|none | nordvpn | custom <host> <port> [--key] | faster | remove-key";

/// Words after `proxy`.
pub fn parse(args: Vec<String>) -> Result<Cmd, Usage> {
    let mut o = Opts::new(args);
    let key = o.flag(&["--key"]);
    let pos = o.finish()?;
    let words: Vec<&str> = pos.iter().map(String::as_str).collect();
    Ok(match (words.as_slice(), key) {
        ([] | ["status"], false) => Cmd::Status,
        (["on"], false) => Cmd::Switch(true),
        (["off"], false) => Cmd::Switch(false),
        (["country", "none" | "-"], false) => Cmd::Country(String::new()),
        (["country", code], false) => Cmd::Country((*code).to_owned()),
        (["nordvpn"], false) => Cmd::NordVpn,
        (["custom", host, port], key) => {
            let port = port.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| Usage(format!("`{port}` is no port (1-65535)")))?;
            Cmd::Custom { host: (*host).to_owned(), port, key }
        }
        (["faster"], false) => Cmd::Faster,
        (["remove-key"], false) => Cmd::RemoveKey,
        _ => return Err(Usage(USAGE.into())),
    })
}

/// The username and password from standard input: two lines. Refused on a terminal, where they would show as typed.
fn key_from_stdin() -> R<(String, String)> {
    if std::io::stdin().is_terminal() {
        return Err(CliError::plain(2, "the key comes on standard input, two lines (the username, then the password): printf '%s\\n%s\\n' \"$USER_NAME\" \"$PASSWORD\" | seedwell proxy nordvpn"));
    }
    let mut bytes = Vec::new();
    std::io::stdin().take(4096).read_to_end(&mut bytes).map_err(|e| CliError::plain(1, format!("cannot read standard input: {e}")))?;
    let text = String::from_utf8(bytes).map_err(|_| CliError::plain(2, "the key on standard input is not text"))?;
    let mut lines = crate::chat::without_bom(&text).lines().map(|l| l.trim_end_matches('\r'));
    match (lines.next(), lines.next()) {
        (Some(u), Some(p)) if !u.trim().is_empty() && !p.is_empty() => Ok((u.trim().to_owned(), p.to_owned())),
        _ => Err(CliError::plain(2, "give the username and the password on standard input, one per line")),
    }
}

/// Run a `proxy` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    // The key is read before connecting: a refusal comes before anything is asked of the app.
    let req = match cmd {
        Cmd::Status => Request::ProxyStatus(Empty {}),
        Cmd::Switch(on) => Request::ProxySet(ProxySetParams { on: Some(on), country: None }),
        Cmd::Country(code) => Request::ProxySet(ProxySetParams { on: None, country: Some(code) }),
        Cmd::NordVpn => {
            let (user, pass) = key_from_stdin()?;
            Request::ProxyKey(ProxyKeyParams { kind: "nordvpn".into(), user: Some(user), pass: Some(pass), host: None, port: None })
        }
        Cmd::Custom { host, port, key } => {
            let (user, pass) = if key { key_from_stdin().map(|(u, p)| (Some(u), Some(p)))? } else { (None, None) };
            Request::ProxyKey(ProxyKeyParams { kind: "custom".into(), user, pass, host: Some(host), port: Some(port) })
        }
        Cmd::Faster => Request::ProxyFaster(Empty {}),
        Cmd::RemoveKey => Request::ProxyRemoveKey(Empty {}),
    };
    let mut c = connect(g).await?;
    let (raw, d): (_, ProxyDto) = ask(&mut c, req).await?;
    show(g, &raw, || words(&d));
    Ok(())
}

/// What `proxy` says.
pub fn words(d: &ProxyDto) -> String {
    let mut lines = Vec::new();
    if !d.note.is_empty() {
        lines.push(d.note.clone());
    }
    lines.push(format!("Torrent proxy: {}", d.status));
    if let Some(p) = &d.problem {
        lines.push(p.clone());
    }
    let which = match d.kind.as_str() {
        "custom" => format!("a proxy of your own ({}:{})", d.host, if d.port == 0 { 1080 } else { d.port }),
        _ => "NordVPN".into(),
    };
    lines.push(format!("Key: {}. Kind: {which}. Country: {}.", if d.has_key { "installed" } else { "none" }, if d.country.is_empty() { "no preference" } else { &d.country }));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, Usage> {
        parse(crate::cmd::shell_words(s))
    }

    #[test]
    fn the_words_parse() {
        assert_eq!(p("").unwrap(), Cmd::Status);
        assert_eq!(p("on").unwrap(), Cmd::Switch(true));
        assert_eq!(p("off").unwrap(), Cmd::Switch(false));
        assert_eq!(p("country NL").unwrap(), Cmd::Country("NL".into()));
        assert_eq!(p("country none").unwrap(), Cmd::Country(String::new()));
        assert_eq!(p("nordvpn").unwrap(), Cmd::NordVpn);
        assert_eq!(p("custom proxy.example 1080 --key").unwrap(), Cmd::Custom { host: "proxy.example".into(), port: 1080, key: true });
        assert_eq!(p("custom proxy.example 1080").unwrap(), Cmd::Custom { host: "proxy.example".into(), port: 1080, key: false });
        // The key is never an argument.
        for bad in ["nordvpn user pass", "custom h 0", "custom h x", "on --key", "frob", "country"] {
            assert!(p(bad).is_err(), "{bad}");
        }
    }

    /// Failure: `seedwell proxy` prints the key, or says a key is installed when none is.
    #[test]
    fn it_says_whether_a_key_is_installed_never_the_key() {
        let d = ProxyDto { enabled: true, kind: "nordvpn".into(), has_key: true, status: "On · NordVPN".into(), country: "NL".into(), ..Default::default() };
        assert!(words(&d).contains("Key: installed"), "{}", words(&d));
        let d = ProxyDto { kind: "custom".into(), host: "p.example".into(), status: "Off".into(), ..Default::default() };
        assert!(words(&d).contains("Key: none") && words(&d).contains("p.example:1080"), "{}", words(&d));
    }
}
