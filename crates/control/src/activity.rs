//! The agent activity log (SPEC 9A.3): every mutating call, who made it, with redacted arguments.
//! Kept in a bounded in-memory ring; the app can also register a sink to show or persist entries
//! ("Claude paused 3 downloads").

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::proto::{Request, clip};
use crate::redact::redact_source;
use crate::types::ActivityDto;

const CAP: usize = 1000;

type Sink = Arc<dyn Fn(&ActivityDto) + Send + Sync>;

/// Shared, cloneable handle to the log.
#[derive(Clone, Default)]
pub struct ActivityLog {
    inner: Arc<Mutex<VecDeque<ActivityDto>>>,
    sink: Arc<Mutex<Option<Sink>>>,
}

impl ActivityLog {
    /// Empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Call `f` for every future entry (replaces a previous sink). Keep it quick and non-blocking.
    pub fn set_sink(&self, f: impl Fn(&ActivityDto) + Send + Sync + 'static) {
        *self.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(f));
    }

    /// Record an entry.
    pub fn push(&self, e: ActivityDto) {
        let sink = self.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(s) = sink {
            s(&e);
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() >= CAP {
            g.pop_front();
        }
        g.push_back(e);
    }

    /// The newest `limit` entries, newest first.
    pub fn recent(&self, limit: usize) -> Vec<ActivityDto> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.iter().rev().take(limit).cloned().collect()
    }

    /// Number of entries held.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// No entries yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn n(count: usize, what: &str) -> String {
    format!("{count} {what}{}", if count == 1 { "" } else { "s" })
}

