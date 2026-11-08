//! Who a folder or channel waits for, as one short sentence, and which of its devices are gone (SPEC §7G.5).
//!
//! A folder with members of which none is connected says whom it waits for and since when: "Waiting for Kristian's
//! Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)", or for several "Waiting for 2 devices:
//! Mac mini (3 days ago), Makro (1 hour ago)". A device not connected and not seen for [`GONE_AFTER_S`] (or never
//! seen here) is *gone*: an owner is offered to remove it, and a member whose every owner and admin is gone is
//! offered to share the folder again from this device. Nothing here acts on its own: removing a device and sharing
//! again are the user's choice.

use std::io;
use std::net::SocketAddr;

use seedwell_sync_proto::ids::DeviceId;
use seedwell_sync_proto::members::Role;

use crate::status::GoneDevice;

/// How long a device that is not connected stays "away" before it counts as gone (a day): an owner may remove it
/// then, and a member may share a folder again whose every owner and admin is gone.
pub const GONE_AFTER_S: u64 = 86_400;

/// Names listed in the sentence for several devices before "and N more".
const NAMES_SHOWN: usize = 3;

/// A member of a folder or channel that is not connected now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Absent {
    /// Its id.
    pub id: DeviceId,
    /// Its name (empty when it never said).
    pub name: String,
    /// When a session with it last began or ended, Unix seconds; `None`: never here.
    pub last_seen_s: Option<u64>,
    /// Why the last attempt to reach it failed ([`reach_problem`]).
    pub problem: Option<String>,
}

impl Absent {
    /// Its name as the sentences show it: "device MFZWI3D" when it never gave one.
    pub(crate) fn shown(&self) -> String {
        if self.name.trim().is_empty() {
            let id = self.id.to_string();
            format!("device {}", id.split('-').next().unwrap_or(&id))
        } else {
            self.name.trim().to_string()
        }
    }

    /// Not seen for `gone_after_s` (or never seen here): gone, as far as this device can tell.
    pub(crate) fn gone(&self, now: u64, gone_after_s: u64) -> bool {
        self.last_seen_s.is_none_or(|t| now.saturating_sub(t) >= gone_after_s)
    }
}

/// The members of a folder or channel other than this device, as far as being away goes.
#[derive(Clone, Debug, Default)]
pub(crate) struct Away {
    /// Now, Unix seconds.
    pub now: u64,
    /// How long a member stays away before it is gone ([`GONE_AFTER_S`]).
    pub gone_after_s: u64,
    /// Members connected now.
    pub connected: usize,
    /// One of them is an owner or admin.
    pub admitter_connected: bool,
    /// The members not connected, with their roles.
    pub absent: Vec<(Absent, Role)>,
    /// This device may remove members: an owner holding the folder's key.
    pub may_remove: bool,
}

impl Away {
    /// Nobody connected: whom it waits for ([`waiting`]).
    pub(crate) fn waiting(&self) -> Option<String> {
        if self.connected > 0 {
            return None;
        }
        let absent: Vec<Absent> = self.absent.iter().map(|(a, _)| a.clone()).collect();
        waiting(&absent, self.now)
    }

    /// The gone members this device may remove (none unless it is an owner).
    pub(crate) fn removable(&self) -> Vec<GoneDevice> {
        if !self.may_remove {
            return Vec::new();
        }
        self.absent
            .iter()
            .filter(|(a, _)| a.gone(self.now, self.gone_after_s))
            .map(|(a, _)| GoneDevice { id: a.id.to_string(), name: a.shown(), last_seen_s: a.last_seen_s })
            .collect()
    }

    /// This device, a member at `role`, cannot have anyone admitted or any role changed any more: it is no owner or
    /// admin itself, none is connected, and every one of them is gone.
    pub(crate) fn stuck(&self, role: Option<Role>) -> bool {
        let admitters: Vec<&Absent> = self.absent.iter().filter(|(_, r)| r.admits()).map(|(a, _)| a).collect();
        matches!(role, Some(Role::ReadWrite | Role::ReadOnly))
            && !self.admitter_connected
            && !admitters.is_empty()
            && admitters.iter().all(|a| a.gone(self.now, self.gone_after_s))
    }
}

