//! The agent activity log (SPEC 9A.3): every mutating call, who made it, with redacted arguments.
//! Kept in a bounded in-memory ring; the app can also register a sink to show entries ("Claude paused 3 downloads").
//! The app's and the daemon's log is also kept on disk ([`ActivityLog::persisted`]), so it survives a restart or an
//! update, which an agent can start itself (`seedwell restart`).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proto::{Request, clip};
use crate::redact::redact_source;
use crate::types::ActivityDto;

const CAP: usize = 1000;

/// The name of the log's file in the state folder.
pub const ACTIVITY_FILE: &str = "agent-activity.jsonl";

type Sink = Arc<dyn Fn(&ActivityDto) + Send + Sync>;

/// Shared, cloneable handle to the log.
#[derive(Clone, Default)]
pub struct ActivityLog {
    inner: Arc<Mutex<VecDeque<ActivityDto>>>,
    sink: Arc<Mutex<Option<Sink>>>,
    /// The writer of the file the log is kept in, when it is kept.
    disk: Option<Sender<Disk>>,
}

/// A line of the log's file: the entry with what an agent's `activity` filters on (its channel, private messages),
/// which the answer leaves out, so an entry about a channel closed to agents stays hidden after a restart.
#[derive(Serialize, Deserialize)]
struct Line {
    at: u64,
    client: String,
    method: String,
    summary: String,
    outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    direct: bool,
}

impl Line {
    fn of(e: &ActivityDto) -> Self {
        Self {
            at: e.at,
            client: e.client.clone(),
            method: e.method.clone(),
            summary: e.summary.clone(),
            outcome: e.outcome.clone(),
            channel: e.channel.clone(),
            direct: e.direct,
        }
    }

    fn entry(&self) -> ActivityDto {
        let l = self;
        ActivityDto {
            at: l.at,
            client: l.client.clone(),
            method: l.method.clone(),
            summary: l.summary.clone(),
            outcome: l.outcome.clone(),
            channel: l.channel.clone(),
            direct: l.direct,
        }
    }
}

enum Disk {
    Entry(ActivityDto),
    /// Answered once everything before it is on disk.
    #[cfg_attr(not(test), allow(dead_code))]
    Flush(Sender<()>),
}

