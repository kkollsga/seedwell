//! Running a parsed [`Cmd`]: talk to the control socket, print text (default) or JSON (`--json`).
//!
//! Every JSON object carries `"v":1`; errors are `{"v":1,"error":{"code","message","exit"}}` on stderr.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use trs_control::client::{Client, ClientError};
use trs_control::proto::*;
use trs_control::types::*;

use crate::args::Usage;
use crate::cmd::{AddArgs, Cmd, DevAction, GoalArgs, ListArgs, LogArgs, StartMode};
use crate::select::{SelectError, Selection, match_term, need_one};
use crate::{debug, render, sim, skill, sync};

/// The version of the JSON shapes. Fields are added, never changed or removed, within a version.
pub const JSON_VERSION: u32 = 1;

/// The bundle id of the app, for `--launch`.
pub const APP_BUNDLE_ID: &str = "io.github.kkollsga.Seedwell";

/// Options every command shares.
#[derive(Clone, Debug)]
pub struct Global {
    /// Print JSON.
    pub json: bool,
    /// Socket to connect to.
    pub socket: PathBuf,
    /// The socket was named with `--socket` (the daemon otherwise puts it in its state dir).
    pub socket_given: bool,
    /// Name announced in `hello` (shows up in the activity log).
    pub client: String,
    /// Start the app when it is not running.
    pub launch: bool,
    /// `--yes-i-know`: allow `sim` without Developer mode.
    pub yes_i_know: bool,
}

/// A failure with its exit code.
#[derive(Debug)]
pub struct CliError {
    /// Process exit code: 1 error, 2 usage, 3 not found/ambiguous, 4 denied, 5 not running, 6 timed out.
    pub code: i32,
    /// Message for the user.
    pub message: String,
    /// Protocol error code, for `--json`.
    pub api_code: Option<ErrorCode>,
    /// Structured detail for `--json` (candidates of an ambiguous selector).
    pub data: Option<Value>,
}

impl CliError {
    /// An error with a plain message.
    pub fn plain(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), api_code: None, data: None }
    }
}

impl From<Usage> for CliError {
    fn from(u: Usage) -> Self {
        Self { code: 2, message: u.0, api_code: Some(ErrorCode::InvalidParams), data: None }
    }
}

impl From<SelectError> for CliError {
    fn from(e: SelectError) -> Self {
        let api_code = Some(match e {
            SelectError::NotFound(_) => ErrorCode::NotFound,
            SelectError::Ambiguous { .. } => ErrorCode::Ambiguous,
        });
        Self { code: 3, message: e.message(), api_code, data: e.data() }
    }
}

impl From<ClientError> for CliError {
    fn from(e: ClientError) -> Self {
        let message = e.to_string();
        match e {
            ClientError::NotRunning { .. } => Self { code: 5, message, api_code: None, data: None },
            ClientError::Api(a) => {
                let code = match a.code {
                    ErrorCode::AgentAccessOff | ErrorCode::PermissionDenied => 4,
                    ErrorCode::InvalidParams => 2,
                    ErrorCode::NotFound | ErrorCode::Ambiguous => 3,
                    ErrorCode::Timeout => 6,
                    _ => 1,
                };
                let message = if a.code == ErrorCode::AgentAccessOff {
                    format!("{message} (the user can switch it on in Seedwell > Settings > Agents)")
                } else {
                    message
                };
                Self { code, message, api_code: Some(a.code), data: a.data }
            }
            _ => Self { code: 1, message, api_code: None, data: None },
        }
    }
}

/// Result of a command.
pub type R<T = ()> = Result<T, CliError>;

/// Start the installed app in the background (`open -g`), for `--launch`.
fn launch_app(g: &Global) -> R {
    if g.socket_given {
        return Err(Usage("--launch starts the installed Seedwell app; it cannot be combined with --socket".into()).into());
    }
    if !cfg!(target_os = "macos") {
        return Err(CliError::plain(5, "--launch works on macOS only; start Seedwell or `seedwell daemon` yourself"));
    }
    let st = std::process::Command::new("open").args(["-g", "-b", APP_BUNDLE_ID]).status();
    match st {
        Ok(s) if s.success() => Ok(()),
        _ => Err(CliError::plain(5, "Seedwell is not running and could not be started (is the app installed?)")),
    }
}