/// A one-line description of a request that is safe to store and show: ids shortened, magnet
/// links and URLs redacted, no file contents.
pub fn describe(r: &Request) -> String {
    let ids = |v: &[String]| n(v.len(), "transfer");
    let s = match r {
        Request::Hello(p) => format!("hello from {}", clip(&p.client, 64)),
        Request::Add(p) => {
            let src = p
                .source
                .as_deref()
                .or(p.magnet.as_deref())
                .or(p.url.as_deref())
                .or(p.torrent_path.as_deref())
                .map(redact_source)
                .unwrap_or_else(|| if p.torrent_base64.is_some() { "(.torrent data)".into() } else { "(nothing)".into() });
            let mut s = format!("add {src}");
            if let Some(d) = &p.save_path {
                s.push_str(&format!(" into {}", clip(d, 200)));
            }
            if p.paused {
                s.push_str(" paused");
            }
            if let Some(c) = &p.category {
                s.push_str(&format!(" category={}", clip(c, 32)));
            }
            s
        }
        Request::Pause(t) if t.all => "pause all".into(),
        Request::Pause(t) => format!("pause {}", ids(&t.ids)),
        Request::Resume(t) if t.all => "resume all".into(),
        Request::Resume(t) => format!("resume {}", ids(&t.ids)),
        Request::StreamUrl(p) => format!("stream {}", short(&p.id)),
        Request::StopStream(_) => "stop stream".into(),
        Request::MoveToTop(p) => format!("move {} to top", ids(&p.ids)),
        Request::SetPriority(p) => format!("set priority {} on {}", p.priority.as_str(), ids(&p.ids)),
        Request::SetFilePriorities(p) => format!("set file priorities on {}", short(&p.id)),
        Request::SetCategory(p) => format!("set category {} on {}", p.category.as_deref().map_or("(detect)".into(), |c| clip(c, 32)), ids(&p.ids)),
        Request::SetSeedGoal(p) => format!("set seeding goal on {}", ids(&p.ids)),
        Request::SetLimits(p) => {
            let mut parts = Vec::new();
            for (k, v) in [("down", p.down_limit), ("up", p.up_limit), ("alt_down", p.alt_down_limit), ("alt_up", p.alt_up_limit)] {
                if let Some(v) = v {
                    parts.push(format!("{k}={v}"));
                }
            }
            if let Some(a) = p.alt_speed {
                parts.push(format!("alt_speed={a}"));
            }
            format!("set limits {}", parts.join(" "))
        }
        Request::SetTrackerLimits(p) => format!(
            "set tracker limits on {} down={:?} up={:?}",
            clip(&p.id, 64),
            p.down_limit,
            p.up_limit
        ),
        Request::SetTrackerPex(p) => format!("{} peer exchange on tracker {}", if p.allow { "allow" } else { "forbid" }, clip(&p.id, 64)),
        Request::Reannounce(p) => format!("reannounce {}", ids(&p.ids)),
        Request::Recheck(p) => format!("recheck {}", ids(&p.ids)),
        Request::Remove(p) => format!("remove {}{} (step 1, asks for confirmation)", ids(&p.ids), if p.delete_files { " and delete files" } else { "" }),
        Request::RemoveConfirm(_) => "confirm removal".into(),
        Request::SettingsSet(p) => format!("change settings: {}", p.changes.keys().map(|k| clip(k, 64)).collect::<Vec<_>>().join(", ")),
        Request::SyncAdd(p) => format!("share the folder {}", clip(&p.path, 200)),
        // The key is the permission to the folder: never in the log.
        Request::SyncJoin(p) => format!("join a shared folder (key not logged) at {}", p.path.as_deref().map_or("the default location".into(), |d| clip(d, 200))),
        Request::SyncShare(p) => format!(
            "make a {} link to folder {}{} (link not logged)",
            clip(p.role.as_deref().unwrap_or("read_write"), 20),
            folder(&p.folder),
            p.chat.as_deref().map(|c| format!(" and channel {}", clip(c, 60))).unwrap_or_default()
        ),
        Request::SyncMembers(p) => format!("list the devices of folder {}", folder(&p.folder)),
        Request::SyncInvites(p) => format!("list the links of folder {} (links not logged)", folder(&p.folder)),
        Request::SyncRevokeInvite(p) => format!("revoke link {} of folder {}", clip(&p.invite, 40), folder(&p.folder)),
        Request::SyncSetRole(p) => format!("make device {} {} in folder {}", clip(&p.device, 60), clip(&p.role, 20), folder(&p.folder)),
        Request::SyncRemoveDevice(p) => format!("remove device {} from folder {}", clip(&p.device, 60), folder(&p.folder)),
        Request::SyncSetShareUntil(p) => match (p.days, p.extend) {
            (0, _) => format!("share folder {} with no end", folder(&p.folder)),
            (d, true) => format!("share folder {} {d} days longer", folder(&p.folder)),
            (d, false) => format!("share folder {} for {d} days", folder(&p.folder)),
        },
        Request::SyncRename(p) => format!("rename folder {} to {}", folder(&p.folder), clip(&p.name, 100)),
        Request::SyncMove(p) => format!("move folder {} to {}", folder(&p.folder), clip(&p.path, 200)),
        Request::SyncPause(p) => format!("pause folder {}", folder(&p.folder)),
        Request::SyncResume(p) => format!("resume folder {}", folder(&p.folder)),
        Request::SyncRescan(p) => format!("rescan folder {}", folder(&p.folder)),
        Request::SyncSetLanOnly(p) => format!("{} for folder {}", if p.lan_only { "sync only on the local network" } else { "sync everywhere" }, folder(&p.folder)),
        Request::SyncConflicts(p) => format!("list conflicts of folder {}", folder(&p.folder)),
        Request::SyncResolve(p) => format!("resolve conflict {} in folder {}, keep {}", clip(&p.copy, 200), folder(&p.folder), clip(&p.keep, 20)),
        Request::SyncAddAddress(p) => format!("add address {} to folder {}", clip(&p.address, 100), folder(&p.folder)),
        Request::SyncRemove(p) => format!(
            "remove folder {}{} (step 1, asks for confirmation)",
            folder(&p.folder),
            if p.delete_files { concat!(" and move it to the ", trs_platform::trash!()) } else { "" }
        ),
        Request::SyncRemoveConfirm(_) => "confirm folder removal".into(),
        Request::SyncSetSettings(p) => {
            let v = serde_json::to_value(p).unwrap_or_default();
            let keys: Vec<String> = v.as_object().into_iter().flatten().filter(|(_, v)| !v.is_null()).map(|(k, _)| k.clone()).collect();
            format!("change folder sync settings: {}", keys.join(", "))
        }
        Request::ChatNew(p) => format!("make the channel {}", clip(&p.name, 100)),
        // The link is the permission to the channel: never in the log.
        Request::ChatJoin(_) => "join a channel (link not logged)".into(),
        Request::ChatShare(p) => format!("make a {} link to channel {} (link not logged)", clip(p.role.as_deref().unwrap_or("read_write"), 20), folder(&p.channel)),
        Request::ChatSetRole(p) => format!("make device {} {} in channel {}", clip(&p.device, 60), clip(&p.role, 20), folder(&p.channel)),
        Request::ChatRemoveDevice(p) => format!("remove device {} from channel {}", clip(&p.device, 60), folder(&p.channel)),
        Request::ChatSetShareUntil(p) => format!("share channel {} for {} days{}", folder(&p.channel), p.days, if p.extend { " more" } else { "" }),
        // What it says stays out of the log; how long it is does not.
        Request::ChatSend(p) => format!("post {} bytes{} to channel {}", p.body.len(), if p.json { " of JSON" } else { "" }, folder(&p.channel)),
        Request::ChatAddAddress(p) => format!("add address {} to channel {}", clip(&p.address, 100), folder(&p.channel)),
        Request::ChatRename(p) => format!("rename channel {}", folder(&p.channel)),
        Request::ChatMute(p) => format!("{} channel {}", if p.muted { "mute" } else { "unmute" }, folder(&p.channel)),
        Request::ChatSetAgents(p) => format!("let agents {} in channel {}", clip(&p.agents, 20), folder(&p.channel)),
        Request::ChatSetWake(p) => match p.command.as_deref().filter(|c| !c.trim().is_empty()) {
            Some(c) => format!("wake-up command of channel {}: {}", folder(&p.channel), clip(c, 200)),
            None => format!("no wake-up command for channel {}", folder(&p.channel)),
        },
        Request::ChatLeave(p) => format!("leave channel {}", folder(&p.channel)),
        Request::ChatNewTopic(p) => format!("make the topic {} in channel {}", clip(&p.title, 100), folder(&p.channel)),
        Request::ChatDmSend(p) => format!("write {} bytes{} privately to device {}", p.body.len(), if p.json { " of JSON" } else { "" }, clip(&p.device, 60)),
        Request::ChatDmSetAgents(p) => format!("let agents {} with private messages", clip(&p.agents, 20)),
        other => other.method().name().to_owned(),
    };
    // A key pasted into any field (a name, a path) is masked too.
    clip(&crate::redact::scrub_keys(&s), 400)
}

