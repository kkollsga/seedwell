//! "Other devices can't reach this PC": whether other devices can dial this computer, and what the user is told and
//! offered (SPEC §7G.3, *Can other devices reach this one*; the hosts' part: docs/build-76-firewall.md).
//!
//! Folder sync decides whether this device is blocked ([`seedwell_sync_net::Inbound`], from a dial back by a device it
//! reached). While it is, and the system's firewall has rules per program (Windows), the firewall's rule for this app is
//! read off the UI thread ([`seedwell_platform::firewall::state`]) and kept for [`FIREWALL_EVERY`]. The notice names the
//! reason, and when the rule is missing or a rule blocks Seedwell it carries the one action that helps: the command the
//! host runs with administrator rights. After it ran, the host calls [`SyncReach::recheck`]: the rule is read again,
//! the devices dial back again, and the notice goes once either says so.
//!
//! The host makes one [`SyncReach`] per app run and draws [`SyncReach::model`] (instant: a lock) as a banner at the top
//! of the Folder Sync page, asking again every `refresh_ms`.
//!
//! **QA only** (a state folder inside a temporary folder, as `SEEDWELL_QA_TRASH_DIR` is): `<state>/qa/inbound.txt`
//! holding `blocked` (or `blocked-internet`, `reachable`, `unknown`) stands for folder sync's verdict, and
//! `<state>/qa/firewall.txt` holding a line as the firewall check prints it (`missing|Public|False`,
//! `allowed|Private|True`, `blocked|Public|False`, anything else: not known) stands for the rule, on any system (so a
//! check sees the notice come and go without a firewall to change). Both are read at each [`SyncReach::model`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use seedwell_platform::firewall::{self, Rule, State};
use seedwell_sync_net::{Inbound, Network};

use crate::Core;

/// The firewall's rule is read again this often while this device is blocked (never while it is not).
pub const FIREWALL_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long one reading may take (PowerShell starts in about a second; a busy PC takes longer).
pub const FIREWALL_LIMIT: Duration = Duration::from_secs(20);
/// After [`SyncReach::recheck`] the new reading of the rule and the devices' new answer are waited for this long at
/// most (folder sync gives its check up after [`seedwell_sync_net::inbound::CHECK_LIMIT`]) before a notice says anything
/// else.
pub const RECHECK_LIMIT: Duration = Duration::from_secs(150);
/// How often the host asks [`SyncReach::model`]; [`REFRESH_SOON_MS`] while something is being read.
pub const REFRESH_MS: u32 = 60_000;
/// See [`REFRESH_MS`].
pub const REFRESH_SOON_MS: u32 = 2_000;

/// The one action of the notice: what the host runs with administrator rights (Windows: `ShellExecuteW` with the verb
/// `runas`, `program` as the file, `arguments` as the parameters, the window hidden), then
/// [`SyncReach::recheck`] once it ended, whatever its exit code.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncReachAction {
    /// The button: "Allow Seedwell through Windows Firewall…".
    pub title: String,
    /// `netsh.exe`.
    pub program: String,
    /// Its arguments as one string, quoted as netsh reads them.
    pub arguments: String,
}

/// The banner: other devices cannot reach this computer, why, and what helps.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncReachNotice {
    /// "Other devices can't reach this PC".
    pub title: String,
    /// Who could not connect, where, and the reason.
    pub body: String,
    /// `None`: nothing Seedwell can do (the Mac's firewall, the router, the rule is there already).
    pub action: Option<SyncReachAction>,
}

/// What the host draws, and when it asks again.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncReachModel {
    /// `None`: draw nothing (reachable, not known, or still being read).
    pub notice: Option<SyncReachNotice>,
    /// Ask [`SyncReach::model`] again after this long.
    pub refresh_ms: u32,
}

/// The words of this system ([`HERE`]), apart so a test can build another system's notice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Words {
    pub computer: &'static str,
    pub firewall: &'static str,
    /// Where the user lets an app in ([`firewall::SETTINGS_HINT`]).
    pub hint: &'static str,
}

/// This system's words.
pub(crate) const HERE: Words = Words { computer: seedwell_platform::COMPUTER, firewall: firewall::NAME, hint: firewall::SETTINGS_HINT };