/// Connect to the control socket; with `--launch` start the app first when nothing answers.
pub async fn connect(g: &Global) -> R<Client> {
    match Client::connect(&g.socket, &g.client).await {
        Err(ClientError::NotRunning { .. }) if g.launch => {
            eprintln!("seedwell: starting Seedwell in the background...");
            launch_app(g)?;
            let deadline = Instant::now() + Duration::from_secs(40);
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                match Client::connect(&g.socket, &g.client).await {
                    Ok(c) => return Ok(c),
                    Err(ClientError::NotRunning { .. }) if Instant::now() < deadline => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(ClientError::NotRunning { path, source }) => {
            let e = ClientError::NotRunning { path, source };
            let mut err = CliError::from(e);
            err.message.push_str(" Use --launch to start the app.");
            Err(err)
        }
        other => Ok(other?),
    }
}

/// Call and decode, keeping the raw JSON for `--json`.
pub async fn ask<T: DeserializeOwned>(c: &mut Client, req: Request) -> R<(Value, T)> {
    let v = c.call(&req).await?;
    let t = serde_json::from_value(v.clone()).map_err(|e| CliError::plain(1, format!("unexpected answer: {e}")))?;
    Ok((v, t))
}

/// The JSON text of an answer with `"v":1` as the first key (serde_json sorts keys, so it is spliced in).
pub fn json_text(v: &Value, pretty: bool) -> String {
    let mut v = v.clone();
    if let Value::Object(o) = &mut v {
        o.remove("v");
        let empty = o.is_empty();
        let body = if pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) }.unwrap_or_default();
        return match (pretty, empty) {
            (_, true) => format!("{{\"v\":{JSON_VERSION}}}"),
            (true, false) => format!("{{\n  \"v\": {JSON_VERSION},{}", &body[1..]),
            (false, false) => format!("{{\"v\":{JSON_VERSION},{}", &body[1..]),
        };
    }
    if pretty { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) }.unwrap_or_default()
}

/// Print `raw` as JSON or the human text.
pub fn show(g: &Global, raw: &Value, human: impl FnOnce() -> String) {
    if g.json {
        println!("{}", json_text(raw, true));
    } else {
        println!("{}", human());
    }
}

/// Print one JSON object per line (streams).
pub fn show_line(g: &Global, raw: &Value, human: impl FnOnce() -> String) {
    if g.json {
        println!("{}", json_text(raw, false));
    } else {
        println!("{}", human());
    }
    let _ = std::io::stdout().flush();
}

/// Print an error the way `main` does.
pub fn print_error(g: &Global, e: &CliError) {
    report(g, e);
}

fn report(g: &Global, e: &CliError) {
    if e.message.is_empty() {
        return;
    }
    if g.json {
        let mut err = json!({ "code": e.api_code.map(|c| c.as_str()), "message": e.message, "exit": e.code });
        if let (Some(d), Some(o)) = (&e.data, err.as_object_mut()) {
            if let Some(dm) = d.as_object() {
                for (k, v) in dm {
                    o.insert(k.clone(), v.clone());
                }
            } else {
                o.insert("data".into(), d.clone());
            }
        }
        eprintln!("{}", json_text(&json!({ "error": err }), false));
    } else {
        eprintln!("seedwell: {}", e.message);
    }
}

// ----------------------------------------------------------------------------- selection

/// Every row matching `p`, paged.
pub async fn fetch_rows(c: &mut Client, mut p: ListParams) -> R<Vec<TransferDto>> {
    let mut out: Vec<TransferDto> = Vec::new();
    loop {
        p.limit = Some(500);
        p.offset = Some(out.len() as u32);
        let (_, l): (_, ListDto) = ask(c, Request::List(p.clone())).await?;
        let n = l.transfers.len();
        out.extend(l.transfers);
        if n == 0 || out.len() as u32 >= l.total {
            return Ok(out);
        }
    }
}

/// The torrents a selection names. Terms resolve one torrent each; filters may match many.
pub async fn resolve(c: &mut Client, sel: &Selection) -> R<Vec<TransferDto>> {
    if !sel.terms.is_empty() {
        let rows = fetch_rows(c, ListParams::default()).await?;
        let mut out: Vec<TransferDto> = Vec::new();
        for t in &sel.terms {
            let r = match_term(&rows, t)?;
            if !out.iter().any(|o| o.id == r.id) {
                out.push(r.clone());
            }
        }
        return Ok(out);
    }
    let p = ListParams { filter: sel.list_filter()?, tracker: sel.tracker.clone(), category: sel.category.clone(), ..Default::default() };
    let rows = fetch_rows(c, p).await?;
    if rows.is_empty() {
        return Err(SelectError::NotFound("no torrents match that filter (see `seedwell list`)".into()).into());
    }
    Ok(rows)
}

/// Exactly one torrent.
pub async fn resolve_one(c: &mut Client, sel: &Selection, what: &str) -> R<TransferDto> {
    let rows = resolve(c, sel).await?;
    need_one(&rows, what)?;
    rows.into_iter().next().ok_or_else(|| CliError::plain(3, "no such torrent"))
}

fn ids(rows: &[TransferDto]) -> Vec<String> {
    rows.iter().map(|r| r.id.clone()).collect()
}

/// An action over many ids, in chunks of the protocol's limit, answered as one `ActionDto`.
async fn action_on(g: &Global, c: &mut Client, rows: &[TransferDto], make: impl Fn(Vec<String>) -> Request) -> R {
    let mut total = ActionDto::default();
    for chunk in rows.chunks(MAX_IDS) {
        let (_, a): (_, ActionDto) = ask(c, make(ids(chunk))).await?;
        total.action = a.action;
        total.count += a.count;
        total.transfers.extend(a.transfers);
    }
    show(g, &serde_json::to_value(&total).unwrap_or_default(), || render::action(&total));
    Ok(())
}

