//! The tracker log viewer (SPEC 8.7: Settings › Advanced › Developer › View Log…) as a model every host draws
//! (docs/CROSS-PLATFORM.md §1): a live tail of `trackers.log` with its filters (text, event, errors only), Live, the
//! rows a click opens, and every word. The log is written by the engine with every secret already masked.
//!
//! The host keeps one [`TrackerLog`] while the app runs (its filters survive closing the sheet), draws
//! [`TrackerLog::view`], hands it each change, and while the sheet is open reads the tail again
//! ([`TrackerLog::read`], off its UI thread) every `refresh_ms` while `reads` says so.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::types::{TrackerLogQuery, TrackerLogRow};
use crate::Core;

/// How often the open viewer reads the tail again while it follows the log.
const REFRESH_MS: u32 = 1_000;
/// The newest lines shown.
const LIMIT: u32 = 400;
/// The event filter's choices: the stored value and its title ("" = any event).
const EVENTS: [(&str, &str); 5] = [("", "All events"), ("started", "Started"), ("regular", "Regular"), ("completed", "Completed"), ("stopped", "Stopped")];

/// A choice of the event filter.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerLogEvent {
    pub value: String,
    pub title: String,
    /// A separator follows it in the menu.
    pub separator_after: bool,
}

/// One line of the log: time, torrent, tracker host and a pill (transport and event), what happened under it, and,
/// while open, the request, the answer and the schedule (monospaced, selectable).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerLogLine {
    /// Stable while the tail moves on (a row keeps its open state).
    pub id: String,
    /// "14:32:07", local time.
    pub time: String,
    pub title: String,
    pub host: String,
    /// "HTTPS", or empty.
    pub transport: String,
    /// The kind of line: `announce`, `scrape`, `decision`, `gate`, `quit`.
    pub kind: String,
    /// The event, or the kind of line ("decision").
    pub label: String,
    /// One line: what happened (red for a failed exchange).
    pub summary: String,
    /// The request, the answer and the schedule: `Some` while the row is open.
    pub detail: Option<String>,
    pub error: bool,
    /// The row's help: what a click does.
    pub help: String,
    pub accessibility_label: String,
    pub accessibility_hint: String,
}

/// Nothing to show yet.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerLogEmpty {
    pub title: String,
    pub text: String,
}

/// The viewer, top to bottom: the header with Done, the filters and Live, the lines (newest last), the footer.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerLogPage {
    pub title: String,
    pub subtitle: String,
    pub done_title: String,
    pub done_help: String,
    /// The search field: its text, prompt, and the button that empties it (shown while there is text).
    pub text: String,
    pub text_prompt: String,
    pub clear_text_name: String,
    pub clear_text_help: String,
    pub event_name: String,
    /// The event chosen (a `value` of `events`).
    pub event: String,
    pub events: Vec<TrackerLogEvent>,
    pub errors_only_title: String,
    pub errors_only: bool,
    pub live_title: String,
    /// Following the log: the list keeps its newest line in view.
    pub live: bool,
    pub live_help: String,
    pub rows: Vec<TrackerLogLine>,
    /// `Some` while there are no lines.
    pub empty: Option<TrackerLogEmpty>,
    /// "12 entries".
    pub count_line: String,
    /// Lines the writer dropped (it could not keep up): a warning.
    pub dropped_line: Option<String>,
    pub reveal_title: String,
    pub reveal_help: String,
    pub clear_title: String,
    pub clear_help: String,
    /// While the viewer is open: read the tail again ([`TrackerLog::read`]) every `refresh_ms` while `reads`.
    pub refresh_ms: u32,
    pub reads: bool,
}

/// The viewer's filters and the lines last read for them.
struct State {
    text: String,
    event: String,
    errors_only: bool,
    live: bool,
    open: HashSet<String>,
    rows: Vec<(String, TrackerLogRow)>,
    dropped: u64,
}

/// The tracker log viewer of the app (one while it runs).
#[derive(uniffi::Object)]
pub struct TrackerLog {
    core: Arc<Core>,
    state: Mutex<State>,
}

/// "14:32:07" of a Unix time in milliseconds, in this computer's time zone.
fn clock(ts_ms: u64) -> String {
    let s = i64::try_from(ts_ms / 1000).unwrap_or(i64::MAX);
    let day = (s + i64::from(seedwell_engine::util::local_utc_offset(s))).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day / 60 % 60, day % 60)
}

/// Each line with an id that stays while the tail moves on: what it is, numbered among its equals.
fn with_ids(rows: Vec<TrackerLogRow>) -> Vec<(String, TrackerLogRow)> {
    let mut seen: HashMap<String, u32> = HashMap::new();
    rows.into_iter()
        .map(|r| {
            let base = format!("{}|{}|{}|{}|{}", r.ts_ms, r.torrent, r.kind, r.event, r.host);
            let n = seen.entry(base.clone()).or_insert(0);
            let id = format!("{base}|{n}");
            *n += 1;
            (id, r)
        })
        .collect()
}