/// The notice for `inbound`, with `rule`, the firewall's rules as read (`None`: this system has none to read, or they
/// are being read), for the program at `exe`. `None` unless this device is blocked. `waiting`: a check asked for after
/// the action is under way, so a rule that allows now is not yet contradicted by the devices' old answer. `policy`: the
/// action ran and the rule read after it is the same, so the system's policy decides (Windows: Group Policy, whose
/// rules netsh cannot change and whose setting can make the local ones count for nothing): no action, which would do
/// nothing again.
pub(crate) fn notice(inbound: &Inbound, rule: Option<&State>, exe: &str, waiting: bool, policy: bool, w: Words) -> Option<SyncReachNotice> {
    let Inbound::Blocked { network, by, addr } = inbound else { return None };
    let (c, fw) = (w.computer, w.firewall);
    let firewall_title = capitalized(fw);
    let lead = match network {
        Network::Local => format!("{by} could not connect to Seedwell on this {c} at {}", shown(*addr)),
        Network::Internet => format!("{by}, outside your network, could not connect to Seedwell on this {c} at {}", shown(*addr)),
    };
    let title = format!("Other devices can't reach this {c}");
    let action = |r: Rule| {
        firewall::allow_command(exe, r).map(|(program, arguments)| SyncReachAction { title: format!("Allow Seedwell through {fw}…"), program, arguments })
    };
    let Some(state) = rule else {
        let body = match network {
            Network::Local => format!("{lead}. {}", w.hint),
            Network::Internet => format!("{lead}. The router may not pass connections on to this {c}, or a firewall stops them. {}", w.hint),
        };
        return Some(SyncReachNotice { title, body: body.trim_end().to_string(), action: None });
    };
    let networks = state.profiles.join(", ");
    if policy && matches!(state.rule, Rule::Missing | Rule::Blocked) {
        let what = if state.rule == Rule::Blocked { format!("{firewall_title} blocks Seedwell") } else { format!("{firewall_title} does not let Seedwell in") };
        let body = format!("{lead}: {what}, and an administrator's policy controls it on this {c}, so Seedwell can't change it. Ask whoever manages this {c} to allow Seedwell.");
        return Some(SyncReachNotice { title, body, action: None });
    }
    let (body, action) = match state.rule {
        Rule::Missing => {
            let why = if state.asks == Some(false) && !networks.is_empty() {
                format!("{firewall_title} doesn't ask on this network ({networks}), so Seedwell can't be reached until it's allowed.")
            } else {
                "Allow Seedwell so your other devices can connect to it.".to_string()
            };
            (format!("{lead}: {fw} has no rule that lets other devices connect to Seedwell. {why}"), action(Rule::Missing))
        }
        Rule::Blocked => (format!("{lead}: {firewall_title} blocks Seedwell. Allow it so your other devices can connect to it."), action(Rule::Blocked)),
        Rule::Unknown => (format!("{lead}. {firewall_title} may be blocking it."), action(Rule::Unknown)),
        // Allowed now: the devices' new answer is on its way.
        Rule::Allowed if waiting => return None,
        Rule::Allowed => {
            let also = if *network == Network::Internet { ", or the router" } else { "" };
            (format!("{lead}. {firewall_title} lets Seedwell in, so something else stops it: another firewall or security app{also}."), None)
        }
    };
    Some(SyncReachNotice { title, body, action })
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// An address as the user reads it (an IPv4 one without its IPv6 form).
fn shown(a: SocketAddr) -> String {
    SocketAddr::new(a.ip().to_canonical(), a.port()).to_string()
}

#[derive(Default)]
struct Cache {
    /// The rule as last read, and when.
    rule: Option<(State, Instant)>,
    /// A reading is under way.
    reading: bool,
    /// The host's check after the action, while its answers are waited for.
    rechecked: Option<Recheck>,
    /// A rule the action did not change (the system's policy decides): no action while the rule reads so.
    policy: Option<Rule>,
}

/// A check after the action.
struct Recheck {
    /// When it was asked for.
    at: Instant,
    /// The rule as it read before (a reading after that says the same: the action changed nothing).
    before: Option<Rule>,
}

/// The notice's keeper: one per app run.
#[derive(uniffi::Object)]
pub struct SyncReach {
    core: Arc<Core>,
    cache: Arc<Mutex<Cache>>,
}

#[uniffi::export]
impl SyncReach {
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self { core, cache: Arc::new(Mutex::new(Cache::default())) })
    }

    /// What to draw now. Instant (a lock; in a QA run, two small files): it never waits for the firewall, which is
    /// read on a thread of its own while this device is blocked, and shows up at a later call.
    pub fn model(&self) -> SyncReachModel {
        let qa = self.qa_dir();
        let inbound = qa.as_deref().and_then(qa_inbound).unwrap_or_else(|| self.inbound());
        if !matches!(inbound, Inbound::Blocked { .. }) {
            return SyncReachModel { notice: None, refresh_ms: REFRESH_MS };
        }
        let now = Instant::now();
        let qa_rule = qa.as_deref().and_then(qa_firewall);
        let checking = self.inbound_checking();
        let (rule, waiting, policy, reading) = {
            let mut c = lock(&self.cache);
            let fresh_qa = qa_rule.is_some();
            let rule = match qa_rule {
                Some(s) => Some(s),
                None if !firewall::RULES => None,
                None => {
                    if c.rule.as_ref().is_none_or(|(_, at)| now.saturating_duration_since(*at) >= FIREWALL_EVERY) {
                        self.read_rule(&mut c);
                    }
                    match &c.rule {
                        Some((s, _)) => Some(s.clone()),
                        // Not read yet: nothing to say until it is.
                        None => return SyncReachModel { notice: None, refresh_ms: REFRESH_SOON_MS },
                    }
                }
            };
            let current = rule.as_ref().map(|s| s.rule);
            if c.policy.is_some() && c.policy != current {
                c.policy = None;
            }
            let mut waiting = false;
            if let Some(r) = &c.rechecked {
                // Read again since the check (a QA run's file is read at each call).
                let read_since = fresh_qa || (!c.reading && c.rule.as_ref().is_some_and(|(_, at)| *at >= r.at));
                if now.saturating_duration_since(r.at) >= RECHECK_LIMIT {
                    c.rechecked = None;
                } else if !read_since {
                    waiting = true;
                } else {
                    if let Some(before) = r.before
                        && matches!(before, Rule::Missing | Rule::Blocked)
                        && current == Some(before)
                    {
                        c.policy = Some(before);
                    }
                    // Allowed now: the devices' new answer is on its way, until folder sync has it or gives up.
                    waiting = current == Some(Rule::Allowed) && checking;
                    if let Some(r) = c.rechecked.as_mut() {
                        r.before = None;
                    }
                }
            }
            let policy = c.policy.is_some();
            (rule, waiting, policy, c.reading)
        };
        let exe = firewall::own_program().map(|p| p.display().to_string()).unwrap_or_default();
        let soon = waiting || reading;
        SyncReachModel {
            notice: notice(&inbound, rule.as_ref(), &exe, waiting, policy, HERE),
            refresh_ms: if soon { REFRESH_SOON_MS } else { REFRESH_MS },
        }
    }

    /// The host ran the action (it ended, whatever its exit code: the user may have said no to the system's question).
    /// Reads the rule again and has the devices dial back again; [`Self::model`] shows the outcome when both are in
    /// (or after [`RECHECK_LIMIT`] at most), and says nothing meanwhile. A rule that reads as before means the action
    /// changed nothing (the system's policy decides): the notice then says so, without the action. Returns at once.
    pub fn recheck(&self) {
        let qa_rule = self.qa_dir().and_then(|d| qa_firewall(&d));
        let mut c = lock(&self.cache);
        let before = qa_rule.as_ref().map(|s| s.rule).or_else(|| c.rule.as_ref().map(|(s, _)| s.rule));
        c.rechecked = Some(Recheck { at: Instant::now(), before });
        // (A QA run's file stands for the rule: nothing to read.)
        if firewall::RULES && qa_rule.is_none() {
            self.read_rule(&mut c);
        }
        drop(c);
        if let Some((h, _)) = self.core.backend.sync().chat_service() {
            h.check_inbound_again();
        }
    }
}