// ----------------------------------------------------------------------------- run

/// Run one command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    match cmd {
        Cmd::Status => {
            let mut c = connect(g).await?;
            let (raw, s): (_, StatusDto) = ask(&mut c, Request::Status(Empty {})).await?;
            show(g, &raw, || render::status(&s));
        }
        Cmd::List(a) => list(g, a).await?,
        Cmd::Info { sel, files, peers, trackers } => {
            let mut c = connect(g).await?;
            let row = resolve_one(&mut c, &sel, "info").await?;
            let (raw, d): (_, DetailDto) = ask(
                &mut c,
                Request::Info(InfoParams { id: row.id, files: Some(files), trackers: Some(trackers), peers: Some(peers), max_files: None, max_peers: None }),
            )
            .await?;
            show(g, &raw, || render::info(&d));
        }
        Cmd::Add(a) => return add(g, a).await,
        Cmd::Start { sel, mode } => start(g, sel, mode).await?,
        Cmd::Cancel { sel, keep_file } => cancel(g, sel, keep_file).await?,
        Cmd::Choice { sel } => {
            let mut c = connect(g).await?;
            let row = resolve_one(&mut c, &sel, "choice").await?;
            let (raw, ch): (_, ChoiceDto) = ask(&mut c, Request::Choice(ChoiceParams { id: row.id })).await?;
            show(g, &raw, || render::choice(&ch));
        }
        Cmd::Pause { sel } => pause_resume(g, sel, true).await?,
        Cmd::Resume { sel } => pause_resume(g, sel, false).await?,
        Cmd::Top { sel } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            action_on(g, &mut c, &rows, |ids| Request::MoveToTop(IdsParams { ids })).await?;
        }
        Cmd::Priority { sel, level } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            action_on(g, &mut c, &rows, |ids| Request::SetPriority(SetPriorityParams { ids, priority: level })).await?;
        }
        Cmd::Category { sel, category } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            action_on(g, &mut c, &rows, |ids| Request::SetCategory(SetCategoryParams { ids, category: category.clone() })).await?;
        }
        Cmd::Files { sel, set, only } => files(g, sel, set, only).await?,
        Cmd::Goal { sel, goal } => goal_cmd(g, sel, goal).await?,
        Cmd::Reannounce { sel, force, wait } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            reannounce(g, &mut c, &rows, None, force, wait).await?;
        }
        Cmd::Recheck { sel } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            action_on(g, &mut c, &rows, |ids| Request::Recheck(IdsParams { ids })).await?;
        }
        Cmd::Remove { sel, delete_files, yes } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            if rows.len() > MAX_IDS {
                return Err(Usage(format!("at most {MAX_IDS} torrents per removal; narrow the selection")).into());
            }
            remove(g, c, ids(&rows), delete_files, yes).await?;
        }
        Cmd::RemoveConfirm { token } => {
            let mut c = connect(g).await?;
            let (raw, r): (_, RemovedDto) = ask(&mut c, Request::RemoveConfirm(RemoveConfirmParams { token })).await?;
            show(g, &raw, || render::removed(&r));
        }
        Cmd::Moves => {
            let mut c = connect(g).await?;
            let (raw, m): (_, MoveProposalDto) = ask(&mut c, Request::LocationMoves(Empty {})).await?;
            show(g, &raw, || render::proposal(&m));
        }
        Cmd::Move { sel, path } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            let (raw, m): (_, MoveReportDto) = ask(&mut c, Request::Move(MoveParams { ids: ids(&rows), path })).await?;
            show(g, &raw, || render::move_report(&m));
        }
        Cmd::CancelMove { sel } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            action_on(g, &mut c, &rows, |ids| Request::CancelMove(IdsParams { ids })).await?;
        }
        Cmd::Stream { sel, file } => {
            let mut c = connect(g).await?;
            let row = resolve_one(&mut c, &sel, "stream").await?;
            let (raw, i): (_, StreamInfoDto) = ask(&mut c, Request::StreamUrl(StreamUrlParams { id: row.id, file })).await?;
            show(g, &raw, || match &i.url {
                Some(u) => u.clone(),
                None => format!("{} is complete: open the file itself", i.file_name),
            });
        }
        Cmd::StopStream { token } => {
            let mut c = connect(g).await?;
            let v = c.call(&Request::StopStream(StopStreamParams { token })).await?;
            show(g, &v, || if v["closed"].as_bool() == Some(true) { "stream stopped".into() } else { "no such stream".into() });
        }
        Cmd::Limits(p) => {
            let mut c = connect(g).await?;
            let (raw, l): (Value, LimitsDto) = if p == SetLimitsParams::default() {
                let (r, s): (_, StatusDto) = ask(&mut c, Request::Status(Empty {})).await?;
                (r["limits"].clone(), s.limits)
            } else {
                let (r, x): (_, LimitsDto) = ask(&mut c, Request::SetLimits(p)).await?;
                (r, x)
            };
            show(g, &raw, || render::limits(&l));
        }
        Cmd::Trackers => {
            let mut c = connect(g).await?;
            let (raw, t): (_, TrackersDto) = ask(&mut c, Request::Trackers(Empty {})).await?;
            show(g, &raw, || render::trackers(&t));
        }
        Cmd::TrackerLimits { name, down, up } => tracker_limits(g, name, down, up).await?,
        Cmd::TrackerPex { name, set } => tracker_pex(g, name, set).await?,
        Cmd::Stats { tracker, days } => {
            let mut c = connect(g).await?;
            let (raw, s): (_, TrackerStatsDto) = ask(&mut c, Request::TrackerStats(TrackerStatsParams { tracker, days })).await?;
            show(g, &raw, || render::stats(&s));
        }
        Cmd::Rules => {
            let mut c = connect(g).await?;
            let v = c.call(&Request::Rules(Empty {})).await?;
            show(g, &v, || render_rules(&v));
        }
        Cmd::SettingsGet { keys } => {
            let mut c = connect(g).await?;
            let (raw, s): (_, SettingsDto) = ask(&mut c, Request::SettingsGet(SettingsGetParams { keys: (!keys.is_empty()).then_some(keys) })).await?;
            show(g, &raw, || s.settings.iter().map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("\n"));
        }
        Cmd::SettingsSet { pairs } => {
            let mut c = connect(g).await?;
            let r = settings_set(&mut c, pairs).await?;
            show(g, &r.0, || format!("changed: {}\n{}", r.1.changed.join(", "), r.1.note));
        }
        Cmd::Wait { sel, any, timeout } => wait(g, sel, any, timeout).await?,
        Cmd::Watch { kinds, count, for_secs } => watch(g, kinds, count, for_secs).await?,
        Cmd::Activity { limit } => {
            let mut c = connect(g).await?;
            let v = c.call(&Request::Activity(ActivityParams { limit })).await?;
            let entries: Vec<ActivityDto> = serde_json::from_value(v["entries"].clone()).unwrap_or_default();
            show(g, &v, || render::activity(&entries));
        }
        Cmd::LogTrackers(a) => log_trackers(g, a).await?,
        Cmd::Dev(a) => dev(g, a).await?,
        Cmd::Debug(d) => debug::run(d, g).await?,
        Cmd::Sim(s) => sim::run(s, g).await?,
        Cmd::Skill(s) => skill::run(s, g)?,
        Cmd::Sync(s) => sync::run(s, g).await?,
        Cmd::Relay(a) => sync::relay(a).await?,
        Cmd::Help(topic) => crate::print_help(topic.as_deref())?,
        Cmd::Daemon(_) => unreachable!("handled in lib.rs"),
    }
    Ok(())
}

