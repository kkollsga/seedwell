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
        Request::Reannounce(p) => format!("reannounce {}", ids(&p.ids)),
        Request::Recheck(p) => format!("recheck {}", ids(&p.ids)),
        Request::Remove(p) => format!("remove {}{} (step 1, asks for confirmation)", ids(&p.ids), if p.delete_files { " and delete files" } else { "" }),
        Request::RemoveConfirm(_) => "confirm removal".into(),
        Request::SettingsSet(p) => format!("change settings: {}", p.changes.keys().map(|k| clip(k, 64)).collect::<Vec<_>>().join(", ")),
        other => other.method().name().to_owned(),
    };
    clip(&s, 400)
}

fn short(id: &str) -> String {
    clip(id, 8)
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
    }
}