impl SyncReach {
    fn inbound(&self) -> Inbound {
        self.core.backend.sync().chat_service().map_or(Inbound::Unknown, |(h, _)| h.inbound())
    }

    /// Folder sync's check after the action is under way (without folder sync, as in a QA run: until the wait ends).
    fn inbound_checking(&self) -> bool {
        self.core.backend.sync().chat_service().is_none_or(|(h, _)| h.inbound_checking())
    }

    /// Starts reading the rule on a thread of its own, unless one is under way.
    fn read_rule(&self, c: &mut Cache) {
        if c.reading {
            return;
        }
        let Some(exe) = firewall::own_program() else { return };
        c.reading = true;
        let cache = self.cache.clone();
        let started = std::thread::Builder::new().name("seedwell-firewall".into()).spawn(move || {
            let state = firewall::state(&exe, FIREWALL_LIMIT).unwrap_or_else(State::unknown);
            let mut c = lock(&cache);
            c.rule = Some((state, Instant::now()));
            c.reading = false;
        });
        if started.is_err() {
            c.reading = false;
        }
    }

    /// The QA folder of an isolated run (`<state>/qa`), else `None`.
    fn qa_dir(&self) -> Option<PathBuf> {
        seedwell_platform::dirs::in_temp_folder(&self.core.dir).then(|| self.core.dir.join("qa"))
    }
}