/// `settings set` with values as JSON where they parse as JSON.
pub async fn settings_set(c: &mut Client, pairs: Vec<(String, String)>) -> R<(Value, SettingsDto)> {
    let mut changes = serde_json::Map::new();
    for (k, v) in pairs {
        // Numbers, booleans and quoted JSON pass through; anything else is a string.
        let val = serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v));
        changes.insert(k, val);
    }
    ask(c, Request::SettingsSet(SettingsSetParams { changes })).await
}

async fn list(g: &Global, a: ListArgs) -> R {
    let mut c = connect(g).await?;
    let (raw, l): (_, ListDto) = ask(
        &mut c,
        Request::List(ListParams { filter: a.filter, tracker: a.tracker, category: a.category, text: a.text, kind: a.kind, limit: a.limit, offset: a.offset }),
    )
    .await?;
    show(g, &raw, || render::list(&l));
    Ok(())
}

async fn add(g: &Global, a: AddArgs) -> R {
    let mut c = connect(g).await?;
    let mut last = Ok(());
    for src in a.sources {
        let req = Request::Add(AddParams {
            source: Some(src),
            save_path: a.save_path.clone(),
            paused: a.paused,
            category: a.category.clone(),
            priority: a.priority,
            rule: a.rule.clone(),
            queue_top: a.queue_top,
            now: a.now,
            sha256: a.sha256.clone(),
            peers: a.peers.clone(),
            ..Default::default()
        });
        match ask::<AddedDto>(&mut c, req).await {
            Ok((raw, added)) => show(g, &raw, || render::added(&added)),
            Err(e) => {
                report(g, &e);
                last = Err(e);
            }
        }
    }
    last.map_err(|e| CliError { message: String::new(), ..e })
}

const HELD: [&str; 2] = ["awaiting-choice", "pending"];