fn line(id: &str, r: &TrackerLogRow, open: bool) -> TrackerLogLine {
    let title = if r.title.is_empty() { r.id.clone() } else { r.title.clone() };
    let label = if r.event.is_empty() { r.kind.clone() } else { r.event.clone() };
    TrackerLogLine {
        id: id.into(),
        time: clock(r.ts_ms),
        accessibility_label: format!("{title}, {}, {label}", r.host),
        title,
        host: r.host.clone(),
        transport: r.transport.clone(),
        kind: r.kind.clone(),
        label,
        summary: r.summary.clone(),
        detail: open.then(|| r.detail.clone()),
        error: r.error,
        help: if open { "Hide the request and the answer" } else { "Show the request and the answer" }.into(),
        accessibility_hint: if open { "Hides the request and the answer" } else { "Shows the request and the answer" }.into(),
    }
}

impl TrackerLog {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn query(st: &State) -> TrackerLogQuery {
        TrackerLogQuery { text: st.text.trim().to_string(), tracker: String::new(), event: st.event.clone(), errors_only: st.errors_only, limit: LIMIT }
    }

    /// Reads the tail for the filters as they are; kept unless the filters changed meanwhile (their own read follows).
    pub(crate) fn read_now(&self) {
        let q = Self::query(&self.lock());
        let rows = self.core.tracker_log(q.clone());
        let dropped = self.core.tracker_log_dropped();
        let mut st = self.lock();
        let now = Self::query(&st);
        if (now.text, now.event, now.errors_only) != (q.text, q.event, q.errors_only) {
            return;
        }
        st.rows = with_ids(rows);
        st.dropped = dropped;
    }

    /// Deletes the log and shows nothing.
    pub(crate) fn clear_now(&self) {
        {
            let mut st = self.lock();
            st.rows.clear();
            st.open.clear();
        }
        self.core.tracker_log_clear();
        self.read_now();
    }
}