fn qa_inbound(dir: &Path) -> Option<Inbound> {
    let s = std::fs::read_to_string(dir.join("inbound.txt")).ok()?;
    let addr = SocketAddr::from(([192, 0, 2, 1], 51710));
    let blocked = |network| Inbound::Blocked { network, by: "QA device".into(), addr };
    Some(match s.trim() {
        "blocked" => blocked(Network::Local),
        "blocked-internet" => blocked(Network::Internet),
        "reachable" => Inbound::Reachable,
        _ => Inbound::Unknown,
    })
}

fn qa_firewall(dir: &Path) -> Option<State> {
    std::fs::read_to_string(dir.join("firewall.txt")).ok().map(|s| firewall::parse(&s))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: Words = Words { computer: "PC", firewall: "Windows Firewall", hint: "" };
    const MAC: Words = Words { computer: "Mac", firewall: "the firewall", hint: "Let Seedwell in in the firewall's settings." };
    const EXE: &str = r"C:\Users\Kari Nordmann\AppData\Local\Programs\Seedwell\Seedwell.exe";

    fn blocked(network: Network) -> Inbound {
        Inbound::Blocked { network, by: "Kristian's MacBook".into(), addr: "[::ffff:192.168.68.73]:51710".parse().unwrap() }
    }

    fn rule(r: Rule, asks: Option<bool>) -> State {
        State { rule: r, profiles: vec!["Public".into()], asks }
    }

    /// The failure: a PC whose firewall has no rule for Seedwell, which no device can reach, told nothing or not
    /// offered the one action that helps; or a notice shown on a device that is not blocked.
    #[test]
    fn a_blocked_pc_without_a_rule_is_told_why_and_offered_the_command() {
        let n = notice(&blocked(Network::Local), Some(&rule(Rule::Missing, Some(false))), EXE, false, false, PC).unwrap();
        assert_eq!(n.title, "Other devices can't reach this PC");
        assert!(n.body.starts_with("Kristian's MacBook could not connect to Seedwell on this PC at 192.168.68.73:51710: Windows Firewall"), "{}", n.body);
        assert!(n.body.contains("doesn't ask on this network (Public)"), "{}", n.body);
        let a = n.action.unwrap();
        assert_eq!(a.title, "Allow Seedwell through Windows Firewall…");
        assert_eq!((a.program, a.arguments), firewall::allow_command(EXE, Rule::Missing).unwrap());
        for not_blocked in [Inbound::Unknown, Inbound::Reachable] {
            assert_eq!(notice(&not_blocked, Some(&rule(Rule::Missing, Some(false))), EXE, false, false, PC), None);
        }
    }

    /// The failure: the notice stays after the user allowed Seedwell (the rule is there now), or says something wrong
    /// while the devices' new answer is on its way; or a block rule offered the add command, which a block outlives.
    #[test]
    fn the_notice_goes_when_the_rule_appears_and_a_block_gets_its_own_words() {
        assert_eq!(notice(&blocked(Network::Local), Some(&rule(Rule::Allowed, None)), EXE, true, false, PC), None, "waiting for the dial back");
        let after = notice(&blocked(Network::Local), Some(&rule(Rule::Allowed, None)), EXE, false, false, PC).unwrap();
        assert_eq!(after.action, None, "nothing more Seedwell can do");
        assert!(after.body.contains("lets Seedwell in, so something else stops it"), "{}", after.body);
        let b = notice(&blocked(Network::Local), Some(&rule(Rule::Blocked, Some(true))), EXE, false, false, PC).unwrap();
        assert!(b.body.contains("Windows Firewall blocks Seedwell"), "{}", b.body);
        assert_eq!(b.action.unwrap().arguments, firewall::allow_command(EXE, Rule::Blocked).unwrap().1);
    }

    /// The failure: a Mac (no rules to read; its firewall asks by itself) offered an action, or not told at all.
    #[test]
    fn a_blocked_mac_gets_a_plain_notice() {
        let n = notice(&blocked(Network::Local), None, "/Applications/Seedwell.app/Contents/MacOS/Seedwell", false, false, MAC).unwrap();
        assert_eq!(n.title, "Other devices can't reach this Mac");
        assert_eq!(n.action, None);
        assert!(n.body.ends_with("Let Seedwell in in the firewall's settings."), "{}", n.body);
        let far = notice(&blocked(Network::Internet), None, "", false, false, MAC).unwrap();
        assert!(far.body.contains("outside your network") && far.body.contains("router"), "{}", far.body);
    }

    /// The failure: the host's poll (on its UI thread) waits for the firewall's rule to be read (PowerShell: seconds).
    /// The first poll answers at once: on a system with rules, nothing yet and a poll again soon; elsewhere the plain
    /// notice.
    #[test]
    fn the_first_poll_does_not_wait_for_the_firewall() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(crate::CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
        std::fs::create_dir_all(dir.path().join("qa")).unwrap();
        std::fs::write(dir.path().join("qa/inbound.txt"), "blocked").unwrap();
        let reach = SyncReach::new(core);
        let started = Instant::now();
        let m = reach.model();
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        if firewall::RULES {
            assert_eq!(m, SyncReachModel { notice: None, refresh_ms: REFRESH_SOON_MS });
        } else {
            assert_eq!(m.notice.expect("the plain notice").action, None);
        }
    }

    /// The failure (review): under an administrator's policy (Group Policy) netsh changes nothing, and the button came
    /// back after every click, for ever. Once a check after the action reads the same rule, the notice says the policy
    /// decides and offers nothing; a rule that changes later (the administrator allowed it) is told as usual.
    #[test]
    fn a_rule_the_action_did_not_change_drops_the_button() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(crate::CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
        let qa = dir.path().join("qa");
        std::fs::create_dir_all(&qa).unwrap();
        std::fs::write(qa.join("inbound.txt"), "blocked").unwrap();
        std::fs::write(qa.join("firewall.txt"), "missing|Domain|False").unwrap();
        let reach = SyncReach::new(core);
        assert!(reach.model().notice.unwrap().action.is_some());
        reach.recheck();
        let n = reach.model().notice.expect("still blocked");
        assert_eq!(n.action, None, "{n:?}");
        assert!(n.body.contains("an administrator's policy controls it"), "{}", n.body);
        assert!(reach.model().notice.unwrap().action.is_none(), "and stays so");
        std::fs::write(qa.join("firewall.txt"), "blocked|Domain|False").unwrap();
        assert!(reach.model().notice.unwrap().action.is_some(), "another rule: the action again");
        // With the words of the policy notice for the other rule too.
        let pol = notice(&blocked(Network::Local), Some(&rule(Rule::Blocked, None)), EXE, false, true, PC).unwrap();
        assert!(pol.body.contains("Windows Firewall blocks Seedwell, and an administrator's policy"), "{}", pol.body);
    }

    /// Acts as the host does: polls `model` with the QA files standing for folder sync's verdict and the firewall's
    /// rule. The failures: the notice not drawn while blocked with the rule missing, still drawn once the rule is there
    /// (after the host's recheck and its wait), or a call that waits for the firewall (the host polls on its UI thread).
    #[test]
    fn the_host_sees_the_notice_come_and_go_and_never_waits() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(crate::CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
        let qa = dir.path().join("qa");
        std::fs::create_dir_all(&qa).unwrap();
        let reach = SyncReach::new(core);
        assert_eq!(reach.model().notice, None, "not blocked");
        std::fs::write(qa.join("inbound.txt"), "blocked").unwrap();
        std::fs::write(qa.join("firewall.txt"), "missing|Public|False").unwrap();
        let started = Instant::now();
        let m = reach.model();
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        let n = m.notice.expect("blocked, no rule");
        assert!(n.action.is_some());
        // The user allowed it: the rule is there; the host checks again.
        std::fs::write(qa.join("firewall.txt"), "allowed|Public|False").unwrap();
        reach.recheck();
        let m = reach.model();
        assert_eq!(m.notice, None, "the rule is there");
        assert_eq!(m.refresh_ms, REFRESH_SOON_MS, "asks again soon while the devices answer");
        // The devices could reach it.
        std::fs::write(qa.join("inbound.txt"), "reachable").unwrap();
        assert_eq!(reach.model(), SyncReachModel { notice: None, refresh_ms: REFRESH_MS });
    }
}