impl ActivityLog {
    /// Empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// A log kept in `path` too (JSON lines, the newest [`CAP`] entries): what the file holds is read back, and each
    /// new entry is added to it. The file is read and written on a thread of its own, so neither this nor `push`
    /// waits for the disk; entries pushed before the file is read stay the newest. A file that cannot be read or
    /// written leaves the log in memory only, as before.
    pub fn persisted(path: PathBuf) -> Self {
        let (tx, rx) = channel();
        let log = Self { disk: Some(tx), ..Self::default() };
        let inner = log.inner.clone();
        let spawned = std::thread::Builder::new().name("activity-log".into()).spawn(move || keep(&path, &inner, rx));
        if spawned.is_err() {
            return Self::new();
        }
        log
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
        if let Some(d) = &self.disk {
            let _ = d.send(Disk::Entry(e.clone()));
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() >= CAP {
            g.pop_front();
        }
        g.push_back(e);
    }

    /// Waits until every entry pushed so far is in the file (tests).
    #[cfg(test)]
    fn flush(&self) {
        if let Some(d) = &self.disk {
            let (tx, rx) = channel();
            if d.send(Disk::Flush(tx)).is_ok() {
                let _ = rx.recv();
            }
        }
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

/// The file's writer: reads what the file holds into the front of the ring, then appends each entry, and rewrites the
/// file with the newest [`CAP`] once it holds twice that many.
fn keep(path: &Path, inner: &Mutex<VecDeque<ActivityDto>>, rx: Receiver<Disk>) {
    // The newest CAP lines of the file, as the next rewrite writes them.
    let (mut kept, mut lines, torn) = read_lines(path);
    {
        let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
        for l in kept.iter().rev() {
            if g.len() >= CAP {
                break;
            }
            g.push_front(l.entry());
        }
    }
    let mut file = None;
    // A file grown past CAP over earlier runs, or a last line cut off by a crash (it would join the next one): write it
    // afresh.
    if lines > CAP || torn {
        lines = rewrite(path, &kept).unwrap_or(lines);
    }
    for m in rx {
        let e = match m {
            Disk::Entry(e) => e,
            Disk::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        let line = Line::of(&e);
        let Ok(mut text) = serde_json::to_string(&line) else { continue };
        text.push('\n');
        if kept.len() >= CAP {
            kept.pop_front();
        }
        kept.push_back(line);
        if lines >= 2 * CAP {
            file = None;
            lines = rewrite(path, &kept).unwrap_or(lines);
            continue;
        }
        if file.is_none() {
            file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        }
        let Some(f) = file.as_mut() else { continue };
        if f.write_all(text.as_bytes()).is_ok() {
            lines += 1;
        } else {
            file = None;
        }
    }
}

/// The newest [`CAP`] lines in the file, oldest first (a line that does not read, such as the last one cut off by a
/// crash, is skipped); how many lines the file holds; and whether its last line is cut off.
fn read_lines(path: &Path) -> (VecDeque<Line>, usize, bool) {
    let mut v = VecDeque::new();
    let Ok(f) = std::fs::File::open(path) else { return (v, 0, false) };
    let (mut lines, mut torn) = (0, false);
    let mut r = BufReader::new(f);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        lines += 1;
        torn = !line.ends_with('\n');
        if let Ok(l) = serde_json::from_str::<Line>(line.trim_end()) {
            if v.len() >= CAP {
                v.pop_front();
            }
            v.push_back(l);
        }
    }
    (v, lines, torn)
}

/// Writes `kept` as the whole file (beside it, then moved over it); the number of lines written.
fn rewrite(path: &Path, kept: &VecDeque<Line>) -> Option<usize> {
    let mut text = String::new();
    for l in kept {
        text.push_str(&serde_json::to_string(l).ok()?);
        text.push('\n');
    }
    let tmp = path.with_extension("jsonl.new");
    std::fs::write(&tmp, text).ok()?;
    std::fs::rename(&tmp, path).ok()?;
    Some(kept.len())
}

fn n(count: usize, what: &str) -> String {
    format!("{count} {what}{}", if count == 1 { "" } else { "s" })
}

/// " and 2 images", " and 1 file": how many attachments a post has (never what they are).
fn attached(images: usize, files: usize) -> String {
    match (images, files) {
        (0, 0) => String::new(),
        (i, 0) => format!(" and {i} image{}", if i == 1 { "" } else { "s" }),
        (0, f) => format!(" and {f} file{}", if f == 1 { "" } else { "s" }),
        (i, f) => format!(" and {} attachments", i + f),
    }
}

/// Where a file of a message is: a channel, or a private conversation.
fn file_place(channel: &str, device: Option<&str>) -> String {
    match device {
        Some(d) => format!("the private conversation with device {}", clip(d, 60)),
        None => format!("channel {}", folder(channel)),
    }
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
        Request::SettingsSet(p) => match (&p.file, &p.doc) {
            (Some(f), Some(doc)) => {
                let count = |key: &str, what: &str| doc.get(key).and_then(Value::as_array).map_or_else(String::new, |a| format!(" ({})", n(a.len(), what)));
                let what = match f.as_str() {
                    "rules" => count("rule", "rule"),
                    "trackers" => count("tracker", "favorite tracker"),
                    _ => String::new(),
                };
                format!("replace {}.toml{what}", clip(f, 20))
            }
            // What changed comes after it, from the answer (a key given its value in force changes nothing).
            _ => "change settings".into(),
        },
        Request::TrackerLogClear(_) => "clear the tracker log".into(),
        Request::ProxySet(p) => {
            let mut parts = Vec::new();
            if let Some(c) = &p.country {
                parts.push(if c.is_empty() { "no country preference".to_owned() } else { format!("country {}", clip(c, 8)) });
            }
            if let Some(on) = p.on {
                parts.push(if on { "on".into() } else { "off".into() });
            }
            format!("torrent proxy: {}", parts.join(", "))
        }
        // The key is the user's account: never in the log, not even its username.
        Request::ProxyKey(p) => match p.kind.as_str() {
            "custom" => format!(
                "set up the proxy {}:{} for torrents (key not logged)",
                clip(p.host.as_deref().unwrap_or(""), 100),
                p.port.unwrap_or(0)
            ),
            _ => "set up the NordVPN proxy for torrents (key not logged)".into(),
        },
        Request::ProxyRemoveKey(_) => "remove the torrent proxy's key".into(),
        Request::ProxyFaster(_) => "find a faster proxy server".into(),
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
        Request::SyncReshare(p) => format!("share folder {} again from this {}", folder(&p.folder), seedwell_platform::computer!()),
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
        Request::SyncReadOnly(p) => format!("read whether folder {} is Read only", folder(&p.folder)),
        Request::SyncSetReadOnly(p) => format!("{} folder {}", if p.read_only { "make Read only:" } else { "turn Read only off for" }, folder(&p.folder)),
        Request::SyncSetLanOnly(p) => format!("{} for folder {}", if p.lan_only { "sync only on the local network" } else { "sync everywhere" }, folder(&p.folder)),
        Request::SyncConflicts(p) => format!("list conflicts of folder {}", folder(&p.folder)),
        Request::SyncResolve(p) => format!("resolve conflict {} in folder {}, keep {}", clip(&p.copy, 200), folder(&p.folder), clip(&p.keep, 20)),
        Request::SyncAddAddress(p) => format!("add address {} to folder {}", clip(&p.address, 100), folder(&p.folder)),
        Request::SyncSetMode(p) => format!("make folder {} {}", folder(&p.folder), if p.mode == "full" { "full" } else { "on demand" }),
        Request::SyncFetch(p) => format!("fetch {} in folder {}", item(&p.item, &p.path), folder(&p.folder)),
        Request::SyncCancelFetch(p) => format!("cancel the fetch of {} in folder {}", item(&p.item, &p.path), folder(&p.folder)),
        Request::SyncPin(p) => format!("keep {} on this {} (folder {})", item(&p.item, &p.path), seedwell_platform::computer!(), folder(&p.folder)),
        Request::SyncUnpin(p) => format!("stop keeping {} on this {} (folder {})", item(&p.item, &p.path), seedwell_platform::computer!(), folder(&p.folder)),
        Request::SyncEvict(p) => format!("remove the download of {} (folder {})", item(&p.item, &p.path), folder(&p.folder)),
        Request::SyncHostWrite(p) => format!("write {} in folder {}", clip(&p.path, 200), folder(&p.folder)),
        Request::SyncHostMkdir(p) => format!("make folder {} in folder {}", item(&p.item, &p.path), folder(&p.folder)),
        Request::SyncHostDelete(p) => format!("delete {} in folder {} (on every device)", item(&p.item, &p.path), folder(&p.folder)),
        Request::SyncHostRename(p) => format!("rename {} to {} in folder {}", clip(&p.from, 200), clip(&p.to, 200), folder(&p.folder)),
        Request::SyncRemove(p) => format!(
            "remove folder {}{} (step 1, asks for confirmation)",
            folder(&p.folder),
            if p.delete_files { concat!(" and move it to the ", seedwell_platform::trash!()) } else { "" }
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
        Request::ChatSend(p) => {
            let images = attached(p.attachments.len(), p.files.len());
            format!("post {} bytes{}{images} to channel {}", p.body.len(), if p.json { " of JSON" } else { "" }, folder(&p.channel))
        }
        Request::ChatAddAddress(p) => format!("add address {} to channel {}", clip(&p.address, 100), folder(&p.channel)),
        Request::ChatRename(p) => format!("rename channel {}", folder(&p.channel)),
        Request::ChatMute(p) => format!("{} channel {}", if p.muted { "mute" } else { "unmute" }, folder(&p.channel)),
        Request::ChatGroupNew(p) => format!("make the sidebar group {}{}", clip(&p.name, 100), p.icon.as_deref().map(|i| format!(" (icon {})", clip(i, 20))).unwrap_or_default()),
        Request::ChatGroupEdit(p) => {
            let mut what = Vec::new();
            if let Some(n) = &p.name {
                what.push(format!("rename it {}", clip(n, 100)));
            }
            if let Some(i) = &p.icon {
                what.push(format!("icon {}", clip(i, 20)));
            }
            if let Some(at) = p.position {
                what.push(format!("move it to {at}"));
            }
            format!("change the sidebar group {}: {}", clip(&p.group, 80), what.join(", "))
        }
        Request::ChatGroupDelete(p) => format!("delete the sidebar group {}", clip(&p.group, 80)),
        Request::ChatSetGroup(p) => match &p.group {
            Some(g) => format!("list channel {} under the group {}", folder(&p.channel), clip(g, 80)),
            None => format!("list channel {} under no group", folder(&p.channel)),
        },
        Request::ChatSetWake(p) => match p.command.as_deref().filter(|c| !c.trim().is_empty()) {
            Some(c) => format!("wake-up command of channel {}: {}", folder(&p.channel), clip(c, 200)),
            None => format!("no wake-up command for channel {}", folder(&p.channel)),
        },
        Request::ChatSetAgents(p) => format!("set Agents: {} for channel {}", clip(&p.agents, 10), folder(&p.channel)),
        Request::ChatLeave(p) => format!("leave channel {}", folder(&p.channel)),
        Request::ChatNewTopic(p) => format!("make the topic {} in channel {}", clip(&p.title, 100), folder(&p.channel)),
        Request::ChatDmSend(p) => {
            format!("write {} bytes{}{} privately to device {}", p.body.len(), if p.json { " of JSON" } else { "" }, attached(0, p.files.len()), clip(&p.device, 60))
        }
        // Which file, never its name or where it goes.
        Request::ChatDownload(p) => format!("download file {} of {}", clip(&p.hash, 16), file_place(&p.channel, p.device.as_deref())),
        Request::ChatCancelDownload(p) => format!("cancel the download of file {} of {}", clip(&p.hash, 16), file_place(&p.channel, p.device.as_deref())),
        Request::ChatStopSharing(p) => format!("stop sharing file {} of {}", clip(&p.hash, 16), file_place(&p.channel, p.device.as_deref())),
        Request::UpdateDevBuilds(p) => format!("turn development builds {}", if p.on { "on" } else { "off" }),
        Request::UpdateNow(_) => "check for the latest release and install it".into(),
        Request::Restart(_) => "restart Seedwell".into(),
        Request::AgentsRequest(p) => match crate::access::Want::parse(&p.what, p.target.as_deref()) {
            Ok(w) => format!("ask the user for {}", w.text()),
            Err(_) => format!("ask the user for `{}`", clip(&p.what, 40)),
        },
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

/// An item of an on-demand folder, by path or identifier.
fn item(id: &Option<String>, path: &Option<String>) -> String {
    match (path, id) {
        (Some(p), _) => clip(p, 200),
        (None, Some(i)) => format!("item {}", clip(i, 80)),
        (None, None) => "the whole folder".into(),
    }
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

    fn entry(at: u64, method: &str) -> ActivityDto {
        ActivityDto { at, client: "claude".into(), method: method.into(), summary: format!("{method} {at}"), outcome: "ok".into(), ..Default::default() }
    }

    #[test]
    fn a_restart_keeps_the_log_and_what_hides_an_entry_from_agents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-activity.jsonl");
        let log = ActivityLog::persisted(path.clone());
        log.push(entry(1, "pause"));
        log.push(ActivityDto { channel: Some("c1".into()), ..entry(2, "chat.send") });
        log.push(ActivityDto { direct: true, ..entry(3, "chat.dm_send") });
        log.flush();
        drop(log);

        let again = ActivityLog::persisted(path);
        again.push(entry(4, "restart"));
        again.flush();
        let got = again.recent(10);
        assert_eq!(got.iter().map(|e| e.at).collect::<Vec<_>>(), [4, 3, 2, 1], "the old entries come back, older than the new one");
        assert_eq!(got[2].channel.as_deref(), Some("c1"), "an entry about a channel stays tied to it, so a closed channel still hides it");
        assert!(got[1].direct, "an entry about private messages stays marked as such");
        assert_eq!(got[3], entry(1, "pause"));
    }

    #[test]
    fn the_file_stays_bounded_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-activity.jsonl");
        let log = ActivityLog::persisted(path.clone());
        for i in 0..(5 * CAP as u64 + 7) {
            log.push(entry(i, "pause"));
        }
        log.flush();
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= 2 * CAP, "the file grew to {lines} lines");
        drop(log);
        let again = ActivityLog::persisted(path);
        again.flush();
        assert_eq!(again.len(), CAP);
        assert_eq!(again.recent(1)[0].at, 5 * CAP as u64 + 6, "the newest entry survives the rewrite");
        assert_eq!(again.recent(CAP).last().unwrap().at, 4 * CAP as u64 + 7);
    }

    /// Catches a file trimmed only within one run: the line count was taken from the entries read back (at most CAP), so
    /// runs of fewer than CAP entries each made it grow for ever.
    #[test]
    fn the_file_stays_bounded_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-activity.jsonl");
        let mut at = 0;
        for _ in 0..5 {
            let log = ActivityLog::persisted(path.clone());
            for _ in 0..(CAP * 3 / 4) {
                log.push(entry(at, "pause"));
                at += 1;
            }
            log.flush();
        }
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= 2 * CAP, "the file grew to {lines} lines over 5 runs");
        let again = ActivityLog::persisted(path);
        again.flush();
        assert_eq!(again.recent(1)[0].at, at - 1);
        assert_eq!(again.len(), CAP);
    }