/// "just now", "5 minutes ago", "1 hour ago", "3 days ago".
pub(crate) fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    let (n, unit) = match s {
        s if s < 60 => return "just now".into(),
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s => (s / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// The sentence of a folder or channel whose members are all away (`absent`, every member but this device): whom it
/// waits for, since when, and for one device why it cannot be reached. `None` when nobody is away.
pub(crate) fn waiting(absent: &[Absent], now: u64) -> Option<String> {
    match absent {
        [] => None,
        [one] => {
            let seen = match one.last_seen_s {
                Some(t) => format!("last seen {}", ago(now, t)),
                None => concat!("never connected to this ", seedwell_platform::computer!()).to_string(),
            };
            let why = one.problem.as_deref().map(|p| format!(": {p}")).unwrap_or_default();
            Some(format!("Waiting for {} ({seen}{why})", one.shown()))
        }
        several => {
            let mut sorted: Vec<&Absent> = several.iter().collect();
            sorted.sort_by_cached_key(|a| (a.shown().to_lowercase(), a.id.to_string()));
            let mut names: Vec<String> = sorted
                .iter()
                .take(NAMES_SHOWN)
                .map(|a| format!("{} ({})", a.shown(), a.last_seen_s.map_or_else(|| "never connected".to_string(), |t| ago(now, t))))
                .collect();
            let more = sorted.len().saturating_sub(NAMES_SHOWN);
            let list = if more > 0 {
                format!("{} and {more} more", names.join(", "))
            } else {
                let last = names.pop().unwrap_or_default();
                if names.is_empty() { last } else { format!("{}, {last}", names.join(", ")) }
            };
            Some(format!("Waiting for {} devices: {list}", several.len()))
        }
    }
}

/// Why a dial of a known device at `addr` failed, as the waiting sentence ends: "nothing answers at
/// 192.168.68.57:55799".
pub(crate) fn reach_problem(addr: SocketAddr, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => format!("nothing answers at {addr}"),
        io::ErrorKind::TimedOut => format!("no answer at {addr}"),
        io::ErrorKind::PermissionDenied => format!("another device answers at {addr} now"),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable | io::ErrorKind::AddrNotAvailable => {
            format!(concat!("{addr} is not reachable from this ", seedwell_platform::computer!(), "'s network"), addr = addr)
        }
        _ => format!("{addr}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn dev(n: u8, name: &str, seen: Option<u64>, problem: Option<&str>) -> Absent {
        Absent { id: DeviceId::of_public_key(&[n; 32]), name: name.into(), last_seen_s: seen, problem: problem.map(Into::into) }
    }

    #[test]
    fn one_device_is_named_with_when_it_was_last_seen_and_why_it_cannot_be_reached() {
        let mini = dev(1, "Kristian's Mac mini", Some(NOW - 3 * 86_400 - 5), Some("nothing answers at 192.168.68.57:55799"));
        assert_eq!(waiting(std::slice::from_ref(&mini), NOW).unwrap(), "Waiting for Kristian's Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)");
        let quiet = Absent { problem: None, last_seen_s: Some(NOW - 3_700), ..mini.clone() };
        assert_eq!(waiting(&[quiet], NOW).unwrap(), "Waiting for Kristian's Mac mini (last seen 1 hour ago)");
        let never = Absent { last_seen_s: None, ..mini.clone() };
        assert_eq!(
            waiting(std::slice::from_ref(&never), NOW).unwrap(),
            concat!("Waiting for Kristian's Mac mini (never connected to this ", seedwell_platform::computer!(), ": nothing answers at 192.168.68.57:55799)")
        );
        let nameless = Absent { name: " ".into(), problem: None, ..never };
        let first = nameless.id.to_string().split('-').next().unwrap().to_string();
        assert_eq!(waiting(&[nameless], NOW).unwrap(), format!(concat!("Waiting for device {} (never connected to this ", seedwell_platform::computer!(), ")"), first));
        assert_eq!(waiting(&[], NOW), None, "nobody away");
    }

    #[test]
    fn several_devices_are_counted_and_listed_by_name_with_when_each_was_seen() {
        let makro = dev(2, "Makro", Some(NOW - 3_600), Some("no answer at 10.0.0.2:1"));
        let mini = dev(1, "Mac mini", Some(NOW - 3 * 86_400), None);
        assert_eq!(waiting(&[makro.clone(), mini.clone()], NOW).unwrap(), "Waiting for 2 devices: Mac mini (3 days ago), Makro (1 hour ago)");
        let never = dev(3, "Bo's PC", None, None);
        assert_eq!(waiting(&[makro.clone(), mini.clone(), never.clone()], NOW).unwrap(), "Waiting for 3 devices: Bo's PC (never connected), Mac mini (3 days ago), Makro (1 hour ago)");
        let new = dev(4, "Studio", Some(NOW - 30), None);
        assert_eq!(
            waiting(&[makro, mini, never, new], NOW).unwrap(),
            "Waiting for 4 devices: Bo's PC (never connected), Mac mini (3 days ago), Makro (1 hour ago) and 1 more"
        );
    }

    #[test]
    fn times_and_problems_read_plainly() {
        assert_eq!(ago(NOW, NOW - 59), "just now");
        assert_eq!(ago(NOW, NOW - 60), "1 minute ago");
        assert_eq!(ago(NOW, NOW - 7_200), "2 hours ago");
        assert_eq!(ago(NOW, NOW - 86_400), "1 day ago");
        assert_eq!(ago(NOW, NOW + 10), "just now", "a clock behind");
        let a: SocketAddr = "192.168.68.57:55799".parse().unwrap();
        let e = |k| io::Error::new(k, "x");
        assert_eq!(reach_problem(a, &e(io::ErrorKind::ConnectionRefused)), "nothing answers at 192.168.68.57:55799");
        assert_eq!(reach_problem(a, &e(io::ErrorKind::TimedOut)), "no answer at 192.168.68.57:55799");
        assert!(reach_problem(a, &e(io::ErrorKind::NetworkUnreachable)).starts_with("192.168.68.57:55799 is not reachable from this "));
    }

    #[test]
    fn a_device_is_gone_after_a_day_away_or_when_never_seen() {
        assert!(dev(1, "A", None, None).gone(NOW, GONE_AFTER_S));
        assert!(dev(1, "A", Some(NOW - GONE_AFTER_S), None).gone(NOW, GONE_AFTER_S));
        assert!(!dev(1, "A", Some(NOW - GONE_AFTER_S + 1), None).gone(NOW, GONE_AFTER_S));
    }

    #[test]
    fn only_an_owner_is_offered_removals_and_only_a_member_whose_admitters_are_all_gone_may_share_again() {
        let old = dev(1, "Mac mini", Some(NOW - 2 * GONE_AFTER_S), None);
        let recent = dev(2, "Makro", Some(NOW - 60), None);
        let mut a = Away { now: NOW, gone_after_s: GONE_AFTER_S, absent: vec![(old.clone(), Role::Owner), (recent.clone(), Role::ReadWrite)], ..Away::default() };
        assert!(a.removable().is_empty(), "not an owner here");
        assert!(a.stuck(Some(Role::ReadWrite)) && a.stuck(Some(Role::ReadOnly)));
        assert!(!a.stuck(Some(Role::Admin)) && !a.stuck(None), "an admin admits; a device not admitted has nothing to share");
        assert_eq!(a.waiting().unwrap(), "Waiting for 2 devices: Mac mini (2 days ago), Makro (1 minute ago)");
        a.may_remove = true;
        assert_eq!(a.removable(), vec![GoneDevice { id: old.id.to_string(), name: "Mac mini".into(), last_seen_s: old.last_seen_s }], "only the one away a day");
        a.connected = 1;
        assert_eq!(a.waiting(), None, "someone is connected");
        a.admitter_connected = true;
        assert!(!a.stuck(Some(Role::ReadWrite)), "an admin is connected");
        a.admitter_connected = false;
        a.absent.push((Absent { last_seen_s: Some(NOW - 10), ..dev(3, "Studio", None, None) }, Role::Admin));
        assert!(!a.stuck(Some(Role::ReadWrite)), "an admin seen a moment ago may come back");
    }
}