async fn start(g: &Global, sel: Selection, mode: StartMode) -> R {
    let mut c = connect(g).await?;
    let rows = resolve(&mut c, &sel).await?;
    let mut total = ActionDto::default();
    for r in &rows {
        if matches!(mode, StartMode::All | StartMode::Suggested) && !HELD.contains(&r.state.as_str()) {
            return Err(CliError::plain(1, format!("{} is not waiting to start (it is {}); `seedwell start` confirms a held torrent", r.name, r.state)));
        }
        let p = match &mode {
            // Start in the app begins with every file ticked; `skip` with nothing listed is exactly that.
            StartMode::All => ChooseFilesParams { id: r.id.clone(), skip: Some(Vec::new()), ..Default::default() },
            StartMode::Suggested => ChooseFilesParams { id: r.id.clone(), accept_default: true, ..Default::default() },
            StartMode::Files(f) => ChooseFilesParams { id: r.id.clone(), files: Some(f.clone()), ..Default::default() },
            StartMode::Skip(s) => ChooseFilesParams { id: r.id.clone(), skip: Some(s.clone()), ..Default::default() },
        };
        let (_, a): (_, ActionDto) = ask(&mut c, Request::ChooseFiles(p)).await?;
        total.action = a.action;
        total.count += a.count;
        total.transfers.extend(a.transfers);
    }
    show(g, &serde_json::to_value(&total).unwrap_or_default(), || render::action(&total));
    Ok(())
}

async fn cancel(g: &Global, sel: Selection, keep_file: bool) -> R {
    let mut c = connect(g).await?;
    let rows = resolve(&mut c, &sel).await?;
    let mut docs = Vec::new();
    let mut texts = Vec::new();
    for r in &rows {
        if !HELD.contains(&r.state.as_str()) {
            return Err(CliError::plain(1, format!("{} is not held (it is {}); `seedwell remove` removes a running torrent", r.name, r.state)));
        }
        let v = c.call(&Request::CancelChoice(CancelChoiceParams { id: r.id.clone(), keep_file })).await?;
        texts.push(render::cancelled(&v));
        docs.push(v);
    }
    if g.json {
        let v = if docs.len() == 1 { docs.remove(0) } else { json!({ "action": "cancelled", "count": docs.len(), "results": docs }) };
        show(g, &v, String::new);
    } else {
        println!("{}", texts.join("\n"));
    }
    Ok(())
}

async fn pause_resume(g: &Global, sel: Selection, pause: bool) -> R {
    sel.validate(if pause { "pause" } else { "resume" })?;
    let mut c = connect(g).await?;
    if sel.is_plain_all() {
        let t = Targets { ids: Vec::new(), all: true };
        let v = c.call(&if pause { Request::Pause(t) } else { Request::Resume(t) }).await?;
        show(g, &v, || format!("{} all {} transfers", if pause { "paused" } else { "resumed" }, v["count"]));
        return Ok(());
    }
    let rows = resolve(&mut c, &sel).await?;
    action_on(g, &mut c, &rows, |ids| {
        let t = Targets { ids, all: false };
        if pause { Request::Pause(t) } else { Request::Resume(t) }
    })
    .await
}

async fn files(g: &Global, sel: Selection, mut set: Vec<(Vec<u32>, FilePriorityParam)>, only: Option<Vec<u32>>) -> R {
    let mut c = connect(g).await?;
    let row = resolve_one(&mut c, &sel, "files").await?;
    if set.is_empty() && only.is_none() {
        let (raw, d): (_, DetailDto) = ask(
            &mut c,
            Request::Info(InfoParams { id: row.id, files: Some(true), trackers: Some(false), peers: Some(false), max_files: None, max_peers: None }),
        )
        .await?;
        show(g, &raw, || render::files(&d));
        return Ok(());
    }
    if let Some(keep) = only {
        let (_, d): (_, DetailDto) = ask(
            &mut c,
            Request::Info(InfoParams { id: row.id.clone(), files: Some(true), trackers: Some(false), peers: Some(false), max_files: Some(20_000), max_peers: None }),
        )
        .await?;
        let skip: Vec<u32> = d.files.iter().filter(|f| !f.padding && !keep.contains(&f.index)).map(|f| f.index).collect();
        set.insert(0, (skip, FilePriorityParam::Skip));
        set.insert(1, (keep, FilePriorityParam::Normal));
    }
    let list: Vec<FileSet> = set.into_iter().flat_map(|(idx, priority)| idx.into_iter().map(move |index| FileSet { index, priority })).collect();
    let (raw, a): (_, ActionDto) = ask(&mut c, Request::SetFilePriorities(SetFilePrioritiesParams { id: row.id, priorities: None, set: Some(list) })).await?;
    show(g, &raw, || render::action(&a));
    Ok(())
}

async fn goal_cmd(g: &Global, sel: Selection, goal: GoalArgs) -> R {
    let mut c = connect(g).await?;
    let rows = resolve(&mut c, &sel).await?;
    action_on(g, &mut c, &rows, |ids| {
        Request::SetSeedGoal(SetSeedGoalParams {
            ids,
            ratio: goal.ratio,
            seed_secs: goal.time.map(|t| t.min(u64::from(u32::MAX)) as u32),
            min_seed_secs: goal.min_time.map(|t| t.min(u64::from(u32::MAX)) as u32),
            any: goal.all_conditions.then_some(false),
            at_goal: goal.at_goal,
            stop_on_finish: goal.stop_on_finish,
            forever: goal.forever,
            rule_default: goal.rule_default,
        })
    })
    .await
}