    #[test]
    fn a_line_cut_off_by_a_crash_is_skipped_not_the_whole_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-activity.jsonl");
        let good = serde_json::to_string(&Line::of(&entry(1, "pause"))).unwrap();
        std::fs::write(&path, format!("{good}\n{{\"at\":2,\"cli")).unwrap();
        let log = ActivityLog::persisted(path.clone());
        log.push(entry(3, "resume"));
        log.flush();
        assert_eq!(log.recent(10), [entry(3, "resume"), entry(1, "pause")]);
        drop(log);
        let again = ActivityLog::persisted(path);
        again.flush();
        assert_eq!(again.recent(10), [entry(3, "resume"), entry(1, "pause")], "the entry after the cut is not lost to it");
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
        let r = Request::SyncJoin(SyncJoinParams { key_or_link: "seedwell://sync/SWW-SECRETSECRET?name=x".into(), path: Some("/tmp/x".into()), on_demand: false });
        let d = describe(&r);
        assert!(!d.contains("SECRET") && d.contains("/tmp/x"), "{d}");
        let r = Request::SyncRename(SyncRenameParams { folder: "Docs".into(), name: "SWR-SECRETSECRET".into() });
        assert!(!describe(&r).contains("SECRET"), "{}", describe(&r));
    }
}