fn short(id: &str) -> String {
    clip(id, 8)
}

/// A folder as the caller named it (an id, a prefix or a name), shortened.
fn folder(f: &str) -> String {
    clip(f, 80)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::*;

    #[test]
    fn ring_keeps_newest() {
        let log = ActivityLog::new();
        for i in 0..(CAP + 10) {
            log.push(ActivityDto { at: i as u64, ..Default::default() });
        }
        assert_eq!(log.len(), CAP);
        assert_eq!(log.recent(1)[0].at, (CAP + 9) as u64);
        assert_eq!(log.recent(3).len(), 3);
    }

    #[test]
    fn sink_sees_entries() {
        let log = ActivityLog::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        log.set_sink(move |e| s2.lock().unwrap().push(e.method.clone()));
        log.push(ActivityDto { method: "pause".into(), ..Default::default() });
        assert_eq!(*seen.lock().unwrap(), vec!["pause".to_owned()]);
    }

    #[test]
    fn descriptions_hide_secrets() {
        let r = Request::Add(AddParams {
            magnet: Some("magnet:?xt=urn:btih:ab&tr=https://t.org/announce?passkey=SECRET".into()),
            ..Default::default()
        });
        let d = describe(&r);
        assert!(d.contains("xt=urn:btih:ab") && !d.contains("SECRET"), "{d}");
        let r = Request::Add(AddParams { url: Some("https://dl.example/f.iso?sig=SECRET".into()), ..Default::default() });
        assert!(!describe(&r).contains("SECRET"));
        let r = Request::Pause(Targets { ids: vec!["a".into(), "b".into(), "c".into()], all: false });
        assert_eq!(describe(&r), "pause 3 transfers");
        let r = Request::SyncJoin(SyncJoinParams { key_or_link: "seedwell://sync/SWW-SECRETSECRET?name=x".into(), path: Some("/tmp/x".into()) });
        let d = describe(&r);
        assert!(!d.contains("SECRET") && d.contains("/tmp/x"), "{d}");
        let r = Request::SyncRename(SyncRenameParams { folder: "Docs".into(), name: "SWR-SECRETSECRET".into() });
        assert!(!describe(&r).contains("SECRET"), "{}", describe(&r));
    }
}