/// Ask the trackers again; `only_tracker` narrows what is printed (the engine's gate applies to the torrent's trackers together).
pub async fn reannounce(g: &Global, c: &mut Client, rows: &[TransferDto], only_tracker: Option<&str>, force: bool, wait: bool) -> R {
    let mut v = json!({ "transfers": [] });
    for chunk in rows.chunks(MAX_IDS) {
        let r = c.call(&Request::Reannounce(ReannounceParams { ids: ids(chunk), force, wait })).await?;
        if let (Some(dst), Some(src)) = (v["transfers"].as_array_mut(), r["transfers"].as_array()) {
            dst.extend(src.iter().cloned());
        }
    }
    if let Some(t) = only_tracker {
        let needle = t.to_ascii_lowercase();
        for tr in v["transfers"].as_array_mut().into_iter().flatten() {
            if let Some(a) = tr["trackers"].as_array_mut() {
                a.retain(|x| x["url"].as_str().is_some_and(|u| u.to_ascii_lowercase().contains(&needle)));
            }
        }
    }
    show(g, &v, || render::reannounce(&v));
    Ok(())
}

/// Show or set peer exchange of a private tracker: a favorite, an automatic tracker (by its domain
/// or name) or `other-private`.
async fn tracker_pex(g: &Global, name: String, set: Option<bool>) -> R {
    let mut c = connect(g).await?;
    let (_, t): (_, TrackersDto) = ask(&mut c, Request::Trackers(Empty {})).await?;
    let lower = name.to_lowercase();
    let all: Vec<(String, String, bool, bool)> = t
        .profiles
        .iter()
        .chain(&t.groups)
        .map(|p| (p.id.clone(), p.name.clone(), p.private, p.allow_pex))
        .chain((t.other_private_transfers > 0).then(|| ("other-private".to_owned(), "Other private".to_owned(), true, t.other_private_allow_pex)))
        .collect();
    let exact = all.iter().find(|p| p.0.to_lowercase() == lower || p.1.to_lowercase() == lower);
    let hit = exact.or_else(|| {
        let m: Vec<_> = all.iter().filter(|p| p.0.to_lowercase().contains(&lower) || p.1.to_lowercase().contains(&lower)).collect();
        if m.len() == 1 { Some(m[0]) } else { None }
    });
    let Some((id, _, private, allowed)) = hit else {
        let names: Vec<String> = all.iter().filter(|p| p.2).map(|p| format!("{} ({})", p.0, p.1)).collect();
        return Err(SelectError::NotFound(format!("no private tracker matches `{name}`; private trackers: {}", if names.is_empty() { "none".into() } else { names.join(", ") })).into());
    };
    if !private {
        return Err(SelectError::NotFound(format!("`{id}` is not a private tracker: peer exchange is a setting of private trackers")).into());
    }
    let v = match set {
        None => json!({ "id": id, "private": true, "allow_pex": allowed, "pex_enabled": t.pex_enabled }),
        Some(allow) => {
            let mut v = c.call(&Request::SetTrackerPex(SetTrackerPexParams { id: id.clone(), allow })).await?;
            v["pex_enabled"] = json!(t.pex_enabled);
            v
        }
    };
    show(g, &v, || render::tracker_pex(&v));
    Ok(())
}

async fn tracker_limits(g: &Global, name: String, down: Option<u64>, up: Option<u64>) -> R {
    let mut c = connect(g).await?;
    let (_, t): (_, TrackersDto) = ask(&mut c, Request::Trackers(Empty {})).await?;
    let lower = name.to_lowercase();
    let favs: Vec<&ProfileDto> = t.profiles.iter().filter(|p| p.favorite).collect();
    let hit = favs
        .iter()
        .find(|p| p.id.to_lowercase() == lower || p.name.to_lowercase() == lower)
        .or_else(|| {
            let m: Vec<&&ProfileDto> = favs.iter().filter(|p| p.id.to_lowercase().contains(&lower) || p.name.to_lowercase().contains(&lower)).collect();
            if m.len() == 1 { Some(m[0]) } else { None }
        })
        .copied();
    let Some(p) = hit else {
        let names: Vec<String> = favs.iter().map(|p| format!("{} ({})", p.id, p.name)).collect();
        return Err(SelectError::NotFound(format!(
            "no favorite tracker matches `{name}`{}",
            if names.is_empty() { "; save one first (right-click a torrent > Save as Favorite)".to_owned() } else { format!("; favorites: {}", names.join(", ")) }
        ))
        .into());
    };
    if down.is_none() && up.is_none() {
        let v = json!({ "id": p.id, "down_limit": p.down_limit, "up_limit": p.up_limit });
        show(g, &v, || render::tracker_limits(&v));
        return Ok(());
    }
    let v = c.call(&Request::SetTrackerLimits(SetTrackerLimitsParams { id: p.id.clone(), down_limit: down, up_limit: up })).await?;
    show(g, &v, || render::tracker_limits(&v));
    Ok(())
}