#[uniffi::export]
impl TrackerLog {
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        let state = State {
            text: String::new(),
            event: String::new(),
            errors_only: false,
            live: true,
            open: HashSet::new(),
            rows: Vec::new(),
            dropped: 0,
        };
        Arc::new(TrackerLog { core, state: Mutex::new(state) })
    }

    /// The viewer as it is now (no reading: cheap).
    pub fn view(&self) -> TrackerLogPage {
        let st = self.lock();
        let count = st.rows.len();
        TrackerLogPage {
            title: "Tracker Log".into(),
            subtitle: "Every tracker request and answer, newest last. Passkeys and the client key are masked in the file itself.".into(),
            done_title: "Done".into(),
            done_help: "Close the tracker log".into(),
            text: st.text.clone(),
            text_prompt: "Torrent or tracker".into(),
            clear_text_name: "Clear filter".into(),
            clear_text_help: "Clear the search text".into(),
            event_name: "Event".into(),
            event: st.event.clone(),
            events: EVENTS
                .iter()
                .map(|(value, title)| TrackerLogEvent { value: (*value).into(), title: (*title).into(), separator_after: value.is_empty() })
                .collect(),
            errors_only_title: "Errors only".into(),
            errors_only: st.errors_only,
            live_title: "Live".into(),
            live: st.live,
            live_help: if st.live { "Following the log. Click to pause." } else { "Paused. Click to follow the log again." }.into(),
            rows: st.rows.iter().map(|(id, r)| line(id, r, st.open.contains(id))).collect(),
            empty: (count == 0).then(|| TrackerLogEmpty {
                title: "Nothing logged yet".into(),
                text: "Entries appear here as torrents announce to their trackers.".into(),
            }),
            count_line: format!("{count} {}", if count == 1 { "entry" } else { "entries" }),
            dropped_line: (st.dropped > 0).then(|| format!("{} dropped: the log could not keep up", st.dropped)),
            reveal_title: format!("Show in {}", seedwell_platform::file_manager!()),
            reveal_help: format!("Reveal the tracker log files in {}", seedwell_platform::file_manager!()),
            clear_title: "Clear Log".into(),
            clear_help: "Delete all recorded tracker log entries".into(),
            refresh_ms: REFRESH_MS,
            reads: st.live || st.rows.is_empty(),
        }
    }

    /// The search field's text as the user types (then read again).
    pub fn set_text(&self, text: String) {
        self.lock().text = text;
    }

    /// The event filter (a `value` of `events`; then read again).
    pub fn set_event(&self, event: String) {
        self.lock().event = event;
    }

    pub fn set_errors_only(&self, on: bool) {
        self.lock().errors_only = on;
    }

    /// Live: follow the log, or keep the lines as they are.
    pub fn set_live(&self, on: bool) {
        self.lock().live = on;
    }

    /// A click on a line: opens or closes it.
    pub fn toggle(&self, id: String) {
        let mut st = self.lock();
        if !st.open.remove(&id) {
            st.open.insert(id);
        }
    }

    /// Opens exactly these lines (QA; an empty list closes every line).
    pub fn set_open(&self, ids: Vec<String>) {
        self.lock().open = ids.into_iter().collect();
    }

    /// Reads the tail again for the filters as they are, on a thread of its own; then `view` has it.
    #[uniffi::method(name = "read")]
    pub async fn read_async(self: Arc<Self>) {
        crate::calls::apart(move || self.read_now()).await
    }

    /// Clear Log: deletes the log (after the engine's commands given before it), then reads the tail again.
    #[uniffi::method(name = "clear")]
    pub async fn clear_async(self: Arc<Self>) {
        let core = self.core.clone();
        core.in_order(move |_| self.clear_now()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreOptions;

    fn entry(ts: u64, event: &str, error: bool) -> String {
        let resp = if error {
            r#","response":{"fields":{},"error":"timeout: no answer in time"}"#.to_string()
        } else {
            r#","response":{"status":200,"fields":{"interval":"1800","complete":"4","incomplete":"2","peers":"6"}}"#.to_string()
        };
        format!(
            r#"{{"v":1,"ts_ms":{ts},"kind":"announce","torrent":"aabbccddeeff00112233445566778899aabbccdd","id":"aabbccdd","title":"Ubuntu 24.04","tracker":"https://t.example/announce?passkey=••••••","host":"t.example","transport":"https","event":"{event}","request":{{"url":"https://t.example/announce?passkey=••••••","headers":[],"fields":{{}}}}{resp},"duration_ms":143}}"#
        )
    }

    fn viewer(lines: &[String]) -> (Arc<TrackerLog>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/trackers.log"), lines.join("\n") + "\n").unwrap();
        let options =
            CoreOptions { config_dir: dir.path().display().to_string(), mock: true, allow_loopback_trackers: false, forbidden_root: None };
        (TrackerLog::new(Core::new(options).unwrap()), dir)
    }

    fn append(dir: &tempfile::TempDir, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join("logs/trackers.log")).unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn before_a_read_it_says_nothing_is_logged_and_asks_to_be_read() {
        let (log, _dir) = viewer(&[]);
        let v = log.view();
        assert_eq!((v.title.as_str(), v.done_title.as_str(), v.text_prompt.as_str()), ("Tracker Log", "Done", "Torrent or tracker"));
        assert_eq!(v.empty.map(|e| e.title), Some("Nothing logged yet".into()));
        assert_eq!((v.count_line.as_str(), v.refresh_ms, v.reads, v.live), ("0 entries", 1000, true, true));
        let events: Vec<_> = v.events.iter().map(|e| (e.value.as_str(), e.title.as_str(), e.separator_after)).collect();
        assert_eq!(events[..2], [("", "All events", true), ("started", "Started", false)]);
        assert_eq!(v.reveal_title, format!("Show in {}", seedwell_platform::file_manager!()));
    }

    #[test]
    fn lines_keep_their_ids_and_open_state_as_the_tail_moves_on_and_the_filters_apply() {
        let (log, dir) = viewer(&[entry(1_000, "started", false)]);
        log.read_now();
        let v = log.view();
        assert_eq!((v.rows.len(), v.count_line.as_str(), v.empty.is_none()), (1, "1 entry", true));
        let first = v.rows[0].clone();
        assert_eq!((first.title.as_str(), first.host.as_str(), first.transport.as_str(), first.label.as_str()), ("Ubuntu 24.04", "t.example", "HTTPS", "started"));
        assert_eq!(first.time.len(), 8, "{}", first.time);
        assert!(first.detail.is_none() && first.help.starts_with("Show"));
        log.toggle(first.id.clone());
        assert!(log.view().rows[0].detail.as_deref().is_some_and(|d| d.contains("t.example")));
        // A new line: the first keeps its id and stays open.
        append(&dir, &entry(2_000, "regular", true));
        log.read_now();
        let v = log.view();
        assert_eq!(v.rows.len(), 2);
        assert_eq!(v.rows[0].id, first.id);
        assert!(v.rows[0].detail.is_some() && v.rows[0].help.starts_with("Hide"));
        assert!(v.rows[1].error && v.rows[1].summary.contains("timeout"));
        // Errors only; then paused, it does not read by itself any more.
        log.set_errors_only(true);
        log.read_now();
        let v = log.view();
        assert_eq!((v.rows.len(), v.errors_only), (1, true));
        log.set_live(false);
        let v = log.view();
        assert!(!v.reads && v.live_help.starts_with("Paused"));
        log.set_text("debian".into());
        log.read_now();
        assert!(log.view().reads, "nothing to show: it reads");
        log.clear_now();
        assert!(log.view().rows.is_empty());
    }
}
