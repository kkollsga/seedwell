//! What a joining folder or channel is doing, and why it still waits, as short sentences (SPEC §7G.5). The hosts
//! show them as they are (where "joining" shows); the agent interface gives them as `progress` and `problem`.
//!
//! A join looks for one of the folder's owners or admins: at the addresses its link carries, through LAN discovery
//! and through the DHT. Each dial and each session it starts leaves a trace in the space's [`JoinTrail`]; these
//! functions turn that trail into what the person sees: "Looking for Anna's Mac mini on the local network…",
//! "Trying Anna's Mac mini at 192.168.68.57:55799…", "Anna's Mac mini at 192.168.68.57:55799: no answer".

use std::io;
use std::net::SocketAddr;

use crate::shared::JoinTrail;

/// How this device can look for the device that made the link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ways {
    /// The link carries addresses.
    pub link: bool,
    /// LAN discovery runs.
    pub lan: bool,
    /// The DHT is asked (folders that are not local-network-only).
    pub internet: bool,
}

/// Whom the join is looking for: the link's maker by name, else any owner or admin.
fn whom(noun: &str, maker: Option<&str>) -> String {
    match maker.filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => format!("an owner or admin of this {noun}"),
    }
}

/// The progress sentence of a joining folder or channel (`noun`). `maker`: the name of the device that made the
/// link, and `maker_addrs` the addresses the link gives for it.
pub(crate) fn progress(noun: &str, maker: Option<&str>, maker_addrs: &[SocketAddr], trail: &JoinTrail, ways: Ways) -> String {
    let who = whom(noun, maker);
    if let Some(a) = trail.trying {
        return if maker.is_some_and(|m| !m.is_empty()) && maker_addrs.contains(&a) { format!("Trying {who} at {a}…") } else { format!("Trying {a}…") };
    }
    if let Some(name) = &trail.reached {
        return format!(concat!("Found {name}, which cannot admit this ", trs_platform::computer!(), "; waiting for an owner or admin of this {noun}"), name = name, noun = noun);
    }
    let mut places: Vec<&str> = Vec::new();
    if ways.link {
        places.push("at the addresses in the link");
    }
    if ways.lan {
        places.push("on the local network");
    }
    if ways.internet {
        places.push("on the Internet");
    }
    match places.as_slice() {
        [] => format!("Waiting for {who}: this {} looks on neither the local network nor the Internet (Settings), so add its address", trs_platform::computer!()),
        [one] => format!("Looking for {who} {one}…"),
        [first @ .., last] => format!("Looking for {who} {} and {last}…", first.join(", ")),
    }
}

/// Why a dial of `addr` failed, for the joining folder's status. `maker`: the link maker's name when `addr` is one
/// the link gives for it.
pub(crate) fn dial_problem(maker: Option<&str>, addr: SocketAddr, e: &io::Error) -> String {
    let at = match maker.filter(|m| !m.is_empty()) {
        Some(m) => format!("{m} at {addr}"),
        None => addr.to_string(),
    };
    match e.kind() {
        io::ErrorKind::ConnectionRefused => format!("{at}: nothing answers there (is Seedwell running on it?)"),
        io::ErrorKind::TimedOut => format!("{at}: no answer"),
        io::ErrorKind::PermissionDenied => format!("{at}: another device answers there now"),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable | io::ErrorKind::AddrNotAvailable => {
            format!(concat!("{at}: not reachable from this ", trs_platform::computer!(), "'s network"), at = at)
        }
        _ => format!("{at}: {e}"),
    }
}

/// A device answered but did not admit this one.
pub(crate) fn not_admitted(name: &str, noun: &str) -> String {
    format!(
        concat!(
            "{name} answered but does not admit this ",
            trs_platform::computer!(),
            ": it is not an owner or admin of this {noun}, or the {noun} is paused or shared only on its local network there"
        ),
        name = name,
        noun = noun
    )
}

/// A folder or channel (`name`) one of the user's own devices put on all of them (`by`: that device's name), waiting
/// to be admitted (0.1.13).
pub(crate) fn from_your_devices(name: &str, by: Option<&str>) -> String {
    let by = by.filter(|b| !b.is_empty()).map(|b| format!(" ({b})")).unwrap_or_default();
    format!(concat!("Waiting for one of your devices that can admit this ", trs_platform::computer!(), " to “{name}”{by}…"), name = name, by = by)
}

/// A device that has no channels (its Seedwell is older than 0.1.6's chat bit) was met while joining a channel.
pub(crate) fn no_channels(name: &str, client: &str) -> String {
    format!("{name} runs {}, which has no channels: update it", if client.is_empty() { "an older Seedwell" } else { client })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Ways = Ways { link: true, lan: true, internet: true };

    #[test]
    fn the_sentence_follows_the_join() {
        let a: SocketAddr = "192.168.68.57:55799".parse().unwrap();
        let mut t = JoinTrail::default();
        let maker = Some("Anna's Mac mini");
        assert_eq!(progress("folder", maker, &[a], &t, ALL), "Looking for Anna's Mac mini at the addresses in the link, on the local network and on the Internet…");
        assert_eq!(progress("channel", None, &[], &t, Ways { lan: true, ..Ways::default() }), "Looking for an owner or admin of this channel on the local network…");
        assert_eq!(progress("folder", None, &[], &t, Ways { lan: true, internet: true, link: false }), "Looking for an owner or admin of this folder on the local network and on the Internet…");
        assert!(progress("folder", maker, &[], &t, Ways::default()).starts_with("Waiting for Anna's Mac mini: this "));
        t.trying = Some(a);
        assert_eq!(progress("folder", maker, &[a], &t, ALL), "Trying Anna's Mac mini at 192.168.68.57:55799…");
        assert_eq!(progress("folder", maker, &[], &t, ALL), "Trying 192.168.68.57:55799…", "an address the link does not give");
        t.trying = None;
        t.reached = Some("Bo's PC".into());
        assert!(progress("folder", maker, &[a], &t, ALL).starts_with("Found Bo's PC, which cannot admit this "));
    }

    #[test]
    fn a_folder_from_your_devices_says_whom_it_waits_for() {
        let s = from_your_devices("Dev", Some("Mac mini"));
        assert!(s.starts_with("Waiting for one of your devices that can admit this ") && s.ends_with(" to “Dev” (Mac mini)…"), "{s}");
        assert!(from_your_devices("Dev", None).ends_with(" to “Dev”…"));
    }

    #[test]
    fn a_failed_dial_says_what_happened() {
        let a: SocketAddr = "192.168.68.57:55799".parse().unwrap();
        let e = |k| io::Error::new(k, "x");
        assert_eq!(dial_problem(Some("Anna's Mac mini"), a, &e(io::ErrorKind::TimedOut)), "Anna's Mac mini at 192.168.68.57:55799: no answer");
        assert_eq!(dial_problem(None, a, &e(io::ErrorKind::ConnectionRefused)), "192.168.68.57:55799: nothing answers there (is Seedwell running on it?)");
        assert!(dial_problem(None, a, &e(io::ErrorKind::PermissionDenied)).ends_with("another device answers there now"));
        assert!(dial_problem(None, a, &e(io::ErrorKind::NetworkUnreachable)).contains("not reachable"));
        assert_eq!(no_channels("Bo's PC", "Seedwell 0.1.5"), "Bo's PC runs Seedwell 0.1.5, which has no channels: update it");
        assert!(not_admitted("Bo's PC", "folder").starts_with("Bo's PC answered but does not admit this "));
    }
}