async fn wait(g: &Global, sel: Selection, any: bool, timeout: u32) -> R {
    let mut c = connect(g).await?;
    let rows = resolve(&mut c, &sel).await?;
    let idv = ids(&rows);
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout));
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs().clamp(1, 120) as u32;
        let req = Request::WaitFor(WaitForParams { ids: idv.clone(), mode: Some(if any { WaitMode::Any } else { WaitMode::All }), timeout_secs: Some(left) });
        let (raw, w): (_, WaitDto) = ask(&mut c, req).await?;
        if w.done || Instant::now() >= deadline {
            show(g, &raw, || render::wait(&w));
            if !w.done {
                return Err(CliError { message: "timed out".into(), api_code: Some(ErrorCode::Timeout), ..CliError::plain(6, "") });
            }
            return Ok(());
        }
    }
}

async fn remove(g: &Global, mut c: Client, ids: Vec<String>, delete_files: bool, yes: bool) -> R {
    let (raw, plan): (_, RemovePlanDto) = ask(&mut c, Request::Remove(RemoveParams { ids, delete_files })).await?;
    if !yes {
        if g.json {
            // Scripts get the plan and the token and confirm with `seedwell remove-confirm`.
            println!("{}", json_text(&raw, true));
            eprintln!("seedwell: nothing removed yet; confirm within {} s with `seedwell remove-confirm <token>` or repeat with --yes", plan.expires_in_secs);
            return Ok(());
        }
        println!("{}", render::plan(&plan));
        if !std::io::stdin().is_terminal() {
            return Err(CliError::plain(1, "not removing: no terminal to ask on; repeat with --yes to confirm"));
        }
        print!("Proceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled; nothing was removed.");
            return Ok(());
        }
    }
    let (raw, r): (_, RemovedDto) = ask(&mut c, Request::RemoveConfirm(RemoveConfirmParams { token: plan.token })).await?;
    show(g, &raw, || render::removed(&r));
    Ok(())
}

fn render_rules(v: &Value) -> String {
    let mut o = String::new();
    let d = &v["default"];
    o.push_str(&format!("default: priority {}, goal {}, on goal {}\n", d["priority"].as_str().unwrap_or("?"), d["goal"], d["on_goal"].as_str().unwrap_or("?")));
    for r in v["rule"].as_array().into_iter().flatten() {
        o.push_str(&format!(
            "{:<16} {}{}  match {}  goal {}  priority {}\n",
            r["id"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or(""),
            if r["enabled"].as_bool() == Some(false) { " (disabled)" } else { "" },
            r["match"],
            r["goal"],
            r["priority"].as_str().unwrap_or("-")
        ));
    }
    o.trim_end().to_owned()
}

async fn watch(g: &Global, kinds: Option<Vec<String>>, count: Option<u64>, for_secs: Option<u64>) -> R {
    let mut c = connect(g).await?;
    let sub = c.call(&Request::EventsSubscribe(EventsSubscribeParams { kinds })).await?;
    if !g.json {
        eprintln!("watching events (cursor {}); Ctrl-C to stop", sub["cursor"]);
    }
    let deadline = for_secs.map(|s| Instant::now() + Duration::from_secs(s));
    let mut seen = 0u64;
    loop {
        let wait = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(());
                }
                left.min(Duration::from_secs(60))
            }
            None => Duration::from_secs(60),
        };
        if let Some(e) = c.next_event(wait).await? {
            show_line(g, &serde_json::to_value(&e).unwrap_or_default(), || render::event(&e));
            seen += 1;
            if count.is_some_and(|n| seen >= n) {
                return Ok(());
            }
        }
    }
}

/// `seedwell log trackers`: the developer-mode log. Asks the running app or daemon (permission
/// `read_only`); when none runs it reads the file next to the socket.
pub async fn log_trackers(g: &Global, a: LogArgs) -> R {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let cutoff = a.since_secs.map(|s| now_ms.saturating_sub(s * 1000));
    let print = |v: &Value| {
        if cutoff.is_some_and(|c| v["ts_ms"].as_u64().unwrap_or(0) < c) {
            return;
        }
        if g.json {
            println!("{}", json_text(v, false));
        } else {
            println!("{}\n", trs_devlog::view::Row::from_value(v).detail);
        }
    };
    let query = trs_devlog::read::Query {
        torrent: a.torrent.clone(),
        tracker: a.tracker.clone(),
        event: a.event.clone(),
        errors_only: a.errors,
        text: None,
        limit: a.limit.unwrap_or(50).clamp(1, 500) as usize,
    };
    let mut c = match Client::connect(&g.socket, &g.client).await {
        Ok(c) => c,
        Err(ClientError::NotRunning { .. }) => {
            // No app or daemon: read the file that sits next to the socket.
            let dir = g.socket.parent().map(|p| p.join("logs")).unwrap_or_default();
            eprintln!("seedwell: not running; reading {}", dir.join("trackers.log").display());
            for l in trs_devlog::read::tail(&dir, &query) {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    print(&v);
                }
            }
            if a.follow {
                let mut f = trs_devlog::read::Follower::new(&dir);
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    for l in f.poll() {
                        if let Ok(v) = serde_json::from_str::<Value>(&l)
                            && query.matches(&v) {
                                print(&v);
                            }
                    }
                }
            }
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let mut since = cutoff.unwrap_or(0);
    let mut first = true;
    loop {
        let req = Request::TrackerLog(TrackerLogParams {
            limit: Some(if first { a.limit.unwrap_or(50) } else { 500 }),
            torrent: a.torrent.clone(),
            tracker: a.tracker.clone(),
            event: a.event.clone(),
            errors_only: a.errors.then_some(true),
            since_ms: (since > 0).then_some(since),
        });
        let v = c.call(&req).await?;
        let entries = v["entries"].as_array().cloned().unwrap_or_default();
        if first && entries.is_empty() && v["enabled"] == false {
            eprintln!("seedwell: developer mode is off, so nothing is logged (`seedwell dev on`, Settings > Advanced > Developer mode, or `seedwell daemon --dev-mode`)");
        }
        for e in &entries {
            print(e);
            since = since.max(e["ts_ms"].as_u64().unwrap_or(0));
        }
        first = false;
        if !a.follow {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    Ok(())
}

/// `seedwell dev on|off|status`: Developer mode (Settings > Advanced). It logs every tracker exchange
/// (passkeys masked) and is what `sim` requires. Switching needs permission `full`; it applies live.
async fn dev(g: &Global, action: DevAction) -> R {
    let mut c = connect(g).await?;
    const KEY: &str = "advanced.developer_mode";
    if action != DevAction::Status {
        let (_, _) = settings_set(&mut c, vec![(KEY.into(), (action == DevAction::On).to_string())]).await?;
    }
    let (_, s): (_, SettingsDto) = ask(&mut c, Request::SettingsGet(SettingsGetParams { keys: Some(vec![KEY.into()]) })).await?;
    let on = s.settings.get(KEY) == Some(&Value::Bool(true));
    let log = g.socket.parent().map(|p| p.join("logs").join("trackers.log")).unwrap_or_default();
    let size = std::fs::metadata(&log).map(|m| m.len()).ok();
    let v = json!({ "developer_mode": on, "log_path": log, "log_bytes": size });
    show(g, &v, || {
        let mut t = format!("Developer mode is {}.", if on { "ON: every tracker exchange is logged (passkeys masked)" } else { "off" });
        match size {
            Some(n) => t.push_str(&format!("\nTracker log: {} ({})", log.display(), trs_control::human::bytes(n))),
            None => t.push_str(&format!("\nTracker log: {} (no file yet)", log.display())),
        }
        if !on {
            t.push_str("\nTurn it on with `seedwell dev on`.");
        }
        t
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_comes_first() {
        let v = json!({ "b": 1, "a": [1, 2] });
        assert_eq!(json_text(&v, false), r#"{"v":1,"a":[1,2],"b":1}"#);
        assert!(json_text(&v, true).starts_with("{\n  \"v\": 1,\n  \"a\""));
        assert_eq!(json_text(&json!({}), false), r#"{"v":1}"#);
        // an answer that already carries a version keeps one
        assert_eq!(json_text(&json!({ "v": 1, "x": 2 }), false), r#"{"v":1,"x":2}"#);
        assert_eq!(json_text(&json!([1]), false), "[1]");
    }

    #[test]
    fn errors_map_to_exit_codes() {
        let nf: CliError = SelectError::NotFound("x".into()).into();
        assert_eq!(nf.code, 3);
        let amb: CliError = SelectError::Ambiguous { term: "u".into(), candidates: vec![("a".repeat(40), "A".into())], total: 2 }.into();
        assert_eq!((amb.code, amb.api_code), (3, Some(ErrorCode::Ambiguous)));
        assert_eq!(CliError::from(Usage("bad".into())).code, 2);
        let denied: CliError = ClientError::Api(ApiError::new(ErrorCode::PermissionDenied, "no")).into();
        assert_eq!(denied.code, 4);
        let off: CliError = ClientError::Api(ApiError::new(ErrorCode::AgentAccessOff, "off")).into();
        assert_eq!(off.code, 4);
        assert!(off.message.contains("Settings"));
        let nr: CliError = ClientError::NotRunning { path: "/x".into(), source: std::io::Error::other("e") }.into();
        assert_eq!(nr.code, 5);
        let gone: CliError = ClientError::Api(ApiError::not_found("n")).into();
        assert_eq!(gone.code, 3);
        let bad: CliError = ClientError::Api(ApiError::invalid("n")).into();
        assert_eq!(bad.code, 1);
    }
}
